//! Application eligibility is independent of ZIP/catalog integrity. SQL values remain untouched.
use super::*;
use crate::{
    asset_repository::PayloadCas,
    data_health::{codes, Finding, FindingSink, FirstFinding, Report},
    lossless_f0::{scan_portable_fragment, F0Reference, F0ReferenceStatus, PortableFragment},
    persistent_store::portable_validation,
};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct ReferenceCounts {
    pub(crate) total: u64,
    pub(crate) expected_missing: u64,
}

/// Stops at the first blocking violation, as the backup and activation gates require.
fn fail_fast(view: LibraryView<'_>, probe: &dyn CancellationProbe) -> Result<ReferenceCounts> {
    let mut first = FirstFinding::default();
    let counts = view.validate(&mut Report::new(&mut first), probe)?;
    match first.into_inner() {
        Some(finding) => Err(Error::Store(StoreError::Validation {
            message: finding.detail,
        })),
        None => Ok(counts),
    }
}

/// Reports every violation the same rules produce. A failure outside any single item becomes one
/// unclassified finding, so an unforeseen shape narrows the diagnosis instead of ending it.
fn collect(
    view: LibraryView<'_>,
    sink: &mut dyn FindingSink,
    probe: &dyn CancellationProbe,
) -> Result<ReferenceCounts> {
    let mut report = Report::new(sink);
    match view.validate(&mut report, probe) {
        Ok(counts) => Ok(counts),
        Err(error) => {
            // A stop the caller asked for is the stop, not something wrong with the library.
            check(probe)?;
            report.record(unclassified(error)?);
            Ok(ReferenceCounts::default())
        }
    }
}

/// A rule failure the scan could not attribute to one item still belongs in the diagnosis.
/// Everything else, cancellation and real storage faults included, keeps propagating.
fn unclassified(error: Error) -> Result<Finding> {
    match error {
        error @ (Error::Invalid(_)
        | Error::Json(_)
        | Error::Store(StoreError::Validation { .. })) => Ok(Finding::new(
            codes::UNCLASSIFIED,
            "library",
            "",
            error.to_string(),
        )),
        error => Err(error),
    }
}

impl VerifiedArchive {
    pub(crate) fn validate_library(
        &self,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        if !self.manifest.library_included
            || self.manifest.repair_required
            || self.manifest.profile != Profile::Portable
        {
            return Err(Error::Invalid(
                "archive library requires repair or is not included",
            ));
        }
        fail_fast(
            LibraryView {
                db: &self.db,
                objects: self,
            },
            probe,
        )
    }

    /// The diagnosis exists for archives the gate refuses, so it only requires a library to scan.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn scan_library(
        &self,
        sink: &mut dyn FindingSink,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        if !self.manifest.library_included {
            return Err(Error::Invalid("archive has no library"));
        }
        collect(
            LibraryView {
                db: &self.db,
                objects: self,
            },
            sink,
            probe,
        )
    }
}
impl Catalog {
    pub(crate) fn validate_library(
        &self,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        fail_fast(
            LibraryView {
                db: &self.db,
                objects: self,
            },
            probe,
        )
    }
}

/// Scans the live store. `db` must present the leased generation through the portable raw column
/// set; payloads come from the repository CAS instead of an archive catalog.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn scan_live_library(
    db: &rusqlite::Connection,
    cas: &PayloadCas,
    sink: &mut dyn FindingSink,
    probe: &dyn CancellationProbe,
) -> Result<ReferenceCounts> {
    collect(
        LibraryView {
            db,
            objects: &LivePayloads::open(cas)?,
        },
        sink,
        probe,
    )
}

/// What the deep object pass still has to read, so the screen can show a proportion before the
/// pass starts and the caller can size its pages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObjectTotals {
    pub(crate) objects: u64,
    pub(crate) bytes: u64,
}

/// One bounded step of the deep object pass. `cursor` is the last hash this page finished, and
/// `done` says the enumeration reached the end rather than the page budget.
#[derive(Clone, Debug, Default)]
pub(crate) struct ObjectPage {
    pub(crate) cursor: Option<String>,
    pub(crate) objects: u64,
    pub(crate) bytes: u64,
    pub(crate) done: bool,
}

