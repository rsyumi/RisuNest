//! Physical per-device storage policy. Logical aliases and snapshot roots are
//! preserved when a verified server copy replaces a local payload file.
use super::{snapshot_archive, PersistentStore};
use crate::{
    asset_repository::{object_physical_key, PayloadCas},
    server_sync::{
        client::ServerClient,
        residency::{open_or_hydrate_with_check, AssetPolicy, Residency},
        Result, SyncError,
    },
};
use std::collections::BTreeSet;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResidencyStatus {
    policy: AssetPolicy,
    local_bytes: u64,
    remote_bytes: u64,
    remote_objects: u64,
    unavailable_objects: u64,
    pub evicted_bytes: u64,
}
struct Inventory {
    referenced: BTreeSet<String>,
    local: BTreeSet<String>,
    release_blocked: bool,
}
impl ResidencyStatus {
    pub(crate) fn has_remote_or_missing(&self) -> bool {
        self.remote_objects > 0 || self.unavailable_objects > 0
    }
}

fn cold_hashes(db: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut statement =
        db.prepare("SELECT DISTINCT object_hash FROM cold_aliases WHERE object_hash IS NOT NULL")?;
    let hashes = statement
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(hashes)
}
impl PersistentStore {
    pub(crate) fn hydrate_registered_remote_assets(
        &self,
        check: impl Fn() -> Result<()>,
    ) -> Result<()> {
        if !self.repository_root.join("asset-residency.sqlite").exists() {
            return Ok(());
        }
        let residency = Residency::open(&self.repository_root)?;
        let hydrate = |hash: &str| -> Result<()> {
            check()?;
            if residency.object(hash, None)?.is_some() {
                open_or_hydrate_with_check(&self.repository_root, hash, &check)?
                    .ok_or_else(|| SyncError::new("required-asset-unavailable", 409))?;
            }
            Ok(())
        };
        let mut cursor = None;
        loop {
            check()?;
            // Custody records outlive physical GC. Preserve catalog files even
            // without references, but do not revive deleted catalog entries.
            let page = self.query_asset_object_catalog(128, cursor.as_deref())?;
            for object in page.items {
                hydrate(&object.object_hash)?;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        // Restoring an older snapshot can roll back the live catalog while
        // newer retained snapshots still own remote payloads. GC protects these
        // roots independently of catalog membership; backup must do the same.
        let roots = snapshot_archive::Archive::open(&self.snapshots_dir)?.roots()?;
        let cas = PayloadCas::new(&self.repository_root)?;
        let mut historical = BTreeSet::new();
        let mut manifests = BTreeSet::new();
        for root in roots {
            historical.extend(root.object_hashes);
            manifests.extend(root.manifest_hashes);
        }
        for manifest in manifests {
            hydrate(&manifest)?;
            // Missing or damaged local files remain for the backup's existing
            // preservation path. Only verified manifests supply dependencies.
            if let Some(bytes) = cas.read_object(&manifest)? {
                if risunest_sync_wire::hash(&bytes) == manifest {
                    if let Ok(entries) =
                        crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes)
                    {
                        historical.extend(
                            entries
                                .into_iter()
                                .filter_map(|entry| entry.payload_hash.map(hex::encode)),
                        );
                    }
                }
            }
        }
        for hash in historical {
            hydrate(&hash)?;
        }
        check()
    }
    fn residency_inventory(&self, residency: &Residency, guarded: bool) -> Result<Inventory> {
        let cas = PayloadCas::new(&self.repository_root)?;
        let roots = self.collect_asset_gc_roots(&cas, guarded, false)?;
        let mut referenced = BTreeSet::new();
        let mut local = BTreeSet::new();
        let mut release_blocked = false;
        for root in roots {
            release_blocked |= root.retain_all_objects || !root.blockers.is_empty();
            referenced.extend(root.object_hashes);
            referenced.extend(root.manifest_hashes.iter().cloned());
            local.extend(root.manifest_hashes);
        }
        for hash in &local {
            let bytes = cas
                .read_object(hash)?
                .ok_or_else(|| SyncError::new("local-manifest-missing", 409))?;
            if risunest_sync_wire::hash(&bytes) != *hash {
                return Err(SyncError::new("local-manifest-corrupt", 409));
            }
            for entry in
                crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes)
                    .map_err(|_| SyncError::new("local-manifest-corrupt", 409))?
            {
                if let Some(hash) = entry.payload_hash {
                    referenced.insert(hex::encode(hash));
                }
            }
        }
        local.extend(cold_hashes(&self.connection)?);
        for reader in self.revision_leases.values() {
            local.extend(cold_hashes(&reader.connection)?);
        }
        // Detached consumers and staged imports still require their physical
        // inputs. Their roots do not distinguish media from cold record bytes.
        for root in self
            .active_readers
            .detached_asset_roots()?
            .into_iter()
            .chain(
                crate::asset_repository::migration_gc::collect_staged_migration_roots(
                    &self.repository_root,
                )?,
            )
        {
            local.extend(root.object_hashes);
            local.extend(root.manifest_hashes);
        }
        // External captures require complete local payloads through publication,
        // even when the source alias was removed by a later edit.
        local.extend(crate::external_storage::capture::registered_roots(
            &self.connection, &self.repository_root,
        )?.object_hashes);
        // Archive SQLite holds only metadata. Cache the role list by immutable
        // snapshot ID so later cleanups do not reconstruct each historical DB.
        let archive = snapshot_archive::Archive::open(&self.snapshots_dir)?;
        for info in archive.list()? {
            let hashes = match residency.snapshot_roles(&info.id)? {
                Some(hashes) => hashes,
                None => {
                    let scratch = archive.scratch()?;
                    archive.restore(&info.id, &scratch.path)?;
                    let db = rusqlite::Connection::open_with_flags(
                        &scratch.path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    let hashes = cold_hashes(&db)?;
                    drop(db);
                    residency.save_snapshot_roles(&info.id, &hashes)?;
                    hashes
                }
            };
            local.extend(hashes);
        }
        drop(archive);
        let jobs = if guarded {
            crate::asset_repository::job_pins::collect_durable_cas_job_roots_already_guarded(
                &self.repository_root,
            )
        } else {
            crate::asset_repository::job_pins::collect_durable_cas_job_roots(&self.repository_root)
        };
        if !jobs.blockers.is_empty() {
            return Err(SyncError::new("asset-jobs-unresolved", 409));
        }
        local.extend(jobs.object_hashes);
        local.extend(jobs.manifest_hashes);
        Ok(Inventory {
            referenced,
            local,
            release_blocked,
        })
    }
    pub(crate) fn asset_residency_status(&self) -> Result<ResidencyStatus> {
        let residency = Residency::open(&self.repository_root)?;
        let inventory = self.residency_inventory(&residency, false)?;
        let cas = PayloadCas::new(&self.repository_root)?;
        let mut status = ResidencyStatus {
            policy: residency.policy()?,
            local_bytes: 0,
            remote_bytes: 0,
            remote_objects: 0,
            unavailable_objects: 0,
            evicted_bytes: 0,
        };
        for hash in inventory.referenced {
            if let Some(size) = cas.stat_object(&hash)? {
                status.local_bytes += size;
            } else if let Some(object) = residency.object(&hash, None)? {
                status.remote_bytes += object.size;
                status.remote_objects += 1;
            } else {
                status.unavailable_objects += 1;
            }
        }
        Ok(status)
    }
    pub(crate) fn asset_residency_set_policy(
        &self,
        policy: AssetPolicy,
        check: impl Fn() -> Result<()>,
    ) -> Result<ResidencyStatus> {
        check()?;
        let residency = Residency::open(&self.repository_root)?;
        if policy == AssetPolicy::Remote && self.server_stored_config()?.is_none() {
            return Err(SyncError::new("server-not-bound", 409));
        }
        residency.set_policy(policy)?;
        if policy == AssetPolicy::Full {
            let inventory = self.residency_inventory(&residency, false)?;
            for hash in inventory.referenced {
                check()?;
                if open_or_hydrate_with_check(&self.repository_root, &hash, &check)?.is_none() {
                    return Err(SyncError::new("required-asset-unavailable", 409));
                }
            }
        }
        let status = self.asset_residency_status()?;
        check()?;
        Ok(status)
    }
    pub(crate) fn asset_residency_evict(
        &self,
        check: impl Fn() -> Result<()>,
    ) -> Result<ResidencyStatus> {
        check()?;
        let mut residency = Residency::open(&self.repository_root)?;
        if residency.policy()? != AssetPolicy::Remote {
            return Err(SyncError::new("remote-asset-policy-required", 409));
        }
        if self.server_status()?.operation_pending {
            return Err(SyncError::new("resolve-pending-operation-first", 409));
        }
        let mut config = self
            .server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let mut client = ServerClient::new(config.resolve(&self.repository_root)?)?;
        let head = client.resolve_identity(false)?;
        check()?;
        config.endpoint = client.config().endpoint.clone();
        let inventory = self.residency_inventory(&residency, false)?;
        let candidates = inventory
            .referenced
            .difference(&inventory.local)
            .cloned()
            .collect::<Vec<_>>();
        let cas = PayloadCas::new(&self.repository_root)?;
        let mut evicted = 0;
        for page in candidates.chunks(128) {
            check()?;
            let mut objects = std::collections::BTreeMap::new();
            for hash in page {
                if let Some(size) = cas.stat_object(hash)? {
                    objects.insert(hash.clone(), size);
                }
            }
            if objects.is_empty() {
                continue;
            }
            let identities = objects
                .iter()
                .map(|(hash, size)| serde_json::json!({"hash":hash,"size":size.to_string()}))
                .collect::<Vec<_>>();
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Missing {
                missing: Vec<String>,
            }
            let (_, missing): (_, Missing) = client.json(
                reqwest::Method::POST,
                "objects/missing",
                &[],
                Some(&identities),
                &[],
            )?;
            for hash in missing.missing {
                check()?;
                let size = *objects
                    .get(&hash)
                    .ok_or_else(|| SyncError::new("invalid-missing-response", 502))?;
                // A snapshot-only object may never have been published. The
                // temporary transfer CAS holds at most one file, then is removed.
                let directory = tempfile::Builder::new()
                    .prefix("asset-offload-")
                    .tempdir_in(&self.repository_root)?;
                let cache = crate::server_sync::cache::Cache::open(directory.path())?;
                let mut file = cas
                    .open_object(&hash)?
                    .ok_or_else(|| SyncError::new("asset-changed", 409))?;
                crate::server_sync::transfer::prepare_checked(
                    &cache.cas, &mut file, &hash, size, &check,
                )?;
                crate::server_sync::transfer::Transfer::new(&client, &cache)?
                    .with_check(&check)
                    .upload_with_hints(&[hash], &[], false, &Default::default())?;
            }
            check()?;
            residency.retain(
                &client,
                &config,
                &head,
                &objects
                    .iter()
                    .map(|(hash, size)| (hash.clone(), Some(*size)))
                    .collect::<Vec<_>>(),
            )?;
            check()?;
            let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
            let fresh = self.residency_inventory(&residency, true)?;
            let sync_cache_root =
                self.repository_root
                    .join("server-sync")
                    .join(risunest_sync_wire::hash(
                        format!("{}:{}", config.library_id, config.device_id).as_bytes(),
                    ));
            for (hash, size) in objects {
                check()?;
                if fresh.local.contains(&hash) {
                    continue;
                }
                // Keep aliases and their object catalog row. Offloading is
                // neither a logical deletion nor an asset GC tombstone.
                if let crate::asset_repository::ExactObjectUnlink::Removed { .. } =
                    cas.unlink_exact_object(&hash, size, &object_physical_key(&hash))?
                {
                    evicted += size;
                }
                if sync_cache_root.is_dir() {
                    PayloadCas::new(&sync_cache_root)?.unlink_exact_object(
                        &hash,
                        size,
                        &object_physical_key(&hash),
                    )?;
                }
            }
        }
        self.asset_residency_release_unused(&check)?;
        let mut status = self.asset_residency_status()?;
        check()?;
        status.evicted_bytes = evicted;
        Ok(status)
    }
    pub(crate) fn asset_residency_release_unused(
        &self,
        check: impl Fn() -> Result<()>,
    ) -> Result<()> {
        let residency = Residency::open(&self.repository_root)?;
        let cas = PayloadCas::new(&self.repository_root)?;
        let mut after = String::new();
        loop {
            let page = residency.page(&after)?;
            if page.is_empty() {
                break;
            }
            let mut releases = std::collections::BTreeMap::<
                String,
                Vec<crate::server_sync::residency::RemoteObject>,
            >::new();
            {
                let _guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
                let inventory = self.residency_inventory(&residency, true)?;
                if inventory.release_blocked {
                    return Ok(());
                }
                for (cursor, hash, _) in page {
                    check()?;
                    after = cursor.clone();
                    let context = cursor
                        .split_once('/')
                        .ok_or_else(|| SyncError::new("invalid-custody-cursor", 409))?
                        .0;
                    let Some(object) = residency.release_object(&hash, context)? else {
                        continue;
                    };
                    if inventory.referenced.contains(&hash) || inventory.local.contains(&hash) {
                        continue;
                    }
                    if !residency.begin_release(&object)? {
                        continue;
                    }
                    if cas.stat_object(&hash)?.is_none() {
                        self.connection
                            .execute("DELETE FROM asset_objects WHERE object_hash=?1", [&hash])?;
                    }
                    releases.entry(context.to_owned()).or_default().push(object);
                }
            }
            for objects in releases.values() {
                check()?;
                let mut client =
                    ServerClient::new(objects[0].config.resolve(&self.repository_root)?)?;
                let head = client.resolve_identity(false)?;
                check()?;
                let reply=client.request(reqwest::Method::POST,"objects/retention/release",&[],
                    Some(risunest_sync_wire::canonical::encode(&serde_json::json!({"epoch":head.epoch,"objects":objects.iter().map(|object|serde_json::json!({"deviceId":object.device_id,"hash":object.hash,"retentionId":object.retention_id})).collect::<Vec<_>>()}))?),&[],risunest_sync_wire::MAX_METADATA_BYTES)?;
                if reply.status != 204 {
                    return Err(crate::server_sync::client::response_error(reply));
                }
                for object in objects {
                    residency.finish_release(object)?;
                }
            }
        }
        check()
    }
}
