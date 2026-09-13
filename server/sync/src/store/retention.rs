//! Durable device custody for remotely resident payloads and local snapshots.
//! Unlike transfer leases, custody does not expire when a device goes offline
//! or is revoked. Only an explicit release with the current custody ID removes it.
use super::{random_id, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{validate_hash, Sequence};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

const PAGE_SIZE: usize = 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedObject {
    pub hash: String,
    pub size: Sequence,
    pub retention_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectIdentity {
    pub hash: String,
    /// Owner manifests identify their payloads by hash without a size. When
    /// supplied by an alias, the expected size must match the verified object.
    pub size: Option<Sequence>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetentionRelease {
    pub device_id: String,
    pub hash: String,
    pub retention_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionPage {
    pub objects: Vec<RetainedObject>,
    pub next_after: Option<String>,
}

impl Store {
    pub fn retain_objects(
        &self,
        device: &Device,
        epoch: &str,
        objects: &[ObjectIdentity],
    ) -> Result<Vec<RetainedObject>> {
        if objects.len() > PAGE_SIZE {
            return Err(Error::new("too-many-objects", 400));
        }
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        if Self::read_head(&tx)?.epoch != epoch {
            return Err(Error::new("epoch-mismatch", 409));
        }
        let mut retained = Vec::with_capacity(objects.len());
        let mut seen = std::collections::BTreeSet::new();
        for object in objects {
            validate_hash(&object.hash)?;
            if !seen.insert(&object.hash) {
                return Err(Error::new("duplicate-object", 400));
            }
            let size: Option<i64> = tx
                .query_row(
                    "SELECT size FROM objects WHERE hash=?1",
                    [&object.hash],
                    |r| r.get(0),
                )
                .optional()?;
            let size = size.ok_or(Error::new("object-not-found", 404))?;
            if size < 0
                || object
                    .size
                    .as_ref()
                    .is_some_and(|expected| *expected != Sequence::from(size as u64))
            {
                return Err(Error::new("object-size-mismatch", 409));
            }
            let metadata = std::fs::metadata(self.object_path(&object.hash)?)?;
            if !metadata.is_file() || metadata.len() != size as u64 {
                return Err(Error::new("corrupt-object", 503));
            }
            let retention_id = random_id()?;
            tx.execute("INSERT INTO object_custody(device,hash,retention_id) VALUES(?1,?2,?3) ON CONFLICT(device,hash) DO UPDATE SET retention_id=excluded.retention_id",params![device.id,object.hash,retention_id])?;
            retained.push(RetainedObject {
                hash: object.hash.clone(),
                size: Sequence::from(size as u64),
                retention_id,
            });
        }
        tx.commit()?;
        Ok(retained)
    }

    pub fn retained_objects(
        &self,
        device: &Device,
        epoch: &str,
        after: Option<&str>,
    ) -> Result<RetentionPage> {
        if let Some(after) = after {
            validate_hash(after)?;
        }
        let db = self.db()?;
        Self::require_device(&db, device)?;
        if Self::read_head(&db)?.epoch != epoch {
            return Err(Error::new("epoch-mismatch", 409));
        }
        let mut statement = db.prepare("SELECT c.hash,o.size,c.retention_id FROM object_custody c JOIN objects o ON o.hash=c.hash WHERE device=?1 AND c.hash>?2 ORDER BY c.hash LIMIT 1025")?;
        let mut objects = statement
            .query_map(params![device.id, after.unwrap_or("")], |r| {
                let size: i64 = r.get(1)?;
                Ok(RetainedObject {
                    hash: r.get(0)?,
                    size: (size as u64).into(),
                    retention_id: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let next_after = (objects.len() > PAGE_SIZE).then(|| objects[PAGE_SIZE - 1].hash.clone());
        objects.truncate(PAGE_SIZE);
        Ok(RetentionPage {
            objects,
            next_after,
        })
    }

    pub fn release_retained_objects(
        &self,
        device: &Device,
        epoch: &str,
        objects: &[RetentionRelease],
    ) -> Result<()> {
        if objects.len() > PAGE_SIZE {
            return Err(Error::new("too-many-objects", 400));
        }
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        if Self::read_head(&tx)?.epoch != epoch {
            return Err(Error::new("epoch-mismatch", 409));
        }
        for object in objects {
            validate_hash(&object.hash)?;
            validate_hash(&object.retention_id)?;
            // A replacement registration can release its revoked predecessor's
            // custody, but must still possess the exact local retention ID.
            if object.device_id != device.id {
                let revoked: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1 AND revoked=1)",
                    [&object.device_id],
                    |r| r.get(0),
                )?;
                if !revoked {
                    return Err(Error::new("retention-device-active", 409));
                }
            }
            // A delayed release cannot remove a newer retention acquired by a
            // concurrent reader or a newly activated snapshot.
            tx.execute(
                "DELETE FROM object_custody WHERE device=?1 AND hash=?2 AND retention_id=?3",
                params![object.device_id, object.hash, object.retention_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
