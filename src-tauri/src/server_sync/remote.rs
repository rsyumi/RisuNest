//! Persisted, resumable remote metadata mirror. This never activates PDS data.
use super::{
    client::{response_error, ServerClient},
    Result, SyncError,
};
use reqwest::Method;
use risunest_sync_wire::{
    canonical, Domain, RecordChange, RecordVersion, RemoteHead, Sequence, MAX_METADATA_BYTES,
};

/// Builds a SQL list from schema-owned wire identifiers, never from input.
fn domain_filter(domains: &[Domain]) -> String {
    domains
        .iter()
        .map(|domain| format!("'{}'", domain.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}
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
    domains: Vec<Domain>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Checkpoint {
    checkpoint_id: String,
    head: RemoteHead,
    domains: Vec<Domain>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointPage {
    checkpoint: Checkpoint,
    records: Vec<Entry>,
    next: Option<CheckpointCursor>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointCursor {
    domain: Domain,
    key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    domain: Domain,
    key: String,
    version: RecordVersion,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChangePage {
    through: RemoteHead,
    domains: Vec<Domain>,
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
    Checkpoint {
        id: String,
        after: Option<CheckpointCursor>,
    },
    Journal {
        id: String,
        after: Cursor,
    },
}
fn decode<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T> {
    Ok(canonical::decode(text.as_bytes(), MAX_METADATA_BYTES)?)
}
fn json<T: Serialize>(value: &T) -> Result<String> {
    String::from_utf8(canonical::encode(value)?)
        .map_err(|_| SyncError::new("invalid-remote-metadata", 502))
}
fn version(db: &Connection, domain: Domain, key: &str) -> Result<RecordVersion> {
    let value: Option<String> = db
        .query_row(
            "SELECT version FROM server_sync_remote WHERE domain=?1 AND key=?2",
            params![domain.as_str(), key],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|v| decode(&v))
        .transpose()
        .map(|v| v.unwrap_or(RecordVersion::Absent))
}

/// Whether two heads of one lineage carry an identical state for every listed
/// section. State identity, last change and collection floor all count: a floor
/// that moved is a real change even when no value did.
fn carries_the_same_sections(
    previous: &RemoteHead,
    observed: &RemoteHead,
    domains: &[Domain],
) -> bool {
    previous.library_id == observed.library_id
        && previous.epoch == observed.epoch
        && previous.seq <= observed.seq
        && domains.iter().all(|domain| {
            matches!(
                (previous.section(*domain), observed.section(*domain)),
                (Ok(before), Ok(after)) if before == after
            )
        })
}

/// The library plus whichever device sections this device takes part in, in
/// wire order. A section left out is unreceived, not emptied.
pub(crate) fn refresh(
    db: &mut Connection,
    client: &ServerClient,
    observed: &RemoteHead,
    domains: &[Domain],
) -> Result<RemoteHead> {
    if domains.first() != Some(&Domain::Library) && !domains.contains(&Domain::Library) {
        return Err(SyncError::new("invalid-remote-sections", 409));
    }
    if domains.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SyncError::new("invalid-remote-sections", 409));
    }
    match refresh_once(db, client, observed, domains) {
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
            refresh_once(db, client, observed, domains)
        }
        result => result,
    }
}
fn refresh_once(
    db: &mut Connection,
    client: &ServerClient,
    observed: &RemoteHead,
    domains: &[Domain],
) -> Result<RemoteHead> {
    observed.validate()?;
    let saved: Option<(String, Option<String>, bool, String)> = db
        .query_row(
            "SELECT head,cursor,complete,domains FROM server_sync_remote_cursor WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    // A participation choice that takes on a section leaves the mirror short of
    // that section's records, so the next fetch starts from a checkpoint over
    // every domain. Letting a section go asks for nothing the mirror does not
    // already hold, so a settled cursor narrows in place and keeps its position.
    // A fetch still in flight cannot narrow: the server checks the domain list
    // on every page of the checkpoint or pin it issued.
    let saved = match saved {
        Some(entry) => {
            let stored = decode::<Vec<Domain>>(&entry.3)?;
            if stored == domains {
                Some(entry)
            } else if entry.2 && domains.iter().all(|domain| stored.contains(domain)) {
                let narrowed = json(&domains)?;
                db.execute(
                    "UPDATE server_sync_remote_cursor SET domains=?1 WHERE singleton=1",
                    [&narrowed],
                )?;
                Some((entry.0, entry.1, entry.2, narrowed))
            } else {
                db.execute("DELETE FROM server_sync_remote_cursor", [])?;
                None
            }
        }
        None => None,
    };
    let through;
    let mut fetch;
    if let Some((head, cursor, false, _)) = &saved {
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
            .map(|(h, _, _, _)| decode::<RemoteHead>(h))
            .transpose()?;
        if previous.as_ref().is_some_and(|h| h.same_revision(observed)) {
            return Ok(observed.clone());
        }
        // The commit sequence also moves for sections this device leaves out.
        // When every requested section is untouched the mirror already holds
        // this head, so adopt it without reading a single record.
        if previous
            .as_ref()
            .is_some_and(|h| carries_the_same_sections(h, observed, domains))
        {
            db.execute("INSERT INTO server_sync_remote_cursor VALUES(1,?1,?2,NULL,1) ON CONFLICT(singleton) DO UPDATE SET head=excluded.head,domains=excluded.domains,cursor=NULL,complete=1",params![json(observed)?,json(&domains)?])?;
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
                Some(
                    &serde_json::json!({"epoch":previous.epoch,"afterSeq":previous.seq,"domains":domains}),
                ),
                &[],
            )?;
            if pin.after_seq != previous.seq
                || pin.through.epoch != previous.epoch
                || pin.domains != domains
            {
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
            let (_, checkpoint): (_, Checkpoint) = client.json(
                Method::POST,
                "checkpoints",
                &[],
                Some(&serde_json::json!({"domains":domains})),
                &[],
            )?;
            risunest_sync_wire::validate_id(&checkpoint.checkpoint_id)?;
            if checkpoint.domains != domains {
                return Err(SyncError::new("checkpoint-identity-mismatch", 502));
            }
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
            // Only the sections this checkpoint covers are rebuilt; the rest keep
            // the mirror they already had.
            let scope = domain_filter(domains);
            tx.execute(
                &format!("INSERT OR IGNORE INTO server_sync_remote_dirty SELECT domain,key FROM server_sync_remote WHERE domain IN ({scope})"),
                [],
            )?;
            tx.execute(
                &format!("DELETE FROM server_sync_remote WHERE domain IN ({scope})"),
                [],
            )?;
        }
        tx.execute("INSERT INTO server_sync_remote_cursor VALUES(1,?1,?2,?3,0) ON CONFLICT(singleton) DO UPDATE SET head=excluded.head,domains=excluded.domains,cursor=excluded.cursor,complete=0",params![json(&through)?,json(&domains)?,json(&fetch)?])?;
        tx.commit()?;
    }
    loop {
        let (entries, next, done, path) = match &fetch {
            Fetch::Checkpoint { id, after } => {
                let mut query = vec![("limit", "1024".into())];
                if let Some(after) = after {
                    query.push(("afterDomain", after.domain.as_str().into()));
                    query.push(("afterKey", after.key.clone()));
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
                    || page.checkpoint.domains != domains
                    || page.records.len() > 1024
                {
                    return Err(SyncError::new("checkpoint-identity-mismatch", 502));
                }
                let mut previous = after.as_ref().map(|c| (c.domain, c.key.as_str()));
                for entry in &page.records {
                    if previous.is_some_and(|p| p >= (entry.domain, entry.key.as_str())) {
                        return Err(SyncError::new("unordered-checkpoint", 502));
                    }
                    previous = Some((entry.domain, &entry.key));
                }
                if page.next.as_ref().is_some_and(|next| {
                    page.records.last().map(|r| (r.domain, r.key.as_str()))
                        != Some((next.domain, next.key.as_str()))
                }) {
                    return Err(SyncError::new("invalid-checkpoint-cursor", 502));
                }
                let done = page.next.is_none();
                let next = Fetch::Checkpoint {
                    id: id.clone(),
                    after: page.next,
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
                if !page.through.same_revision(&through)
                    || page.domains != domains
                    || page.entries.len() > 1024
                {
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
                    let address = (entry.change.domain, entry.change.key.clone());
                    let before = within
                        .get(&address)
                        .cloned()
                        .unwrap_or(version(db, address.0, &address.1)?);
                    if before != entry.change.before {
                        return Err(SyncError::new("journal-base-mismatch", 409));
                    }
                    within.insert(address, entry.change.after.clone());
                    entries.push(Entry {
                        domain: entry.change.domain,
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
                || !domains.contains(&entry.domain)
                || matches!(entry.version, RecordVersion::Absent)
            {
                return Err(SyncError::new("invalid-remote-record", 502));
            }
            entry.version.validate()?;
            tx.execute("INSERT INTO server_sync_remote VALUES(?1,?2,?3) ON CONFLICT(domain,key) DO UPDATE SET version=excluded.version",params![entry.domain.as_str(),entry.key,json(&entry.version)?])?;
            tx.execute(
                "INSERT OR IGNORE INTO server_sync_remote_dirty VALUES(?1,?2)",
                params![entry.domain.as_str(), entry.key],
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
