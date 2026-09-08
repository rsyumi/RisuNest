use super::{json, parse, random_id, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical, hash, operation_id, ChangeSet, CommitIntent, Receipt, RecordVersion, RemoteHead,
    Sequence, TerminalStatus,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagedChanges {
    pub staged_changes_id: String,
    pub changes_digest: String,
}

impl Store {
    pub fn stage_changes(&self, device: &Device, changes: &ChangeSet) -> Result<StagedChanges> {
        changes.validate()?;
        let digest = changes.digest()?;
        let body = json(changes)?;
        let id = random_id()?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let count: i64 = db.query_row(
            "SELECT count(*) FROM staged_changes WHERE device=?1",
            [&device.id],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(Error::new("staging-quota", 429));
        }
        db.execute(
            "INSERT INTO staged_changes VALUES(?1,?2,?3,?4)",
            params![id, device.id, digest, body],
        )?;
        Ok(StagedChanges {
            staged_changes_id: id,
            changes_digest: digest,
        })
    }
    pub fn cancel_staged_changes(&self, device: &Device, id: &str) -> Result<()> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        if db.execute(
            "DELETE FROM staged_changes WHERE id=?1 AND device=?2",
            params![id, device.id],
        )? == 0
        {
            return Err(Error::new("staging-not-found", 404));
        }
        Ok(())
    }
    fn read_version(db: &Connection, key: &str) -> Result<RecordVersion> {
        let value: Option<String> = db
            .query_row("SELECT version FROM records WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|v| parse(&v))
            .unwrap_or(Ok(RecordVersion::Absent))
    }
    pub fn record(&self, key: &str) -> Result<RecordVersion> {
        Self::read_version(&*self.db()?, key)
    }

    /// The mutex is the single library writer queue. There is no network/file IO in
    /// this transaction and no application payload parsing. Failed/stale results
    /// consume the operation sequence and are as idempotent as successful commits.
    pub fn commit(
        &self,
        device: &Device,
        intent: &CommitIntent,
        if_match: &str,
    ) -> Result<Receipt> {
        intent.validate()?;
        if if_match != intent.expected_head.etag() {
            return Err(Error::new("if-match-intent-mismatch", 400));
        }
        let digest = intent.digest()?;
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let head = Self::read_head(&db)?;
        if intent.expected_head.library_id != head.library_id {
            return Err(Error::new("library-mismatch", 403));
        }
        let operation = operation_id(&head.library_id, &device.id, &intent.device_operation_seq)?;
        let old: Option<(String, String)> = db
            .query_row(
                "SELECT digest,body FROM receipts WHERE operation=?1 AND device=?2",
                params![operation, device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old_digest, body)) = old {
            if old_digest != digest {
                return Err(Error::new("operation-intent-conflict", 409));
            }
            return parse(&body);
        }
        let watermark: String = db.query_row(
            "SELECT watermark FROM devices WHERE id=?1",
            [&device.id],
            |r| r.get(0),
        )?;
        let watermark: Sequence = watermark.try_into()?;
        if intent.device_operation_seq <= watermark {
            return Err(Error::new("operation-history-expired", 410));
        }
        let tx = db.transaction()?;
        let staged: Option<(String, String)> = tx
            .query_row(
                "SELECT digest,body FROM staged_changes WHERE id=?1 AND device=?2",
                params![intent.staged_changes_id, device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut receipt = Receipt {
            operation_id: operation.clone(),
            device_operation_seq: intent.device_operation_seq.clone(),
            intent_digest: digest.clone(),
            status: TerminalStatus::Failed,
            head: head.clone(),
            error: None,
        };
        let validation = (|| -> Result<ChangeSet> {
            if intent.expected_head != head {
                return Err(Error::new("stale-head", 412));
            }
            let (stored_digest, body) = staged
                .as_ref()
                .ok_or(Error::new("staging-not-found", 404))?;
            if stored_digest != &intent.changes_digest {
                return Err(Error::new("changes-digest-mismatch", 409));
            }
            let changes: ChangeSet = parse(body)?;
            for change in &changes.changes {
                if Self::read_version(&tx, &change.key)? != change.before {
                    return Err(Error::new("before-version-mismatch", 409));
                }
                for digest in change.after.object_hashes() {
                    let present: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
                        [digest],
                        |r| r.get(0),
                    )?;
                    if !present {
                        return Err(Error::new("missing-dependency", 409));
                    }
                }
            }
            for fence in &changes.read_fences {
                if Self::read_version(&tx, &fence.key)? != fence.version {
                    return Err(Error::new("read-fence-mismatch", 409));
                }
            }
            Ok(changes)
        })();
        match validation {
            Ok(changes) => {
                let seq = head.seq.next()?;
                let next = RemoteHead {
                    seq: seq.clone(),
                    head_id: hash(&canonical::encode(
                        &serde_json::json!({"domain":"risunest-sync-head-v1","previous":head.head_id,"operation":operation,"intent":digest,"seq":seq}),
                    )?),
                    ..head
                };
                tx.execute(
                    "INSERT INTO commits VALUES(?1,?2,?3)",
                    params![seq.as_str(), json(&next)?, operation],
                )?;
                for (index, change) in changes.changes.iter().enumerate() {
                    tx.execute("INSERT INTO records VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET version=excluded.version",params![change.key,json(&change.after)?])?;
                    tx.execute(
                        "INSERT INTO changes VALUES(?1,?2,?3)",
                        params![seq.as_str(), index as i64, json(change)?],
                    )?;
                }
                tx.execute(
                    "UPDATE library SET head=?1 WHERE singleton=1",
                    [json(&next)?],
                )?;
                receipt.head = next;
                receipt.status = TerminalStatus::Committed;
            }
            Err(error) if error.status < 500 => {
                receipt.status = if error.status == 412 {
                    TerminalStatus::Stale
                } else {
                    TerminalStatus::Failed
                };
                receipt.error = Some(error.code.into());
            }
            Err(error) => return Err(error),
        }
        tx.execute(
            "INSERT INTO receipts VALUES(?1,?2,?3,?4,?5)",
            params![
                operation,
                device.id,
                intent.device_operation_seq.as_str(),
                digest,
                json(&receipt)?
            ],
        )?;
        tx.execute(
            "UPDATE devices SET watermark=?1 WHERE id=?2",
            params![intent.device_operation_seq.as_str(), device.id],
        )?;
        tx.execute(
            "DELETE FROM staged_changes WHERE id=?1 AND device=?2",
            params![intent.staged_changes_id, device.id],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
    pub fn receipt(&self, device: &Device, operation: &str) -> Result<Receipt> {
        risunest_sync_wire::validate_hash(operation)?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let body: Option<String> = db
            .query_row(
                "SELECT body FROM receipts WHERE operation=?1 AND device=?2",
                params![operation, device.id],
                |r| r.get(0),
            )
            .optional()?;
        parse(&body.ok_or(Error::new("operation-not-found", 404))?)
    }
}
