use super::{parse, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical,
    descriptor::{validate_scope, RecordDescriptor},
    hash, RecordVersion,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeSet;

impl Store {
    pub fn scope_state(&self, scope: &str) -> Result<(String, String)> {
        let (_, version, clear) = self.scope_snapshot(scope)?;
        Ok((version, clear))
    }
    pub fn scope_snapshot(
        &self,
        scope: &str,
    ) -> Result<(risunest_sync_wire::RemoteHead, String, String)> {
        let mut connection = self.reader()?;
        let db = connection.transaction()?;
        let version = Self::read_scope_version(&db, scope)?;
        let clear: Option<String> = db
            .query_row(
                "SELECT version FROM scope_clears WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )
            .optional()?;
        let head = Self::read_head(&db)?;
        Ok((
            head.clone(),
            version,
            clear.unwrap_or(hash(&canonical::encode(&[
                "risunest-sync-clear-v1",
                &head.library_id,
                &head.epoch,
                scope,
            ])?)),
        ))
    }
    pub fn scope_version(&self, scope: &str) -> Result<String> {
        Self::read_scope_version(&*self.reader()?, scope)
    }
    fn read_scope_version(db: &Connection, scope: &str) -> Result<String> {
        validate_scope(scope)?;
        let version: Option<String> = db
            .query_row(
                "SELECT version FROM scope_versions WHERE scope=?1",
                [scope],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(version) = version {
            return Ok(version);
        }
        let head = Self::read_head(db)?;
        Ok(hash(&canonical::encode(&[
            "risunest-sync-scope-v1",
            &head.library_id,
            &head.epoch,
            scope,
        ])?))
    }
    pub(super) fn validate_scope_fences(db: &Connection, stage: &str) -> Result<()> {
        Self::each_scope_fence(db, stage, |fence| {
            if Self::read_scope_version(db, &fence.scope)? != fence.expected_version {
                return Err(Error::new("scope-fence-mismatch", 409));
            }
            if fence.clear {
                let incomplete: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM record_scopes WHERE scope=?1 AND NOT EXISTS(SELECT 1 FROM staged_records WHERE stage=?2 AND staged_records.domain=record_scopes.domain AND staged_records.key=record_scopes.key) AND NOT EXISTS(SELECT 1 FROM staged_fences WHERE stage=?2 AND staged_fences.domain=record_scopes.domain AND staged_fences.key=record_scopes.key))",params![fence.scope,stage],|r|r.get(0))?;
                if incomplete {
                    return Err(Error::new("incomplete-scope-clear", 409));
                }
            }
            Ok(())
        })
    }
    pub(super) fn update_scopes(db: &Connection, stage: &str, operation: &str) -> Result<()> {
        let mut touched = BTreeSet::new();
        Self::each_scope_fence(db, stage, |fence| {
            if fence.clear {
                db.execute("INSERT INTO scope_clears VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET version=excluded.version",params![fence.scope,operation])?;
                touched.insert(fence.scope);
            }
            Ok(())
        })?;
        Self::each_change(db, stage, |change| {
            let mut statement =
                db.prepare("SELECT scope FROM record_scopes WHERE domain=?1 AND key=?2")?;
            for scope in statement.query_map(params![change.domain.as_str(), change.key], |r| {
                r.get::<_, String>(0)
            })? {
                touched.insert(scope?);
            }
            db.execute(
                "DELETE FROM record_scopes WHERE domain=?1 AND key=?2",
                params![change.domain.as_str(), change.key],
            )?;
            if let RecordVersion::Live {
                descriptor_hash: Some(digest),
                ..
            } = &change.after
            {
                let body: String = db.query_row(
                    "SELECT body FROM descriptors WHERE hash=?1",
                    [digest],
                    |r| r.get(0),
                )?;
                let descriptor: RecordDescriptor = parse(&body)?;
                for scope in descriptor.scopes {
                    db.execute(
                        "INSERT INTO record_scopes VALUES(?1,?2,?3)",
                        params![change.domain.as_str(), change.key, scope],
                    )?;
                    touched.insert(scope);
                }
            }
            Ok(())
        })?;
        for scope in touched {
            let prior = Self::read_scope_version(db, &scope)?;
            let version = hash(&canonical::encode(&[
                "risunest-sync-scope-change-v1",
                &scope,
                &prior,
                operation,
            ])?);
            db.execute("INSERT INTO scope_versions VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET version=excluded.version",params![scope,version])?;
        }
        Ok(())
    }
}
