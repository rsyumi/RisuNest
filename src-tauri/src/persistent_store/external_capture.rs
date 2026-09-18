//! PDS-owned capture cache selection. Hydration is planned from the change
//! index before capture admission, and no network runs through a PDS snapshot.
use super::{
    content_change_index, external_storage_state, sync_selection, PersistentStore, StoreError,
    StoreResult,
};
use crate::{
    asset_repository::{owner_manifest_codec::decode_owner_manifest, PayloadCas},
    external_storage::capture::{CaptureCatalog, DurableCaptureReference},
    local_backup::CancellationProbe,
};
use risunest_external_storage_format::format::library_fingerprint_domain;
use rusqlite::{params, OptionalExtension};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub(crate) struct CapturedSnapshot {
    pub id: String,
    pub identity: sync_selection::CaptureIdentity,
    pub catalog: CaptureCatalog,
    pub projected_records: usize,
    pub shared: bool,
}

impl CapturedSnapshot {
    pub(crate) fn durable_reference(
        &self,
        repository_root: &Path,
    ) -> StoreResult<DurableCaptureReference> {
        let reference = self
            .catalog
            .durable_reference(&self.id, repository_root)?;
        if reference.identity != self.identity {
            return Err(invalid("Capture catalog identity differs"));
        }
        Ok(reference)
    }
}

pub(crate) struct CaptureHydration {
    consumer: String,
    identity: sync_selection::CaptureIdentity,
    scope_id: [u8; 32],
    mode: HydrationMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HydrationMode {
    Reuse,
    Incremental,
    Rebuild,
}

struct Candidate {
    id: String,
    identity: sync_selection::CaptureIdentity,
    scope_id: String,
    manifest_hash: String,
    path: Option<PathBuf>,
    file_hash: Option<String>,
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Dependency {
    hash: String,
    manifest: bool,
}

const CODEC: &str = "logical-v1";
const PAGE: i64 = 128;
const GC_SCAN_LIMIT: i64 = 128;
const GC_DELETE_LIMIT: usize = 16;
const GC_REFERENCE_LIMIT: i64 = 32;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

fn check(probe: &dyn CancellationProbe) -> StoreResult<()> {
    if probe.is_cancelled() {
        Err(invalid("External capture cancelled"))
    } else {
        Ok(())
    }
}

fn decode_hash(value: &str, message: &str) -> StoreResult<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid(message))
}

