//! Reconstructible transfer receipts. Publication and activation authority stays
//! in PDS; reopening this journal requires its current PDS job identity.
use super::{contract::*, transfer::SpoolSource};
use crate::persistent_store::sync_selection::CaptureIdentity;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn storage(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JobIdentity {
    pub job_id: String,
    pub connection_id: String,
    pub repository_id: String,
    pub capture_id: String,
    pub capture: CaptureIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SealedResume {
    reference: String,
    confirmed_offset: u64,
    expires_at_ms: Option<u64>,
}
impl SealedResume {
    fn from_state(state: &ResumeState) -> Self {
        Self {
            reference: state.sealed_state.0.clone(),
            confirmed_offset: state.confirmed_offset,
            expires_at_ms: state.expires_at_ms,
        }
    }
    fn into_state(self) -> ResumeState {
        ResumeState {
            sealed_state: SecretRef(self.reference),
            confirmed_offset: self.confirmed_offset,
            expires_at_ms: self.expires_at_ms,
        }
    }
}

pub(crate) struct TransferRecord {
    pub intent: ObjectIntent,
    pub attempted: bool,
    pub resume: Option<ResumeState>,
    pub receipt: Option<ObjectReceipt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpoolCleanup {
    Retained,
    Removed { objects: u64, bytes: u64 },
}

pub(crate) struct TransferJournal {
    db: Connection,
    directory: PathBuf,
    identity: JobIdentity,
}
impl TransferJournal {
    /// Reopens an existing journal after its worker has exited, then removes
    /// terminal spools only when the authoritative PDS owner can be released.
    /// Jobs without a transfer journal have no registered spool to remove.
    pub(crate) fn cleanup_terminal_spools_at(
        directory: &Path,
        job_id: &str,
        store: &mut crate::persistent_store::PersistentStore,
        format_repository_id: &str,
    ) -> Result<SpoolCleanup> {
        let path = directory.join("transfers.sqlite");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SpoolCleanup::Retained);
            }
            Err(error) => return Err(storage(error)),
            Ok(_) => {}
        }
        if crate::trust_boundary::is_link_like(
            &std::fs::symlink_metadata(directory).map_err(storage)?,
        ) {
            return Err(corrupt());
        }
        crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(storage)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(storage)?;
        let encoded: String = db
            .query_row("SELECT value FROM identity WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .map_err(|_| corrupt())?;
        if encoded.len() > 16 * 1024 {
            return Err(corrupt());
        }
        let identity: JobIdentity = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
        if identity.job_id != job_id {
            return Err(corrupt());
        }
        Self { db, directory: directory.into(), identity }
            .cleanup_terminal_spools(store, format_repository_id)
    }

    pub(crate) fn progress(directory: &Path, job_id: &str) -> Result<(u64, u64, u64, u64)> {
        let path = directory.join("transfers.sqlite");
        crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(storage)?;
        let encoded: String = db
            .query_row("SELECT value FROM identity WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .map_err(storage)?;
        let identity: JobIdentity = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
        if identity.job_id != job_id {
            return Err(corrupt());
        }
        let values:(i64,i64,i64,i64)=db.query_row("SELECT COALESCE(SUM(CASE WHEN receipt IS NOT NULL THEN json_extract(intent,'$.byteLength') ELSE COALESCE(json_extract(resume,'$.confirmedOffset'),0) END),0),COALESCE(SUM(json_extract(intent,'$.byteLength')),0),COALESCE(SUM(receipt IS NOT NULL),0),COUNT(*) FROM objects",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).map_err(storage)?;
        Ok((
            values.0.try_into().map_err(|_| corrupt())?,
            values.1.try_into().map_err(|_| corrupt())?,
            values.2.try_into().map_err(|_| corrupt())?,
            values.3.try_into().map_err(|_| corrupt())?,
        ))
    }
    pub(crate) fn job_id(&self) -> &str {
        &self.identity.job_id
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// A previous receipt is historical once a fresh remote observation says
    /// the object is missing. Keep its immutable source and upload session.
    pub(crate) fn reopen_object(&mut self, object: &str) -> Result<()> {
        if self.db.execute("UPDATE objects SET receipt=NULL WHERE id=?1", [object])
            .map_err(storage)? != 1
        {
            return Err(corrupt());
        }
        Ok(())
    }

    /// What one job has already put in the repository, read without taking the
    /// journal over. A cleanup needs this because an unfinished job's fragments
    /// are named by nothing else.
    pub(crate) fn uploaded(directory: &Path, job_id: &str) -> Result<Vec<ObjectReceipt>> {
        let path = directory.join("transfers.sqlite");
        crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(storage)?;
        let encoded: String = db
            .query_row("SELECT value FROM identity WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .map_err(storage)?;
        let identity: JobIdentity = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
        if identity.job_id != job_id {
            return Err(corrupt());
        }
        let mut query = db
            .prepare("SELECT receipt FROM objects WHERE receipt IS NOT NULL")
            .map_err(storage)?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage)?;
        let mut receipts = Vec::new();
        for row in rows {
            let encoded = row.map_err(storage)?;
            if encoded.len() > 64 * 1024 {
                return Err(corrupt());
            }
            receipts.push(serde_json::from_str(&encoded).map_err(|_| corrupt())?);
        }
        Ok(receipts)
    }
    pub fn open(directory: &Path, identity: JobIdentity) -> Result<Self> {
        if [
            &identity.job_id,
            &identity.connection_id,
            &identity.repository_id,
            &identity.capture_id,
        ]
        .iter()
        .any(|value| value.is_empty())
        {
            return Err(corrupt());
        }
        std::fs::create_dir_all(directory).map_err(storage)?;
        if crate::trust_boundary::is_link_like(
            &std::fs::symlink_metadata(directory).map_err(storage)?,
        ) {
            return Err(corrupt());
        }
        let path = directory.join("transfers.sqlite");
        let fresh = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                file.sync_all().map_err(storage)?;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
                false
            }
            Err(error) => return Err(storage(error)),
        };
        let db = Connection::open(&path).map_err(storage)?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(storage)?;
        if fresh {
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE identity(singleton INTEGER PRIMARY KEY CHECK(singleton=1), value TEXT NOT NULL);
                CREATE TABLE objects(id TEXT PRIMARY KEY,intent TEXT NOT NULL,attempted INTEGER NOT NULL CHECK(attempted IN (0,1)),resume TEXT,receipt TEXT);").map_err(storage)?;
            db.execute(
                "INSERT INTO identity VALUES(1,?1)",
                [serde_json::to_string(&identity).map_err(storage)?],
            )
            .map_err(storage)?;
            db.execute_batch("COMMIT").map_err(storage)?;
            crate::trust_boundary::sync_directory(directory).map_err(storage)?;
        }
        let encoded: String = db
            .query_row("SELECT value FROM identity WHERE singleton=1", [], |row| {
                row.get(0)
            })
            .map_err(|_| corrupt())?;
        if encoded.len() > 16 * 1024
            || serde_json::from_str::<JobIdentity>(&encoded).map_err(|_| corrupt())? != identity
        {
            return Err(corrupt());
        }
        Ok(Self {
            db,
            directory: directory.into(),
            identity,
        })
    }

    pub fn spool_path(&self, object_id: &str) -> PathBuf {
        self.directory.join(format!(
            "{}.spool",
            hex::encode(risunest_external_storage_format::content_identity::hash(
                object_id.as_bytes()
            ))
        ))
    }

    /// The producer has already closed and fsynced the immutable ciphertext.
    pub fn register(&mut self, intent: &ObjectIntent) -> Result<()> {
        if intent.job_id != self.identity.job_id
            || intent.repository_id != self.identity.repository_id
            || intent.object_id.is_empty()
            || !crate::trust_boundary::is_lower_hex_256(&intent.sha256)
        {
            return Err(corrupt());
        }
        SpoolSource::verified(
            &self.spool_path(&intent.object_id),
            intent.byte_length,
            &intent.sha256,
        )?;
        crate::trust_boundary::sync_directory(&self.directory).map_err(storage)?;
        if let Some(existing) = self.record(&intent.object_id)? {
            return if existing.intent == *intent {
                Ok(())
            } else {
                Err(corrupt())
            };
        }
        self.db
            .execute(
                "INSERT INTO objects VALUES(?1,?2,0,NULL,NULL)",
                params![
                    intent.object_id,
                    serde_json::to_string(intent).map_err(storage)?
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    pub fn record(&self, object: &str) -> Result<Option<TransferRecord>> {
        let row: Option<(String, bool, Option<String>, Option<String>)> = self
            .db
            .query_row(
                "SELECT intent,attempted,resume,receipt FROM objects WHERE id=?1",
                [object],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((intent, attempted, resume, receipt)) = row else {
            return Ok(None);
        };
        if intent.len() > 64 * 1024
            || resume.as_ref().is_some_and(|s| s.len() > 16 * 1024)
            || receipt.as_ref().is_some_and(|s| s.len() > 64 * 1024)
        {
            return Err(corrupt());
        }
        let intent: ObjectIntent = serde_json::from_str(&intent).map_err(|_| corrupt())?;
        if intent.object_id != object
            || intent.job_id != self.identity.job_id
            || intent.repository_id != self.identity.repository_id
        {
            return Err(corrupt());
        }
        let resume = resume
            .map(|s| {
                serde_json::from_str::<SealedResume>(&s)
                    .map(SealedResume::into_state)
                    .map_err(|_| corrupt())
            })
            .transpose()?;
        if resume
            .as_ref()
            .is_some_and(|r| r.confirmed_offset > intent.byte_length || r.sealed_state.0.is_empty())
        {
            return Err(corrupt());
        }
        let receipt = receipt
            .map(|s| serde_json::from_str(&s).map_err(|_| corrupt()))
            .transpose()?;
        Ok(Some(TransferRecord {
            intent,
            attempted,
            resume,
            receipt,
        }))
    }

    pub fn attempted(&mut self, object: &str, resume: Option<&ResumeState>) -> Result<()> {
        let resume = resume
            .map(|r| serde_json::to_string(&SealedResume::from_state(r)))
            .transpose()
            .map_err(storage)?;
        if self
            .db
            .execute(
                "UPDATE objects SET attempted=1,resume=?2 WHERE id=?1 AND receipt IS NULL",
                params![object, resume],
            )
            .map_err(storage)?
            != 1
        {
            return Err(corrupt());
        }
        Ok(())
    }

    pub fn complete(
        &mut self,
        intent: &ObjectIntent,
        repository: &RepositoryHandle,
        receipt: &ObjectReceipt,
    ) -> Result<()> {
        validate_receipt(intent, repository, receipt)?;
        if self
            .db
            .execute(
                "UPDATE objects SET receipt=?2 WHERE id=?1 AND attempted=1",
                params![
                    intent.object_id,
                    serde_json::to_string(receipt).map_err(storage)?
                ],
            )
            .map_err(storage)?
            != 1
        {
            return Err(corrupt());
        }
        Ok(())
    }

    /// The actual worker must have ended before handing its journal here. PDS
    /// completion is written only after the final remote root is confirmed;
    /// cancellation is terminal only after any unknown publication is resolved.
    /// This releases this job's capture reference, never another owner or the
    /// shared capture payload. Conflicts and exports can retain every spool.
    pub(crate) fn cleanup_terminal_spools(
        self,
        store: &mut crate::persistent_store::PersistentStore,
        format_repository_id: &str,
    ) -> Result<SpoolCleanup> {
        let job = store.external_job(&self.identity.job_id).map_err(storage)?
            .ok_or_else(corrupt)?;
        if job.id != self.identity.job_id || job.connection_id != self.identity.connection_id
            || job.repository_id != format_repository_id || job.capture_id != self.identity.capture_id
            || job.identity != self.identity.capture
        {
            return Err(corrupt());
        }
        if !matches!(job.phase.as_str(), "complete" | "cancelled") {
            return Ok(SpoolCleanup::Retained);
        }
        if !store.release_external_capture(&job.capture_id, &job.id).map_err(storage)? {
            return Ok(SpoolCleanup::Retained);
        }
        if crate::trust_boundary::is_link_like(
            &std::fs::symlink_metadata(&self.directory).map_err(storage)?,
        ) {
            return Err(corrupt());
        }
        let ids = {
            let mut query = self.db.prepare("SELECT id FROM objects ORDER BY id").map_err(storage)?;
            let rows = query.query_map([], |row| row.get::<_, String>(0)).map_err(storage)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(storage)?
        };
        // Validate all registered files before the first unlink. Unregistered
        // partial files, receive caches and shared source paths are not swept.
        let mut files = Vec::new();
        for id in ids {
            self.record(&id)?.ok_or_else(corrupt)?;
            let path = self.spool_path(&id);
            match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(storage(error)),
                Ok(_) => {}
            }
            let file = crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
            let length = file.metadata().map_err(storage)?.len();
            files.push((path, length));
        }
        let mut objects = 0u64;
        let mut bytes = 0u64;
        for (path, length) in files {
            match std::fs::remove_file(path) {
                Ok(()) => {
                    objects = objects.checked_add(1).ok_or_else(corrupt)?;
                    bytes = bytes.checked_add(length).ok_or_else(corrupt)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage(error)),
            }
        }
        crate::trust_boundary::sync_directory(&self.directory).map_err(storage)?;
        Ok(SpoolCleanup::Removed { objects, bytes })
    }

    pub(crate) async fn release_completed_sessions(
        &mut self,
        vault: &dyn super::auth::SecretVault,
    ) -> Result<()> {
        let entries = {
            let mut query = self.db.prepare("SELECT id,resume FROM objects WHERE receipt IS NOT NULL AND resume IS NOT NULL").map_err(storage)?;
            let rows = query
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(storage)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for (id, encoded) in entries {
            let resume: SealedResume = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
            vault.remove(&SecretRef(resume.reference)).await?;
            self.db
                .execute(
                    "UPDATE objects SET resume=NULL WHERE id=?1 AND receipt IS NOT NULL",
                    [id],
                )
                .map_err(storage)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::fake,
        persistent_store::{external_storage_state, PersistentStore},
    };

    fn fixture() -> (tempfile::TempDir, PersistentStore, JobIdentity, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(root.path()).unwrap();
        let capture = store.external_identity().unwrap();
        // Seed only synthetic authority through the same capture registration
        // operation used by PDS tests. No user data or provider is involved.
        let mut db = Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        let tx = db.transaction().unwrap();
        external_storage_state::register_capture(
            &tx, "capture", &capture, "scope", "logical-v1", "", &"a".repeat(64), "connection",
        ).unwrap();
        tx.commit().unwrap();
        drop(db);
        store.external_prepare_backup("job", "connection", "format-repository", "capture", "point").unwrap();
        let identity = JobIdentity {
            job_id: "job".into(), connection_id: "connection".into(),
            repository_id: fake::repository().repository_id, capture_id: "capture".into(), capture,
        };
        let directory = root.path().join("job");
        let mut journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        let bytes = b"synthetic sealed ciphertext";
        let intent = ObjectIntent {
            repository_id: identity.repository_id.clone(), job_id: identity.job_id.clone(),
            object_id: "pack".into(), role: ObjectRole::Pack,
            byte_length: bytes.len() as u64, sha256: risunest_sync_wire::hash(bytes),
        };
        std::fs::write(journal.spool_path("pack"), bytes).unwrap();
        journal.register(&intent).unwrap();
        drop(journal);
        (root, store, identity, directory)
    }

    #[test]
    fn c_terminal_spool_cleanup_preserves_failed_work_and_conflict_or_export_owners() {
        let (root, mut store, identity, directory) = fixture();
        let journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        let spool = journal.spool_path("pack");
        assert_eq!(journal.cleanup_terminal_spools(&mut store, "format-repository").unwrap(), SpoolCleanup::Retained);
        assert!(spool.is_file());
        store.retain_external_capture("capture", "conflict-owner").unwrap();
        store.retain_external_capture("capture", "export-owner").unwrap();
        store.external_finish_backup("job", "point", "bundle", "authenticated-synthetic-observation").unwrap();
        let journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        assert_eq!(journal.cleanup_terminal_spools(&mut store, "format-repository").unwrap(), SpoolCleanup::Retained);
        assert!(spool.is_file());
        assert!(!store.release_external_capture("capture", "conflict-owner").unwrap());
        let journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        assert_eq!(journal.cleanup_terminal_spools(&mut store, "format-repository").unwrap(), SpoolCleanup::Retained);
        assert!(store.release_external_capture("capture", "export-owner").unwrap());
        let shared = root.path().join("shared-source.spool");
        std::fs::write(&shared, b"shared source").unwrap();
        assert_eq!(TransferJournal::cleanup_terminal_spools_at(
            &directory, "job", &mut store, "format-repository",
        ).unwrap(),
            SpoolCleanup::Removed { objects: 1, bytes: b"synthetic sealed ciphertext".len() as u64 });
        assert!(!spool.exists());
        assert_eq!(std::fs::read(shared).unwrap(), b"shared source");
        assert_eq!(TransferJournal::cleanup_terminal_spools_at(
            &directory, "job", &mut store, "format-repository",
        ).unwrap(),
            SpoolCleanup::Removed { objects: 0, bytes: 0 });
        assert_eq!(TransferJournal::cleanup_terminal_spools_at(
            &root.path().join("no-journal"), "job", &mut store, "format-repository",
        ).unwrap(), SpoolCleanup::Retained);
    }

    #[test]
    fn c_only_authoritatively_cancelled_matching_jobs_can_discard_their_spools() {
        let (_root, mut store, identity, directory) = fixture();
        let journal = TransferJournal::open(&directory, identity.clone()).unwrap();
        let spool = journal.spool_path("pack");
        assert_eq!(journal.cleanup_terminal_spools(&mut store, "wrong-repository").unwrap_err().kind, ErrorKind::Corrupt);
        assert!(spool.exists());
        store.external_cancel_prepared("job").unwrap();
        let journal = TransferJournal::open(&directory, identity).unwrap();
        assert!(matches!(journal.cleanup_terminal_spools(&mut store, "format-repository").unwrap(),
            SpoolCleanup::Removed { objects: 1, .. }));
        assert!(!spool.exists());
    }
}

pub(crate) fn validate_receipt(
    intent: &ObjectIntent,
    repository: &RepositoryHandle,
    receipt: &ObjectReceipt,
) -> Result<()> {
    intent.validate(repository)?;
    receipt.locator.validate_for(repository)?;
    if !receipt.complete
        || receipt.byte_length != intent.byte_length
        || receipt
            .checksum
            .as_ref()
            .is_some_and(|c| c.algorithm.eq_ignore_ascii_case("sha256") && c.value != intent.sha256)
    {
        return Err(corrupt());
    }
    Ok(())
}
