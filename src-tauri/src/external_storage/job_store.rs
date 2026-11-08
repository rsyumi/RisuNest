//! Durable native requests survive WebView maintenance and process interruption.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use crate::persistent_store::sync_selection::CaptureIdentity;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::{HashMap, HashSet}, path::Path, sync::{Arc, Mutex}};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    Backup,
    Restore,
    PinHistory,
    DeleteHistory,
    Cleanup,
    CheckRepository,
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
                || self.restore_areas.is_some()
                || self.target_revision.is_some())
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        // A check names at most the published state it reads.
        if self.kind == JobKind::CheckRepository
            && (self.restore_areas.is_some() || self.target_revision.is_some())
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
                    || self.restore_areas.is_some()
                    || self.target_revision.is_some()))
            || !valid(&self.connection_id)
            || [&self.snapshot_id, &self.point_id, &self.session_id]
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
                .is_some_and(|s| !["automatic", "manual"].contains(&s))
            || self
                .session
                .as_deref()
                .is_some_and(|s| s != "foreground")
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
    pub spool_released: bool,
}
impl DurableJob {
    pub fn new(
        request: StartJobRequest,
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
            spool_released: false,
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
fn failure(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient).caused(&error)
}
pub(crate) struct JobStore(Connection);
const STATE_TERMINAL_LIMIT: i64 = 32;
const MAINTENANCE_PAGE: i64 = 128;
const RESTORE_BODY_PAGE: i64 = 128;
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_RESTORE_BODY_PAGE: std::cell::Cell<i64> = const { std::cell::Cell::new(RESTORE_BODY_PAGE) };
}
fn restore_body_page_size() -> i64 {
    #[cfg(test)]
    return TEST_RESTORE_BODY_PAGE.with(std::cell::Cell::get);
    #[cfg(not(test))]
    RESTORE_BODY_PAGE
}
const RELEASED_PAGE_SQL: &str =
    "SELECT rowid,connection_id FROM external_requests
     WHERE json_extract(value,'$.spoolReleased')=1
       AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
       AND rowid>?1 ORDER BY rowid LIMIT ?2";
const TERMINAL_SPOOL_PAGE_SQL: &str =
    "SELECT rowid,value FROM external_requests
     WHERE json_extract(value,'$.spoolReleased')=0
       AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
       AND rowid>?1 ORDER BY rowid LIMIT ?2";
const FINISHED_RESTORE_PAGE_SQL: &str =
    "SELECT rowid,value FROM external_requests
     WHERE json_extract(value,'$.request.kind')='restore'
       AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
       AND rowid>?1 ORDER BY rowid LIMIT ?2";
