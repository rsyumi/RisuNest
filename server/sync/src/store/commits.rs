use super::{json, parse, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical, hash, operation_id, CommitIntent, Receipt, RecordVersion, RemoteHead, Sequence,
    TerminalStatus,
};
use rusqlite::{params, Connection, OptionalExtension};

impl Store {
    pub(super) fn read_version(db: &Connection, key: &str) -> Result<RecordVersion> {
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
        Self::read_version(&*self.reader()?, key)
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
        let pending: Option<(String, String)> = tx
            .query_row(
                "SELECT operation,digest FROM commit_jobs WHERE device=?1",
                [&device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((reserved, reserved_digest)) = pending {
            if reserved != operation {
                return Err(Error::new("device-operation-active", 409));
            }
            if reserved_digest != digest {
                return Err(Error::new("operation-intent-conflict", 409));
            }
        }
        let staged: Option<Option<String>> = tx
            .query_row(
                "SELECT digest FROM staged_changes WHERE id=?1 AND device=?2",
                params![intent.staged_changes_id, device.id],
                |r| r.get(0),
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
        let stage = &intent.staged_changes_id;
        let validation = (|| -> Result<()> {
            if !intent.expected_head.same_revision(&head) {
                return Err(Error::new("stale-head", 412));
            }
            Self::require_staging(&tx, device, stage)?;
            let stored_digest = staged
                .as_ref()
                .ok_or(Error::new("staging-not-found", 404))?
                .as_ref()
                .ok_or(Error::new("staging-not-sealed", 409))?;
            if stored_digest != &intent.changes_digest {
                return Err(Error::new("changes-digest-mismatch", 409));
            }
            Self::each_change(&tx, stage, |change| {
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
                Ok(())
            })?;
            Self::each_fence(&tx, stage, |fence| {
                if Self::read_version(&tx, &fence.key)? != fence.version {
                    return Err(Error::new("read-fence-mismatch", 409));
                }
                Ok(())
            })?;
            Self::validate_scope_fences(&tx, stage)?;
            Ok(())
        })();
        match validation {
            Ok(()) => {
                let seq = head.seq.next()?;
                let next = RemoteHead {
                    seq: seq.clone(),
                    head_id: hash(&canonical::encode(
                        &serde_json::json!({"domain":"risunest-sync-head-v1","previous":head.head_id,"operation":operation,"intent":digest,"seq":seq}),
                    )?),
                    ..head
                };
                tx.execute_batch("SAVEPOINT apply_records")?;
                tx.execute(
                    "INSERT INTO commits VALUES(?1,?2,?3)",
                    params![seq.as_str(), json(&next)?, operation],
                )?;
                let mut index = 0i64;
                Self::each_change(&tx, stage, |change| {
                    tx.execute("INSERT INTO records VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET version=excluded.version",params![change.key,json(&change.after)?])?;
                    tx.execute(
                        "INSERT INTO changes VALUES(?1,?2,?3)",
                        params![seq.as_str(), index, json(&change)?],
                    )?;
                    index += 1;
                    Ok(())
                })?;
                Self::each_change(&tx, stage, |change| {
                    Self::update_relations(&tx, &change.key, &change.after)
                })?;
                if let Err(error) = Self::validate_relations(&tx, stage) {
                    if error.status >= 500 {
                        return Err(error);
                    }
                    tx.execute_batch("ROLLBACK TO apply_records; RELEASE apply_records")?;
                    receipt.error = Some(error.code.into());
                } else {
                    Self::update_scopes(&tx, stage, &operation)?;
                    tx.execute_batch("RELEASE apply_records")?;
                    tx.execute(
                        "UPDATE library SET head=?1 WHERE singleton=1",
                        [json(&next)?],
                    )?;
                    receipt.head = next;
                    receipt.status = TerminalStatus::Committed;
                }
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
            "INSERT INTO receipts(operation,device,seq,digest,body) VALUES(?1,?2,?3,?4,?5)",
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
        tx.execute("DELETE FROM commit_jobs WHERE operation=?1", [&operation])?;
        tx.commit()?;
        Ok(receipt)
    }
    pub fn receipt(&self, device: &Device, operation: &str) -> Result<Receipt> {
        risunest_sync_wire::validate_hash(operation)?;
        let db = self.reader()?;
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
