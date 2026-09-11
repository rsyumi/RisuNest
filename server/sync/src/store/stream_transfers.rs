use super::{json, uploads::now, Device, Store, TransferRequest};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical, delta::Base, hash, stream_delta, validate_hash, validate_id, WireError,
};
use rusqlite::{params, OptionalExtension};
use std::time::{Duration, Instant};

pub enum DeltaProgress {
    Pending,
    FullRequired,
    Ready(Vec<u8>),
}

struct DownloadRow {
    state: String,
    body: Option<Vec<u8>>,
    error: Option<String>,
    expires: i64,
}

impl Store {
    /// Attaching a recipe is a durable, idempotent transition on the client's
    /// existing upload identity. Neither request processing nor retry restores it.
    pub fn attach_upload_delta(&self, device: &Device, id: &str, bytes: &[u8]) -> Result<()> {
        let recipe = stream_delta::decode(bytes)?;
        let mut db = self.db()?;
        let (digest, size, state) = Self::upload_row(&db, device, id)?;
        if digest != recipe.target_hash || size != recipe.target_size {
            return Err(Error::new("upload-manifest-mismatch", 409));
        }
        let prior: Option<Vec<u8>> = db
            .query_row(
                "SELECT body FROM upload_deltas WHERE upload=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            return if prior == bytes {
                Ok(())
            } else {
                Err(Error::new("upload-intent-conflict", 409))
            };
        }
        if state != "open" {
            return Err(Error::new("upload-not-open", 409));
        }
        let chunks: i64 = db.query_row(
            "SELECT count(*) FROM upload_chunks WHERE upload=?1",
            [id],
            |r| r.get(0),
        )?;
        if chunks != 0 {
            return Err(Error::new("upload-intent-conflict", 409));
        }
        let total: i64 = db.query_row(
            "SELECT coalesce(sum(length(body)),0) FROM upload_deltas",
            [],
            |r| r.get(0),
        )?;
        if total + bytes.len() as i64 > 64 * 1024 * 1024 {
            return Err(Error::new("delta-cache-quota", 429));
        }
        let tx = db.transaction()?;
        for base in &recipe.bases {
            let size: Option<i64> = tx
                .query_row(
                    "SELECT size FROM objects WHERE hash=?1",
                    [&base.hash],
                    |r| r.get(0),
                )
                .optional()?;
            if size != Some(base.size as i64) {
                return Err(Error::new("delta-base-missing", 404));
            }
            tx.execute(
                "INSERT INTO upload_delta_bases VALUES(?1,?2)",
                params![id, base.hash],
            )?;
        }
        tx.execute(
            "INSERT INTO upload_deltas VALUES(?1,?2)",
            params![id, bytes],
        )?;
        tx.execute("INSERT INTO upload_jobs(upload) VALUES(?1)", [id])?;
        tx.execute("UPDATE uploads SET state='queued' WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn begin_download_delta(
        &self,
        device: &Device,
        request: &TransferRequest,
    ) -> Result<String> {
        validate_hash(&request.target)?;
        if request.bases.len() > risunest_sync_wire::delta::MAX_BASES {
            return Err(Error::new("too-many-bases", 400));
        }
        let mut seen = std::collections::BTreeSet::new();
        for base in &request.bases {
            validate_hash(base)?;
            if !seen.insert(base) {
                return Err(Error::new("duplicate-base", 400));
            }
        }
        let body = json(request)?;
        let id = hash(&canonical::encode(&[&device.id, &body])?);
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let existing: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM download_deltas WHERE id=?1 AND expires>?2 AND error IS NULL)",
            params![id, now()?],
            |r| r.get(0),
        )?;
        if existing {
            return Ok(id);
        }
        db.execute(
            "DELETE FROM download_deltas WHERE id=?1 AND state!='working'",
            [&id],
        )?;
        let count: i64 = db.query_row(
            "SELECT count(*) FROM download_deltas WHERE device=?1 AND expires>?2",
            params![device.id, now()?],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(Error::new("delta-job-quota", 429));
        }
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO download_deltas(id,device,request,expires) VALUES(?1,?2,?3,?4)",
            params![id, device.id, body, now()? + 3600],
        )?;
        let mut total = 0u64;
        for digest in std::iter::once(&request.target).chain(request.bases.iter()) {
            let size: Option<i64> = tx
                .query_row("SELECT size FROM objects WHERE hash=?1", [digest], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(size) = size {
                if size < 0 || size as u64 > stream_delta::MAX_FILE_BYTES {
                    return Err(Error::new("object-too-large", 413));
                }
                if digest != &request.target {
                    total += size as u64;
                }
                if total > stream_delta::MAX_FILE_BYTES {
                    return Err(Error::new("delta-base-limit", 413));
                }
                tx.execute(
                    "INSERT OR IGNORE INTO download_delta_bases VALUES(?1,?2)",
                    params![id, digest],
                )?;
            } else if digest == &request.target {
                return Err(Error::new("object-not-found", 404));
            }
        }
        tx.commit()?;
        Ok(id)
    }
    pub fn download_delta_progress(&self, device: &Device, id: &str) -> Result<DeltaProgress> {
        validate_id(id)?;
        let db = self.reader()?;
        Self::require_device(&db, device)?;
        let row: Option<DownloadRow> = db
            .query_row(
                "SELECT state,body,error,expires FROM download_deltas WHERE id=?1 AND device=?2",
                params![id, device.id],
                |r| {
                    Ok(DownloadRow {
                        state: r.get(0)?,
                        body: r.get(1)?,
                        error: r.get(2)?,
                        expires: r.get(3)?,
                    })
                },
            )
            .optional()?;
        let Some(DownloadRow {
            state,
            body,
            error,
            expires,
        }) = row
        else {
            return Err(Error::new("delta-job-not-found", 404));
        };
        if expires <= now()? {
            return Err(Error::new("delta-job-expired", 410));
        }
        if error.is_some() {
            return Err(Error::new("delta-generation-failed", 409));
        }
        Ok(match state.as_str() {
            "complete" => body
                .map(DeltaProgress::Ready)
                .unwrap_or(DeltaProgress::FullRequired),
            _ => DeltaProgress::Pending,
        })
    }
    pub fn release_download_delta(&self, device: &Device, id: &str) -> Result<()> {
        validate_id(id)?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        db.execute(
            "DELETE FROM download_deltas WHERE id=?1 AND device=?2",
            params![id, device.id],
        )?;
        Ok(())
    }
    pub fn run_pending_download_delta(&self) -> Result<bool> {
        let _guard = match self.download_job_gate.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(_) => return Err(Error::new("delta-worker-unavailable", 503)),
        };
        let job: Option<(String,String,String)> = self.reader()?.query_row("SELECT id,device,request FROM download_deltas WHERE state='queued' ORDER BY rowid LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((id, device, body)) = job else {
            return Ok(false);
        };
        self.db()?.execute(
            "UPDATE download_deltas SET state='working' WHERE id=?1",
            [&id],
        )?;
        let result = (|| -> Result<Option<Vec<u8>>> {
            let request: TransferRequest =
                canonical::decode(body.as_bytes(), risunest_sync_wire::MAX_METADATA_BYTES)?;
            let mut target = self.open_object(&request.target)?.0;
            let identity = Base {
                hash: request.target,
                size: target.metadata()?.len(),
            };
            let mut bases = Vec::new();
            let mut identities = Vec::new();
            for digest in request.bases {
                match self.open_object(&digest) {
                    Ok((file, size)) => {
                        bases.push(file);
                        identities.push(Base { hash: digest, size });
                    }
                    Err(e) if e.status == 404 => (),
                    Err(e) => return Err(e),
                }
            }
            if bases.is_empty() {
                return Ok(None);
            }
            let started = Instant::now();
            let mut checked = Instant::now() - Duration::from_secs(1);
            let recipe =
                stream_delta::create(&mut bases, &identities, &mut target, identity, || {
                    // Includes hashing and repeated-content comparisons, not just IO.
                    if started.elapsed() > Duration::from_secs(120) {
                        return Err(WireError("delta-budget"));
                    }
                    if checked.elapsed() >= Duration::from_millis(100) {
                        checked = Instant::now();
                        self.download_delta_progress(&Device { id: device.clone() }, &id)
                            .map_err(|_| WireError("delta-cancelled"))?;
                    }
                    Ok(())
                });
            match recipe {
                Ok(recipe) => Ok(Some(stream_delta::encode(&recipe)?)),
                Err(WireError("delta-limit" | "delta-budget")) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })();
        let mut db = self.db()?;
        let tx = db.transaction()?;
        match result {
            Ok(body) => {
                let total: i64 = tx.query_row(
                    "SELECT coalesce(sum(length(body)),0) FROM download_deltas",
                    [],
                    |r| r.get(0),
                )?;
                if total + body.as_ref().map_or(0, |b| b.len() as i64) > 64 * 1024 * 1024 {
                    tx.execute("UPDATE download_deltas SET state='complete',error='delta-cache-quota' WHERE id=?1",[&id])?;
                } else {
                    tx.execute(
                        "UPDATE download_deltas SET state='complete',body=?2 WHERE id=?1",
                        params![id, body],
                    )?;
                }
            }
            Err(e) => {
                tx.execute(
                    "UPDATE download_deltas SET state='complete',error=?2 WHERE id=?1",
                    params![id, e.code],
                )?;
            }
        }
        tx.commit()?;
        Ok(true)
    }
}