impl JobStore {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(failure)?;
        let db = crate::sqlite_open::open(root.join("external-jobs.sqlite")).map_err(failure)?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(failure)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS external_requests(
                 id TEXT PRIMARY KEY,
                 connection_id TEXT NOT NULL,
                 value TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS external_requests_pending
             ON external_requests(connection_id)
             WHERE COALESCE(json_extract(value,'$.summary.state'),'')
                 NOT IN ('succeeded','failed','cancelled');
             CREATE INDEX IF NOT EXISTS external_requests_spool_owners
             ON external_requests(connection_id)
             WHERE json_extract(value,'$.spoolReleased')=0
                 AND COALESCE(json_extract(value,'$.summary.state'),'') NOT IN ('succeeded','cancelled');
             CREATE INDEX IF NOT EXISTS external_requests_terminal
             ON external_requests(connection_id)
             WHERE json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled');
             CREATE INDEX IF NOT EXISTS external_requests_released
             ON external_requests(json_extract(value,'$.spoolReleased'))
             WHERE json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled');
             CREATE INDEX IF NOT EXISTS external_requests_finished_kind
             ON external_requests(json_extract(value,'$.request.kind'))
             WHERE json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled');
             CREATE TABLE IF NOT EXISTS external_spool_cleanup(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 after_rowid INTEGER NOT NULL CHECK(after_rowid>=0)
             );
             CREATE TABLE IF NOT EXISTS external_history_cleanup(
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 after_rowid INTEGER NOT NULL CHECK(after_rowid>=0)
             );
             CREATE TABLE IF NOT EXISTS external_restore_bodies(
                 job_id TEXT NOT NULL REFERENCES external_requests(id) ON DELETE CASCADE,
                 hash TEXT NOT NULL,
                 source TEXT NOT NULL,
                 present INTEGER NOT NULL CHECK(present IN (0,1)),
                 settled INTEGER NOT NULL CHECK(settled IN (0,1)),
                 PRIMARY KEY(job_id,hash)
             );",
        )
        .map_err(failure)?;
        Ok(Self(db))
    }
    pub(crate) fn freeze_restore_bodies<O:serde::Serialize>(&self,job:&DurableJob,sources:&[super::lww_residency::PackedSource<O>],present:&std::collections::BTreeSet<String>) -> Result<()> {
        let tx=self.0.unchecked_transaction().map_err(failure)?;
        let mut current=self.read(&job.id)?;
        let ready=current.summary["restoreBodiesReady"]==true;
        let mut seen=std::collections::BTreeSet::new();
        for source in sources {
            if !seen.insert(source.hash.clone()) {return Err(ProviderError::new(ErrorKind::Corrupt));}
            let encoded=serde_json::to_string(source).map_err(failure)?;
            let is_present=present.contains(&source.hash);
            let previous:Option<(String,bool)>=tx.query_row("SELECT source,present FROM external_restore_bodies WHERE job_id=?1 AND hash=?2",rusqlite::params![job.id,source.hash],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(failure)?;
            if let Some(previous)=previous {
                if previous.0!=encoded {return Err(ProviderError::new(ErrorKind::Corrupt));}
            } else {
                if ready {return Err(ProviderError::new(ErrorKind::Corrupt));}
                tx.execute("INSERT INTO external_restore_bodies VALUES(?1,?2,?3,?4,?4)",rusqlite::params![job.id,source.hash,encoded,is_present]).map_err(failure)?;
            }
        }
        let count:i64=tx.query_row("SELECT count(*) FROM external_restore_bodies WHERE job_id=?1",[&job.id],|row|row.get(0)).map_err(failure)?;
        if count as usize!=sources.len() || present.iter().any(|hash|!seen.contains(hash)) {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        current.summary["restoreBodiesReady"]=json!(true);
        tx.execute("UPDATE external_requests SET value=?2 WHERE id=?1",rusqlite::params![job.id,serde_json::to_string(&current).map_err(failure)?]).map_err(failure)?;
        tx.commit().map_err(failure)
    }
    /// Reads the next page of unsettled bodies. Sources read through one
    /// interner share their catalog and pack objects across pages.
    pub(crate) fn restore_body_page(&self,job:&str,after:&str,interner:&mut super::lww_residency::ObjectInterner)->Result<Vec<super::lww_residency::SharedPackedSource>> {
        if self.read(job)?.summary["restoreBodiesReady"]!=true {return Err(ProviderError::new(ErrorKind::Corrupt));}
        let mut statement=self.0.prepare("SELECT source FROM external_restore_bodies WHERE job_id=?1 AND hash>?2 AND settled=0 ORDER BY hash LIMIT ?3").map_err(failure)?;
        let rows=statement.query_map(rusqlite::params![job,after,restore_body_page_size()],|row|row.get::<_,String>(0)).map_err(failure)?;
        rows.map(|row|{
            let source:super::lww_residency::PackedSource=serde_json::from_str(&row.map_err(failure)?).map_err(failure)?;
            Ok(source.interned(interner))
        }).collect()
    }
    pub(crate) fn restore_body(&self,job:&str,hash:&str)->Result<Option<super::lww_residency::PackedSource>> {
        let source:Option<String>=self.0.query_row("SELECT source FROM external_restore_bodies WHERE job_id=?1 AND hash=?2 AND settled=0",rusqlite::params![job,hash],|row|row.get(0)).optional().map_err(failure)?;
        source.map(|value|serde_json::from_str(&value).map_err(failure)).transpose()
    }
    pub(crate) fn settle_restore_body(&self,job:&str,hash:&str)->Result<()> {
        if self.0.execute("UPDATE external_restore_bodies SET settled=1 WHERE job_id=?1 AND hash=?2",rusqlite::params![job,hash]).map_err(failure)?!=1 {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }
    pub(crate) fn restore_bodies_settled(&self,job:&str)->Result<bool> {
        if self.read(job)?.summary["restoreBodiesReady"]!=true {return Ok(false);}
        let pending:bool=self.0.query_row("SELECT EXISTS(SELECT 1 FROM external_restore_bodies WHERE job_id=?1 AND settled=0)",[job],|row|row.get(0)).map_err(failure)?;
        Ok(!pending)
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

    /// Jobs whose transfer material may still be on disk. A job that ended in
    /// failure keeps its sealed ciphertext until a cleanup can release it.
    pub fn list_retaining(&self) -> Result<Vec<DurableJob>> {
        let mut statement = self
            .0
            .prepare(
                "SELECT value FROM external_requests
                 WHERE json_extract(value,'$.spoolReleased')=0
                    AND COALESCE(json_extract(value,'$.summary.state'),'')
                    NOT IN ('succeeded','cancelled')",
            )
            .map_err(failure)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(failure)?;
        rows.map(|row| Self::decode(row.map_err(failure)?))
            .collect()
    }
    pub fn release_spool(&self, id: &str) -> Result<()> {
        self.0.execute("UPDATE external_requests SET value=json_set(value,'$.spoolReleased',json('true')) WHERE id=?1", [id]).map_err(failure)?;
        Ok(())
    }

    /// Checks one indexed history page per pass. The cursor advances past
    /// retained recovery owners, so a long-lived owner cannot stall pruning.
    pub fn prune_released(&self) -> Result<()> {
        let tx = self.0.unchecked_transaction().map_err(failure)?;
        let after = tx.query_row(
            "SELECT after_rowid FROM external_history_cleanup WHERE singleton=1",
            [], |row| row.get::<_, i64>(0),
        ).optional().map_err(failure)?.unwrap_or(0);
        let rows = {
            let mut query = tx.prepare(RELEASED_PAGE_SQL).map_err(failure)?;
            let rows = query.query_map(rusqlite::params![after, MAINTENANCE_PAGE], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            }).map_err(failure)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(failure)?
        };
        for (rowid, connection) in &rows {
            tx.execute(
                "DELETE FROM external_requests
                 WHERE rowid=?1
                   AND json_extract(value,'$.spoolReleased')=1
                   AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
                   AND rowid<COALESCE((SELECT rowid FROM external_requests
                       WHERE connection_id=?2
                         AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
                       ORDER BY rowid DESC LIMIT 1 OFFSET ?3),0)",
                rusqlite::params![rowid, connection, STATE_TERMINAL_LIMIT - 1],
            ).map_err(failure)?;
        }
        let next = if rows.len() == MAINTENANCE_PAGE as usize {
            rows.last().map(|(rowid, _)| *rowid).unwrap_or(0)
        } else { 0 };
        tx.execute(
            "INSERT INTO external_history_cleanup(singleton,after_rowid) VALUES(1,?1)
             ON CONFLICT(singleton) DO UPDATE SET after_rowid=excluded.after_rowid",
            [next],
        ).map_err(failure)?;
        tx.commit().map_err(failure)
    }

    pub fn terminal_spool_page(&self) -> Result<Vec<(i64, DurableJob)>> {
        let after = self.0.query_row(
            "SELECT after_rowid FROM external_spool_cleanup WHERE singleton=1",
            [], |row| row.get::<_, i64>(0),
        ).optional().map_err(failure)?.unwrap_or(0);
        let mut query = self.0.prepare(TERMINAL_SPOOL_PAGE_SQL).map_err(failure)?;
        let rows = query.query_map(rusqlite::params![after, MAINTENANCE_PAGE], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        }).map_err(failure)?;
        rows.map(|row| {
            let (cursor, encoded) = row.map_err(failure)?;
            Ok((cursor, Self::decode(encoded)?))
        }).collect()
    }

    pub fn advance_terminal_spool_page(&self, page: &[(i64, DurableJob)]) -> Result<()> {
        let next = if page.len() == MAINTENANCE_PAGE as usize {
            page.last().map(|(rowid, _)| *rowid).unwrap_or(0)
        } else { 0 };
        self.0.execute(
            "INSERT INTO external_spool_cleanup(singleton,after_rowid) VALUES(1,?1)
             ON CONFLICT(singleton) DO UPDATE SET after_rowid=excluded.after_rowid",
            [next],
        ).map_err(failure)?;
        Ok(())
    }

    pub fn finished_restore_page(&self, after: i64) -> Result<Vec<(i64, DurableJob)>> {
        let mut query = self.0.prepare(FINISHED_RESTORE_PAGE_SQL).map_err(failure)?;
        let rows = query.query_map(rusqlite::params![after, MAINTENANCE_PAGE], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        }).map_err(failure)?;
        rows.map(|row| {
            let (cursor, encoded) = row.map_err(failure)?;
            Ok((cursor, Self::decode(encoded)?))
        }).collect()
    }

    /// Rewrites each unfinished job of a connection that `end` changes, in one
    /// transaction, so a removal never leaves some of them behind.
    pub fn end_connection_jobs(&self, connection: &str, end: impl Fn(&mut DurableJob) -> bool) -> Result<()> {
        let tx = self.0.unchecked_transaction().map_err(failure)?;
        let rows = {
            let mut statement = tx
                .prepare(
                    "SELECT value FROM external_requests WHERE connection_id=?1
                     AND COALESCE(json_extract(value,'$.summary.state'),'')
                        NOT IN ('succeeded','failed','cancelled')",
                )
                .map_err(failure)?;
            let rows = statement
                .query_map([connection], |row| row.get::<_, String>(0))
                .map_err(failure)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(failure)?
        };
        for encoded in rows {
            let mut job = Self::decode(encoded)?;
            if !end(&mut job) {
                continue;
            }
            let encoded = serde_json::to_string(&job).map_err(failure)?;
            tx.execute(
                "UPDATE external_requests SET value=?2 WHERE id=?1 AND connection_id=?3",
                rusqlite::params![job.id, encoded, connection],
            )
            .map_err(failure)?;
        }
        tx.commit().map_err(failure)
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

#[derive(Default)]
pub(crate) struct JobCommandState {
    cleanup_closed: std::sync::atomic::AtomicBool,
    background_workers: Arc<std::sync::atomic::AtomicUsize>,
    pub root: std::sync::OnceLock<std::path::PathBuf>,
    lease_ledger: Mutex<Option<Arc<super::lease_ledger::LocalLeaseLedger>>>,
    pub active: ActiveJobs,
    /// Connections being removed. No job of theirs may start meanwhile.
    removing: Arc<Mutex<HashSet<String>>>,
    pub session: Mutex<Session>,
    blocking_tasks: Mutex<HashMap<String, Vec<tokio::sync::oneshot::Receiver<()>>>>,
    /// The selection last read from the store, for a state read while device
    /// maintenance holds the store.
    pub shown_selection: Mutex<Option<Value>>,
}
pub(crate) struct BackgroundWorker(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for BackgroundWorker {
    fn drop(&mut self) { self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel); }
}

impl JobCommandState {
    pub(crate) fn open_lease_ledger(&self, cache: &std::path::Path) -> Result<()> {
        let mut slot = self.lease_ledger.lock().map_err(failure)?;
        if slot.is_some() { return Err(ProviderError::new(ErrorKind::PreconditionFailed)); }
        *slot = Some(Arc::new(super::lease_ledger::LocalLeaseLedger::open(cache)?));
        Ok(())
    }

    pub(crate) fn lease_ledger(&self) -> Result<Arc<super::lease_ledger::LocalLeaseLedger>> {
        let slot = self.lease_ledger.lock().map_err(failure)?;
        if self.cleanup_closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        slot.clone().ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))
    }

    pub(crate) fn close_lease_ledger_for_cleanup(&self) -> Result<()> {
        if !self.cleanup_closed.load(std::sync::atomic::Ordering::Acquire) || !self.cleanup_drained()? {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        let mut slot = self.lease_ledger.lock().map_err(failure)?;
        if slot.as_ref().is_some_and(|ledger| Arc::strong_count(ledger) != 1) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        slot.take();
        Ok(())
    }

    pub(crate) fn track_blocking(&self, job: &str) -> Result<tokio::sync::oneshot::Sender<()>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        self.blocking_tasks.lock().map_err(failure)?.entry(job.to_owned()).or_default().push(receive);
        Ok(send)
    }
    pub(crate) async fn settle_blocking(&self, job: &str) -> Result<()> {
        let pending = self.blocking_tasks.lock().map_err(failure)?.remove(job).unwrap_or_default();
        for task in pending { let _ = task.await; }
        Ok(())
    }

    pub(crate) fn track_worker(&self) -> BackgroundWorker {
        self.background_workers.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        BackgroundWorker(self.background_workers.clone())
    }

    /// One step of background maintenance that cleanup waits for, or `None`
    /// once cleanup has begun. Checked under the lock `cleanup_drained` reads
    /// the workers under, so a step never starts after cleanup saw none.
    pub(crate) fn background_step(&self) -> Option<BackgroundWorker> {
        let _active = self.active.lock().ok()?;
        if self.cleanup_closed.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        Some(self.track_worker())
    }

    pub(crate) fn begin_cleanup(&self) -> Result<()> {
        let active = self.active.lock().map_err(failure)?;
        self.cleanup_closed.store(true, std::sync::atomic::Ordering::Release);
        for (_, cancel) in active.values() { cancel.cancel(); }
        Ok(())
    }

    pub(crate) fn cleanup_drained(&self) -> Result<bool> {
        let active = self.active.lock().map_err(failure)?;
        if !active.is_empty() || self.background_workers.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return Ok(false);
        }
        *self.session.lock().map_err(failure)? = Session::default();
        Ok(true)
    }

    pub(crate) fn finish_cleanup(&self) -> Result<()> {
        if !self.cleanup_drained()? { return Err(ProviderError::new(ErrorKind::PreconditionFailed)); }
        self.cleanup_closed.store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    pub fn claim(&self, job: &DurableJob) -> Result<(Cancellation, JobClaim)> {
        self.claim_inner(job, false)
    }
    pub(crate) fn job_is_active(&self, id: &str) -> Result<bool> {
        Ok(self.active.lock().map_err(failure)?.contains_key(id))
    }
    pub(crate) fn claim_restore_settlement(&self, job: &DurableJob) -> Result<(Cancellation, JobClaim)> {
        if !matches!(job.request.kind, JobKind::Restore) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        self.claim_inner(job, true)
    }
    fn claim_inner(&self, job: &DurableJob, local_settlement: bool) -> Result<(Cancellation, JobClaim)> {
        let cancel = Cancellation::default();
        let mut active = self.active.lock().map_err(failure)?;
        if self.cleanup_closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        if active.contains_key(&job.id)
            || (!local_settlement
                && (active.values().any(|(connection, _)| connection == &job.request.connection_id)
                    || self.removing.lock().map_err(failure)?.contains(&job.request.connection_id)))
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        active.insert(job.id.clone(), (job.request.connection_id.clone(), cancel.clone()));
        Ok((cancel, JobClaim { _owner: Arc::new(JobClaimOwner {
            id: job.id.clone(), active: self.active.clone(),
        }) }))
    }

    /// Refuses while a job of the connection runs, and keeps any from starting
    /// until the returned hold is dropped. Both happen under the lock a claim
    /// takes, so no job can start between the check and the hold.
    pub(crate) fn hold_connection_removal(&self, connection: &str) -> Result<ConnectionRemovalHold> {
        let active = self.active.lock().map_err(failure)?;
        let mut removing = self.removing.lock().map_err(failure)?;
        if active.values().any(|(held, _)| held == connection) || !removing.insert(connection.to_owned()) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(ConnectionRemovalHold { connection: connection.to_owned(), removing: self.removing.clone() })
    }
}

