use super::{record_projection::codec_error, StoreError, StoreResult};
use risunest_external_storage_format::{
    logical_records::{decode_message_page, encoded_object, LOGICAL_MESSAGE_PAGE_SIZE},
    message_pages::{
        ManifestPage, MessageHash, MessageManifest, PageBoundary, MANIFEST_SCHEMA, MAX_PAGE_BYTES,
        MIN_PAGE_MESSAGES, PAGE_PREFIX, PAGE_SUFFIX,
    },
};
use risunest_sync_wire::{
    descriptor::{build_reference_tree, inline_references, RecordDescriptor},
    payload_value,
    stamp::DecimalU64,
    unit::UnitValue,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;

pub(super) const SCHEMA: &str = "
CREATE TABLE message_page_indexes (
    generation TEXT NOT NULL, character_id TEXT NOT NULL, conversation_id TEXT NOT NULL,
    page_start INTEGER NOT NULL, message_count INTEGER NOT NULL,
    hash TEXT NOT NULL, byte_length INTEGER NOT NULL,
    PRIMARY KEY (generation,character_id,conversation_id,page_start)
);
CREATE TABLE message_page_manifests (
    generation TEXT NOT NULL, character_id TEXT NOT NULL, conversation_id TEXT NOT NULL,
    body BLOB NOT NULL, PRIMARY KEY(generation,character_id,conversation_id)
);
CREATE TABLE message_page_objects (hash TEXT PRIMARY KEY, body BLOB NOT NULL);
CREATE TABLE message_page_verified_objects (
    hash TEXT PRIMARY KEY REFERENCES message_page_objects(hash) ON DELETE CASCADE
);
CREATE TABLE message_page_proofs (
    hash TEXT PRIMARY KEY REFERENCES message_page_objects(hash) ON DELETE CASCADE,
    message_count INTEGER NOT NULL, byte_length INTEGER NOT NULL,
    first_length INTEGER NOT NULL, ends_cut INTEGER NOT NULL, message_hashes TEXT NOT NULL
);
CREATE TABLE message_page_object_marks (
    hash TEXT PRIMARY KEY, unreferenced_since INTEGER NOT NULL
);
CREATE TABLE message_page_sweep_cursor (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1), after_hash TEXT NOT NULL
);
CREATE TRIGGER message_page_objects_immutable_update BEFORE UPDATE ON message_page_objects
BEGIN SELECT RAISE(ABORT,'message object is immutable'); END;
CREATE TRIGGER message_page_objects_immutable_replace BEFORE INSERT ON message_page_objects
WHEN EXISTS(SELECT 1 FROM message_page_objects WHERE hash=NEW.hash)
BEGIN SELECT RAISE(ABORT,'message object is immutable'); END;
CREATE TRIGGER message_page_objects_invalidate AFTER DELETE ON message_page_objects
BEGIN
    DELETE FROM message_page_verified_objects WHERE hash=OLD.hash;
    DELETE FROM message_page_proofs WHERE hash=OLD.hash;
    DELETE FROM message_page_object_marks WHERE hash=OLD.hash;
END;
CREATE TRIGGER message_page_verified_objects_immutable BEFORE UPDATE ON message_page_verified_objects
BEGIN SELECT RAISE(ABORT,'message verification is immutable'); END;
CREATE TRIGGER message_page_verified_objects_parent BEFORE INSERT ON message_page_verified_objects
WHEN NOT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=NEW.hash)
BEGIN SELECT RAISE(ABORT,'message object is missing'); END;
CREATE TRIGGER message_page_verified_objects_no_replace BEFORE INSERT ON message_page_verified_objects
WHEN EXISTS(SELECT 1 FROM message_page_verified_objects WHERE hash=NEW.hash)
BEGIN SELECT RAISE(ABORT,'message verification is immutable'); END;
CREATE TRIGGER message_page_proofs_immutable BEFORE UPDATE ON message_page_proofs
BEGIN SELECT RAISE(ABORT,'message page proof is immutable'); END;
CREATE TRIGGER message_page_proofs_parent BEFORE INSERT ON message_page_proofs
WHEN NOT EXISTS(SELECT 1 FROM message_page_objects o
    JOIN message_page_verified_objects v ON v.hash=o.hash WHERE o.hash=NEW.hash)
BEGIN SELECT RAISE(ABORT,'message object is not verified'); END;
CREATE TRIGGER message_page_proofs_no_replace BEFORE INSERT ON message_page_proofs
WHEN EXISTS(SELECT 1 FROM message_page_proofs WHERE hash=NEW.hash)
BEGIN SELECT RAISE(ABORT,'message page proof is immutable'); END;
";

#[derive(Clone, Copy, Debug)]
pub(super) struct MessageEdit {
    pub start: i64,
    pub delete_count: i64,
    pub insert_count: i64,
}

/// A changed span, in old and new message positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DirtyRegion {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Piece {
    Old { start: usize, len: usize },
    New { len: usize },
}

impl Piece {
    fn len(self) -> usize {
        match self {
            Self::Old { len, .. } | Self::New { len } => len,
        }
    }
    fn slice(self, offset: usize, len: usize) -> Self {
        match self {
            Self::Old { start, .. } => Self::Old { start: start + offset, len },
            Self::New { .. } => Self::New { len },
        }
    }
}

fn pieces_between(pieces: &[Piece], from: usize, to: usize, out: &mut Vec<Piece>) {
    let mut position = 0;
    for &piece in pieces {
        let (start, end) = (position, position + piece.len());
        position = end;
        let (low, high) = (from.max(start), to.min(end));
        if low < high {
            out.push(piece.slice(low - start, high - low));
        }
    }
}

/// Applies the edits in order to the old sequence and returns the spans that
/// differ from it. Each edit is checked against the count the earlier ones left.
fn dirty_regions(old_count: usize, count: usize, edits: &[MessageEdit]) -> StoreResult<Vec<DirtyRegion>> {
    let invalid = || codec_error("invalid message page edit range");
    let mut pieces = vec![Piece::Old { start: 0, len: old_count }];
    let mut length = old_count;
    for edit in edits {
        let start = usize::try_from(edit.start).map_err(|_| invalid())?;
        let deleted = usize::try_from(edit.delete_count).map_err(|_| invalid())?;
        let inserted = usize::try_from(edit.insert_count).map_err(|_| invalid())?;
        if start > length || deleted > length - start {
            return Err(invalid());
        }
        let mut next = Vec::with_capacity(pieces.len() + 2);
        pieces_between(&pieces, 0, start, &mut next);
        next.push(Piece::New { len: inserted });
        pieces_between(&pieces, start + deleted, length, &mut next);
        pieces = next;
        length = length - deleted + inserted;
    }
    if length != count {
        return Err(invalid());
    }
    let mut merged: Vec<Piece> = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match (merged.last_mut(), piece) {
            (Some(Piece::New { len }), Piece::New { len: more }) => *len += more,
            (Some(Piece::Old { start, len }), Piece::Old { start: next, len: more }) if *start + *len == next => *len += more,
            _ => merged.push(piece),
        }
    }
    // A region is a run of new pieces, or a gap between kept old pieces.
    let mut regions = Vec::new();
    let (mut old_position, mut new_position) = (0, 0);
    let mut open: Option<(usize, usize)> = None;
    for piece in merged.into_iter().chain([Piece::Old { start: old_count, len: 0 }]) {
        match piece {
            Piece::Old { start, len } => {
                let (old_start, new_start) = open.take().unwrap_or((old_position, new_position));
                if start != old_start || new_position != new_start {
                    regions.push(DirtyRegion {
                        old_start,
                        old_end: start,
                        new_start,
                        new_end: new_position,
                    });
                }
                old_position = start + len;
                new_position += len;
            }
            Piece::New { len } => {
                open.get_or_insert((old_position, new_position));
                new_position += len;
            }
        }
    }
    Ok(regions)
}

pub(super) fn object_body(db: &Connection, hash: &str) -> StoreResult<Option<Vec<u8>>> {
    let body: Option<Vec<u8>> = db
        .query_row(
            "SELECT body FROM message_page_objects WHERE hash=?1",
            [hash],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(body) = &body {
        if {
            let hash_input = body;
            #[cfg(test)]
            crate::persistent_store::hash_work::observe("native_page_object_verify", hash_input.len());
            risunest_sync_wire::hash(hash_input)
        } != hash {
            return Err(codec_error("message object hash mismatch"));
        }
    }
    Ok(body)
}

pub(super) fn put_object(db: &Connection, hash: &str, body: &[u8]) -> StoreResult<()> {
    risunest_sync_wire::validate_hash(hash).map_err(codec_error)?;
    if {
        let hash_input = body;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_page_object_verify", hash_input.len());
        risunest_sync_wire::hash(hash_input)
    } != hash {
        return Err(codec_error("message object hash mismatch"));
    }
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)",
        [hash], |row| row.get(0))?;
    if !exists {
        db.execute("INSERT INTO message_page_objects(hash,body) VALUES(?1,?2)", params![hash, body])?;
    }
    if object_body(db, hash)?.as_deref() != Some(body) {
        return Err(codec_error("message object identity collision"));
    }
    if !verified_object_present(db, hash)? {
        db.execute("INSERT INTO message_page_verified_objects(hash) VALUES(?1)", [hash])?;
    }
    // Storing an object is a new use of it, so a pending collection stops.
    db.execute("DELETE FROM message_page_object_marks WHERE hash=?1", [hash])?;
    Ok(())
}