/// Totals over the objects the leased generation registers. An object several aliases share is
/// read once; the byte total includes only bodies currently stored on this device.
pub(crate) fn registered_object_totals(db: &rusqlite::Connection, cas: &PayloadCas) -> Result<ObjectTotals> {
    let mut statement = db.prepare(REGISTERED_OBJECTS)?;
    let mut rows = statement.query([""])?;
    let mut totals = ObjectTotals::default();
    let mut hashes = Vec::new();
    loop {
        hashes.clear();
        for _ in 0..ABSENT_PAGE {
            let Some(row) = rows.next()? else { break; };
            hashes.push(row.get::<_, String>(0)?);
        }
        if hashes.is_empty() { break; }
        totals.objects += hashes.len() as u64;
        totals.bytes += cas.stat_objects(hashes.iter().map(String::as_str))?.into_iter().flatten().sum::<u64>();
    }
    Ok(totals)
}

const REGISTERED_OBJECTS: &str = "SELECT object_hash,MAX(size),MAX(kind) FROM (
            SELECT object_hash,size,kind FROM asset_aliases WHERE object_hash IS NOT NULL
        ) WHERE object_hash>?1 GROUP BY object_hash ORDER BY object_hash";

/// Rereads the stored bytes of every registered object and compares the digest with the hash the
/// alias registered. `budget` bounds one page by the bytes it reads, so a large library makes
/// progress the screen can show and the caller can stop between pages.
pub(crate) fn scan_registered_objects(
    db: &rusqlite::Connection,
    cas: &PayloadCas,
    after: Option<&str>,
    budget: u64,
    sink: &mut dyn FindingSink,
    probe: &dyn CancellationProbe,
) -> Result<ObjectPage> {
    let mut report = Report::new(sink);
    let mut page = ObjectPage::default();
    let mut statement = db.prepare(REGISTERED_OBJECTS)?;
    let mut rows = statement.query([after.unwrap_or("")])?;
    while let Some(row) = rows.next()? {
        check(probe)?;
        let hash: String = row.get(0)?;
        let size = sql_u64(row.get(1)?)?;
        let kind: String = row.get(2)?;
        match reread_object(cas, &hash, size) {
            Ok(read) => page.bytes += read,
            Err((detail, read)) => {
                page.bytes += read;
                if !report.record(Finding::new(
                    codes::ALIAS_OBJECT_MISMATCH,
                    kind,
                    hash.clone(),
                    detail,
                )) {
                    return Ok(page);
                }
            }
        }
        page.objects += 1;
        page.cursor = Some(hash);
        if page.bytes >= budget {
            return Ok(page);
        }
    }
    page.done = true;
    Ok(page)
}

/// Returns the bytes read, or the reason the stored object no longer matches its registration.
/// An absent object is the quick scan's own finding, so this pass stays quiet about it.
fn reread_object(cas: &PayloadCas, hash: &str, size: u64) -> std::result::Result<u64, (String, u64)> {
    use sha2::Digest;
    let Some(mut file) = cas.open_object(hash).map_err(|error| (error.to_string(), 0))? else {
        return Ok(0);
    };
    let mut digest = sha2::Sha256::new();
    let mut read = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err((error.to_string(), read)),
        };
        digest.update(&buffer[..count]);
        read += count as u64;
    }
    if read != size {
        return Err(("stored payload length differs from the registered size".to_owned(), read));
    }
    if hex::encode(digest.finalize()) != hash {
        return Err(("stored payload digest differs from the registered hash".to_owned(), read));
    }
    Ok(read)
}

/// The gate a backup and an activation use, over a live generation. A repair runs it on its
/// staged result before that result becomes the library.
pub(crate) fn validate_live_library(
    db: &rusqlite::Connection,
    cas: &PayloadCas,
    probe: &dyn CancellationProbe,
) -> Result<ReferenceCounts> {
    fail_fast(
        LibraryView {
            db,
            objects: &LivePayloads::open(cas)?,
        },
        probe,
    )
}