impl PersistentStore {
    fn capture_candidate(
        &self,
        identity: &sync_selection::CaptureIdentity,
        scope: &str,
    ) -> StoreResult<Option<Candidate>> {
        let candidate: Option<(String, String, Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT c.id,c.manifest_hash,f.catalog_path,f.file_hash FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.identity=?1 AND c.scope_id=?2 AND c.codec_id=?3 AND c.device_capture_id=''",
                params![serde_json::to_string(identity)?, scope, CODEC],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(
            candidate.map(|(id, manifest_hash, path, file_hash)| Candidate {
                id,
                identity: identity.clone(),
                scope_id: scope.into(),
                manifest_hash,
                path: path.map(PathBuf::from),
                file_hash,
            }),
        )
    }

    fn capture_candidate_by_id(&self, id: &str) -> StoreResult<Option<Candidate>> {
        let candidate: Option<(String, String, String, Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT c.identity,c.scope_id,c.manifest_hash,f.catalog_path,f.file_hash FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.id=?1 AND c.codec_id=?2 AND c.device_capture_id=''",
                params![id, CODEC],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        candidate
            .map(|(identity, scope_id, manifest_hash, path, file_hash)| {
                Ok(Candidate {
                    id: id.into(),
                    identity: serde_json::from_str(&identity)?,
                    scope_id,
                    manifest_hash,
                    path: path.map(PathBuf::from),
                    file_hash,
                })
            })
            .transpose()
    }

    fn reopen_candidate(&self, candidate: &Candidate) -> StoreResult<CaptureCatalog> {
        let path = candidate
            .path
            .as_ref()
            .ok_or_else(|| invalid("Capture cache file is missing"))?;
        let file_hash = decode_hash(
            candidate
                .file_hash
                .as_deref()
                .ok_or_else(|| invalid("Capture cache hash is missing"))?,
            "Invalid capture cache hash",
        )?;
        let scope = decode_hash(&candidate.scope_id, "Invalid capture scope")?;
        let root = self
            .repository_root
            .join("external-storage")
            .canonicalize()?;
        if !path.canonicalize()?.starts_with(&root) {
            return Err(invalid("Capture cache escaped its native directory"));
        }
        let catalog =
            CaptureCatalog::reopen(path, &root.join("objects"), &file_hash, &candidate.identity)?;
        if hex::encode(catalog.content_fingerprint(&scope)?) != candidate.manifest_hash {
            return Err(invalid("Capture cache content differs"));
        }
        Ok(catalog)
    }

    /// Reopens the exact immutable capture owned by a durable job. Old capture
    /// revisions remain valid and are not compared with the current PDS head.
    pub(crate) fn reopen_external_capture(
        &self,
        capture_id: &str,
    ) -> StoreResult<CapturedSnapshot> {
        let candidate = self
            .capture_candidate_by_id(capture_id)?
            .ok_or_else(|| invalid("Pinned capture is unavailable"))?;
        if !external_storage_state::capture_has_consumers(&self.connection, capture_id)? {
            return Err(invalid("Capture is not pinned by a durable owner"));
        }
        let catalog = self.reopen_candidate(&candidate)?;
        Ok(CapturedSnapshot {
            id: candidate.id,
            identity: candidate.identity,
            catalog,
            projected_records: 0,
            shared: true,
        })
    }

    fn invalidate_unreferenced_candidate(&mut self, candidate: &Candidate) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        if external_storage_state::capture_has_consumers(&tx, &candidate.id)? {
            return Err(invalid("Referenced capture cache is unavailable"));
        }
        tx.execute(
            "UPDATE content_change_consumers SET rebuild_required=1 WHERE generation=?1 AND revision=?2",
            params![candidate.identity.generation, candidate.identity.revision],
        )?;
        tx.execute(
            "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
            [&candidate.id],
        )?;
        tx.execute(
            "DELETE FROM external_storage_captures WHERE id=?1",
            [&candidate.id],
        )?;
        tx.commit()?;
        if let Some(path) = &candidate.path {
            self.remove_capture_file(path);
        }
        Ok(())
    }

    fn validated_candidate(
        &mut self,
        identity: &sync_selection::CaptureIdentity,
        scope: &str,
    ) -> StoreResult<Option<(Candidate, CaptureCatalog)>> {
        let Some(candidate) = self.capture_candidate(identity, scope)? else {
            return Ok(None);
        };
        match self.reopen_candidate(&candidate) {
            Ok(catalog) => Ok(Some((candidate, catalog))),
            Err(_) => {
                self.invalidate_unreferenced_candidate(&candidate)?;
                Ok(None)
            }
        }
    }

    fn remove_capture_file(&self, path: &Path) {
        let captures = self
            .repository_root
            .join("external-storage")
            .join("captures");
        let Some(parent) = path.parent() else { return };
        if path.file_name().and_then(|name| name.to_str()) != Some("capture.sqlite") {
            return;
        }
        let (Ok(captures), Ok(parent)) = (captures.canonicalize(), parent.canonicalize()) else {
            return;
        };
        if parent == captures || !parent.starts_with(&captures) {
            return;
        }
        if fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata)
        }) && fs::remove_file(path).is_ok()
        {
            let _ = crate::trust_boundary::sync_directory(&parent);
            let _ = fs::remove_dir(&parent);
            let _ = crate::trust_boundary::sync_directory(&captures);
        }
    }

    fn cleanup_terminal_capture_references(&mut self) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        let terminal = {
            let mut query = tx.prepare(
                "SELECT r.capture_id,r.job_id FROM external_storage_capture_refs r JOIN external_storage_jobs j ON j.id=r.job_id WHERE j.phase IN ('complete','cancelled','stale') ORDER BY r.capture_id,r.job_id LIMIT ?1",
            )?;
            let rows = query.query_map([GC_REFERENCE_LIMIT], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (capture, job) in terminal {
            tx.execute(
                "DELETE FROM external_storage_capture_refs WHERE capture_id=?1 AND job_id=?2",
                params![capture, job],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn cleanup_capture_cache(&mut self, keep: &str) -> StoreResult<()> {
        self.cleanup_terminal_capture_references()?;
        let protected_catalogs = match self.device_store() {
            Ok(device) => match super::external_conflicts::registered_conflict_roots(
                device.connection(),
                &self.repository_root,
            ) {
                Ok(roots) => roots.catalogs,
                Err(error) => {
                    crate::nlog!("warn", "external capture cleanup deferred: {error}");
                    return Ok(());
                }
            },
            Err(error) => {
                crate::nlog!("warn", "external capture cleanup deferred: {error}");
                return Ok(());
            }
        };
        let mut paths = Vec::new();
        {
            let tx = self.connection.transaction()?;
            let candidates = {
                let mut query = tx.prepare(
                    "SELECT c.id,c.identity,f.catalog_path FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.id!=?1 AND NOT EXISTS(SELECT 1 FROM external_storage_capture_refs r WHERE r.capture_id=c.id) ORDER BY c.rowid LIMIT ?2",
                )?;
                let rows = query.query_map(params![keep, GC_SCAN_LIMIT], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (id, encoded, path) in candidates {
                if paths.len() == GC_DELETE_LIMIT {
                    break;
                }
                if path
                    .as_deref()
                    .and_then(|value| Path::new(value).canonicalize().ok())
                    .is_some_and(|value| protected_catalogs.contains(&value))
                {
                    continue;
                }
                let needed = match serde_json::from_str::<sync_selection::CaptureIdentity>(&encoded) {
                    Ok(identity) => tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM content_change_consumers WHERE generation=?1 AND revision=?2 AND rebuild_required=0)",
                        params![identity.generation, identity.revision],
                        |row| row.get::<_, bool>(0),
                    )?,
                    Err(_) => false,
                };
                if needed {
                    continue;
                }
                tx.execute(
                    "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
                    [&id],
                )?;
                tx.execute("DELETE FROM external_storage_captures WHERE id=?1", [&id])?;
                if let Some(path) = path {
                    paths.push(PathBuf::from(path));
                }
            }
            tx.commit()?;
        }
        for path in paths {
            self.remove_capture_file(&path);
        }
        Ok(())
    }

    fn hydrate_hash(&self, hash: &str, probe: &dyn CancellationProbe) -> StoreResult<()> {
        check(probe)?;
        if PayloadCas::new(&self.repository_root)?
            .stat_object(hash)?
            .is_some()
        {
            return Ok(());
        }
        let cancellation = || {
            if probe.is_cancelled() {
                Err(crate::server_sync::SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        };
        match crate::server_sync::residency::open_or_hydrate_with_check(
            &self.repository_root,
            hash,
            &cancellation,
        ) {
            Ok(Some(file)) => {
                drop(file);
                Ok(())
            }
            Ok(None) => Err(invalid("Required capture payload is unavailable")),
            Err(error) if error.code == "cancelled" => Err(invalid("External capture cancelled")),
            Err(error) => Err(invalid(&format!(
                "External capture hydration failed: {}",
                error.code
            ))),
        }
    }

    fn hydrate_dependencies(
        &self,
        dependencies: BTreeSet<Dependency>,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<()> {
        let manifests = dependencies
            .iter()
            .filter(|item| item.manifest)
            .map(|item| item.hash.clone())
            .collect::<Vec<_>>();
        for dependency in dependencies {
            self.hydrate_hash(&dependency.hash, probe)?;
        }
        let cas = PayloadCas::new(&self.repository_root)?;
        for manifest in manifests {
            check(probe)?;
            let bytes = cas
                .read_object(&manifest)?
                .ok_or_else(|| invalid("Required owner manifest is unavailable"))?;
            for entry in decode_owner_manifest(&bytes)
                .map_err(|_| invalid("Required owner manifest is invalid"))?
            {
                if let Some(hash) = entry.payload_hash {
                    self.hydrate_hash(&hex::encode(hash), probe)?;
                }
            }
        }
        Ok(())
    }

    fn full_dependency_page(&self, after: &str) -> StoreResult<BTreeSet<Dependency>> {
        let generation = super::active_generation(&self.connection)?;
        let mut query = self.connection.prepare(
            "WITH dependencies(hash,manifest) AS (
                SELECT object_hash,0 FROM asset_aliases WHERE generation=?1 AND object_hash IS NOT NULL
                UNION SELECT manifest_hash,1 FROM asset_owner_heads WHERE generation=?1 AND manifest_hash IS NOT NULL
                UNION SELECT json_extract(archived_object,'$.objectHash'),0
                      FROM characters WHERE generation=?1 AND archived_object IS NOT NULL
                UNION SELECT assets.value,0 FROM characters, json_each(
                    json_extract(characters.archived_object,'$.assetHashes')
                ) AS assets
                      WHERE generation=?1 AND archived_object IS NOT NULL
             ) SELECT hash,max(manifest) FROM dependencies WHERE hash>?2 GROUP BY hash ORDER BY hash LIMIT ?3",
        )?;
        let rows = query.query_map(params![generation, after, PAGE], |row| {
            Ok(Dependency {
                hash: row.get(0)?,
                manifest: row.get::<_, bool>(1)?,
            })
        })?;
        let dependencies = rows.collect::<Result<_, _>>()?;
        Ok(dependencies)
    }

    fn owner_dependencies(
        &self,
        generation: &str,
        character: Option<&str>,
        dependencies: &mut BTreeSet<Dependency>,
    ) -> StoreResult<()> {
        let mut query = match character {
            Some(_) => self.connection.prepare(
                "SELECT manifest_hash FROM asset_owner_heads WHERE generation=?1 AND owner_kind='character-additional-assets' AND owner_locator=?2 AND manifest_hash IS NOT NULL",
            )?,
            None => self.connection.prepare(
                "SELECT manifest_hash FROM asset_owner_heads WHERE generation=?1 AND owner_kind IN ('root-module-assets','persona-embedded-module-assets') AND manifest_hash IS NOT NULL AND ?2=''",
            )?,
        };
        let hashes = query
            .query_map(params![generation, character.unwrap_or("")], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(query);
        dependencies.extend(hashes.into_iter().map(|hash| Dependency {
            hash,
            manifest: true,
        }));
        Ok(())
    }

    fn record_dependencies(
        &self,
        generation: &str,
        key: &content_change_index::ContentKey,
    ) -> StoreResult<BTreeSet<Dependency>> {
        let mut dependencies = BTreeSet::new();
        let direct = match key.kind.as_str() {
            "asset" | "inlay" => self.connection.query_row(
                "SELECT object_hash FROM asset_aliases WHERE generation=?1 AND kind=?2 AND logical_key=?3",
                params![generation, key.kind, key.key1], |row| row.get::<_, Option<String>>(0),
            ).optional()?.flatten(),
            _ => None,
        };
        if let Some(hash) = direct {
            dependencies.insert(Dependency {
                hash,
                manifest: false,
            });
        }
        match key.kind.as_str() {
            "root" => self.owner_dependencies(generation, None, &mut dependencies)?,
            "character" => {
                self.owner_dependencies(generation, Some(&key.key1), &mut dependencies)?;
                let archived: Option<String> = self
                    .connection
                    .query_row(
                        "SELECT archived_object FROM characters
                         WHERE generation=?1 AND character_id=?2",
                        params![generation, key.key1],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(archived) = archived {
                    let archived: super::archive::ArchivedObject = serde_json::from_str(&archived)?;
                    dependencies.insert(Dependency {
                        hash: archived.object_hash,
                        manifest: false,
                    });
                    dependencies.extend(archived.asset_hashes.into_iter().map(|hash| Dependency {
                        hash,
                        manifest: false,
                    }));
                }
            }
            "owner" if key.key1 == "character-additional-assets" => {
                self.owner_dependencies(generation, Some(&key.key2), &mut dependencies)?
            }
            "owner"
                if matches!(
                    key.key1.as_str(),
                    "root-module-assets" | "persona-embedded-module-assets"
                ) =>
            {
                self.owner_dependencies(generation, None, &mut dependencies)?
            }
            _ => {}
        }
        Ok(dependencies)
    }

    fn incremental_dependency_page(
        &self,
        identity: &sync_selection::CaptureIdentity,
        after_revision: i64,
        after: Option<&content_change_index::ContentKey>,
    ) -> StoreResult<(Vec<content_change_index::ContentKey>, BTreeSet<Dependency>)> {
        let (kind, key1, key2) = after
            .map(|key| (key.kind.as_str(), key.key1.as_str(), key.key2.as_str()))
            .unwrap_or(("", "", ""));
        let keys = {
            let mut query = self.connection.prepare(
                "SELECT kind,key1,key2 FROM content_changes WHERE generation=?1 AND revision>?2 AND revision<=?3 AND (kind,key1,key2)>(?4,?5,?6) ORDER BY kind,key1,key2 LIMIT ?7",
            )?;
            let rows = query.query_map(
                params![
                    identity.generation,
                    after_revision,
                    identity.revision,
                    kind,
                    key1,
                    key2,
                    PAGE
                ],
                |row| {
                    Ok(content_change_index::ContentKey {
                        kind: row.get(0)?,
                        key1: row.get(1)?,
                        key2: row.get(2)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut dependencies = BTreeSet::new();
        for key in &keys {
            dependencies.extend(self.record_dependencies(&identity.generation, key)?);
        }
        Ok((keys, dependencies))
    }

    /// Call before acquiring file(true). Network hydration is restricted to
    /// dependencies of changed records unless a full rebuild is unavoidable.
    pub(crate) fn hydrate_external_capture_dependencies(
        &self,
        consumer: &str,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CaptureHydration> {
        if consumer.is_empty()
        {
            return Err(invalid(
                "Library capture requires the library and referenced asset scope",
            ));
        }
        check(probe)?;
        let identity = sync_selection::identity(&self.connection)?;
        let scope_id = library_fingerprint_domain();
        let scope_hex = hex::encode(scope_id);
        let mut mode = if self
            .capture_candidate(&identity, &scope_hex)?
            .is_some_and(|candidate| self.reopen_candidate(&candidate).is_ok())
        {
            HydrationMode::Reuse
        } else {
            let cursor: Option<(String, i64, bool)> = self.connection.query_row(
                "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
                [consumer], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            match cursor {
                Some((generation, revision, false)) if generation == identity.generation => {
                    let mut previous = identity.clone();
                    previous.revision = revision;
                    if self
                        .capture_candidate(&previous, &scope_hex)?
                        .is_some_and(|candidate| self.reopen_candidate(&candidate).is_ok())
                    {
                        HydrationMode::Incremental
                    } else {
                        HydrationMode::Rebuild
                    }
                }
                _ => HydrationMode::Rebuild,
            }
        };
        if crate::server_sync::residency::Residency::exists(&self.repository_root) {
            match mode {
                HydrationMode::Reuse => {}
                HydrationMode::Incremental => {
                    let after_revision: i64 = self.connection.query_row(
                        "SELECT revision FROM content_change_consumers WHERE id=?1 AND generation=?2 AND rebuild_required=0",
                        params![consumer, identity.generation], |row| row.get(0),
                    )?;
                    let full: bool = self.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM content_changes WHERE generation=?1 AND kind='full' AND revision>?2 AND revision<=?3)",
                        params![identity.generation, after_revision, identity.revision], |row| row.get(0),
                    )?;
                    if full {
                        self.hydrate_full_dependencies(probe)?;
                        mode = HydrationMode::Rebuild;
                    } else {
                        let mut after = None;
                        loop {
                            let (keys, dependencies) = self.incremental_dependency_page(
                                &identity,
                                after_revision,
                                after.as_ref(),
                            )?;
                            if keys.is_empty() {
                                break;
                            }
                            after = keys.last().cloned();
                            self.hydrate_dependencies(dependencies, probe)?;
                        }
                    }
                }
                HydrationMode::Rebuild => self.hydrate_full_dependencies(probe)?,
            }
        }
        if sync_selection::identity(&self.connection)? != identity {
            return Err(invalid("Library changed during capture hydration"));
        }
        Ok(CaptureHydration {
            consumer: consumer.into(),
            identity,
            scope_id,
            mode,
        })
    }

    fn hydrate_full_dependencies(&self, probe: &dyn CancellationProbe) -> StoreResult<()> {
        let mut after = String::new();
        loop {
            check(probe)?;
            let dependencies = self.full_dependency_page(&after)?;
            if dependencies.is_empty() {
                return Ok(());
            }
            after = dependencies
                .last()
                .expect("nonempty dependency page")
                .hash
                .clone();
            self.hydrate_dependencies(dependencies, probe)?;
        }
    }

    /// Call under file(true), using a hydration token prepared before admission.
    pub(crate) fn capture_external_library(
        &mut self,
        consumer: &str,
        hydration: &CaptureHydration,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CapturedSnapshot> {
        let identity = sync_selection::identity(&self.connection)?;
        let scope_id = library_fingerprint_domain();
        if hydration.consumer != consumer
            || hydration.identity != identity
            || hydration.scope_id != scope_id
        {
            return Err(invalid("Capture hydration is stale"));
        }
        check(probe)?;
        self.cleanup_terminal_capture_references()?;
        let scope_hex = hex::encode(scope_id);
        let root = self.repository_root.join("external-storage");
        let objects = root.join("objects");
        if let Some((candidate, catalog)) = self.validated_candidate(&identity, &scope_hex)? {
            let tx = self.connection.transaction()?;
            content_change_index::commit_cursor(
                &tx,
                consumer,
                &identity.generation,
                identity.revision,
            )?;
            content_change_index::prune(&tx)?;
            tx.commit()?;
            self.cleanup_capture_cache(&candidate.id)?;
            return Ok(CapturedSnapshot {
                id: candidate.id,
                identity,
                catalog,
                projected_records: 0,
                shared: true,
            });
        }
        let cursor: Option<(String, i64, bool)> = self.connection.query_row(
            "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
            [consumer], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        let previous_identity = match cursor {
            Some((generation, revision, false)) if generation == identity.generation => {
                let mut previous = identity.clone();
                previous.revision = revision;
                Some(previous)
            }
            _ => None,
        };
        let previous = match previous_identity {
            Some(previous) => {
                self.validated_candidate(&previous, &scope_hex)?
                    .map(|(candidate, catalog)| {
                        drop(catalog);
                        candidate
                    })
            }
            None => None,
        };
        if previous.is_none() {
            let tx = self.connection.transaction()?;
            content_change_index::require_rebuild(&tx, consumer)?;
            tx.commit()?;
            if hydration.mode != HydrationMode::Rebuild {
                return Err(invalid("Capture hydration must be retried"));
            }
        }
        let previous_hash = previous
            .as_ref()
            .map(|candidate| {
                decode_hash(
                    candidate
                        .file_hash
                        .as_deref()
                        .ok_or_else(|| invalid("Capture cache hash is missing"))?,
                    "Invalid capture cache hash",
                )
            })
            .transpose()?;
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.join("captures").join(&id);
        let prior = previous
            .as_ref()
            .zip(previous_hash.as_ref())
            .map(|(candidate, hash)| {
                (
                    candidate
                        .path
                        .as_ref()
                        .expect("validated capture path")
                        .as_path(),
                    hash,
                )
            });
        let mut catalog = CaptureCatalog::create(&directory, &objects, prior)?;
        let prepared = self.prepare_content_capture(&id, consumer, identity.revision)?;
        let projected_records = prepared.project(&mut catalog, probe)?;
        let capture_id = prepared.register(self, &catalog, &scope_id, CODEC)?;
        self.cleanup_capture_cache(&capture_id)?;
        Ok(CapturedSnapshot {
            id: capture_id,
            identity,
            catalog,
            projected_records,
            shared: false,
        })
    }

    pub(crate) fn retain_external_capture(
        &mut self,
        capture: &str,
        owner: &str,
    ) -> StoreResult<()> {
        if owner.is_empty() {
            return Err(invalid("Missing capture owner"));
        }
        let tx = self.connection.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_storage_captures WHERE id=?1)",
            [capture],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(invalid("Capture does not exist"));
        }
        tx.execute(
            "INSERT OR IGNORE INTO external_storage_capture_refs VALUES(?1,?2)",
            params![capture, owner],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn release_external_capture(
        &mut self,
        capture: &str,
        owner: &str,
    ) -> StoreResult<bool> {
        let tx = self.connection.transaction()?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE id=?1 AND phase NOT IN ('complete','cancelled','stale'))",
            [owner], |row| row.get(0),
        )?;
        if active {
            return Err(invalid("Active job still owns its capture"));
        }
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE capture_id=?1 AND job_id=?2",
            params![capture, owner],
        )?;
        let remaining = external_storage_state::capture_has_consumers(&tx, capture)?;
        tx.commit()?;
        Ok(!remaining)
    }

    pub(crate) fn cleanup_deleted_conflict_capture(
        &mut self,
        reference: &DurableCaptureReference,
    ) -> StoreResult<bool> {
        let remaining = super::external_conflicts::registered_conflict_roots(
            self.device_store()?.connection(),
            &self.repository_root,
        )?;
        let roots = crate::external_storage::capture::registered_capture_roots(
            [reference],
            &self.repository_root,
        )?;
        let path = roots
            .catalogs
            .into_iter()
            .next()
            .ok_or_else(|| invalid("Conflict capture catalog is unavailable"))?;
        if remaining.catalogs.contains(&path) {
            return Ok(false);
        }

        let tx = self.connection.transaction()?;
        let mut rows = {
            let mut query = tx.prepare(
                "SELECT c.id,c.identity,f.catalog_path,f.file_hash FROM external_storage_capture_files f JOIN external_storage_captures c ON c.id=f.capture_id",
            )?;
            let rows = query
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        rows.retain(|(_, _, candidate, _)| {
            Path::new(candidate)
                .canonicalize()
                .is_ok_and(|candidate| candidate == path)
        });
        for (capture_id, encoded_identity, _, file_hash) in &rows {
            if capture_id != &reference.capture_id
                || file_hash != &reference.catalog_hash
                || serde_json::from_str::<sync_selection::CaptureIdentity>(encoded_identity)?
                    != reference.identity
            {
                return Err(invalid("Conflict capture owner differs"));
            }
            let active: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_storage_capture_refs WHERE capture_id=?1)",
                [capture_id],
                |row| row.get(0),
            )?;
            let needed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM content_change_consumers WHERE generation=?1 AND revision=?2 AND rebuild_required=0)",
                params![reference.identity.generation, reference.identity.revision],
                |row| row.get(0),
            )?;
            if active || needed {
                return Ok(false);
            }
        }
        for (capture_id, _, _, _) in rows {
            tx.execute(
                "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
                [&capture_id],
            )?;
            tx.execute(
                "DELETE FROM external_storage_captures WHERE id=?1",
                [&capture_id],
            )?;
        }
        tx.commit()?;
        self.remove_capture_file(&path);
        Ok(!path.exists())
    }
}

#[cfg(test)]
mod conflict_cleanup_tests {
    use super::*;
    use crate::persistent_store::external_conflicts::{
        delete_external_conflict, preserve_local_conflict, ExternalConflictRecord,
        PreservedHeadObservation, PreservedRemoteState,
    };
    use risunest_external_storage_format::snapshot::{
        envelope_length, ObjectRole, PublicObjectHeader, StoredObject, WireLocator,
    };

    fn stored(repository: &str, id: &str, role: ObjectRole) -> StoredObject {
        let header = PublicObjectHeader::new(repository.into(), id.into(), role, 1).unwrap();
        StoredObject {
            ciphertext_length: envelope_length(&header).unwrap(),
            header,
            locator: WireLocator {
                connection_identity: "synthetic/root".into(),
                collection: None,
                object: id.into(),
            },
            ciphertext_sha256: [2; 32],
            plaintext_length: 1,
            plaintext_sha256: [1; 32],
        }
    }

    #[test]
    fn restored_old_capture_index_cannot_delete_a_device_owned_conflict_catalog() {
        use crate::persistent_store::content_capture::ContentCaptureSink;

        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let external = store.repository_root.join("external-storage");
        let capture_directory = external.join("captures/restored-old-index");
        let object_directory = external.join("objects");
        let identity = sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 7,
        };
        let mut catalog =
            CaptureCatalog::create(&capture_directory, &object_directory, None).unwrap();
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", b"device-owned conflict source").unwrap();
        catalog.finish().unwrap();
        let reference = catalog
            .durable_reference("capture-conflict", &store.repository_root)
            .unwrap();
        let catalog_path = catalog.manifest().unwrap().1.to_path_buf();
        drop(catalog);

        let device = store.device_store().unwrap().connection();
        preserve_local_conflict(
            device,
            &ExternalConflictRecord {
                id: "conflict".into(),
                created_at_ms: 1,
                connection_id: "connection".into(),
                repository_id: "repository".into(),
                local: reference.clone(),
                remote: PreservedRemoteState {
                    snapshot: stored("repository", "snapshot-remote", ObjectRole::SyncState),
                    logical_revision: 8,
                    commit_id: "remote-commit".into(),
                    head: PreservedHeadObservation {
                        commit_id: "remote-commit".into(),
                        authenticated_body_hash: "02".repeat(32),
                    },
                },
                remote_point: None,
                resolved: false,
            },
        )
        .unwrap();

        store
            .connection
            .execute(
                "INSERT INTO external_storage_captures VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    reference.capture_id,
                    serde_json::to_string(&identity).unwrap(),
                    "restored-scope",
                    CODEC,
                    "restored-device-capture",
                    reference.catalog_hash,
                ],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO external_storage_capture_files VALUES(?1,?2,?3)",
                params![
                    reference.capture_id,
                    catalog_path.to_string_lossy().into_owned(),
                    reference.catalog_hash,
                ],
            )
            .unwrap();

        store.cleanup_capture_cache("new-capture").unwrap();
        assert!(catalog_path.exists());
        let retained: bool = store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM external_storage_captures WHERE id=?1)",
                [&reference.capture_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(retained);

        store
            .connection
            .execute(
                "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
                [&reference.capture_id],
            )
            .unwrap();
        store
            .connection
            .execute(
                "DELETE FROM external_storage_captures WHERE id=?1",
                [&reference.capture_id],
            )
            .unwrap();
        delete_external_conflict(store.device_store().unwrap().connection(), "conflict").unwrap();
        assert!(store
            .cleanup_deleted_conflict_capture(&reference)
            .unwrap());
        assert!(!catalog_path.exists());
    }
}
