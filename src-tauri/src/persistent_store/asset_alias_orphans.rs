//! `asset` aliases nothing in the library refers to any more. The unused-file cleanup removes
//! the ones deleted owners left behind, and a permanent delete removes the ones only the deleted
//! character, module or persona used.

use super::device_store::plugin_gc::PluginGcFence;
use super::device_store::plugin_values::text_value;
use super::{lww, message_pages, snapshot, PersistentStore, StoreError, StoreResult, WorkingSetCommit};
use crate::asset_repository::migration_gc::{mark_asset_roots_with_remote, AssetGcMarks, AssetRootSet};
use base64::Engine;
use risunest_sync_wire::{stamp::Stamp, unit::{UnitKey, UnitValue}};
use rusqlite::{types::ValueRef, Connection, OptionalExtension};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt::Write as _,
    io,
};

/// Characters, modules and personas keep their files under this prefix. Other `asset` aliases,
/// such as sync conflict backups, are reached by their own owners and never count as unused.
pub(crate) const OWNED_ASSET_KEY_PREFIX: &str = "assets/";

/// Every encoded form of an owned key starts with this word, whatever its separator became.
const ANCHOR: &str = "assets";

/// The one device setting that names keys only to remember what an account already received.
const PUBLICATION_LEDGER_SETTING: &str = "official-account.asset-ledger.v1";

/// Which unreferenced aliases a run may delete.
pub(crate) enum OrphanAliasScope<'a> {
    /// The unused-file cleanup: any alias last written before `now_ms` less the file grace.
    Cleanup { now_ms: i64 },
    /// A permanent delete: the owner's keys, last written before the delete started.
    DeletedOwner { keys: &'a BTreeSet<String>, started_at_ms: i64 },
}

