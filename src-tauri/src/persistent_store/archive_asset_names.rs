//! What each archive payload names, recorded on this device once per payload. The unused-file
//! cleanup and the cleanup after a permanent delete look at these records instead of reading the
//! payloads, which can decode to 2 GiB and may be held only by the Sync server.
//!
//! A record belongs to the payload's object hash, so it describes those bytes wherever they are
//! referenced from. A payload another version or another device wrote has its own hash and has
//! no record until this device reads it.

use super::asset_alias_orphans::{key_forms, AssetKeyMatcher, ANCHOR};
use super::{PersistentStore, StoreError, StoreResult};
use crate::server_sync::residency::AssetPolicy;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const FILE_NAME: &str = "archive-asset-names.sqlite";
const SCHEMA: &str = "CREATE TABLE archive_asset_names (
    hash TEXT PRIMARY KEY NOT NULL,
    names TEXT NOT NULL
);";

/// The longest piece of text kept after one `assets/`. A longer one is kept as a stem.
const TOKEN_MAX_BYTES: usize = 1024;
/// The most text one record keeps. A payload that names more counts as naming every key.
const RECORD_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Characters that end a key in text: whitespace, quotes, brackets and separators. `\` is not one,
/// since it separates older keys and starts every escape in the payload's JSON.
fn ends_key(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c
            | b'"' | b'\'' | b'`' | b'<' | b'>' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'|' | b',' | b';'
    )
}

/// The text a payload holds after each `assets/` or `assets\`: up to the character that ends it
/// (`tokens`), and that text with the character (`stems`); text cut at the length limit is a stem
/// on its own. Any form of a key that starts at one of those places either fits in its token or
/// starts with its stem, so `names` finds every key `AssetKeyMatcher` finds in the payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArchiveNames {
    tokens: BTreeSet<String>,
    stems: BTreeSet<String>,
}

impl ArchiveNames {
    /// What a payload too unusual to record in full counts as: every key starts with this stem.
    fn every_key() -> Self {
        Self { tokens: BTreeSet::new(), stems: BTreeSet::from([ANCHOR.to_owned()]) }
    }

    pub(super) fn extend(&mut self, other: Self) {
        self.tokens.extend(other.tokens);
        self.stems.extend(other.stems);
    }

    /// The `keys` some recorded payload names.
    pub(super) fn named(&self, keys: &BTreeSet<String>) -> BTreeSet<String> {
        let mut matcher = AssetKeyMatcher::new(keys.iter().cloned());
        for token in &self.tokens {
            if matcher.is_complete() {
                break;
            }
            matcher.observe(token, false);
        }
        let unfound = matcher.unreferenced().collect::<BTreeSet<_>>();
        keys.iter()
            .filter(|key| !unfound.contains(*key) || self.stem_names(key))
            .cloned()
            .collect()
    }

    fn stem_names(&self, key: &str) -> bool {
        !self.stems.is_empty()
            && key_forms(key).any(|form| {
                (ANCHOR.len()..=form.len())
                    .filter(|length| form.is_char_boundary(*length))
                    .any(|length| self.stems.contains(&form[..length]))
            })
    }
}

/// Collects `ArchiveNames` from payload text that arrives in pieces of any size.
pub(super) struct NameScan {
    pending: Vec<u8>,
    names: ArchiveNames,
    bytes: usize,
    overflowed: bool,
    invalid: bool,
}

impl NameScan {
    pub(super) fn new() -> Self {
        Self { pending: Vec::new(), names: ArchiveNames::default(), bytes: 0, overflowed: false, invalid: false }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
        if self.overflowed || self.invalid {
            return;
        }
        self.pending.extend_from_slice(bytes);
        self.scan(false);
    }

    /// The record, or `None` when the text was not UTF-8.
    pub(super) fn finish(mut self) -> Option<ArchiveNames> {
        if !self.overflowed && !self.invalid {
            self.scan(true);
        }
        if self.invalid {
            return None;
        }
        Some(if self.overflowed { ArchiveNames::every_key() } else { self.names })
    }