/// The live library's payloads. A body this device does not hold still counts as stored while
/// the Sync server or a live external storage connection holds it, so it is neither reported as
/// missing nor offered for removal. Every lookup reads this device's own records.
struct LivePayloads<'a> {
    cas: &'a PayloadCas,
    holders: std::cell::RefCell<crate::persistent_store::asset_residency::RemoteHolders>,
}

impl<'a> LivePayloads<'a> {
    fn open(cas: &'a PayloadCas) -> Result<Self> {
        let holders = crate::persistent_store::asset_residency::RemoteHolders::open(cas.repository_root())
            .map_err(custody_failure)?;
        Ok(Self {
            cas,
            holders: std::cell::RefCell::new(holders),
        })
    }

    fn held_elsewhere(&self, hashes: &[String]) -> Result<Vec<bool>> {
        self.holders.borrow_mut().held(hashes).map_err(custody_failure)
    }

    /// Records the waiting alias findings in their scan order, leaving out the absent bodies
    /// another storage holds. Returns false once the report stops.
    fn flush(
        &self,
        waiting: &mut Vec<(Finding, Option<usize>)>,
        absent: &mut Vec<String>,
        report: &mut Report<'_>,
    ) -> Result<bool> {
        let held = match absent.is_empty() {
            true => Vec::new(),
            false => self.held_elsewhere(absent)?,
        };
        absent.clear();
        for (finding, body) in waiting.drain(..) {
            if body.is_some_and(|body| held[body]) {
                continue;
            }
            if !report.record(finding) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// A residency record that cannot be read stops the scan: guessing would either hide a missing
/// body or report a held one for removal.
fn custody_failure(error: crate::server_sync::SyncError) -> Error {
    Error::Io(std::io::Error::other(error.code))
}

/// Absent bodies classified per residency question.
const ABSENT_PAGE: usize = 1024;

trait LibraryObjects {
    fn read_object(&self, hash: &str) -> Result<(Box<dyn Read>, u64)>;
    /// Every registered alias needs an exact payload binding, independently of reference scans.
    fn validate_registered_payloads(
        &self,
        db: &rusqlite::Connection,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()>;
    /// Whether the historical payload an owner manifest entry names is still stored.
    fn has_object(&self, db: &rusqlite::Connection, hash: &[u8]) -> Result<bool>;
}
impl LibraryObjects for VerifiedArchive {
    fn read_object(&self, hash: &str) -> Result<(Box<dyn Read>, u64)> {
        self.open_object(hash).map(|(reader,size)|(Box::new(reader) as Box<dyn Read>,size))
    }
    fn validate_registered_payloads(
        &self,
        db: &rusqlite::Connection,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        validate_catalog_payloads(db, report, probe)
    }
    fn has_object(&self, db: &rusqlite::Connection, hash: &[u8]) -> Result<bool> {
        catalog_has_object(db, hash)
    }
}
impl LibraryObjects for Catalog {
    fn read_object(&self, hash: &str) -> Result<(Box<dyn Read>, u64)> {
        let (path,size):(String,i64)=self.db.query_row("SELECT s.path,o.byte_length FROM sources s JOIN objects o ON o.sha256=s.sha256 WHERE o.sha256=?1",[hex::decode(hash).map_err(|_|Error::Invalid("invalid object hash"))?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let size = sql_u64(size)?;
        Ok((Box::new(std::fs::File::open(path)?.take(size)), size))
    }
    fn validate_registered_payloads(
        &self,
        db: &rusqlite::Connection,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        validate_catalog_payloads(db, report, probe)
    }
    fn has_object(&self, db: &rusqlite::Connection, hash: &[u8]) -> Result<bool> {
        catalog_has_object(db, hash)
    }
}
impl LibraryObjects for LivePayloads<'_> {
    fn read_object(&self, hash: &str) -> Result<(Box<dyn Read>, u64)> {
        let file = self
            .cas
            .open_object(hash)?
            .ok_or(Error::Invalid("library object is missing"))?;
        let size = file.metadata()?.len();
        Ok((Box::new(file.take(size)), size))
    }
    fn validate_registered_payloads(
        &self,
        db: &rusqlite::Connection,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        // A finding waits with its absent body's position until a page of absent bodies is
        // classified, so the report keeps the scan order.
        let mut waiting = Vec::new();
        let mut absent = Vec::new();
        let mut statement = db.prepare(
            "SELECT kind,logical_key,object_hash,size FROM asset_aliases ORDER BY kind,logical_key",
        )?;
        let mut rows = statement.query([])?;
        loop {
            check(probe)?;
            let mut page = Vec::new();
            for _ in 0..ABSENT_PAGE {
                let Some(row) = rows.next()? else { break; };
                page.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, sql_u64(row.get(3)?)?));
            }
            if page.is_empty() { break; }
            let mut sizes = self.cas.stat_objects(page.iter().filter_map(|(_, _, hash, _)| hash.as_deref()))?.into_iter();
            for (kind, key, hash, size) in page {
                check(probe)?;
                let stored = hash.as_ref().and_then(|_| sizes.next().flatten());
                let (finding, body) = match (stored, hash) {
                    (Some(actual), _) if actual == size => continue,
                    (Some(_), _) => (alias_finding(true, &kind, &key), None),
                    (None, Some(hash)) => {
                        absent.push(hash);
                        (alias_finding(false, &kind, &key), Some(absent.len() - 1))
                    }
                    (None, None) => (alias_finding(false, &kind, &key), None),
                };
                waiting.push((finding, body));
                // Nothing to classify ahead of it, so a fail-fast caller still stops here.
                let settled = body.is_none() && absent.is_empty();
                if (settled || absent.len() == ABSENT_PAGE)
                    && !self.flush(&mut waiting, &mut absent, report)?
                {
                    return Ok(());
                }
            }
        }
        self.flush(&mut waiting, &mut absent, report)?;
        Ok(())
    }
    fn has_object(&self, _db: &rusqlite::Connection, hash: &[u8]) -> Result<bool> {
        let hash = hex::encode(hash);
        if self.cas.stat_object(&hash)?.is_some() {
            return Ok(true);
        }
        Ok(self.held_elsewhere(std::slice::from_ref(&hash))?[0])
    }
}

/// Enumerates the aliases whose registered file binding is absent or inexact. Fail-fast callers
/// stop at the first row, which costs the same as the previous existence query.
fn validate_catalog_payloads(
    db: &rusqlite::Connection,
    report: &mut Report<'_>,
    probe: &dyn CancellationProbe,
) -> Result<()> {
    for sql in [
        "SELECT a.kind,a.logical_key,EXISTS(SELECT 1 FROM files f WHERE f.kind=a.kind AND f.logical_key=a.logical_key AND f.state='present') FROM asset_aliases a WHERE a.object_hash IS NULL OR NOT EXISTS(SELECT 1 FROM files f JOIN objects o ON f.object_hash=o.sha256 WHERE f.kind=a.kind AND f.logical_key=a.logical_key AND f.state='present' AND lower(hex(o.sha256))=a.object_hash AND o.byte_length=a.size) ORDER BY a.kind,a.logical_key",
    ] {
        let mut statement = db.prepare(sql)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let kind: String = row.get(0)?;
            let key: String = row.get(1)?;
            let bound: bool = row.get(2)?;
            if !report.record(alias_finding(bound, &kind, &key)) {
                return Ok(());
            }
        }
    }
    let mut statement = db.prepare("SELECT kind,logical_key FROM files WHERE state!='present'")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        check(probe)?;
        let kind: String = row.get(0)?;
        let key: String = row.get(1)?;
        if !report.record(Finding::new(
            codes::ALIAS_OBJECT_ABSENT,
            kind,
            key,
            "registered payload is not present",
        )) {
            return Ok(());
        }
    }
    Ok(())
}

fn catalog_has_object(db: &rusqlite::Connection, hash: &[u8]) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects WHERE sha256=?1)",
        [hash],
        |r| r.get(0),
    )?)
}

