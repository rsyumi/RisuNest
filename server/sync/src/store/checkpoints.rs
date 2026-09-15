use super::{
    json, parse, random_id, requested_domains, uploads::now, ChangeCursor, ChangePage, Device,
    Store,
};
use crate::{Error, Result};
use risunest_sync_wire::{
    Domain, RecordVersion, RemoteHead, Sequence, MAX_METADATA_BYTES, MAX_PAGE_RECORDS,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadPin {
    pub pin_id: String,
    pub after_seq: Sequence,
    pub through: RemoteHead,
    pub domains: Vec<Domain>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub checkpoint_id: String,
    pub head: RemoteHead,
    pub domains: Vec<Domain>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointPage {
    pub checkpoint: Checkpoint,
    pub records: Vec<CheckpointRecord>,
    pub next: Option<CheckpointCursor>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCursor {
    pub domain: Domain,
    pub key: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRecord {
    pub domain: Domain,
    pub key: String,
    pub version: RecordVersion,
}
impl CheckpointRecord {
    fn cursor(&self) -> CheckpointCursor {
        CheckpointCursor {
            domain: self.domain,
            key: self.key.clone(),
        }
    }
}

impl Store {
    pub fn pin_changes(
        &self,
        device: &Device,
        epoch: &str,
        after: &Sequence,
        domains: &[Domain],
    ) -> Result<ReadPin> {
        let domains = requested_domains(domains)?;
        let db = self.db()?;
        Self::require_device(&db, device)?;
        let head = Self::read_head(&db)?;
        if epoch != head.epoch {
            return Err(Error::new("epoch-changed", 409));
        }
        for domain in &domains {
            if after < &head.section(*domain)?.gc_floor {
                return Err(Error::new("checkpoint-required", 410));
            }
        }
        if after > &head.seq {
            return Err(Error::new("invalid-cursor", 400));
        }
        let count: i64 = db.query_row(
            "SELECT count(*) FROM read_pins WHERE device=?1 AND expires>?2",
            params![device.id, now()?],
            |r| r.get(0),
        )?;
        if count >= 16 {
            return Err(Error::new("pin-quota", 429));
        }
        let id = random_id()?;
        db.execute(
            "INSERT INTO read_pins VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                id,
                device.id,
                after.as_str(),
                json(&head)?,
                json(&domains)?,
                now()? + 86400
            ],
        )?;
        Ok(ReadPin {
            pin_id: id,
            after_seq: after.clone(),
            through: head,
            domains,
        })
    }
    pub fn pinned_changes(
        &self,
        device: &Device,
        id: &str,
        after: &ChangeCursor,
        limit: usize,
    ) -> Result<ChangePage> {
        let (start, head, domains) = {
            let db = self.reader()?;
            Self::require_device(&db, device)?;
            let (start, body, domains, expires): (String, String, String, i64) = db
                .query_row(
                    "SELECT after_seq,through,domains,expires FROM read_pins WHERE id=?1 AND device=?2",
                    params![id, device.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?
                .ok_or(Error::new("pin-not-found", 404))?;
            if expires <= now()? {
                return Err(Error::new("pin-expired", 410));
            }
            (
                Sequence::try_from(start)?,
                parse::<RemoteHead>(&body)?,
                parse::<Vec<Domain>>(&domains)?,
            )
        };
        if after.seq < start {
            return Err(Error::new("invalid-cursor", 400));
        }
        // The pin fixed both the through head and the sections at creation.
        self.changes(&head.epoch, after, &head.seq, &domains, limit)
    }
    pub fn release_pin(&self, device: &Device, id: &str) -> Result<()> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        if db.execute(
            "DELETE FROM read_pins WHERE id=?1 AND device=?2",
            params![id, device.id],
        )? == 0
        {
            return Err(Error::new("pin-not-found", 404));
        }
        Ok(())
    }
    pub fn create_checkpoint(&self, device: &Device, domains: &[Domain]) -> Result<Checkpoint> {
        let domains = requested_domains(domains)?;
        let mut db = self.db()?;
        Self::require_device(&db, device)?;
        let count: i64 = db.query_row(
            "SELECT count(*) FROM checkpoints WHERE device=?1 AND expires>?2",
            params![device.id, now()?],
            |r| r.get(0),
        )?;
        if count >= 2 {
            return Err(Error::new("checkpoint-quota", 429));
        }
        let tx = db.transaction()?;
        let head = Self::read_head(&tx)?;
        let id = random_id()?;
        tx.execute(
            "INSERT INTO checkpoints VALUES(?1,?2,?3,?4,?5)",
            params![id, device.id, json(&head)?, json(&domains)?, now()? + 86400],
        )?;
        for domain in &domains {
            tx.execute(
                "INSERT INTO checkpoint_records SELECT ?1,domain,key,version FROM records WHERE domain=?2",
                params![id, domain.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(Checkpoint {
            checkpoint_id: id,
            head,
            domains,
        })
    }
    pub fn checkpoint_page(
        &self,
        device: &Device,
        id: &str,
        after: Option<&CheckpointCursor>,
        limit: usize,
    ) -> Result<CheckpointPage> {
        if limit == 0 || limit > MAX_PAGE_RECORDS {
            return Err(Error::new("invalid-page-limit", 400));
        }
        let mut connection = self.reader()?;
        let db = connection.transaction()?;
        Self::require_device(&db, device)?;
        let (body, domains, expires): (String, String, i64) = db
            .query_row(
                "SELECT head,domains,expires FROM checkpoints WHERE id=?1 AND device=?2",
                params![id, device.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(Error::new("checkpoint-not-found", 404))?;
        if expires <= now()? {
            return Err(Error::new("checkpoint-expired", 410));
        }
        let (after_domain, after_key) = match after {
            Some(cursor) => (cursor.domain.as_str(), cursor.key.as_str()),
            None => ("", ""),
        };
        let mut statement=db.prepare("SELECT domain,key,version FROM checkpoint_records WHERE checkpoint=?1 AND (domain,key)>(?2,?3) ORDER BY domain,key LIMIT ?4")?;
        let mut rows = statement.query(params![id, after_domain, after_key, limit as i64 + 1])?;
        let mut records = Vec::<CheckpointRecord>::new();
        let mut used = 0;
        let mut next = None;
        while let Some(row) = rows.next()? {
            let domain: String = row.get(0)?;
            let key: String = row.get(1)?;
            let version: String = row.get(2)?;
            let record = CheckpointRecord {
                domain: Domain::try_from(domain.as_str())?,
                key,
                version: parse(&version)?,
            };
            let size = json(&record)?.len();
            if records.len() == limit
                || (!records.is_empty() && used + size > MAX_METADATA_BYTES - 4096)
            {
                next = records.last().map(CheckpointRecord::cursor);
                break;
            }
            used += size;
            records.push(record);
        }
        Ok(CheckpointPage {
            checkpoint: Checkpoint {
                checkpoint_id: id.into(),
                head: parse(&body)?,
                domains: parse(&domains)?,
            },
            records,
            next,
        })
    }
    pub fn release_checkpoint(&self, device: &Device, id: &str) -> Result<()> {
        let db = self.db()?;
        Self::require_device(&db, device)?;
        if db.execute(
            "DELETE FROM checkpoints WHERE id=?1 AND device=?2",
            params![id, device.id],
        )? == 0
        {
            return Err(Error::new("checkpoint-not-found", 404));
        }
        Ok(())
    }
}