    fn scan(&mut self, end: bool) {
        let mut start = 0;
        loop {
            let Some(anchor) = find_anchor(&self.pending, start) else {
                // Keep the bytes an anchor could still be starting in.
                let keep = if end { 0 } else { ANCHOR.len() - 1 };
                self.pending.drain(..start.max(self.pending.len().saturating_sub(keep)));
                return;
            };
            let separator = anchor + ANCHOR.len();
            match self.pending.get(separator) {
                None if end => {
                    self.pending.clear();
                    return;
                }
                None => {
                    self.pending.drain(..anchor);
                    return;
                }
                Some(b'/' | b'\\') => {}
                Some(_) => {
                    start = separator;
                    continue;
                }
            }
            let body = separator + 1;
            let limit = anchor + TOKEN_MAX_BYTES;
            let reach = self.pending.len().min(limit);
            match self.pending[body..reach].iter().position(|byte| ends_key(*byte)) {
                Some(offset) => {
                    let stop = body + offset;
                    self.record(anchor, stop, Some(stop + 1));
                }
                None if self.pending.len() > limit => {
                    let mut stop = limit;
                    while stop > body && self.pending[stop] & 0xC0 == 0x80 {
                        stop -= 1;
                    }
                    self.record(anchor, stop, Some(stop));
                }
                None if end => self.record(anchor, self.pending.len(), None),
                None => {
                    self.pending.drain(..anchor);
                    return;
                }
            }
            if self.overflowed || self.invalid {
                self.pending.clear();
                return;
            }
            start = separator;
        }
    }

    fn record(&mut self, anchor: usize, token_end: usize, stem_end: Option<usize>) {
        for (end, stem) in std::iter::once((token_end, false)).chain(stem_end.map(|end| (end, true))) {
            let Ok(text) = std::str::from_utf8(&self.pending[anchor..end]) else {
                self.invalid = true;
                return;
            };
            let set = if stem { &mut self.names.stems } else { &mut self.names.tokens };
            if !set.contains(text) {
                self.bytes += text.len();
                set.insert(text.to_owned());
            }
        }
        if self.bytes > RECORD_MAX_BYTES {
            self.overflowed = true;
        }
    }
}

fn find_anchor(bytes: &[u8], start: usize) -> Option<usize> {
    let mut from = start;
    while let Some(offset) = bytes.get(from..)?.iter().position(|byte| *byte == b'a') {
        let at = from + offset;
        if bytes[at..].starts_with(ANCHOR.as_bytes()) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// Passes payload text on to `inner` and records what it names on the way.
pub(super) struct NameTee<'a, W> {
    pub(super) inner: W,
    pub(super) scan: &'a mut NameScan,
}

impl<W: Write> Write for NameTee<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.scan.push(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation { message: message.to_owned() }
}

fn validate_hash(hash: &str) -> StoreResult<()> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Err(invalid("invalid archive payload hash"));
    }
    Ok(())
}

fn open(root: &Path) -> StoreResult<Connection> {
    let mut db = crate::sqlite_open::open(root.join("persistent").join(FILE_NAME))?;
    // Connections here are opened per call from several threads. The rollback journal waits out
    // a concurrent writer on the busy timeout, which starting a WAL read does not.
    db.execute_batch("PRAGMA busy_timeout=5000;")?;
    let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: u32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.execute_batch("PRAGMA user_version=1;")?;
        } else if current != 1 {
            return Err(StoreError::SchemaMismatch { message: "Unsupported archive asset name schema".into() });
        }
        tx.commit()?;
    } else if version != 1 {
        return Err(StoreError::SchemaMismatch { message: "Unsupported archive asset name schema".into() });
    }
    let schema: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='archive_asset_names'",
        [],
        |row| row.get(0),
    )?;
    let normalize = |sql: &str| sql.chars().filter(|c| !c.is_whitespace() && *c != ';').collect::<String>();
    if normalize(&schema) != normalize(SCHEMA) {
        return Err(StoreError::SchemaMismatch { message: "Archive asset name schema is incompatible".into() });
    }
    Ok(db)
}

