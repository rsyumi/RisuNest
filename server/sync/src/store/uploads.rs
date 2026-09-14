use super::{
    objects::{check_path, publish, sync_directory},
    random_id, Device, Store,
};
use crate::{Error, Result};
use risunest_sync_wire::{hash, validate_hash, validate_id, Sequence};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const UPLOAD_CHUNK_BYTES: u64 = risunest_sync_wire::transfer::UPLOAD_CHUNK_BYTES as u64;
const MAX_UPLOAD_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub(super) fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::new("clock-unavailable", 503))?
        .as_secs() as i64)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UploadManifest {
    pub hash: String,
    pub size: Sequence,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UploadProgress {
    pub upload_id: String,
    pub manifest: UploadManifest,
    pub chunk_bytes: Sequence,
    pub verified: Vec<Sequence>,
    pub next_after: Option<Sequence>,
    pub complete: bool,
    pub finishing: bool,
    pub failure: Option<String>,
    pub retryable_failure: Option<String>,
}

impl Store {
    pub fn begin_upload(&self, device: &Device, manifest: &UploadManifest) -> Result<String> {
        validate_hash(&manifest.hash)?;
        let size = manifest
            .size
            .as_str()
            .parse::<u64>()
            .ok()
            .filter(|v| *v <= MAX_UPLOAD_BYTES)
            .ok_or(Error::new("object-too-large", 413))?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let (count,bytes):(i64,i64)=db.query_row("SELECT count(*),coalesce(sum(size),0) FROM uploads WHERE device=?1 AND state IN ('open','queued','finalizing') AND expires>?2",params![device.id,now()?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if count >= 16 || bytes as u64 + size > MAX_UPLOAD_BYTES {
            return Err(Error::new("upload-quota", 429));
        }
        let id = random_id()?;
        db.execute(
            "INSERT INTO uploads(id,device,hash,size,expires) VALUES(?1,?2,?3,?4,?5)",
            params![id, device.id, manifest.hash, size as i64, now()? + 86400],
        )?;
        Ok(id)
    }
    pub(super) fn upload_row(
        db: &Connection,
        device: &Device,
        id: &str,
    ) -> Result<(String, u64, String)> {
        validate_id(id)?;
        Self::require_device(db, device)?;
        let (hash, size, expires, state): (String, i64, i64, String) = db
            .query_row(
                "SELECT hash,size,expires,state FROM uploads WHERE id=?1 AND device=?2",
                params![id, device.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or(Error::new("upload-not-found", 404))?;
        if expires <= now()? {
            return Err(Error::new("upload-expired", 410));
        }
        Ok((hash, size as u64, state))
    }
    pub(super) fn chunk_path(&self, id: &str, index: u64) -> Result<PathBuf> {
        validate_id(id)?;
        let path = self
            .root
            .join("staging")
            .join(format!("{id}-{index}.chunk"));
        check_path(&path)?;
        Ok(path)
    }
    pub fn upload_progress(
        &self,
        device: &Device,
        id: &str,
        after: Option<u64>,
    ) -> Result<UploadProgress> {
        let db = self.reader()?;
        let (hash, size, state) = Self::upload_row(&db, device, id)?;
        let after = after
            .map(i64::try_from)
            .transpose()
            .map_err(|_| Error::new("invalid-cursor", 400))?
            .unwrap_or(-1);
        let mut stmt=db.prepare("SELECT ordinal FROM upload_chunks WHERE upload=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 1025")?;
        let mut indexes = stmt
            .query_map(params![id, after], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let more = indexes.len() > 1024;
        indexes.truncate(1024);
        let next_after = if more {
            indexes.last().map(|v| (*v as u64).into())
        } else {
            None
        };
        let job_failure = db
            .query_row(
                "SELECT terminal,error FROM upload_jobs WHERE upload=?1",
                [id],
                |r| Ok((r.get::<_, bool>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(UploadProgress {
            upload_id: id.into(),
            manifest: UploadManifest {
                hash,
                size: size.into(),
            },
            chunk_bytes: UPLOAD_CHUNK_BYTES.into(),
            verified: indexes.into_iter().map(|v| (v as u64).into()).collect(),
            next_after,
            complete: state == "complete",
            finishing: matches!(state.as_str(), "queued" | "finalizing"),
            failure: job_failure
                .as_ref()
                .filter(|(terminal, _)| *terminal)
                .and_then(|(_, error)| error.clone()),
            retryable_failure: job_failure
                .filter(|(terminal, _)| !*terminal)
                .and_then(|(_, error)| error),
        })
    }
    pub fn put_upload_chunk(
        &self,
        device: &Device,
        id: &str,
        index: u64,
        digest: &str,
        bytes: &[u8],
    ) -> Result<()> {
        validate_hash(digest)?;
        let (_, size, state) = Self::upload_row(&*self.reader()?, device, id)?;
        if state != "open" {
            return Err(Error::new("upload-not-open", 409));
        }
        let offset = index
            .checked_mul(UPLOAD_CHUNK_BYTES)
            .filter(|v| *v < size)
            .ok_or(Error::new("invalid-chunk-offset", 400))?;
        if bytes.len() as u64 != (size - offset).min(UPLOAD_CHUNK_BYTES) || hash(bytes) != digest {
            return Err(Error::new("chunk-mismatch", 400));
        }
        let mut temp = tempfile::NamedTempFile::new_in(self.root.join("staging"))?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        // Serialize only durable publication, never request-body IO or hashing.
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let db = self.db()?;
        let (_, _, state) = Self::upload_row(&db, device, id)?;
        if state != "open" {
            return Err(Error::new("upload-not-open", 409));
        }
        let prior: Option<String> = db
            .query_row(
                "SELECT hash FROM upload_chunks WHERE upload=?1 AND ordinal=?2",
                params![id, index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            return if prior == digest {
                Ok(())
            } else {
                Err(Error::new("chunk-intent-conflict", 409))
            };
        }
        publish(temp.path(), &self.chunk_path(id, index)?)?;
        db.execute(
            "INSERT INTO upload_chunks VALUES(?1,?2,?3,?4)",
            params![id, index as i64, digest, bytes.len() as i64],
        )?;
        Ok(())
    }
    pub fn finish_upload(&self, device: &Device, id: &str) -> Result<String> {
        let (digest, size) = {
            let db = self.db()?;
            let (digest, size, state) = Self::upload_row(&db, device, id)?;
            if state == "complete" {
                return Ok(digest);
            }
            if state != "open" && state != "queued" {
                return Err(Error::new("upload-not-open", 409));
            }
            let count: i64 = db.query_row(
                "SELECT count(*) FROM upload_chunks WHERE upload=?1",
                [id],
                |r| r.get(0),
            )?;
            let delta: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM upload_deltas WHERE upload=?1)",
                [id],
                |r| r.get(0),
            )?;
            if !delta && count as u64 != size.div_ceil(UPLOAD_CHUNK_BYTES) {
                return Err(Error::new("upload-incomplete", 409));
            }
            db.execute("UPDATE uploads SET state='finalizing' WHERE id=?1", [id])?;
            (digest, size)
        };
        let result = (|| {
            let mut temp = tempfile::NamedTempFile::new_in(self.root.join("staging"))?;
            let recipe: Option<Vec<u8>> = self
                .reader()?
                .query_row(
                    "SELECT body FROM upload_deltas WHERE upload=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(bytes) = recipe {
                let recipe = risunest_sync_wire::stream_delta::decode(&bytes)?;
                let mut bases = recipe
                    .bases
                    .iter()
                    .map(|b| self.open_object(&b.hash).map(|v| v.0))
                    .collect::<Result<Vec<_>>>()?;
                let mut checked = std::time::Instant::now() - std::time::Duration::from_secs(1);
                risunest_sync_wire::stream_delta::apply(&recipe, &mut bases, &mut temp, || {
                    if checked.elapsed() >= std::time::Duration::from_millis(100) {
                        checked = std::time::Instant::now();
                        let db = self
                            .reader()
                            .map_err(|_| risunest_sync_wire::WireError("delta-cancelled"))?;
                        Self::upload_row(&db, device, id)
                            .map_err(|_| risunest_sync_wire::WireError("delta-cancelled"))?;
                    }
                    Ok(())
                })?;
            } else {
                let mut full = Sha256::new();
                let mut buffer = vec![0u8; 1024 * 1024];
                let mut total = 0u64;
                for index in 0..size.div_ceil(UPLOAD_CHUNK_BYTES) {
                    let (expected, expected_size): (String, i64) = self.reader()?.query_row(
                        "SELECT hash,size FROM upload_chunks WHERE upload=?1 AND ordinal=?2",
                        params![id, index as i64],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    let mut file = File::open(self.chunk_path(id, index)?)?;
                    let mut chunk = Sha256::new();
                    let mut length = 0u64;
                    loop {
                        let n = file.read(&mut buffer)?;
                        if n == 0 {
                            break;
                        }
                        length += n as u64;
                        if length > expected_size as u64 {
                            return Err(Error::new("corrupt-chunk", 503));
                        }
                        chunk.update(&buffer[..n]);
                        full.update(&buffer[..n]);
                        temp.write_all(&buffer[..n])?;
                    }
                    if length != expected_size as u64
                        || format!("{:x}", chunk.finalize()) != expected
                    {
                        return Err(Error::new("corrupt-chunk", 503));
                    }
                    total += length;
                }
                if total != size || format!("{:x}", full.finalize()) != digest {
                    return Err(Error::new("hash-mismatch", 400));
                }
            }
            temp.as_file().sync_all()?;
            let destination = self.object_path(&digest)?;
            fs::create_dir_all(destination.parent().unwrap())?;
            sync_directory(&self.root.join("objects"))?;
            let _gate = self
                .objects_gate
                .lock()
                .map_err(|_| Error::new("storage-unavailable", 503))?;
            let mut db = self.db()?;
            Self::upload_row(&db, device, id)?;
            publish(temp.path(), &destination)?;
            let tx = db.transaction()?;
            tx.execute(
                "INSERT INTO objects VALUES(?1,?2) ON CONFLICT DO NOTHING",
                params![digest, size as i64],
            )?;
            tx.execute("UPDATE uploads SET state='complete' WHERE id=?1", [id])?;
            Self::lease_object(&tx, device, &digest)?;
            tx.commit()?;
            Ok(digest.clone())
        })();
        if result.is_err() {
            self.db()?.execute(
                "UPDATE uploads SET state='open' WHERE id=?1 AND state='finalizing'",
                [id],
            )?;
        }
        result
    }
    pub fn cancel_upload(&self, device: &Device, id: &str) -> Result<()> {
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let mut db = self.db()?;
        let (_, _, state) = Self::upload_row(&db, device, id)?;
        if state == "finalizing" {
            return Err(Error::new("upload-not-open", 409));
        }
        let tx = db.transaction()?;
        tx.execute("INSERT OR IGNORE INTO staging_trash SELECT upload,ordinal FROM upload_chunks WHERE upload=?1",[id])?;
        tx.execute(
            "DELETE FROM uploads WHERE id=?1 AND device=?2",
            params![id, device.id],
        )?;
        tx.commit()?;
        self.drain_staging_trash(&db)?;
        Ok(())
    }
    /// Bounded identity range, also used by resumable downloads of large objects.
    pub fn read_object_range(&self, digest: &str, start: u64, length: u64) -> Result<Vec<u8>> {
        if length > UPLOAD_CHUNK_BYTES {
            return Err(Error::new("range-too-large", 413));
        }
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let size = self
            .object_size(digest)?
            .ok_or(Error::new("object-not-found", 404))?;
        if start.checked_add(length).is_none_or(|v| v > size) {
            return Err(Error::new("invalid-range", 416));
        }
        let mut file = File::open(self.object_path(digest)?)?;
        drop(_gate);
        if file.metadata()?.len() != size {
            return Err(Error::new("corrupt-object", 503));
        }
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}
