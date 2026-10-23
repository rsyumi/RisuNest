//! Section rows as a remote adapter sees them. Publication reads rows here and
//! reception merges them back, so one rule decides every value no matter which
//! remote carried it.
use super::{invalid, observe_remote_clock, sequence, DeviceStore, Section};
use crate::persistent_store::StoreResult;
use risunest_external_storage_format::section::{
    decode_local_plugin_entry_key, hypa_entry_key, local_plugin_entry_key, HypaValue,
    InlineOrObject, LocalPluginValue, LocalSettingValue, ObjectReference, PluginSpace, PluginValue,
    SectionEntry, SectionEntryVersion, SectionKind, SectionValue, ValueOrObject,
    MAX_INLINE_VALUE_BYTES,
};
use risunest_sync_wire::Sequence;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use super::plugin_values::{self, text_value};

mod large_plugin;

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
    PluginBinary {
        object: plugin_values::raw_value::BinaryInfo,
        parts: Vec<ObjectReference>,
    },
    PluginObject {
        space: String,
        object: text_value::TextInfo,
        parts: Vec<ObjectReference>,
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
            (Self::PluginObject { space: a, object: x, .. }, Self::PluginObject { space: b, object: y, .. }) => a == b && x == y,
            (Self::PluginBinary { object: a, .. }, Self::PluginBinary { object: b, .. }) => a == b,
            (Self::Plugin { space, value }, Self::PluginObject { space: other_space, object, .. })
            | (Self::PluginObject { space: other_space, object, .. }, Self::Plugin { space, value }) => {
                space == other_space && text_value::encode(value.as_bytes(), std::io::sink(), text_value::MAX_VALUE_BYTES).ok().as_ref() == Some(object)
            }
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
    /// The bytes this row's entry names as an object instead of carrying them
    /// inline: a vector, or a plugin or setting value's stored text.
    pub(crate) fn object_body(&self) -> Option<&[u8]> {
        let bytes = match self {
            Self::Hypa { vector, .. } => vector.as_slice(),
            Self::Plugin { value, .. } | Self::Setting { value } => value.as_bytes(),
            Self::PluginBinary { .. } | Self::PluginObject { .. } | Self::PluginPermission { .. } | Self::Tombstone { .. } => return None,
        };
        rides_as_object(bytes).then_some(bytes)
    }
    fn take_object_body(&mut self) -> Option<Vec<u8>> {
        self.object_body()?;
        match self {
            Self::Hypa { vector, .. } => Some(std::mem::take(vector)),
            Self::Plugin { value, .. } | Self::Setting { value } => Some(std::mem::take(value).into_bytes()),
            Self::PluginBinary { .. } | Self::PluginObject { .. } | Self::PluginPermission { .. } | Self::Tombstone { .. } => None,
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

fn setting_is_local(key: &str) -> bool {
    LOCAL_SETTING_KEYS.contains(&key)
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
                || std::mem::discriminant(&self.value) == std::mem::discriminant(&other.value)
                || matches!((&self.value, &other.value), (SectionValueRow::Plugin { .. }, SectionValueRow::PluginObject { .. })
                    | (SectionValueRow::PluginObject { .. }, SectionValueRow::Plugin { .. })))
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

fn rides_as_object(bytes: &[u8]) -> bool {
    bytes.len() > MAX_INLINE_VALUE_BYTES
}

fn object_reference(bytes: &[u8]) -> ObjectReference {
    ObjectReference {
        content_sha256: risunest_external_storage_format::content_identity::hash(bytes),
        byte_length: bytes.len() as u64,
    }
}

fn is_json(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// Stored JSON text as its entry carries it. Text that does not parse is
/// refused at any length; past the inline limit the entry names the text.
fn json_value(text: &str) -> StoreResult<ValueOrObject> {
    if !rides_as_object(text.as_bytes()) {
        return serde_json::from_str(text)
            .map(ValueOrObject::Inline)
            .map_err(|_| invalid("Device value is not JSON"));
    }
    if !is_json(text) {
        return Err(invalid("Device value is not JSON"));
    }
    Ok(ValueOrObject::Object(object_reference(text.as_bytes())))
}

fn read_object(
    reference: &ObjectReference,
    object: &mut impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>,
) -> StoreResult<Vec<u8>> {
    let bytes = object(reference)?;
    if bytes.len() as u64 != reference.byte_length
        || risunest_external_storage_format::content_identity::hash(&bytes) != reference.content_sha256 {
        return Err(invalid("Section object does not match its reference"));
    }
    Ok(bytes)
}

/// The stored text an object holds. It is installed byte for byte, so it has
/// to be text, and JSON text has to parse.
fn object_text(
    reference: &ObjectReference,
    object: &mut impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>,
    json: bool,
) -> StoreResult<String> {
    let text = String::from_utf8(read_object(reference, object)?)
        .map_err(|_| invalid("Section value object is not text"))?;
    if json && !is_json(&text) {
        return Err(invalid("Section value object is not JSON"));
    }
    Ok(text)
}

fn json_text(
    value: ValueOrObject,
    object: &mut impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>,
) -> StoreResult<String> {
    match value {
        ValueOrObject::Inline(value) => Ok(serde_json::to_string(&value)?),
        ValueOrObject::Object(reference) => object_text(&reference, object, true),
    }
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

fn plugin_columns() -> String {
    format!("rowid AS value_rowid,typeof(value)='blob' AS binary,owner,space,key,CASE WHEN byte_size<={} THEN value END AS value,byte_size,tombstone,write_clock,writer_id,published_clock,first_published_generation,first_published_at_ms", text_value::PART_BYTES)
}

fn row_from_sql(db: &Connection, section: Section, row: &rusqlite::Row<'_>, objects: Option<&Connection>) -> StoreResult<SectionRow> {
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
            Section::LocalPlugins => {
                let size: i64 = row.get("byte_size")?;
                let space: String = row.get("space")?;
                if row.get::<_, bool>("binary")? {
                    let rowid = row.get("value_rowid")?;
                    if space != "json" { return Err(invalid("Binary plugin value belongs to another space")); }
                    large_plugin::capture_binary(objects, || Ok(db.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?))?
                } else if size > text_value::PART_BYTES as i64 {
                    let rowid = row.get("value_rowid")?;
                    large_plugin::capture(objects, &space, || Ok(db.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?))?
                } else { SectionValueRow::Plugin { space, value: row.get("value")? } }
            }
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
    let sql = match section {
        Section::Hypa => "SELECT * FROM hypa_embeddings ORDER BY cache_key".into(),
        Section::LocalPlugins => format!("SELECT {} FROM plugin_device_storage ORDER BY owner,space,key", plugin_columns()),
    };
    let mut statement = db.prepare(&sql)?;
    let mut query = statement.query([])?;
    let mut rows = Vec::new();
    while let Some(row) = query.next()? {
        rows.push(row_from_sql(db, section, row, None)?);
    }
    Ok(rows)
}

fn read_row(db: &Connection, section: Section, key: &SectionKey) -> StoreResult<Option<(SectionRow, bool)>> {
    let sql = match section {
        Section::Hypa => "SELECT * FROM hypa_embeddings WHERE cache_key=?1 AND ?2='' AND ?3=''".into(),
        Section::LocalPlugins => format!("SELECT {} FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3", plugin_columns()),
    };
    let mut statement = db.prepare(&sql)?;
    let mut query = statement.query(params![key.0, key.1, key.2])?;
    let Some(row) = query.next()? else { return Ok(None); };
    let current = row_from_sql(db, section, row, None)?;
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
                let vector = if rides_as_object(vector) {
                    InlineOrObject::Object(object_reference(vector))
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
                    "string" if rides_as_object(value.as_bytes()) => {
                        (PluginSpace::String, ValueOrObject::Object(object_reference(value.as_bytes())))
                    }
                    "string" => (PluginSpace::String, ValueOrObject::Inline(serde_json::Value::String(value.clone()))),
                    "json" => (PluginSpace::Json, json_value(value)?),
                    _ => return Err(invalid("Plugin device space is invalid")),
                };
                SectionValue::LocalPlugin(LocalPluginValue { space, value: value.into() })
            }
            (SectionValueRow::PluginBinary { parts, .. }, SectionKind::LocalPlugins) => {
                if self.key2 != "json" { return Err(invalid("Binary plugin value belongs to another space")); }
                SectionValue::LocalPlugin(LocalPluginValue { space: PluginSpace::Json, value: PluginValue::Binary(parts.clone()) })
            }
            (SectionValueRow::PluginObject { space, parts, .. }, SectionKind::LocalPlugins) => {
                if space != &self.key2 { return Err(invalid("Plugin value names another space")); }
                let space = match space.as_str() { "string" => PluginSpace::String, "json" => PluginSpace::Json,
                    _ => return Err(invalid("Plugin device space is invalid")) };
                let value = if parts.len() == 1 { PluginValue::Object(parts[0].clone()) } else { PluginValue::Objects(parts.clone()) };
                SectionValue::LocalPlugin(LocalPluginValue { space, value })
            }
            (SectionValueRow::Setting { value }, SectionKind::LocalSettings) => {
                if self.key1 != "setting" || !setting_is_local(&self.key2) || !self.key3.is_empty() {
                    return Err(invalid("Device setting is outside the backup scope"));
                }
                SectionValue::LocalSetting(LocalSettingValue { value: json_value(value)? })
            }
            (SectionValueRow::PluginPermission { granted }, SectionKind::LocalSettings) => {
                if self.key1 != "pluginPermission" || self.key2.is_empty() || self.key3.is_empty() {
                    return Err(invalid("Plugin permission key is invalid"));
                }
                SectionValue::LocalSetting(LocalSettingValue {
                    value: ValueOrObject::Inline(serde_json::Value::Bool(*granted)),
                })
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
                    InlineOrObject::Object(reference) => read_object(reference, &mut object)?,
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
                let text = match (value.space, value.value) {
                    (PluginSpace::String, PluginValue::Inline(serde_json::Value::String(text))) => text,
                    (PluginSpace::String, PluginValue::Inline(_)) => return Err(invalid("Plugin string value is not text")),
                    (PluginSpace::String, PluginValue::Object(reference)) => object_text(&reference, &mut object, false)?,
                    (PluginSpace::Json, PluginValue::Inline(value)) => json_text(ValueOrObject::Inline(value), &mut object)?,
                    (PluginSpace::Json, PluginValue::Object(reference)) => json_text(ValueOrObject::Object(reference), &mut object)?,
                    (_, PluginValue::Binary(_)) => return Err(invalid("Binary plugin value requires a section spool")),
                    (_, PluginValue::Objects(_)) => return Err(invalid("Multipart plugin value requires a section spool")),
                };
                if text_value::encoded_length(&text) > text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
                SectionValueRow::Plugin { space: space.to_owned(), value: text }
            }
            SectionValue::LocalSetting(value) => match key1.as_str() {
                "setting" if setting_is_local(&key2) && key3.is_empty() => {
                    SectionValueRow::Setting { value: json_text(value.value, &mut object)? }
                }
                "pluginPermission" if !key2.is_empty() && !key3.is_empty() => SectionValueRow::PluginPermission {
                    granted: match value.value {
                        ValueOrObject::Inline(serde_json::Value::Bool(granted)) => granted,
                        _ => return Err(invalid("Plugin permission is not a decision")),
                    },
                },
                _ => return Err(invalid("Device setting is outside the backup scope")),
            },
        };
        Ok(Self { key1, key2, key3, value, write_clock, writer_id })
    }
}

/// An unfinished spool cannot be applied. It becomes a read-only input only
/// after every received row and the transport fingerprint have been checked.
pub(crate) struct SectionSpoolBuilder {
    connection: Connection,
    directory: tempfile::TempDir,
    kind: SectionKind,
    versioned: bool,
    count: i64,
}

pub(crate) struct PreparedSectionRows {
    connection: Connection,
    // Declared after the connection so Windows closes the file before cleanup.
    _directory: tempfile::TempDir,
    kind: SectionKind,
    versioned: bool,
    count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FrozenBackupSections {
    pub(crate) hypa: Vec<SectionRow>,
    pub(crate) local_plugins: Vec<SectionRow>,
    local_settings: Vec<SectionRow>,
}

pub(crate) fn freeze_backup_sections(sections: &[&PreparedSectionRows]) -> StoreResult<FrozenBackupSections> {
    let mut frozen = FrozenBackupSections { hypa: vec![], local_plugins: vec![], local_settings: vec![] };
    let mut seen = BTreeSet::new();
    for prepared in sections {
        if prepared.versioned || !seen.insert(prepared.kind.id()) {
            return Err(invalid("Replacement device sections are repeated or versioned"));
        }
        let rows = match prepared.kind {
            SectionKind::Hypa => &mut frozen.hypa,
            SectionKind::LocalPlugins => &mut frozen.local_plugins,
            SectionKind::LocalSettings => &mut frozen.local_settings,
        };
        prepared.visit(|mut row| {
            if row.value.is_tombstone() { return Err(invalid("Replacement device row has no value")); }
            row.write_clock = Sequence::from(0u64);
            row.writer_id.clear();
            rows.push(row);
            Ok(())
        })?;
    }
    if sections.len() != 3 { return Err(invalid("Replacement device sections are incomplete")); }
    Ok(frozen)
}

pub(crate) fn capture_replacement_device_rows(db: &Connection) -> StoreResult<FrozenBackupSections> {
    Ok(FrozenBackupSections {
        hypa: read_rows(db, Section::Hypa)?,
        local_plugins: read_rows(db, Section::LocalPlugins)?,
        local_settings: vec![],
    })
}

pub(crate) fn replacement_local_settings_digest(db: &Connection) -> StoreResult<String> {
    let mut digest = Sha256::new();
    digest.update(b"risunest.replacement-local-settings/v1\0");
    let mut frame = |bytes: &[u8]| {
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    };
    for key in LOCAL_SETTING_KEYS {
        let value: Option<String> = db.query_row("SELECT value FROM device_settings WHERE key=?1", [key], |row| row.get(0)).optional()?;
        frame(&serde_json::to_vec(&(key, value))?);
    }
    let mut statement = db.prepare("SELECT code_hash,permission,granted FROM plugin_permissions ORDER BY code_hash,permission")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let permission: (String, String, i64) = (row.get(0)?, row.get(1)?, row.get(2)?);
        frame(&serde_json::to_vec(&permission)?);
    }
    Ok(hex::encode(digest.finalize()))
}

pub(crate) fn restore_frozen_backup_sections(
    tx: &Transaction<'_>,
    frozen: &FrozenBackupSections,
    stamp: &risunest_sync_wire::stamp::Stamp,
) -> StoreResult<()> {
    let mut clock: Option<Sequence> = None;
    let mut next_clock = || -> StoreResult<Sequence> {
        if let Some(clock) = &clock { return Ok(clock.clone()); }
        let previous: String = tx.query_row(
            "SELECT max_write_clock FROM device_sections ORDER BY length(max_write_clock) DESC,max_write_clock DESC LIMIT 1",
            [], |row| row.get(0),
        )?;
        let next = sequence(&previous)?.next().map_err(|_| invalid("device write clock is exhausted"))?;
        clock = Some(next.clone());
        Ok(next)
    };
    for (section, incoming) in [(Section::Hypa, &frozen.hypa), (Section::LocalPlugins, &frozen.local_plugins)] {
        let desired: BTreeMap<_, _> = incoming.iter().map(|row| (row.key(), row)).collect();
        let mut changed = false;
        for row in incoming {
            if read_row(tx, section, &row.key())?.is_some_and(|(old, _)| old.value.same_content(&row.value)) { continue; }
            let mut row = row.clone();
            row.write_clock = next_clock()?;
            row.writer_id = stamp.writer_id.clone();
            write_row(tx, section, &row, false)?;
            changed = true;
        }
        let mut after = (String::new(), String::new(), String::new());
        loop {
            let page = local_key_page(tx, section, &after, false)?;
            if page.is_empty() { break; }
            for key in page {
                let (mut row, _) = read_row(tx, section, &key)?.ok_or_else(|| invalid("Device replacement row disappeared"))?;
                after = key;
                if row.value.is_tombstone() || desired.contains_key(&after) { continue; }
                row.value = SectionValueRow::Tombstone { first_published: None };
                row.write_clock = next_clock()?;
                row.writer_id = stamp.writer_id.clone();
                write_row(tx, section, &row, false)?;
                changed = true;
            }
        }
        if changed { tx.execute("UPDATE device_sections SET max_write_clock=?1 WHERE section=?2", params![next_clock()?.as_str(),section.as_str()])?; }
    }
    let mut settings = BTreeSet::new();
    let mut permissions = BTreeSet::new();
    for row in &frozen.local_settings {
        match &row.value {
            SectionValueRow::Setting { value } if row.key1 == "setting" && setting_is_local(&row.key2) && row.key3.is_empty() => {
                tx.execute("INSERT INTO device_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![row.key2,value])?;
                settings.insert(row.key2.as_str());
            }
            SectionValueRow::PluginPermission { granted } if row.key1 == "pluginPermission" && !row.key2.is_empty() && !row.key3.is_empty() => {
                tx.execute("INSERT INTO plugin_permissions(code_hash,permission,granted) VALUES(?1,?2,?3) ON CONFLICT(code_hash,permission) DO UPDATE SET granted=excluded.granted", params![row.key2,row.key3,i64::from(*granted)])?;
                permissions.insert(row.key());
            }
            _ => return Err(invalid("Replacement device setting has the wrong shape")),
        }
    }
    for key in LOCAL_SETTING_KEYS {
        if !settings.contains(key) { tx.execute("DELETE FROM device_settings WHERE key=?1", [key])?; }
    }
    let mut after = (String::new(), String::new(), String::new());
    loop {
        let page = local_permission_key_page(tx, &after)?;
        if page.is_empty() { break; }
        for key in page {
            after = key;
            if !permissions.contains(&after) {
                tx.execute("DELETE FROM plugin_permissions WHERE code_hash=?1 AND permission=?2", params![after.1,after.2])?;
            }
        }
    }
    Ok(())
}

impl SectionSpoolBuilder {
    /// `scratch` is an app-owned directory; the spool keeps its rows in a
    /// temporary directory inside it.
    pub(crate) fn new(section: Section, scratch: &std::path::Path) -> StoreResult<Self> {
        Self::open(kind_of_section(section), true, scratch)
    }

    pub(crate) fn new_backup(kind: SectionKind, scratch: &std::path::Path) -> StoreResult<Self> {
        Self::open(kind, false, scratch)
    }

    fn open(kind: SectionKind, versioned: bool, scratch: &std::path::Path) -> StoreResult<Self> {
        let directory = tempfile::Builder::new().prefix("section-spool-").tempdir_in(scratch)?;
        let connection = crate::sqlite_open::open(directory.path().join("rows.sqlite"))?;
        connection.execute_batch(
            "PRAGMA cache_size=-4096;
             PRAGMA temp_store=FILE;
             PRAGMA mmap_size=0;
             CREATE TABLE vector_values(hash TEXT PRIMARY KEY,bytes BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE section_objects(hash TEXT PRIMARY KEY,bytes BLOB NOT NULL) WITHOUT ROWID;
             CREATE TABLE message_page_objects(hash TEXT PRIMARY KEY,body BLOB NOT NULL);
             CREATE TABLE message_page_verified_objects(hash TEXT PRIMARY KEY);
             CREATE TABLE message_page_object_marks(hash TEXT PRIMARY KEY,unreferenced_since INTEGER NOT NULL);
             CREATE TABLE row_values(id INTEGER PRIMARY KEY,metadata TEXT NOT NULL,vector_hash TEXT);
             CREATE TABLE row_index(
                key1 TEXT NOT NULL,key2 TEXT NOT NULL,key3 TEXT NOT NULL,
                body_id INTEGER NOT NULL,write_clock TEXT NOT NULL,writer_id TEXT NOT NULL,
                tombstone INTEGER NOT NULL,entry_key TEXT NOT NULL UNIQUE,entry_hash BLOB NOT NULL,
                PRIMARY KEY(key1,key2,key3)
             ) WITHOUT ROWID;
             BEGIN IMMEDIATE;",
        )?;
        Ok(Self { connection, directory, kind, versioned, count: 0 })
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
        let row = self.decode_entry(entry, false, object)?;
        if row.value.is_tombstone() { return Err(invalid("Backup section entry is a removal")); }
        self.push_row(row, &entry_key, &entry_hash)
    }

    pub(super) fn push_backup_row(&mut self, row: SectionRow) -> StoreResult<()> {
        if self.versioned || row.value.is_tombstone() { return Err(invalid("Backup section row is invalid")); }
        let entry = row.to_entry(self.kind, false)?;
        let entry_key = entry.key.clone();
        let entry_hash = risunest_external_storage_format::content_identity::hash(
            &entry.encode().map_err(section_format_error)?,
        );
        self.push_row(row, &entry_key, &entry_hash)
    }

    fn push_row(&mut self, mut row: SectionRow, entry_key: &str, entry_hash: &[u8; 32]) -> StoreResult<()> {
        if let SectionValueRow::Plugin { space, value } = &row.value {
            if text_value::encoded_length(value) > text_value::PART_BYTES as u64 {
                row.value = large_plugin::capture(Some(&self.connection), space, || Ok(std::io::Cursor::new(value.as_bytes())))?;
            }
        }
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
        let connection = crate::sqlite_open::open_with_flags(
            self.directory.path().join("rows.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.execute_batch("PRAGMA cache_size=-4096; PRAGMA mmap_size=0; PRAGMA query_only=ON; BEGIN;")?;
        let count: i64 = connection.query_row("SELECT count(*) FROM row_index", [], |row| row.get(0))?;
        if count != self.count { return Err(invalid("Section spool is incomplete")); }
        Ok(PreparedSectionRows { connection, _directory: self.directory,
            kind: self.kind, versioned: self.versioned, count })
    }

    pub(super) fn finish_captured(self) -> StoreResult<PreparedSectionRows> {
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
        visitor: impl FnMut(&SectionEntry, &mut dyn FnMut(&ObjectReference) -> StoreResult<Vec<u8>>) -> StoreResult<()>,
    ) -> StoreResult<()> {
        self.visit_entries_mapped(visitor, |error| error)
    }

    pub(crate) fn visit_entries_mapped<E>(
        &self,
        mut visitor: impl FnMut(&SectionEntry, &mut dyn FnMut(&ObjectReference) -> StoreResult<Vec<u8>>) -> std::result::Result<(), E>,
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
                let mut object = row.value.take_object_body();
                visitor(&entry, &mut |reference| {
                    let bytes = match object.take() {
                        Some(bytes) => bytes,
                        None => self.connection.query_row("SELECT bytes FROM section_objects WHERE hash=?1",
                            [hex::encode(reference.content_sha256)], |row| row.get(0))?,
                    };
                    if bytes.len() as u64 != reference.byte_length || object_reference(&bytes) != *reference {
                        return Err(invalid("Section object does not match its reference"));
                    }
                    #[cfg(test)]
                    SECTION_TEST_MAX_VALUE_BYTES.fetch_max(bytes.len(), Ordering::Relaxed);
                    Ok(bytes)
                })?;
                visited += 1;
                after = key;
            }
        }
        if visited != self.count { return Err(map_error(invalid("Section spool scan is incomplete"))); }
        Ok(())
    }
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
            if space == "json" { serde_json::from_str::<serde::de::IgnoredAny>(value)?; }
            if text_value::encoded_length(value) > text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
        }
        (Section::LocalPlugins, SectionValueRow::PluginBinary { object, .. }) => {
            if row.key2 != "json" || object.byte_length == 0 || object.byte_length > text_value::MAX_VALUE_BYTES { return Err(invalid("Binary plugin value has an invalid identity or size")); }
        }
        (Section::LocalPlugins, SectionValueRow::PluginObject { space, object, .. }) => {
            if space != &row.key2 || !matches!(space.as_str(), "string" | "json") || object.encoded_length > text_value::MAX_VALUE_BYTES {
                return Err(invalid("Plugin value names another space or is too large"));
            }
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

fn capture_backup_rows(
    snapshot: &Connection,
    kind: SectionKind,
    spool: &mut SectionSpoolBuilder,
) -> StoreResult<()> {
    match section_of_kind(kind) {
        Some(section) => {
            let sql = match section {
                Section::Hypa => "SELECT * FROM hypa_embeddings WHERE tombstone=0 ORDER BY cache_key".into(),
                Section::LocalPlugins => format!("SELECT {} FROM plugin_device_storage WHERE tombstone=0 ORDER BY owner,space,key", plugin_columns()),
            };
            let mut statement = snapshot.prepare(&sql)?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let row = row_from_sql(snapshot, section, row, Some(&spool.connection))?;
                spool.push_backup_row(row)?;
            }
        }
        None if kind == SectionKind::LocalSettings => {
            let mut statement = snapshot.prepare("SELECT key,value FROM device_settings ORDER BY key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                if setting_is_local(&key) {
                    spool.push_backup_row(SectionRow { key1: "setting".into(), key2: key, key3: String::new(),
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
                spool.push_backup_row(SectionRow { key1: "pluginPermission".into(), key2: row.get(0)?, key3: row.get(1)?,
                    value: SectionValueRow::PluginPermission { granted: row.get::<_, i64>(2)? == 1 },
                    write_clock: Sequence::from(0u64), writer_id: String::new() })?;
            }
        }
        None => return Err(invalid("Backup section kind is unsupported")),
    }
    Ok(())
}

pub(crate) fn capture_backup_sections_snapshot(
    snapshot: &Connection,
    kinds: &[SectionKind],
    scratch: &std::path::Path,
) -> StoreResult<Vec<PreparedSectionRows>> {
    let mut seen = BTreeSet::new();
    let mut spools = Vec::with_capacity(kinds.len());
    for kind in kinds {
        if !seen.insert(kind.id()) { return Err(invalid("Backup section kind is repeated")); }
        spools.push((*kind, SectionSpoolBuilder::new_backup(*kind, scratch)?));
    }
    for (kind, spool) in &mut spools { capture_backup_rows(snapshot, *kind, spool)?; }
    spools.into_iter().map(|(_, spool)| spool.finish_captured()).collect()
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
        scratch: &std::path::Path,
    ) -> StoreResult<Vec<PreparedSectionRows>> {
        if kinds.is_empty() { return Ok(Vec::new()); }
        let path: String = self.connection.query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0),
        )?;
        if path.is_empty() { return Err(invalid("Device database path is unavailable")); }
        let snapshot = crate::sqlite_open::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        snapshot.execute_batch(
            "PRAGMA busy_timeout=5000; PRAGMA query_only=ON; PRAGMA mmap_size=0; BEGIN;",
        )?;
        let sections = capture_backup_sections_snapshot(&snapshot, kinds, scratch)?;
        snapshot.execute_batch("COMMIT;")?;
        Ok(sections)
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
        if section==Section::Hypa&&!participating {return Err(invalid("Hypa participation is required"));}
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
                super::begin_mutation_remote(&transaction)?;
                super::super::lww::activate_plugin_local_units(&transaction)?;
                super::finish_mutation_remote(&transaction)?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every section value, including permanent removal markers.
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
            if text_value::encoded_length(value) > text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
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
        (Section::LocalPlugins, SectionValueRow::PluginBinary { object, .. }) => {
            if row.key2 != "json" { return Err(invalid("Binary plugin value belongs to another space")); }
            let raw = &plugin_values::raw_value::inspect_object(tx, &object.hash)?;
            if raw != object { return Err(invalid("Binary plugin identity differs")); }
            let (source, _) = text_value::object_row(tx, &object.hash)?;
            plugin_values::raw_value::write_reserved(tx, &row.key1, &row.key3, object, row.write_clock.as_str(), &row.writer_id, |target| {
                let mut input = tx.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", source, true)?;
                std::io::copy(&mut input, target)?; Ok(())
            })?;
            tx.execute("UPDATE plugin_device_storage SET published_clock=?4 WHERE owner=?1 AND space=?2 AND key=?3", params![row.key1,row.key2,row.key3,published_clock])?;
        }
        (Section::LocalPlugins, SectionValueRow::PluginObject { space, object, .. }) => {
            if *space != row.key2 { return Err(invalid("received plugin value names another space")); }
            let verified = text_value::inspect_object(tx, &object.hash)?;
            if verified != *object { return Err(invalid("received plugin value identity differs")); }
            let (source, _) = text_value::object_row(tx, &object.hash)?;
            plugin_values::write_reserved(tx, &row.key1, space, &row.key3, object.byte_length, row.write_clock.as_str(), &row.writer_id, |target| {
                let input = tx.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", source, true)?;
                if text_value::decode(input, target, text_value::MAX_VALUE_BYTES)? != *object {
                    return Err(invalid("received plugin value identity differs"));
                }
                Ok(())
            })?;
            tx.execute("UPDATE plugin_device_storage SET published_clock=?4 WHERE owner=?1 AND space=?2 AND key=?3",
                params![row.key1, row.key2, row.key3, published_clock])?;
        }
        _ => return Err(invalid("received section row belongs to another section")),
    }
    Ok(())
}