/// The records of `hashes` that exist. A record that cannot be read counts as missing.
pub(super) fn read(root: &Path, hashes: &BTreeSet<String>) -> StoreResult<BTreeMap<String, ArchiveNames>> {
    let db = open(root)?;
    let mut query = db.prepare("SELECT names FROM archive_asset_names WHERE hash=?1")?;
    let mut records = BTreeMap::new();
    for hash in hashes {
        validate_hash(hash)?;
        let Some(names) = query.query_row([hash], |row| row.get::<_, String>(0)).optional()? else { continue };
        if let Ok(names) = serde_json::from_str::<ArchiveNames>(&names) {
            records.insert(hash.clone(), names);
        }
    }
    Ok(records)
}

pub(super) fn write(root: &Path, hash: &str, names: &ArchiveNames) -> StoreResult<()> {
    validate_hash(hash)?;
    let names = serde_json::to_string(names)?;
    open(root)?.execute(
        "INSERT INTO archive_asset_names(hash,names) VALUES(?1,?2) ON CONFLICT(hash) DO UPDATE SET names=excluded.names",
        params![hash, names],
    )?;
    Ok(())
}

/// Writes the record of a payload this device just wrote. The archive does not depend on it: a
/// payload without a record is read later.
pub(super) fn write_new(root: &Path, hash: &str, scan: NameScan) {
    let Some(names) = scan.finish() else { return };
    if let Err(error) = write(root, hash, &names) {
        crate::nlog!("warn", "An archive's asset names were not recorded: {error}");
    }
}

/// Reads the payload in `reader` and records what it names. A record that cannot be written is
/// only logged, since the names read are still right for this run.
pub(super) fn record_payload(
    root: &Path,
    hash: &str,
    reader: impl Read,
    cancelled: &dyn Fn() -> bool,
) -> StoreResult<ArchiveNames> {
    let mut scan = NameScan::new();
    super::archive::read_payload_text(reader, |bytes| {
        scan.push(bytes);
        Ok(!cancelled())
    })?;
    if cancelled() {
        return Err(invalid(super::archive::ARCHIVE_CANCELLED_MESSAGE));
    }
    let names = scan.finish().ok_or_else(|| invalid("archive payload is not text"))?;
    if let Err(error) = write(root, hash, &names) {
        crate::nlog!("warn", "An archive's asset names were not recorded: {error}");
    }
    Ok(names)
}

/// What the archives of the active generation name, as far as their records tell.
pub(super) struct ArchiveNameRecords {
    pub(super) names: ArchiveNames,
    /// The payloads, by object hash, that have no record yet.
    pub(super) unrecorded: BTreeSet<String>,
}

/// The payloads of the active generation's archives that have no record, read once without
/// holding the store.
pub(crate) struct ArchiveNameFill {
    root: PathBuf,
    unrecorded: BTreeSet<String>,
    policy: AssetPolicy,
}

impl PersistentStore {
    /// The payload of every archived character of the active generation, the one a restore reads.
    /// `None` when an archive row cannot be read.
    fn archive_payload_hashes(&self) -> StoreResult<Option<BTreeSet<String>>> {
        let generation = super::active_generation(&self.connection)?;
        let mut statement = self.connection.prepare(
            "SELECT archived_object FROM characters WHERE generation = ?1 AND archived_object IS NOT NULL",
        )?;
        let mut rows = statement.query([generation])?;
        let mut hashes = BTreeSet::new();
        while let Some(row) = rows.next()? {
            let rusqlite::types::ValueRef::Text(record) = row.get_ref(0)? else { return Ok(None) };
            let Ok(archived) = serde_json::from_slice::<super::archive::ArchivedObject>(record) else {
                return Ok(None);
            };
            if validate_hash(&archived.object_hash).is_err() {
                return Ok(None);
            }
            hashes.insert(archived.object_hash);
        }
        Ok(Some(hashes))
    }

    /// The records of the active generation's archives. `None` when an archive row or the records
    /// cannot be read, so no archive can be shown to name nothing.
    pub(super) fn archive_name_records(&self) -> StoreResult<Option<ArchiveNameRecords>> {
        let Some(hashes) = self.archive_payload_hashes()? else { return Ok(None) };
        if hashes.is_empty() {
            return Ok(Some(ArchiveNameRecords { names: ArchiveNames::default(), unrecorded: hashes }));
        }
        let records = match read(&self.repository_root, &hashes) {
            Ok(records) => records,
            Err(error) => {
                crate::nlog!("warn", "Archive asset names could not be read: {error}");
                return Ok(None);
            }
        };
        let mut names = ArchiveNames::default();
        let mut unrecorded = BTreeSet::new();
        for hash in hashes {
            match records.get(&hash) {
                Some(record) => names.extend(record.clone()),
                None => {
                    unrecorded.insert(hash);
                }
            }
        }
        Ok(Some(ArchiveNameRecords { names, unrecorded }))
    }