impl OrphanAliasScope<'_> {
    fn deletes_now(&self, written_at_ms: u64) -> bool {
        let written_at_ms = i64::try_from(written_at_ms).unwrap_or(i64::MAX);
        match self {
            Self::Cleanup { now_ms } => written_at_ms <= now_ms.saturating_sub(super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS),
            Self::DeletedOwner { started_at_ms, .. } => written_at_ms < *started_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnreferencedAlias {
    pub(crate) key: String,
    pub(crate) object_hash: String,
    written_at_ms: u64,
}

/// The aliases one search found unreferenced.
pub(crate) struct OrphanAliasSearch {
    revision: i64,
    /// The received rows waiting to apply that the references were read from.
    pending_fence: [u8; 32],
    /// The device plugin values the references were read from, when any were read.
    device_fence: Option<PluginGcFence>,
    unreferenced: Vec<UnreferencedAlias>,
}

impl OrphanAliasSearch {
    /// The unreferenced aliases whose objects no other root holds, split into the ones `scope`
    /// deletes now and the ones it keeps for now.
    pub(crate) fn orphans(&self, marks: &AssetGcMarks, scope: &OrphanAliasScope<'_>) -> (Vec<UnreferencedAlias>, Vec<UnreferencedAlias>) {
        if marks.is_blocked() {
            return (Vec::new(), Vec::new());
        }
        self.unreferenced
            .iter()
            .filter(|alias| !marks.holds(&alias.object_hash))
            .cloned()
            .partition(|alias| scope.deletes_now(alias.written_at_ms))
    }
}

impl PersistentStore {
    /// Finds the owned `asset` aliases of the active generation that `scope` covers and nothing
    /// references. `None` when some stored text could not be read, so no alias can be shown
    /// unreferenced.
    pub(crate) fn search_unreferenced_asset_aliases(
        &self,
        scope: &OrphanAliasScope<'_>,
    ) -> StoreResult<Option<OrphanAliasSearch>> {
        let revision = self.revision()?;
        let pending_fence = super::gallery::pending_fence(&self.connection)?;
        let generation = super::active_generation(&self.connection)?;
        let writes = owned_alias_writes(&self.connection, &generation, scope)?;
        let mut matcher = AssetKeyMatcher::new(writes.keys().cloned());
        let mut device_fence = None;
        if !matcher.is_complete() {
            let Some(fence) = self.observe_asset_key_references(&generation, &mut matcher)? else {
                return Ok(None);
            };
            device_fence = Some(fence);
        }
        let unreferenced_keys = matcher.unreferenced().collect::<BTreeSet<_>>();
        let unreferenced = writes
            .into_iter()
            .filter(|(key, _)| unreferenced_keys.contains(key))
            .map(|(key, (object_hash, written_at_ms))| UnreferencedAlias { key, object_hash, written_at_ms })
            .collect();
        Ok(Some(OrphanAliasSearch { revision, pending_fence, device_fence, unreferenced }))
    }

    /// Every cleanup root, the device plugin values included, with the aliases `search` found
    /// unreferenced left out, labelled as the cleanup labels them.
    pub(crate) fn collect_asset_gc_roots_without_unreferenced_aliases(
        &self,
        search: &OrphanAliasSearch,
        read_only: bool,
    ) -> StoreResult<Vec<(&'static str, AssetRootSet)>> {
        let (fence, device_roots) = snapshot::collect_device_plugin_asset_roots(self)?;
        if search.device_fence.as_ref().is_some_and(|searched| *searched != fence) {
            return Err(StoreError::CommitBusy);
        }
        let excluded = search.unreferenced.iter().map(|alias| alias.key.clone()).collect();
        let mut roots = vec![
            ("library", snapshot::collect_asset_roots_without_aliases(&self.connection, &excluded)?),
            ("library", device_roots),
        ];
        roots.extend(self.collect_auxiliary_asset_gc_roots(false, read_only, None)?);
        Ok(roots)
    }

    /// Deletes the aliases `scope` finds orphaned in one commit, returning them. Nothing is
    /// deleted while any root cannot be read, and a library or device plugin value that changed
    /// since the search returns busy.
    pub(crate) fn delete_orphaned_asset_aliases(
        &mut self,
        scope: &OrphanAliasScope<'_>,
    ) -> StoreResult<Vec<UnreferencedAlias>> {
        let Some(search) = self.search_unreferenced_asset_aliases(scope)? else {
            return Ok(Vec::new());
        };
        if search.unreferenced.is_empty() {
            return Ok(Vec::new());
        }
        let roots = self.collect_asset_gc_roots_without_unreferenced_aliases(&search, false)?;
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let residency = crate::server_sync::residency::Residency::open(&self.repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let marks = mark_asset_roots_with_remote(&cas, roots.into_iter().map(|(_, roots)| roots), |hash| residency.gc_size(hash))?;
        let (orphans, _) = search.orphans(&marks, scope);
        if orphans.is_empty() {
            return Ok(Vec::new());
        }
        if self.revision()? != search.revision {
            return Err(StoreError::CommitBusy);
        }
        let unit_mutations = orphans
            .iter()
            .map(|alias| Ok(lww::UnitMutation::Delete { key: lww::unit_key(&["asset", alias.key.as_str()])? }))
            .collect::<StoreResult<Vec<_>>>()?;
        // Other connections also write device plugin values, so their writers stay out from the
        // fence check until the aliases are committed.
        self.commit_holding(
            &WorkingSetCommit {
                expected_revision: search.revision,
                // Rows can be received without moving the revision, so the commit also requires
                // the ones the search read.
                gallery_pending_fence: Some(search.pending_fence),
                // Recovery of an interrupted commit checks the plugin values against it too.
                device_plugin_fence: search.device_fence.clone(),
                unit_mutations: Some(unit_mutations),
                ..Default::default()
            },
            |store| match &search.device_fence {
                Some(fence) => Ok(Some(store.device_store()?.acquire_plugin_gc_barrier(fence)?)),
                None => Ok(None),
            },
        )?;
        Ok(orphans)
    }

    /// Marks every key `matcher` finds in the active generation, its owner manifests, the stored
    /// and received units, device plugin values and device settings. Returns the device plugin
    /// fence it read, or `None` when some text could not be read.
    fn observe_asset_key_references(
        &self,
        generation: &str,
        matcher: &mut AssetKeyMatcher,
    ) -> StoreResult<Option<PluginGcFence>> {
        let mut readable = true;
        for (query, plugin) in [
            ("SELECT value FROM root WHERE generation = ?1", false),
            ("SELECT name, image, value FROM bot_presets WHERE generation = ?1", false),
            ("SELECT name, image, creator_notes, detail FROM characters WHERE generation = ?1", false),
            ("SELECT name, detail FROM conversations WHERE generation = ?1", false),
            ("SELECT value FROM messages WHERE generation = ?1", false),
            ("SELECT owner, storage_key, value FROM plugin_storage WHERE generation = ?1", true),
        ] {
            readable &= observe_text_rows(&self.connection, query, [generation], plugin, matcher)?;
        }
        readable &= self.observe_owner_manifests(generation, matcher)?;
        readable &= observe_units(&self.connection, matcher)?;
        let device = self.device_store()?;
        readable &= observe_text_rows(
            device.connection(),
            "SELECT value FROM device_settings WHERE key <> ?1",
            [PUBLICATION_LEDGER_SETTING],
            false,
            matcher,
        )?;
        let device_readable = Cell::new(true);
        let objects = RefCell::new(Vec::new());
        let fence = {
            let shared = RefCell::new(&mut *matcher);
            device.visit_plugin_gc_values(
                |owner, space, key, value| {
                    let mut matcher = shared.borrow_mut();
                    for text in [owner, space, key] {
                        matcher.observe(text, true);
                    }
                    let mut stream = matcher.stream(true);
                    if text_value::visit_text(value, |text| { stream.push(text); Ok(()) }).is_err() {
                        device_readable.set(false);
                    }
                    stream.finish();
                    Ok(())
                },
                |_, value| {
                    match value {
                        UnitValue::Inline { .. } => match inline_text(value) {
                            Some(text) => shared.borrow_mut().observe(&text, true),
                            None => device_readable.set(false),
                        },
                        UnitValue::Object { .. } => objects.borrow_mut().push(value.clone()),
                        UnitValue::Deleted => {}
                    }
                    Ok(())
                },
            )?
        };
        // Plugin-local units are device units, so their large bodies live in the device store.
        for value in objects.into_inner() {
            let UnitValue::Object { descriptor, .. } = &value else { continue };
            let result = (|| -> StoreResult<()> {
                let db = device.connection();
                let (rowid, length) = text_value::object_row(db, &descriptor.object_hash)?;
                if length <= text_value::PART_BYTES as u64 {
                    let body = message_pages::object_body(db, &descriptor.object_hash)?
                        .ok_or_else(|| unreadable("plugin object body is missing"))?;
                    let text = std::str::from_utf8(&body).map_err(|_| unreadable("plugin object is not text"))?;
                    matcher.observe(text, true);
                    return Ok(());
                }
                let input = db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
                let mut output = TextWriter { stream: matcher.stream(true), held: Vec::new() };
                let info = text_value::decode(input, &mut output, text_value::MAX_VALUE_BYTES)?;
                if info.hash != descriptor.object_hash || !output.held.is_empty() {
                    return Err(unreadable("plugin object hash mismatch"));
                }
                output.stream.finish();
                Ok(())
            })();
            readable &= result.is_ok();
        }
        Ok((readable && device_readable.get()).then_some(fence))
    }

    fn observe_owner_manifests(&self, generation: &str, matcher: &mut AssetKeyMatcher) -> StoreResult<bool> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let mut statement = self.connection.prepare(
            "SELECT manifest_hash FROM asset_owner_heads
             WHERE generation = ?1 AND present = 1 AND manifest_hash IS NOT NULL",
        )?;
        let hashes = statement.query_map([generation], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for hash in hashes {
            if matcher.is_complete() {
                break;
            }
            let Some(bytes) = cas.read_object(&hash)? else { return Ok(false) };
            let Ok(entries) = crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes) else {
                return Ok(false);
            };
            for entry in entries {
                for text in &entry.tuple {
                    matcher.observe(text, false);
                }
            }
        }
        Ok(true)
    }
}

/// Owned aliases of `generation` that `scope` covers, with their object and the physical time of
/// their unit's stamp. An alias without a unit has no known write time and is left out.
fn owned_alias_writes(
    connection: &Connection,
    generation: &str,
    scope: &OrphanAliasScope<'_>,
) -> StoreResult<BTreeMap<String, (String, u64)>> {
    let keys = match scope {
        OrphanAliasScope::DeletedOwner { keys, .. } => keys.iter().cloned().collect(),
        OrphanAliasScope::Cleanup { .. } => {
            let mut statement = connection.prepare(
                "SELECT logical_key FROM asset_aliases
                 WHERE generation = ?1 AND kind = 'asset' AND object_hash IS NOT NULL",
            )?;
            let keys = statement.query_map([generation], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            keys
        }
    };
    let mut alias = connection.prepare(
        "SELECT object_hash FROM asset_aliases
         WHERE generation = ?1 AND kind = 'asset' AND logical_key = ?2 AND object_hash IS NOT NULL",
    )?;
    let mut unit = connection.prepare("SELECT stamp FROM lww_units WHERE key = ?1")?;
    let mut writes = BTreeMap::new();
    for key in keys {
        if !key.starts_with(OWNED_ASSET_KEY_PREFIX) {
            continue;
        }
        let Some(object_hash) = alias.query_row(rusqlite::params![generation, key], |row| row.get::<_, String>(0)).optional()? else {
            continue;
        };
        let Some(stamp) = unit.query_row([lww::unit_key(&["asset", key.as_str()])?.as_str()], |row| row.get::<_, String>(0)).optional()? else {
            continue;
        };
        let Ok(stamp) = serde_json::from_str::<Stamp>(&stamp) else { continue };
        writes.insert(key, (object_hash, stamp.physical_ms.0));
    }
    Ok(writes)
}

/// Feeds every text column of every row to `matcher`, returning false when a value is not text.
fn observe_text_rows<P: rusqlite::Params>(
    connection: &Connection,
    query: &str,
    params: P,
    plugin: bool,
    matcher: &mut AssetKeyMatcher,
) -> StoreResult<bool> {
    let mut statement = connection.prepare(query)?;
    let columns = statement.column_count();
    let mut rows = statement.query(params)?;
    while let Some(row) = rows.next()? {
        if matcher.is_complete() {
            break;
        }
        for column in 0..columns {
            match row.get_ref(column)? {
                ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                    Ok(text) => matcher.observe(text, plugin),
                    Err(_) => return Ok(false),
                },
                ValueRef::Null => {}
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}

/// Units outside the projected tables can still hold a reference: stored units of kinds this
/// version does not project, received units waiting to apply and a snapshot's original units.
/// Alias units only name themselves, archive units have their own roots and stored message units
/// are projected, so only the pages of message units not applied yet are read.
fn observe_units(connection: &Connection, matcher: &mut AssetKeyMatcher) -> StoreResult<bool> {
    let mut unapplied_messages = 0;
    for table in ["lww_units", "lww_receive_rows", "snapshot_original_units"] {
        let predicate = if table == "lww_receive_rows" { " WHERE status IN ('staged','held','deferred')" } else { "" };
        let mut statement = connection.prepare(&format!("SELECT key,value FROM {table}{predicate}"))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if matcher.is_complete() {
                return Ok(true);
            }
            let Ok(key) = UnitKey::try_from(row.get::<_, String>(0)?) else { return Ok(false) };
            if key.as_str().starts_with("[\"asset\",") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<UnitValue>(&row.get::<_, String>(1)?) else { return Ok(false) };
            match &value {
                UnitValue::Inline { .. } => match inline_text(&value) {
                    Some(text) => matcher.observe(&text, false),
                    None => return Ok(false),
                },
                UnitValue::Object { descriptor, .. } if super::external_capture::is_large_unit(&key) => {
                    let Some(body) = message_pages::object_body(connection, &descriptor.object_hash)? else { return Ok(false) };
                    let Ok(text) = std::str::from_utf8(&body) else { return Ok(false) };
                    matcher.observe(text, false);
                }
                UnitValue::Object { descriptor, .. }
                    if table != "lww_units" && key.components().first().is_some_and(|kind| kind == "messages") =>
                {
                    unapplied_messages += 1;
                    if unapplied_messages > UNAPPLIED_MESSAGE_UNITS_READ
                        || !observe_message_pages(connection, &descriptor.object_hash, matcher)?
                    {
                        return Ok(false);
                    }
                }
                UnitValue::Object { .. } | UnitValue::Deleted => {}
            }
        }
    }
    Ok(true)
}

/// Message units not applied yet that one search reads page by page. More, as while a whole
/// library is being received, leave every alias in place until they are applied.
const UNAPPLIED_MESSAGE_UNITS_READ: usize = 256;

/// Feeds every string of the messages a manifest's pages hold to `matcher`, returning false when
/// a body has not arrived or cannot be decoded.
fn observe_message_pages(connection: &Connection, manifest_hash: &str, matcher: &mut AssetKeyMatcher) -> StoreResult<bool> {
    use risunest_external_storage_format::{logical_records::decode_message_page, message_pages::MessageManifest};
    let Some(body) = message_pages::object_body(connection, manifest_hash)? else { return Ok(false) };
    let Ok(manifest) = MessageManifest::decode(&body) else { return Ok(false) };
    for page in manifest.pages {
        if matcher.is_complete() {
            break;
        }
        let Some(body) = message_pages::object_body(connection, &page.hash)? else { return Ok(false) };
        let Ok(messages) = decode_message_page(&body) else { return Ok(false) };
        for message in &messages {
            observe_json_strings(message, matcher);
        }
    }
    Ok(true)
}

fn observe_json_strings(value: &serde_json::Value, matcher: &mut AssetKeyMatcher) {
    match value {
        serde_json::Value::String(text) => matcher.observe(text, false),
        serde_json::Value::Array(values) => values.iter().for_each(|value| observe_json_strings(value, matcher)),
        serde_json::Value::Object(values) => values.values().for_each(|value| observe_json_strings(value, matcher)),
        _ => {}
    }
}

fn inline_text(value: &UnitValue) -> Option<String> {
    let UnitValue::Inline { bytes } = value else { return None };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(bytes).ok()?;
    String::from_utf8(decoded).ok()
}

fn unreadable(message: &str) -> StoreError {
    StoreError::Validation { message: message.to_owned() }
}

/// Finds asset keys anywhere in stored text, in the forms a reader still resolves: the key, the
/// key with `\` separators, and either one JSON-escaped. Each `assets` in the text is looked up by
/// the lengths of those forms, so the cost follows the text, not the number of keys. Plugin text
/// is also searched for keys without the prefix, which the plugin image reader adds back.
pub(super) struct AssetKeyMatcher {
    keys: Vec<String>,
    forms: HashMap<Box<[u8]>, usize>,
    bare: HashMap<Box<[u8]>, usize>,
    /// The extra keys of a form two keys share.
    shared: HashMap<Box<[u8]>, Vec<usize>>,
    lengths: Vec<usize>,
    /// The offset of the last `.` and the length of each prefixless form, by which plugin text is searched.
    bare_shapes: Vec<(usize, usize)>,
    longest: usize,
    found: Vec<bool>,
    remaining: usize,
}

impl AssetKeyMatcher {
    pub(super) fn new(keys: impl IntoIterator<Item = String>) -> Self {
        let keys = keys.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
        let mut matcher = Self {
            forms: HashMap::new(),
            bare: HashMap::new(),
            shared: HashMap::new(),
            lengths: Vec::new(),
            bare_shapes: Vec::new(),
            longest: 0,
            found: vec![false; keys.len()],
            remaining: keys.len(),
            keys: Vec::new(),
        };
        let mut lengths = BTreeSet::new();
        let mut shapes = BTreeSet::new();
        for (index, key) in keys.iter().enumerate() {
            let separated = key.replace('/', "\\");
            for form in escaped_forms(key).into_iter().chain(escaped_forms(&separated)) {
                lengths.insert(form.len());
                matcher.insert(false, form, index);
            }
            let Some(rest) = key.strip_prefix(OWNED_ASSET_KEY_PREFIX) else { continue };
            for form in escaped_forms(rest) {
                let Some(dot) = form.rfind('.') else { continue };
                shapes.insert((dot, form.len()));
                matcher.insert(true, form, index);
            }
        }
        matcher.longest = lengths.iter().chain(shapes.iter().map(|(_, length)| length)).copied().max().unwrap_or(0);
        matcher.lengths = lengths.into_iter().collect();
        matcher.bare_shapes = shapes.into_iter().collect();
        matcher.keys = keys;
        matcher
    }

    fn insert(&mut self, bare: bool, form: String, index: usize) {
        let form = form.into_bytes().into_boxed_slice();
        let forms = if bare { &mut self.bare } else { &mut self.forms };
        match forms.get(&form) {
            Some(&first) if first != index => {
                let shared = self.shared.entry(form).or_default();
                if !shared.contains(&index) {
                    shared.push(index);
                }
            }
            Some(_) => {}
            None => {
                forms.insert(form, index);
            }
        }
    }

    pub(super) fn is_complete(&self) -> bool {
        self.remaining == 0
    }

    pub(super) fn observe(&mut self, text: &str, plugin: bool) {
        self.scan(text, text.len(), plugin);
    }

    pub(super) fn stream(&mut self, plugin: bool) -> KeyStream<'_> {
        KeyStream { matcher: self, pending: String::new(), plugin }
    }

    pub(super) fn unreferenced(&self) -> impl Iterator<Item = String> + '_ {
        self.keys.iter().zip(&self.found).filter(|(_, found)| !**found).map(|(key, _)| key.clone())
    }

    /// Looks up every form that starts before `cut` and fits in `text`.
    fn scan(&mut self, text: &str, cut: usize, plugin: bool) {
        if self.remaining == 0 {
            return;
        }
        let bytes = text.as_bytes();
        for (start, _) in text.match_indices(ANCHOR) {
            if start >= cut {
                break;
            }
            for index in 0..self.lengths.len() {
                let Some(candidate) = bytes.get(start..start + self.lengths[index]) else { break };
                if let Some(&key) = self.forms.get(candidate) {
                    self.mark(key, candidate);
                }
            }
        }
        if !plugin || self.bare_shapes.is_empty() {
            return;
        }
        let reach = cut.saturating_add(self.bare_shapes.iter().map(|(dot, _)| *dot).max().unwrap_or(0));
        for (dot, _) in text.match_indices('.') {
            if dot >= reach {
                break;
            }
            for index in 0..self.bare_shapes.len() {
                let (offset, length) = self.bare_shapes[index];
                let Some(start) = dot.checked_sub(offset).filter(|start| *start < cut) else { continue };
                let Some(candidate) = bytes.get(start..start + length) else { continue };
                if let Some(&key) = self.bare.get(candidate) {
                    self.mark(key, candidate);
                }
            }
        }
    }

    fn mark(&mut self, key: usize, form: &[u8]) {
        let shared = self.shared.get(form).map(Vec::as_slice).unwrap_or_default();
        for &index in std::iter::once(&key).chain(shared) {
            if !self.found[index] {
                self.found[index] = true;
                self.remaining -= 1;
            }
        }
    }
}

/// Text read in pieces, keeping the tail a form could still be starting in.
pub(super) struct KeyStream<'a> {
    matcher: &'a mut AssetKeyMatcher,
    pending: String,
    plugin: bool,
}

impl KeyStream<'_> {
    pub(super) fn push(&mut self, text: &str) {
        if self.matcher.is_complete() {
            return;
        }
        self.pending.push_str(text);
        let keep = self.matcher.longest.saturating_sub(1);
        if self.pending.len() <= keep {
            return;
        }
        let mut cut = self.pending.len() - keep;
        while !self.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        self.matcher.scan(&self.pending, cut, self.plugin);
        self.pending.drain(..cut);
    }

    pub(super) fn finish(self) {
        let length = self.pending.len();
        self.matcher.scan(&self.pending, length, self.plugin);
    }
}

