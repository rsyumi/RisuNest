use super::{domain_filter, parse, requested_domains, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    Domain, RecordChange, RemoteHead, Sequence, MAX_METADATA_BYTES, MAX_PAGE_RECORDS,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeCursor {
    pub seq: Sequence,
    pub ordinal: Sequence,
}
impl ChangeCursor {
    /// Start after an entirely applied commit; ordinal i64::MAX is an end-of-commit sentinel.
    pub fn after_commit(seq: Sequence) -> Self {
        Self {
            seq,
            ordinal: (i64::MAX as u64).into(),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JournalChange {
    pub cursor: ChangeCursor,
    pub change: RecordChange,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangePage {
    pub through: RemoteHead,
    pub domains: Vec<Domain>,
    pub entries: Vec<JournalChange>,
    pub next: ChangeCursor,
    pub has_more: bool,
}

impl Store {
    /// All history is retained in the initial slice. Immutable commit heads give a
    /// stable through boundary without copying records or pinning the writer.
    /// Retention/GC must introduce durable read pins before removing this invariant.
    pub fn changes(
        &self,
        epoch: &str,
        after: &ChangeCursor,
        through: &Sequence,
        domains: &[Domain],
        limit: usize,
    ) -> Result<ChangePage> {
        if limit == 0 || limit > MAX_PAGE_RECORDS {
            return Err(Error::new("invalid-page-limit", 400));
        }
        let domains = requested_domains(domains)?;
        let ordinal = after
            .ordinal
            .as_str()
            .parse::<i64>()
            .ok()
            .filter(|v| *v >= 0)
            .ok_or(Error::new("invalid-cursor", 400))?;
        let mut connection = self.reader()?;
        let db = connection.transaction()?;
        let head = Self::read_head(&db)?;
        if epoch != head.epoch {
            return Err(Error::new("epoch-changed", 409));
        }
        for domain in &domains {
            if after.seq < head.section(*domain)?.gc_floor {
                return Err(Error::new("checkpoint-required", 410));
            }
        }
        if after.seq > *through || through > &head.seq {
            return Err(Error::new("invalid-cursor", 400));
        }
        let through_head = if through == &head.seq {
            head
        } else if through == &Sequence::from(0) {
            RemoteHead::genesis(head.library_id.clone(), head.epoch.clone())?
        } else {
            let body: Option<String> = db
                .query_row(
                    "SELECT head FROM commits WHERE seq=?1",
                    [through.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            parse(&body.ok_or(Error::new("through-head-not-found", 404))?)?
        };
        let mut statement = db.prepare(&format!("SELECT seq,ordinal,body FROM changes WHERE domain IN ({}) AND (length(seq),seq,ordinal)>(?1,?2,?3) AND (length(seq),seq)<=(?4,?5) ORDER BY length(seq),seq,ordinal LIMIT ?6",domain_filter(&domains)))?;
        let mut rows = statement.query(params![
            after.seq.as_str().len() as i64,
            after.seq.as_str(),
            ordinal,
            through.as_str().len() as i64,
            through.as_str(),
            limit as i64 + 1
        ])?;
        let mut entries = Vec::new();
        let mut used = 0usize;
        let mut next = after.clone();
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            let body: String = row.get(2)?;
            if entries.len() == limit
                || (!entries.is_empty() && used + body.len() > MAX_METADATA_BYTES)
            {
                has_more = true;
                break;
            }
            let seq: String = row.get(0)?;
            let ordinal: i64 = row.get(1)?;
            next = ChangeCursor {
                seq: seq.try_into()?,
                ordinal: (ordinal as u64).into(),
            };
            used += body.len();
            entries.push(JournalChange {
                cursor: next.clone(),
                change: parse(&body)?,
            });
        }
        Ok(ChangePage {
            through: through_head,
            domains,
            entries,
            next,
            has_more,
        })
    }
}
