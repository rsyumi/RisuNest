use super::{json, parse, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{operation_id, CommitIntent, Receipt, Sequence};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum CommitSubmission {
    Terminal(Receipt),
    Pending {
        #[serde(rename = "operationId")]
        operation_id: String,
        status: &'static str,
    },
}

impl Store {
    /// Reservation is durable before acknowledging acceptance, including sequence
    /// identity and staging ownership. A restart runs the same intent again.
    pub fn submit_commit(
        &self,
        device: &Device,
        intent: &CommitIntent,
        if_match: &str,
    ) -> Result<CommitSubmission> {
        intent.validate()?;
        if if_match != intent.expected_head.etag() {
            return Err(Error::new("if-match-intent-mismatch", 400));
        }
        let digest = intent.digest()?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let head = Self::read_head(&db)?;
        if head.library_id != intent.expected_head.library_id {
            return Err(Error::new("library-mismatch", 403));
        }
        let operation = operation_id(&head.library_id, &device.id, &intent.device_operation_seq)?;
        let receipt: Option<(String, String)> = db
            .query_row(
                "SELECT digest,body FROM receipts WHERE operation=?1 AND device=?2",
                params![operation, device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((prior, body)) = receipt {
            if prior != digest {
                return Err(Error::new("operation-intent-conflict", 409));
            }
            return Ok(CommitSubmission::Terminal(parse(&body)?));
        }
        let pending: Option<(String, String)> = db
            .query_row(
                "SELECT operation,digest FROM commit_jobs WHERE device=?1",
                [&device.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((prior, prior_digest)) = pending {
            if prior != operation {
                return Err(Error::new("device-operation-active", 409));
            }
            if prior_digest != digest {
                return Err(Error::new("operation-intent-conflict", 409));
            }
        } else {
            let watermark: String = db.query_row(
                "SELECT watermark FROM devices WHERE id=?1",
                [&device.id],
                |r| r.get(0),
            )?;
            if intent.device_operation_seq <= Sequence::try_from(watermark)? {
                return Err(Error::new("operation-history-expired", 410));
            }
            Self::require_staging(&db, device, &intent.staged_changes_id)?;
            db.execute(
                "INSERT INTO commit_jobs(operation,device,digest,body,stage) VALUES(?1,?2,?3,?4,?5)",
                params![
                    operation,
                    device.id,
                    digest,
                    json(intent)?,
                    intent.staged_changes_id
                ],
            )?;
        }
        Ok(CommitSubmission::Pending {
            operation_id: operation,
            status: "pending",
        })
    }
    pub fn operation_status(&self, device: &Device, operation: &str) -> Result<CommitSubmission> {
        match self.receipt(device, operation) {
            Ok(receipt) => return Ok(CommitSubmission::Terminal(receipt)),
            Err(e) if e.code == "operation-not-found" => (),
            Err(e) => return Err(e),
        }
        let db = self.reader()?;
        Self::require_device(&db, device)?;
        let found: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM commit_jobs WHERE operation=?1 AND device=?2)",
            params![operation, device.id],
            |r| r.get(0),
        )?;
        if !found {
            return Err(Error::new("operation-not-found", 404));
        }
        Ok(CommitSubmission::Pending {
            operation_id: operation.into(),
            status: "pending",
        })
    }
    pub fn run_pending_commit(&self) -> Result<bool> {
        let job: Option<(String, String)> = {
            let db = self.reader()?;
            db.query_row(
                "SELECT device,body FROM commit_jobs WHERE retry_after<=unixepoch() ORDER BY retry_after,rowid LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        };
        let Some((id, body)) = job else {
            return Ok(false);
        };
        let intent: CommitIntent = parse(&body)?;
        let device = Device { id };
        match self.commit(&device, &intent, &intent.expected_head.etag()) {
            Ok(_) => Ok(true),
            Err(error) if error.code == "unauthorized" => {
                self.db()?
                    .execute("DELETE FROM commit_jobs WHERE device=?1", [&device.id])?;
                Ok(true)
            }
            Err(error) => {
                self.db()?.execute(
                    "UPDATE commit_jobs SET retry_after=unixepoch()+1 WHERE device=?1",
                    [&device.id],
                )?;
                Err(error)
            }
        }
    }
}
