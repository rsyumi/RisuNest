//! Section rows as a remote adapter sees them. Publication reads rows here and
//! reception merges them back, so one rule decides every value no matter which
//! remote carried it.
use super::{invalid, observe_remote_clock, sequence, DeviceStore, Section};
use crate::persistent_store::StoreResult;
use risunest_external_storage_format::section::{
    decode_local_plugin_entry_key, hypa_entry_key, local_plugin_entry_key, HypaValue,
    InlineOrObject, LocalPluginValue, LocalSettingValue, ObjectReference, PluginSpace,
    SectionEntry, SectionEntryVersion, SectionKind, SectionValue, MAX_INLINE_VALUE_BYTES,
};
use risunest_sync_wire::Sequence;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Device settings a restored installation wants back, as opposed to the
/// coordination state it must never inherit from another run.
pub(crate) const LOCAL_SETTING_KEYS: [&str; 9] = [
    "accountst",
    "dosync",
    "hub",
    "ignoreRisuAuth",
    "nightlyWarned",
    "risuNestDeviceSettings",
    "risuNestUpdateSettings",
    "risu_service_tos_v1",
    "risunest_tos_v1",
];

/// The sections a device chooses to take part in, in the order the settings
/// screen lists them.
pub(crate) const CHOOSABLE_SECTIONS: [Section; 2] = [Section::Hypa, Section::LocalPlugins];

/// Resolves the identifier a renderer sends. An unknown one is refused rather
/// than silently treated as one of the known sections.
pub(crate) fn section_from_id(id: &str) -> Option<Section> {
    CHOOSABLE_SECTIONS
        .into_iter()
        .find(|section| section.as_str() == id)
}

/// The commit a removal first reached a remote in, and when that happened. A
/// removal this device has not published yet carries none, and the marker only
/// means anything inside the remote lineage that issued the commit number.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TombstonePublication {
    pub generation: Sequence,
    pub at_ms: u64,
}

/// Every section value a device can publish, plus the removal of one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum SectionValueRow {
    Hypa {
        producer: String,
        model: String,
        endpoint: Option<String>,
        preprocess_version: i64,
        dimensions: i64,
        vector: Vec<u8>,
        metadata: Option<String>,
    },
    Plugin {
        space: String,
        value: String,
    },
    Setting {
        value: String,
    },
    PluginPermission {
        granted: bool,
    },
    Tombstone {
        first_published: Option<TombstonePublication>,
    },
}

impl SectionValueRow {
    pub(crate) fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone { .. })
    }
    /// Whether two rows hold the same thing. A removal's first publication
    /// marker is bookkeeping about the removal, not part of what the key holds,
    /// so two removals of the same key are the same content either way.
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Tombstone { .. }, Self::Tombstone { .. }) => true,
            (Self::Plugin { space: a_space, value: a }, Self::Plugin { space: b_space, value: b })
                if a_space == "json" && b_space == "json" => same_json(a, b),
            (Self::Setting { value: a }, Self::Setting { value: b }) => same_json(a, b),
            (
                Self::Hypa { producer: ap, model: am, endpoint: ae, preprocess_version: av,
                    dimensions: ad, vector: ab, metadata: ax },
                Self::Hypa { producer: bp, model: bm, endpoint: be, preprocess_version: bv,
                    dimensions: bd, vector: bb, metadata: bx },
            ) => ap == bp && am == bm && ae == be && av == bv && ad == bd && ab == bb
                && match (ax, bx) {
                    (Some(a), Some(b)) => same_json(a, b),
                    (None, None) => true,
                    _ => false,
                },
            _ => self == other,
        }
    }
    pub(crate) fn first_published(&self) -> Option<&TombstonePublication> {
        match self {
            Self::Tombstone { first_published } => first_published.as_ref(),
            _ => None,
        }
    }
}