/// Decoded text arrives in arbitrary byte pieces, so a character split between two is held.
struct TextWriter<'a> {
    stream: KeyStream<'a>,
    held: Vec<u8>,
}

impl io::Write for TextWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.held) {
            Ok(_) => self.held.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "plugin text is invalid")),
        };
        let text = std::str::from_utf8(&self.held[..valid]).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        self.stream.push(text);
        self.held.drain(..valid);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// `text`, its JSON-escaped form, and both with `/` escaped as `\/`; non-ASCII text also with
/// every non-ASCII character as a lowercase or uppercase `\u` escape.
fn escaped_forms(text: &str) -> BTreeSet<String> {
    let json = serde_json::to_string(text).expect("a string always serializes");
    let json = json[1..json.len() - 1].to_owned();
    let mut escaped = vec![json];
    if !text.is_ascii() {
        escaped.push(ascii_escaped(&escaped[0], false));
        escaped.push(ascii_escaped(&escaped[0], true));
    }
    let mut forms = BTreeSet::from([text.to_owned()]);
    for form in escaped {
        forms.insert(form.replace('/', "\\/"));
        forms.insert(form);
    }
    forms
}

fn ascii_escaped(text: &str, uppercase: bool) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii() {
            escaped.push(character);
            continue;
        }
        let mut units = [0_u16; 2];
        for unit in character.encode_utf16(&mut units) {
            let _ = if uppercase { write!(escaped, "\\u{unit:04X}") } else { write!(escaped, "\\u{unit:04x}") };
        }
    }
    escaped
}

