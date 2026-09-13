use super::{json, random_id, uploads::now, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{canonical, hash, Sequence};
use rusqlite::{params, Connection};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceResult {
    pub min_retained_seq: Sequence,
    pub objects_removed: u64,
}
impl Store {
    pub(super) fn drain_staging_trash(&self, db: &Connection) -> Result<()> {
        let rows = {
            let mut statement =
                db.prepare("SELECT upload,ordinal FROM staging_trash LIMIT 1024")?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        for (upload, ordinal) in rows {
            let index =
                u64::try_from(ordinal).map_err(|_| Error::new("corrupt-staging-trash", 503))?;
            match std::fs::remove_file(self.chunk_path(&upload, index)?) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => continue,
            }
            super::objects::sync_directory(&self.root.join("staging"))?;
            db.execute(
                "DELETE FROM staging_trash WHERE upload=?1 AND ordinal=?2",
                params![upload, ordinal],
            )?;
        }
        Ok(())
    }
    pub(super) fn lease_object(db: &Connection, device: &Device, digest: &str) -> Result<()> {
        db.execute("INSERT INTO object_leases VALUES(?1,?2,?3) ON CONFLICT(device,hash) DO UPDATE SET expires=excluded.expires",params![device.id,digest,now()?+86400])?;
        Ok(())
    }
    pub fn pin_objects(&self, device: &Device, hashes: &[String]) -> Result<()> {
        if hashes.len() > 1024 {
            return Err(Error::new("too-many-candidates", 400));
        }
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        for digest in hashes {
            risunest_sync_wire::validate_hash(digest)?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
                [digest],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(Error::new("object-not-found", 404));
            }
            Self::lease_object(&tx, device, digest)?;
        }
        tx.commit()?;
        Ok(())
    }
    /// Explicit maintenance uses every active device acknowledgement and read pin.
    /// Offline devices are never silently forgotten. Tombstones stay as identity
    /// fences; payloads and acknowledged journal bodies can be reclaimed.
    pub fn maintain(&self) -> Result<MaintenanceResult> {
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let mut db = self.db()?;
        let tx = db.transaction()?;
        let current = now()?;
        tx.execute("DELETE FROM transfer_recipes WHERE expires<=?1", [current])?;
        tx.execute("DELETE FROM download_deltas WHERE state!='working' AND (expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1))", [current])?;
        tx.execute("DELETE FROM upload_deltas WHERE upload IN (SELECT id FROM uploads WHERE state='complete')", [])?;
        tx.execute("DELETE FROM upload_delta_bases WHERE upload IN (SELECT id FROM uploads WHERE state='complete')", [])?;
        tx.execute("INSERT OR IGNORE INTO staging_trash SELECT c.upload,c.ordinal FROM upload_chunks c JOIN uploads u ON c.upload=u.id WHERE u.state!='finalizing' AND (u.state='complete' OR u.expires<=?1 OR u.device IN (SELECT id FROM devices WHERE revoked=1))",[current])?;
        tx.execute("DELETE FROM upload_chunks WHERE (upload,ordinal) IN (SELECT upload,ordinal FROM staging_trash)",[])?;
        tx.execute("DELETE FROM uploads WHERE state!='finalizing' AND (expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1))",[current])?;
        tx.execute("DELETE FROM read_pins WHERE expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1)",[current])?;
        tx.execute("DELETE FROM checkpoints WHERE expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1)",[current])?;
        tx.execute("DELETE FROM object_leases WHERE expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1)",[current])?;
        tx.execute("DELETE FROM staged_changes WHERE (expires<=?1 OR device IN (SELECT id FROM devices WHERE revoked=1)) AND id NOT IN (SELECT stage FROM commit_jobs)",[current])?;
        let mut head = Self::read_head(&tx)?;
        let mut floor = head.seq.clone();
        {
            let mut stmt = tx.prepare(
                "SELECT ack FROM devices WHERE revoked=0 UNION ALL SELECT after_seq FROM read_pins",
            )?;
            for value in stmt.query_map([], |r| r.get::<_, String>(0))? {
                floor = floor.min(Sequence::try_from(value?)?);
            }
        }
        floor = floor.max(head.min_retained_seq.clone());
        tx.execute(
            "DELETE FROM changes WHERE (length(seq),seq)<=(?1,?2)",
            params![floor.as_str().len() as i64, floor.as_str()],
        )?;
        // Receipts are pruned only once their resulting head is acknowledged.
        tx.execute("DELETE FROM receipts WHERE created<=unixepoch()-86400 AND (length(json_extract(body,'$.head.seq')),json_extract(body,'$.head.seq'))<=(?1,?2)",params![floor.as_str().len() as i64,floor.as_str()])?;
        tx.execute(
            "DELETE FROM commits WHERE (length(seq),seq)<(?1,?2)",
            params![floor.as_str().len() as i64, floor.as_str()],
        )?;
        head.min_retained_seq = floor.clone();
        tx.execute("UPDATE library SET head=?1", [json(&head)?])?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS gc_roots(hash TEXT PRIMARY KEY); DELETE FROM gc_roots;
            INSERT OR IGNORE INTO gc_roots SELECT hash FROM object_leases;
            INSERT OR IGNORE INTO gc_roots SELECT hash FROM object_custody;
            INSERT OR IGNORE INTO gc_roots SELECT hash FROM uploads WHERE expires>unixepoch();
            INSERT OR IGNORE INTO gc_roots SELECT hash FROM upload_delta_bases;
            INSERT OR IGNORE INTO gc_roots SELECT hash FROM download_delta_bases;
            CREATE TEMP TABLE IF NOT EXISTS gc_versions(body TEXT); DELETE FROM gc_versions;
            INSERT INTO gc_versions SELECT version FROM records UNION ALL SELECT version FROM checkpoint_records;
            INSERT INTO gc_versions SELECT json_extract(body,'$.before') FROM changes UNION ALL SELECT json_extract(body,'$.after') FROM changes;
            INSERT INTO gc_versions SELECT json_extract(body,'$.before') FROM staged_records UNION ALL SELECT json_extract(body,'$.after') FROM staged_records UNION ALL SELECT json_extract(body,'$.version') FROM staged_fences;
            INSERT OR IGNORE INTO gc_roots SELECT json_extract(body,'$.objectHash') FROM gc_versions WHERE json_extract(body,'$.objectHash') IS NOT NULL;
            INSERT OR IGNORE INTO gc_roots SELECT json_extract(body,'$.descriptorHash') FROM gc_versions WHERE json_extract(body,'$.descriptorHash') IS NOT NULL;
            CREATE TEMP TABLE IF NOT EXISTS gc_alive(hash TEXT PRIMARY KEY); DELETE FROM gc_alive;
            WITH RECURSIVE edges(source,target) AS (
                SELECT hash,object FROM descriptors UNION ALL SELECT hash,json_extract(body,'$.dependencyRoot') FROM descriptors UNION ALL SELECT hash,json_extract(body,'$.relationRoot') FROM descriptors
                UNION ALL SELECT root,child FROM reference_children UNION ALL SELECT root,object FROM reference_objects
            ), alive(hash) AS (SELECT hash FROM gc_roots UNION SELECT target FROM edges JOIN alive ON source=alive.hash WHERE target IS NOT NULL)
            INSERT INTO gc_alive SELECT hash FROM alive;
            DELETE FROM descriptors WHERE hash NOT IN gc_alive;
            DELETE FROM reference_children WHERE root NOT IN gc_alive;
            DELETE FROM reference_objects WHERE root NOT IN gc_alive;
            DELETE FROM reference_relations WHERE root NOT IN gc_alive;
            DELETE FROM reference_nodes WHERE hash NOT IN gc_alive;
            INSERT OR IGNORE INTO object_trash SELECT hash FROM objects WHERE hash NOT IN gc_alive;
            DELETE FROM objects WHERE hash NOT IN gc_alive;
            DELETE FROM gc_versions; DELETE FROM gc_roots; DELETE FROM gc_alive;")?;
        tx.commit()?;
        self.drain_staging_trash(&db)?;
        let hashes = {
            let mut statement = db.prepare("SELECT hash FROM object_trash LIMIT 1024")?;
            let values = statement
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            values
        };
        let mut removed = 0;
        for digest in hashes {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
                [&digest],
                |r| r.get(0),
            )?;
            if !exists {
                match std::fs::remove_file(self.object_path(&digest)?) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(_) => continue,
                }
            }
            db.execute("DELETE FROM object_trash WHERE hash=?1", [&digest])?;
        }
        Ok(MaintenanceResult {
            min_retained_seq: floor,
            objects_removed: removed,
        })
    }
    /// Call only after restoring a stopped, complete server-directory backup.
    /// Old clients must reconcile against the restored checkpoint under a new epoch.
    pub fn rotate_restored_epoch(&self) -> Result<()> {
        let mut db = self.db()?;
        let tx = db.transaction()?;
        let mut head = Self::read_head(&tx)?;
        head.epoch = random_id()?;
        head.seq = 0.into();
        head.min_retained_seq = 0.into();
        head.head_id = hash(&canonical::encode(&[
            "risunest-sync-genesis-v1",
            &head.library_id,
            &head.epoch,
        ])?);
        tx.execute_batch("DELETE FROM changes; DELETE FROM commits; DELETE FROM receipts; DELETE FROM commit_jobs; DELETE FROM staged_changes; DELETE FROM read_pins; DELETE FROM checkpoints; DELETE FROM uploads; DELETE FROM download_deltas; DELETE FROM transfer_recipes; DELETE FROM object_leases; DELETE FROM scope_versions; UPDATE devices SET ack='0';")?;
        tx.execute("UPDATE library SET head=?1", [json(&head)?])?;
        tx.commit()?;
        Ok(())
    }
}