pub(crate) struct ConnectionRemovalHold {
    connection: String,
    removing: Arc<Mutex<HashSet<String>>>,
}
impl Drop for ConnectionRemovalHold {
    fn drop(&mut self) {
        if let Ok(mut removing) = self.removing.lock() {
            removing.remove(&self.connection);
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_blocking_caller_keeps_admission_until_cancelled_worker_settles() {
        use crate::native_file_jobs::admission::Admission;
        use std::time::Duration;
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let state = JobCommandState::default();
            let job = DurableJob::new(request(), 1, identity());
            let (cancel, claim) = state.claim(&job).unwrap();
            let completion = state.track_blocking(&job.id).unwrap();
            let admission = Arc::new(Admission::default());
            let worker_admission = admission.clone();
            let worker_cancel = cancel.clone();
            let (arrived, arrival) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::sync_channel(1);
            let (completed, mut outcome) = tokio::sync::oneshot::channel();
            let caller = tokio::task::spawn_blocking(move || {
                let _completion = completion;
                let _permit = worker_admission.file(true).unwrap();
                arrived.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(10)).unwrap();
                completed.send(worker_cancel.check().err().map(|error| error.kind)).unwrap();
            });
            tokio::time::timeout(Duration::from_secs(10), arrival).await.unwrap().unwrap();
            // Dropping a JoinHandle detaches the running worker; it does not cancel it.
            drop(caller);
            assert_eq!(admission.file(false).err(), Some("library-operation-busy"));
            assert!(state.claim(&job).is_err());
            cancel.cancel();
            let wait = state.settle_blocking(&job.id);
            tokio::pin!(wait);
            assert!(tokio::time::timeout(Duration::from_millis(20), &mut wait).await.is_err());
            assert_eq!(admission.file(false).err(), Some("library-operation-busy"));
            assert!(matches!(outcome.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), &mut wait).await.unwrap().unwrap();
            assert_eq!(outcome.await.unwrap(), Some(ErrorKind::Cancelled));
            assert!(admission.file(false).is_ok());
            drop(claim);
            assert!(state.claim(&job).is_ok());
        });
    }

    /// A removal waits for a running job of its connection, and no job of that
    /// connection starts until the removal lets go.
    #[test]
    fn a_connection_removal_and_its_jobs_exclude_each_other() {
        let state = JobCommandState::default();
        let job = DurableJob::new(request(), 1, identity());
        let refused = |result: Result<_>| result.err().map(|error: ProviderError| error.kind);
        let (_, claim) = state.claim(&job).unwrap();
        assert_eq!(refused(state.hold_connection_removal("synthetic").map(|_| ())), Some(ErrorKind::PreconditionFailed));
        drop(claim);

        let hold = state.hold_connection_removal("synthetic").unwrap();
        assert_eq!(refused(state.claim(&job).map(|_| ())), Some(ErrorKind::PreconditionFailed));
        assert_eq!(refused(state.hold_connection_removal("synthetic").map(|_| ())), Some(ErrorKind::PreconditionFailed));
        let mut other = DurableJob::new(request(), 1, identity());
        other.request.connection_id = "synthetic-other".into();
        assert!(state.claim(&other).is_ok());

        drop(hold);
        assert!(state.claim(&job).is_ok());
    }

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
    fn restore_body_pages_share_one_copy_of_each_catalog_and_pack() {
        use crate::external_storage::lww_residency::{synthetic_packed, ObjectInterner};
        let root = tempfile::tempdir().unwrap();
        let jobs = JobStore::open(root.path()).unwrap();
        let job = DurableJob::new(request(), 1, identity());
        jobs.put(&job).unwrap();
        let sources = (0..130)
            .map(|index| synthetic_packed(&format!("{index:064x}"), "library", 1, &["pack"]))
            .collect::<Vec<_>>();
        jobs.freeze_restore_bodies(&job, &sources, &std::collections::BTreeSet::new()).unwrap();
        let mut interner = ObjectInterner::default();
        let mut pages = Vec::new();
        let mut after = String::new();
        loop {
            let page = jobs.restore_body_page(&job.id, &after, &mut interner).unwrap();
            let Some(last) = page.last() else { break };
            after = last.hash.clone();
            pages.push(page);
        }
        assert_eq!(pages.len(), 2);
        assert_eq!(pages.iter().map(Vec::len).sum::<usize>(), sources.len());
        let first = &pages[0][0];
        for source in pages.iter().flatten() {
            assert!(std::ptr::eq(&*first.catalog, &*source.catalog));
            assert!(std::ptr::eq(&*first.packs[0], &*source.packs[0]));
        }
    }

    #[test]
    fn cleanup_waits_for_worker_tail_after_the_job_claim_is_released() {
        let state = JobCommandState::default();
        let job = DurableJob::new(request(), 1, identity());
        let (cancel, claim) = state.claim(&job).unwrap();
        let worker = state.track_worker();
        let step = state.background_step().unwrap();
        state.begin_cleanup().unwrap();
        assert!(cancel.check().is_err());
        assert!(state.claim(&job).is_err());
        // Background maintenance takes no new step once cleanup has begun.
        assert!(state.background_step().is_none());
        drop(claim);
        assert!(!state.cleanup_drained().unwrap());
        assert!(state.finish_cleanup().is_err());
        drop(worker);
        assert!(!state.cleanup_drained().unwrap());
        drop(step);
        assert!(state.cleanup_drained().unwrap());
        state.finish_cleanup().unwrap();
        assert!(state.claim(&job).is_ok());
        assert!(state.background_step().is_some());
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
        let queued = DurableJob::new(input, 1, identity())
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
        assert!(DurableJob::new(request(), 1, identity()).with_restore_id(id).is_err());
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
        let job = DurableJob::new(request(), 1, invalid);
        db.put(&job).unwrap();
        assert_eq!(db.read(&job.id).err().unwrap().kind, ErrorKind::Corrupt);
    }

    #[test]
    fn renderer_state_keeps_all_pending_and_only_recent_terminal_jobs() {
        let root = tempfile::tempdir().unwrap();
        let db = JobStore::open(root.path()).unwrap();
        let mut terminal_ids = Vec::new();
        for index in 0..40 {
            let mut completed = DurableJob::new(request(), index, identity());
            completed.summary["state"] = json!("succeeded");
            completed.summary["phase"] = json!("complete");
            terminal_ids.push(completed.id.clone());
            db.put(&completed).unwrap();
        }
        let mut first = request();
        first.connection_id = "pending-a".into();
        let first = DurableJob::new(first, 41, identity());
        let mut second = request();
        second.connection_id = "pending-b".into();
        let second = DurableJob::new(second, 42, identity());
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
        let job = DurableJob::new(request(), 1, identity());
        let (cancel, claim) = state.claim(&job).unwrap();
        let retained = claim.clone();
        cancel.cancel();
        assert!(state.claim(&job).is_err());
        let other = DurableJob::new(request(), 2, identity());
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
        let job = DurableJob::new(request(), 1, identity());
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
    fn local_restore_settlement_keeps_exact_job_and_removal_ownership_without_stopping_a_backup() {
        let state = JobCommandState::default();
        let backup = DurableJob::new(request(), 1, identity());
        let (backup_cancel, backup_claim) = state.claim(&backup).unwrap();
        let mut restore = DurableJob::new(request(), 2, identity());
        restore.request.kind = JobKind::Restore;
        let (_, settlement) = state.claim_restore_settlement(&restore).unwrap();
        assert!(backup_cancel.check().is_ok());
        assert!(state.job_is_active(&restore.id).unwrap());
        assert_eq!(state.active.lock().unwrap().values().filter(|(connection, _)| connection == &restore.request.connection_id).count(), 2);
        assert!(state.claim(&restore).is_err());
        assert!(state.claim_restore_settlement(&restore).is_err());
        assert!(state.claim_restore_settlement(&backup).is_err());
        assert!(!state.cleanup_drained().unwrap());
        drop(backup_claim);
        assert!(state.claim(&backup).is_err());
        drop(settlement);
        assert!(!state.job_is_active(&restore.id).unwrap());
        let (_, restarted) = state.claim(&restore).unwrap();
        assert!(state.claim_restore_settlement(&restore).is_err());
        drop(restarted);
        state.begin_cleanup().unwrap();
        assert!(state.claim_restore_settlement(&restore).is_err());
    }

    #[test]
    fn a_claim_is_bound_to_its_job_and_runtime_instance() {
        let state = JobCommandState::default();
        let other_state = JobCommandState::default();
        let job = DurableJob::new(request(), 1, identity());
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
        let mut job = DurableJob::new(request(), 1, identity());
        job.summary["state"] = json!("running");
        let (_, claim) = state.claim(&job).unwrap();
        assert_eq!(state.active.lock().unwrap().len(), 1);
        drop(claim);
        assert!(state.active.lock().unwrap().is_empty());
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
    #[test]
    fn terminal_maintenance_pages_use_indexes_and_skip_unrelated_history() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut wanted = Vec::new();
        for n in 0..260 {
            let mut other = DurableJob::new(request(), n, identity());
            other.summary["state"] = json!("succeeded");
            other.spool_released = true;
            store.put(&other).unwrap();
            let mut restore = other.clone();
            restore.id = uuid::Uuid::new_v4().to_string();
            restore.request.kind = JobKind::Restore;
            restore.request.snapshot_id = Some("snapshot".into());
            restore.request.target_revision = Some("1".into());
            wanted.push(restore.id.clone());
            store.put(&restore).unwrap();
        }
        let mut actual = Vec::new();
        let mut after = 0;
        loop {
            let page = store.finished_restore_page(after).unwrap();
            assert!(page.len() <= MAINTENANCE_PAGE as usize);
            if page.is_empty() { break; }
            for (cursor, job) in page {
                assert!(cursor > after);
                after = cursor;
                actual.push(job.id);
            }
        }
        assert_eq!(actual, wanted);
        for (sql, index) in [
            (FINISHED_RESTORE_PAGE_SQL, "external_requests_finished_kind"),
            (RELEASED_PAGE_SQL, "external_requests_released"),
        ] {
            let details = store.0.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap()
                .query_map(rusqlite::params![0, MAINTENANCE_PAGE], |row| row.get::<_, String>(3))
                .unwrap().map(|row| row.unwrap()).collect::<Vec<_>>();
            assert!(details.iter().any(|step| step.contains(index)), "{details:?}");
            assert!(!details.iter().any(|step| step.contains("USE TEMP B-TREE")), "{details:?}");
        }
    }

    #[test]
    fn terminal_spool_cursor_survives_restart_and_passes_retained_owners() {
        let root = tempfile::tempdir().unwrap();
        let jobs = JobStore::open(root.path()).unwrap();
        let mut ids = Vec::new();
        for n in 0..260 {
            let mut job = DurableJob::new(request(), n, identity());
            job.summary["state"] = json!("failed");
            ids.push(job.id.clone());
            jobs.put(&job).unwrap();
        }
        let page = jobs.terminal_spool_page().unwrap();
        assert_eq!(page.len(), MAINTENANCE_PAGE as usize);
        jobs.advance_terminal_spool_page(&page).unwrap();
        drop(jobs);
        let jobs = JobStore::open(root.path()).unwrap();
        let page = jobs.terminal_spool_page().unwrap();
        assert_eq!(page[0].1.id, ids[128]);
        assert_eq!(page.len(), MAINTENANCE_PAGE as usize);
        for (_, job) in &page { jobs.release_spool(&job.id).unwrap(); }
        jobs.advance_terminal_spool_page(&page).unwrap();
        let page = jobs.terminal_spool_page().unwrap();
        assert_eq!(page.len(), 4);
        jobs.advance_terminal_spool_page(&page).unwrap();
        assert_eq!(jobs.terminal_spool_page().unwrap()[0].1.id, ids[0]);
        let details = jobs.0.prepare(&format!("EXPLAIN QUERY PLAN {TERMINAL_SPOOL_PAGE_SQL}"))
            .unwrap().query_map(rusqlite::params![0, MAINTENANCE_PAGE], |row| row.get::<_, String>(3))
            .unwrap().map(|row| row.unwrap()).collect::<Vec<_>>();
        assert!(details.iter().any(|step| step.contains("external_requests_released")), "{details:?}");
        assert!(!details.iter().any(|step| step.contains("USE TEMP B-TREE")), "{details:?}");
    }

    #[test]
    fn pruning_keeps_recovery_owners_and_recent_history_per_connection() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut protected = Vec::new();
        for _ in 0..3 {
            let mut job = DurableJob::new(request(), 1, identity());
            job.summary["state"] = json!("failed");
            protected.push(job.id.clone());
            store.put(&job).unwrap();
        }
        for connection in ["synthetic", "second"] {
            for n in 0..40 {
                let mut request = request();
                request.connection_id = connection.into();
                let mut job = DurableJob::new(request, n, identity());
                job.summary["state"] = json!("succeeded");
                job.spool_released = true;
                store.put(&job).unwrap();
            }
        }
        store.prune_released().unwrap();
        for id in protected { assert!(store.read(&id).is_ok()); }
        let remaining = store.list().unwrap();
        assert_eq!(remaining.iter().filter(|job| job.request.connection_id == "synthetic").count(), 35);
        assert_eq!(remaining.iter().filter(|job| job.request.connection_id == "second").count(), 32);
        let details = store.0.prepare(
            "EXPLAIN QUERY PLAN SELECT rowid FROM external_requests
             WHERE connection_id=?1
               AND json_extract(value,'$.summary.state') IN ('succeeded','failed','cancelled')
             ORDER BY rowid DESC LIMIT 1 OFFSET ?2",
        ).unwrap().query_map(rusqlite::params!["synthetic", STATE_TERMINAL_LIMIT - 1],
            |row| row.get::<_, String>(3),
        ).unwrap().map(|row| row.unwrap()).collect::<Vec<_>>();
        assert!(details.iter().any(|step| step.contains("external_requests_terminal")), "{details:?}");
        assert!(!details.iter().any(|step| step.contains("USE TEMP B-TREE")), "{details:?}");
    }

    #[test]
    fn released_history_pruning_preserves_live_material_and_recent_retry_ids() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut held = DurableJob::new(request(), 1, identity());
        held.summary["state"] = json!("failed");
        store.put(&held).unwrap();
        let mut latest = String::new();
        for n in 0..1000 {
            let mut job = DurableJob::new(request(), n, identity());
            job.summary["state"] = json!("failed");
            job.spool_released = true;
            latest = job.id.clone();
            store.put(&job).unwrap();
        }
        store.prune_released().unwrap();
        assert_eq!(store.list().unwrap().len(), 1001 - MAINTENANCE_PAGE as usize);
        for _ in 0..8 { store.prune_released().unwrap(); }
        assert_eq!(store.list().unwrap().len(), STATE_TERMINAL_LIMIT as usize + 1);
        assert_eq!(store.list_retaining().unwrap().iter().map(|job| job.id.clone()).collect::<Vec<_>>(), vec![held.id.clone()]);
        assert!(store.read(&latest).unwrap().terminal());
        store.release_spool(&held.id).unwrap();
        store.prune_released().unwrap();
        assert_eq!(store.list().unwrap().len(), STATE_TERMINAL_LIMIT as usize);
        assert!(store.list_retaining().unwrap().is_empty());
    }
    #[test]
    fn publication_unknown_excludes_a_second_pending_operation() {
        let root = tempfile::tempdir().unwrap();
        let store = JobStore::open(root.path()).unwrap();
        let mut unknown = DurableJob::new(request(), 1, identity());
        unknown.summary["state"] = json!("uncertain");
        unknown.summary["phase"] = json!("publication-unknown");
        store.put(&unknown).unwrap();
        let next = DurableJob::new(request(), 2, identity());
        assert_eq!(store.put(&next).unwrap_err().kind, ErrorKind::PreconditionFailed);
        assert!(!store.insert_new(&next).unwrap());
        assert_eq!(store.list_pending().unwrap().len(), 1);
    }
    #[test]
    fn blocking_capture_remains_joined_until_its_owner_releases() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let state = JobCommandState::default();
            let owner = state.track_blocking("capture").unwrap();
            let wait = state.settle_blocking("capture");
            tokio::pin!(wait);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(10), &mut wait).await.is_err());
            drop(owner);
            wait.await.unwrap();
        });
    }

}