impl PersistentStore {
    /// The owned keys the conversations of `character_ids` name in their details and messages, by
    /// the rules the renderer applies to a deleted owner's own data, and the keys of archived
    /// characters, up to `limit` keys.
    pub(crate) fn character_conversation_asset_keys(
        &self,
        character_ids: &[String],
        limit: usize,
    ) -> StoreResult<BTreeSet<String>> {
        let generation = super::active_generation(&self.connection)?;
        let mut keys = BTreeSet::new();
        for (table, column) in [("conversations", "detail"), ("messages", "value")] {
            let mut statement = self.connection.prepare(&format!(
                "SELECT {column} FROM {table} WHERE generation = ?1 AND character_id = ?2"
            ))?;
            for character_id in character_ids {
                let mut rows = statement.query(rusqlite::params![generation, character_id])?;
                while let Some(row) = rows.next()? {
                    let text = row.get::<_, String>(0)?;
                    // Most rows name no key, so only those that might are parsed.
                    if !text.contains(ANCHOR) {
                        continue;
                    }
                    let mut pending = vec![serde_json::from_str::<serde_json::Value>(&text)?];
                    while let Some(value) = pending.pop() {
                        match value {
                            serde_json::Value::String(text) => collect_owned_keys(&text, &mut keys),
                            serde_json::Value::Array(values) => pending.extend(values),
                            serde_json::Value::Object(values) => pending.extend(values.into_iter().map(|(_, value)| value)),
                            _ => {}
                        }
                    }
                    if keys.len() >= limit {
                        return Ok(keys.into_iter().take(limit).collect());
                    }
                }
            }
        }
        // An archived character's details and chats are inside its archive, which the renderer
        // cannot read, so its keys are the owned aliases of the objects the archive recorded.
        let mut archived_hashes = BTreeSet::new();
        for character_id in character_ids {
            if let Some(archived) = super::archive::read_archived_object(&self.connection, &generation, character_id)? {
                archived_hashes.extend(archived.asset_hashes.into_iter().chain(archived.shared_asset_hashes));
            }
        }
        if archived_hashes.is_empty() {
            return Ok(keys);
        }
        let mut statement = self.connection.prepare(
            "SELECT logical_key, object_hash FROM asset_aliases
             WHERE generation = ?1 AND kind = 'asset' AND object_hash IS NOT NULL",
        )?;
        let mut rows = statement.query([&generation])?;
        while let Some(row) = rows.next()? {
            let key = row.get::<_, String>(0)?;
            if !archived_hashes.contains(&row.get::<_, String>(1)?) || owned_key(&key).as_ref() != Some(&key) {
                continue;
            }
            keys.insert(key);
            if keys.len() >= limit {
                return Ok(keys.into_iter().take(limit).collect());
            }
        }
        Ok(keys)
    }
}

