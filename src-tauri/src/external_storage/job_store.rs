//! Durable native requests survive WebView maintenance and process interruption.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use crate::persistent_store::sync_selection::CaptureIdentity;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::{Arc, Mutex}};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    Backup,
    Sync,
    Restore,
    PinHistory,
    DeleteHistory,
    ResolveConflict,
    Cleanup,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StartJobRequest {
    pub connection_id: String,
    pub kind: JobKind,
    pub snapshot_id: Option<String>,
    pub point_id: Option<String>,
    pub point_observation: Option<String>,
    pub confirm_other_device: Option<bool>,
    pub confirm_last_retained: Option<bool>,
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
        // A removal is described entirely by the connection it runs on.
        if self.kind == JobKind::Cleanup
            && (self.snapshot_id.is_some()
                || self.point_id.is_some()
                || self.point_observation.is_some()
                || self.confirm_other_device.is_some()
                || self.confirm_last_retained.is_some()
                || self.conflict_id.is_some()
                || self.choice.is_some()
                || self.restore_areas.is_some()
                || self.target_revision.is_some())
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let delete_history = self.kind == JobKind::DeleteHistory;
        if delete_history
            != (self.point_id.is_some()
                && self.point_observation.is_some()
                && self.confirm_other_device.is_some()
                && self.confirm_last_retained.is_some())
            || (!delete_history
                && (self.point_id.is_some()
                    || self.point_observation.is_some()
                    || self.confirm_other_device.is_some()
                    || self.confirm_last_retained.is_some()))
            || (delete_history
                && (self.snapshot_id.is_some()
                    || self.conflict_id.is_some()
                    || self.choice.is_some()
                    || self.restore_areas.is_some()
                    || self.target_revision.is_some()))
            || !valid(&self.connection_id)
            || [&self.snapshot_id, &self.point_id, &self.conflict_id, &self.session_id]
                .into_iter()
                .flatten()
                .any(|s| !valid(s))
            || self.point_observation.as_ref().is_some_and(|s| s.is_empty() || s.len() > 256 * 1024 || s.contains('\0'))
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
                areas.len() > 5
                    || areas.iter().any(|s| {
                        ![
                            "library",
                            "referencedAssets",
                            "hypa",
                            "local-plugins",
                            "local-settings",
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
    pub capture_id: Option<String>,
    pub snapshot_id: String,
    pub admission_identity: CaptureIdentity,
    pub receive_staging_id: Option<String>,
}
impl DurableJob {
    pub fn new(
        request: StartJobRequest,
        _device: bool,
        now: u64,
        admission_identity: CaptureIdentity,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let summary = json!({"id":id,"connectionId":request.connection_id,"kind":request.kind,
            "state":"queued","phase":"queued",
            "completedBytes":"0","completedItems":"0","startedAtMs":now.to_string(),"updatedAtMs":now.to_string()});
        Self {
            id,
            request,
            summary,
            capture_id: None,
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            admission_identity,
            receive_staging_id: None,
        }
    }
    pub fn with_restore_id(mut self, id: String) -> Result<Self> {
        if self.request.kind != JobKind::Restore
            || uuid::Uuid::parse_str(&id).ok().is_none_or(|parsed| parsed.to_string() != id)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        self.summary["id"] = json!(id);
        self.id = id;
        Ok(self)
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
    /// Unlike put, a retried start must never replace a worker's durable state.
    pub fn insert_new(&self, job: &DurableJob) -> Result<bool> {
        let encoded = serde_json::to_string(job).map_err(failure)?;
        Ok(self.0.execute(
            "INSERT OR IGNORE INTO external_requests
             SELECT ?1,?2,?3 WHERE NOT EXISTS(
                 SELECT 1 FROM external_requests WHERE connection_id=?2
                 AND COALESCE(json_extract(value,'$.summary.state'),'') NOT IN ('succeeded','failed','cancelled'))",
            rusqlite::params![job.id, job.request.connection_id, encoded],
        ).map_err(failure)? == 1)
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
        if job.receive_staging_id.as_ref().is_some_and(|id| {
            !id.starts_with("staging-") || id.len() > 128 || id.contains('\0')
        }) || job.admission_identity.revision < 0
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
}
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub kind: String,
    pub id: String,
}
type ActiveJobs = Arc<Mutex<HashMap<String, (String, Cancellation)>>>;

#[derive(Clone)]
pub(crate) struct JobClaim {
    _owner: Arc<JobClaimOwner>,
}
impl JobClaim {
    pub(super) fn require_job(&self, state: &JobCommandState, job: &DurableJob) -> Result<()> {
        if self._owner.id != job.id || !Arc::ptr_eq(&self._owner.active, &state.active) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }
}

struct JobClaimOwner {
    id: String,
    active: ActiveJobs,
}
impl Drop for JobClaimOwner {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.id);
        }
    }
}

#[derive(Clone)]
pub(crate) struct AutomaticTarget {
    pub owner_job_id: String,
    pub request: StartJobRequest,
    pub identity: CaptureIdentity,
}

#[derive(Default)]
pub(crate) struct JobCommandState {
    pub root: std::sync::OnceLock<std::path::PathBuf>,
    pub active: ActiveJobs,
    pub session: Mutex<Session>,
    pub automatic_targets: Mutex<HashMap<String, AutomaticTarget>>,
    pub prepared_receives: Mutex<HashMap<String, super::sync_engine::PreparedReceive>>,
}
impl JobCommandState {
    pub fn claim(&self, job: &DurableJob) -> Result<(Cancellation, JobClaim)> {
        let cancel = Cancellation::default();
        let mut active = self.active.lock().map_err(failure)?;
        if active.contains_key(&job.id)
            || active.values().any(|(connection, _)| connection == &job.request.connection_id)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        active.insert(job.id.clone(), (job.request.connection_id.clone(), cancel.clone()));
        Ok((cancel, JobClaim { _owner: Arc::new(JobClaimOwner {
            id: job.id.clone(), active: self.active.clone(),
        }) }))
    }

    pub fn coalesce_automatic(
        &self,
        running: &DurableJob,
        request: &StartJobRequest,
        identity: &CaptureIdentity,
    ) -> Result<bool> {
        if running.request.kind != JobKind::Sync
            || request.kind != JobKind::Sync
            || running.request.reason.as_deref() != Some("automatic")
            || request.reason.as_deref() != Some("automatic")
            || running.request.connection_id != request.connection_id
        {
            return Ok(false);
        }
        super::runtime::require_admitted_library(running, identity)?;
        let target = requested_revision(request, identity.revision)?;
        if target <= requested_revision(&running.request, running.admission_identity.revision)? {
            return Ok(true);
        }
        let mut queued = self.automatic_targets.lock().map_err(failure)?;
        if queued.get(&request.connection_id).is_some_and(|held| {
            held.owner_job_id == running.id && requested_revision(&held.request, held.identity.revision)
                .is_ok_and(|revision| revision >= target)
        }) {
            return Ok(true);
        }
        let mut request = request.clone();
        request.target_revision = Some(target.to_string());
        queued.insert(request.connection_id.clone(), AutomaticTarget {
            owner_job_id: running.id.clone(),
            request,
            identity: identity.clone(),
        });
        Ok(true)
    }

    pub fn cancel_automatic_target(&self, job: &DurableJob) -> Result<()> {
        let mut queued = self.automatic_targets.lock().map_err(failure)?;
        if queued.get(&job.request.connection_id)
            .is_some_and(|target| target.owner_job_id == job.id)
        {
            queued.remove(&job.request.connection_id);
        }
        Ok(())
    }
}

pub(crate) fn requested_revision(request: &StartJobRequest, fallback: i64) -> Result<i64> {
    request.target_revision.as_deref().map_or(Ok(fallback), |value| {
        value.parse().map_err(|_| ProviderError::new(ErrorKind::Corrupt))
    })
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
    fn a_retried_restore_start_never_overwrites_its_original_job() {
        let root = tempfile::tempdir().unwrap();
        let jobs = JobStore::open(root.path()).unwrap();
        let input = serde_json::from_value(json!({
            "connectionId":"synthetic", "kind":"restore",
            "snapshotId":"snapshot", "targetRevision":"1"
        })).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let queued = DurableJob::new(input, false, 1, identity())
            .with_restore_id(id.clone()).unwrap();
        assert_eq!(queued.summary["id"], id);
        assert!(jobs.insert_new(&queued).unwrap());
        let mut running = queued.clone();
        running.summary["state"] = json!("running");
        running.summary["applicationStarted"] = json!(true);
        jobs.put(&running).unwrap();
        assert!(!jobs.insert_new(&queued).unwrap());
        assert_eq!(jobs.read(&id).unwrap().summary, running.summary);
        let other = queued.clone().with_restore_id(uuid::Uuid::new_v4().to_string()).unwrap();
        assert!(!jobs.insert_new(&other).unwrap());
        assert!(jobs.read(&other.id).is_err());
        running.summary["state"] = json!("succeeded");
        running.summary["result"] = json!({"receivedRevision":"2"});
        jobs.put(&running).unwrap();
        drop(jobs);
        let reopened = JobStore::open(root.path()).unwrap();
        assert!(!reopened.insert_new(&queued).unwrap());
        assert_eq!(reopened.read(&id).unwrap().summary, running.summary);
        assert!(queued.clone().with_restore_id("../job".into()).is_err());
        assert!(DurableJob::new(request(), false, 1, identity()).with_restore_id(id).is_err());
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
    /// A removal is described by the connection it runs on and nothing else,
    /// and the name it goes out under is the one the renderer already reads.
    #[test]
    fn a_cleanup_request_carries_nothing_but_its_connection() {
        let mut input: StartJobRequest =
            serde_json::from_value(json!({"connectionId":"synthetic","kind":"cleanup"})).unwrap();
        assert!(input.validate().is_ok());
        assert_eq!(serde_json::to_value(input.kind).unwrap(), json!("cleanup"));
        input.snapshot_id = Some("snapshot".into());
        assert!(input.validate().is_err());
        input.snapshot_id = None;
        input.restore_areas = Some(vec!["library".into()]);
        assert!(input.validate().is_err());
        input.restore_areas = None;
        input.choice = Some("local".into());
        assert!(input.validate().is_err());
        input.choice = None;
        // The session a job runs under is set by the start path for every kind.
        input.session = Some("foreground".into());
        input.session_id = Some("session".into());
        input.reason = Some("automatic".into());
        assert!(input.validate().is_ok());
    }

    #[test]
    fn selected_history_deletion_requires_one_exact_durable_observation_and_confirmations() {
        let observation = serde_json::to_string(&risunest_external_storage_format::snapshot::StoredObject {
            header: risunest_external_storage_format::snapshot::PublicObjectHeader::new(
                "repository".into(),
                "backup-point-point".into(),
                risunest_external_storage_format::snapshot::ObjectRole::BackupPoint,
                1,
            ).unwrap(),
            locator: risunest_external_storage_format::snapshot::WireLocator {
                connection_identity: "account/root".into(),
                collection: Some("points".into()),
                object: "opaque".into(),
            },
            ciphertext_length: 0,
            ciphertext_sha256: [1; 32],
            plaintext_length: 1,
            plaintext_sha256: [2; 32],
        }).unwrap();
        let mut input: StartJobRequest = serde_json::from_value(json!({
            "connectionId":"synthetic",
            "kind":"delete-history",
            "pointId":"point",
            "pointObservation":observation,
            "confirmOtherDevice":false,
            "confirmLastRetained":true
        })).unwrap();
        assert!(input.validate().is_ok());
        input.point_observation = None;
        assert!(input.validate().is_err());
        input.point_observation = Some("{}".into());
        input.snapshot_id = Some("snapshot".into());
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
    fn cancellation_keeps_the_worker_claim_until_its_owner_returns() {
        let state = JobCommandState::default();
        let job = DurableJob::new(request(), false, 1, identity());
        let (cancel, claim) = state.claim(&job).unwrap();
        let retained = claim.clone();
        cancel.cancel();
        assert!(state.claim(&job).is_err());
        let other = DurableJob::new(request(), false, 2, identity());
        assert!(state.claim(&other).is_err());
        drop(claim);
        assert!(state.claim(&other).is_err());
        drop(retained);
        assert!(state.active.lock().unwrap().is_empty());
        assert!(state.claim(&other).is_ok());
    }

    #[test]
    fn cleanup_claim_excludes_restart_until_settlement_finishes() {
        let state = Arc::new(JobCommandState::default());
        let job = DurableJob::new(request(), false, 1, identity());
        let (cancel, worker) = state.claim(&job).unwrap();
        cancel.cancel();
        drop(worker);
        let (_, cleanup) = state.claim(&job).unwrap();
        let contender_state = state.clone();
        let contender_job = job.clone();
        std::thread::spawn(move || {
            assert!(contender_state.claim(&contender_job).is_err());
            let mut other = contender_job;
            other.id = "different-job".into();
            assert!(contender_state.claim(&other).is_err());
            other.request.connection_id = "different-connection".into();
            assert!(contender_state.claim(&other).is_ok());
        }).join().unwrap();
        assert_eq!(state.active.lock().unwrap().len(), 1);
        drop(cleanup);
        assert!(state.claim(&job).is_ok());
    }

    #[test]
    fn a_claim_is_bound_to_its_job_and_runtime_instance() {
        let state = JobCommandState::default();
        let other_state = JobCommandState::default();
        let job = DurableJob::new(request(), false, 1, identity());
        let (_, claim) = state.claim(&job).unwrap();
        assert!(claim.require_job(&state, &job).is_ok());
        assert!(claim.require_job(&other_state, &job).is_err());
        let mut other_job = job.clone();
        other_job.id = "different-job".into();
        assert!(claim.require_job(&state, &other_job).is_err());
    }

    #[test]
    fn stored_running_state_does_not_own_a_worker() {
        let state = JobCommandState::default();
        let mut job = DurableJob::new(request(), false, 1, identity());
        job.summary["state"] = json!("running");
        let (_, claim) = state.claim(&job).unwrap();
        assert_eq!(state.active.lock().unwrap().len(), 1);
        drop(claim);
        assert!(state.active.lock().unwrap().is_empty());
    }

    #[test]
    fn automatic_requests_keep_one_latest_target_without_changing_the_capture() {
        let state = JobCommandState::default();
        let mut request = request();
        request.kind = JobKind::Sync;
        request.reason = Some("automatic".into());
        request.target_revision = Some("1".into());
        let job = DurableJob::new(request.clone(), false, 1, identity());
        for revision in [2, 7, 3, 6] {
            request.target_revision = Some(revision.to_string());
            let mut current = identity();
            current.revision = revision;
            assert!(state.coalesce_automatic(&job, &request, &current).unwrap());
        }
        let queued = state.automatic_targets.lock().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued["synthetic"].request.target_revision.as_deref(), Some("7"));
        assert_eq!(job.request.target_revision.as_deref(), Some("1"));
        assert_eq!(job.admission_identity.revision, 1);
    }

    #[test]
    fn automatic_targets_do_not_absorb_explicit_operations_or_other_connections() {
        let state = JobCommandState::default();
        let mut automatic = request();
        automatic.kind = JobKind::Sync;
        automatic.reason = Some("automatic".into());
        let job = DurableJob::new(automatic.clone(), false, 1, identity());
        let mut explicit = automatic.clone();
        explicit.reason = Some("manual".into());
        for kind in [JobKind::Sync, JobKind::Backup, JobKind::Restore, JobKind::ResolveConflict] {
            explicit.kind = kind;
            assert!(!state.coalesce_automatic(&job, &explicit, &identity()).unwrap());
        }
        automatic.connection_id = "another".into();
        assert!(!state.coalesce_automatic(&job, &automatic, &identity()).unwrap());
        assert!(state.automatic_targets.lock().unwrap().is_empty());
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
