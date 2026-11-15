use super::{
    lww::{AckEntry, Header, OutboxEntry},
    PersistentStore, StoreResult,
};
use risunest_sync_wire::stamp::DecimalU64;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: &str = r#"
CREATE TABLE external_lww_sequences(target TEXT NOT NULL,writer TEXT NOT NULL,next_seq TEXT NOT NULL,PRIMARY KEY(target,writer));
CREATE TABLE external_lww_segments(target TEXT NOT NULL,writer TEXT NOT NULL,seq TEXT NOT NULL,authority TEXT NOT NULL,metadata TEXT NOT NULL,sealed BLOB NOT NULL,complete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(target,writer,seq));
CREATE TABLE external_lww_seen(target TEXT NOT NULL,writer TEXT NOT NULL,seq TEXT NOT NULL,sha256 TEXT NOT NULL,refs TEXT,absent INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(target,writer,seq));
CREATE TABLE external_lww_objects(target TEXT NOT NULL,hash TEXT NOT NULL,body BLOB,PRIMARY KEY(target,hash));
CREATE TABLE external_lww_receives(request_id TEXT PRIMARY KEY,target TEXT NOT NULL,body TEXT NOT NULL);
CREATE TABLE external_lww_versions(target TEXT NOT NULL,key TEXT NOT NULL,stamp TEXT NOT NULL,identity TEXT NOT NULL,writer TEXT NOT NULL,seq TEXT NOT NULL,PRIMARY KEY(target,key,stamp,writer,seq));
CREATE INDEX external_lww_versions_segment ON external_lww_versions(target,writer,seq);
"#;

#[cfg(test)]
mod publication_write_probe {
    use super::*;

    fn publication(size: usize) -> SealedPublication {
        SealedPublication {
            target: "synthetic-target".into(), writer: "synthetic-writer".into(), seq: 1.into(), authority: 0.into(),
            captured_at_ms: Some(1), object_id: "synthetic-object".into(), sha256: "b".repeat(64),
            payload_sha256: "a".repeat(64), payload: "x".repeat(size), sealed: true,
            entries: Vec::new(), bodies: Vec::new(), assets: Vec::new(), controls: Vec::new(),
            reused_control_catalogs: Vec::new(), reused_assets: Vec::new(), asset_job: None,
            data_catalogs: Vec::new(), asset_catalogs: Vec::new(), resume: None, dispatched: false, complete: false,
        }
    }