fn alias_finding(stored: bool, kind: &str, key: &str) -> Finding {
    if stored {
        Finding::new(
            codes::ALIAS_OBJECT_MISMATCH,
            kind,
            key,
            "stored payload hash or size differs from the alias",
        )
    } else {
        Finding::new(
            codes::ALIAS_OBJECT_ABSENT,
            kind,
            key,
            "no stored payload binds to the alias",
        )
    }
}

struct LibraryView<'a> {
    db: &'a rusqlite::Connection,
    objects: &'a dyn LibraryObjects,
}
impl LibraryView<'_> {
    fn open_object(&self, hash: &str) -> Result<(Box<dyn Read>, u64)> {
        self.objects.read_object(hash)
    }
    /// A record the scan cannot parse is reported and skipped, leaving the rest scannable.
    fn parsed(&self, report: &mut Report<'_>, kind: &str, id: &str, serialized: &str) -> Option<Value> {
        match serde_json::from_str(serialized) {
            Ok(value) => Some(value),
            Err(_) => {
                report.record(Finding::new(
                    codes::RECORD_INVALID,
                    kind,
                    id,
                    "record JSON is invalid",
                ));
                None
            }
        }
    }
    fn validate(
        &self,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        let mut counts = ReferenceCounts::default();
        portable_validation::validate_records_into(self.db, probe, report)?;
        if report.running() {
        }
        if report.running() {
            self.objects
                .validate_registered_payloads(self.db, report, probe)?;
        }
        if !report.running() {
            return Ok(counts);
        }
        let mut statement = self.db.prepare("SELECT character_id,archived_object FROM characters WHERE archived_object IS NOT NULL")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let id: String = row.get(0)?;
            let archived = serde_json::from_str::<crate::persistent_store::archive::ArchivedObject>(&row.get::<_, String>(1)?);
            let valid = match archived {
                Ok(archived) => {
                    let mut valid = true;
                    for hash in archived.object_roots() {
                        match hex::decode(hash) {
                            Ok(bytes) if bytes.len() == 32 => valid &= self.objects.has_object(self.db, &bytes)?,
                            _ => valid = false,
                        }
                    }
                    valid
                }
                Err(_) => false,
            };
            if !valid && !report.record(Finding::new(codes::RECORD_INVALID, "character", id, "archived character payload is invalid or missing")) {
                return Ok(counts);
            }
        }
        let serialized: Option<String> = self
            .db
            .query_row("SELECT value FROM root", [], |r| r.get(0))
            .optional()?;
        let Some(root) = serialized
            .as_deref()
            .and_then(|serialized| self.parsed(report, "root", "", serialized))
        else {
            if serialized.is_none() {
                report.record(Finding::new(
                    codes::RECORD_INVALID,
                    "root",
                    "",
                    "library has no root record",
                ));
            }
            return Ok(counts);
        };
        self.validate_owners(&root, report, probe)?;
        if !report.running() {
            return Ok(counts);
        }
        let selected = selected_preset(self.db, root.get("botPresetsId"))?;
        self.validate_fragment(
            PortableFragment::Root {
                value: &root,
                selected_preset: selected.as_ref(),
            },
            &root,
            "root",
            "",
            &mut counts,
            report,
            probe,
        )?;
        for (sql,kind) in [("SELECT preset_id,configured_index,value,preset_id FROM bot_presets ORDER BY configured_index","preset"),("SELECT storage_key,ordinal,value,json_object('owner',owner,'key',storage_key) FROM plugin_storage ORDER BY ordinal","plugin")] {
            let mut statement=self.db.prepare(sql)?;let mut rows=statement.query([])?;
            while let Some(row)=rows.next()? {check(probe)?;
                if !report.running() {return Ok(counts);}
                let key:String=row.get(0)?;let index:i64=row.get(1)?;let owner_id:String=row.get(3)?;
                let Some(value)=self.parsed(report,kind,&owner_id,&row.get::<_,String>(2)?) else {continue};
                let fragment=if kind=="preset" {PortableFragment::Preset{value:&value,index}}else{PortableFragment::Plugin{value:&value,key:&key}};
                self.validate_fragment(fragment,&root,kind,&owner_id,&mut counts,report,probe)?;
            }
        }
        let mut statement=self.db.prepare("SELECT character_id,detail,conversation_count FROM characters ORDER BY configured_index")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            if !report.running() {
                return Ok(counts);
            }
            let id: String = row.get(0)?;
            let Some(value) = self.parsed(report, "character", &id, &row.get::<_, String>(1)?)
            else {
                continue;
            };
            let selected=match value.get("chatPage").and_then(Value::as_u64).and_then(|n|i64::try_from(n).ok()) {Some(index)=>self.db.query_row("SELECT detail FROM conversations WHERE character_id=?1 ORDER BY configured_index LIMIT 1 OFFSET ?2",params![id,index],|r|r.get::<_,String>(0)).optional()?.and_then(|v|self.parsed(report,"character",&id,&v)),None=>None};
            self.validate_fragment(
                PortableFragment::Character {
                    value: &value,
                    selected_chat: selected.as_ref(),
                    has_chats: row.get::<_, i64>(2)? > 0,
                },
                &root,
                "character",
                &id,
                &mut counts,
                report,
                probe,
            )?;
        }
        let mut statement = self.db.prepare(
            "SELECT character_id,conversation_id,detail FROM conversations ORDER BY character_id,configured_index",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            if !report.running() {
                return Ok(counts);
            }
            let character: String = row.get(0)?;
            // The owner names one conversation, so a repair can address the record it belongs to.
            let owner = format!("{character}/{}", row.get::<_, String>(1)?);
            let Some(value) = self.parsed(report, "conversation", &owner, &row.get::<_, String>(2)?)
            else {
                continue;
            };
            self.validate_fragment(
                PortableFragment::Conversation {
                    value: &value,
                    character_id: &character,
                },
                &root,
                "conversation",
                &owner,
                &mut counts,
                report,
                probe,
            )?;
        }
        let mut statement=self.db.prepare("SELECT character_id,conversation_id,message_index,value FROM messages ORDER BY character_id,conversation_id,message_index")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            if !report.running() {
                return Ok(counts);
            }
            let character: String = row.get(0)?;
            let conversation: String = row.get(1)?;
            let index: i64 = row.get(2)?;
            let owner = format!("{character}/{conversation}/{index}");
            let Some(value) = self.parsed(report, "message", &owner, &row.get::<_, String>(3)?)
            else {
                continue;
            };
            self.validate_fragment(
                PortableFragment::Message {
                    value: &value,
                    character_id: &character,
                    conversation_id: &conversation,
                    index,
                },
                &root,
                "message",
                &owner,
                &mut counts,
                report,
                probe,
            )?;
        }
        Ok(counts)
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_fragment(
        &self,
        fragment: PortableFragment<'_>,
        root: &Value,
        owner_kind: &str,
        owner_id: &str,
        counts: &mut ReferenceCounts,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let Ok(references) = scan_portable_fragment(fragment) else {
            report.record(Finding::new(
                codes::RECORD_INVALID,
                owner_kind,
                owner_id,
                "portable F0 value shape is invalid",
            ));
            return Ok(());
        };
        for mut reference in references {
            // Native row identities stay separate from the upstream F0 display identities.
            if owner_kind == "preset" || owner_kind == "plugin" {
                reference.owner_kind = if owner_kind == "plugin" { "plugin-storage" } else { "preset" }.to_owned();
                reference.owner_id = owner_id.to_owned();
            }
            check(probe)?;
            counts.total += 1;
            if matches!(reference.status, F0ReferenceStatus::Invalid) {
                report.note(reference_finding(
                    codes::REFERENCE_INVALID,
                    &reference,
                    "reference value cannot be resolved",
                ));
                continue;
            }
            if matches!(reference.status, F0ReferenceStatus::External) {
                continue;
            }
            if !self.reference_present(&reference, root)? {
                counts.expected_missing += 1;
                report.note(reference_finding(
                    codes::REFERENCE_MISSING,
                    &reference,
                    "reference has no target in this library",
                ));
            }
        }
        // Complete v2 aliases and raw rows prove absence. Existing dangling references remain
        // expected missing, as in the previous F0 capture contract; registered missing bytes fail
        // above and cannot be reclassified as a harmless absent reference.
        Ok(())
    }

    fn reference_present(&self, r: &F0Reference, root: &Value) -> Result<bool> {
        let key = &r.target_key;
        Ok(match r.target_kind.as_str() {
            "asset" | "inlay" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM asset_aliases WHERE kind=?1 AND logical_key=?2)",
                params![r.target_kind, key],
                |row| row.get(0),
            )?,
            "character" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM characters WHERE character_id=?1)",
                [key],
                |row| row.get(0),
            )?,
            "preset" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM bot_presets WHERE name=?1)",
                [key],
                |row| row.get(0),
            )?,
            "module" => {
                named(root, "modules", "id", key)
                    || root
                        .get("personas")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|v| {
                                v.pointer("/embeddedModule/id").and_then(Value::as_str) == Some(key)
                            })
                        })
            }
            "persona" => named(root, "personas", "id", key),
            "loadout" => named(root, "loadouts", "name", key),
            "conversation" | "folder" => {
                let scope = r
                    .metadata
                    .get("characterId")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        matches!(r.owner_kind.as_str(), "character")
                            .then_some(r.owner_id.as_str())
                    });
                if let Some(scope) = scope {
                    if r.target_kind == "conversation" {
                        self.db.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE character_id=?1 AND conversation_id=?2)",params![scope,key],|row|row.get(0))?
                    } else {
                        let detail: Option<String> = self
                            .db
                            .query_row(
                                "SELECT detail FROM characters WHERE character_id=?1",
                                [scope],
                                |row| row.get(0),
                            )
                            .optional()?;
                        detail
                            .map(|v| serde_json::from_str::<Value>(&v))
                            .transpose()?
                            .is_some_and(|v| named(&v, "chatFolders", "id", key))
                    }
                } else {
                    false
                }
            }
            _ => return Err(Error::Invalid("unknown F0 reference kind")),
        })
    }

    fn validate_owners(
        &self,
        root: &Value,
        report: &mut Report<'_>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let mut statement=self.db.prepare("SELECT owner_kind,owner_locator,present,manifest_hash,entry_count FROM asset_owner_heads ORDER BY owner_kind,owner_locator")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            if !report.running() {
                return Ok(());
            }
            let kind: String = row.get(0)?;
            let locator: String = row.get(1)?;
            if let Err(error) = self.validate_owner(row, &kind, &locator, root, probe) {
                let detail = match &error {
                    Error::Invalid(message) => *message,
                    Error::Json(_) => "owner record JSON is invalid",
                    _ => return Err(error),
                };
                report.record(Finding::new(codes::RECORD_INVALID, kind, locator, detail));
            }
        }
        Ok(())
    }

    fn validate_owner(
        &self,
        row: &rusqlite::Row<'_>,
        kind: &str,
        locator: &str,
        root: &Value,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let present: bool = row.get(2)?;
        let (character, archived): (Option<Value>, bool) = if kind == "character-additional-assets" {
            match self
                .db
                .query_row(
                    "SELECT detail,archived_object IS NOT NULL FROM characters WHERE character_id=?1",
                    [locator],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
                )
                .optional()?
            {
                Some((detail, archived)) => (Some(serde_json::from_str(&detail)?), archived),
                None => (None, false),
            }
        } else {
            (None, false)
        };
        let owner = crate::persistent_store::AssetOwnerLocator::from_storage(kind, locator)
            .map_err(|_| Error::Invalid("invalid owner identity"))?;
        let (parent, property) = match &owner {
            crate::persistent_store::AssetOwnerLocator::CharacterAdditionalAssets { .. } => (character.as_ref(), "additionalAssets"),
            _ => (owner.root_parent(root), "assets"),
        };
        let parent = parent
            .and_then(Value::as_object)
            .ok_or(Error::Invalid("owner parent is missing"))?;
        // An archived character's additional assets are listed in its archive. Its owner manifest
        // stays, and only the manifest itself can be checked.
        if !archived && parent.contains_key(property) != present {
            return Err(Error::Invalid("owner property presence differs"));
        }
        if !present {
            return Ok(());
        }
        let hash: String = row.get(3)?;
        let (mut input, size) = self.open_object(&hash)?;
        if size > 64 * 1024 * 1024 {
            return Err(Error::Invalid("owner manifest exceeds decoding limit"));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        input.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != size || hex::encode(Sha256::digest(&bytes)) != hash {
            return Err(Error::Invalid("owner manifest identity differs"));
        }
        let entries = crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes)
            .map_err(|_| Error::Invalid("invalid owner manifest"))?;
        let tuples = match archived {
            true => None,
            false => Some(
                parent
                    .get(property)
                    .and_then(Value::as_array)
                    .ok_or(Error::Invalid("owner property is not an array"))?,
            ),
        };
        if entries.len()
            != usize::try_from(row.get::<_, i64>(4)?)
                .map_err(|_| Error::Invalid("owner count overflow"))?
            || tuples.is_some_and(|tuples| entries.len() != tuples.len())
        {
            return Err(Error::Invalid("owner entry count differs"));
        }
        for (index, entry) in entries.iter().enumerate() {
            check(probe)?;
            if let Some(tuples) = tuples {
                let tuple = tuples[index]
                    .as_array()
                    .ok_or(Error::Invalid("invalid owner tuple"))?;
                if tuple.len() < 3
                    || (kind == "character-additional-assets" && tuple.len() != 3)
                    || !tuple[..3]
                        .iter()
                        .zip(&entry.tuple)
                        .all(|(a, b)| a.as_str() == Some(b))
                {
                    return Err(Error::Invalid("owner tuples differ"));
                }
            }
            if let Some(hash) = entry.payload_hash {
                if !self.objects.has_object(self.db, hash.as_slice())? {
                    return Err(Error::Invalid("owner historical payload is missing"));
                }
            }
        }
        Ok(())
    }
}

