use super::{json, parse, random_id, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    change_digest::ChangeDigest, hash, ChangeSet, ReadFence, RecordChange, ScopeFence,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const MAX_STAGED_BYTES: i64 = 256 * 1024 * 1024;
pub const MAX_STAGED_RECORDS: i64 = 500_000;
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagedChanges {
    pub staged_changes_id: String,
    pub changes_digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagingProgress {
    pub next_page: risunest_sync_wire::Sequence,
    pub changes_digest: Option<String>,
}

impl Store {
    pub(super) fn require_staging(db: &Connection, device: &Device, id: &str) -> Result<()> {
        risunest_sync_wire::validate_id(id)?;
        let valid:Option<bool>=db.query_row("SELECT expires>unixepoch() OR EXISTS(SELECT 1 FROM commit_jobs WHERE stage=?1 AND device=?2) FROM staged_changes WHERE id=?1 AND device=?2",params![id,device.id],|r|r.get(0)).optional()?;
        match valid {
            Some(true) => Ok(()),
            Some(false) => Err(Error::new("staging-expired", 410)),
            None => Err(Error::new("staging-not-found", 404)),
        }
    }
    pub fn changes_progress(&self, device: &Device, id: &str) -> Result<StagingProgress> {
        let db = self.reader()?;
        Self::require_device(&db, device)?;
        Self::require_staging(&db, device, id)?;
        let (count, digest): (i64, Option<String>) = db
            .query_row(
                "SELECT page_count,digest FROM staged_changes WHERE id=?1 AND device=?2",
                params![id, device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::new("staging-not-found", 404))?;
        Ok(StagingProgress {
            next_page: (count as u64).into(),
            changes_digest: digest,
        })
    }
    pub fn begin_changes(&self, device: &Device) -> Result<String> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let count: i64 = db.query_row(
            "SELECT count(*) FROM staged_changes WHERE device=?1 AND (expires>unixepoch() OR id IN (SELECT stage FROM commit_jobs))",
            [&device.id],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(Error::new("staging-quota", 429));
        }
        let id = random_id()?;
        db.execute(
            "INSERT INTO staged_changes(id,device) VALUES(?1,?2)",
            params![id, device.id],
        )?;
        Ok(id)
    }
    pub fn stage_changes(&self, device: &Device, changes: &ChangeSet) -> Result<StagedChanges> {
        changes.validate()?;
        let id = self.begin_changes(device)?;
        let result = self
            .put_changes_page(device, &id, 0, changes)
            .and_then(|_| self.seal_changes(device, &id));
        if result.is_err() {
            let _ = self.cancel_staged_changes(device, &id);
        }
        result
    }
    pub fn put_changes_page(
        &self,
        device: &Device,
        id: &str,
        index: u64,
        page: &ChangeSet,
    ) -> Result<()> {
        page.validate_page()?;
        {
            let db = self.reader()?;
            Self::require_device(&db, device)?;
            Self::require_staging(&db, device, id)?;
        }
        let bytes = json(page)?;
        let digest = hash(bytes.as_bytes());
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        self.prepare_descriptors(page)?;
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let tx = db.transaction()?;
        Self::require_staging(&tx, device, id)?;
        let (sealed,count,used,records):(Option<String>,i64,i64,i64)=tx.query_row("SELECT digest,page_count,byte_count,change_count FROM staged_changes WHERE id=?1 AND device=?2",params![id,device.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or(Error::new("staging-not-found",404))?;
        let index = i64::try_from(index).map_err(|_| Error::new("invalid-page-index", 400))?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT hash FROM staged_pages WHERE stage=?1 AND ordinal=?2",
                params![id, index],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            return if prior == digest {
                Ok(())
            } else {
                Err(Error::new("page-intent-conflict", 409))
            };
        }
        if sealed.is_some() {
            return Err(Error::new("staging-sealed", 409));
        }
        if index != count {
            return Err(Error::new("page-gap", 409));
        }
        if used + bytes.len() as i64 > MAX_STAGED_BYTES
            || records + page.changes.len() as i64 > MAX_STAGED_RECORDS
        {
            return Err(Error::new("staging-too-large", 413));
        }
        for (table, domain, key, body) in page
            .changes
            .iter()
            .map(|v| Ok(("staged_records", v.domain, v.key.as_str(), json(v)?)))
            .chain(
                page.read_fences
                    .iter()
                    .map(|v| Ok(("staged_fences", v.domain, v.key.as_str(), json(v)?))),
            )
            .collect::<Result<Vec<_>>>()?
        {
            let prior: Option<(String, String)> = tx
                .query_row(
                    &format!("SELECT domain,key FROM {table} WHERE stage=?1 ORDER BY domain DESC,key DESC LIMIT 1"),
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if prior
                .as_ref()
                .is_some_and(|(d, k)| (d.as_str(), k.as_str()) >= (domain.as_str(), key))
            {
                return Err(Error::new("unordered-keys", 400));
            }
            tx.execute(
                &format!("INSERT INTO {table} VALUES(?1,?2,?3,?4)"),
                params![id, domain.as_str(), key, body],
            )?;
        }
        for fence in &page.scope_fences {
            let prior: Option<String> = tx.query_row(
                "SELECT MAX(scope) FROM staged_scope_fences WHERE stage=?1",
                [id],
                |r| r.get(0),
            )?;
            if prior
                .as_ref()
                .is_some_and(|prior| prior.as_str() >= fence.scope.as_str())
            {
                return Err(Error::new("unordered-keys", 400));
            }
            tx.execute(
                "INSERT INTO staged_scope_fences VALUES(?1,?2,?3)",
                params![id, fence.scope, json(fence)?],
            )?;
        }
        tx.execute(
            "INSERT INTO staged_pages VALUES(?1,?2,?3)",
            params![id, index, digest],
        )?;
        tx.execute("UPDATE staged_changes SET page_count=page_count+1,byte_count=byte_count+?1,change_count=change_count+?2 WHERE id=?3",params![bytes.len() as i64,page.changes.len() as i64,id])?;
        tx.commit()?;
        Ok(())
    }
    pub fn seal_changes(&self, device: &Device, id: &str) -> Result<StagedChanges> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        Self::require_staging(&db, device, id)?;
        let (digest, count): (Option<String>, i64) = db
            .query_row(
                "SELECT digest,change_count FROM staged_changes WHERE id=?1 AND device=?2",
                params![id, device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(Error::new("staging-not-found", 404))?;
        if let Some(changes_digest) = digest {
            return Ok(StagedChanges {
                staged_changes_id: id.into(),
                changes_digest,
            });
        }
        if count == 0 {
            let mut has_clear = false;
            Self::each_scope_fence(&db, id, |fence| {
                has_clear |= fence.clear;
                Ok(())
            })?;
            if !has_clear {
                return Err(Error::new("empty-staged-changes", 400));
            }
        }
        let mut digest = ChangeDigest::new();
        Self::each_change(&db, id, |change| {
            digest.change(&change)?;
            Ok(())
        })?;
        Self::each_fence(&db, id, |fence| {
            digest.read_fence(&fence)?;
            Ok(())
        })?;
        Self::each_scope_fence(&db, id, |fence| {
            digest.scope_fence(&fence)?;
            Ok(())
        })?;
        let changes_digest = digest.finish()?;
        db.execute(
            "UPDATE staged_changes SET digest=?1 WHERE id=?2",
            params![changes_digest, id],
        )?;
        Ok(StagedChanges {
            staged_changes_id: id.into(),
            changes_digest,
        })
    }
    pub fn cancel_staged_changes(&self, device: &Device, id: &str) -> Result<()> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let active: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM commit_jobs WHERE stage=?1 AND device=?2)",
            params![id, device.id],
            |r| r.get(0),
        )?;
        if active {
            return Err(Error::new("device-operation-active", 409));
        }
        if db.execute(
            "DELETE FROM staged_changes WHERE id=?1 AND device=?2",
            params![id, device.id],
        )? == 0
        {
            return Err(Error::new("staging-not-found", 404));
        }
        Ok(())
    }
    pub(super) fn each_change(
        db: &Connection,
        id: &str,
        mut visit: impl FnMut(RecordChange) -> Result<()>,
    ) -> Result<()> {
        let mut statement =
            db.prepare("SELECT body FROM staged_records WHERE stage=?1 ORDER BY domain,key")?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            visit(parse(&row.get::<_, String>(0)?)?)?;
        }
        Ok(())
    }
    pub(super) fn each_fence(
        db: &Connection,
        id: &str,
        mut visit: impl FnMut(ReadFence) -> Result<()>,
    ) -> Result<()> {
        let mut statement =
            db.prepare("SELECT body FROM staged_fences WHERE stage=?1 ORDER BY domain,key")?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            visit(parse(&row.get::<_, String>(0)?)?)?;
        }
        Ok(())
    }
    pub(super) fn each_scope_fence(
        db: &Connection,
        id: &str,
        mut visit: impl FnMut(ScopeFence) -> Result<()>,
    ) -> Result<()> {
        let mut statement =
            db.prepare("SELECT body FROM staged_scope_fences WHERE stage=?1 ORDER BY scope")?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            visit(parse(&row.get::<_, String>(0)?)?)?;
        }
        Ok(())
    }
}