// Only successful hash verification certifies presence. Deleting an immutable
// object invalidates its proofs, including when foreign keys are disabled.
pub(super) fn verified_object_present(db: &Connection, hash: &str) -> StoreResult<bool> {
    risunest_sync_wire::validate_hash(hash).map_err(codec_error)?;
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM message_page_verified_objects v
        JOIN message_page_objects o ON o.hash=v.hash WHERE v.hash=?1)", [hash], |row| row.get(0))?)
}

fn require_object(db: &Connection, hash: &str) -> StoreResult<Vec<u8>> {
    object_body(db, hash)?.ok_or_else(|| codec_error("message object body is missing"))
}

fn descriptor(
    manifest: &MessageManifest,
) -> StoreResult<(RecordDescriptor, Vec<(String, Vec<u8>)>)> {
    let object = {
        let result = manifest.encode();
        #[cfg(test)]
        crate::persistent_store::hash_work::encoded_object("native_manifest_identity", &result);
        result
    }.map_err(codec_error)?;
    let mut descriptor = RecordDescriptor::content(object.hash);
    let mut hashes = manifest
        .pages
        .iter()
        .map(|p| p.hash.clone())
        .collect::<Vec<_>>();
    hashes.sort();
    hashes.dedup();
    let objects = if inline_references(&hashes).map_err(codec_error)? {
        descriptor.dependencies = hashes;
        Vec::new()
    } else {
        let (root, objects) = {
            let result = build_reference_tree(&hashes, false);
            #[cfg(test)]
            crate::persistent_store::hash_work::reference_creation(&result);
            result
        }.map_err(codec_error)?;
        descriptor.dependency_root = root;
        objects
    };
    descriptor.validate().map_err(codec_error)?;
    Ok((descriptor, objects))
}

fn unit_value(db: &Connection, manifest: &MessageManifest) -> StoreResult<UnitValue> {
    let object = {
        let result = manifest.encode();
        #[cfg(test)]
        crate::persistent_store::hash_work::encoded_object("native_manifest_identity", &result);
        result
    }.map_err(codec_error)?;
    put_object(db, &object.hash, &object.bytes)?;
    let (descriptor, references) = descriptor(manifest)?;
    for (hash, body) in references {
        put_object(db, &hash, &body)?;
    }
    let body = descriptor.bytes().map_err(codec_error)?;
    put_object(db, &{
        let hash_input = &body;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_descriptor_object_identity", hash_input.len());
        risunest_sync_wire::hash(hash_input)
    }, &body)?;
    {
        let result = UnitValue::object(descriptor);
        #[cfg(test)]
        crate::persistent_store::hash_work::descriptor_creation(&result);
        result
    }.map_err(codec_error)
}

