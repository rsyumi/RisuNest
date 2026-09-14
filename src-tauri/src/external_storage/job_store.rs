//! Durable native requests survive WebView maintenance and process interruption.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use crate::persistent_store::sync_selection::CaptureIdentity;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Mutex};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    Backup,
    Sync,
    Restore,
    PinHistory,
    ResolveConflict,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StartJobRequest {
    pub connection_id: String,
    pub kind: JobKind,
    pub snapshot_id: Option<String>,
    pub conflict_id: Option<String>,
    pub choice: Option<String>,
    pub restore_areas: Option<Vec<String>>,
    pub target_revision: Option<String>,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub reason: Option<String>,
}
impl StartJobRequest {
    pub fn validate(&self) -> Result<()> {
        let valid = |s: &str| !s.is_empty() && s.len() <= 1024 && !s.contains('\0');
        let decimal = |s: &str| {
            s == "0"
                || s.as_bytes()
                    .first()
                    .is_some_and(|byte| (b'1'..=b'9').contains(byte))
                    && s.as_bytes()[1..].iter().all(u8::is_ascii_digit)
        };
        if !valid(&self.connection_id)
            || [&self.snapshot_id, &self.conflict_id, &self.session_id]
                .into_iter()
                .flatten()
                .any(|s| !valid(s))
            || self
                .target_revision
                .as_ref()
                .is_some_and(|s| !decimal(s) || s.parse::<i64>().is_err())
            || self
                .reason
                .as_deref()
                .is_some_and(|s| !["automatic", "manual", "exitDrain"].contains(&s))
            || self
                .session
                .as_deref()
                .is_some_and(|s| !["foreground", "exitDrain"].contains(&s))
            || self
                .choice
                .as_deref()
                .is_some_and(|s| !["local", "remote"].contains(&s))
            || self.restore_areas.as_ref().is_some_and(|areas| {
                areas.len() > 4
                    || areas.iter().any(|s| {
                        ![
                            "library",
                            "referencedAssets",
                            "deviceSettings",
                            "devicePlugins",
                        ]
                        .contains(&s.as_str())
                    })
            })
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DurableJob {
    pub id: String,
    pub request: StartJobRequest,
    pub summary: Value,
    pub device_capture_id: Option<String>,
    pub capture_id: Option<String>,
    pub snapshot_id: String,
    pub admission_identity: CaptureIdentity,
}
impl DurableJob {
    pub fn new(
        request: StartJobRequest,
        device: bool,
        now: u64,
        admission_identity: CaptureIdentity,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let summary = json!({"id":id,"connectionId":request.connection_id,"kind":request.kind,
            "state":if device {"waiting"} else {"queued"},"phase":if device {"device-capture"} else {"queued"},
            "completedBytes":"0","completedItems":"0","startedAtMs":now.to_string(),"updatedAtMs":now.to_string()});
        Self {
            id,
            request,
            summary,
            device_capture_id: None,
            capture_id: None,
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            admission_identity,
        }
    }
    pub fn terminal(&self) -> bool {
        matches!(
            self.summary["state"].as_str(),
            Some("succeeded" | "failed" | "cancelled")
        )
    }
}
fn failure(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
pub(crate) struct JobStore(Connection);
const STATE_TERMINAL_LIMIT: i64 = 32;
impl JobStore {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(failure)?;
        let db = Connection::open(root.join("external-jobs.sqlite")).map_err(failure)?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS external_requests(
                 id TEXT PRIMARY KEY,
                 connection_id TEXT NOT NULL,
                 value TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS external_requests_pending
             ON external_requests(connection_id)
             WHERE COALESCE(json_extract(value,'$.summary.state'),'')
                 NOT IN ('succeeded','failed','cancelled');",
        )
        .map_err(failure)?;
        Ok(Self(db))
    }
    pub fn put(&self, job: &DurableJob) -> Result<()> {
        let encoded = serde_json::to_string(job).map_err(failure)?;
        if self
            .0
            .execute(
                "UPDATE external_requests SET value=?2 WHERE id=?1 AND connection_id=?3",
                rusqlite::params![job.id, encoded, job.request.connection_id],
            )
            .map_err(failure)?
            == 1
        {
            return Ok(());
        }
        if self.0.execute("INSERT INTO external_requests SELECT ?1,?2,?3 WHERE NOT EXISTS(SELECT 1 FROM external_requests WHERE connection_id=?2 AND COALESCE(json_extract(value,'$.summary.state'),'') NOT IN ('succeeded','failed','cancelled'))",rusqlite::params![job.id,job.request.connection_id,encoded]).map_err(failure)? != 1 {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }
    pub fn read(&self, id: &str) -> Result<DurableJob> {
        let bytes: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM external_requests WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(failure)?;
        Self::decode(bytes.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?)
    }
    fn decode(bytes: String) -> Result<DurableJob> {
        if bytes.len() > 128 * 1024 {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let job: DurableJob =
            serde_json::from_str(&bytes).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        job.request.validate()?;
        if job.admission_identity.revision < 0
            || [
                &job.admission_identity.store_id,
                &job.admission_identity.library_epoch,
                &job.admission_identity.generation,
                &job.admission_identity.selection_epoch,
            ]
            .iter()
            .any(|value| value.is_empty() || value.len() > 1024 || value.contains('\0'))
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(job)
    }
    pub fn list(&self) -> Result<Vec<DurableJob>> {
        let mut statement = self
            .0
            .prepare("SELECT value FROM external_requests ORDER BY rowid DESC")
            .map_err(failure)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(failure)?;
        rows.map(|row| Self::decode(row.map_err(failure)?))
            .collect()
    }

    pub fn list_pending(&self) -> Result<Vec<DurableJob>> {
        let mut statement = self
            .0
            .prepare(
                "SELECT value FROM external_requests
                 WHERE COALESCE(json_extract(value,'$.summary.state'),'')
                    NOT IN ('succeeded','failed','cancelled')",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(failure)?;
        rows.map(|row| Self::decode(row.map_err(failure)?))
            .collect()
    }

    /// Every live job plus a bounded recent terminal history for renderer state.
    pub fn list_for_state(&self) -> Result<Vec<DurableJob>> {
        let mut result = self.list_pending()?;
        let mut statement = self
            .0
            .prepare(
                "SELECT value FROM external_requests
                 WHERE json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
                 ORDER BY rowid DESC LIMIT ?1",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map([STATE_TERMINAL_LIMIT], |row| row.get::<_, String>(0))
            .map_err(failure)?;
        result.extend(
            rows.map(|row| Self::decode(row.map_err(failure)?))
                .collect::<Result<Vec<_>>>()?,
        );
        Ok(result)
    }
    pub fn attach_device(&mut self, ids: &[String], capture: &str) -> Result<()> {
        if ids.is_empty() || capture.is_empty() {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let mut jobs = ids
            .iter()
            .map(|id| self.read(id))
            .collect::<Result<Vec<_>>>()?;
        if jobs
            .iter()
            .any(|job| job.summary["phase"] != "device-capture" || job.terminal())
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let tx = self.0.transaction().map_err(failure)?;
        for job in &mut jobs {
            job.device_capture_id = Some(capture.into());
            job.summary["phase"] = json!("queued");
            job.summary["state"] = json!("queued");
            tx.execute(
                "UPDATE external_requests SET value=?2 WHERE id=?1",
                rusqlite::params![job.id, serde_json::to_string(job).map_err(failure)?],
            )
            .map_err(failure)?;
        }
        tx.commit().map_err(failure)
    }
}
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub kind: String,
    pub id: String,
}
#[derive(Default)]
pub(crate) struct JobCommandState {
    pub root: std::sync::OnceLock<std::path::PathBuf>,
    pub active: Mutex<HashMap<String, (String, Cancellation)>>,
    pub session: Mutex<Session>,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> StartJobRequest {
        serde_json::from_value(json!({"connectionId":"synthetic","kind":"backup"})).unwrap()
    }
    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        }
    }
    #[test]
    fn maintenance_capture_attaches_atomically_and_survives_restart() {
        let root = tempfile::tempdir().unwrap();
        let mut db = JobStore::open(root.path()).unwrap();
        let a = DurableJob::new(request(), true, 1, identity());
        let mut other = request();
        other.connection_id = "synthetic-other".into();
        let b = DurableJob::new(other, true, 1, identity());
        db.put(&a).unwrap();
        db.put(&b).unwrap();
        assert!(db
            .attach_device(&[a.id.clone(), "missing".into()], "capture")
            .is_err());
        assert_eq!(db.read(&a.id).unwrap().device_capture_id, None);
        db.attach_device(&[a.id.clone(), b.id.clone()], "capture")
            .unwrap();
        drop(db);
        let db = JobStore::open(root.path()).unwrap();
        assert_eq!(
            db.read(&a.id).unwrap().device_capture_id.as_deref(),
            Some("capture")
        );
        assert!(db.attach_device_for_test(&a.id).is_err());
    }
    impl JobStore {
        fn attach_device_for_test(mut self, id: &str) -> Result<()> {
            self.attach_device(&[id.into()], "other")
        }
    }
    #[test]
    fn revisions_and_session_inputs_are_validated() {
        let mut input = request();
        input.target_revision = Some("-1".into());
        assert!(input.validate().is_err());
        input.target_revision = Some("9223372036854775807".into());
        assert!(input.validate().is_ok());
        input.target_revision = Some("+1".into());
        assert!(input.validate().is_err());
        input.target_revision = Some("01".into());
        assert!(input.validate().is_err());
        input.target_revision = Some("0".into());
        assert!(input.validate().is_ok());
        input.reason = Some("hidden-retry".into());
        assert!(input.validate().is_err());
    }
    #[test]
    fn durable_admission_identity_is_required_and_validated() {
        let root = tempfile::tempdir().unwrap();
        let db = JobStore::open(root.path()).unwrap();
        let mut invalid = identity();
        invalid.library_epoch.clear();
        let job = DurableJob::new(request(), false, 1, invalid);
        db.put(&job).unwrap();
        assert_eq!(db.read(&job.id).err().unwrap().kind, ErrorKind::Corrupt);
    }

    #[test]
    fn renderer_state_keeps_all_pending_and_only_recent_terminal_jobs() {
        let root = tempfile::tempdir().unwrap();
        let db = JobStore::open(root.path()).unwrap();
        let mut terminal_ids = Vec::new();
        for index in 0..40 {
            let mut completed = DurableJob::new(request(), false, index, identity());
            completed.summary["state"] = json!("succeeded");
            completed.summary["phase"] = json!("complete");
            terminal_ids.push(completed.id.clone());
            db.put(&completed).unwrap();
        }
        let mut first = request();
        first.connection_id = "pending-a".into();
        let first = DurableJob::new(first, false, 41, identity());
        let mut second = request();
        second.connection_id = "pending-b".into();
        let second = DurableJob::new(second, false, 42, identity());
        db.put(&first).unwrap();
        db.put(&second).unwrap();

        let pending = db.list_pending().unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|job| job.id == first.id));
        assert!(pending.iter().any(|job| job.id == second.id));

        let state = db.list_for_state().unwrap();
        assert_eq!(state.len(), 34);
        assert!(state.iter().any(|job| job.id == first.id));
        assert!(state.iter().any(|job| job.id == second.id));
        assert!(state.iter().any(|job| job.id == terminal_ids[39]));
        assert!(!state.iter().any(|job| job.id == terminal_ids[7]));
    }

    #[test]
    fn pending_lookup_uses_the_partial_index() {
        let root = tempfile::tempdir().unwrap();
        let db = JobStore::open(root.path()).unwrap();
        let detail: Vec<String> =
            db.0.prepare(
                "EXPLAIN QUERY PLAN SELECT value FROM external_requests
                 WHERE COALESCE(json_extract(value,'$.summary.state'),'')
                    NOT IN ('succeeded','failed','cancelled')",
            )
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(
            detail
                .iter()
                .any(|step| step.contains("external_requests_pending")),
            "query plan did not use pending index: {detail:?}"
        );
    }
}