fn reference_finding(code: &'static str, reference: &F0Reference, detail: &str) -> Finding {
    let finding = Finding::new(code, &reference.owner_kind, &reference.owner_id, detail)
        .at(&reference.source_path, reference.occurrence)
        .targeting(&reference.target_kind, &reference.target_key);
    match reference.metadata.get("tokenIndex").and_then(Value::as_u64) {
        Some(token) if reference.target_kind == "inlay" => finding.in_token(token),
        _ => finding,
    }
}

fn named(value: &Value, collection: &str, field: &str, key: &str) -> bool {
    value
        .get(collection)
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|v| v.get(field).and_then(Value::as_str) == Some(key))
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_health::Severity;

    #[test]
    fn only_rule_failures_become_an_unclassified_finding() {
        for error in [
            Error::Invalid("synthetic rule failure"),
            Error::Json(serde_json::from_str::<Value>("{").expect_err("invalid JSON")),
            Error::Store(StoreError::Validation {
                message: "synthetic rule failure".to_owned(),
            }),
        ] {
            let finding = unclassified(error).expect("a rule failure is reported, not propagated");
            assert_eq!(finding.code, codes::UNCLASSIFIED);
            assert_eq!(finding.severity, Severity::Blocking);
        }
        assert!(unclassified(Error::Cancelled).is_err());
        assert!(unclassified(Error::Store(StoreError::SnapshotReleased)).is_err());
    }
}

/// The stored root selects a preset by its `id`; an index selects by configured order.
fn selected_preset(db: &rusqlite::Connection, selection: Option<&Value>) -> Result<Option<Value>> {
    let row = match selection {
        Some(Value::String(id)) => db.query_row(
            "SELECT value FROM bot_presets WHERE preset_id=?1",
            [id],
            |r| r.get::<_, String>(0),
        ),
        selection => {
            let Some(index) = selection
                .and_then(Value::as_u64)
                .and_then(|n| i64::try_from(n).ok())
            else {
                return Ok(None);
            };
            db.query_row(
                "SELECT value FROM bot_presets ORDER BY configured_index LIMIT 1 OFFSET ?1",
                [index],
                |r| r.get::<_, String>(0),
            )
        }
    };
    Ok(row
        .optional()?
        .map(|v| serde_json::from_str(&v))
        .transpose()?)
}