fn same_json(a: &str, b: &str) -> bool {
    match (serde_json::from_str::<serde_json::Value>(a), serde_json::from_str::<serde_json::Value>(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The triple a section row is named by, in the order the change index holds.
pub(crate) type SectionKey = (String, String, String);
pub(crate) type PublishedRows = Vec<(SectionKey, SectionEntryVersion)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimedRowVersion {
    pub write_clock: Sequence,
    pub writer_id: String,
    pub first_published: TombstonePublication,
}

pub(crate) type ReclaimedRows = BTreeMap<SectionKey, ReclaimedRowVersion>;

pub(crate) enum SectionPublicationDisposition {
    Published { first_published: Option<TombstonePublication> },
    Reclaimed { first_published: TombstonePublication },
}

pub(crate) struct SectionPublicationRow {
    pub key: SectionKey,
    pub version: SectionEntryVersion,
    pub disposition: SectionPublicationDisposition,
}

/// The key triple matches the change index, so a row and its change entry name
/// the same thing without a second encoding.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SectionRow {
    pub key1: String,
    pub key2: String,
    pub key3: String,
    pub value: SectionValueRow,
    pub write_clock: Sequence,
    pub writer_id: String,
}

impl SectionRow {
    pub(crate) fn key(&self) -> (String, String, String) {
        (self.key1.clone(), self.key2.clone(), self.key3.clone())
    }
    pub(crate) fn version(&self) -> SectionEntryVersion {
        SectionEntryVersion {
            write_clock: self.write_clock.clone(),
            writer_id: self.writer_id.clone(),
        }
    }
    fn same_version(&self, other: &Self) -> bool {
        self.write_clock == other.write_clock && self.writer_id == other.writer_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionState {
    pub participating: bool,
    pub max_write_clock: Sequence,
    pub gc_floor: Sequence,
    pub participation_generation: Sequence,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SectionApplyOutcome {
    pub applied: usize,
    pub kept: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionCursor {
    pub applied_generation: Sequence,
    pub applied_gc_floor: Sequence,
    pub observed_max_write_clock: Sequence,
}

pub(crate) enum SectionSnapshotEvent {
    Section {
        section: Section,
        state: SectionState,
        cursor: Option<SectionCursor>,
    },
    Row {
        section: Section,
        row: SectionRow,
    },
}

impl SectionCursor {
    /// Whether this lineage has carried the section to this device. A cursor
    /// reset to no commit at all says the markers this device holds came from
    /// this lineage while nothing it holds has reached the remote state.
    pub(crate) fn joined(&self) -> bool {
        self.applied_generation > Sequence::from(0u64)
    }
}

fn setting_is_local(key: &str) -> bool {
    LOCAL_SETTING_KEYS.contains(&key)
}

/// Keys whose current value no remote has been told about. A restore uses them
/// to tell its own interrupted attempt apart from a value some remote already
/// carries, and publication uses them to decide whether it owes one at all. A
/// row reissued above the version it was published at counts as unpublished.
fn unpublished_keys(
    db: &Connection,
    section: Section,
) -> StoreResult<BTreeSet<(String, String, String)>> {
    let mut keys = BTreeSet::new();
    match section {
        Section::Hypa => {
            let mut statement = db.prepare(
                "SELECT cache_key FROM hypa_embeddings
                    WHERE published_clock IS NULL OR published_clock<>write_clock",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, String::new(), String::new()));
            }
        }
        Section::LocalPlugins => {
            let mut statement = db.prepare(
                "SELECT owner,space,key FROM plugin_device_storage
                    WHERE published_clock IS NULL OR published_clock<>write_clock",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, row.get(1)?, row.get(2)?));
            }
        }
    }
    Ok(keys)
}

/// The stored marker pair. The schema keeps the two columns set or unset
/// together, so a half-written pair is a broken device file rather than a
/// removal this device may publish.
fn first_published(
    generation: Option<String>,
    at_ms: Option<i64>,
) -> StoreResult<Option<TombstonePublication>> {
    match (generation, at_ms) {
        (Some(generation), Some(at_ms)) => Ok(Some(TombstonePublication {
            generation: sequence(&generation)?,
            at_ms: u64::try_from(at_ms)
                .map_err(|_| invalid("device removal marker time is out of range"))?,
        })),
        (None, None) => Ok(None),
        _ => Err(invalid("device removal marker is incomplete")),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SectionMergeDecision {
    ApplyIncoming,
    PublishLocal,
    Settled,
    MergeRemovalMarker {
        marker: Option<TombstonePublication>,
        write_local: bool,
        publish: bool,
    },
}

/// The wire form can decide a version before a large object is downloaded.
/// Both forms borrow their values and use the same ordering and marker policy.
pub(crate) trait SectionMergeRow {
    fn matching_key(&self, other: &Self) -> bool;
    fn merge_version(&self) -> StoreResult<(&Sequence, &str)>;
    fn matching_content(&self, other: &Self) -> StoreResult<bool>;
    fn removal_marker(&self) -> Option<(&Sequence, u64)>;
}

impl SectionMergeRow for SectionRow {
    fn matching_key(&self, other: &Self) -> bool {
        (&self.key1, &self.key2, &self.key3) == (&other.key1, &other.key2, &other.key3)
            && (self.value.is_tombstone() || other.value.is_tombstone()
                || std::mem::discriminant(&self.value) == std::mem::discriminant(&other.value))
    }
    fn merge_version(&self) -> StoreResult<(&Sequence, &str)> {
        if self.writer_id.is_empty() || self.writer_id.len() > 1024 {
            return Err(invalid("Section row carries no valid writer"));
        }
        Ok((&self.write_clock, &self.writer_id))
    }
    fn matching_content(&self, other: &Self) -> StoreResult<bool> {
        Ok(self.value.same_content(&other.value))
    }
    fn removal_marker(&self) -> Option<(&Sequence, u64)> {
        self.value.first_published().map(|marker| (&marker.generation, marker.at_ms))
    }
}

impl SectionMergeRow for SectionEntry {
    fn matching_key(&self, other: &Self) -> bool {
        self.kind == other.kind && self.key == other.key
    }
    fn merge_version(&self) -> StoreResult<(&Sequence, &str)> {
        self.validate().map_err(section_format_error)?;
        let version = self.version.as_ref().ok_or_else(|| invalid("Section entry carries no version"))?;
        Ok((&version.write_clock, &version.writer_id))
    }
    fn matching_content(&self, other: &Self) -> StoreResult<bool> {
        match (&self.value, &other.value) {
            (SectionValue::Tombstone { .. }, SectionValue::Tombstone { .. }) => Ok(true),
            (SectionValue::Hypa(a), SectionValue::Hypa(b)) => Ok(
                a.producer == b.producer && a.model == b.model && a.endpoint == b.endpoint
                    && a.preprocess_version == b.preprocess_version && a.dimensions == b.dimensions
                    && a.metadata == b.metadata && vector_identity(&a.vector)? == vector_identity(&b.vector)?
            ),
            _ => Ok(self.value == other.value),
        }
    }
    fn removal_marker(&self) -> Option<(&Sequence, u64)> {
        match &self.value {
            SectionValue::Tombstone { first_published_generation, first_published_at_ms } => {
                Some((first_published_generation, *first_published_at_ms))
            }
            _ => None,
        }
    }
}

pub(crate) fn resolve_section_row<T: SectionMergeRow>(
    local: Option<&T>,
    incoming: &T,
) -> StoreResult<SectionMergeDecision> {
    let arrived_version = incoming.merge_version()?;
    let Some(local) = local else {
        return Ok(SectionMergeDecision::ApplyIncoming);
    };
    if !local.matching_key(incoming) {
        return Err(invalid("Section merge keys or kinds differ"));
    }
    match arrived_version.cmp(&local.merge_version()?) {
        std::cmp::Ordering::Greater => return Ok(SectionMergeDecision::ApplyIncoming),
        std::cmp::Ordering::Less => return Ok(SectionMergeDecision::PublishLocal),
        std::cmp::Ordering::Equal => {}
    }
    if !local.matching_content(incoming)? {
        return Err(invalid("Section version carries two different values"));
    }
    let (held, arrived) = (local.removal_marker(), incoming.removal_marker());
    if held == arrived {
        return Ok(SectionMergeDecision::Settled);
    }
    let earliest = match (held, arrived) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    Ok(SectionMergeDecision::MergeRemovalMarker {
        write_local: held != earliest,
        publish: arrived != earliest,
        marker: earliest.map(|(generation, at_ms)| TombstonePublication { generation: generation.clone(), at_ms }),
    })
}

fn section_format_error(_: risunest_external_storage_format::FormatError) -> crate::persistent_store::StoreError {
    invalid("Section entry is invalid")
}

fn vector_identity(vector: &InlineOrObject) -> StoreResult<([u8; 32], u64)> {
    match vector {
        InlineOrObject::Object(reference) => Ok((reference.content_sha256, reference.byte_length)),
        InlineOrObject::Inline(_) => {
            let bytes = vector.decode_inline().map_err(section_format_error)?;
            Ok((risunest_external_storage_format::content_identity::hash(&bytes), bytes.len() as u64))
        }
    }
}

fn row_from_sql(section: Section, row: &rusqlite::Row<'_>) -> StoreResult<SectionRow> {
    let tombstone: bool = row.get("tombstone")?;
    let value = if tombstone {
        SectionValueRow::Tombstone {
            first_published: first_published(row.get("first_published_generation")?, row.get("first_published_at_ms")?)?,
        }
    } else {
        match section {
            Section::Hypa => SectionValueRow::Hypa {
                producer: row.get("producer")?, model: row.get("model")?, endpoint: row.get("endpoint")?,
                preprocess_version: row.get("preprocess_version")?, dimensions: row.get("dimensions")?,
                vector: row.get("vector")?, metadata: row.get("metadata")?,
            },
            Section::LocalPlugins => SectionValueRow::Plugin { space: row.get("space")?, value: row.get("value")? },
        }
    };
    let (key1, key2, key3) = match section {
        Section::Hypa => (row.get("cache_key")?, String::new(), String::new()),
        Section::LocalPlugins => (row.get("owner")?, row.get("space")?, row.get("key")?),
    };
    Ok(SectionRow {
        key1, key2, key3, value,
        write_clock: sequence(&row.get::<_, String>("write_clock")?)?,
        writer_id: row.get("writer_id")?,
    })
}

fn read_rows(db: &Connection, section: Section) -> StoreResult<Vec<SectionRow>> {
    let mut statement = db.prepare(match section {
        Section::Hypa => "SELECT * FROM hypa_embeddings ORDER BY cache_key",
        Section::LocalPlugins => "SELECT * FROM plugin_device_storage ORDER BY owner,space,key",
    })?;
    let mut query = statement.query([])?;
    let mut rows = Vec::new();
    while let Some(row) = query.next()? {
        rows.push(row_from_sql(section, row)?);
    }
    Ok(rows)
}

fn read_row(db: &Connection, section: Section, key: &SectionKey) -> StoreResult<Option<(SectionRow, bool)>> {
    let mut statement = db.prepare(match section {
        Section::Hypa => "SELECT * FROM hypa_embeddings WHERE cache_key=?1 AND ?2='' AND ?3=''",
        Section::LocalPlugins => "SELECT * FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3",
    })?;
    let mut query = statement.query(params![key.0, key.1, key.2])?;
    let Some(row) = query.next()? else { return Ok(None); };
    let current = row_from_sql(section, row)?;
    let published: Option<String> = row.get("published_clock")?;
    let published = published.as_deref() == Some(current.write_clock.as_str());
    Ok(Some((current, published)))
}

pub(crate) fn kind_of_section(section: Section) -> SectionKind {
    match section {
        Section::Hypa => SectionKind::Hypa,
        Section::LocalPlugins => SectionKind::LocalPlugins,
    }
}

fn section_of_kind(kind: SectionKind) -> Option<Section> {
    match kind {
        SectionKind::Hypa => Some(Section::Hypa),
        SectionKind::LocalPlugins => Some(Section::LocalPlugins),
        SectionKind::LocalSettings => None,
    }
}

pub(crate) fn section_entry_key(kind: SectionKind, row: &SectionRow) -> StoreResult<String> {
    match kind {
        SectionKind::Hypa => {
            if !row.key2.is_empty() || !row.key3.is_empty() {
                return Err(invalid("Embedding key is not canonical"));
            }
            hypa_entry_key(&row.key1).map_err(section_format_error)
        }
        SectionKind::LocalPlugins => local_plugin_entry_key(&row.key1, &row.key2, &row.key3).map_err(section_format_error),
        SectionKind::LocalSettings => Ok(serde_json::to_string(&[&row.key1, &row.key2, &row.key3])?),
    }
}

fn decode_entry_key(kind: SectionKind, key: &str) -> StoreResult<SectionKey> {
    match kind {
        SectionKind::Hypa => Ok((hypa_entry_key(key).map_err(section_format_error)?, String::new(), String::new())),
        SectionKind::LocalPlugins => decode_local_plugin_entry_key(key).map_err(section_format_error),
        SectionKind::LocalSettings => {
            let parts: [String; 3] = serde_json::from_str(key)?;
            if serde_json::to_string(&parts)? != key {
                return Err(invalid("Device setting key is not canonical"));
            }
            let [key1, key2, key3] = parts;
            Ok((key1, key2, key3))
        }
    }
}

impl SectionRow {
    pub(crate) fn to_entry(&self, kind: SectionKind, versioned: bool) -> StoreResult<SectionEntry> {
        let value = match (&self.value, kind) {
            (SectionValueRow::Tombstone { first_published }, _) => {
                let marker = first_published.as_ref().ok_or_else(|| invalid("Removal carries no first publication marker"))?;
                SectionValue::tombstone(marker.generation.clone(), marker.at_ms)
            }
            (SectionValueRow::Hypa { producer, model, endpoint, preprocess_version, dimensions, vector, metadata }, SectionKind::Hypa) => {
                let vector = if vector.len() > MAX_INLINE_VALUE_BYTES {
                    InlineOrObject::Object(ObjectReference {
                        content_sha256: risunest_external_storage_format::content_identity::hash(vector),
                        byte_length: vector.len() as u64,
                    })
                } else {
                    InlineOrObject::inline(vector).map_err(section_format_error)?
                };
                SectionValue::Hypa(HypaValue {
                    producer: producer.clone(), model: model.clone(), endpoint: endpoint.clone(),
                    preprocess_version: u32::try_from(*preprocess_version).map_err(|_| invalid("Embedding preprocess version is out of range"))?,
                    dimensions: u32::try_from(*dimensions).map_err(|_| invalid("Embedding dimensions are out of range"))?,
                    vector, metadata: metadata.as_deref().map(serde_json::from_str).transpose()?,
                })
            }
            (SectionValueRow::Plugin { space, value }, SectionKind::LocalPlugins) => {
                if space != &self.key2 {
                    return Err(invalid("Plugin value names another space"));
                }
                let (space, value) = match space.as_str() {
                    "string" => (PluginSpace::String, serde_json::Value::String(value.clone())),
                    "json" => (PluginSpace::Json, serde_json::from_str(value)?),
                    _ => return Err(invalid("Plugin device space is invalid")),
                };
                SectionValue::LocalPlugin(LocalPluginValue { space, value })
            }
            (SectionValueRow::Setting { value }, SectionKind::LocalSettings) => {
                if self.key1 != "setting" || !setting_is_local(&self.key2) || !self.key3.is_empty() {
                    return Err(invalid("Device setting is outside the backup scope"));
                }
                SectionValue::LocalSetting(LocalSettingValue { value: serde_json::from_str(value)? })
            }
            (SectionValueRow::PluginPermission { granted }, SectionKind::LocalSettings) => {
                if self.key1 != "pluginPermission" || self.key2.is_empty() || self.key3.is_empty() {
                    return Err(invalid("Plugin permission key is invalid"));
                }
                SectionValue::LocalSetting(LocalSettingValue { value: serde_json::Value::Bool(*granted) })
            }
            _ => return Err(invalid("Section row belongs to another section")),
        };
        SectionEntry::new(kind, section_entry_key(kind, self)?, value, versioned.then(|| self.version()))
            .map_err(section_format_error)
    }

    pub(crate) fn from_entry(
        entry: SectionEntry,
        versioned: bool,
        mut object: impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>,
    ) -> StoreResult<Self> {
        entry.validate().map_err(section_format_error)?;
        let (key1, key2, key3) = decode_entry_key(entry.kind, &entry.key)?;
        let (write_clock, writer_id) = match entry.version {
            Some(version) => (version.write_clock, version.writer_id),
            None if !versioned => (Sequence::from(0u64), String::new()),
            None => return Err(invalid("Section entry carries no version")),
        };
        let value = match entry.value {
            SectionValue::Tombstone { first_published_generation, first_published_at_ms } => {
                i64::try_from(first_published_at_ms).map_err(|_| invalid("Device removal marker time is out of range"))?;
                SectionValueRow::Tombstone { first_published: Some(TombstonePublication {
                    generation: first_published_generation, at_ms: first_published_at_ms,
                }) }
            }
            SectionValue::Hypa(value) => {
                let vector = match &value.vector {
                    InlineOrObject::Inline(_) => value.vector.decode_inline().map_err(section_format_error)?,
                    InlineOrObject::Object(reference) => {
                        let bytes = object(reference)?;
                        if bytes.len() as u64 != reference.byte_length
                            || risunest_external_storage_format::content_identity::hash(&bytes) != reference.content_sha256 {
                            return Err(invalid("Section vector object does not match its reference"));
                        }
                        bytes
                    }
                };
                if vector.len() as u64 != u64::from(value.dimensions) * 4 {
                    return Err(invalid("Section vector length does not match its dimensions"));
                }
                SectionValueRow::Hypa {
                    producer: value.producer, model: value.model, endpoint: value.endpoint,
                    preprocess_version: i64::from(value.preprocess_version), dimensions: i64::from(value.dimensions),
                    vector, metadata: value.metadata.map(|value| serde_json::to_string(&value)).transpose()?,
                }
            }
            SectionValue::LocalPlugin(value) => {
                let space = match value.space { PluginSpace::String => "string", PluginSpace::Json => "json" };
                if space != key2 { return Err(invalid("Plugin value names another space")); }
                let text = match value.space {
                    PluginSpace::String => match value.value {
                        serde_json::Value::String(text) => text,
                        _ => return Err(invalid("Plugin string value is not text")),
                    },
                    PluginSpace::Json => serde_json::to_string(&value.value)?,
                };
                SectionValueRow::Plugin { space: space.to_owned(), value: text }
            }
            SectionValue::LocalSetting(value) => match key1.as_str() {
                "setting" if setting_is_local(&key2) && key3.is_empty() => {
                    SectionValueRow::Setting { value: serde_json::to_string(&value.value)? }
                }
                "pluginPermission" if !key2.is_empty() && !key3.is_empty() => SectionValueRow::PluginPermission {
                    granted: value.value.as_bool().ok_or_else(|| invalid("Plugin permission is not a decision"))?,
                },
                _ => return Err(invalid("Device setting is outside the backup scope")),
            },
        };
        Ok(Self { key1, key2, key3, value, write_clock, writer_id })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalSectionEntry {
    pub version: SectionEntryVersion,
    pub entry: SectionEntry,
    pub object: Option<Vec<u8>>,
    pub published: bool,
}

pub(crate) enum SectionWriteInput<'a> {
    Apply { section: Section, entry: &'a SectionEntry, object: Option<&'a [u8]> },
    Mark { section: Section, key: &'a str },
    MarkVersion { section: Section, key: &'a str, version: &'a SectionEntryVersion },
}

/// An unfinished spool cannot be applied. It becomes a read-only input only
/// after every received row and the transport fingerprint have been checked.
pub(crate) struct SectionSpoolBuilder {
    connection: Connection,
    directory: tempfile::TempDir,
    kind: SectionKind,
    versioned: bool,
    count: i64,
    max_write_clock: Sequence,
}

pub(crate) struct PreparedSectionRows {
    connection: Connection,
    // Declared after the connection so Windows closes the file before cleanup.
    _directory: tempfile::TempDir,
    kind: SectionKind,
    versioned: bool,
    count: i64,
    max_write_clock: Sequence,
}

impl SectionSpoolBuilder {
    pub(crate) fn new(section: Section) -> StoreResult<Self> {
        Self::open(kind_of_section(section), true)
    }

    pub(crate) fn new_backup(kind: SectionKind) -> StoreResult<Self> {
        Self::open(kind, false)
    }

    fn open(kind: SectionKind, versioned: bool) -> StoreResult<Self> {
        let directory = tempfile::tempdir()?;
        let connection = Connection::open(directory.path().join("rows.sqlite"))?;
        connection.execute_batch(
            "PRAGMA cache_size=-4096;
             PRAGMA temp_store=FILE;
             PRAGMA mmap_size=0;
             CREATE TABLE vector_values(hash TEXT PRIMARY KEY,bytes BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE row_values(id INTEGER PRIMARY KEY,metadata TEXT NOT NULL,vector_hash TEXT);
             CREATE TABLE row_index(
                key1 TEXT NOT NULL,key2 TEXT NOT NULL,key3 TEXT NOT NULL,
                body_id INTEGER NOT NULL,write_clock TEXT NOT NULL,writer_id TEXT NOT NULL,
                tombstone INTEGER NOT NULL,entry_key TEXT NOT NULL UNIQUE,entry_hash BLOB NOT NULL,
                PRIMARY KEY(key1,key2,key3)
             ) WITHOUT ROWID;
             BEGIN IMMEDIATE;",
        )?;
        Ok(Self { connection, directory, kind, versioned, count: 0, max_write_clock: Sequence::from(0u64) })
    }

    pub(crate) fn push(&mut self, row: SectionRow, entry_key: &str, entry_hash: &[u8; 32]) -> StoreResult<()> {
        let section = section_of_kind(self.kind).ok_or_else(|| invalid("Backup section is not synchronized"))?;
        if !self.versioned { return Err(invalid("Backup rows must use push_backup_entry")); }
        validate_sync_row(section, &row)?;
        self.push_row(row, entry_key, entry_hash)
    }

    pub(crate) fn push_backup_entry(
        &mut self,
        entry: SectionEntry,
        object: impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>,
    ) -> StoreResult<()> {
        if self.versioned || entry.kind != self.kind || entry.version.is_some() {
            return Err(invalid("Backup section entry has the wrong identity"));
        }
        let encoded = entry.encode().map_err(section_format_error)?;
        let entry_key = entry.key.clone();
        let entry_hash = risunest_external_storage_format::content_identity::hash(&encoded);
        let row = SectionRow::from_entry(entry, false, object)?;
        if row.value.is_tombstone() { return Err(invalid("Backup section entry is a removal")); }
        self.push_row(row, &entry_key, &entry_hash)
    }

    fn push_backup_row(&mut self, row: SectionRow) -> StoreResult<()> {
        if self.versioned || row.value.is_tombstone() { return Err(invalid("Backup section row is invalid")); }
        let entry = row.to_entry(self.kind, false)?;
        let entry_key = entry.key.clone();
        let entry_hash = risunest_external_storage_format::content_identity::hash(
            &entry.encode().map_err(section_format_error)?,
        );
        self.push_row(row, &entry_key, &entry_hash)
    }

    fn push_row(&mut self, mut row: SectionRow, entry_key: &str, entry_hash: &[u8; 32]) -> StoreResult<()> {
        if section_entry_key(self.kind, &row)? != entry_key {
            return Err(invalid("Section spool key differs from its entry"));
        }
        let duplicate: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM row_index
                WHERE (key1=?1 AND key2=?2 AND key3=?3) OR entry_key=?4)",
            params![row.key1, row.key2, row.key3, entry_key], |row| row.get(0),
        )?;
        if duplicate { return Err(invalid("Section spool repeats an entry key")); }
        let vector_hash = match &mut row.value {
            SectionValueRow::Hypa { vector, .. } => {
                let bytes = std::mem::take(vector);
                let digest = risunest_sync_wire::hash(&bytes);
                self.connection.execute("INSERT OR IGNORE INTO vector_values(hash,bytes) VALUES (?1,?2)", params![digest, bytes])?;
                Some(digest)
            }
            _ => None,
        };
        let id = self.count.checked_add(1).ok_or_else(|| invalid("Section spool row count is exhausted"))?;
        self.connection.execute(
            "INSERT INTO row_values(id,metadata,vector_hash) VALUES (?1,?2,?3)",
            params![id, serde_json::to_string(&row)?, vector_hash],
        )?;
        self.connection.execute(
            "INSERT INTO row_index(key1,key2,key3,body_id,write_clock,writer_id,tombstone,entry_key,entry_hash)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![row.key1, row.key2, row.key3, id, row.write_clock.as_str(), row.writer_id,
                row.value.is_tombstone(), entry_key, entry_hash.as_slice()],
        )?;
        self.count = id;
        self.max_write_clock = self.max_write_clock.clone().max(row.write_clock);
        Ok(())
    }

    pub(crate) fn finish(self, expected_fingerprint: &[u8; 32]) -> StoreResult<PreparedSectionRows> {
        let mut fingerprint = risunest_external_storage_format::format::FingerprintBuilder::new(
            &self.kind.fingerprint_domain(),
        );
        {
            let mut statement = self.connection.prepare("SELECT entry_key,entry_hash FROM row_index ORDER BY entry_key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let digest: Vec<u8> = row.get(1)?;
                let digest: [u8; 32] = digest.try_into().map_err(|_| invalid("Section entry hash is invalid"))?;
                fingerprint.push(&key, &digest).map_err(section_format_error)?;
            }
        }
        if fingerprint.finish() != *expected_fingerprint {
            return Err(invalid("Section content differs from its reference"));
        }
        self.connection.execute_batch("COMMIT;")?;
        drop(self.connection);
        let connection = Connection::open_with_flags(
            self.directory.path().join("rows.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.execute_batch("PRAGMA cache_size=-4096; PRAGMA mmap_size=0; PRAGMA query_only=ON; BEGIN;")?;
        let count: i64 = connection.query_row("SELECT count(*) FROM row_index", [], |row| row.get(0))?;
        if count != self.count { return Err(invalid("Section spool is incomplete")); }
        Ok(PreparedSectionRows { connection, _directory: self.directory,
            kind: self.kind, versioned: self.versioned, count, max_write_clock: self.max_write_clock })
    }

    fn finish_captured(self) -> StoreResult<PreparedSectionRows> {
        let mut fingerprint = risunest_external_storage_format::format::FingerprintBuilder::new(
            &self.kind.fingerprint_domain(),
        );
        {
            let mut statement = self.connection.prepare(
                "SELECT entry_key,entry_hash FROM row_index ORDER BY entry_key",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let digest: Vec<u8> = row.get(1)?;
                let digest: [u8; 32] = digest.try_into()
                    .map_err(|_| invalid("Section entry hash is invalid"))?;
                fingerprint.push(&key, &digest).map_err(section_format_error)?;
            }
        }
        self.finish(&fingerprint.finish())
    }
}

impl PreparedSectionRows {
    pub(crate) fn kind(&self) -> SectionKind { self.kind }
    pub(crate) fn len(&self) -> u64 { self.count as u64 }
    pub(crate) fn is_empty(&self) -> bool { self.count == 0 }
    pub(crate) fn max_write_clock(&self) -> &Sequence { &self.max_write_clock }

    fn synchronized_section(&self) -> StoreResult<Section> {
        if !self.versioned { return Err(invalid("Backup section cannot be merged as synchronized state")); }
        section_of_kind(self.kind).ok_or_else(|| invalid("Section is not synchronized"))
    }

    fn row(&self, key: &SectionKey) -> StoreResult<Option<SectionRow>> {
        let stored: Option<(String, Option<Vec<u8>>)> = self.connection.query_row(
            "SELECT v.metadata,b.bytes FROM row_index i JOIN row_values v ON v.id=i.body_id
                LEFT JOIN vector_values b ON b.hash=v.vector_hash
                WHERE i.key1=?1 AND i.key2=?2 AND i.key3=?3",
            params![key.0, key.1, key.2], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        stored.map(|(metadata, vector)| {
            let mut row: SectionRow = serde_json::from_str(&metadata)?;
            if let SectionValueRow::Hypa { vector: target, .. } = &mut row.value {
                *target = vector.ok_or_else(|| invalid("Prepared vector is missing"))?;
            }
            Ok(row)
        }).transpose()
    }

    fn row_by_entry_key(&self, entry_key: &str) -> StoreResult<Option<SectionRow>> {
        let stored: Option<(String, Option<Vec<u8>>)> = self.connection.query_row(
            "SELECT v.metadata,b.bytes FROM row_index i JOIN row_values v ON v.id=i.body_id
                LEFT JOIN vector_values b ON b.hash=v.vector_hash WHERE i.entry_key=?1",
            [entry_key], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        stored.map(|(metadata, vector)| {
            let mut row: SectionRow = serde_json::from_str(&metadata)?;
            if let SectionValueRow::Hypa { vector: target, .. } = &mut row.value {
                *target = vector.ok_or_else(|| invalid("Prepared vector is missing"))?;
            }
            Ok(row)
        }).transpose()
    }

    fn contains(&self, key: &SectionKey, tombstone_only: bool) -> StoreResult<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM row_index WHERE key1=?1 AND key2=?2 AND key3=?3
                AND (?4=0 OR tombstone=1))",
            params![key.0, key.1, key.2, tombstone_only], |row| row.get(0),
        )?)
    }

    fn visit(&self, mut visitor: impl FnMut(SectionRow) -> StoreResult<()>) -> StoreResult<()> {
        let mut after = (String::new(), String::new(), String::new());
        let mut visited = 0i64;
        loop {
            let mut statement = self.connection.prepare(
                "SELECT key1,key2,key3 FROM row_index WHERE (key1,key2,key3)>(?1,?2,?3)
                    ORDER BY key1,key2,key3 LIMIT 256",
            )?;
            let page = read_key_page(&mut statement.query(params![after.0, after.1, after.2])?)?;
            if page.is_empty() { break; }
            for key in page {
                visitor(self.row(&key)?.ok_or_else(|| invalid("Prepared row is missing"))?)?;
                visited += 1;
                after = key;
            }
        }
        if visited != self.count { return Err(invalid("Section spool scan is incomplete")); }
        Ok(())
    }

    pub(crate) fn visit_entries(
        &self,
        visitor: impl FnMut(&SectionEntry, Option<&[u8]>) -> StoreResult<()>,
    ) -> StoreResult<()> {
        self.visit_entries_mapped(visitor, |error| error)
    }

    pub(crate) fn visit_entries_mapped<E>(
        &self,
        mut visitor: impl FnMut(&SectionEntry, Option<&[u8]>) -> std::result::Result<(), E>,
        mut map_error: impl FnMut(crate::persistent_store::StoreError) -> E,
    ) -> std::result::Result<(), E> {
        let mut after = String::new();
        let mut visited = 0i64;
        loop {
            let page = {
                let mut statement = self.connection.prepare(
                    "SELECT entry_key FROM row_index WHERE entry_key>?1 ORDER BY entry_key LIMIT 256",
                ).map_err(|error| map_error(error.into()))?;
                let mut rows = statement.query([&after]).map_err(|error| map_error(error.into()))?;
                read_text_page(&mut rows).map_err(&mut map_error)?
            };
            if page.is_empty() { break; }
            for key in page {
                let mut row = self.row_by_entry_key(&key).map_err(&mut map_error)?
                    .ok_or_else(|| map_error(invalid("Prepared row is missing")))?;
                let entry = row.to_entry(self.kind, self.versioned).map_err(&mut map_error)?;
                let object = match &mut row.value {
                    SectionValueRow::Hypa { vector, .. } if vector.len() > MAX_INLINE_VALUE_BYTES => {
                        Some(std::mem::take(vector))
                    }
                    _ => None,
                };
                #[cfg(test)]
                if let Some(object) = &object {
                    SECTION_TEST_MAX_VALUE_BYTES.fetch_max(object.len(), Ordering::Relaxed);
                }
                visitor(&entry, object.as_deref())?;
                visited += 1;
                after = key;
            }
        }
        if visited != self.count { return Err(map_error(invalid("Section spool scan is incomplete"))); }
        Ok(())
    }
}

fn read_cursor(db: &Connection, connection_id: &str, library_lineage: &str, section: Section) -> StoreResult<Option<SectionCursor>> {
    let stored: Option<(String, String, String)> = db.query_row(
        "SELECT applied_generation,applied_gc_floor,observed_max_write_clock FROM device_remote_cursors
            WHERE connection_id=?1 AND library_lineage=?2 AND section=?3",
        params![connection_id, library_lineage, section.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    stored.map(|(generation, floor, observed)| Ok(SectionCursor {
        applied_generation: sequence(&generation)?, applied_gc_floor: sequence(&floor)?,
        observed_max_write_clock: sequence(&observed)?,
    })).transpose()
}

fn record_cursor(tx: &Transaction<'_>, connection_id: &str, library_lineage: &str, section: Section, cursor: &SectionCursor) -> StoreResult<()> {
    if connection_id.is_empty() || library_lineage.is_empty() {
        return Err(invalid("Section cursor identity is missing"));
    }
    let current = read_cursor(tx, connection_id, library_lineage, section)?;
    let merged = match current {
        Some(current) => SectionCursor {
            applied_generation: cursor.applied_generation.clone().max(current.applied_generation),
            applied_gc_floor: cursor.applied_gc_floor.clone().max(current.applied_gc_floor),
            observed_max_write_clock: cursor.observed_max_write_clock.clone().max(current.observed_max_write_clock),
        },
        None => cursor.clone(),
    };
    tx.execute(
        "INSERT INTO device_remote_cursors(connection_id,library_lineage,section,
            applied_generation,applied_gc_floor,observed_max_write_clock) VALUES (?1,?2,?3,?4,?5,?6)
            ON CONFLICT(connection_id,library_lineage,section) DO UPDATE SET
                applied_generation=excluded.applied_generation,applied_gc_floor=excluded.applied_gc_floor,
                observed_max_write_clock=excluded.observed_max_write_clock",
        params![connection_id, library_lineage, section.as_str(), merged.applied_generation.as_str(),
            merged.applied_gc_floor.as_str(), merged.observed_max_write_clock.as_str()],
    )?;
    Ok(())
}

fn mark_row_version(tx: &Connection, section: Section, key: &SectionKey, version: &SectionEntryVersion, published: bool) -> StoreResult<()> {
    let clock = published.then_some(version.write_clock.as_str());
    match section {
        Section::Hypa => tx.execute(
            "UPDATE hypa_embeddings SET published_clock=?4 WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3",
            params![key.0, version.write_clock.as_str(), version.writer_id, clock],
        )?,
        Section::LocalPlugins => tx.execute(
            "UPDATE plugin_device_storage SET published_clock=?6
                WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5",
            params![key.0, key.1, key.2, version.write_clock.as_str(), version.writer_id, clock],
        )?,
    };
    Ok(())
}

fn validate_sync_row(section: Section, row: &SectionRow) -> StoreResult<()> {
    row.merge_version()?;
    section_entry_key(kind_of_section(section), row)?;
    match (section, &row.value) {
        (_, SectionValueRow::Tombstone { first_published }) => {
            if let Some(marker) = first_published {
                i64::try_from(marker.at_ms).map_err(|_| invalid("Device removal marker time is out of range"))?;
            }
        }
        (Section::Hypa, SectionValueRow::Hypa { producer, model, endpoint, preprocess_version,
            dimensions, vector, metadata }) => {
            if producer.is_empty() || model.is_empty() || endpoint.as_ref().is_some_and(String::is_empty)
                || u32::try_from(*preprocess_version).is_err() || *dimensions <= 0
                || u32::try_from(*dimensions).is_err() || vector.len() as u64 != *dimensions as u64 * 4 {
                return Err(invalid("Embedding row is invalid"));
            }
            if let Some(metadata) = metadata { serde_json::from_str::<serde_json::Value>(metadata)?; }
        }
        (Section::LocalPlugins, SectionValueRow::Plugin { space, value }) => {
            if space != &row.key2 || !matches!(space.as_str(), "string" | "json") {
                return Err(invalid("Plugin value names another space"));
            }
            if space == "json" { serde_json::from_str::<serde_json::Value>(value)?; }
        }
        _ => return Err(invalid("Section row belongs to another section")),
    }
    Ok(())
}

fn merge_row(tx: &Transaction<'_>, section: Section, incoming: &SectionRow) -> StoreResult<bool> {
    validate_sync_row(section, incoming)?;
    merge_validated_row(tx, section, incoming)
}

fn merge_validated_row(tx: &Transaction<'_>, section: Section, incoming: &SectionRow) -> StoreResult<bool> {
    let key = incoming.key();
    let local = read_row(tx, section, &key)?;
    match resolve_section_row(local.as_ref().map(|(row, _)| row), incoming)? {
        SectionMergeDecision::ApplyIncoming => {
            write_row(tx, section, incoming, true)?;
            Ok(true)
        }
        SectionMergeDecision::PublishLocal => {
            let (local, _) = local.expect("local winner");
            mark_row_version(tx, section, &key, &local.version(), false)?;
            Ok(false)
        }
        SectionMergeDecision::Settled => {
            mark_row_version(tx, section, &key, &incoming.version(), true)?;
            Ok(false)
        }
        SectionMergeDecision::MergeRemovalMarker { marker, write_local, publish } => {
            if write_local {
                let row = SectionRow { value: SectionValueRow::Tombstone { first_published: marker },
                    key1: incoming.key1.clone(), key2: incoming.key2.clone(), key3: incoming.key3.clone(),
                    write_clock: incoming.write_clock.clone(), writer_id: incoming.writer_id.clone() };
                write_row(tx, section, &row, !publish)?;
            } else {
                mark_row_version(tx, section, &key, &incoming.version(), !publish)?;
            }
            Ok(false)
        }
    }
}

const SECTION_PAGE_ROWS: usize = 256;
const SECTION_PAGE_BYTES: usize = 4 * 1024 * 1024;

#[cfg(test)]
static SECTION_TEST_MAX_PAGE_ROWS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SECTION_TEST_MAX_PAGE_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SECTION_TEST_MAX_VALUE_BYTES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
fn observe_section_page(rows: usize, bytes: usize) {
    SECTION_TEST_MAX_PAGE_ROWS.fetch_max(rows, Ordering::Relaxed);
    SECTION_TEST_MAX_PAGE_BYTES.fetch_max(bytes, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn reset_section_resource_evidence() {
    SECTION_TEST_MAX_PAGE_ROWS.store(0, Ordering::Relaxed);
    SECTION_TEST_MAX_PAGE_BYTES.store(0, Ordering::Relaxed);
    SECTION_TEST_MAX_VALUE_BYTES.store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn section_resource_evidence() -> (usize, usize, usize) {
    (
        SECTION_TEST_MAX_PAGE_ROWS.load(Ordering::Relaxed),
        SECTION_TEST_MAX_PAGE_BYTES.load(Ordering::Relaxed),
        SECTION_TEST_MAX_VALUE_BYTES.load(Ordering::Relaxed),
    )
}

fn read_key_page(rows: &mut rusqlite::Rows<'_>) -> StoreResult<Vec<SectionKey>> {
    let mut page = Vec::new();
    let mut bytes = 0usize;
    while page.len() < SECTION_PAGE_ROWS {
        let Some(row) = rows.next()? else { break; };
        let key: SectionKey = (row.get(0)?, row.get(1)?, row.get(2)?);
        let size = key.0.len().saturating_add(key.1.len()).saturating_add(key.2.len())
            .saturating_add(std::mem::size_of::<SectionKey>());
        if !page.is_empty() && bytes.saturating_add(size) > SECTION_PAGE_BYTES { break; }
        bytes = bytes.saturating_add(size);
        page.push(key);
    }
    #[cfg(test)]
    observe_section_page(page.len(), bytes);
    Ok(page)
}

fn read_text_page(rows: &mut rusqlite::Rows<'_>) -> StoreResult<Vec<String>> {
    let mut page = Vec::new();
    let mut bytes = 0usize;
    while page.len() < SECTION_PAGE_ROWS {
        let Some(row) = rows.next()? else { break; };
        let key: String = row.get(0)?;
        let size = key.len().saturating_add(std::mem::size_of::<String>());
        if !page.is_empty() && bytes.saturating_add(size) > SECTION_PAGE_BYTES { break; }
        bytes = bytes.saturating_add(size);
        page.push(key);
    }
    #[cfg(test)]
    observe_section_page(page.len(), bytes);
    Ok(page)
}

fn local_key_page(db: &Connection, section: Section, after: &SectionKey, tombstones_only: bool) -> StoreResult<Vec<SectionKey>> {
    let mut statement = db.prepare(match section {
        Section::Hypa => "SELECT cache_key,'','' FROM hypa_embeddings
            WHERE cache_key>?1 AND ?2='' AND ?3='' AND (?4=0 OR tombstone=1) ORDER BY cache_key LIMIT 256",
        Section::LocalPlugins => "SELECT owner,space,key FROM plugin_device_storage
            WHERE (owner,space,key)>(?1,?2,?3) AND (?4=0 OR tombstone=1) ORDER BY owner,space,key LIMIT 256",
    })?;
    let mut rows = statement.query(params![after.0, after.1, after.2, tombstones_only])?;
    read_key_page(&mut rows)
}

fn local_permission_key_page(db: &Connection, after: &SectionKey) -> StoreResult<Vec<SectionKey>> {
    let mut statement = db.prepare(
        "SELECT 'pluginPermission',code_hash,permission FROM plugin_permissions
            WHERE (code_hash,permission)>(?1,?2) ORDER BY code_hash,permission LIMIT 256",
    )?;
    let mut rows = statement.query(params![after.1, after.2])?;
    read_key_page(&mut rows)
}

fn delete_current_row(tx: &Transaction<'_>, section: Section, row: &SectionRow) -> StoreResult<()> {
    match section {
        Section::Hypa => tx.execute(
            "DELETE FROM hypa_embeddings WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3 AND tombstone=?4",
            params![row.key1, row.write_clock.as_str(), row.writer_id, row.value.is_tombstone()],
        )?,
        Section::LocalPlugins => tx.execute(
            "DELETE FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3
                AND write_clock=?4 AND writer_id=?5 AND tombstone=?6",
            params![row.key1, row.key2, row.key3, row.write_clock.as_str(), row.writer_id, row.value.is_tombstone()],
        )?,
    };
    Ok(())
}

fn reclaim_prepared(tx: &Transaction<'_>, prepared: &PreparedSectionRows, floor: &Sequence) -> StoreResult<()> {
    if *floor == Sequence::from(0u64) { return Ok(()); }
    let section = prepared.synchronized_section()?;
    let mut after = (String::new(), String::new(), String::new());
    loop {
        let page = local_key_page(tx, section, &after, true)?;
        if page.is_empty() { break; }
        for key in page {
            let (row, _) = read_row(tx, section, &key)?.ok_or_else(|| invalid("Section row disappeared"))?;
            if row.value.first_published().is_some_and(|marker| marker.generation <= *floor)
                && !prepared.contains(&key, true)? {
                delete_current_row(tx, section, &row)?;
            }
            after = key;
        }
    }
    Ok(())
}

fn rejoin_prepared(tx: &Transaction<'_>, prepared: &PreparedSectionRows, observed: &Sequence, behind_floor: bool) -> StoreResult<()> {
    let section = prepared.synchronized_section()?;
    let writer_id: String = tx.query_row("SELECT writer_id FROM device_meta WHERE singleton=1", [], |row| row.get(0))?;
    let mut issued: Option<Sequence> = None;
    let mut after = (String::new(), String::new(), String::new());
    loop {
        let page = local_key_page(tx, section, &after, false)?;
        if page.is_empty() { break; }
        for key in page {
            let (mut row, published) = read_row(tx, section, &key)?.ok_or_else(|| invalid("Section row disappeared"))?;
            after = key;
            if published {
                // A complete snapshot beyond the floor is authoritative for
                // settled values. Only unpublished edits may be carried back.
                if behind_floor && !row.value.is_tombstone() && !prepared.contains(&after, false)? {
                    delete_current_row(tx, section, &row)?;
                }
                continue;
            }
            if let Some(remote) = prepared.row(&after)? {
                if row.same_version(&remote) {
                    resolve_section_row(Some(&row), &remote)?;
                    continue;
                }
            }
            if row.writer_id == writer_id && row.write_clock > *observed { continue; }
            if issued.is_none() {
                super::begin_mutation(tx)?;
                issued = Some(super::issue_write_clock(tx, section)?);
            }
            row.write_clock = issued.as_ref().expect("issued write clock").clone();
            row.writer_id = writer_id.clone();
            if row.value.is_tombstone() {
                row.value = SectionValueRow::Tombstone { first_published: None };
            }
            write_row(tx, section, &row, false)?;
        }
    }
    if issued.is_some() { super::finish_mutation(tx)?; }
    Ok(())
}

fn capture_backup_rows(
    snapshot: &Connection,
    kind: SectionKind,
    spool: &mut SectionSpoolBuilder,
) -> StoreResult<()> {
    let mut push = |row: SectionRow| spool.push_backup_row(row);
    match section_of_kind(kind) {
        Some(section) => {
            let mut statement = snapshot.prepare(match section {
                Section::Hypa => "SELECT * FROM hypa_embeddings WHERE tombstone=0 ORDER BY cache_key",
                Section::LocalPlugins => "SELECT * FROM plugin_device_storage WHERE tombstone=0 ORDER BY owner,space,key",
            })?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? { push(row_from_sql(section, row)?)?; }
        }
        None if kind == SectionKind::LocalSettings => {
            let mut statement = snapshot.prepare("SELECT key,value FROM device_settings ORDER BY key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                if setting_is_local(&key) {
                    push(SectionRow { key1: "setting".into(), key2: key, key3: String::new(),
                        value: SectionValueRow::Setting { value: row.get(1)? },
                        write_clock: Sequence::from(0u64), writer_id: String::new() })?;
                }
            }
            drop(rows);
            drop(statement);
            let mut statement = snapshot.prepare(
                "SELECT code_hash,permission,granted FROM plugin_permissions ORDER BY code_hash,permission",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                push(SectionRow { key1: "pluginPermission".into(), key2: row.get(0)?, key3: row.get(1)?,
                    value: SectionValueRow::PluginPermission { granted: row.get::<_, i64>(2)? == 1 },
                    write_clock: Sequence::from(0u64), writer_id: String::new() })?;
            }
        }
        None => return Err(invalid("Backup section kind is unsupported")),
    }
    Ok(())
}

fn read_section_state(db: &Connection, section: Section) -> StoreResult<SectionState> {
    let (participating, max_write_clock, gc_floor, participation_generation): (
        i64,
        String,
        String,
        String,
    ) = db.query_row(
        "SELECT participating,max_write_clock,gc_floor,participation_generation
            FROM device_sections WHERE section=?1",
        [section.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    Ok(SectionState {
        participating: participating == 1,
        max_write_clock: sequence(&max_write_clock)?,
        gc_floor: sequence(&gc_floor)?,
        participation_generation: sequence(&participation_generation)?,
    })
}

impl DeviceStore {
    /// Captures every selected section from one read-only SQLite snapshot and
    /// releases that snapshot after all local spools are complete.
    pub(crate) fn capture_backup_sections(
        &mut self,
        kinds: &[SectionKind],
    ) -> StoreResult<Vec<PreparedSectionRows>> {
        let mut seen = BTreeSet::new();
        let mut spools = Vec::with_capacity(kinds.len());
        for kind in kinds {
            if !seen.insert(kind.id()) { return Err(invalid("Backup section kind is repeated")); }
            spools.push((*kind, SectionSpoolBuilder::new_backup(*kind)?));
        }
        if spools.is_empty() { return Ok(Vec::new()); }
        let path: String = self.connection.query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0),
        )?;
        if path.is_empty() { return Err(invalid("Device database path is unavailable")); }
        let snapshot = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        snapshot.execute_batch(
            "PRAGMA busy_timeout=5000; PRAGMA query_only=ON; PRAGMA mmap_size=0; BEGIN;",
        )?;
        for (kind, spool) in &mut spools { capture_backup_rows(&snapshot, *kind, spool)?; }
        snapshot.execute_batch("COMMIT;")?;
        spools.into_iter().map(|(_, spool)| spool.finish_captured()).collect()
    }

    /// Visits participating synchronized rows from one read-only SQLite
    /// snapshot. The callback may spool each row before the snapshot closes.
    pub(crate) fn visit_participating_section_snapshot_mapped<E>(
        &self,
        connection_id: &str,
        library_lineage: &str,
        mut visitor: impl FnMut(SectionSnapshotEvent) -> std::result::Result<(), E>,
        mut map_error: impl FnMut(crate::persistent_store::StoreError) -> E,
    ) -> std::result::Result<(), E> {
        let path: String = self.connection.query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0),
        ).map_err(|error| map_error(error.into()))?;
        if path.is_empty() { return Err(map_error(invalid("Device database path is unavailable"))); }
        let snapshot = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ).map_err(|error| map_error(error.into()))?;
        snapshot.execute_batch(
            "PRAGMA busy_timeout=5000; PRAGMA query_only=ON; PRAGMA mmap_size=0; BEGIN;",
        ).map_err(|error| map_error(error.into()))?;
        for section in CHOOSABLE_SECTIONS {
            let state = read_section_state(&snapshot, section).map_err(&mut map_error)?;
            if !state.participating { continue; }
            let cursor = read_cursor(&snapshot, connection_id, library_lineage, section)
                .map_err(&mut map_error)?;
            visitor(SectionSnapshotEvent::Section { section, state, cursor })?;
            let mut statement = snapshot.prepare(match section {
                Section::Hypa => "SELECT * FROM hypa_embeddings ORDER BY cache_key",
                Section::LocalPlugins => "SELECT * FROM plugin_device_storage ORDER BY owner,space,key",
            }).map_err(|error| map_error(error.into()))?;
            let mut rows = statement.query([]).map_err(|error| map_error(error.into()))?;
            while let Some(row) = rows.next().map_err(|error| map_error(error.into()))? {
                visitor(SectionSnapshotEvent::Row {
                    section,
                    row: row_from_sql(section, row).map_err(&mut map_error)?,
                })?;
            }
        }
        snapshot.execute_batch("COMMIT;").map_err(|error| map_error(error.into()))?;
        Ok(())
    }

    /// The input owns a validated read-only spool. No transport or archive
    /// decoding takes place inside this single section transaction.
    pub(crate) fn apply_prepared_section_rows(
        &mut self,
        connection_id: &str,
        library_lineage: &str,
        expected_participation_generation: &Sequence,
        prepared: &PreparedSectionRows,
        cursor: &SectionCursor,
        rejoining: bool,
    ) -> StoreResult<SectionApplyOutcome> {
        let section = prepared.synchronized_section()?;
        if connection_id.is_empty() || library_lineage.is_empty()
            || cursor.applied_gc_floor > cursor.applied_generation
            || prepared.max_write_clock > cursor.observed_max_write_clock {
            return Err(invalid("Prepared section identity or bounds are invalid"));
        }
        let tx = self.transaction()?;
        let (participating, generation): (bool, String) = tx.query_row(
            "SELECT participating,participation_generation FROM device_sections WHERE section=?1",
            [section.as_str()], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !participating || sequence(&generation)? != *expected_participation_generation {
            return Err(invalid("Section participation changed during preparation"));
        }
        let current = read_cursor(&tx, connection_id, library_lineage, section)?;
        if current.as_ref().is_some_and(|current| current.applied_generation > cursor.applied_generation
            || current.applied_gc_floor > cursor.applied_gc_floor) {
            return Err(invalid("Prepared section is older than the applied cursor"));
        }
        let behind_floor = current.as_ref().is_some_and(|current| cursor.applied_gc_floor > current.applied_generation);
        let rejoining = rejoining || behind_floor || current.as_ref().is_none_or(|cursor| !cursor.joined());
        let floor = if current.is_some() { cursor.applied_gc_floor.clone() } else { Sequence::from(0u64) };
        observe_remote_clock(&tx, section, &cursor.observed_max_write_clock)?;
        if rejoining {
            reclaim_prepared(&tx, prepared, &floor)?;
            rejoin_prepared(&tx, prepared, &cursor.observed_max_write_clock, behind_floor)?;
        }
        let mut outcome = SectionApplyOutcome::default();
        prepared.visit(|row| {
            if merge_validated_row(&tx, section, &row)? { outcome.applied += 1; }
            else { outcome.kept += 1; }
            Ok(())
        })?;
        if !rejoining { reclaim_prepared(&tx, prepared, &floor)?; }
        record_cursor(&tx, connection_id, library_lineage, section, cursor)?;
        tx.commit()?;
        Ok(outcome)
    }

    pub(crate) fn read_section_entry(&self, section: Section, key: &str) -> StoreResult<Option<LocalSectionEntry>> {
        let kind = kind_of_section(section);
        let Some((mut row, published)) = read_row(&self.connection, section, &decode_entry_key(kind, key)?)? else { return Ok(None); };
        let entry = row.to_entry(kind, true)?;
        let object = match &mut row.value {
            SectionValueRow::Hypa { vector, .. } if vector.len() > MAX_INLINE_VALUE_BYTES => Some(std::mem::take(vector)),
            _ => None,
        };
        Ok(Some(LocalSectionEntry { version: row.version(), entry, object, published }))
    }

    pub(crate) fn stamp_section_removal(&self, section: Section, key: &str, generation: &Sequence, now_ms: u64) -> StoreResult<()> {
        let at_ms = i64::try_from(now_ms).map_err(|_| invalid("Device removal marker time is out of range"))?;
        let key = decode_entry_key(kind_of_section(section), key)?;
        let Some((row, _)) = read_row(&self.connection, section, &key)? else { return Ok(()); };
        if !row.value.is_tombstone() || row.value.first_published().is_some() { return Ok(()); }
        match section {
            Section::Hypa => self.connection.execute(
                "UPDATE hypa_embeddings SET first_published_generation=?4,first_published_at_ms=?5
                    WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3 AND tombstone=1 AND first_published_generation IS NULL",
                params![key.0, row.write_clock.as_str(), row.writer_id, generation.as_str(), at_ms],
            )?,
            Section::LocalPlugins => self.connection.execute(
                "UPDATE plugin_device_storage SET first_published_generation=?6,first_published_at_ms=?7
                    WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5 AND tombstone=1
                      AND first_published_generation IS NULL",
                params![key.0, key.1, key.2, row.write_clock.as_str(), row.writer_id, generation.as_str(), at_ms],
            )?,
        };
        Ok(())
    }

    pub(crate) fn has_pending_section_entries(&self, section: Section) -> StoreResult<bool> {
        let table = match section {
            Section::Hypa => "hypa_embeddings",
            Section::LocalPlugins => "plugin_device_storage",
        };
        Ok(self.connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE published_clock IS NULL OR published_clock<>write_clock)"),
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn pending_section_entry_keys(&self, section: Section, after: &str, limit: usize) -> StoreResult<Vec<String>> {
        let limit = limit.min(256);
        match section {
            Section::Hypa => {
                let mut statement = self.connection.prepare(
                    "SELECT cache_key FROM hypa_embeddings WHERE (published_clock IS NULL OR published_clock<>write_clock)
                        AND cache_key>?1 ORDER BY cache_key LIMIT ?2",
                )?;
                let keys = statement.query_map(params![after, limit as i64], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                keys.iter().map(|key| hypa_entry_key(key).map_err(section_format_error)).collect()
            }
            Section::LocalPlugins => {
                let keys = unpublished_keys(&self.connection, section)?;
                let mut encoded = keys.iter().map(|key| local_plugin_entry_key(&key.0, &key.1, &key.2).map_err(section_format_error))
                    .collect::<StoreResult<Vec<_>>>()?;
                encoded.sort();
                Ok(encoded.into_iter().filter(|key| key.as_str() > after).take(limit).collect())
            }
        }
    }

    pub(crate) fn forget_section_publications(&mut self) -> StoreResult<()> {
        let tx = self.transaction()?;
        for table in ["hypa_embeddings", "plugin_device_storage"] {
            tx.execute(&format!("UPDATE {table} SET published_clock=NULL WHERE published_clock IS NOT NULL"), [])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn write_section_entry(tx: &Transaction<'_>, write: &SectionWriteInput<'_>) -> StoreResult<()> {
        match write {
            SectionWriteInput::Apply { section, entry, object } => {
                if entry.kind != kind_of_section(*section) { return Err(invalid("Section entry belongs to another section")); }
                let row = SectionRow::from_entry((*entry).clone(), true, |_| {
                    object.map(|bytes| bytes.to_vec()).ok_or_else(|| invalid("Section vector object is missing"))
                })?;
                merge_row(tx, *section, &row)?;
                observe_remote_clock(tx, *section, &row.write_clock)?;
            }
            SectionWriteInput::Mark { section, key } => {
                let key = decode_entry_key(kind_of_section(*section), key)?;
                if let Some((row, _)) = read_row(tx, *section, &key)? {
                    mark_row_version(tx, *section, &key, &row.version(), true)?;
                }
            }
            SectionWriteInput::MarkVersion { section, key, version } => {
                mark_row_version(tx, *section, &decode_entry_key(kind_of_section(*section), key)?, version, true)?;
            }
        }
        Ok(())
    }

    pub(crate) fn section_state(&self, section: Section) -> StoreResult<SectionState> {
        read_section_state(&self.connection, section)
    }

    /// Turning a section on or off changes what this device exchanges from the
    /// next publication onward. The stored values are left alone either way.
    pub(crate) fn set_section_participating(
        &mut self,
        section: Section,
        participating: bool,
    ) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let current: i64 = transaction.query_row(
            "SELECT participating FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| row.get(0),
        )?;
        if (current == 1) != participating {
            let generation = sequence(&transaction.query_row(
                "SELECT participation_generation FROM device_sections WHERE section=?1",
                [section.as_str()],
                |row| row.get::<_, String>(0),
            )?)?
            .next()
            .map_err(|_| invalid("device participation generation is exhausted"))?;
            transaction.execute(
                "UPDATE device_sections SET participating=?1,participation_generation=?2
                    WHERE section=?3",
                params![
                    i64::from(participating),
                    generation.as_str(),
                    section.as_str()
                ],
            )?;
            if participating {
                // Keep lineage identity for removal markers, but require a
                // rejoin before exchanging values again after a pause.
                transaction.execute(
                    "UPDATE device_remote_cursors SET applied_generation='0',applied_gc_floor='0'
                        WHERE section=?1",
                    [section.as_str()],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every row of a synchronized section, tombstones included. A removal only
    /// travels while its tombstone does.
    pub(crate) fn read_section_rows(&mut self, section: Section) -> StoreResult<Vec<SectionRow>> {
        let transaction = self.transaction()?;
        let rows = read_rows(&transaction, section)?;
        transaction.commit()?;
        Ok(rows)
    }

    /// The values a backup keeps for the same device. Control rows stay behind,
    /// and a removed value is simply absent rather than carried as a removal.
    pub(crate) fn read_backup_section_rows(
        &mut self,
        section: Section,
    ) -> StoreResult<Vec<SectionRow>> {
        Ok(self
            .read_section_rows(section)?
            .into_iter()
            .filter(|row| !row.value.is_tombstone())
            .collect())
    }

    /// Device-fixed values. They are backup material only and never reach a
    /// synchronized state.
    pub(crate) fn read_local_setting_rows(&mut self) -> StoreResult<Vec<SectionRow>> {
        let transaction = self.transaction()?;
        let mut rows = Vec::new();
        {
            let mut statement = transaction
                .prepare("SELECT key,value FROM device_settings ORDER BY key")?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let key: String = row.get(0)?;
                if !setting_is_local(&key) {
                    continue;
                }
                rows.push(SectionRow {
                    key1: "setting".into(),
                    key2: key,
                    key3: String::new(),
                    value: SectionValueRow::Setting { value: row.get(1)? },
                    write_clock: Sequence::from(0u64),
                    writer_id: String::new(),
                });
            }
            let mut statement = transaction.prepare(
                "SELECT code_hash,permission,granted FROM plugin_permissions
                    ORDER BY code_hash,permission",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                rows.push(SectionRow {
                    key1: "pluginPermission".into(),
                    key2: row.get(0)?,
                    key3: row.get(1)?,
                    value: SectionValueRow::PluginPermission {
                        granted: row.get::<_, i64>(2)? == 1,
                    },
                    write_clock: Sequence::from(0u64),
                    writer_id: String::new(),
                });
            }
        }
        transaction.commit()?;
        Ok(rows)
    }

    /// Whether a participating section holds a write this remote lineage has
    /// not seen. A device value can change without the library changing, so
    /// this is what makes a section-only edit reach the remote at all.
    pub(crate) fn sections_await_publication(
        &self,
        connection_id: &str,
        library_lineage: &str,
    ) -> StoreResult<bool> {
        for section in [Section::Hypa, Section::LocalPlugins] {
            let state = self.section_state(section)?;
            if !state.participating {
                continue;
            }
            match self
                .read_section_cursor(connection_id, library_lineage, section)?
                .filter(SectionCursor::joined)
            {
                Some(_) => {
                    if !unpublished_keys(&self.connection, section)?.is_empty() {
                        return Ok(true);
                    }
                }
                // A lineage this device never exchanged with holds none of its
                // rows, however far the local counter has already travelled.
                None => {
                    if state.max_write_clock > Sequence::from(0u64) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Records the versions a confirmed publication put on the remote. Each row
    /// is matched at the version it was captured at, so a local write that
    /// landed between the capture and the publication stays unpublished.
    /// Publication bookkeeping is control metadata, so it runs outside a change
    /// context and never reaches the device change index.
    pub(crate) fn note_section_published(
        &mut self,
        section: Section,
        published: &[(SectionKey, SectionEntryVersion)],
        stamped: &[(SectionKey, SectionEntryVersion)],
        first_published: &TombstonePublication,
        reclaimed: &ReclaimedRows,
        gc_floor: &Sequence,
        cursor: Option<(&str, &str, &Sequence, &SectionCursor)>,
    ) -> StoreResult<bool> {
        let transaction = self.transaction()?;
        if let Some((_, _, expected_generation, _)) = cursor {
            let (participating, generation): (bool, String) = transaction.query_row(
                "SELECT participating,participation_generation FROM device_sections WHERE section=?1",
                [section.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if !participating || sequence(&generation)? != *expected_generation {
                transaction.commit()?;
                return Ok(false);
            }
        }
        // The removals this publication stopped carrying go with the floor it
        // published, in the transaction that records the publication, so a
        // publication that never finished reclaims nothing.
        for ((key1, key2, key3), version) in reclaimed {
            let at_ms = i64::try_from(version.first_published.at_ms)
                .map_err(|_| invalid("device removal marker time is out of range"))?;
            match section {
                Section::Hypa => {
                    transaction.execute(
                        "DELETE FROM hypa_embeddings WHERE cache_key=?1 AND tombstone=1
                            AND write_clock=?2 AND writer_id=?3
                            AND first_published_generation=?4 AND first_published_at_ms=?5",
                        params![key1, version.write_clock.as_str(), version.writer_id,
                            version.first_published.generation.as_str(), at_ms],
                    )?;
                }
                Section::LocalPlugins => {
                    transaction.execute(
                        "DELETE FROM plugin_device_storage
                            WHERE owner=?1 AND space=?2 AND key=?3 AND tombstone=1
                              AND write_clock=?4 AND writer_id=?5
                              AND first_published_generation=?6 AND first_published_at_ms=?7",
                        params![key1, key2, key3, version.write_clock.as_str(), version.writer_id,
                            version.first_published.generation.as_str(), at_ms],
                    )?;
                }
            }
        }
        let current = sequence(&transaction.query_row(
            "SELECT gc_floor FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| row.get::<_, String>(0),
        )?)?;
        if *gc_floor > current {
            transaction.execute(
                "UPDATE device_sections SET gc_floor=?1 WHERE section=?2",
                params![gc_floor.as_str(), section.as_str()],
            )?;
        }
        // A removal takes the marker the publication carried, so the device
        // file and every remote that read it name the same commit. A removal
        // rewritten since the capture is not the one that went out.
        {
            let mut statement = match section {
                Section::Hypa => transaction.prepare(
                    "UPDATE hypa_embeddings
                        SET first_published_generation=?4,first_published_at_ms=?5
                        WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3 AND tombstone=1
                          AND first_published_generation IS NULL",
                )?,
                Section::LocalPlugins => transaction.prepare(
                    "UPDATE plugin_device_storage
                        SET first_published_generation=?6,first_published_at_ms=?7
                        WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5
                          AND tombstone=1 AND first_published_generation IS NULL",
                )?,
            };
            let generation = first_published.generation.as_str();
            let at_ms = i64::try_from(first_published.at_ms)
                .map_err(|_| invalid("device removal marker time is out of range"))?;
            for ((key1, key2, key3), version) in stamped {
                match section {
                    Section::Hypa => {
                        statement.execute(params![key1, version.write_clock.as_str(),
                            version.writer_id, generation, at_ms])?;
                    }
                    Section::LocalPlugins => {
                        statement.execute(params![key1, key2, key3, version.write_clock.as_str(),
                            version.writer_id, generation, at_ms])?;
                    }
                }
            }
        }
        {
            let mut statement = match section {
                Section::Hypa => transaction.prepare(
                    "UPDATE hypa_embeddings SET published_clock=?2
                        WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3",
                )?,
                Section::LocalPlugins => transaction.prepare(
                    "UPDATE plugin_device_storage SET published_clock=?4
                        WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5",
                )?,
            };
            for ((key1, key2, key3), version) in published {
                match section {
                    Section::Hypa => {
                        statement.execute(params![key1, version.write_clock.as_str(), version.writer_id])?;
                    }
                    Section::LocalPlugins => {
                        statement.execute(params![key1, key2, key3, version.write_clock.as_str(), version.writer_id])?;
                    }
                }
            }
        }
        if let Some((connection_id, library_lineage, _, cursor)) = cursor {
            record_cursor(&transaction, connection_id, library_lineage, section, cursor)?;
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Applies immutable publication evidence a row at a time. The evidence
    /// may come from a durable job spool and is consumed inside one local
    /// transaction, so a confirmed publication is either fully recorded or
    /// left for reconciliation to retry.
    pub(crate) fn note_spooled_section_published(
        &mut self,
        section: Section,
        expected_participation_generation: &Sequence,
        first_published: &TombstonePublication,
        gc_floor: &Sequence,
        cursor: (&str, &str, &SectionCursor),
        rows: impl Iterator<Item = StoreResult<SectionPublicationRow>>,
    ) -> StoreResult<bool> {
        let transaction = self.transaction()?;
        let (participating, generation): (bool, String) = transaction.query_row(
            "SELECT participating,participation_generation FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if !participating || sequence(&generation)? != *expected_participation_generation {
            transaction.commit()?;
            return Ok(false);
        }
        let stamp_at_ms = i64::try_from(first_published.at_ms)
            .map_err(|_| invalid("device removal marker time is out of range"))?;
        for evidence in rows {
            let evidence = evidence?;
            let (key1, key2, key3) = evidence.key;
            match evidence.disposition {
                SectionPublicationDisposition::Reclaimed { first_published } => {
                    let at_ms = i64::try_from(first_published.at_ms)
                        .map_err(|_| invalid("device removal marker time is out of range"))?;
                    match section {
                        Section::Hypa => transaction.execute(
                            "DELETE FROM hypa_embeddings WHERE cache_key=?1 AND tombstone=1
                                AND write_clock=?2 AND writer_id=?3
                                AND first_published_generation=?4 AND first_published_at_ms=?5",
                            params![key1, evidence.version.write_clock.as_str(), evidence.version.writer_id,
                                first_published.generation.as_str(), at_ms],
                        )?,
                        Section::LocalPlugins => transaction.execute(
                            "DELETE FROM plugin_device_storage
                                WHERE owner=?1 AND space=?2 AND key=?3 AND tombstone=1
                                  AND write_clock=?4 AND writer_id=?5
                                  AND first_published_generation=?6 AND first_published_at_ms=?7",
                            params![key1, key2, key3, evidence.version.write_clock.as_str(),
                                evidence.version.writer_id, first_published.generation.as_str(), at_ms],
                        )?,
                    };
                }
                SectionPublicationDisposition::Published { first_published: stamp } => {
                    if let Some(stamp) = stamp {
                        if stamp != *first_published {
                            return Err(invalid("Publication removal marker differs from its spool"));
                        }
                        match section {
                            Section::Hypa => transaction.execute(
                                "UPDATE hypa_embeddings
                                    SET first_published_generation=?4,first_published_at_ms=?5
                                    WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3 AND tombstone=1
                                      AND first_published_generation IS NULL",
                                params![key1, evidence.version.write_clock.as_str(), evidence.version.writer_id,
                                    first_published.generation.as_str(), stamp_at_ms],
                            )?,
                            Section::LocalPlugins => transaction.execute(
                                "UPDATE plugin_device_storage
                                    SET first_published_generation=?6,first_published_at_ms=?7
                                    WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5
                                      AND tombstone=1 AND first_published_generation IS NULL",
                                params![key1, key2, key3, evidence.version.write_clock.as_str(),
                                    evidence.version.writer_id, first_published.generation.as_str(), stamp_at_ms],
                            )?,
                        };
                    }
                    match section {
                        Section::Hypa => transaction.execute(
                            "UPDATE hypa_embeddings SET published_clock=?2
                                WHERE cache_key=?1 AND write_clock=?2 AND writer_id=?3",
                            params![key1, evidence.version.write_clock.as_str(), evidence.version.writer_id],
                        )?,
                        Section::LocalPlugins => transaction.execute(
                            "UPDATE plugin_device_storage SET published_clock=?4
                                WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4 AND writer_id=?5",
                            params![key1, key2, key3, evidence.version.write_clock.as_str(), evidence.version.writer_id],
                        )?,
                    };
                }
            }
        }
        let current = sequence(&transaction.query_row(
            "SELECT gc_floor FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| row.get::<_, String>(0),
        )?)?;
        if *gc_floor > current {
            transaction.execute(
                "UPDATE device_sections SET gc_floor=?1 WHERE section=?2",
                params![gc_floor.as_str(), section.as_str()],
            )?;
        }
        record_cursor(&transaction, cursor.0, cursor.1, section, cursor.2)?;
        transaction.commit()?;
        Ok(true)
    }

    /// Merges a received section. The higher `(write_clock, writer_id)` wins,
    /// the same version with different content is refused, and a received row
    /// keeps the version it arrived with instead of becoming a local write.
    pub(crate) fn apply_section_rows(
        &mut self,
        section: Section,
        rows: &[SectionRow],
    ) -> StoreResult<SectionApplyOutcome> {
        let transaction = self.transaction()?;
        let mut outcome = SectionApplyOutcome::default();
        let mut highest = Sequence::from(0u64);
        let mut keys = BTreeSet::new();
        for row in rows {
            if !keys.insert(row.key()) {
                return Err(invalid("Received section rows repeat a key"));
            }
            highest = highest.max(row.write_clock.clone());
            if merge_row(&transaction, section, row)? {
                outcome.applied += 1;
            } else {
                outcome.kept += 1;
            }
        }
        observe_remote_clock(&transaction, section, &highest)?;
        transaction.commit()?;
        Ok(outcome)
    }

    pub(crate) fn read_section_cursor(
        &self,
        connection_id: &str,
        library_lineage: &str,
        section: Section,
    ) -> StoreResult<Option<SectionCursor>> {
        read_cursor(&self.connection, connection_id, library_lineage, section)
    }

    /// The applied point of one section on one remote lineage. A cursor never
    /// moves backwards, so a replayed apply cannot lose ground.
    pub(crate) fn write_section_cursor(
        &mut self,
        connection_id: &str,
        library_lineage: &str,
        section: Section,
        cursor: &SectionCursor,
    ) -> StoreResult<()> {
        let tx = self.transaction()?;
        record_cursor(&tx, connection_id, library_lineage, section, cursor)?;
        tx.commit()?;
        Ok(())
    }

    /// Forgets how far one lineage has been applied, so the section goes back
    /// through the rejoin path before this device publishes over it again. The
    /// row stays, because it is what says the markers this device holds were
    /// issued by this lineage. Explicit rejoin resets may move a cursor backwards.
    pub(crate) fn forget_section_cursor(
        &mut self,
        connection_id: &str,
        library_lineage: &str,
        section: Section,
    ) -> StoreResult<()> {
        self.connection.execute(
            "UPDATE device_remote_cursors SET applied_generation='0',applied_gc_floor='0'
                WHERE connection_id=?1 AND library_lineage=?2 AND section=?3",
            params![connection_id, library_lineage, section.as_str()],
        )?;
        Ok(())
    }

    /// Replaces exactly one present backup section. An empty spool clears the
    /// selected scope; callers preserve an absent scope by not invoking this.
    pub(crate) fn restore_prepared_backup_section(
        &mut self,
        prepared: &PreparedSectionRows,
    ) -> StoreResult<()> {
        if prepared.versioned { return Err(invalid("Synchronized section input is not backup material")); }
        match section_of_kind(prepared.kind) {
            Some(section) => self.restore_prepared_value_section(section, prepared),
            None if prepared.kind == SectionKind::LocalSettings => self.restore_prepared_local_settings(prepared),
            None => Err(invalid("Backup section kind is unsupported")),
        }
    }

    fn restore_prepared_value_section(
        &mut self,
        section: Section,
        prepared: &PreparedSectionRows,
    ) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let writer_id: String = transaction.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1", [], |row| row.get(0),
        )?;
        let mut changed = false;
        prepared.visit(|mut row| {
            if row.value.is_tombstone() { return Err(invalid("Restored section row has no value")); }
            let settled = read_row(&transaction, section, &row.key())?.is_some_and(|(current, published)| {
                !published && current.writer_id == writer_id && current.value.same_content(&row.value)
            });
            if settled { return Ok(()); }
            if !changed { super::begin_mutation(&transaction)?; changed = true; }
            row.write_clock = super::issue_write_clock(&transaction, section)?;
            row.writer_id = writer_id.clone();
            write_row(&transaction, section, &row, false)
        })?;
        let mut after = (String::new(), String::new(), String::new());
        loop {
            let page = local_key_page(&transaction, section, &after, false)?;
            if page.is_empty() { break; }
            for key in page {
                let (mut current, _) = read_row(&transaction, section, &key)?
                    .ok_or_else(|| invalid("Section row disappeared"))?;
                after = key;
                if current.value.is_tombstone() || prepared.contains(&after, false)? { continue; }
                if !changed { super::begin_mutation(&transaction)?; changed = true; }
                current.value = SectionValueRow::Tombstone { first_published: None };
                current.write_clock = super::issue_write_clock(&transaction, section)?;
                current.writer_id = writer_id.clone();
                write_row(&transaction, section, &current, false)?;
            }
        }
        if changed { super::finish_mutation(&transaction)?; }
        transaction.commit()?;
        Ok(())
    }

    fn restore_prepared_local_settings(&mut self, prepared: &PreparedSectionRows) -> StoreResult<()> {
        let transaction = self.transaction()?;
        prepared.visit(|row| match row.value {
            SectionValueRow::Setting { value } => {
                if row.key1 != "setting" || !setting_is_local(&row.key2) || !row.key3.is_empty() {
                    return Err(invalid("Restored device setting is not a device setting"));
                }
                transaction.execute(
                    "INSERT INTO device_settings (key,value) VALUES (?1,?2)
                        ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    params![row.key2, value],
                )?;
                Ok(())
            }
            SectionValueRow::PluginPermission { granted } => {
                if row.key1 != "pluginPermission" || row.key2.is_empty() || row.key3.is_empty() {
                    return Err(invalid("Restored plugin permission is incomplete"));
                }
                transaction.execute(
                    "INSERT INTO plugin_permissions (code_hash,permission,granted)
                        VALUES (?1,?2,?3)
                        ON CONFLICT(code_hash,permission) DO UPDATE SET granted=excluded.granted",
                    params![row.key2, row.key3, i64::from(granted)],
                )?;
                Ok(())
            }
            _ => Err(invalid("Restored device setting has the wrong shape")),
        })?;
        for key in LOCAL_SETTING_KEYS {
            let row_key = ("setting".to_owned(), key.to_owned(), String::new());
            if !prepared.contains(&row_key, false)? {
                transaction.execute("DELETE FROM device_settings WHERE key=?1", [key])?;
            }
        }
        let mut after = (String::new(), String::new(), String::new());
        loop {
            let page = local_permission_key_page(&transaction, &after)?;
            if page.is_empty() { break; }
            for key in page {
                after = key;
                if !prepared.contains(&after, false)? {
                    transaction.execute(
                        "DELETE FROM plugin_permissions WHERE code_hash=?1 AND permission=?2",
                        params![after.1, after.2],
                    )?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Installs backup material for the same device. A restored value is this
    /// device's own write, so it takes a freshly issued clock and this writer
    /// rather than whatever produced the bundle. The section is replaced: a key
    /// the material leaves out is removed, so restoring an empty section empties
    /// it. A value this device already holds unpublished under its own writer is
    /// left alone, which keeps a retried restore from issuing a second clock for
    /// something it already wrote.
    pub(crate) fn restore_section_rows(
        &mut self,
        section: Section,
        rows: &[SectionRow],
    ) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let writer_id: String = transaction.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let restored: BTreeSet<(String, String, String)> =
            rows.iter().map(SectionRow::key).collect();
        if restored.len() != rows.len() {
            return Err(invalid("restored section rows repeat a key"));
        }
        let unpublished = unpublished_keys(&transaction, section)?;
        let held: BTreeMap<(String, String, String), SectionRow> = read_rows(&transaction, section)?
            .into_iter()
            .map(|row| (row.key(), row))
            .collect();
        super::begin_mutation(&transaction)?;
        for row in rows {
            if row.value.is_tombstone() {
                return Err(invalid("restored section row has no value"));
            }
            let settled = held.get(&row.key()).is_some_and(|current| {
                current.value.same_content(&row.value)
                    && current.writer_id == writer_id
                    && unpublished.contains(&row.key())
            });
            if settled {
                continue;
            }
            let clock = super::issue_write_clock(&transaction, section)?;
            write_row(
                &transaction,
                section,
                &SectionRow {
                    write_clock: clock,
                    writer_id: writer_id.clone(),
                    ..row.clone()
                },
                false,
            )?;
        }
        for (key, current) in &held {
            if restored.contains(key) || current.value.is_tombstone() {
                continue;
            }
            let clock = super::issue_write_clock(&transaction, section)?;
            write_row(
                &transaction,
                section,
                &SectionRow {
                    value: SectionValueRow::Tombstone {
                        first_published: None,
                    },
                    write_clock: clock,
                    writer_id: writer_id.clone(),
                    ..current.clone()
                },
                false,
            )?;
        }
        super::finish_mutation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Installs backup material for the same device. Versions are reissued
    /// locally because a bundle carries user values without them. The area is
    /// replaced within its own bounds: a local setting or permission the
    /// material leaves out is removed, while every device setting outside the
    /// backed-up list keeps whatever this device holds.
    pub(crate) fn restore_local_setting_rows(&mut self, rows: &[SectionRow]) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let mut settings = BTreeSet::new();
        let mut permissions = BTreeSet::new();
        for row in rows {
            match &row.value {
                SectionValueRow::Setting { value } => {
                    if row.key1 != "setting" || !setting_is_local(&row.key2) {
                        return Err(invalid("restored device setting is not a device setting"));
                    }
                    if !settings.insert(row.key2.clone()) {
                        return Err(invalid("restored device settings repeat a key"));
                    }
                    transaction.execute(
                        "INSERT INTO device_settings (key,value) VALUES (?1,?2)
                            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![row.key2, value],
                    )?;
                }
                SectionValueRow::PluginPermission { granted } => {
                    if row.key1 != "pluginPermission" || row.key2.is_empty() || row.key3.is_empty()
                    {
                        return Err(invalid("restored plugin permission is incomplete"));
                    }
                    if !permissions.insert((row.key2.clone(), row.key3.clone())) {
                        return Err(invalid("restored plugin permissions repeat a key"));
                    }
                    transaction.execute(
                        "INSERT INTO plugin_permissions (code_hash,permission,granted)
                            VALUES (?1,?2,?3)
                            ON CONFLICT(code_hash,permission) DO UPDATE SET granted=excluded.granted",
                        params![row.key2, row.key3, i64::from(*granted)],
                    )?;
                }
                _ => return Err(invalid("restored device setting has the wrong shape")),
            }
        }
        for key in LOCAL_SETTING_KEYS {
            if !settings.contains(key) {
                transaction.execute("DELETE FROM device_settings WHERE key=?1", [key])?;
            }
        }
        let held = {
            let mut statement =
                transaction.prepare("SELECT code_hash,permission FROM plugin_permissions")?;
            let mut query = statement.query([])?;
            let mut held = Vec::new();
            while let Some(row) = query.next()? {
                held.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
            }
            held
        };
        for key in held {
            if !permissions.contains(&key) {
                transaction.execute(
                    "DELETE FROM plugin_permissions WHERE code_hash=?1 AND permission=?2",
                    params![key.0, key.1],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// `published` is false for a restored value: it is a new local write that no
/// remote has seen yet.
fn write_row(
    tx: &Transaction<'_>,
    section: Section,
    row: &SectionRow,
    published: bool,
) -> StoreResult<()> {
    let published_clock = published.then(|| row.write_clock.as_str().to_owned());
    let marker = row.value.first_published();
    let generation = marker.map(|marker| marker.generation.as_str().to_owned());
    let at_ms = marker.map(|marker| i64::try_from(marker.at_ms)
        .map_err(|_| invalid("Device removal marker time is out of range"))).transpose()?;
    match (section, &row.value) {
        (Section::Hypa, SectionValueRow::Tombstone { .. }) => {
            tx.execute(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock,
                     first_published_generation,first_published_at_ms)
                    VALUES (?1,'','',NULL,0,1,NULL,NULL,1,?2,?3,?4,?5,?6)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        vector=NULL,metadata=NULL,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=excluded.first_published_generation,
                        first_published_at_ms=excluded.first_published_at_ms",
                params![
                    row.key1,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock,
                    generation,
                    at_ms
                ],
            )?;
        }
        (
            Section::Hypa,
            SectionValueRow::Hypa {
                producer,
                model,
                endpoint,
                preprocess_version,
                dimensions,
                vector,
                metadata,
            },
        ) => {
            tx.execute(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock,
                     first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11,NULL,NULL)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        producer=excluded.producer,model=excluded.model,
                        endpoint=excluded.endpoint,
                        preprocess_version=excluded.preprocess_version,
                        dimensions=excluded.dimensions,vector=excluded.vector,
                        metadata=excluded.metadata,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=NULL,first_published_at_ms=NULL",
                params![
                    row.key1,
                    producer,
                    model,
                    endpoint,
                    preprocess_version,
                    dimensions,
                    vector,
                    metadata,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        (Section::LocalPlugins, SectionValueRow::Tombstone { .. }) => {
            tx.execute(
                "INSERT INTO plugin_device_storage
                    (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                     published_clock,first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,NULL,0,1,?4,?5,?6,?7,?8)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=NULL,byte_size=0,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=excluded.first_published_generation,
                        first_published_at_ms=excluded.first_published_at_ms",
                params![
                    row.key1,
                    row.key2,
                    row.key3,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock,
                    generation,
                    at_ms
                ],
            )?;
        }
        (Section::LocalPlugins, SectionValueRow::Plugin { space, value }) => {
            if *space != row.key2 {
                return Err(invalid("received plugin value names another space"));
            }
            let byte_size = i64::try_from(value.len())
                .map_err(|_| invalid("received plugin value is too large"))?;
            tx.execute(
                "INSERT INTO plugin_device_storage
                    (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                     published_clock,first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,?4,?5,0,?6,?7,?8,NULL,NULL)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=excluded.value,byte_size=excluded.byte_size,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=NULL,first_published_at_ms=NULL",
                params![
                    row.key1,
                    row.key2,
                    row.key3,
                    value,
                    byte_size,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        _ => return Err(invalid("received section row belongs to another section")),
    }
    Ok(())
}

#[cfg(test)]
#[path = "section_merge_tests.rs"]
mod merge_tests;