    /// The archives whose payloads have no record, to read without holding the store.
    pub(crate) fn archive_name_fill(&self) -> StoreResult<ArchiveNameFill> {
        let unrecorded = self.archive_name_records()?.map(|records| records.unrecorded).unwrap_or_default();
        Ok(ArchiveNameFill {
            root: self.repository_root.clone(),
            unrecorded,
            policy: self.device_store()?.asset_residency_policy()?,
        })
    }
}

impl ArchiveNameFill {
    pub(crate) fn is_empty(&self) -> bool {
        self.unrecorded.is_empty()
    }

    /// Reads each payload without a record and records what it names, skipping one that fails.
    /// With `fetch`, a payload this device does not hold is fetched once: under the full asset
    /// policy it stays on this device like every other body, and under the remote policy it is
    /// read from a temporary copy.
    pub(crate) fn run(self, fetch: bool, cancelled: &Arc<AtomicBool>) -> StoreResult<()> {
        let is_cancelled = || cancelled.load(Ordering::Acquire);
        let check = || {
            if is_cancelled() {
                Err(crate::server_sync::SyncError::new("cancelled", 499))
            } else {
                Ok(())
            }
        };
        let cas = crate::asset_repository::PayloadCas::new(&self.root)?;
        let scratch = if fetch && self.policy == AssetPolicy::Remote {
            Some(crate::external_storage::leftovers::local_scratch(&self.root, "archive-names-")?)
        } else {
            None
        };
        let mut hydration = None;
        for hash in &self.unrecorded {
            if is_cancelled() {
                break;
            }
            let recorded = (|| -> StoreResult<()> {
                if let Some(file) = cas.open_object(hash)? {
                    record_payload(&self.root, hash, file, &is_cancelled)?;
                    return Ok(());
                }
                if !fetch {
                    return Ok(());
                }
                let fetched = |error: crate::server_sync::SyncError| invalid(&format!("archive payload fetch failed: {}", error.code));
                match &scratch {
                    Some(scratch) => {
                        let body = crate::server_sync::residency::open_transient_with_check(&self.root, scratch.path(), hash, &check)
                            .map_err(fetched)?;
                        if let Some(body) = body {
                            record_payload(&self.root, hash, body, &is_cancelled)?;
                        }
                    }
                    None => {
                        let session = match &mut hydration {
                            Some(session) => session,
                            None => hydration.insert(
                                crate::server_sync::residency::HydrationSession::new(&self.root, Some(Arc::clone(cancelled)))
                                    .map_err(fetched)?,
                            ),
                        };
                        if let Some(file) = session.open(hash, &check).map_err(fetched)? {
                            record_payload(&self.root, hash, file, &is_cancelled)?;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = recorded {
                if !is_cancelled() {
                    crate::nlog!("warn", "An archive's asset names could not be recorded: {error}");
                }
            }
        }
        Ok(())
    }
}

/// Removes the record of the payload `hash`, as for a payload an older release wrote.
#[cfg(test)]
pub(crate) fn forget(root: &Path, hash: &str) {
    open(root).unwrap().execute("DELETE FROM archive_asset_names WHERE hash = ?1", [hash]).unwrap();
}

#[cfg(test)]
pub(crate) fn recorded(root: &Path, hash: &str) -> bool {
    !read(root, &BTreeSet::from([hash.to_owned()])).unwrap().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanned(text: &str, piece: usize) -> ArchiveNames {
        let mut scan = NameScan::new();
        for chunk in text.as_bytes().chunks(piece.max(1)) {
            scan.push(chunk);
        }
        scan.finish().unwrap()
    }

    fn matcher_named(keys: &BTreeSet<String>, text: &str) -> BTreeSet<String> {
        let mut matcher = AssetKeyMatcher::new(keys.iter().cloned());
        matcher.observe(text, false);
        let unfound = matcher.unreferenced().collect::<BTreeSet<_>>();
        keys.difference(&unfound).cloned().collect()
    }

    fn keys(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|key| key.to_string()).collect()
    }

    /// The record names at least what the matcher finds in the same payload text, however the
    /// text arrives, and no more for keys that hold no character that ends a key.
    #[test]
    fn records_name_what_the_matcher_finds_in_the_payload_text() {
        let candidates = keys(&[
            "assets/a.png", "assets/a.png.bak", "assets/b.png", "assets/c.png", "assets/d.png",
            "assets/그림.png", "assets/e.png", "assets/sub/f.png", "assets/n.png", "assets/unused.png",
            "assets/my image.png", "assets/long.png",
        ]);
        let long_tail = "가".repeat(600);
        let text = serde_json::to_string(&serde_json::json!({
            "detail": {"desc": "<p><img src=\"assets/a.png.bak\"></p>", "image": "assets/b.png"},
            "messages": [
                "앞 글<img src=\"assets/c.png?v=1\">뒤 글",
                "legacy assets\\d.png and assets\\sub\\f.png",
                "assets/그림.png", "assets/e.png\nnext line", "the company's assets\nnext",
                "<img src=\"assets/my image.png\">",
                format!("assets/long.png{long_tail}"),
            ],
        }))
        .unwrap();
        let expected = matcher_named(&candidates, &text);
        assert!(expected.contains("assets/a.png") && expected.contains("assets/sub/f.png"));
        assert!(!expected.contains("assets/unused.png"));
        for piece in [1, 2, 3, 5, 7, 64, text.len()] {
            let named = scanned(&text, piece).named(&candidates);
            assert!(named.is_superset(&expected), "piece {piece}: {named:?} misses some of {expected:?}");
            // Only the key with a space is kept by a stem the text does not hold in full.
            let extra = named.difference(&expected).cloned().collect::<Vec<_>>();
            assert!(extra.is_empty(), "piece {piece}: {extra:?}");
        }
    }

    #[test]
    fn a_key_holding_a_character_that_ends_keys_is_kept_by_its_stem() {
        let text = r#"{"data":"<img src=\"assets/my image.png\">"}"#;
        let record = scanned(text, 3);
        let named = record.named(&keys(&["assets/my image.png", "assets/my", "assets/other.png"]));
        assert_eq!(named, keys(&["assets/my image.png", "assets/my"]));
        // A key longer than a token is kept by the cut text.
        let long = format!("assets/{}.png", "x".repeat(TOKEN_MAX_BYTES));
        let record = scanned(&serde_json::to_string(&long).unwrap(), 100);
        assert_eq!(record.named(&keys(&[&long, "assets/x.png"])), keys(&[&long]));
    }

    #[test]
    fn a_payload_naming_too_much_names_every_key_and_invalid_text_has_no_record() {
        let text = (0..RECORD_MAX_BYTES / 16 + 1).map(|index| format!("assets/{index:09}.png ")).collect::<String>();
        let record = scanned(&text, 4096);
        assert_eq!(record, ArchiveNames::every_key());
        assert_eq!(record.named(&keys(&["assets/anything.png"])), keys(&["assets/anything.png"]));
        let mut scan = NameScan::new();
        scan.push(b"assets/\xff\xfe.png ");
        assert!(scan.finish().is_none());
    }

    #[test]
    fn records_survive_reopening_and_do_not_change_the_library() {
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let revision = store.revision().unwrap();
        let hash = "a".repeat(64);
        let record = scanned("assets/a.png", 4);
        write(directory.path(), &hash, &record).unwrap();
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(store.revision().unwrap(), revision);
        let hashes = BTreeSet::from([hash.clone(), "b".repeat(64)]);
        assert_eq!(read(directory.path(), &hashes).unwrap(), BTreeMap::from([(hash, record)]));
        assert!(read(directory.path(), &BTreeSet::from(["bad".to_owned()])).is_err());
        open(directory.path()).unwrap().execute_batch("PRAGMA user_version=2").unwrap();
        assert!(matches!(read(directory.path(), &hashes), Err(StoreError::SchemaMismatch { .. })));
    }
}