/// The longest key, in UTF-8 bytes, a deleted owner's cleanup accepts.
const OWNED_KEY_MAX_BYTES: usize = 4096;

fn owned_key(text: &str) -> Option<String> {
    let key = text.replace('\\', "/");
    (key.len() > OWNED_ASSET_KEY_PREFIX.len() && key.len() <= OWNED_KEY_MAX_BYTES && key.starts_with(OWNED_ASSET_KEY_PREFIX))
        .then_some(key)
}

/// Adds `text` when it is an owned key itself, and each key inside it, such as an `<img src>` or
/// a `{{raw::}}` reference.
fn collect_owned_keys(text: &str, keys: &mut BTreeSet<String>) {
    keys.extend(owned_key(text));
    let mut rest = text;
    while let Some(start) = rest.find(ANCHOR) {
        let after = &rest[start + ANCHOR.len()..];
        let Some(body) = after.strip_prefix(['/', '\\']) else {
            rest = after;
            continue;
        };
        let end = body
            .find(|c: char| c.is_whitespace() || "\"'`<>()[]{}|,;\\".contains(c))
            .unwrap_or(body.len());
        if end > 0 {
            keys.extend(owned_key(&format!("{OWNED_ASSET_KEY_PREFIX}{}", &body[..end])));
        }
        rest = &body[end..];
    }
}

