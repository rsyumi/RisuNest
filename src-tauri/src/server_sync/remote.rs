//! Persisted, resumable remote metadata mirror. This never activates PDS data.
use super::{
    client::{response_error, ServerClient},
    Result, SyncError,
};
use reqwest::Method;
use risunest_sync_wire::{
    canonical, RecordChange, RecordVersion, RemoteHead, Sequence, MAX_METADATA_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    seq: Sequence,
    ordinal: Sequence,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pin {
    pin_id: String,
    after_seq: Sequence,
    through: RemoteHead,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Checkpoint {
    checkpoint_id: String,
    head: RemoteHead,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointPage {
    checkpoint: Checkpoint,
    records: Vec<Entry>,
    next_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key: String,
    version: RecordVersion,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangePage {
    through: RemoteHead,
    entries: Vec<JournalChange>,
    next: Cursor,
    has_more: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalChange {
    cursor: Cursor,
    change: RecordChange,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Fetch {
    Checkpoint { id: String, after: Option<String> },
    Journal { id: String, after: Cursor },
}
fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    Ok(canonical::decode(text.as_bytes(), MAX_METADATA_BYTES)?)
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    String::from_utf8(canonical::encode(value)?)
        .map_err(|_| SyncError::new("invalid-remote-metadata", 502))
}
fn version(db: &Connection, key: &str) -> Result<RecordVersion> {
    let value: Option<String> = db
        .query_row(
            "SELECT version FROM server_sync_remote WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|v| decode(&v))
        .transpose()
        .map(|v| v.unwrap_or(RecordVersion::Absent))
}

pub(crate) fn refresh(
    db: &mut Connection,
    client: &ServerClient,
    observed: &RemoteHead,
) -> Result<RemoteHead> {
    match refresh_once(db, client, observed) {
        Err(error)
            if error.status == 410
                || (error.status == 404
                    && matches!(
                        error.code.as_str(),
                        "checkpoint-not-found" | "pin-not-found"
                    )) =>
        {
            // An expired metadata lease invalidates only the staging cursor.
            // Rebuild a fixed checkpoint while preserving PDS, bases and outbox.
            db.execute("DELETE FROM server_sync_remote_cursor", [])?;
            refresh_once(db, client, observed)
        }
        result => result,
    }
}
fn refresh_once(
    db: &mut Connection,
    client: &ServerClient,
    observed: &RemoteHead,
) -> Result<RemoteHead> {
    observed.validate()?;
    let saved: Option<(String, Option<String>, bool)> = db
        .query_row(
            "SELECT head,cursor,complete FROM server_sync_remote_cursor WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let through;
    let mut fetch;
    if let Some((head, cursor, false)) = &saved {
        through = decode::<RemoteHead>(head)?;
        if through.epoch != observed.epoch {
            return Err(SyncError::new("epoch-reconciliation-required", 409));
        }
        fetch = decode::<Fetch>(
            cursor
                .as_deref()
                .ok_or_else(|| SyncError::new("invalid-local-fetch-cursor", 409))?,
        )?;
    } else {
        let previous = saved
            .as_ref()
            .map(|(h, _, _)| decode::<RemoteHead>(h))
            .transpose()?;
        if previous.as_ref().is_some_and(|h| h.same_revision(observed)) {
            return Ok(observed.clone());
        }
        if previous
            .as_ref()
            .is_some_and(|h| h.epoch != observed.epoch || h.library_id != observed.library_id)
        {
            return Err(SyncError::new("epoch-reconciliation-required", 409));
        }
        if let Some(previous) = previous.filter(|h| h.seq >= observed.min_retained_seq) {
            let (_, pin): (_, Pin) = client.json(
                Method::POST,
                "read-pins",
                &[],
                Some(&serde_json::json!({"epoch":previous.epoch,"afterSeq":previous.seq})),
                &[],
            )?;
            if pin.after_seq != previous.seq || pin.through.epoch != previous.epoch {
                return Err(SyncError::new("invalid-read-pin", 502));
            }
            risunest_sync_wire::validate_id(&pin.pin_id)?;
            through = pin.through;
            fetch = Fetch::Journal {
                id: pin.pin_id,
                after: Cursor {
                    seq: previous.seq,
                    ordinal: Sequence::from(i64::MAX as u64),
                },
            };
        } else {
            let (_, checkpoint): (_, Checkpoint) =
                client.json(Method::POST, "checkpoints", &[], None::<&()>, &[])?;
            risunest_sync_wire::validate_id(&checkpoint.checkpoint_id)?;
            through = checkpoint.head;
            fetch = Fetch::Checkpoint {
                id: checkpoint.checkpoint_id,
                after: None,
            };
        }
        through.validate()?;
        if through.library_id != observed.library_id || through.epoch != observed.epoch {
            return Err(SyncError::new("remote-fetch-identity-mismatch", 502));
        }
        let tx = db.transaction()?;
        if matches!(fetch, Fetch::Checkpoint { .. }) {
            // Preserve removals from the previous remote snapshot in the work set.
            tx.execute(
                "INSERT OR IGNORE INTO server_sync_remote_dirty SELECT key FROM server_sync_remote",
                [],
            )?;
            tx.execute("DELETE FROM server_sync_remote", [])?;
        }
        tx.execute("INSERT INTO server_sync_remote_cursor VALUES(1,?1,?2,0) ON CONFLICT(singleton) DO UPDATE SET head=excluded.head,cursor=excluded.cursor,complete=0",params![json(&through)?,json(&fetch)?])?;
        tx.commit()?;
    }
    loop {
        let (entries, next, done, path) = match &fetch {
            Fetch::Checkpoint { id, after } => {
                let mut query = vec![("limit", "1024".into())];
                if let Some(after) = after {
                    query.push(("afterKey", after.clone()));
                }
                let (_, page): (_, CheckpointPage) = client.json(
                    Method::GET,
                    &format!("checkpoints/{id}"),
                    &query,
                    None::<&()>,
                    &[],
                )?;
                if page.checkpoint.checkpoint_id != *id
                    || !page.checkpoint.head.same_revision(&through)
                    || page.records.len() > 1024
                {
                    return Err(SyncError::new("checkpoint-identity-mismatch", 502));
                }
                let mut previous = after.as_deref();
                for entry in &page.records {
                    if previous.is_some_and(|p| p >= entry.key.as_str()) {
                        return Err(SyncError::new("unordered-checkpoint", 502));
                    }
                    previous = Some(&entry.key);
                }
                if page.next_key.is_some()
                    && page.next_key.as_deref() != page.records.last().map(|r| r.key.as_str())
                {
                    return Err(SyncError::new("invalid-checkpoint-cursor", 502));
                }
                let done = page.next_key.is_none();
                let next = Fetch::Checkpoint {
                    id: id.clone(),
                    after: page.next_key,
                };
                (page.records, next, done, format!("checkpoints/{id}"))
            }
            Fetch::Journal { id, after } => {
                let (_, page): (_, ChangePage) = client.json(
                    Method::GET,
                    &format!("read-pins/{id}"),
                    &[
                        ("afterSeq", after.seq.as_str().into()),
                        ("afterOrdinal", after.ordinal.as_str().into()),
                        ("limit", "1024".into()),
                    ],
                    None::<&()>,
                    &[],
                )?;
                if !page.through.same_revision(&through) || page.entries.len() > 1024 {
                    return Err(SyncError::new("journal-identity-mismatch", 502));
                }
                let mut prior = after.clone();
                let mut entries = Vec::new();
                let mut within = std::collections::BTreeMap::new();
                for entry in page.entries {
                    if (&entry.cursor.seq, &entry.cursor.ordinal) <= (&prior.seq, &prior.ordinal)
                        || entry.cursor.seq > through.seq
                    {
                        return Err(SyncError::new("invalid-journal-cursor", 502));
                    }
                    let before = within
                        .get(&entry.change.key)
                        .cloned()
                        .unwrap_or(version(db, &entry.change.key)?);
                    if before != entry.change.before {
                        return Err(SyncError::new("journal-base-mismatch", 409));
                    }
                    within.insert(entry.change.key.clone(), entry.change.after.clone());
                    entries.push(Entry {
                        key: entry.change.key,
                        version: entry.change.after,
                    });
                    prior = entry.cursor;
                }
                if (page.has_more
                    && ((&page.next.seq, &page.next.ordinal) != (&prior.seq, &prior.ordinal)
                        || (&prior.seq, &prior.ordinal) <= (&after.seq, &after.ordinal)))
                    || page.next.seq > through.seq
                {
                    return Err(SyncError::new("invalid-journal-cursor", 502));
                }
                let next = Fetch::Journal {
                    id: id.clone(),
                    after: page.next,
                };
                (entries, next, !page.has_more, format!("read-pins/{id}"))
            }
        };
        let tx = db.transaction()?;
        for entry in entries {
            if entry.key.is_empty()
                || entry.key.len() > risunest_sync_wire::MAX_KEY_BYTES
                || matches!(entry.version, RecordVersion::Absent)
            {
                return Err(SyncError::new("invalid-remote-record", 502));
            }
            entry.version.validate()?;
            tx.execute("INSERT INTO server_sync_remote VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET version=excluded.version",params![entry.key,json(&entry.version)?])?;
            tx.execute(
                "INSERT OR IGNORE INTO server_sync_remote_dirty VALUES(?1)",
                [entry.key],
            )?;
        }
        tx.execute(
            "UPDATE server_sync_remote_cursor SET cursor=?1,complete=?2 WHERE singleton=1",
            params![if done { None } else { Some(json(&next)?) }, done],
        )?;
        tx.commit()?;
        if done {
            let reply =
                client.request(Method::DELETE, &path, &[], None, &[], MAX_METADATA_BYTES)?;
            if reply.status != 204 && reply.status != 404 && reply.status != 410 {
                return Err(response_error(reply));
            }
            return Ok(through);
        }
        fetch = next;
    }
}
