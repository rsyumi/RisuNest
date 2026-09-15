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

pub(crate) struct TransferJournal {
    db: Connection,
    directory: PathBuf,
    identity: JobIdentity,
}
impl TransferJournal {
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