/// Adds the owned keys every string in `value` names, as `collect_owned_keys` finds them.
pub(super) fn collect_value_owned_keys(value: &serde_json::Value, keys: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(text) => collect_owned_keys(text, keys),
        serde_json::Value::Array(values) => values.iter().for_each(|value| collect_value_owned_keys(value, keys)),
        serde_json::Value::Object(values) => values.values().for_each(|value| collect_value_owned_keys(value, keys)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::AssetKeyMatcher;

    fn unreferenced(keys: &[&str], texts: &[&str], plugin: bool) -> Vec<String> {
        let mut matcher = AssetKeyMatcher::new(keys.iter().map(|key| key.to_string()));
        for text in texts {
            matcher.observe(text, plugin);
        }
        matcher.unreferenced().collect()
    }

    fn streamed(keys: &[&str], text: &str, piece: usize, plugin: bool) -> Vec<String> {
        let mut matcher = AssetKeyMatcher::new(keys.iter().map(|key| key.to_string()));
        let mut stream = matcher.stream(plugin);
        let mut rest = text;
        while !rest.is_empty() {
            let mut end = piece.min(rest.len());
            while !rest.is_char_boundary(end) {
                end += 1;
            }
            stream.push(&rest[..end]);
            rest = &rest[end..];
        }
        stream.finish();
        matcher.unreferenced().collect()
    }

    #[test]
    fn keys_are_found_anywhere_in_text_in_every_resolved_form() {
        let keys = ["assets/a.png", "assets/b.png", "assets/c.png", "assets/d.png", "assets/e.png"];
        let texts = [
            r#"{"image":"assets/a.png"}"#,
            r#"<p>앞 글</p><img src="assets/b.png">뒤 글"#,
            r#"{"path":"assets\/c.png"}"#,
            r#"{"legacy":"assets\\d.png"}"#,
        ];
        assert_eq!(unreferenced(&keys, &texts, false), ["assets/e.png"]);
        assert_eq!(unreferenced(&keys, &["assets\\e.png"], false).len(), 4);
    }

    #[test]
    fn several_keys_in_one_string_and_a_longer_key_containing_a_shorter_one() {
        let keys = ["assets/a.png", "assets/a.png.bak", "assets/z.png"];
        assert_eq!(unreferenced(&keys, &["assets/a.png.bak and assets/z.png"], false), Vec::<String>::new());
        assert_eq!(unreferenced(&keys, &["assets/a.png"], false), ["assets/a.png.bak", "assets/z.png"]);
    }

    #[test]
    fn non_ascii_keys_match_raw_and_unicode_escaped() {
        let keys = ["assets/그림.png", "assets/사진.png", "assets/표.png"];
        let texts = ["\"assets/그림.png\"", "\"assets/\\uc0ac\\uc9c4.png\"", "\"assets\\/\\uD45C.png\""];
        assert_eq!(unreferenced(&keys, &texts, false), Vec::<String>::new());
    }

    #[test]
    fn plugin_text_also_matches_keys_without_the_prefix() {
        let keys = ["assets/0123abcd.png", "assets/feed.webp"];
        assert_eq!(unreferenced(&keys, &[r#"{"cached":"0123abcd.png"}"#], false).len(), 2);
        assert_eq!(unreferenced(&keys, &[r#"{"cached":"0123abcd.png"}"#], true), ["assets/feed.webp"]);
    }

    #[test]
    fn streamed_text_finds_keys_split_across_pieces() {
        let keys = ["assets/0123abcd.png", "assets/그림.webp", "assets/unused.png"];
        let text = "앞 assets/0123abcd.png 중간 assets/그림.webp 끝";
        for piece in 1..text.len() {
            assert_eq!(streamed(&keys, text, piece, false), ["assets/unused.png"], "piece {piece}");
        }
        for piece in 1..12 {
            assert_eq!(streamed(&keys, "x 0123abcd.png y", piece, true), ["assets/unused.png", "assets/그림.webp"], "piece {piece}");
        }
    }
}