pub(super) fn load_pages(
    db: &Connection,
    generation: &str,
    character: &str,
    conversation: &str,
) -> StoreResult<Vec<PageBoundary>> {
    let mut statement = db.prepare(
        "SELECT page_start,message_count,hash,byte_length FROM message_page_indexes
        WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 ORDER BY page_start",
    )?;
    let rows = statement.query_map(params![generation, character, conversation], |r| {
        Ok(PageBoundary {
            start: sql_usize(r.get::<_, i64>(0)?, 0)?,
            page: ManifestPage {
                message_count: r.get::<_, u32>(1)?,
                hash: r.get(2)?,
                byte_length: DecimalU64(sql_u64(r.get::<_, i64>(3)?, 3)?),
            },
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub(super) fn capture_manifest(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    edits: Option<&[MessageEdit]>,
) -> StoreResult<UnitValue> {
    #[cfg(not(test))]
    {
        capture(tx, generation, character, conversation, edits)
    }
    #[cfg(test)]
    {
        let mut work = PageWork::default();
        let result = capture(tx, generation, character, conversation, edits, &mut work);
        record_capture_work(&work, result.is_ok());
        result
    }
}

#[cfg(test)]
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct PageWork {
    pub messages_read: usize,
    pub hash_rows_read: usize,
    pub bytes_read: u64,
    pub pages_written: usize,
    pub prefix_reused: usize,
    pub suffix_reused: usize,
    pub peak_page_messages: usize,
    pub peak_page_bytes: usize,
}

#[cfg(test)]
#[derive(Debug, Default, Eq, PartialEq)]
pub(super) struct CaptureWork {
    pub capture_calls: usize,
    pub successful_captures: usize,
    pub failed_captures: usize,
    pub work: PageWork,
}

#[cfg(test)]
std::thread_local! {
    static CAPTURE_WORK: std::cell::RefCell<CaptureWork> = std::cell::RefCell::new(CaptureWork::default());
}

#[cfg(test)]
pub(super) fn reset_capture_work() {
    CAPTURE_WORK.with(|slot| *slot.borrow_mut() = CaptureWork::default());
}

#[cfg(test)]
pub(super) fn take_capture_work() -> CaptureWork {
    CAPTURE_WORK.with(|slot| std::mem::take(&mut *slot.borrow_mut()))
}

#[cfg(test)]
fn record_capture_work(work: &PageWork, succeeded: bool) {
    CAPTURE_WORK.with(|slot| {
        let mut totals = slot.borrow_mut();
        totals.capture_calls += 1;
        totals.successful_captures += usize::from(succeeded);
        totals.failed_captures += usize::from(!succeeded);
        totals.work.messages_read += work.messages_read;
        totals.work.hash_rows_read += work.hash_rows_read;
        totals.work.bytes_read += work.bytes_read;
        totals.work.pages_written += work.pages_written;
        totals.work.prefix_reused += work.prefix_reused;
        totals.work.suffix_reused += work.suffix_reused;
        totals.work.peak_page_messages = totals.work.peak_page_messages.max(work.peak_page_messages);
        totals.work.peak_page_bytes = totals.work.peak_page_bytes.max(work.peak_page_bytes);
    });
}

#[cfg(test)]
pub(super) fn capture_with_work(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    edits: Option<&[MessageEdit]>,
) -> StoreResult<(UnitValue, PageWork)> {
    let mut work = PageWork::default();
    let value = capture(tx, generation, character, conversation, edits, &mut work)?;
    Ok((value, work))
}

/// Drops a conversation's page index and manifest, so the next capture pages
/// its messages again.
pub(super) fn forget_pages(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
) -> StoreResult<()> {
    tx.execute("DELETE FROM message_page_indexes WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
    tx.execute("DELETE FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
    Ok(())
}

// Live stores normalize only the active generation; copied databases validate all generations.
pub(super) fn forget_orphaned_pages(tx: &Transaction<'_>, generation: Option<&str>) -> StoreResult<()> {
    let scope = if generation.is_some() { "generation=?1 AND " } else { "" };
    for table in ["message_page_indexes", "message_page_manifests"] {
        let sql = format!("DELETE FROM {table} WHERE {scope}NOT EXISTS(
            SELECT 1 FROM conversations c WHERE c.generation={table}.generation
            AND c.character_id={table}.character_id AND c.conversation_id={table}.conversation_id)");
        if let Some(generation) = generation {
            tx.execute(&sql, [generation])?;
        } else {
            tx.execute(&sql, [])?;
        }
    }
    Ok(())
}

pub(super) fn current_manifest(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
) -> StoreResult<UnitValue> {
    let body: Option<Vec<u8>> = tx
        .query_row(
            "SELECT body FROM message_page_manifests WHERE generation=?1 AND character_id=?2
        AND conversation_id=?3",
            params![generation, character, conversation],
            |r| r.get(0),
        )
        .optional()?;
    match body {
        Some(body) => unit_value(tx, &{
            let result = MessageManifest::decode(&body);
            #[cfg(test)]
            crate::persistent_store::hash_work::decoded("native_manifest_decode_identity", &body, &result);
            result
        }.map_err(codec_error)?),
        None => capture_manifest(tx, generation, character, conversation, None),
    }
}

fn capture(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    edits: Option<&[MessageEdit]>,
    #[cfg(test)] work: &mut PageWork,
) -> StoreResult<UnitValue> {
    let count: Option<i64> = tx
        .query_row(
            "SELECT message_count FROM conversations WHERE generation=?1 AND character_id=?2
        AND conversation_id=?3",
            params![generation, character, conversation],
            |r| r.get(0),
        )
        .optional()?;
    let Some(count) = count else {
        tx.execute("DELETE FROM message_page_indexes WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
        tx.execute("DELETE FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
        return Ok(UnitValue::Deleted);
    };
    if count < 0 {
        return Err(codec_error("negative conversation message count"));
    }
    let count_usize = usize::try_from(count).map_err(codec_error)?;
    let previous = load_pages(tx, generation, character, conversation)?;
    let edits = edits.filter(|_| !previous.is_empty());
    let regions = match edits {
        Some(edits) => {
            let old_count = previous
                .iter()
                .try_fold(0usize, |count, p| count.checked_add(p.page.message_count as usize))
                .ok_or_else(|| codec_error("message page count overflow"))?;
            Some(dirty_regions(old_count, count_usize, edits)?)
        }
        None => None,
    };
    let restart = |old_start: usize| {
        previous
            .partition_point(|p| p.start < old_start)
            .saturating_sub(1)
    };
    let (mut pages, mut position) = match regions.as_deref() {
        Some([first, ..]) => {
            let prefix = restart(first.old_start);
            (previous[..prefix].to_vec(), previous.get(prefix).map_or(0, |p| p.start))
        }
        Some([]) => (previous.clone(), count_usize),
        None => (Vec::new(), 0),
    };
    #[cfg(test)]
    {
        work.prefix_reused = pages.len();
    }
    let mut region_index = 0;
    let mut statement = tx.prepare("SELECT message_index,canonical_hash,canonical_size FROM messages
        WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index>=?4 ORDER BY message_index")?;
    'walk: while position < count_usize {
        let mut rows = statement.query(params![
            generation,
            character,
            conversation,
            position as i64
        ])?;
        let mut next = next_hash(tx, &mut rows, generation, character, conversation)?;
        #[cfg(test)]
        {
            work.hash_rows_read += usize::from(next.is_some());
        }
        while let Some((_, _)) = &next {
            let start = position;
            let mut bytes = PAGE_PREFIX.to_vec();
            let mut page_hashes = Vec::new();
            loop {
                let (index, hash) = next
                    .take()
                    .ok_or_else(|| codec_error("message page index is incomplete"))?;
                if index != position {
                    return Err(codec_error("noncontiguous conversation messages"));
                }
                let body: String = tx.query_row(
                    "SELECT value FROM messages WHERE generation=?1 AND character_id=?2
                    AND conversation_id=?3 AND message_index=?4",
                    params![generation, character, conversation, position as i64],
                    |r| r.get(0),
                )?;
                let body = payload_value::canonicalize(body.as_bytes()).map_err(codec_error)?;
                if {
                    #[cfg(test)]
                    crate::persistent_store::hash_work::observe("native_message_verify", body.len());
                    MessageHash::from_bytes(&body)
                } != hash {
                    return Err(codec_error("cached message hash mismatch"));
                }
                if position > start {
                    bytes.push(b',');
                }
                bytes.extend(&body);
                page_hashes.push(hash.clone());
                #[cfg(test)]
                {
                    work.messages_read += 1;
                }
                #[cfg(test)]
                {
                    work.bytes_read += body.len() as u64;
                    work.peak_page_messages = work.peak_page_messages.max(page_hashes.len());
                    work.peak_page_bytes = work.peak_page_bytes.max(bytes.len() + PAGE_SUFFIX.len());
                }
                position += 1;
                next = next_hash(tx, &mut rows, generation, character, conversation)?;
                #[cfg(test)]
                {
                    work.hash_rows_read += usize::from(next.is_some());
                }
                let page_count = position - start;
                if page_count == LOGICAL_MESSAGE_PAGE_SIZE
                    || (page_count >= MIN_PAGE_MESSAGES && hash.boundary().map_err(codec_error)?)
                    || bytes.len() + PAGE_SUFFIX.len() > MAX_PAGE_BYTES
                    || next.is_none()
                    || next.as_ref().is_some_and(|(_, h)| {
                        bytes.len() as u64 + 1 + h.byte_length + PAGE_SUFFIX.len() as u64
                            > MAX_PAGE_BYTES as u64
                    })
                {
                    break;
                }
            }
            bytes.extend(PAGE_SUFFIX);
            let object = {
                let result = encoded_object(bytes);
                #[cfg(test)]
                crate::persistent_store::hash_work::encoded_object("native_page_identity", &result);
                result
            }.map_err(codec_error)?;
            let boundary = PageBoundary {
                start,
                page: ManifestPage {
                    hash: object.hash.clone(),
                    message_count: (position - start) as u32,
                    byte_length: DecimalU64(object.size),
                },
            };
            // Once a recomputed page lines up with an old page past the changed
            // range, the old pages up to the next changed range are reused.
            if let Some(regions) = regions.as_deref() {
                while regions.get(region_index + 1).is_some_and(|next| next.new_start <= start) {
                    region_index += 1;
                }
                let region = regions[region_index];
                if start >= region.new_end {
                    let old_start = start - region.new_end + region.old_end;
                    if let Ok(old_page) = previous.binary_search_by_key(&old_start, |p| p.start) {
                        let limit = regions
                            .get(region_index + 1)
                            .map_or(previous.len(), |next| restart(next.old_start));
                        if old_page < limit && boundary.page == previous[old_page].page {
                            for old in &previous[old_page..limit] {
                                let mut page = old.clone();
                                page.start = page.start - region.old_end + region.new_end;
                                pages.push(page);
                                #[cfg(test)]
                                {
                                    work.suffix_reused += 1;
                                }
                            }
                            if limit == previous.len() {
                                position = count_usize;
                                break 'walk;
                            }
                            position = previous[limit].start - region.old_end + region.new_end;
                            region_index += 1;
                            continue 'walk;
                        }
                    }
                }
            }
            put_object(tx, &object.hash, &object.bytes)?;
            store_page_proof(tx, &boundary.page, &page_hashes)?;
            pages.push(boundary);
            #[cfg(test)]
            {
                work.pages_written += 1;
            }
        }
        break;
    }
    if position != count_usize {
        return Err(codec_error("conversation message count mismatch"));
    }
    let manifest = MessageManifest {
        schema: MANIFEST_SCHEMA.into(),
        message_count: DecimalU64(count as u64),
        pages: pages.iter().map(|p| p.page.clone()).collect(),
    };
    save_index(tx, generation, character, conversation, &pages, &manifest)?;
    unit_value(tx, &manifest)
}

pub(super) fn stage_conversation_pages(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    check_cancelled: &dyn Fn() -> StoreResult<()>,
) -> StoreResult<()> {
    #[cfg(not(test))]
    { stage_pages(tx, generation, character, conversation, check_cancelled) }
    #[cfg(test)]
    {
        let mut work = PageWork::default();
        let result = stage_pages(tx, generation, character, conversation, check_cancelled, &mut work);
        record_capture_work(&work, result.is_ok());
        result
    }
}

fn stage_pages(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    check_cancelled: &dyn Fn() -> StoreResult<()>,
    #[cfg(test)] work: &mut PageWork,
) -> StoreResult<()> {
    check_cancelled()?;
    let expected: Option<i64> = tx.query_row(
        "SELECT message_count FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![generation, character, conversation], |row| row.get(0),
    ).optional()?;
    let Some(expected) = expected else { return Ok(()); };
    let available: i64 = tx.query_row(
        "SELECT COALESCE(MAX(message_index)+1,0) FROM messages
         WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![generation, character, conversation], |row| row.get(0),
    )?;
    if expected < 0 || available < 0 || available > expected {
        return Err(codec_error("conversation message count mismatch"));
    }
    // Only the trailing page can change when another batch arrives. Its size
    // boundary can depend on the first message of that next batch.
    let mut position: i64 = tx.query_row(
        "SELECT COALESCE(MAX(page_start),0) FROM message_page_indexes
         WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![generation, character, conversation], |row| row.get(0),
    )?;
    tx.execute("DELETE FROM message_page_indexes WHERE generation=?1 AND character_id=?2
        AND conversation_id=?3 AND page_start>=?4", params![generation, character, conversation, position])?;
    tx.execute("DELETE FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![generation, character, conversation])?;
    let mut statement = tx.prepare("SELECT message_index,canonical_hash,canonical_size FROM messages
        WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index>=?4 ORDER BY message_index")?;
    let mut rows = statement.query(params![generation, character, conversation, position])?;
    let mut next = next_hash(tx, &mut rows, generation, character, conversation)?;
    #[cfg(test)]
    { work.hash_rows_read += usize::from(next.is_some()); }
    while next.is_some() {
        check_cancelled()?;
        let start = position;
        let mut bytes = PAGE_PREFIX.to_vec();
        let mut hashes = Vec::new();
        loop {
            check_cancelled()?;
            let (index, hash) = next.take().ok_or_else(|| codec_error("message page index is incomplete"))?;
            if index as i64 != position {
                return Err(codec_error("noncontiguous conversation messages"));
            }
            let body: String = tx.query_row("SELECT value FROM messages WHERE generation=?1 AND character_id=?2
                AND conversation_id=?3 AND message_index=?4", params![generation, character, conversation, position],
                |row| row.get(0))?;
            let body = payload_value::canonicalize(body.as_bytes()).map_err(codec_error)?;
            if {
                #[cfg(test)]
                crate::persistent_store::hash_work::observe("native_message_verify", body.len());
                MessageHash::from_bytes(&body)
            } != hash {
                return Err(codec_error("cached message hash mismatch"));
            }
            if position > start { bytes.push(b','); }
            bytes.extend(&body);
            hashes.push(hash.clone());
            position += 1;
            next = next_hash(tx, &mut rows, generation, character, conversation)?;
            #[cfg(test)]
            {
                work.messages_read += 1;
                work.bytes_read += body.len() as u64;
                work.hash_rows_read += usize::from(next.is_some());
                work.peak_page_messages = work.peak_page_messages.max(hashes.len());
                work.peak_page_bytes = work.peak_page_bytes.max(bytes.len() + PAGE_SUFFIX.len());
            }
            if hashes.len() == LOGICAL_MESSAGE_PAGE_SIZE
                || (hashes.len() >= MIN_PAGE_MESSAGES && hash.boundary().map_err(codec_error)?)
                || bytes.len() + PAGE_SUFFIX.len() > MAX_PAGE_BYTES
                || next.is_none()
                || next.as_ref().is_some_and(|(_, hash)| {
                    bytes.len() as u128 + 1 + hash.byte_length as u128 + PAGE_SUFFIX.len() as u128 > MAX_PAGE_BYTES as u128
                })
            { break; }
        }
        bytes.extend(PAGE_SUFFIX);
        let object = {
            let result = encoded_object(bytes);
            #[cfg(test)]
            crate::persistent_store::hash_work::encoded_object("native_page_identity", &result);
            result
        }.map_err(codec_error)?;
        let page = ManifestPage {
            hash: object.hash.clone(), message_count: hashes.len() as u32, byte_length: DecimalU64(object.size),
        };
        put_object(tx, &object.hash, &object.bytes)?;
        store_page_proof(tx, &page, &hashes)?;
        tx.execute("INSERT INTO message_page_indexes VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![generation, character, conversation, start, page.message_count, page.hash, sql_i64(page.byte_length.0)?])?;
        #[cfg(test)]
        { work.pages_written += 1; }
    }
    if position != available {
        return Err(codec_error("conversation message count mismatch"));
    }
    check_cancelled()?;
    if available == expected {
        let manifest = MessageManifest {
            schema: MANIFEST_SCHEMA.into(), message_count: DecimalU64(expected as u64),
            pages: load_pages(tx, generation, character, conversation)?.into_iter().map(|page| page.page).collect(),
        };
        let encoded = manifest.encode().map_err(codec_error)?;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_manifest_identity", encoded.bytes.len());
        tx.execute("INSERT INTO message_page_manifests VALUES(?1,?2,?3,?4)",
            params![generation, character, conversation, encoded.bytes])?;
        unit_value(tx, &manifest)?;
    }
    Ok(())
}

fn next_hash(
    tx: &Transaction<'_>,
    rows: &mut rusqlite::Rows<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
) -> StoreResult<Option<(usize, MessageHash)>> {
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let index: i64 = row.get(0)?;
    let hash: String = row.get(1)?;
    let size = sql_u64(row.get::<_, i64>(2)?, 2)?;
    if index < 0 {
        return Err(codec_error("negative message index"));
    }
    if !hash.is_empty() {
        return Ok(Some((
            index as usize,
            MessageHash {
                hash,
                byte_length: size,
            },
        )));
    }
    // Upstream import staging may have inserted messages without a computed cache.
    let body: String = tx.query_row(
        "SELECT value FROM messages WHERE generation=?1 AND character_id=?2
        AND conversation_id=?3 AND message_index=?4",
        params![generation, character, conversation, index],
        |r| r.get(0),
    )?;
    let bytes = payload_value::canonicalize(body.as_bytes()).map_err(codec_error)?;
    let cached = {
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_message_verify", bytes.len());
        MessageHash::from_bytes(&bytes)
    };
    tx.execute("UPDATE messages SET canonical_hash=?5,canonical_size=?6 WHERE generation=?1 AND character_id=?2
        AND conversation_id=?3 AND message_index=?4",params![generation,character,conversation,index,cached.hash,sql_i64(cached.byte_length)?])?;
    Ok(Some((index as usize, cached)))
}

fn save_index(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    pages: &[PageBoundary],
    manifest: &MessageManifest,
) -> StoreResult<()> {
    manifest.validate().map_err(codec_error)?;
    let old = load_pages(tx, generation, character, conversation)?;
    for page in &old {
        if pages
            .binary_search_by_key(&page.start, |p| p.start)
            .is_err()
        {
            tx.execute("DELETE FROM message_page_indexes WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND page_start=?4",
                params![generation,character,conversation,page.start as i64])?;
        }
    }
    for page in pages {
        if old
            .binary_search_by_key(&page.start, |p| p.start)
            .is_ok_and(|i| old[i] == *page)
        {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO message_page_indexes VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                generation,
                character,
                conversation,
                page.start as i64,
                page.page.message_count,
                page.page.hash,
                sql_i64(page.page.byte_length.0)?
            ],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO message_page_manifests VALUES(?1,?2,?3,?4)",
        params![
            generation,
            character,
            conversation,
            {
                let result = manifest.encode();
                #[cfg(test)]
                crate::persistent_store::hash_work::encoded_object("native_manifest_identity", &result);
                result
            }.map_err(codec_error)?.bytes
        ],
    )?;
    Ok(())
}

#[derive(Debug, PartialEq)]
struct PageProof {
    message_count: u32,
    byte_length: u64,
    first_length: u64,
    ends_cut: bool,
}

fn derive_page_proof(page: &ManifestPage, hashes: &[MessageHash]) -> StoreResult<PageProof> {
    if hashes.is_empty() || hashes.len() != page.message_count as usize {
        return Err(codec_error("message page manifest length mismatch"));
    }
    let mut length = (PAGE_PREFIX.len() + PAGE_SUFFIX.len()) as u64;
    let mut ends_cut = false;
    for (offset, hash) in hashes.iter().enumerate() {
        length = length.checked_add(hash.byte_length)
            .and_then(|v| v.checked_add(u64::from(offset != 0)))
            .ok_or_else(|| codec_error("message page length overflow"))?;
        let count = offset + 1;
        ends_cut = count == LOGICAL_MESSAGE_PAGE_SIZE
            || (count >= MIN_PAGE_MESSAGES && hash.boundary().map_err(codec_error)?)
            || length > MAX_PAGE_BYTES as u64;
        if let Some(next) = hashes.get(offset + 1) {
            if ends_cut || length.checked_add(1).and_then(|v| v.checked_add(next.byte_length))
                .is_none_or(|v| v > MAX_PAGE_BYTES as u64) {
                return Err(codec_error("message page boundaries are not deterministic"));
            }
        }
    }
    if length != page.byte_length.0 {
        return Err(codec_error("message page manifest length mismatch"));
    }
    Ok(PageProof { message_count: page.message_count, byte_length: length,
        first_length: hashes[0].byte_length, ends_cut })
}

fn cached_page_proof(db: &Connection, hash: &str) -> StoreResult<Option<PageProof>> {
    Ok(db.query_row("SELECT p.message_count,p.byte_length,p.first_length,p.ends_cut
        FROM message_page_proofs p JOIN message_page_verified_objects v ON v.hash=p.hash
        JOIN message_page_objects o ON o.hash=p.hash WHERE p.hash=?1", [hash], |r| {
        Ok(PageProof { message_count: r.get(0)?, byte_length: sql_u64(r.get(1)?, 1)?,
            first_length: sql_u64(r.get(2)?, 2)?, ends_cut: r.get(3)? })
    }).optional()?)
}

// A proof certifies canonical bytes and every internal cut, not a page's
// position. The next page's first message still determines a size cut.
fn store_page_proof(db: &Connection, page: &ManifestPage, hashes: &[MessageHash]) -> StoreResult<PageProof> {
    let proof = derive_page_proof(page, hashes)?;
    if !verified_object_present(db, &page.hash)? {
        return Err(codec_error("message object is not verified"));
    }
    if let Some(existing) = cached_page_proof(db, &page.hash)? {
        if existing != proof {
            return Err(codec_error("message page proof mismatch"));
        }
        return Ok(existing);
    }
    db.execute("INSERT INTO message_page_proofs VALUES(?1,?2,?3,?4,?5,?6)", params![
        page.hash, proof.message_count, sql_i64(proof.byte_length)?, sql_i64(proof.first_length)?,
        proof.ends_cut, serde_json::to_string(hashes)?])?;
    Ok(proof)
}

fn ensure_page_proof(db: &Connection, page: &ManifestPage) -> StoreResult<PageProof> {
    if let Some(proof) = cached_page_proof(db, &page.hash)? {
        if proof.message_count != page.message_count || proof.byte_length != page.byte_length.0 {
            return Err(codec_error("message page manifest length mismatch"));
        }
        return Ok(proof);
    }
    let body = require_object(db, &page.hash)?;
    let decoded = {
        let result = decode_message_page(&body);
        #[cfg(test)]
        crate::persistent_store::hash_work::decoded("native_page_decode_identity", &body, &result);
        result
    }.map_err(codec_error)?;
    if decoded.len() != page.message_count as usize || body.len() as u64 != page.byte_length.0 {
        return Err(codec_error("message page manifest length mismatch"));
    }
    let hashes = decoded.iter().map(|value| {
        let body = payload_value::encode(value).map_err(codec_error)?;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_message_verify", body.len());
        Ok(MessageHash::from_bytes(&body))
    }).collect::<StoreResult<Vec<_>>>()?;
    derive_page_proof(page, &hashes)?;
    if !verified_object_present(db, &page.hash)? {
        db.execute("INSERT INTO message_page_verified_objects(hash) VALUES(?1)", [&page.hash])?;
    }
    store_page_proof(db, page, &hashes)
}

fn validated_manifest(
    db: &Connection,
    value: &UnitValue,
) -> StoreResult<(MessageManifest, Vec<PageBoundary>)> {
    {
        let result = value.validate();
        #[cfg(test)]
        crate::persistent_store::hash_work::validation(&value);
        result
    }.map_err(codec_error)?;
    let UnitValue::Object {
        descriptor: received,
        ..
    } = value
    else {
        return Err(codec_error("messages require an object manifest"));
    };
    let body = require_object(db, &received.object_hash)?;
    let manifest = {
        let result = MessageManifest::decode(&body);
        #[cfg(test)]
        crate::persistent_store::hash_work::decoded("native_manifest_decode_identity", &body, &result);
        result
    }.map_err(codec_error)?;
    let (expected, references) = descriptor(&manifest)?;
    if received != &expected {
        return Err(codec_error("message manifest dependencies mismatch"));
    }
    for (hash, body) in references {
        if require_object(db, &hash)? != body {
            return Err(codec_error("message reference tree mismatch"));
        }
    }
    let mut pages = Vec::new();
    let mut previous: Option<PageProof> = None;
    let mut position = 0usize;
    for page in &manifest.pages {
        let proof = ensure_page_proof(db, page)?;
        if let Some(previous) = previous {
            if !previous.ends_cut && previous.byte_length.checked_add(1)
                .and_then(|v| v.checked_add(proof.first_length))
                .is_some_and(|v| v <= MAX_PAGE_BYTES as u64) {
                return Err(codec_error("message page boundaries are not deterministic"));
            }
        }
        pages.push(PageBoundary {
            start: position,
            page: page.clone(),
        });
        position = position.checked_add(page.message_count as usize)
            .ok_or_else(|| codec_error("message count overflow"))?;
        previous = Some(proof);
    }
    Ok((manifest, pages))
}

pub(super) fn validate_manifest(db: &Connection, value: &UnitValue) -> StoreResult<()> {
    validated_manifest(db, value).map(|_| ())
}

// Copied SQLite files cannot supply certificates for their own contents.
pub(super) fn accept_copied_database(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("DELETE FROM message_page_proofs; DELETE FROM message_page_verified_objects;
        DELETE FROM message_page_object_marks; DELETE FROM message_page_sweep_cursor;")?;
    let mut messages = tx.prepare("SELECT generation,character_id,conversation_id,message_index,value,canonical_hash,canonical_size FROM messages")?;
    let mut rows = messages.query([])?;
    while let Some(row) = rows.next()? {
        let text: String = row.get(4)?;
        let value: Value = serde_json::from_str(&text)?;
        let bytes = payload_value::encode(&value).map_err(codec_error)?;
        let hash = MessageHash::from_bytes(&bytes);
        let imported_hash: String = row.get(5)?;
        let imported_size: i64 = row.get(6)?;
        if !imported_hash.is_empty() && (imported_hash != hash.hash || imported_size != sql_i64(hash.byte_length)?) {
            return Err(codec_error("copied native message identity mismatch"));
        }
        tx.execute("UPDATE messages SET canonical_hash=?5,canonical_size=?6 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index=?4",
            params![row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,hash.hash,sql_i64(hash.byte_length)?])?;
    }
    drop(rows);
    drop(messages);
    let invalid_parent: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages m LEFT JOIN conversations c ON c.generation=m.generation AND c.character_id=m.character_id AND c.conversation_id=m.conversation_id WHERE c.conversation_id IS NULL)",[],|r|r.get(0))?;
    if invalid_parent { return Err(codec_error("copied native message parent is missing")); }
    let invalid_order: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations c WHERE c.message_count!=(SELECT COUNT(*) FROM messages m WHERE m.generation=c.generation AND m.character_id=c.character_id AND m.conversation_id=c.conversation_id) OR (c.message_count>0 AND ((SELECT MIN(message_index) FROM messages m WHERE m.generation=c.generation AND m.character_id=c.character_id AND m.conversation_id=c.conversation_id)!=0 OR (SELECT MAX(message_index) FROM messages m WHERE m.generation=c.generation AND m.character_id=c.character_id AND m.conversation_id=c.conversation_id)!=c.message_count-1)))",[],|r|r.get(0))?;
    if invalid_order { return Err(codec_error("copied native message indices or count mismatch")); }
    let invalid_id: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE message_id IS NOT json_extract(value,'$.chatId'))",[],|r|r.get(0))?;
    if invalid_id { return Err(codec_error("copied native message chatId mismatch")); }
    let orphan: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM message_page_indexes i LEFT JOIN message_page_manifests m
        ON m.generation=i.generation AND m.character_id=i.character_id AND m.conversation_id=i.conversation_id WHERE m.body IS NULL)",[],|r|r.get(0))?;
    if orphan { return Err(codec_error("copied page index has no manifest")); }
    // Older stores kept these derived rows after deleting their conversations.
    forget_orphaned_pages(tx, None)?;
    let mut manifests = tx.prepare("SELECT generation,character_id,conversation_id,body FROM message_page_manifests")?;
    let entries = manifests.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Vec<u8>>(3)?)))?
        .collect::<Result<Vec<_>,_>>()?;
    drop(manifests);
    for (generation,character,conversation,body) in entries {
        let original = MessageManifest::decode(&body).map_err(codec_error)?;
        let original_pages = load_pages(tx,&generation,&character,&conversation)?;
        let original_value = unit_value(tx,&original)?;
        let (_, expected_pages) = validated_manifest(tx,&original_value)?;
        if original_pages != expected_pages { return Err(codec_error("copied page index mismatch")); }
        tx.execute("DELETE FROM message_page_indexes WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
        tx.execute("DELETE FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation])?;
        let actual = capture_manifest(tx,&generation,&character,&conversation,None)?;
        if actual != original_value { return Err(codec_error("copied message manifest differs from native messages")); }
    }
    for table in ["lww_units", "lww_outbox", "lww_receive_rows", "lww_binding_source_units"] {
        let identity_column = if matches!(table,"lww_units"|"lww_outbox") { "identity" } else { "NULL" };
        let mut statement = tx.prepare(&format!("SELECT key,value,stamp,{identity_column} FROM {table}"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let key: risunest_sync_wire::unit::UnitKey = row.get::<_,String>(0)?.try_into().map_err(codec_error)?;
            let value: UnitValue = serde_json::from_str(&row.get::<_,String>(1)?)?;
            value.validate().map_err(codec_error)?;
            let stamp: risunest_sync_wire::stamp::Stamp = serde_json::from_str(&row.get::<_,String>(2)?)?;
            stamp.validate().map_err(codec_error)?;
            if let Some(identity) = row.get::<_,Option<String>>(3)? {
                let bytes = risunest_sync_wire::canonical::encode(&value).map_err(codec_error)?;
                if risunest_sync_wire::hash(&bytes)!=identity { return Err(codec_error("copied native unit identity mismatch")); }
            }
            super::lww::validate_received(tx,&key,&value)?;
            if key.components()[0]=="messages" && !matches!(value,UnitValue::Deleted) {
                let (manifest,_) = validated_manifest(tx,&value)?;
                unit_value(tx,&manifest)?;
            }
        }
    }
    Ok(())
}

pub(super) fn apply_manifest(
    tx: &Transaction<'_>,
    generation: &str,
    character: &str,
    conversation: &str,
    value: &UnitValue,
) -> StoreResult<()> {
    let (manifest, pages) = validated_manifest(tx, value)?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3)",
        params![generation,character,conversation],|r|r.get(0))?;
    if !exists {
        return Err(codec_error("message manifest parent is missing"));
    }
    let old = load_pages(tx, generation, character, conversation)?;
    let prefix = old.iter().zip(&pages).take_while(|(a,b)| a == b).count();
    let suffix = old[prefix..].iter().rev().zip(pages[prefix..].iter().rev())
        .take_while(|(a,b)| a.page == b.page).count();
    let mut start = pages.get(prefix).map_or(manifest.message_count.0, |p| p.start as u64);
    let old_count = if let Some(last) = old.last() {
        last.start as u64 + last.page.message_count as u64
    } else {
        sql_u64(tx.query_row("SELECT message_count FROM conversations WHERE generation=?1
            AND character_id=?2 AND conversation_id=?3", params![generation,character,conversation],
            |r| r.get(0))?, 0)?
    };
    let mut old_end = old.get(old.len()-suffix).map_or(old_count, |p| p.start as u64);
    let mut new_end = pages.get(pages.len()-suffix).map_or(manifest.message_count.0, |p| p.start as u64);
    let mut replacement = Vec::new();
    for page in &pages[prefix..pages.len()-suffix] {
        let (body, hashes): (Vec<u8>, String) = tx.query_row("SELECT o.body,p.message_hashes
            FROM message_page_objects o JOIN message_page_proofs p ON p.hash=o.hash
            JOIN message_page_verified_objects v ON v.hash=o.hash WHERE o.hash=?1", [&page.page.hash],
            |r| Ok((r.get(0)?, r.get(1)?)))?;
        // These exact immutable bytes already passed canonical decoding and hash
        // verification. Only changed pages are materialized into native rows.
        let document: Value = serde_json::from_slice(&body)?;
        let messages = document["messages"].as_array().ok_or_else(|| codec_error("invalid verified page"))?;
        let hashes: Vec<MessageHash> = serde_json::from_str(&hashes)?;
        if messages.len() != hashes.len() || messages.len() != page.page.message_count as usize {
            return Err(codec_error("message page proof mismatch"));
        }
        for (value, hash) in messages.iter().zip(hashes) {
            let body = payload_value::encode(value).map_err(codec_error)?;
            replacement.push((value.get("chatId").and_then(Value::as_str).map(str::to_owned),
                String::from_utf8(body).map_err(codec_error)?, hash));
        }
    }
    // A changed page can contain unchanged messages. Compare only the affected
    // native range's stored identities to preserve its leading/trailing rows.
    let mut leading = 0;
    while leading < replacement.len() && start < old_end {
        if native_message_hash(tx, generation, character, conversation, start)? != replacement[leading].2 {
            break;
        }
        start += 1;
        leading += 1;
    }
    let mut trailing = 0;
    while trailing < replacement.len()-leading && old_end > start {
        if native_message_hash(tx, generation, character, conversation, old_end-1)?
            != replacement[replacement.len()-trailing-1].2 {
            break;
        }
        old_end -= 1;
        new_end -= 1;
        trailing += 1;
    }
    tx.execute("DELETE FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3
        AND message_index>=?4 AND message_index<?5",
        params![generation,character,conversation,sql_i64(start)?,sql_i64(old_end)?])?;
    let delta = sql_i64(new_end)? - sql_i64(old_end)?;
    if delta != 0 {
        tx.execute("UPDATE messages SET message_index=-(message_index+?4)-1
            WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index>=?5",
            params![generation,character,conversation,delta,sql_i64(old_end)?])?;
        tx.execute("UPDATE messages SET message_index=-message_index-1
            WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND message_index<0",
            params![generation,character,conversation])?;
    }
    for (offset, (id, body, hash)) in replacement[leading..replacement.len()-trailing].iter().enumerate() {
        tx.execute("INSERT INTO messages(generation,character_id,conversation_id,message_index,message_id,value,canonical_hash,canonical_size)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![generation,character,conversation,
                sql_i64(start+offset as u64)?,id,body,hash.hash,sql_i64(hash.byte_length)?])?;
    }
    tx.execute("UPDATE conversations SET message_count=?4 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",
        params![generation,character,conversation,sql_i64(manifest.message_count.0)?])?;
    save_index(tx, generation, character, conversation, &pages, &manifest)
}

fn native_message_hash(db: &Connection, generation: &str, character: &str,
    conversation: &str, index: u64) -> StoreResult<MessageHash> {
    Ok(db.query_row("SELECT canonical_hash,canonical_size FROM messages WHERE generation=?1
        AND character_id=?2 AND conversation_id=?3 AND message_index=?4",
        params![generation,character,conversation,sql_i64(index)?], |r| {
            Ok(MessageHash { hash: r.get(0)?, byte_length: sql_u64(r.get(1)?, 1)? })
        })?)
}

impl From<risunest_external_storage_format::logical_records::LogicalRecordError> for StoreError {
    fn from(error: risunest_external_storage_format::logical_records::LogicalRecordError) -> Self {
        codec_error(error)
    }
}

fn sql_i64(value: u64) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| codec_error("message size exceeds database integer range"))
}
/// Roots and traversal state live in a file-backed SQLite scratch
/// database. Each sweep target has a bounded page cache, independent of its roots.
pub(super) struct ObjectRoots {
    scratch: Connection,
}

impl ObjectRoots {
    fn new(scratch: Connection) -> StoreResult<Self> {
        scratch.execute_batch("PRAGMA cache_size=-512; PRAGMA mmap_size=0;
            PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_spill=ON;
            CREATE TABLE roots(hash TEXT PRIMARY KEY, walked INTEGER NOT NULL DEFAULT 0,
                pending INTEGER) WITHOUT ROWID;
            CREATE INDEX pending_roots ON roots(pending,hash) WHERE pending IS NOT NULL;
            BEGIN IMMEDIATE;")?;
        Ok(Self { scratch })
    }

    fn insert(&mut self, hash: impl AsRef<str>) -> StoreResult<()> {
        self.scratch.prepare_cached("INSERT OR IGNORE INTO roots(hash) VALUES(?1)")?
            .execute([hash.as_ref()])?;
        Ok(())
    }

    fn enqueue(&mut self, hash: &str, relations: bool) -> StoreResult<()> {
        self.scratch.prepare_cached("INSERT INTO roots(hash,pending) VALUES(?1,?2)
            ON CONFLICT(hash) DO UPDATE SET pending=excluded.pending WHERE walked=0")?
            .execute(params![hash, relations])?;
        Ok(())
    }

    fn value(&mut self, objects: &Connection, value: &UnitValue) -> StoreResult<()> {
        let UnitValue::Object { descriptor_hash, descriptor } = value else {
            return Ok(());
        };
        self.insert(descriptor_hash)?;
        self.insert(&descriptor.object_hash)?;
        for hash in &descriptor.dependencies {
            self.insert(hash)?;
        }
        for (root, relations) in [(&descriptor.dependency_root, false), (&descriptor.relation_root, true)] {
            if let Some(root) = root {
                self.tree(objects, root, relations)?;
            }
        }
        Ok(())
    }

    fn tree(&mut self, objects: &Connection, root: &str, relations: bool) -> StoreResult<()> {
        use risunest_sync_wire::descriptor::ReferencePage;
        self.enqueue(root, relations)?;
        loop {
            let pending: Option<(String, bool)> = self.scratch.prepare_cached(
                "SELECT hash,pending FROM roots WHERE pending IS NOT NULL ORDER BY pending,hash LIMIT 1"
            )?.query_row([], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
            let Some((hash, relations)) = pending else { break; };
            self.scratch.prepare_cached("UPDATE roots SET walked=1,pending=NULL WHERE hash=?1")?
                .execute([&hash])?;
            // A node this store does not hold has nothing below it here. A node
            // it holds but cannot read stops the sweep, since what it names is
            // unknown.
            let Some(body) = object_body(objects, &hash)? else {
                continue;
            };
            let page = risunest_sync_wire::canonical::decode::<ReferencePage>(&body, risunest_sync_wire::MAX_METADATA_BYTES)
                .map_err(codec_error)?;
            match page {
                ReferencePage::Branches { children } => {
                    for child in children { self.enqueue(&child, relations)?; }
                }
                ReferencePage::Objects { hashes } if !relations => {
                    for hash in hashes { self.insert(hash)?; }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Roots every unit value found anywhere inside a stored JSON body.
    fn json(&mut self, objects: &Connection, value: &Value) -> StoreResult<()> {
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            match value {
                Value::Object(map) => {
                    if let Some(plugin) = map.get("PluginObject").or_else(|| map.get("PluginBinary")) {
                        let hash = plugin.get("object").and_then(|object| object.get("hash")).and_then(Value::as_str)
                            .ok_or_else(|| codec_error("plugin object is missing"))?;
                        risunest_sync_wire::validate_hash(hash).map_err(codec_error)?;
                        self.insert(hash)?;
                    }
                    if map.get("kind").and_then(Value::as_str) == Some("object") && map.contains_key("descriptorHash") {
                        let unit = serde_json::from_value::<UnitValue>(value.clone())?;
                        self.value(objects, &unit)?;
                        continue;
                    }
                    pending.extend(map.values());
                }
                Value::Array(items) => pending.extend(items),
                _ => {}
            }
        }
        Ok(())
    }

    fn contains(&self, hash: &str) -> StoreResult<bool> {
        Ok(self.scratch.prepare_cached("SELECT EXISTS(SELECT 1 FROM roots WHERE hash=?1)")?
            .query_row([hash], |row| row.get(0))?)
    }
}

/// Every hash that a library row, a revision lease's view of the library or of
/// the device it pinned, or a device row references. The result roots the sweep
/// of either store; tree nodes are read from `objects`, the store being swept.
pub(super) fn object_roots<'a>(
    library: &Connection,
    leases: impl IntoIterator<Item = &'a Connection>,
    device: &Connection,
    objects: &Connection,
    scratch: Connection,
) -> StoreResult<ObjectRoots> {
    let mut roots = ObjectRoots::new(scratch)?;
    library_object_roots(library, objects, &mut roots)?;
    for lease in leases {
        library_object_roots(lease, objects, &mut roots)?;
        let pinned: bool = lease.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name='backup_device')",
            [],
            |row| row.get(0),
        )?;
        if pinned {
            device_object_roots(lease, "backup_device", objects, &mut roots)?;
        }
    }
    device_object_roots(device, "main", objects, &mut roots)?;
    roots.scratch.execute_batch("COMMIT")?;
    Ok(roots)
}

/// Collects the objects that library rows visible on `db` reference: page
/// indexes and manifests of every generation, unit values in every unit table,
/// and hashes kept by staged restores and external storage captures.
fn library_object_roots(db: &Connection, objects: &Connection, roots: &mut ObjectRoots) -> StoreResult<()> {
    for sql in [
        "SELECT hash FROM message_page_indexes",
        "SELECT manifest_hash FROM external_storage_captures",
        "SELECT file_hash FROM external_storage_capture_files",
        "SELECT hash FROM snapshot_restore_payloads",
    ] {
        let mut statement = db.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            roots.insert(row?)?;
        }
    }
    let mut statement = db.prepare("SELECT body FROM message_page_manifests")?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    for row in rows {
        roots.insert(risunest_sync_wire::hash(&row?))?;
    }
    drop(statement);
    for sql in [
        "SELECT value FROM lww_units WHERE json_extract(value,'$.kind')='object'",
        "SELECT value FROM lww_outbox WHERE json_extract(value,'$.kind')='object'",
        "SELECT value FROM lww_receive_rows WHERE status<>'done' AND json_extract(value,'$.kind')='object'",
        "SELECT value FROM lww_binding_source_units WHERE json_extract(value,'$.kind')='object'",
        "SELECT value FROM snapshot_original_units WHERE json_extract(value,'$.kind')='object'",
        "SELECT value FROM replacement_source_units WHERE json_extract(value,'$.kind')='object'",
    ] {
        unit_value_roots(db, objects, sql, roots)?;
    }
    let mut statement = db.prepare("SELECT archived_object FROM characters WHERE archived_object IS NOT NULL")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let archived: super::archive::ArchivedObject = serde_json::from_str(&row?)?;
        for hash in super::lww::archive_object_hashes(&archived)? {
            roots.insert(hash)?;
        }
    }
    Ok(())
}

/// Collects the objects that unfinished device rows in `schema` on `db`
/// reference: device units, staged receives, unfinished intents with their
/// rows, and unpublished proofs.
fn device_object_roots(db: &Connection, schema: &str, objects: &Connection, roots: &mut ObjectRoots) -> StoreResult<()> {
    for sql in [
        format!("SELECT value FROM {schema}.lww_units WHERE json_extract(value,'$.kind')='object'"),
        format!("SELECT value FROM {schema}.lww_outbox WHERE json_extract(value,'$.kind')='object'"),
        format!("SELECT value FROM {schema}.lww_receive_rows WHERE status<>'done' AND json_extract(value,'$.kind')='object'"),
        format!("SELECT r.value FROM {schema}.lww_intents i JOIN {schema}.lww_intent_rows r ON r.request_id=i.request_id WHERE i.complete=0 AND json_extract(r.value,'$.kind')='object'"),
    ] {
        unit_value_roots(db, objects, &sql, roots)?;
    }
    for sql in [
        format!("SELECT body FROM {schema}.lww_receive WHERE finished=0"),
        format!("SELECT body FROM {schema}.lww_intents WHERE complete=0"),
        format!("SELECT entries FROM {schema}.lww_unpublished_proofs"),
    ] {
        let mut statement = db.prepare(&sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            let body: Value = serde_json::from_str(&row?)?;
            roots.json(objects, &body)?;
        }
    }
    Ok(())
}

fn unit_value_roots(db: &Connection, objects: &Connection, sql: &str, roots: &mut ObjectRoots) -> StoreResult<()> {
    let mut statement = db.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let value: UnitValue = serde_json::from_str(&row?)?;
        roots.value(objects, &value)?;
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ObjectSweep {
    pub(crate) marked: u64,
    pub(crate) deleted: u64,
    pub(crate) deleted_bytes: u64,
    /// The pass reached the last object and starts from the first one next time.
    pub(crate) wrapped: bool,
}

/// Visits up to `limit` objects of the store that `tx` writes, after its stored
/// cursor. An object nothing references is marked the first time and deleted
/// once it has stayed unreferenced for `grace_ms`; a referenced object loses
/// its mark.
pub(super) fn sweep_objects(
    tx: &Transaction<'_>,
    roots: &ObjectRoots,
    now_ms: i64,
    grace_ms: i64,
    limit: usize,
) -> StoreResult<ObjectSweep> {
    let after: Option<String> = tx
        .query_row("SELECT after_hash FROM message_page_sweep_cursor WHERE singleton=1", [], |row| row.get(0))
        .optional()?;
    let candidates: Vec<(String, i64, Option<i64>)> = {
        let mut statement = tx.prepare(
            "SELECT o.hash,length(o.body),m.unreferenced_since FROM message_page_objects o
            LEFT JOIN message_page_object_marks m ON m.hash=o.hash
            WHERE ?1 IS NULL OR o.hash>?1 ORDER BY o.hash LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut sweep = ObjectSweep::default();
    for (hash, size, mark) in &candidates {
        if roots.contains(hash)? {
            if mark.is_some() {
                tx.execute("DELETE FROM message_page_object_marks WHERE hash=?1", [hash])?;
            }
            continue;
        }
        match mark {
            None => {
                tx.execute("INSERT INTO message_page_object_marks VALUES(?1,?2)", params![hash, now_ms])?;
                sweep.marked += 1;
            }
            Some(since) if now_ms.saturating_sub(*since) >= grace_ms => {
                tx.execute("DELETE FROM message_page_objects WHERE hash=?1", [hash])?;
                sweep.deleted += 1;
                sweep.deleted_bytes += u64::try_from(*size).unwrap_or(0);
            }
            Some(_) => {}
        }
    }
    match candidates.last() {
        Some((last, _, _)) if candidates.len() == limit => {
            tx.execute(
                "INSERT INTO message_page_sweep_cursor VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET after_hash=excluded.after_hash",
                [last],
            )?;
        }
        _ => {
            tx.execute("DELETE FROM message_page_sweep_cursor", [])?;
            sweep.wrapped = true;
        }
    }
    Ok(sweep)
}

/// A marked object may be collected soon, so callers that would otherwise skip
/// fetching it treat it as absent and put it again, which clears the mark.
pub(super) fn retained_object_present(db: &Connection, hash: &str) -> StoreResult<bool> {
    Ok(verified_object_present(db, hash)?
        && !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_page_object_marks WHERE hash=?1)",
            [hash],
            |row| row.get::<_, bool>(0),
        )?)
}

fn sql_u64(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
fn sql_usize(value: i64, index: usize) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

#[cfg(test)]
mod object_root_tests {
    use super::*;

    #[test]
    fn root_membership_and_reference_walk_state_are_deduplicated_in_sqlite() {
        let objects = Connection::open_in_memory().unwrap();
        objects.execute_batch(SCHEMA).unwrap();
        let hashes = (0..4097).map(|index| format!("{index:064x}")).collect::<Vec<_>>();
        let (root, nodes) = build_reference_tree(&hashes, false).unwrap();
        for (hash, body) in &nodes { put_object(&objects, hash, body).unwrap(); }
        let mut descriptor = RecordDescriptor::content("f0".repeat(32));
        descriptor.dependency_root = root;
        let value = UnitValue::object(descriptor).unwrap();
        let mut roots = ObjectRoots::new(Connection::open_in_memory().unwrap()).unwrap();
        roots.value(&objects, &value).unwrap();
        let count: i64 = roots.scratch.query_row("SELECT count(*) FROM roots", [], |row| row.get(0)).unwrap();
        roots.json(&objects, &serde_json::json!({"proofs":[value.clone(),value]})).unwrap();
        assert_eq!(roots.scratch.query_row("SELECT count(*) FROM roots", [], |row| row.get::<_, i64>(0)).unwrap(), count);
        assert_eq!(roots.scratch.query_row("SELECT count(*) FROM roots WHERE walked=1", [], |row| row.get::<_, i64>(0)).unwrap(), nodes.len() as i64);
        assert_eq!(roots.scratch.query_row("SELECT count(*) FROM roots WHERE pending IS NOT NULL", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert!(hashes.iter().all(|hash| roots.contains(hash).unwrap()));
        assert!(nodes.iter().all(|(hash, _)| roots.contains(hash).unwrap()));
        assert!(!roots.contains(&"fe".repeat(32)).unwrap());
        assert_eq!(roots.scratch.query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0)).unwrap(), -512);
    }

    #[test]
    fn unreadable_reference_nodes_abort_root_collection() {
        let objects = Connection::open_in_memory().unwrap();
        objects.execute_batch(SCHEMA).unwrap();
        let body = b"invalid synthetic reference page";
        let hash = risunest_sync_wire::hash(body);
        put_object(&objects, &hash, body).unwrap();
        let mut roots = ObjectRoots::new(Connection::open_in_memory().unwrap()).unwrap();
        assert!(roots.tree(&objects, &hash, false).is_err());
    }
}

#[cfg(test)]
mod capture_work_tests {
    use super::*;

    #[test]
    fn edits_resolve_to_the_spans_that_differ_from_the_old_sequence() {
        let edit = |start, delete_count, insert_count| MessageEdit { start, delete_count, insert_count };
        let region = |old_start, old_end, new_start, new_end| DirtyRegion { old_start, old_end, new_start, new_end };
        assert_eq!(
            dirty_regions(1024, 1025, &[edit(5, 1, 0), edit(600, 0, 2)]).unwrap(),
            [region(5, 6, 5, 5), region(601, 601, 600, 602)],
        );
        assert_eq!(
            dirty_regions(1024, 1024, &[edit(600, 0, 2), edit(5, 2, 0)]).unwrap(),
            [region(5, 7, 5, 5), region(600, 600, 598, 600)],
        );
        assert_eq!(dirty_regions(10, 10, &[edit(0, 0, 0)]).unwrap(), []);
        assert_eq!(dirty_regions(10, 11, &[edit(10, 0, 1)]).unwrap(), [region(10, 10, 10, 11)]);
        assert_eq!(dirty_regions(10, 9, &[edit(9, 1, 0)]).unwrap(), [region(9, 10, 9, 9)]);
        assert_eq!(dirty_regions(10, 10, &[edit(3, 2, 2), edit(4, 1, 1)]).unwrap(), [region(3, 5, 3, 5)]);
        assert!(dirty_regions(10, 10, &[edit(11, 0, 0)]).is_err());
        assert!(dirty_regions(10, 11, &[edit(0, 0, 0)]).is_err());
    }

    fn fixture() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE messages(generation TEXT,character_id TEXT,conversation_id TEXT,message_index INTEGER,
            message_id TEXT,value TEXT,canonical_hash TEXT NOT NULL DEFAULT '',canonical_size INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(generation,character_id,conversation_id,message_index));
            CREATE TABLE conversations(generation TEXT,character_id TEXT,conversation_id TEXT,message_count INTEGER,
            PRIMARY KEY(generation,character_id,conversation_id));
            INSERT INTO conversations VALUES('g','c','chat',2);
            INSERT INTO messages VALUES('g','c','chat',0,'first','{\"chatId\":\"first\"}','','0');
            INSERT INTO messages VALUES('g','c','chat',1,'second','{\"chatId\":\"second\"}','','0');").unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }

    #[test]
    fn hash_work_counts_fallback_reverification_pages_and_manifest_passes() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work, DomainWork};
        let mut db = fixture(); let tx = db.transaction().unwrap();
        let bodies = [b"{\"chatId\":\"first\"}".to_vec(), b"{\"chatId\":\"second\"}".to_vec()];
        let message_bytes = bodies.iter().map(|body| body.len() as u64).sum::<u64>();
        let page_bytes = PAGE_PREFIX.len() as u64 + message_bytes + 1 + PAGE_SUFFIX.len() as u64;
        reset_hash_work();
        let value = capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        let manifest_bytes: Vec<u8> = tx.query_row("SELECT body FROM message_page_manifests", [], |row| row.get(0)).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_message_verify"], DomainWork { calls: 4, bytes: 2 * message_bytes });
        assert_eq!(work.domains["native_page_identity"], DomainWork { calls: 1, bytes: page_bytes });
        assert_eq!(work.domains["native_manifest_identity"], DomainWork { calls: 3, bytes: 3 * manifest_bytes.len() as u64 });
        assert!(work.incomplete.is_empty());
        reset_hash_work();
        validate_manifest(&tx, &value).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_manifest_decode_identity"], DomainWork { calls: 1, bytes: manifest_bytes.len() as u64 });
        for domain in ["native_page_decode_identity", "native_message_verify", "native_repage_message_verify", "native_repage_page_identity"] {
            assert!(!work.domains.contains_key(domain), "persisted proof must skip {domain}");
        }
        assert!(work.incomplete.is_empty());
        // Removing the proof forces real canonical decoding and message hashes.
        tx.execute("DELETE FROM message_page_proofs", []).unwrap();
        reset_hash_work();
        validate_manifest(&tx, &value).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_page_decode_identity"], DomainWork { calls: 1, bytes: page_bytes });
        assert_eq!(work.domains["native_message_verify"], DomainWork { calls: 2, bytes: message_bytes });
        assert!(!work.domains.contains_key("native_repage_message_verify"));
        assert!(!work.domains.contains_key("native_repage_page_identity"));
        assert!(work.incomplete.is_empty());
    }

    #[test]
    fn verified_object_put_counts_both_passes_and_failed_hash_exactly() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work, DomainWork};
        let db = fixture(); let body = b"synthetic control"; let hash = risunest_sync_wire::hash(body);
        reset_hash_work();
        put_object(&db, &hash, body).unwrap(); object_body(&db, &hash).unwrap();
        assert!(put_object(&db, &hash, b"wrong").is_err());
        let work = take_hash_work();
        assert_eq!(work.domains["native_page_object_verify"], DomainWork { calls: 4, bytes: (3 * body.len() + 5) as u64 });
        assert!(work.incomplete.is_empty());
    }

    #[test]
    fn failed_delegated_decode_marks_unknown_partial_work_incomplete() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work};
        let db = fixture();
        let body = b"{ \"schema\":\"risunest.message-manifest/v1\",\"messageCount\":\"0\",\"pages\":[]}";
        let hash = risunest_sync_wire::hash(body); put_object(&db, &hash, body).unwrap();
        let value = UnitValue::object(RecordDescriptor::content(hash)).unwrap();
        reset_hash_work();
        assert!(validate_manifest(&db, &value).is_err());
        let work = take_hash_work();
        assert_eq!(work.incomplete["native_manifest_decode_identity"], 1);
        assert!(!work.domains.contains_key("native_manifest_decode_identity"));
        assert_eq!(work.domains["native_page_object_verify"].bytes, body.len() as u64);
    }

    #[test]
    fn reference_creation_counts_returned_pages_without_rehashing() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work, DomainWork};
        let manifest = MessageManifest { schema: MANIFEST_SCHEMA.into(), message_count: 65.into(), pages: (0..65).map(|index| ManifestPage {
            hash: format!("{index:064x}"), message_count: 1, byte_length: 80.into()
        }).collect() };
        reset_hash_work();
        let (_, objects) = descriptor(&manifest).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_reference_create"], DomainWork { calls: objects.len() as u64, bytes: objects.iter().map(|(_, body)| body.len() as u64).sum() });
        assert!(!work.domains.contains_key("native_reference_relation_boundary"));
        assert!(work.incomplete.is_empty());
    }

    #[test]
    fn captures_are_counted_once_and_take_clears_the_actual_work() {
        let mut db = fixture();
        let tx = db.transaction().unwrap();
        capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        reset_capture_work();
        current_manifest(&tx, "g", "c", "chat").unwrap();
        assert_eq!(take_capture_work(), CaptureWork::default());
        let value = serde_json::json!({"chatId":"third"});
        let body = payload_value::encode(&value).unwrap();
        let hash = MessageHash::from_bytes(&body);
        tx.execute(
            "INSERT INTO messages VALUES('g','c','chat',2,'third',?1,?2,?3)",
            params![
                String::from_utf8(body).unwrap(),
                hash.hash,
                hash.byte_length as i64
            ],
        )
        .unwrap();
        tx.execute("UPDATE conversations SET message_count=3", [])
            .unwrap();
        capture_manifest(
            &tx,
            "g",
            "c",
            "chat",
            Some(&[MessageEdit {
                start: 2,
                delete_count: 0,
                insert_count: 1,
            }]),
        )
        .unwrap();
        let totals = take_capture_work();
        assert_eq!(totals.capture_calls, 1);
        assert_eq!(totals.successful_captures, 1);
        assert_eq!(totals.failed_captures, 0);
        assert_eq!(totals.work.messages_read, 3);
        assert_eq!(totals.work.hash_rows_read, 3);
        assert_eq!(
            totals.work.bytes_read,
            b"{\"chatId\":\"first\"}".len() as u64
                + b"{\"chatId\":\"second\"}".len() as u64
                + b"{\"chatId\":\"third\"}".len() as u64
        );
        assert_eq!(totals.work.pages_written, 1);
        assert_eq!(totals.work.prefix_reused + totals.work.suffix_reused, 0);
        assert_eq!(take_capture_work(), CaptureWork::default());
        assert!(capture_manifest(
            &tx,
            "g",
            "c",
            "chat",
            Some(&[MessageEdit {
                start: 4,
                delete_count: 0,
                insert_count: 1
            }])
        )
        .is_err());
        let totals = take_capture_work();
        assert_eq!(totals.capture_calls, 1);
        assert_eq!(totals.successful_captures, 0);
        assert_eq!(totals.failed_captures, 1);
        assert_eq!(totals.work, PageWork::default());
    }

    #[test]
    fn accumulator_is_thread_local_and_explicit_probes_are_not_double_counted() {
        reset_capture_work();
        record_capture_work(
            &PageWork {
                messages_read: 7,
                bytes_read: 70,
                ..Default::default()
            },
            true,
        );
        std::thread::spawn(|| {
            assert_eq!(take_capture_work(), CaptureWork::default());
            record_capture_work(
                &PageWork {
                    messages_read: 2,
                    bytes_read: 20,
                    ..Default::default()
                },
                false,
            );
            let totals = take_capture_work();
            assert_eq!(totals.capture_calls, 1);
            assert_eq!(totals.failed_captures, 1);
            assert_eq!(totals.work.messages_read, 2);
        })
        .join()
        .unwrap();
        let totals = take_capture_work();
        assert_eq!(totals.capture_calls, 1);
        assert_eq!(totals.successful_captures, 1);
        assert_eq!(totals.work.messages_read, 7);
        let mut db = fixture();
        let tx = db.transaction().unwrap();
        let (_, work) = capture_with_work(&tx, "g", "c", "chat", None).unwrap();
        assert_eq!(work.messages_read, 2);
        assert_eq!(take_capture_work(), CaptureWork::default());
    }
}
