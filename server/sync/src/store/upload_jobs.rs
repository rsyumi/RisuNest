use super::{uploads::now, Device, Store};
use crate::{Error, Result};
use rusqlite::{params, OptionalExtension};

impl Store {
    /// Large finalization is accepted only after its identity and chunk set are
    /// durable. The worker can resume the same upload after daemon restart.
    pub fn submit_upload(&self, device: &Device, id: &str) -> Result<Option<String>> {
        let mut db = self.db()?;
        let (hash, size, state) = Self::upload_row(&db, device, id)?;
        if state == "complete" {
            return Ok(Some(hash));
        }
        if state == "failed" {
            return Err(Error::new("upload-finalization-failed", 409));
        }
        let delta: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM upload_deltas WHERE upload=?1)",
            [id],
            |r| r.get(0),
        )?;
        if delta {
            return Ok(None);
        }
        if size < 64 * 1024 * 1024 {
            drop(db);
            return self.finish_upload(device, id).map(Some);
        }
        let count: i64 = db.query_row(
            "SELECT count(*) FROM upload_chunks WHERE upload=?1",
            [id],
            |r| r.get(0),
        )?;
        if count as u64 != size.div_ceil(super::UPLOAD_CHUNK_BYTES) {
            return Err(Error::new("upload-incomplete", 409));
        }
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO upload_jobs(upload) VALUES(?1) ON CONFLICT DO NOTHING",
            [id],
        )?;
        tx.execute(
            "UPDATE uploads SET state='queued' WHERE id=?1 AND state='open'",
            [id],
        )?;
        tx.commit()?;
        Ok(None)
    }

    pub fn run_pending_upload(&self) -> Result<bool> {
        let _guard = match self.upload_job_gate.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Err(Error::new("upload-worker-unavailable", 503)),
        };
        let job: Option<(String, String)> = self.reader()?.query_row(
            "SELECT u.id,u.device FROM upload_jobs j JOIN uploads u ON u.id=j.upload WHERE j.terminal=0 AND j.retry_after<=?1 ORDER BY j.retry_after,j.rowid LIMIT 1",
            [now()?], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        let Some((id, device)) = job else {
            return Ok(false);
        };
        match self.finish_upload(&Device { id: device }, &id) {
            Ok(_) => {
                self.db()?
                    .execute("DELETE FROM upload_jobs WHERE upload=?1", [&id])?;
            }
            Err(error) => {
                let terminal = error.status < 500;
                let mut db = self.db()?;
                let tx = db.transaction()?;
                tx.execute(
                    "UPDATE upload_jobs SET terminal=?2,error=?3,retry_after=?4 WHERE upload=?1",
                    params![id, terminal, error.code, now()? + 1],
                )?;
                tx.execute(
                    "UPDATE uploads SET state=?2 WHERE id=?1",
                    params![id, if terminal { "failed" } else { "queued" }],
                )?;
                tx.commit()?;
                return Err(error);
            }
        }
        Ok(true)
    }
}