    #[test]
    fn publication_progress_preserves_legacy_json_without_rewriting_large_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut publication = publication(1024 * 1024);
        let sealed = vec![73u8; 1024 * 1024];
        store.device_store().unwrap().connection().execute(
            "INSERT INTO external_lww_segments VALUES(?1,?2,'1','0',?3,?4,0)",
            params![publication.target, publication.writer, serde_json::to_string(&publication).unwrap(), sealed],
        ).unwrap();
        publication.resume = Some(UploadState {data: crate::external_storage::contract::ResumeData::Secret("synthetic-session".into()),
            confirmed_offset: 17.into(), expires_at_ms: Some(999.into())});
        store.external_lww_persist(&publication, &sealed).unwrap();
        store.device_store().unwrap().connection().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        publication.resume.as_mut().unwrap().confirmed_offset = 1001.into();
        publication.dispatched = true;
        store.external_lww_persist(&publication, &sealed).unwrap();
        let wal = std::fs::metadata(directory.path().join("persistent/device.sqlite-wal")).unwrap().len();
        assert!(wal < 64 * 1024, "progress rewrote {wal} bytes of frozen publication content");
        let (stored, bytes) = store.external_lww_pending(&publication.target, &publication.writer).unwrap().unwrap();
        assert_eq!(serde_json::to_value(&stored).unwrap(), serde_json::to_value(&publication).unwrap());
        assert_eq!(bytes, sealed);
        let kind: String = store.device_store().unwrap().connection().query_row(
            "SELECT typeof(metadata) FROM external_lww_segments", [], |row|row.get(0)).unwrap();
        assert_eq!(kind, "text");
        store.device_store().unwrap().connection().execute("UPDATE external_lww_segments SET metadata=?1",
            [serde_json::to_string(&stored).unwrap()]).unwrap();
        assert!(store.external_lww_pending(&publication.target, &publication.writer).unwrap().unwrap().0.dispatched);
        store.device_store().unwrap().connection().execute("UPDATE external_lww_segments SET metadata='{'", []).unwrap();
        assert!(store.external_lww_persist(&publication, &sealed).is_err());
    }

    #[test]
    fn publication_validation_leaves_the_device_store_available_to_another_writer() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut publication = publication(1024 * 1024);
        let sealed = vec![73u8; 1024 * 1024];
        store.external_lww_persist(&publication, &sealed).unwrap();
        let writer = super::super::device_store::DeviceStore::open(&directory.path().join("persistent")).unwrap();
        writer.connection().busy_timeout(std::time::Duration::ZERO).unwrap();
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = outcome.clone();
        let mut attempted = false;
        store.device_store().unwrap().connection().authorizer(Some(move |context: AuthContext<'_>| {
            if !attempted && matches!(context.action, AuthAction::Read {table_name: "external_lww_segments", ..}) {
                attempted = true;
                *observed.lock().unwrap() = Some(writer.write_setting("nightlyWarned", &serde_json::json!(true)).map_err(|error|error.to_string()));
            }
            Authorization::Allow
        })).unwrap();
        publication.dispatched = true;
        store.external_lww_persist(&publication, &sealed).unwrap();
        store.device_store().unwrap().connection().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
        assert_eq!(outcome.lock().unwrap().take(), Some(Ok(())), "publication validation held the device writer lock");
        assert_eq!(store.device_store().unwrap().read_setting("nightlyWarned").unwrap(), Some(serde_json::json!(true)));
    }

    #[test]
    fn publication_refuses_a_state_change_after_its_validation_snapshot() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut publication = publication(4096);
        let sealed = vec![73u8; 4096];
        store.external_lww_persist(&publication, &sealed).unwrap();
        let writer = super::super::device_store::DeviceStore::open(&directory.path().join("persistent")).unwrap();
        let mut dispatched = publication.clone();
        dispatched.dispatched = true;
        let newer = serde_json::to_string(&dispatched).unwrap();
        let outcome = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = outcome.clone();
        let mut attempted = false;
        store.device_store().unwrap().connection().authorizer(Some(move |context: AuthContext<'_>| {
            if !attempted && matches!(context.action, AuthAction::Transaction {operation: TransactionOperation::Begin}) {
                attempted = true;
                *observed.lock().unwrap() = Some(writer.connection().execute("UPDATE external_lww_segments SET metadata=?1", [&newer]).map_err(|error|error.to_string()));
            }
            Authorization::Allow
        })).unwrap();
        publication.resume = Some(UploadState {data: crate::external_storage::contract::ResumeData::Secret("stale-session".into()),
            confirmed_offset: 17.into(), expires_at_ms: None});
        let error = store.external_lww_persist(&publication, &sealed).unwrap_err();
        store.device_store().unwrap().connection().authorizer(None::<fn(AuthContext<'_>) -> Authorization>).unwrap();
        assert_eq!(outcome.lock().unwrap().take(), Some(Ok(1)));
        assert_eq!(error.to_string(), "publication-state-changed");
        let (saved, saved_bytes) = store.external_lww_pending(&publication.target, &publication.writer).unwrap().unwrap();
        assert!(saved.dispatched);
        assert!(saved.resume.is_none());
        assert_eq!(saved_bytes, sealed);
    }

}
/// Key prefix in `external_lww_objects` of the coverage a binding stage
/// applied, kept until the first receive after its activation.
const STAGE_PREFIX: &str = "stage/";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UploadState {
    pub data: crate::external_storage::contract::ResumeData,
    pub confirmed_offset: DecimalU64,
    pub expires_at_ms: Option<DecimalU64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SealedBody {
    pub object_id: String,
    pub content_hash: String,
    /// Length and digest of the sealed body file; empty until it is sealed.
    pub byte_length: u64,
    pub sha256: String,
    pub resume: Option<UploadState>,
    pub complete: bool,
    pub locator: Option<crate::external_storage::contract::RemoteLocator>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrozenAsset {
    pub content_hash: String,
    pub byte_length: u64,
    pub local_pin: bool,
    pub remote_source: Option<crate::external_storage::lww_residency::FrozenBodySource>,
    pub server_source: Option<crate::server_sync::residency::RemoteObject>,
}
impl PartialEq for FrozenAsset {
    fn eq(&self, other: &Self) -> bool {
        self.content_hash == other.content_hash && self.byte_length == other.byte_length
            && self.local_pin == other.local_pin
            && match (serde_json::to_value((&self.remote_source, &self.server_source)), serde_json::to_value((&other.remote_source, &other.server_source))) {
                (Ok(left), Ok(right)) => left == right,
                _ => false,
            }
    }
}
impl Eq for FrozenAsset {}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrozenControl {
    pub content_hash: String,
    pub byte_length: u64,
    pub source: std::path::PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrozenControlCatalog {
    pub catalog: risunest_external_storage_format::snapshot::StoredObject,
    pub rooted_at_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "object", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum AssetReference {
    Catalog(risunest_external_storage_format::snapshot::StoredObject),
    Standalone(crate::external_storage::lww_segment::LargeBody),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FrozenAssetReference {
    pub content_hash: String,
    pub byte_length: u64,
    pub rooted_at_ms: u64,
    pub reference: AssetReference,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
enum AssetWitness {
    Catalog { content_hash: String, byte_length: u64, catalog_key: String },
    Standalone { content_hash: String, body: crate::external_storage::lww_segment::LargeBody, rooted_at_ms: Option<u64> },
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DataCatalogWitness {
    catalog: risunest_external_storage_format::snapshot::StoredObject,
    hashes: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlCatalogProof {
    catalog: risunest_external_storage_format::snapshot::StoredObject,
    rooted_at_ms: Option<u64>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlCatalogReference {
    content_hash: String,
    catalog_key: String,
}
impl std::fmt::Debug for FrozenAsset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("FrozenAsset").field("content_hash", &self.content_hash)
            .field("byte_length", &self.byte_length).field("local_pin", &self.local_pin)
            .field("remote_source", &self.remote_source.is_some())
            .field("server_source", &self.server_source.is_some()).finish()
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SealedPublication {
    pub target: String,
    pub writer: String,
    pub seq: DecimalU64,
    pub authority: DecimalU64,
    #[serde(deserialize_with = "required_capture_time")]
    pub captured_at_ms: Option<u64>,
    pub object_id: String,
    pub sha256: String,
    pub payload_sha256: String,
    pub payload: String,
    pub sealed: bool,
    pub entries: Vec<OutboxEntry>,
    pub bodies: Vec<SealedBody>,
    pub assets: Vec<FrozenAsset>,
    pub controls: Vec<FrozenControl>,
    pub reused_control_catalogs: Vec<FrozenControlCatalog>,
    pub reused_assets: Vec<FrozenAssetReference>,
    pub asset_job: Option<crate::external_storage::journal::JobIdentity>,
    pub data_catalogs: Vec<risunest_external_storage_format::snapshot::StoredObject>,
    pub asset_catalogs: Vec<risunest_external_storage_format::snapshot::StoredObject>,
    pub resume: Option<UploadState>,
    pub dispatched: bool,
    pub complete: bool,
}
fn required_capture_time<'de,D:serde::Deserializer<'de>>(deserializer:D) -> std::result::Result<Option<u64>,D::Error> {
    Option::<u64>::deserialize(deserializer)
}
/// What cleanup needs from a published segment without reading it again. The
/// segment name carries its hash, so an entry never goes stale.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SegmentReferences {
    pub byte_length: u64,
    pub plaintext_length: u64,
    pub plaintext_sha256: String,
    pub newest_physical_ms: u64,
    pub data_catalogs: Vec<risunest_external_storage_format::snapshot::StoredObject>,
    pub asset_catalogs: Vec<risunest_external_storage_format::snapshot::StoredObject>,
    pub large_bodies: std::collections::BTreeMap<String, crate::external_storage::lww_segment::LargeBody>,
}
/// The files a publication keeps until it lands: frozen controls and sealed
/// large bodies.
pub(crate) fn publication_directory(root: &std::path::Path, job_id: &str) -> std::path::PathBuf {
    root.join("external-storage").join("lww-publications").join(job_id)
}
pub(crate) fn sealed_body_path(root: &std::path::Path, job_id: &str, object_id: &str) -> std::path::PathBuf {
    publication_directory(root, job_id).join("bodies").join(object_id)
}
/// Removes what a publication kept on disk once nothing reads it again.
pub(crate) fn remove_publication_files(root: &std::path::Path, job_id: &str) -> StoreResult<()> {
    match std::fs::remove_dir_all(publication_directory(root, job_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
impl SealedPublication {
    pub(crate) fn acknowledgements(&self) -> StoreResult<Vec<AckEntry>> {
        self.entries
            .iter()
            .map(|entry| {
                let identity = entry.value.identity();
                #[cfg(test)]
                crate::external_storage::lww_segment::observe_value_identity(
                    "publish-ack.descriptor",
                    "publish-ack.identity",
                    &entry.value,
                    identity.is_ok(),
                );
                Ok(AckEntry {
                    key: entry.key.clone(),
                    version: entry.version.clone(),
                    stamp: entry.stamp.clone(),
                    value_identity: identity.map_err(super::lww::error)?,
                })
            })
            .collect()
    }
}
pub(super) fn carry_pending_publications(db: &rusqlite::Connection, old: DecimalU64, new: DecimalU64) -> StoreResult<()> {
    let pending: Vec<(String, String, String, String)> = {
        let mut statement = db.prepare("SELECT target,writer,seq,metadata FROM external_lww_segments WHERE complete=0 AND authority=?1")?;
        let rows = statement
            .query_map([old.0.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    for (target, writer, seq, metadata) in pending {
        let mut publication: SealedPublication = serde_json::from_str(&metadata)?;
        publication.authority = new;
        for entry in &mut publication.entries {
            entry.target_authority = new;
        }
        db.execute(
            "UPDATE external_lww_segments SET authority=?4,metadata=?5 WHERE target=?1 AND writer=?2 AND seq=?3",
            params![target, writer, seq, new.0.to_string(), serde_json::to_string(&publication)?],
        )?;
    }
    Ok(())
}
/// Releases a segment's file protection. A journal that is already gone was
/// released before.
pub(crate) fn release_cas_job(
    root: &std::path::Path,
    job_id: &str,
    outcome: crate::asset_repository::job_pins::CasReleaseOutcome,
) -> StoreResult<()> {
    match crate::asset_repository::job_pins::DurableCasJob::open(root, job_id) {
        Ok(mut job) => job.release(outcome).map_err(super::lww::error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(super::lww::error(error)),
    }
}
impl PersistentStore {
    pub(super) fn external_lww_active_asset_roots(&self) -> StoreResult<crate::asset_repository::migration_gc::AssetRootSet> {
        let device = self.device_store()?;
        let db = device.connection();
        if db.query_row("SELECT 1 FROM external_lww_segments WHERE complete=0 AND (json_type(metadata,'$.assets') IS NOT 'array' OR json_type(metadata,'$.reusedAssets') IS NOT 'array') LIMIT 1", [], |_| Ok(())).optional()?.is_some() {
            return Err(super::lww::error("captured-asset-roots-integrity"));
        }
        let mut roots = crate::asset_repository::migration_gc::AssetRootSet::default();
        let mut statement = db.prepare("SELECT json_extract(asset.value,'$.contentHash') FROM external_lww_segments publication,json_each(publication.metadata,'$.assets') asset WHERE publication.complete=0
            UNION SELECT json_extract(asset.value,'$.contentHash') FROM external_lww_segments publication,json_each(publication.metadata,'$.reusedAssets') asset WHERE publication.complete=0")?;
        for hash in statement.query_map([], |row| row.get::<_, String>(0))? {
            let hash = hash?;
            risunest_sync_wire::validate_hash(&hash).map_err(super::lww::error)?;
            roots.object_hashes.insert(hash);
        }
        Ok(roots)
    }
    pub(crate) fn external_lww_unpublished_entries(
        &self,
        expected: DecimalU64,
    ) -> StoreResult<Vec<OutboxEntry>> {
        if self.lww_binding_authority()? != expected {
            return Err(super::lww::error("stale-binding-authority"));
        }
        let mut entries = Vec::new();
        for db in [&self.connection, self.device_store()?.connection()] {
            let mut statement =
                db.prepare("SELECT key,stamp,value,version FROM lww_outbox WHERE authority=?1")?;
            let rows = statement.query_map([expected.0.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (key, stamp, value, version) = row?;
                entries.push(OutboxEntry {
                    key: key.try_into().map_err(super::lww::error)?,
                    stamp: serde_json::from_str(&stamp)?,
                    value: serde_json::from_str(&value)?,
                    version,
                    target_authority: expected,
                });
            }
        }
        let mut statement = self
            .device_store()?
            .connection()
            .prepare("SELECT metadata FROM external_lww_segments")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let publication: SealedPublication = serde_json::from_str(&row?)?;
            if publication.dispatched
                && publication.entries.iter().any(|published| {
                    entries.iter().any(|entry| {
                        published.key == entry.key
                            && published.version == entry.version
                            && published.stamp == entry.stamp
                            && published.value == entry.value
                    })
                })
            {
                return Err(super::lww::error("publication-unsettled"));
            }
        }
        Ok(entries)
    }
    pub(crate) fn external_lww_root(&self) -> std::path::PathBuf {
        self.repository_root.clone()
    }
    pub(crate) fn external_lww_abandon_unsent(
        &mut self,
        target: &str,
        writer: &str,
    ) -> StoreResult<()> {
        if let Some((pending, _)) = self.external_lww_pending(target, writer)? {
            if pending.dispatched {
                return Err(super::lww::error("publication-unsettled"));
            }
            self.device_store()?.connection().execute(
                "DELETE FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
                params![target, writer, pending.seq.0.to_string()],
            )?;
            if let Some(job) = &pending.asset_job {
                remove_publication_files(&self.repository_root, &job.job_id)?;
            }
        }
        Ok(())
    }
    /// Settles a publication that a binding switch left under an earlier
    /// authority. One the repository holds keeps its sequence; any other is
    /// removed so the sequence goes to the next publication.
    pub(crate) fn external_lww_settle_detached(
        &mut self,
        publication: &SealedPublication,
        landed: Option<&SegmentReferences>,
    ) -> StoreResult<()> {
        let seq = publication.seq.0.to_string();
        let tx = self.device_store_mut()?.transaction()?;
        let authority: String = tx.query_row(
            "SELECT binding_authority FROM lww_clock WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        let stored: Option<(String, String)> = tx
            .query_row(
                "SELECT authority,metadata FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
                params![publication.target, publication.writer, seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((stored_authority, metadata)) = stored else {
            return Err(super::lww::error("detached-publication-integrity"));
        };
        let stored: SealedPublication = serde_json::from_str(&metadata)?;
        if stored_authority == authority
            || stored_authority != publication.authority.0.to_string()
            || stored.object_id != publication.object_id
            || stored.sha256 != publication.sha256
            || stored.dispatched != publication.dispatched
            || stored.complete != publication.complete
        {
            return Err(super::lww::error("detached-publication-integrity"));
        }
        let next: Option<String> = tx
            .query_row(
                "SELECT next_seq FROM external_lww_sequences WHERE target=?1 AND writer=?2",
                params![publication.target, publication.writer],
                |r| r.get(0),
            )
            .optional()?;
        let next = next
            .map(|value| value.try_into().map(|value: DecimalU64| value.0).map_err(super::lww::error))
            .unwrap_or(Ok(1))?;
        if next != publication.seq.0 {
            return Err(super::lww::error("detached-publication-integrity"));
        }
        let seen: Option<String> = tx
            .query_row(
                "SELECT sha256 FROM external_lww_seen WHERE target=?1 AND writer=?2 AND seq=?3",
                params![publication.target, publication.writer, seq],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(references) = landed {
            if seen.as_deref().is_some_and(|hash| hash != publication.sha256) {
                return Err(super::lww::error("writer-sequence-integrity"));
            }
            let following = publication
                .seq
                .0
                .checked_add(1)
                .ok_or_else(|| super::lww::error("sequence-overflow"))?;
            insert_seen(&tx, &publication.target, &publication.writer, publication.seq.0, &publication.sha256, Some(references))?;
            tx.execute(
                "INSERT INTO external_lww_sequences(target,writer,next_seq) VALUES(?1,?2,?3)
                ON CONFLICT(target,writer) DO UPDATE SET next_seq=excluded.next_seq",
                params![publication.target, publication.writer, following.to_string()],
            )?;
            // The sequence and seen rows now hold what this row carried.
            tx.execute(
                "DELETE FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
                params![publication.target, publication.writer, seq],
            )?;
        } else {
            // A sequence some segment already used is never given to another.
            if seen.is_some() {
                return Err(super::lww::error("writer-sequence-integrity"));
            }
            tx.execute(
                "DELETE FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
                params![publication.target, publication.writer, seq],
            )?;
        }
        tx.commit()?;
        if let Some(job) = &stored.asset_job {
            remove_publication_files(&self.repository_root, &job.job_id)?;
        }
        Ok(())
    }
    /// Forgets what this device was sending to a removed connection's
    /// repository. Settled and unsent segments go; a sent one that no answer
    /// confirmed stays without its files, so returning to that repository
    /// never gives its sequence to another segment.
    pub(crate) fn external_lww_forget_target(&mut self, target: &str) -> StoreResult<()> {
        let rows: Vec<(String, String, String)> = {
            let mut statement = self
                .device_store()?
                .connection()
                .prepare("SELECT writer,seq,metadata FROM external_lww_segments WHERE target=?1")?;
            let rows = statement
                .query_map([target], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        let mut kept = Vec::new();
        let mut dropped = Vec::new();
        let mut jobs = Vec::new();
        for (writer, seq, metadata) in rows {
            let mut publication: SealedPublication = serde_json::from_str(&metadata)?;
            jobs.extend(publication.asset_job.as_ref().map(|job| job.job_id.clone()));
            if !publication.complete {
                if let Some(job) = publication.asset_job.take() {
                    release_cas_job(&self.repository_root, &job.job_id, crate::asset_repository::job_pins::CasReleaseOutcome::Aborted)?;
                }
            }
            if publication.dispatched && !publication.complete {
                publication.assets.clear();
                publication.reused_assets.clear();
                kept.push((writer, seq, serde_json::to_string(&publication)?));
            } else {
                dropped.push((writer, seq));
            }
        }
        let tx = self.device_store_mut()?.transaction()?;
        for (writer, seq) in dropped {
            tx.execute(
                "DELETE FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
                params![target, writer, seq],
            )?;
        }
        for (writer, seq, metadata) in kept {
            tx.execute(
                "UPDATE external_lww_segments SET metadata=?4 WHERE target=?1 AND writer=?2 AND seq=?3",
                params![target, writer, seq, metadata],
            )?;
        }
        tx.execute("DELETE FROM external_lww_receives WHERE target=?1", [target])?;
        tx.commit()?;
        // A kept segment was sent whole, so none of these files is read again.
        for job in jobs {
            remove_publication_files(&self.repository_root, &job)?;
        }
        Ok(())
    }
    /// Releases the file protection of every unfinished segment a retired
    /// writer captured and removes the files its segments kept. The rows go
    /// with the writer change that retires it.
    pub(super) fn external_lww_release_writer_jobs(&self, writer: &str) -> StoreResult<()> {
        let mut statement = self
            .device_store()?
            .connection()
            .prepare("SELECT metadata,complete FROM external_lww_segments WHERE writer=?1")?;
        let rows = statement
            .query_map([writer], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (metadata, complete) in rows {
            let publication: SealedPublication = serde_json::from_str(&metadata)?;
            if let Some(job) = &publication.asset_job {
                if !complete {
                    release_cas_job(&self.repository_root, &job.job_id, crate::asset_repository::job_pins::CasReleaseOutcome::Aborted)?;
                }
                remove_publication_files(&self.repository_root, &job.job_id)?;
            }
        }
        Ok(())
    }
    /// Whether the current binding has a segment this device sent that no
    /// answer has confirmed.
    pub(crate) fn external_lww_unconfirmed_dispatch(&self) -> StoreResult<bool> {
        let authority = self.lww_binding_authority()?;
        let writer = self.lww_clock_state()?.writer_id;
        let device = self.device_store()?;
        let mut statement = device.connection().prepare(
            "SELECT metadata FROM external_lww_segments WHERE complete=0 AND authority=?1 AND writer=?2",
        )?;
        let rows = statement.query_map(params![authority.0.to_string(), writer], |row| row.get::<_, String>(0))?;
        for row in rows {
            let publication: SealedPublication = serde_json::from_str(&row?)?;
            if publication.dispatched {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// Stores a receive page unless it is finished. A page stored before is
    /// kept as it was, so a page already staged is offered unchanged. Returns
    /// the id of an unfinished page.
    pub(crate) fn external_lww_stable_receive(
        &self,
        target: &str,
        request: &super::lww::StageReceive,
    ) -> StoreResult<Option<String>> {
        let id = &request.header.request_id;
        if self.external_lww_receive_finished(id)? {
            self.device_store()?.connection().execute("DELETE FROM external_lww_receives WHERE request_id=?1", [id])?;
            return Ok(None);
        }
        self.device_store()?.connection().execute(
            "INSERT OR IGNORE INTO external_lww_receives(request_id,target,body) VALUES(?1,?2,?3)",
            params![id, target, serde_json::to_string(request)?],
        )?;
        Ok(Some(id.clone()))
    }
    /// The stored receive page with this id, unless it is missing or finished.
    /// A finished page is removed.
    pub(crate) fn external_lww_unfinished_receive(
        &self,
        request_id: &str,
    ) -> StoreResult<Option<super::lww::StageReceive>> {
        let body: Option<String> = self
            .device_store()?
            .connection()
            .query_row(
                "SELECT body FROM external_lww_receives WHERE request_id=?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(body) = body else { return Ok(None) };
        if self.external_lww_receive_finished(request_id)? {
            self.device_store()?.connection().execute("DELETE FROM external_lww_receives WHERE request_id=?1", [request_id])?;
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&body)?))
    }
    /// Removes the pages of `target` that the latest listing no longer offers:
    /// finished ones and those of segments another receive already covered.
    pub(crate) fn external_lww_retain_receives(
        &self,
        target: &str,
        offered: &std::collections::BTreeSet<String>,
    ) -> StoreResult<()> {
        let db = self.device_store()?.connection();
        let stored = {
            let mut statement = db.prepare("SELECT request_id FROM external_lww_receives WHERE target=?1")?;
            let rows = statement.query_map([target], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        for id in stored.iter().filter(|id| !offered.contains(*id)) {
            db.execute("DELETE FROM external_lww_receives WHERE request_id=?1", [id])?;
        }
        Ok(())
    }
    pub(crate) fn external_lww_receive_finished(&self, request_id: &str) -> StoreResult<bool> {
        Ok(self.device_store()?.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM lww_receive WHERE request_id=?1 AND finished=1)",
            [request_id],
            |r| r.get(0),
        )?)
    }
    /// Records the versions a segment carries and refuses one whose stamp
    /// another value already holds.
    pub(crate) fn external_lww_verify_versions(
        &mut self,
        target: &str,
        writer: &str,
        seq: u64,
        changes: &[super::lww::Change],
    ) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        let seq = seq.to_string();
        for change in changes {
            let stamp = serde_json::to_string(&change.stamp)?;
            let identity = change.value.identity();
            #[cfg(test)]
            crate::external_storage::lww_segment::observe_value_identity(
                "receive-history.descriptor",
                "receive-history.identity",
                &change.value,
                identity.is_ok(),
            );
            let identity = identity.map_err(super::lww::error)?;
            let conflict: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM external_lww_versions WHERE target=?1 AND key=?2 AND stamp=?3 AND identity<>?4)",
                params![target, change.key.as_str(), stamp, identity],
                |r| r.get(0),
            )?;
            if conflict {
                return Err(super::lww::error("equal-stamp-integrity"));
            }
            tx.execute(
                "INSERT OR IGNORE INTO external_lww_versions(target,key,stamp,identity,writer,seq) VALUES(?1,?2,?3,?4,?5,?6)",
                params![target, change.key.as_str(), stamp, identity, writer, seq],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Every version this device recorded for `target`, as key, stamp and
    /// value identity.
    pub(crate) fn external_lww_visit_versions(
        &self,
        target: &str,
        visit: &mut dyn FnMut(&str, &str, &str) -> StoreResult<()>,
    ) -> StoreResult<()> {
        let db = self.device_store()?.connection();
        let mut statement = db.prepare("SELECT key,stamp,identity FROM external_lww_versions WHERE target=?1")?;
        let mut rows = statement.query([target])?;
        while let Some(row) = rows.next()? {
            visit(&row.get::<_, String>(0)?, &row.get::<_, String>(1)?, &row.get::<_, String>(2)?)?;
        }
        Ok(())
    }
    pub(crate) fn external_lww_register_source(
        &self,
        source: &crate::external_storage::lww_residency::Source,
    ) -> StoreResult<()> {
        crate::external_storage::lww_residency::register(&self.repository_root, source)
            .map_err(super::lww::error)
    }
    pub(crate) fn external_lww_object_is_local(&self, hash: &str) -> StoreResult<bool> {
        Ok(
            crate::asset_repository::PayloadCas::new(&self.repository_root)?
                .open_object(hash)?
                .is_some(),
        )
    }
    pub(crate) fn external_lww_object_is_control(&self, hash: &str) -> StoreResult<bool> {
        for connection in [&self.connection, self.device_store()?.connection()] {
            if connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)",
                [hash],
                |r| r.get(0),
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(crate) fn external_lww_witness_object(&self, target: &str, hash: &str) -> StoreResult<()> {
        self.device_store()?.connection().execute(
            "INSERT OR IGNORE INTO external_lww_objects(target,hash) VALUES(?1,?2)",
            params![target, hash],
        )?;
        Ok(())
    }
    fn external_lww_data_catalog_key(catalog: &risunest_external_storage_format::snapshot::StoredObject) -> StoreResult<String> {
        catalog.validate().map_err(super::lww::error)?;
        if catalog.header.role != risunest_external_storage_format::snapshot::ObjectRole::Catalog {
            return Err(super::lww::error("invalid-control-catalog"));
        }
        // Catalog proofs cannot collide with canonical control hash witnesses.
        Ok(format!("data/{}", hex::encode(catalog.ciphertext_sha256)))
    }
    fn external_lww_data_catalog_witness(&self, target: &str, key: &str) -> StoreResult<Option<DataCatalogWitness>> {
        let body: Option<Option<Vec<u8>>> = self.device_store()?.connection().query_row(
            "SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2", params![target,key], |row|row.get(0),
        ).optional()?;
        body.flatten().map(|body| {
            let witness:DataCatalogWitness=serde_json::from_slice(&body)?;
            if Self::external_lww_data_catalog_key(&witness.catalog)?!=key
                || witness.hashes.windows(2).any(|pair|pair[0]>=pair[1]) {
                return Err(super::lww::error("control-catalog-witness-integrity"));
            }
            for hash in &witness.hashes {risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;}
            Ok(witness)
        }).transpose()
    }
    fn external_lww_control_catalog_key(catalog: &risunest_external_storage_format::snapshot::StoredObject) -> StoreResult<String> {
        Self::external_lww_data_catalog_key(catalog)?;
        Ok(format!("data-proof/{}",hex::encode(catalog.ciphertext_sha256)))
    }
    fn external_lww_asset_catalog_key(catalog:&risunest_external_storage_format::snapshot::StoredObject) -> StoreResult<String> {
        Self::external_lww_data_catalog_key(catalog)?;
        Ok(format!("asset-proof/{}",hex::encode(catalog.ciphertext_sha256)))
    }
    // Routine reuse and renewal read a bounded proof, not the complete admission inventory.
    fn external_lww_control_catalog_proof(&self,target:&str,key:&str) -> StoreResult<Option<ControlCatalogProof>> {
        self.external_lww_catalog_proof(target,key,risunest_external_storage_format::snapshot::CatalogKind::Records)
    }
    fn external_lww_catalog_proof(&self,target:&str,key:&str,kind:risunest_external_storage_format::snapshot::CatalogKind) -> StoreResult<Option<ControlCatalogProof>> {
        let body:Option<Option<Vec<u8>>>=self.device_store()?.connection().query_row(
            "SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2",params![target,key],|row|row.get(0),
        ).optional()?;
        body.flatten().map(|body| {
            let proof:ControlCatalogProof=serde_json::from_slice(&body)?;
            let expected=match kind {
                risunest_external_storage_format::snapshot::CatalogKind::Records=>Self::external_lww_control_catalog_key(&proof.catalog)?,
                risunest_external_storage_format::snapshot::CatalogKind::Assets=>Self::external_lww_asset_catalog_key(&proof.catalog)?,
                _=>return Err(super::lww::error("catalog-proof-kind-integrity")),
            };
            if expected!=key {
                return Err(super::lww::error("control-catalog-witness-integrity"));
            }
            Ok(proof)
        }).transpose()
    }
    pub(crate) fn external_lww_verified_data_catalog(&self, target:&str, catalog:&risunest_external_storage_format::snapshot::StoredObject) -> StoreResult<Option<Vec<String>>> {
        let key=Self::external_lww_data_catalog_key(catalog)?;
        self.external_lww_data_catalog_witness(target,&key)?.map(|witness| {
            if witness.catalog!=*catalog {return Err(super::lww::error("control-catalog-witness-integrity"));}
            Ok(witness.hashes)
        }).transpose()
    }
    pub(crate) fn external_lww_verified_control_size(&self, hash:&str) -> StoreResult<Option<u64>> {
        risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;
        // A marked control may be collected, so it is fetched again instead.
        let size:Option<i64>=self.connection.query_row("SELECT length(o.body) FROM message_page_objects o
            JOIN message_page_verified_objects v ON v.hash=o.hash WHERE o.hash=?1
            AND NOT EXISTS(SELECT 1 FROM message_page_object_marks m WHERE m.hash=o.hash)",[hash],|row|row.get(0)).optional()?;
        size.map(|size|u64::try_from(size).map_err(super::lww::error)).transpose()
    }
    fn external_lww_store_data_catalog_witness(&self,target:&str,witness:&DataCatalogWitness) -> StoreResult<()> {
        let key=Self::external_lww_data_catalog_key(&witness.catalog)?;
        let proof_key=Self::external_lww_control_catalog_key(&witness.catalog)?;
        let proof=if let Some(previous)=self.external_lww_control_catalog_proof(target,&proof_key)? {
            if previous.catalog!=witness.catalog {return Err(super::lww::error("control-catalog-witness-integrity"));}
            previous
        } else {ControlCatalogProof{catalog:witness.catalog.clone(),rooted_at_ms:None}};
        let tx=self.device_store()?.connection().unchecked_transaction()?;
        tx.execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
            ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,key,serde_json::to_vec(witness)?])?;
        tx.execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
            ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,proof_key,serde_json::to_vec(&proof)?])?;
        for hash in &witness.hashes {
            let reference=ControlCatalogReference{content_hash:hash.clone(),catalog_key:proof_key.clone()};
            tx.execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
                ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,hash,serde_json::to_vec(&reference)?])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_lww_witness_data_catalog(&self,target:&str,catalog:&risunest_external_storage_format::snapshot::StoredObject,hashes:&[String]) -> StoreResult<()> {
        let key=Self::external_lww_data_catalog_key(catalog)?;
        let mut hashes=hashes.to_vec(); hashes.sort(); hashes.dedup();
        for hash in &hashes {risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;}
        if let Some(previous)=self.external_lww_data_catalog_witness(target,&key)? {
            if previous.catalog!=*catalog || previous.hashes!=hashes {return Err(super::lww::error("control-catalog-witness-integrity"));}
        }
        self.external_lww_store_data_catalog_witness(target,&DataCatalogWitness{catalog:catalog.clone(),hashes})
    }
    pub(crate) fn external_lww_authorize_data_catalog(&self,target:&str,catalog:&risunest_external_storage_format::snapshot::StoredObject,rooted_at_ms:u64) -> StoreResult<()> {
        self.external_lww_authorize_catalog_proof(target,catalog,rooted_at_ms,risunest_external_storage_format::snapshot::CatalogKind::Records)
    }
    pub(crate) fn external_lww_authorize_asset_catalog(&self,target:&str,catalog:&risunest_external_storage_format::snapshot::StoredObject,rooted_at_ms:u64) -> StoreResult<()> {
        self.external_lww_authorize_catalog_proof(target,catalog,rooted_at_ms,risunest_external_storage_format::snapshot::CatalogKind::Assets)
    }
    fn external_lww_authorize_catalog_proof(&self,target:&str,catalog:&risunest_external_storage_format::snapshot::StoredObject,rooted_at_ms:u64,kind:risunest_external_storage_format::snapshot::CatalogKind) -> StoreResult<()> {
        let key=match kind {
            risunest_external_storage_format::snapshot::CatalogKind::Records=>Self::external_lww_control_catalog_key(catalog)?,
            risunest_external_storage_format::snapshot::CatalogKind::Assets=>Self::external_lww_asset_catalog_key(catalog)?,
            _=>return Err(super::lww::error("catalog-proof-kind-integrity")),
        };
        let mut proof=self.external_lww_catalog_proof(target,&key,kind)?.ok_or_else(||super::lww::error("control-catalog-witness-missing"))?;
        if proof.catalog!=*catalog {return Err(super::lww::error("control-catalog-witness-integrity"));}
        proof.rooted_at_ms=Some(proof.rooted_at_ms.map_or(rooted_at_ms,|previous|previous.max(rooted_at_ms)));
        self.device_store()?.connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
            params![serde_json::to_vec(&proof)?,target,key])?;
        Ok(())
    }
    pub(crate) fn external_lww_reusable_control_catalog(&self,target:&str,hash:&str,now_ms:u64) -> StoreResult<Option<FrozenControlCatalog>> {
        risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;
        let body:Option<Option<Vec<u8>>>=self.device_store()?.connection().query_row("SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2",
            params![target,hash],|row|row.get(0)).optional()?;
        let Some(body)=body.flatten() else {return Ok(None)};
        let reference:ControlCatalogReference=serde_json::from_slice(&body)?;
        if reference.content_hash!=hash {return Err(super::lww::error("control-catalog-witness-integrity"));}
        let proof=self.external_lww_control_catalog_proof(target,&reference.catalog_key)?.ok_or_else(||super::lww::error("control-catalog-witness-missing"))?;
        let Some(rooted_at_ms)=proof.rooted_at_ms else {return Ok(None)};
        if now_ms.checked_sub(rooted_at_ms).is_none_or(|age|age>crate::external_storage::leases::CACHE_REUSE_LIMIT_MS) {return Ok(None);}
        Ok(Some(FrozenControlCatalog{catalog:proof.catalog,rooted_at_ms}))
    }
    fn external_lww_asset_witness(&self,target:&str,hash:&str) -> StoreResult<Option<AssetWitness>> {
        risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;
        let body:Option<Option<Vec<u8>>>=self.device_store()?.connection().query_row("SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2",
            params![target,format!("asset/{hash}")],|row|row.get(0)).optional()?;
        body.flatten().map(|body| {
            let witness:AssetWitness=serde_json::from_slice(&body)?;
            let content_hash=match &witness {AssetWitness::Catalog{content_hash,..}|AssetWitness::Standalone{content_hash,..}=>content_hash};
            if content_hash!=hash {return Err(super::lww::error("asset-witness-integrity"));}
            if let AssetWitness::Standalone{body,..}=&witness {Self::external_lww_validate_standalone(body)?;}
            Ok(witness)
        }).transpose()
    }
    fn external_lww_validate_standalone(body:&crate::external_storage::lww_segment::LargeBody) -> StoreResult<()> {
        risunest_sync_wire::validate_hash(&body.sha256).map_err(super::lww::error)?;
        if uuid::Uuid::parse_str(&body.object_id).map_err(super::lww::error)?.to_string()!=body.object_id
            || body.byte_length.0==0 || body.locator.is_none() {
            return Err(super::lww::error("asset-witness-integrity"));
        }
        Ok(())
    }
    pub(crate) fn external_lww_witness_asset_catalog(&self,target:&str,catalog:&risunest_external_storage_format::snapshot::StoredObject,entries:&[(String,u64)]) -> StoreResult<()> {
        let key=Self::external_lww_asset_catalog_key(catalog)?;
        let proof=if let Some(previous)=self.external_lww_catalog_proof(target,&key,risunest_external_storage_format::snapshot::CatalogKind::Assets)? {
            if previous.catalog!=*catalog {return Err(super::lww::error("asset-witness-integrity"));}
            previous
        } else {ControlCatalogProof{catalog:catalog.clone(),rooted_at_ms:None}};
        let mut sizes=std::collections::BTreeMap::new();
        for (hash,size) in entries {
            if sizes.insert(hash,*size).is_some_and(|previous|previous!=*size) {return Err(super::lww::error("asset-witness-size-integrity"));}
            if let Some(previous)=self.external_lww_asset_witness(target,hash)? {
                let previous_size=match previous {AssetWitness::Catalog{byte_length,..}=>byte_length,AssetWitness::Standalone{body,..}=>body.plaintext_byte_length.0};
                if previous_size!=*size {return Err(super::lww::error("asset-witness-size-integrity"));}
            }
        }
        let tx=self.device_store()?.connection().unchecked_transaction()?;
        tx.execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
            ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,key,serde_json::to_vec(&proof)?])?;
        for (hash,size) in entries {
            let witness=AssetWitness::Catalog{content_hash:hash.clone(),byte_length:*size,catalog_key:key.clone()};
            tx.execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
                ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,format!("asset/{hash}"),serde_json::to_vec(&witness)?])?;
        }
        tx.commit()?;Ok(())
    }
    pub(crate) fn external_lww_witness_standalone(&self,target:&str,hash:&str,body:&crate::external_storage::lww_segment::LargeBody) -> StoreResult<()> {
        risunest_sync_wire::validate_hash(hash).map_err(super::lww::error)?;Self::external_lww_validate_standalone(body)?;
        let rooted_at_ms=match self.external_lww_asset_witness(target,hash)? {
            Some(AssetWitness::Standalone{body:previous,rooted_at_ms,..})=>{
                if previous.plaintext_byte_length!=body.plaintext_byte_length {return Err(super::lww::error("asset-witness-size-integrity"));}
                if previous==*body {rooted_at_ms}else{None}
            }
            Some(AssetWitness::Catalog{byte_length,..}) if byte_length!=body.plaintext_byte_length.0=>return Err(super::lww::error("asset-witness-size-integrity")),
            _=>None,
        };
        let witness=AssetWitness::Standalone{content_hash:hash.into(),body:body.clone(),rooted_at_ms};
        self.device_store()?.connection().execute("INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)
            ON CONFLICT(target,hash) DO UPDATE SET body=excluded.body",params![target,format!("asset/{hash}"),serde_json::to_vec(&witness)?])?;
        Ok(())
    }
    pub(crate) fn external_lww_authorize_standalone(&self,target:&str,hash:&str,body:&crate::external_storage::lww_segment::LargeBody,rooted_at_ms:u64) -> StoreResult<()> {
        let Some(AssetWitness::Standalone{body:previous,rooted_at_ms:previous_at,..})=self.external_lww_asset_witness(target,hash)? else {return Err(super::lww::error("asset-witness-missing"))};
        if previous!=*body {return Err(super::lww::error("asset-witness-integrity"));}
        let witness=AssetWitness::Standalone{content_hash:hash.into(),body:body.clone(),rooted_at_ms:Some(previous_at.map_or(rooted_at_ms,|previous|previous.max(rooted_at_ms)))};
        self.device_store()?.connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
            params![serde_json::to_vec(&witness)?,target,format!("asset/{hash}")])?;Ok(())
    }
    pub(crate) fn external_lww_reusable_asset(&self,target:&str,hash:&str,now_ms:u64) -> StoreResult<Option<FrozenAssetReference>> {
        let Some(witness)=self.external_lww_asset_witness(target,hash)? else {return Ok(None)};
        let (byte_length,rooted_at_ms,reference)=match witness {
            AssetWitness::Catalog{byte_length,catalog_key,..}=>{
                let proof=self.external_lww_catalog_proof(target,&catalog_key,risunest_external_storage_format::snapshot::CatalogKind::Assets)?.ok_or_else(||super::lww::error("asset-witness-missing"))?;
                (byte_length,proof.rooted_at_ms,AssetReference::Catalog(proof.catalog))
            }
            AssetWitness::Standalone{body,rooted_at_ms,..}=>(body.plaintext_byte_length.0,rooted_at_ms,AssetReference::Standalone(body)),
        };
        let Some(rooted_at_ms)=rooted_at_ms else {return Ok(None)};
        if now_ms.checked_sub(rooted_at_ms).is_none_or(|age|age>crate::external_storage::leases::CACHE_REUSE_LIMIT_MS) {return Ok(None);}
        Ok(Some(FrozenAssetReference{content_hash:hash.into(),byte_length,rooted_at_ms,reference}))
    }
    pub(crate) fn external_lww_next_sequence(
        &self,
        target: &str,
        writer: &str,
    ) -> StoreResult<u64> {
        let sequence: Option<String> = self
            .device_store()?
            .connection()
            .query_row(
                "SELECT next_seq FROM external_lww_sequences WHERE target=?1 AND writer=?2",
                params![target, writer],
                |r| r.get(0),
            )
            .optional()?;
        sequence
            .map(|value| {
                value
                    .try_into()
                    .map(|value: DecimalU64| value.0)
                    .map_err(super::lww::error)
            })
            .unwrap_or(Ok(1))
    }
    pub(crate) fn external_lww_pending(
        &self,
        target: &str,
        writer: &str,
    ) -> StoreResult<Option<(SealedPublication, Vec<u8>)>> {
        let seq = self.external_lww_next_sequence(target, writer)?;
        let found: Option<(String,Vec<u8>)> = self.device_store()?.connection().query_row(
            "SELECT metadata,sealed FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
            params![target,writer,seq.to_string()], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        found
            .map(|(metadata, bytes)| Ok((serde_json::from_str(&metadata)?, bytes)))
            .transpose()
    }
    pub(crate) fn external_lww_persist(
        &mut self,
        publication: &SealedPublication,
        bytes: &[u8],
    ) -> StoreResult<()> {
        if publication.dispatched && !publication.sealed {return Err(super::lww::error("unsealed-dispatch-integrity"));}
        if publication.assets.iter().any(|asset| {
            u8::from(asset.local_pin) + u8::from(asset.remote_source.is_some()) + u8::from(asset.server_source.is_some()) != 1
                || asset.server_source.as_ref().is_some_and(|source| source.hash != asset.content_hash || source.size != asset.byte_length)
        }) {
            return Err(super::lww::error("captured-asset-source-integrity"));
        }
        for control in &publication.controls {
            risunest_sync_wire::validate_hash(&control.content_hash).map_err(super::lww::error)?;
            let job=publication.asset_job.as_ref().ok_or_else(||super::lww::error("captured-control-job-missing"))?;
            let expected=self.repository_root.join("external-storage").join("lww-publications")
                .join(&job.job_id).join("controls").join(&control.content_hash);
            // Written under the data root of the run that froze it.
            if crate::app_paths::relocated_external_storage_path(&control.source,&self.repository_root)!=expected {
                return Err(super::lww::error("captured-control-source-integrity"));
            }
        }
        if !publication.reused_control_catalogs.is_empty() && publication.asset_job.is_none() {
            return Err(super::lww::error("captured-control-job-missing"));
        }
        for control in &publication.reused_control_catalogs {Self::external_lww_data_catalog_key(&control.catalog)?;}
        if !publication.reused_assets.is_empty() && publication.asset_job.is_none() {
            return Err(super::lww::error("captured-asset-job-missing"));
        }
        for asset in &publication.reused_assets {
            risunest_sync_wire::validate_hash(&asset.content_hash).map_err(super::lww::error)?;
            match &asset.reference {
                AssetReference::Catalog(catalog)=>{Self::external_lww_asset_catalog_key(catalog)?;}
                AssetReference::Standalone(body)=>{
                    Self::external_lww_validate_standalone(body)?;
                    if body.plaintext_byte_length.0!=asset.byte_length {return Err(super::lww::error("captured-asset-size-integrity"));}
                }
            }
        }
        if publication.bodies.iter().any(|body| {
            if body.sha256.is_empty() {
                body.byte_length != 0 || body.resume.is_some()
                    || body.complete || body.locator.is_some() || publication.sealed
            } else {
                risunest_sync_wire::validate_hash(&body.sha256).is_err() || body.byte_length == 0
                    || (body.complete && body.locator.is_none())
            }
        }) {
            return Err(super::lww::error("sealed-body-integrity"));
        }
        // Validate and encode before taking the writer lock; the update below
        // must still match this exact snapshot when the lock is acquired.
        let previous: Option<(Vec<u8>,String)> = self.device_store()?.connection().query_row("SELECT sealed,metadata FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
            params![publication.target,publication.writer,publication.seq.0.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((sealed, metadata)) = &previous {
            let previous: SealedPublication = serde_json::from_str(metadata)?;
            if previous.sealed && sealed.as_slice() != bytes {
                return Err(super::lww::error("sealed-segment-integrity"));
            }
            if previous.entries != publication.entries {
                return Err(super::lww::error("captured-version-integrity"));
            }
            if previous.captured_at_ms != publication.captured_at_ms {
                return Err(super::lww::error("captured-time-integrity"));
            }
            if previous.assets != publication.assets || previous.asset_job != publication.asset_job {
                return Err(super::lww::error("captured-asset-integrity"));
            }
            if previous.controls != publication.controls {
                return Err(super::lww::error("captured-control-integrity"));
            }
            if previous.reused_control_catalogs != publication.reused_control_catalogs {
                return Err(super::lww::error("captured-control-catalog-integrity"));
            }
            if previous.reused_assets != publication.reused_assets {
                return Err(super::lww::error("captured-asset-reference-integrity"));
            }
            if !previous.data_catalogs.is_empty() && previous.data_catalogs != publication.data_catalogs {
                return Err(super::lww::error("sealed-data-catalog-integrity"));
            }
            if !previous.asset_catalogs.is_empty() && previous.asset_catalogs != publication.asset_catalogs {
                return Err(super::lww::error("sealed-asset-catalog-integrity"));
            }
            if previous.bodies.len() != publication.bodies.len()
                || previous
                    .bodies
                    .iter()
                    .zip(&publication.bodies)
                    .any(|(old, new)| {
                        old.object_id != new.object_id
                            || old.content_hash != new.content_hash
                            || (!old.sha256.is_empty() && (old.sha256 != new.sha256 || old.byte_length != new.byte_length))
                            || (old.complete && (!new.complete || old.locator != new.locator))
                    })
            {
                return Err(super::lww::error("sealed-body-integrity"));
            }
            if previous.sealed
                && (!publication.sealed
                    || previous.object_id != publication.object_id
                    || previous.sha256 != publication.sha256
                    || previous.payload != publication.payload
                    || previous.payload_sha256 != publication.payload_sha256)
            {
                return Err(super::lww::error("sealed-segment-integrity"));
            }
            if !previous.sealed
                && !publication.sealed
                && (previous.payload != publication.payload
                    || previous.payload_sha256 != publication.payload_sha256)
            {
                return Err(super::lww::error("captured-payload-integrity"));
            }
        }
        let mut metadata = serde_json::to_string(publication)?;
        // Stable row lengths let SQLite retain unchanged overflow pages. JSON
        // whitespace preserves the released readers and their ordinary writes.
        let capacity = previous.as_ref().map(|(_, stored)| stored.len())
            .filter(|capacity| *capacity >= metadata.len())
            .unwrap_or_else(|| metadata.len() + (metadata.len() / 16).max(4096));
        metadata.extend(std::iter::repeat_n(' ', capacity - metadata.len()));
        let replace_sealed = previous.as_ref().is_some_and(|(sealed, _)| sealed.as_slice() != bytes);
        let tx = self.device_store_mut()?.transaction()?;
        let authority: String = tx.query_row(
            "SELECT binding_authority FROM lww_clock WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if authority != publication.authority.0.to_string() {
            return Err(super::lww::error("stale-binding-authority"));
        }
        let changed = if let Some((sealed, stored)) = &previous {
            if !replace_sealed {
                tx.execute("UPDATE external_lww_segments SET metadata=?1,complete=?2
                    WHERE target=?3 AND writer=?4 AND seq=?5 AND metadata=?6 AND sealed=?7",
                    params![metadata,publication.complete,publication.target,publication.writer,publication.seq.0.to_string(),stored,sealed])?
            } else {
                tx.execute("UPDATE external_lww_segments SET metadata=?1,complete=?2,sealed=?8
                    WHERE target=?3 AND writer=?4 AND seq=?5 AND metadata=?6 AND sealed=?7",
                    params![metadata,publication.complete,publication.target,publication.writer,publication.seq.0.to_string(),stored,sealed,bytes])?
            }
        } else {
            tx.execute("INSERT INTO external_lww_segments(target,writer,seq,authority,metadata,sealed,complete) VALUES(?1,?2,?3,?4,?5,?6,?7)
                ON CONFLICT(target,writer,seq) DO NOTHING",
                params![publication.target,publication.writer,publication.seq.0.to_string(),publication.authority.0.to_string(),metadata,bytes,publication.complete])?
        };
        if changed != 1 {
            return Err(super::lww::error("publication-state-changed"));
        }
        tx.commit()?;
        Ok(())
    }
    /// Completes a landed publication. The versions were recorded when it
    /// landed; the seen row takes its references here, and the row goes in
    /// the same transaction, so nothing it carried is lost before then.
    pub(crate) fn external_lww_finish_publication(
        &mut self,
        publication: &SealedPublication,
        bytes: &[u8],
        references: &SegmentReferences,
    ) -> StoreResult<()> {
        self.external_lww_persist(publication, bytes)?;
        self.lww_ack_outbox(
            &Header {
                binding_authority: publication.authority,
                request_id: format!(
                    "external-ack-{}-{}-{}-{}",
                    publication.target,
                    publication.authority.0,
                    publication.writer,
                    publication.seq.0
                ),
            },
            &publication.acknowledgements()?,
        )?;
        let next = publication
            .seq
            .0
            .checked_add(1)
            .ok_or_else(|| super::lww::error("sequence-overflow"))?;
        let tx = self.device_store_mut()?.transaction()?;
        let authority: String = tx.query_row(
            "SELECT binding_authority FROM lww_clock WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if authority != publication.authority.0.to_string() {
            return Err(super::lww::error("stale-binding-authority"));
        }
        insert_seen(&tx, &publication.target, &publication.writer, publication.seq.0, &publication.sha256, Some(references))?;
        tx.execute(
            "INSERT INTO external_lww_sequences(target,writer,next_seq) VALUES(?1,?2,?3)
            ON CONFLICT(target,writer) DO UPDATE SET next_seq=excluded.next_seq",
            params![publication.target, publication.writer, next.to_string()],
        )?;
        tx.execute("INSERT INTO lww_progress(authority,kind,writer_id,cursor) VALUES(?1,'external',?2,?3) ON CONFLICT(kind,writer_id) DO UPDATE SET cursor=excluded.cursor,authority=excluded.authority",params![authority,publication.writer,publication.seq.0.to_string()])?;
        tx.execute(
            "DELETE FROM external_lww_segments WHERE target=?1 AND writer=?2 AND seq=?3",
            params![publication.target, publication.writer, publication.seq.0.to_string()],
        )?;
        tx.commit()?;
        if let Some(job) = &publication.asset_job {
            remove_publication_files(&self.repository_root, &job.job_id)?;
        }
        Ok(())
    }
    pub(crate) fn external_lww_verify_seen(
        &self,
        target: &str,
        writer: &str,
        seq: u64,
        hash: &str,
    ) -> StoreResult<bool> {
        verify_seen(self.device_store()?.connection(), target, writer, seq, hash)
    }
    pub(crate) fn external_lww_record_seen(
        &self,
        target: &str,
        writer: &str,
        seq: u64,
        hash: &str,
        references: Option<&SegmentReferences>,
    ) -> StoreResult<()> {
        insert_seen(self.device_store()?.connection(), target, writer, seq, hash, references)
    }
    /// The references this device recorded for the segment with this name.
    pub(crate) fn external_lww_segment_references(
        &self,
        target: &str,
        writer: &str,
        seq: u64,
        hash: &str,
    ) -> StoreResult<Option<SegmentReferences>> {
        let refs: Option<Option<String>> = self.device_store()?.connection().query_row(
            "SELECT refs FROM external_lww_seen WHERE target=?1 AND writer=?2 AND seq=?3 AND sha256=?4",
            params![target, writer, seq.to_string(), hash],
            |r| r.get(0),
        ).optional()?;
        Ok(match refs.flatten() {
            Some(refs) => Some(serde_json::from_str(&refs)?),
            None => None,
        })
    }
    /// Keeps the coverage a binding stage applied and the segments it covers,
    /// so the first receive after activating that stage starts after them.
    pub(crate) fn external_lww_record_stage(
        &mut self,
        target: &str,
        header: &Header,
        coverage: &std::collections::BTreeMap<String, DecimalU64>,
        covered: &[(String, u64, String)],
    ) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        for (writer, seq, hash) in covered {
            insert_seen(&tx, target, writer, *seq, hash, None)?;
        }
        tx.execute(
            "DELETE FROM external_lww_objects WHERE target=?1 AND substr(hash,1,length(?2))=?2",
            params![target, STAGE_PREFIX],
        )?;
        let stage = StageCoverage { authority: header.binding_authority, coverage: coverage.clone() };
        tx.execute(
            "INSERT INTO external_lww_objects(target,hash,body) VALUES(?1,?2,?3)",
            params![target, format!("{STAGE_PREFIX}{}", header.request_id), serde_json::to_vec(&stage)?],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Starts receive progress at the coverage of the stage the current
    /// binding activated. A stage waits while its switch or replacement is
    /// still to come; one whose activation this store cannot prove is dropped
    /// without seeding, and the receive starts from the beginning.
    pub(crate) fn external_lww_seed_activation(&mut self, target: &str, authority: DecimalU64) -> StoreResult<()> {
        let stages: Vec<(String, Vec<u8>)> = {
            let mut statement = self.device_store()?.connection().prepare(
                "SELECT hash,body FROM external_lww_objects WHERE target=?1 AND substr(hash,1,length(?2))=?2",
            )?;
            let rows = statement
                .query_map(params![target, STAGE_PREFIX], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        for (key, body) in stages {
            let stage: StageCoverage = serde_json::from_slice(&body)?;
            if stage.authority == authority {
                continue;
            }
            let receive_id = key.strip_prefix(STAGE_PREFIX).ok_or_else(|| super::lww::error("stage-coverage-integrity"))?;
            // The activation replaces the library in one commit under the stage's
            // receive id, with the authority the switch before it issued.
            let switched = stage.authority.0.checked_add(1) == Some(authority.0);
            let replaced: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM lww_requests WHERE request_id=?1)",
                [receive_id],
                |r| r.get(0),
            )?;
            if switched && !replaced {
                continue;
            }
            let activated = switched && replaced;
            let tx = self.device_store_mut()?.transaction()?;
            if activated {
                let current: std::collections::BTreeMap<String, u64> = {
                    let mut statement = tx.prepare("SELECT writer_id,cursor FROM lww_progress WHERE kind='external' AND authority=?1")?;
                    let rows = statement
                        .query_map([authority.0.to_string()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    rows.into_iter()
                        .map(|(writer, cursor)| Ok((writer, cursor.parse::<u64>().map_err(super::lww::error)?)))
                        .collect::<StoreResult<_>>()?
                };
                for (writer, prefix) in &stage.coverage {
                    if current.get(writer).is_some_and(|cursor| *cursor >= prefix.0) {
                        continue;
                    }
                    tx.execute(
                        "INSERT INTO lww_progress(authority,kind,writer_id,cursor) VALUES(?1,'external',?2,?3) ON CONFLICT(kind,writer_id) DO UPDATE SET cursor=excluded.cursor,authority=excluded.authority",
                        params![authority.0.to_string(), writer, prefix.0.to_string()],
                    )?;
                }
            }
            tx.execute("DELETE FROM external_lww_objects WHERE target=?1 AND hash=?2", params![target, key])?;
            tx.commit()?;
        }
        Ok(())
    }
    /// Counts the listings a seen segment was missing from and forgets the
    /// versions and references of a covered segment once it was missing from
    /// two in a row.
    pub(crate) fn external_lww_observe_listing(
        &mut self,
        target: &str,
        listed: &std::collections::BTreeSet<(String, u64)>,
        coverage: &std::collections::BTreeMap<String, DecimalU64>,
    ) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        let rows: Vec<(String, String, i64)> = {
            let mut statement = tx.prepare("SELECT writer,seq,absent FROM external_lww_seen WHERE target=?1")?;
            let rows = statement
                .query_map([target], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        for (writer, seq_text, absent) in rows {
            let seq = seq_text.parse::<u64>().map_err(super::lww::error)?;
            if listed.contains(&(writer.clone(), seq)) {
                if absent != 0 {
                    tx.execute("UPDATE external_lww_seen SET absent=0 WHERE target=?1 AND writer=?2 AND seq=?3", params![target, writer, seq_text])?;
                }
            } else if absent >= 1 && coverage.get(&writer).is_some_and(|prefix| seq <= prefix.0) {
                forget_segment(&tx, target, &writer, &seq_text)?;
            } else {
                tx.execute("UPDATE external_lww_seen SET absent=absent+1 WHERE target=?1 AND writer=?2 AND seq=?3", params![target, writer, seq_text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    /// Forgets the versions and references of a covered segment this device's
    /// cleanup removed.
    pub(crate) fn external_lww_forget_removed_segment(&mut self, target: &str, writer: &str, seq: u64, hash: &str) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        let seq = seq.to_string();
        let seen: Option<String> = tx.query_row(
            "SELECT sha256 FROM external_lww_seen WHERE target=?1 AND writer=?2 AND seq=?3",
            params![target, writer, seq],
            |r| r.get(0),
        ).optional()?;
        if seen.is_none_or(|seen| seen == hash) {
            forget_segment(&tx, target, writer, &seq)?;
        }
        tx.commit()?;
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StageCoverage {
    authority: DecimalU64,
    coverage: std::collections::BTreeMap<String, DecimalU64>,
}
fn verify_seen(db: &rusqlite::Connection, target: &str, writer: &str, seq: u64, hash: &str) -> StoreResult<bool> {
    let previous: Option<String> = db
        .query_row(
            "SELECT sha256 FROM external_lww_seen WHERE target=?1 AND writer=?2 AND seq=?3",
            params![target, writer, seq.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    if previous.as_deref().is_some_and(|value| value != hash) {
        return Err(super::lww::error("writer-sequence-integrity"));
    }
    Ok(previous.is_some())
}
fn insert_seen(
    db: &rusqlite::Connection,
    target: &str,
    writer: &str,
    seq: u64,
    hash: &str,
    references: Option<&SegmentReferences>,
) -> StoreResult<()> {
    verify_seen(db, target, writer, seq, hash)?;
    let references = references.map(serde_json::to_string).transpose()?;
    db.execute(
        "INSERT INTO external_lww_seen(target,writer,seq,sha256,refs,absent) VALUES(?1,?2,?3,?4,?5,0)
        ON CONFLICT(target,writer,seq) DO UPDATE SET refs=COALESCE(excluded.refs,refs),absent=0",
        params![target, writer, seq.to_string(), hash, references],
    )?;
    Ok(())
}
fn forget_segment(db: &rusqlite::Connection, target: &str, writer: &str, seq: &str) -> StoreResult<()> {
    db.execute("DELETE FROM external_lww_versions WHERE target=?1 AND writer=?2 AND seq=?3", params![target, writer, seq])?;
    db.execute("DELETE FROM external_lww_seen WHERE target=?1 AND writer=?2 AND seq=?3", params![target, writer, seq])?;
    Ok(())
}

#[cfg(test)]
mod asset_root_tests {
    use super::*;
    use crate::external_storage::{contract::{Cancellation, ErrorKind, Provider, RemoteLocator}, lww_tests::{CycleFixture, HeldAssetTransfer, small_asset}};
    use crate::server_sync::{lww_tests::LocalServerFixture, residency::{AssetPolicy, Residency}};
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

    fn age_asset_proof(store:&PersistentStore,target:&str,hash:&str,aged:u64) {
        let (key,body)=match store.external_lww_asset_witness(target,hash).unwrap().unwrap() {
            AssetWitness::Catalog{catalog_key,..}=>{
                let mut proof=store.external_lww_catalog_proof(target,&catalog_key,risunest_external_storage_format::snapshot::CatalogKind::Assets).unwrap().unwrap();
                proof.rooted_at_ms=Some(aged);
                (catalog_key,serde_json::to_vec(&proof).unwrap())
            }
            AssetWitness::Standalone{content_hash,body,..}=>{
                let witness=AssetWitness::Standalone{content_hash,body,rooted_at_ms:Some(aged)};
                (format!("asset/{hash}"),serde_json::to_vec(&witness).unwrap())
            }
        };
        store.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",params![body,target,key]).unwrap();
    }

    #[test]
    fn retired_packed_and_standalone_assets_are_recarried_for_fresh_bootstrap() {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            for size in [96*1024,5*1024*1024] {
                let mut f=CycleFixture::new();let cancel=Cancellation::default();let bytes=vec![63;size];
                let transfer=HeldAssetTransfer::new(f.provider.clone());transfer.held.store(false,Ordering::SeqCst);
                f.sender.provider=transfer.clone();
                let hash=small_asset(&mut f.a,"synthetic-retired-reference",&bytes);
                let alias=f.a.list_asset_aliases(None).unwrap().value.remove(0);
                assert_eq!(f.publish_a().await.segments.0,1);
                let target=f.sender.target_scope();let now=f.now_ms();
                let original=f.a.external_lww_reusable_asset(&target,&hash,now).unwrap().unwrap();
                assert_eq!(original.byte_length,size as u64);
                let uploads=transfer.attempts.lock().unwrap().len();
                f.a.delete_asset_alias("asset",&alias.key,f.a.revision().unwrap()).unwrap();f.publish_a().await;
                f.a.commit_asset_alias(&alias,f.a.revision().unwrap()).unwrap();f.publish_a().await;
                assert_eq!(transfer.attempts.lock().unwrap().len(),uploads,"fresh root carriage never reuploads the Asset body");
                let receipt=f.sender.listing(&cancel).await.unwrap().into_iter().find(|receipt|crate::external_storage::contract::parse_segment_object_id(&receipt.locator.object).unwrap().1==3).unwrap();
                let (writer,seq,_)=crate::external_storage::contract::parse_segment_object_id(&receipt.locator.object).unwrap();
                let carried=crate::external_storage::lww_segment::open(&f.provider.contents(&receipt.locator.object).unwrap(),&f.sender.library,writer,seq,&f.sender.root_key).unwrap();
                match &original.reference {
                    AssetReference::Catalog(catalog)=>assert_eq!(carried.asset_catalogs,vec![catalog.clone()]),
                    AssetReference::Standalone(body)=>assert_eq!(carried.large_bodies.get(&hash),Some(body)),
                }
                f.a.delete_asset_alias("asset",&alias.key,f.a.revision().unwrap()).unwrap();f.publish_a().await;
                let work=tempfile::tempdir().unwrap();
                let writer=f.a.lww_clock_state().unwrap().writer_id;
                let completed=f.sender.compact_published(&mut f.a,work.path(),"00000000-0000-4000-8000-000000000085",
                    &writer,&f.sender.capabilities,&cancel,None).await.unwrap();
                let (_,checkpoint)=f.sender.checkpoint(&completed.reference.receipt,&cancel).await.unwrap();
                assert!(checkpoint.asset_catalogs.is_empty());assert!(checkpoint.standalone_bodies.is_empty());
                assert_eq!(checkpoint.covered_prefixes.values().next().unwrap().0,4);
                for receipt in f.sender.listing(&cancel).await.unwrap() {
                    f.provider.delete_object(&f.sender.repository,&receipt.locator,&cancel).await.unwrap();
                }
                let mut dependencies=transfer.attempts.lock().unwrap().iter().take(uploads).map(|intent|RemoteLocator {
                    connection_identity:f.sender.repository.connection_identity.clone(),collection:None,object:intent.object_id.clone(),
                }).collect::<Vec<_>>();
                if let AssetReference::Catalog(catalog)=&original.reference {dependencies.push(crate::external_storage::packaging::RemoteObject::from_stored(catalog,&f.sender.repository).unwrap().receipt.locator);}
                for locator in dependencies {
                    f.provider.delete_object(&f.sender.repository,&locator,&cancel).await.unwrap();
                    assert!(f.provider.contents(&locator.object).is_none());
                }
                let aged=f.now_ms()-crate::external_storage::leases::CACHE_REUSE_LIMIT_MS-1;
                age_asset_proof(&f.a,&target,&hash,aged);
                assert!(f.a.external_lww_reusable_asset(&target,&hash,f.now_ms()).unwrap().is_none());
                assert!(f.a.external_lww_object_is_local(&hash).unwrap());
                let before_readd=transfer.attempts.lock().unwrap().len();
                f.a.commit_asset_alias(&alias,f.a.revision().unwrap()).unwrap();
                assert_eq!(f.publish_a().await.segments.0,1);
                let renewed=f.a.external_lww_reusable_asset(&target,&hash,f.now_ms()).unwrap().unwrap();
                assert_ne!(renewed.reference,original.reference,"retired sources are replaced from the exact local CAS hash");
                assert!(transfer.attempts.lock().unwrap().len()>before_readd);
                assert!(f.b.lww_receive_progress(0.into()).unwrap().is_empty());
                assert!(f.receive_b().await>0);
                assert_eq!(f.b.list_asset_aliases(None).unwrap().value[0].object_hash.as_deref(),Some(hash.as_str()));
                assert!(!f.b.external_lww_object_is_local(&hash).unwrap());
                assert_eq!(crate::external_storage::lww_residency::stat(f.directory_b.path(),&hash).unwrap(),Some(size as u64));
                let mut body=crate::external_storage::lww_tests::hydrate_receiver(&f,&hash).await;
                let mut actual=Vec::new();std::io::Read::read_to_end(&mut body,&mut actual).unwrap();assert_eq!(actual,bytes);
            }
        });
    }

    #[test]
    fn aged_reused_asset_roots_are_checked_before_frozen_retry() {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            for (size,missing,dispatched) in [(96*1024,false,false),(96*1024,true,false),(5*1024*1024,false,false),(5*1024*1024,true,false),(96*1024,true,true)] {
                let mut f=CycleFixture::new();let cancel=Cancellation::default();
                let hash=small_asset(&mut f.a,"synthetic-reused-retry",&vec![64;size]);
                let alias=f.a.list_asset_aliases(None).unwrap().value.remove(0);f.publish_a().await;
                f.a.delete_asset_alias("asset",&alias.key,f.a.revision().unwrap()).unwrap();f.publish_a().await;
                f.a.commit_asset_alias(&alias,f.a.revision().unwrap()).unwrap();
                let transfer=HeldAssetTransfer::new(f.provider.clone());transfer.held.store(false,Ordering::SeqCst);
                transfer.fail_segment_begin.store(true,Ordering::SeqCst);f.sender.provider=transfer.clone();
                assert_eq!(f.sender.publish(&mut f.a,0.into(),&[],&cancel).await.err().unwrap().kind,ErrorKind::Transient);
                let target=f.sender.target_scope();let writer=f.a.lww_clock_state().unwrap().writer_id;
                let (mut pending,sealed)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
                assert!(pending.sealed&&!pending.dispatched&&pending.assets.is_empty()&&pending.bodies.is_empty());
                assert_eq!(pending.reused_assets.len(),1);assert_eq!(pending.reused_assets[0].content_hash,hash);
                assert_eq!(f.a.external_lww_active_asset_roots().unwrap().object_hashes,std::collections::BTreeSet::from([hash.clone()]));
                let aged=f.now_ms()-crate::external_storage::leases::CACHE_REUSE_LIMIT_MS-1;
                pending.reused_assets[0].rooted_at_ms=aged;
                assert!(f.a.external_lww_persist(&pending,&sealed).is_err());
                pending.dispatched=dispatched;
                // Model a restart after the captured source proof has aged, keeping all immutable bytes and locators.
                f.a.device_store().unwrap().connection().execute("UPDATE external_lww_segments SET metadata=?1 WHERE target=?2 AND writer=?3 AND seq=?4",
                    params![serde_json::to_string(&pending).unwrap(),target,writer,pending.seq.0.to_string()]).unwrap();
                let (locator,object_id)=match &pending.reused_assets[0].reference {
                    AssetReference::Catalog(catalog)=>(crate::external_storage::packaging::RemoteObject::from_stored(catalog,&f.sender.repository).unwrap().receipt.locator,None),
                    AssetReference::Standalone(body)=>(body.locator.clone().unwrap(),Some(body.object_id.clone())),
                };
                let before_reads=f.provider.read_attempts(&locator.object);
                let before_reconcile=object_id.as_ref().map(|id|f.provider.reconcile_attempts(id));
                if missing {f.provider.forget(&locator.object);}
                f.a=PersistentStore::open(f.directory_a.path()).unwrap();
                let outbox=f.a.lww_read_outbox(0.into(),4096).unwrap().entries;
                let result=f.sender.publish(&mut f.a,0.into(),&[],&cancel).await;
                if dispatched {assert!(f.provider.reconcile_attempts(&pending.object_id)>0,"original uncertain Segment must be reconciled before dependency checks");}
                if let Some(id)=object_id {assert!(f.provider.reconcile_attempts(&id)>before_reconcile.unwrap());}
                else {assert!(f.provider.read_attempts(&locator.object)>before_reads);}
                assert!(transfer.attempts.lock().unwrap().is_empty(),"reused retry cannot repair or upload a replacement body");
                if missing {
                    assert_eq!(result.err().unwrap().kind,ErrorKind::NotFound);
                    let (after,after_sealed)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
                    assert_eq!(after_sealed,sealed);assert_eq!(after.reused_assets,pending.reused_assets);assert_eq!(after.entries,pending.entries);
                    assert_eq!(after.dispatched,dispatched);
                    assert_eq!(f.a.external_lww_next_sequence(&target,&writer).unwrap(),pending.seq.0);
                    assert_eq!(f.a.lww_read_outbox(0.into(),4096).unwrap().entries,outbox);
                    assert_eq!(f.provider.upload_attempts(&pending.object_id),0);
                } else {
                    assert_eq!(result.unwrap().segments.0,1);assert_eq!(f.provider.contents(&pending.object_id).unwrap(),sealed);
                    assert!(f.a.external_lww_pending(&target,&writer).unwrap().is_none());
                    assert!(f.a.external_lww_active_asset_roots().unwrap().object_hashes.is_empty());
                }
            }
        });
    }

    #[test]
    fn aged_saved_new_dependencies_are_verified_before_exact_publication_retry() {
        for (missing,dispatched) in [(None,false),(Some("data"),false),(Some("asset"),false),(Some("body"),false),(Some("body"),true)] {
            let mut f=CycleFixture::new();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["exists","character","char"]).unwrap(),value:serde_json::json!({"type":"character"})},
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["exists","conversation","char","conv"]).unwrap(),value:serde_json::json!(true)},
            ]),..Default::default()}).unwrap();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
                conversations:Some(vec![crate::persistent_store::ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                    start:0,delete_count:0,messages:(0..40).map(|index|serde_json::json!({"chatId":format!("synthetic-new-dependency-{index}"),"data":"x".repeat(128*1024)})).collect(),
                    conversation:None,configured_index:None}]),..Default::default()}).unwrap();
            small_asset(&mut f.a,"synthetic-new-packed",&vec![31;96*1024]);
            small_asset(&mut f.a,"synthetic-new-standalone",&vec![32;5*1024*1024]);
            let transfer=HeldAssetTransfer::new(f.provider.clone());
            transfer.held.store(false,Ordering::SeqCst);
            transfer.fail_segment_begin.store(true,Ordering::SeqCst);
            f.sender.provider=transfer.clone();
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            assert_eq!(runtime.block_on(f.sender.publish(&mut f.a,0.into(),&[],&Cancellation::default())).err().unwrap().kind,ErrorKind::Transient);
            let target=f.sender.target_scope();let writer=f.a.lww_clock_state().unwrap().writer_id;
            let (mut pending,bytes)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
            assert!(pending.sealed && !pending.dispatched && pending.captured_at_ms.is_some());
            assert_eq!(pending.data_catalogs.len(),1);assert_eq!(pending.asset_catalogs.len(),1);
            assert_eq!(pending.bodies.len(),1);assert!(pending.bodies[0].complete);
            let mut shape=serde_json::to_value(&pending).unwrap();shape.as_object_mut().unwrap().remove("capturedAtMs");
            assert!(serde_json::from_value::<SealedPublication>(shape).is_err());
            pending.captured_at_ms=missing.map(|_|f.now_ms()-crate::external_storage::leases::CACHE_REUSE_LIMIT_MS-1);
            assert!(f.a.external_lww_persist(&pending,&bytes).is_err(),"capture age cannot be changed during retry");
            pending.dispatched=dispatched;
            // Model an aged or unproven capture without changing its dependency identities or sealed bytes.
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_segments SET metadata=?1 WHERE target=?2 AND writer=?3 AND seq=?4",
                params![serde_json::to_string(&pending).unwrap(),target,writer,pending.seq.0.to_string()]).unwrap();
            let data=&pending.data_catalogs[0].locator.object;let asset=&pending.asset_catalogs[0].locator.object;
            let body=&pending.bodies[0].object_id;
            let reads=(f.provider.read_attempts(data),f.provider.read_attempts(asset));
            let reconciles=f.provider.reconcile_attempts(body);
            let packs=transfer.attempts.lock().unwrap().len();
            if let Some(missing)=missing {
                f.provider.forget(match missing {"data"=>data.as_str(),"asset"=>asset.as_str(),"body"=>pending.bodies[0].locator.as_ref().unwrap().object.as_str(),_=>unreachable!()});
            }
            f.a=PersistentStore::open(f.directory_a.path()).unwrap();
            if missing.is_some() {
                f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
                    conversations:Some(vec![crate::persistent_store::ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                        start:40,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-newer-dependency","data":"newer"})],
                        conversation:None,configured_index:None}]),..Default::default()}).unwrap();
            }
            let outbox=f.a.lww_read_outbox(0.into(),4096).unwrap().entries;
            let result=runtime.block_on(f.sender.publish(&mut f.a,0.into(),&[],&Cancellation::default()));
            assert!(f.provider.read_attempts(data)>reads.0);
            if missing!=Some("data") {assert!(f.provider.read_attempts(asset)>reads.1);}
            if missing.is_none() || missing==Some("body") {assert!(f.provider.reconcile_attempts(body)>reconciles);}
            assert_eq!(transfer.attempts.lock().unwrap().len(),packs,"saved dependency validation must not repair or upload replacements");
            if dispatched {assert!(f.provider.reconcile_attempts(&pending.object_id)>0,"uncertain original segment must be reconciled first");}
            if missing.is_some() {
                assert_eq!(result.err().unwrap().kind,ErrorKind::NotFound);
                let (after,after_bytes)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
                assert_eq!(after_bytes,bytes);assert_eq!(after.entries,pending.entries);
                assert_eq!(after.captured_at_ms,pending.captured_at_ms);assert_eq!(after.dispatched,dispatched);
                assert!(after.data_catalogs==pending.data_catalogs && after.asset_catalogs==pending.asset_catalogs);
                assert!(serde_json::to_value(&after.bodies).unwrap()==serde_json::to_value(&pending.bodies).unwrap());
                assert_eq!(f.a.lww_read_outbox(0.into(),4096).unwrap().entries,outbox);
                assert_eq!(f.a.external_lww_next_sequence(&target,&writer).unwrap(),pending.seq.0);
                assert_eq!(f.a.read_conversation("char","conv",None).unwrap().unwrap().value["message"].as_array().unwrap().len(),41);
                assert_eq!(f.provider.upload_attempts(&pending.object_id),0);
            } else {
                assert_eq!(result.unwrap().segments.0,1);
                assert_eq!(f.provider.contents(&pending.object_id).unwrap(),bytes);
                assert!(f.a.external_lww_pending(&target,&writer).unwrap().is_none());
                assert_eq!(f.a.external_lww_next_sequence(&target,&writer).unwrap(),pending.seq.0+1);
            }
        }
    }

    #[test]
    fn expired_control_catalogs_are_not_renewed_by_cache_admission_and_frozen_retry_checks_exact_root() {
        for missing in [false,true] {
            let mut f=CycleFixture::new();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["exists","character","char"]).unwrap(),value:serde_json::json!({"type":"character"})},
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["exists","conversation","char","conv"]).unwrap(),value:serde_json::json!(true)},
            ]),..Default::default()}).unwrap();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
                conversations:Some(vec![crate::persistent_store::ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                    start:0,delete_count:0,messages:(0..40).map(|index|serde_json::json!({"chatId":format!("synthetic-aged-{index}"),"data":"x".repeat(128*1024)})).collect(),
                    conversation:None,configured_index:None}]),..Default::default()}).unwrap();
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(f.publish_a());
            let receipt=runtime.block_on(f.sender.listing(&Cancellation::default())).unwrap().remove(0);
            let (writer,seq,_)=crate::external_storage::contract::parse_segment_object_id(&receipt.locator.object).unwrap();
            let payload=crate::external_storage::lww_segment::open(&f.provider.contents(&receipt.locator.object).unwrap(),&f.sender.library,writer,seq,&f.sender.root_key).unwrap();
            let catalog=payload.data_catalogs[0].clone();
            let target=f.sender.target_scope();
            let key=PersistentStore::external_lww_data_catalog_key(&catalog).unwrap();
            let witness=f.a.external_lww_data_catalog_witness(&target,&key).unwrap().unwrap();
            let proof_key=PersistentStore::external_lww_control_catalog_key(&catalog).unwrap();
            let mut proof=f.a.external_lww_control_catalog_proof(&target,&proof_key).unwrap().unwrap();
            let now=f.now_ms();
            let aged=now-crate::external_storage::leases::CACHE_REUSE_LIMIT_MS-1;
            proof.rooted_at_ms=Some(aged);
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
                params![serde_json::to_vec(&proof).unwrap(),target,proof_key]).unwrap();
            f.a.external_lww_witness_data_catalog(&target,&catalog,&witness.hashes).unwrap();
            assert_eq!(f.a.external_lww_control_catalog_proof(&target,&proof_key).unwrap().unwrap().rooted_at_ms,Some(aged));
            assert!(f.a.external_lww_reusable_control_catalog(&target,&witness.hashes[0],now).unwrap().is_none());
            assert!(f.a.external_lww_verified_data_catalog("another-target",&catalog).unwrap().is_none());
            f.a.external_lww_authorize_data_catalog(&target,&catalog,now).unwrap();
            let hash=&witness.hashes[0];
            let pointer:Vec<u8>=f.a.device_store().unwrap().connection().query_row(
                "SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2",params![target,hash],|row|row.get(0)).unwrap();
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
                params![b"invalid inventory".as_slice(),target,key]).unwrap();
            assert!(f.a.external_lww_verified_data_catalog(&target,&catalog).is_err());
            assert!(f.a.external_lww_reusable_control_catalog(&target,hash,now).unwrap().is_some());
            f.a.external_lww_authorize_data_catalog(&target,&catalog,now).unwrap();
            let after:Vec<u8>=f.a.device_store().unwrap().connection().query_row(
                "SELECT body FROM external_lww_objects WHERE target=?1 AND hash=?2",params![target,hash],|row|row.get(0)).unwrap();
            assert_eq!(after,pointer,"routine proof renewal must not rewrite control pointers");
            assert!(f.a.external_lww_verified_data_catalog(&target,&catalog).is_err(),"routine lookup and renewal must not deserialize or replace the inventory");
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
                params![serde_json::to_vec(&witness).unwrap(),target,key]).unwrap();
            let changed=ControlCatalogReference{content_hash:"a".repeat(64),catalog_key:proof_key.clone()};
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
                params![serde_json::to_vec(&changed).unwrap(),target,hash]).unwrap();
            assert!(f.a.external_lww_reusable_control_catalog(&target,hash,now).is_err());
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_objects SET body=?1 WHERE target=?2 AND hash=?3",
                params![pointer,target,hash]).unwrap();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
                conversations:Some(vec![crate::persistent_store::ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                    start:40,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-aged-append","data":"append"})],
                    conversation:None,configured_index:None}]),..Default::default()}).unwrap();
            f.provider.fail_upload_number(f.provider.upload_count()+1,ErrorKind::Transient);
            assert!(runtime.block_on(f.sender.publish(&mut f.a,0.into(),&[],&Cancellation::default())).is_err());
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            let (mut pending,bytes)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
            assert!(!pending.dispatched && !pending.reused_control_catalogs.is_empty());
            pending.reused_control_catalogs[0].rooted_at_ms=aged;
            assert!(f.a.external_lww_persist(&pending,&bytes).is_err(),"captured proof cannot be changed on retry");
            // Age the persisted fixture as if its undispatched capture survived a long restart.
            f.a.device_store().unwrap().connection().execute("UPDATE external_lww_segments SET metadata=?1 WHERE target=?2 AND writer=?3 AND seq=?4",
                params![serde_json::to_string(&pending).unwrap(),target,writer,pending.seq.0.to_string()]).unwrap();
            let reads=f.provider.read_attempts(&catalog.locator.object);
            if missing {f.provider.forget(&catalog.locator.object);}
            f.a=PersistentStore::open(f.directory_a.path()).unwrap();
            let result=runtime.block_on(f.sender.publish(&mut f.a,0.into(),&[],&Cancellation::default()));
            assert!(f.provider.read_attempts(&catalog.locator.object)>reads,"aged exact source must be checked under Work");
            if missing {
                assert_eq!(result.err().unwrap().kind,ErrorKind::NotFound);
                let (after,after_bytes)=f.a.external_lww_pending(&target,&writer).unwrap().unwrap();
                assert_eq!(after_bytes,bytes);
                assert_eq!(after.entries,pending.entries);
                assert_eq!(after.reused_control_catalogs,pending.reused_control_catalogs);
                assert!(!after.dispatched);
                assert_eq!(f.a.external_lww_next_sequence(&target,&writer).unwrap(),pending.seq.0);
                assert!(!f.a.lww_read_outbox(0.into(),4096).unwrap().entries.is_empty());
            } else {
                assert_eq!(result.unwrap().segments.0,1);
                assert!(f.a.external_lww_pending(&target,&writer).unwrap().is_none());
                assert_eq!(f.a.external_lww_next_sequence(&target,&writer).unwrap(),pending.seq.0+1);
            }
        }
    }

    #[test]
    fn pending_remote_asset_roots_survive_alias_overwrite_and_release_only_after_ack_or_abandon() {
        for acknowledged in [false, true] {
            let mut f = CycleFixture::new();
            let server = LocalServerFixture::new();
            let core = server.client(&f.a);
            let hash = small_asset(&mut f.a, "synthetic-pending-root", &vec![71; 96 * 1024]);
            crate::server_sync::lww_tests::drain_publications(&core, &mut f.a, &[]).unwrap();
            f.a.asset_residency_set_policy(AssetPolicy::Remote, || Ok(())).unwrap();
            f.a.asset_residency_evict(|| Ok(())).unwrap();
            assert!(!f.a.external_lww_object_is_local(&hash).unwrap());
            let alias = crate::persistent_store::AssetAlias { key: "synthetic-pending-root".into(),
                object_hash: Some(hash.clone()), kind: "asset".into(), size: 96 * 1024,
                mime: "application/octet-stream".into(), name: "Synthetic captured alias".into(), ext: "bin".into(),
                inlay_type: None, width: None, height: None, metadata: serde_json::json!({}) };
            f.a.commit_asset_alias(&alias, f.a.revision().unwrap()).unwrap();
            let held = HeldAssetTransfer::new(f.provider.clone());
            held.held.store(false, Ordering::SeqCst);
            let root = f.directory_a.path().to_owned();
            let cancel = Cancellation::default();
            let callback_cancel = cancel.clone();
            let overwritten = Arc::new(AtomicBool::new(false));
            let callback_overwritten = overwritten.clone();
            *held.before_lease.lock().unwrap() = Some(Box::new(move || {
                let mut store = PersistentStore::open(&root).unwrap();
                small_asset(&mut store, "synthetic-pending-root", b"synthetic later alias body");
                callback_overwritten.store(true, Ordering::SeqCst);
                callback_cancel.cancel();
            }));
            f.sender.provider = held.clone();
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            assert_eq!(runtime.block_on(f.sender.publish(&mut f.a, 0.into(), &[], &cancel)).err().expect("cancelled publication must retain its capture").kind, ErrorKind::Cancelled);
            assert!(overwritten.load(Ordering::SeqCst));
            f.a = PersistentStore::open(f.directory_a.path()).unwrap();
            let writer = f.a.lww_clock_state().unwrap().writer_id;
            let (pending, _) = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap();
            assert_eq!(f.a.external_lww_active_asset_roots().unwrap().object_hashes, [hash.clone()].into());
            crate::persistent_store::ASSET_GC_ROOT_COLLECTIONS.with(|count| count.set((0, 0)));
            assert!(f.a.residency_inventory_classification_test(false).unwrap().0.contains(&hash));
            crate::persistent_store::ASSET_GC_ROOT_COLLECTIONS.with(|count| assert_eq!(count.get(), (1, 0)));
            f.a.asset_residency_release_unused(|| Ok(())).unwrap();
            assert!(Residency::open(f.directory_a.path()).unwrap().object(&hash, None).unwrap().is_some());
            if acknowledged {
                held.fail_pack_after.store(2, Ordering::SeqCst);
                assert_eq!(runtime.block_on(f.sender.publish(&mut f.a, 0.into(), &[], &Cancellation::default())).err().expect("later publication must reach the held pack").kind, ErrorKind::RateLimited);
                assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), 2);
                assert!(f.a.external_lww_active_asset_roots().unwrap().object_hashes.len() == 1);
                assert!(!f.a.external_lww_active_asset_roots().unwrap().object_hashes.contains(&hash));
                assert!(f.a.lww_read_outbox(0.into(), 100).unwrap().entries.len() == 1);
            } else {
                f.a.external_lww_abandon_unsent(&f.sender.target_scope(), &writer).unwrap();
                let job = pending.asset_job.unwrap();
                crate::asset_repository::job_pins::DurableCasJob::open(f.directory_a.path(), &job.job_id).unwrap()
                    .release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted).unwrap();
                assert!(f.a.external_lww_active_asset_roots().unwrap().object_hashes.is_empty());
            }
            assert!(!f.a.residency_inventory_classification_test(false).unwrap().0.contains(&hash));
            f.a.asset_residency_release_unused(|| Ok(())).unwrap();
            assert!(Residency::open(f.directory_a.path()).unwrap().object(&hash, None).unwrap().is_none());
        }
    }
}
