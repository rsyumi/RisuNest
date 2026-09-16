//! Section capture and reception for file-based remotes. Device rows become
//! codec entries here and come back the same way, so the local tables can
//! change without changing what the repository holds.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use crate::persistent_store::device_store::{
    sections::{SectionCursor, SectionKey, SectionRow, SectionValueRow, TombstonePublication},
    Section,
};
use risunest_external_storage_format::{
    content_identity::hash,
    format::fingerprint,
    section::{
        decode_local_plugin_entry_key, hypa_entry_key, local_plugin_entry_key, HypaValue,
        InlineOrObject, LocalPluginValue, LocalSettingValue, ObjectReference, PluginSpace,
        SectionEntry, SectionEntryVersion, SectionKind, SectionValue, MAX_INLINE_VALUE_BYTES,
    },
    snapshot as wire,
};
use risunest_sync_wire::head::Sequence;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

/// One file the packager will carry. Section bytes live in the job spool
/// because a section changes without the library revision changing.
#[derive(Clone, Debug)]
pub(crate) struct SectionSource {
    pub kind: wire::CatalogEntryKind,
    pub key: String,
    pub content_sha256: String,
    pub byte_length: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct CapturedSection {
    pub kind: SectionKind,
    pub generation: Sequence,
    pub gc_floor: Sequence,
    pub max_write_clock: Sequence,
    pub content_fingerprint: [u8; 32],
    pub sources: Vec<SectionSource>,
}

/// What a confirmed publication has to record locally for one section. Nothing
/// here is written before the remote holds the captured content, so a failed
/// publication leaves the device file as it was.
#[derive(Clone, Debug)]
pub(crate) struct SectionPublication {
    pub section: Section,
    pub published: Vec<(SectionKey, Sequence)>,
    /// Removals this capture is publishing for the first time, with the marker
    /// the entries carry.
    pub stamped: Vec<(SectionKey, Sequence)>,
    pub first_published: TombstonePublication,
}

/// Stamps the removals this capture is publishing for the first time. A marker
/// a removal already carries is what every other device has, so it is kept.
fn stamp_removals(
    rows: Vec<SectionRow>,
    marker: &TombstonePublication,
) -> (Vec<SectionRow>, Vec<(SectionKey, Sequence)>) {
    let mut stamped = Vec::new();
    let rows = rows
        .into_iter()
        .map(|row| match &row.value {
            SectionValueRow::Tombstone { first_published: None } => {
                stamped.push((row.key(), row.write_clock.clone()));
                SectionRow {
                    value: SectionValueRow::Tombstone {
                        first_published: Some(marker.clone()),
                    },
                    ..row
                }
            }
            _ => row,
        })
        .collect();
    (rows, stamped)
}

pub(crate) fn section_of(kind: SectionKind) -> Option<Section> {
    match kind {
        SectionKind::Hypa => Some(Section::Hypa),
        SectionKind::LocalPlugins => Some(Section::LocalPlugins),
        SectionKind::LocalSettings => None,
    }
}

fn setting_entry_key(row: &SectionRow) -> Result<String> {
    serde_json::to_string(&[
        row.key1.as_str(),
        row.key2.as_str(),
        row.key3.as_str(),
    ])
    .map_err(corrupt)
}

fn decode_setting_entry_key(encoded: &str) -> Result<(String, String, String)> {
    let parts: Vec<String> = serde_json::from_str(encoded).map_err(corrupt)?;
    let [key1, key2, key3]: [String; 3] = parts
        .try_into()
        .map_err(|_| corrupt("device setting key is not a triple"))?;
    Ok((key1, key2, key3))
}

fn entry_key(kind: SectionKind, row: &SectionRow) -> Result<String> {
    match kind {
        SectionKind::Hypa => hypa_entry_key(&row.key1).map_err(corrupt),
        SectionKind::LocalPlugins => {
            local_plugin_entry_key(&row.key1, &row.key2, &row.key3).map_err(corrupt)
        }
        SectionKind::LocalSettings => setting_entry_key(row),
    }
}

fn write_spool_object(spool: &Path, bytes: &[u8]) -> Result<(String, PathBuf)> {
    let digest = hex::encode(hash(bytes));
    let path = spool.join(&digest);
    if path.exists() {
        let metadata = fs::symlink_metadata(&path).map_err(transient)?;
        if metadata.is_file()
            && !crate::trust_boundary::is_link_like(&metadata)
            && metadata.len() == bytes.len() as u64
        {
            return Ok((digest, path));
        }
        fs::remove_file(&path).map_err(transient)?;
    }
    let staging = spool.join(format!(".section-{}.partial", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .map_err(transient)?;
    file.write_all(bytes).map_err(transient)?;
    file.sync_all().map_err(transient)?;
    drop(file);
    fs::rename(&staging, &path).map_err(transient)?;
    crate::trust_boundary::sync_directory(spool).map_err(transient)?;
    Ok((digest, path))
}

fn plugin_value(space: &str, stored: &str) -> Result<LocalPluginValue> {
    match space {
        "string" => Ok(LocalPluginValue {
            space: PluginSpace::String,
            value: serde_json::Value::String(stored.into()),
        }),
        "json" => Ok(LocalPluginValue {
            space: PluginSpace::Json,
            value: serde_json::from_str(stored).map_err(corrupt)?,
        }),
        _ => Err(corrupt("plugin value names an unknown space")),
    }
}

fn section_value(
    kind: SectionKind,
    row: &SectionRow,
    spool: &Path,
    sources: &mut Vec<SectionSource>,
) -> Result<SectionValue> {
    match (&row.value, kind) {
        (SectionValueRow::Tombstone { first_published }, _) => {
            let marker = first_published
                .as_ref()
                .ok_or_else(|| corrupt("removal has no first publication marker"))?;
            Ok(SectionValue::tombstone(
                marker.generation.clone(),
                marker.at_ms,
            ))
        }
        (
            SectionValueRow::Hypa {
                producer,
                model,
                endpoint,
                preprocess_version,
                dimensions,
                vector,
                metadata,
            },
            SectionKind::Hypa,
        ) => {
            let carried = if vector.len() > MAX_INLINE_VALUE_BYTES {
                let (digest, path) = write_spool_object(spool, vector)?;
                sources.push(SectionSource {
                    kind: wire::CatalogEntryKind::SectionObject,
                    key: format!("object/{digest}"),
                    content_sha256: digest.clone(),
                    byte_length: vector.len() as u64,
                    path,
                });
                InlineOrObject::Object(ObjectReference {
                    content_sha256: hash(vector),
                    byte_length: vector.len() as u64,
                })
            } else {
                InlineOrObject::inline(vector).map_err(corrupt)?
            };
            Ok(SectionValue::Hypa(HypaValue {
                producer: producer.clone(),
                model: model.clone(),
                endpoint: endpoint.clone(),
                preprocess_version: u32::try_from(*preprocess_version).map_err(corrupt)?,
                dimensions: u32::try_from(*dimensions).map_err(corrupt)?,
                vector: carried,
                metadata: metadata
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(corrupt)?,
            }))
        }
        (SectionValueRow::Plugin { space, value }, SectionKind::LocalPlugins) => Ok(
            SectionValue::LocalPlugin(plugin_value(space, value)?),
        ),
        (SectionValueRow::Setting { value }, SectionKind::LocalSettings) => {
            Ok(SectionValue::LocalSetting(LocalSettingValue {
                value: serde_json::from_str(value).map_err(corrupt)?,
            }))
        }
        (SectionValueRow::PluginPermission { granted }, SectionKind::LocalSettings) => {
            Ok(SectionValue::LocalSetting(LocalSettingValue {
                value: serde_json::Value::Bool(*granted),
            }))
        }
        _ => Err(corrupt("device row belongs to another section")),
    }
}

/// Encodes one section into spool files. `versioned` is false for a backup
/// bundle, which carries user values without the counters behind them.
pub(crate) fn capture_section(
    kind: SectionKind,
    rows: &[SectionRow],
    versioned: bool,
    generation: Sequence,
    gc_floor: Sequence,
    max_write_clock: Sequence,
    spool: &Path,
    cancel: &Cancellation,
) -> Result<CapturedSection> {
    cancel.check()?;
    fs::create_dir_all(spool).map_err(transient)?;
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(spool).map_err(transient)?) {
        return Err(corrupt("section spool is a link"));
    }
    let mut sources = Vec::new();
    let mut fingerprints: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    for row in rows {
        cancel.check()?;
        let key = entry_key(kind, row)?;
        let value = section_value(kind, row, spool, &mut sources)?;
        let version = versioned.then(|| SectionEntryVersion {
            write_clock: row.write_clock.clone(),
            writer_id: row.writer_id.clone(),
        });
        let entry = SectionEntry::new(kind, key.clone(), value, version).map_err(corrupt)?;
        let bytes = entry.encode().map_err(corrupt)?;
        let (digest, path) = write_spool_object(spool, &bytes)?;
        if fingerprints.insert(key.clone(), hash(&bytes)).is_some() {
            return Err(corrupt("section key appears twice"));
        }
        sources.push(SectionSource {
            kind: wire::CatalogEntryKind::SectionEntry,
            key,
            content_sha256: digest,
            byte_length: bytes.len() as u64,
            path,
        });
    }
    sources.sort_by(|a, b| (a.kind as u8, &a.key).cmp(&(b.kind as u8, &b.key)));
    sources.dedup_by(|a, b| a.kind == b.kind && a.key == b.key);
    Ok(CapturedSection {
        kind,
        generation,
        gc_floor,
        max_write_clock,
        content_fingerprint: fingerprint(&kind.fingerprint_domain(), &fingerprints),
        sources,
    })
}

fn device_error(_: crate::persistent_store::StoreError) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

/// What a backup connection keeps beside the library. Selected sections are
/// always present, an emptied one as a reference with no entries.
pub(crate) fn capture_backup_sections(
    store: &mut crate::persistent_store::PersistentStore,
    policy: super::connection::CapturePolicy,
    spool: &Path,
    cancel: &Cancellation,
) -> Result<Vec<CapturedSection>> {
    let device = store.device_store_mut().map_err(device_error)?;
    let mut captured = Vec::new();
    for (kind, selected) in [
        (SectionKind::Hypa, policy.hypa),
        (SectionKind::LocalPlugins, policy.local_plugins),
        (SectionKind::LocalSettings, policy.local_settings),
    ] {
        if !selected {
            continue;
        }
        let rows = match section_of(kind) {
            Some(section) => device.read_backup_section_rows(section).map_err(device_error)?,
            None => device.read_local_setting_rows().map_err(device_error)?,
        };
        captured.push(capture_section(
            kind,
            &rows,
            false,
            Sequence::from(0u64),
            Sequence::from(0u64),
            Sequence::from(0u64),
            &spool.join(kind.id()),
            cancel,
        )?);
    }
    Ok(captured)
}

/// What a synchronization connection publishes. Only a participating section
/// is captured; the rest keep whatever reference the observed state carried.
pub(crate) fn capture_state_sections(
    store: &mut crate::persistent_store::PersistentStore,
    generation: &Sequence,
    spool: &Path,
    cancel: &Cancellation,
) -> Result<(Vec<CapturedSection>, Vec<SectionPublication>)> {
    let at_ms = u64::try_from(
        crate::persistent_store::device_store::now_ms().map_err(device_error)?,
    )
    .map_err(corrupt)?;
    let device = store.device_store_mut().map_err(device_error)?;
    let mut captured = Vec::new();
    let mut publications = Vec::new();
    for kind in [SectionKind::Hypa, SectionKind::LocalPlugins] {
        let section = section_of(kind).expect("synchronizable section");
        let state = device.section_state(section).map_err(device_error)?;
        if !state.participating {
            continue;
        }
        let marker = TombstonePublication {
            generation: generation.clone(),
            at_ms,
        };
        let (rows, stamped) = stamp_removals(
            device.read_section_rows(section).map_err(device_error)?,
            &marker,
        );
        captured.push(capture_section(
            kind,
            &rows,
            true,
            generation.clone(),
            state.gc_floor.clone(),
            state.max_write_clock.clone(),
            &spool.join(kind.id()),
            cancel,
        )?);
        publications.push(SectionPublication {
            section,
            published: rows
                .iter()
                .map(|row| (row.key(), row.write_clock.clone()))
                .collect(),
            stamped,
            first_published: marker,
        });
    }
    Ok((captured, publications))
}

/// The sections this device takes part in that this remote lineage holds no
/// cursor for. Taking one back on has to bring the remote content in before a
/// publication puts local rows in its place.
pub(crate) fn rejoining_sections(
    store: &mut crate::persistent_store::PersistentStore,
    connection_id: &str,
    library_lineage: &str,
) -> Result<BTreeSet<String>> {
    let device = store.device_store_mut().map_err(device_error)?;
    let mut wanted = BTreeSet::new();
    for kind in [SectionKind::Hypa, SectionKind::LocalPlugins] {
        let section = section_of(kind).expect("synchronizable section");
        if !device.section_state(section).map_err(device_error)?.participating {
            continue;
        }
        if device
            .read_section_cursor(connection_id, library_lineage, section)
            .map_err(device_error)?
            .is_none()
        {
            wanted.insert(kind.id().to_owned());
        }
    }
    Ok(wanted)
}

/// Whether this remote lineage has carried the section to this device before.
/// A rejoined one records this device's own values as its newest writes first,
/// so the merge keeps them wherever both sides hold the same key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionArrival {
    Continuing,
    Rejoining,
}

/// Merges one received section into the device file and records how far this
/// remote lineage has been applied. The cursor is written last, so an
/// interrupted apply runs again from the same remote state instead of
/// reporting the section as done.
pub(crate) fn apply_received_section(
    store: &mut crate::persistent_store::PersistentStore,
    connection_id: &str,
    library_lineage: &str,
    arrival: SectionArrival,
    prepared: &super::snapshot_restore::PreparedSection,
) -> Result<()> {
    let Some(section) = section_of(prepared.kind) else {
        return Err(corrupt("device-fixed section in a synchronized state"));
    };
    let rows = decode_section(
        prepared.kind,
        &prepared.entries,
        &prepared.content_fingerprint,
    )?;
    let device = store.device_store_mut().map_err(device_error)?;
    if !device.section_state(section).map_err(device_error)?.participating {
        return Ok(());
    }
    let cursor = device
        .read_section_cursor(connection_id, library_lineage, section)
        .map_err(device_error)?;
    // A floor above what this device has applied means the removals between
    // them were reclaimed before this device ever saw them, so the section
    // cannot be carried forward as an increment.
    let arrival = match &cursor {
        Some(cursor) if prepared.gc_floor > cursor.applied_generation => SectionArrival::Rejoining,
        _ => arrival,
    };
    // Markers name commits of one lineage only, so a section from a lineage
    // this device holds no cursor for decides nothing about them.
    let reclaim_floor = cursor
        .map(|_| prepared.gc_floor.clone())
        .unwrap_or_else(|| Sequence::from(0u64));
    if arrival == SectionArrival::Rejoining {
        // Reissuing rewrites this device's rows as new writes, so a removal the
        // remote reclaimed has to go before it can come back with a new version.
        device
            .reclaim_section_tombstones(section, &reclaim_floor, &rows)
            .map_err(device_error)?;
        device
            .reissue_section_rows(section, &prepared.max_write_clock, &rows)
            .map_err(device_error)?;
    }
    device.apply_section_rows(section, &rows).map_err(device_error)?;
    if arrival == SectionArrival::Continuing {
        device
            .reclaim_section_tombstones(section, &reclaim_floor, &rows)
            .map_err(device_error)?;
    }
    device
        .write_section_cursor(
            connection_id,
            library_lineage,
            section,
            &SectionCursor {
                applied_generation: prepared.generation.clone(),
                applied_gc_floor: prepared.gc_floor.clone(),
                observed_max_write_clock: prepared.max_write_clock.clone(),
            },
        )
        .map_err(device_error)
}

/// A decoded section as it arrived. Object bodies are resolved by content hash
/// before a row is produced, so a missing object fails the whole section.
pub(crate) fn decode_section(
    kind: SectionKind,
    entries: &[(wire::CatalogEntryKind, String, Vec<u8>)],
    expected_fingerprint: &[u8; 32],
) -> Result<Vec<SectionRow>> {
    let mut objects: BTreeMap<[u8; 32], &Vec<u8>> = BTreeMap::new();
    for (entry_kind, _, bytes) in entries {
        if *entry_kind == wire::CatalogEntryKind::SectionObject {
            objects.insert(hash(bytes), bytes);
        }
    }
    let mut fingerprints: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    let mut rows = Vec::new();
    for (entry_kind, key, bytes) in entries {
        if *entry_kind != wire::CatalogEntryKind::SectionEntry {
            continue;
        }
        let entry = SectionEntry::decode(bytes).map_err(corrupt)?;
        if entry.kind != kind || entry.key != *key {
            return Err(corrupt("section entry names another section"));
        }
        if fingerprints.insert(key.clone(), hash(bytes)).is_some() {
            return Err(corrupt("section key appears twice"));
        }
        rows.push(row_of(kind, &entry, &objects)?);
    }
    if fingerprint(&kind.fingerprint_domain(), &fingerprints) != *expected_fingerprint {
        return Err(corrupt("section content differs from its reference"));
    }
    Ok(rows)
}

fn row_of(
    kind: SectionKind,
    entry: &SectionEntry,
    objects: &BTreeMap<[u8; 32], &Vec<u8>>,
) -> Result<SectionRow> {
    let (key1, key2, key3) = match kind {
        SectionKind::Hypa => (entry.key.clone(), String::new(), String::new()),
        SectionKind::LocalPlugins => decode_local_plugin_entry_key(&entry.key).map_err(corrupt)?,
        SectionKind::LocalSettings => decode_setting_entry_key(&entry.key)?,
    };
    let value = match &entry.value {
        SectionValue::Tombstone {
            first_published_generation,
            first_published_at_ms,
        } => SectionValueRow::Tombstone {
            first_published: Some(TombstonePublication {
                generation: first_published_generation.clone(),
                at_ms: *first_published_at_ms,
            }),
        },
        SectionValue::Hypa(value) => {
            let vector = match &value.vector {
                InlineOrObject::Inline(_) => value.vector.decode_inline().map_err(corrupt)?,
                InlineOrObject::Object(reference) => {
                    let bytes = objects
                        .get(&reference.content_sha256)
                        .ok_or_else(|| corrupt("section object is missing"))?;
                    if bytes.len() as u64 != reference.byte_length {
                        return Err(corrupt("section object length differs"));
                    }
                    (*bytes).clone()
                }
            };
            SectionValueRow::Hypa {
                producer: value.producer.clone(),
                model: value.model.clone(),
                endpoint: value.endpoint.clone(),
                preprocess_version: i64::from(value.preprocess_version),
                dimensions: i64::from(value.dimensions),
                vector,
                metadata: value
                    .metadata
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(corrupt)?,
            }
        }
        SectionValue::LocalPlugin(value) => SectionValueRow::Plugin {
            space: match value.space {
                PluginSpace::String => "string".into(),
                PluginSpace::Json => "json".into(),
            },
            value: match (&value.space, &value.value) {
                (PluginSpace::String, serde_json::Value::String(text)) => text.clone(),
                (PluginSpace::String, _) => return Err(corrupt("plugin string value is not text")),
                (PluginSpace::Json, value) => serde_json::to_string(value).map_err(corrupt)?,
            },
        },
        SectionValue::LocalSetting(value) => match key1.as_str() {
            "pluginPermission" => SectionValueRow::PluginPermission {
                granted: value
                    .value
                    .as_bool()
                    .ok_or_else(|| corrupt("plugin permission is not a decision"))?,
            },
            _ => SectionValueRow::Setting {
                value: serde_json::to_string(&value.value).map_err(corrupt)?,
            },
        },
    };
    let (write_clock, writer_id) = match &entry.version {
        Some(version) => (version.write_clock.clone(), version.writer_id.clone()),
        None => (Sequence::from(0u64), String::new()),
    };
    Ok(SectionRow {
        key1,
        key2,
        key3,
        value,
        write_clock,
        writer_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::{
        device_store::plugin_values::PluginDeviceMutation, PersistentStore,
    };

    fn plugin_row(key: &str, value: &str, clock: u64, writer: &str) -> SectionRow {
        SectionRow {
            key1: "plugin-a".into(),
            key2: "string".into(),
            key3: key.into(),
            value: SectionValueRow::Plugin {
                space: "string".into(),
                value: value.into(),
            },
            write_clock: Sequence::from(clock),
            writer_id: writer.into(),
        }
    }

    fn plugin_tombstone(
        key: &str,
        clock: u64,
        writer: &str,
        marker: Option<(u64, u64)>,
    ) -> SectionRow {
        SectionRow {
            value: SectionValueRow::Tombstone {
                first_published: marker.map(|(generation, at_ms)| TombstonePublication {
                    generation: Sequence::from(generation),
                    at_ms,
                }),
            },
            ..plugin_row(key, "", clock, writer)
        }
    }

    /// Every plugin key the device file still holds, with the removals marked.
    fn held_plugin_keys(store: &mut PersistentStore) -> Vec<(String, bool)> {
        store
            .device_store_mut()
            .expect("open device store")
            .read_section_rows(Section::LocalPlugins)
            .expect("read section rows")
            .into_iter()
            .map(|row| (row.key3, row.value.is_tombstone()))
            .collect()
    }

    fn remote_section(
        rows: &[SectionRow],
        generation: u64,
        gc_floor: u64,
        max_write_clock: u64,
        spool: &Path,
    ) -> CapturedSection {
        capture_section(
            SectionKind::LocalPlugins,
            rows,
            true,
            Sequence::from(generation),
            Sequence::from(gc_floor),
            Sequence::from(max_write_clock),
            spool,
            &Cancellation::default(),
        )
        .expect("capture a remote plugin section")
    }

    fn joined(store: &mut PersistentStore, generation: u64, observed: u64) {
        store
            .device_store_mut()
            .expect("open device store")
            .write_section_cursor(
                "connection",
                "library",
                Section::LocalPlugins,
                &SectionCursor {
                    applied_generation: Sequence::from(generation),
                    applied_gc_floor: Sequence::from(0u64),
                    observed_max_write_clock: Sequence::from(observed),
                },
            )
            .expect("record what this lineage carried");
    }

    fn participating_plugin_store(root: &Path) -> PersistentStore {
        let mut store = PersistentStore::open(root).expect("open persistent store");
        let device = store.device_store_mut().expect("open device store");
        device
            .set_section_participating(Section::LocalPlugins, true)
            .expect("take part in the plugin section");
        device
            .set_section_participating(Section::Hypa, false)
            .expect("leave the embedding section out");
        store
    }

    fn hypa_row(key: &str, clock: u64, writer: &str, dimensions: i64) -> SectionRow {
        SectionRow {
            key1: key.into(),
            key2: String::new(),
            key3: String::new(),
            value: SectionValueRow::Hypa {
                producer: "hypa-v2".into(),
                model: "text-embedding".into(),
                endpoint: None,
                preprocess_version: 1,
                dimensions,
                vector: vec![7u8; dimensions as usize * 4],
                metadata: None,
            },
            write_clock: Sequence::from(clock),
            writer_id: writer.into(),
        }
    }

    fn carried(section: &CapturedSection) -> Vec<(wire::CatalogEntryKind, String, Vec<u8>)> {
        section
            .sources
            .iter()
            .map(|source| {
                (
                    source.kind,
                    source.key.clone(),
                    fs::read(&source.path).expect("read section source"),
                )
            })
            .collect()
    }

    #[test]
    fn a_large_vector_becomes_an_object_and_returns_byte_identical() {
        let spool = tempfile::tempdir().expect("create spool");
        let rows = [
            hypa_row(&"a".repeat(64), 3, "writer-a", 4),
            hypa_row(&"b".repeat(64), 4, "writer-a", 2_048),
        ];
        let captured = capture_section(
            SectionKind::Hypa,
            &rows,
            true,
            Sequence::from(1u64),
            Sequence::from(0u64),
            Sequence::from(4u64),
            spool.path(),
            &Cancellation::default(),
        )
        .expect("capture hypa section");
        assert!(captured
            .sources
            .iter()
            .any(|source| source.kind == wire::CatalogEntryKind::SectionObject));
        let decoded = decode_section(
            SectionKind::Hypa,
            &carried(&captured),
            &captured.content_fingerprint,
        )
        .expect("decode hypa section");
        assert_eq!(decoded, rows);
    }

    #[test]
    fn plugin_spaces_and_device_settings_round_trip_without_being_rewritten() {
        let spool = tempfile::tempdir().expect("create spool");
        let plugin_rows = [
            SectionRow {
                key1: "provider-manager".into(),
                key2: "json".into(),
                key3: "settings".into(),
                value: SectionValueRow::Plugin {
                    space: "json".into(),
                    value: "{\"zeta\":1,\"alpha\":[2,3]}".into(),
                },
                write_clock: Sequence::from(9u64),
                writer_id: "writer-a".into(),
            },
            SectionRow {
                key1: "yumi-translator".into(),
                key2: "string".into(),
                key3: "token".into(),
                value: SectionValueRow::Plugin {
                    space: "string".into(),
                    value: "kept".into(),
                },
                write_clock: Sequence::from(10u64),
                writer_id: "writer-b".into(),
            },
        ];
        let plugins = capture_section(
            SectionKind::LocalPlugins,
            &plugin_rows,
            true,
            Sequence::from(2u64),
            Sequence::from(0u64),
            Sequence::from(10u64),
            spool.path(),
            &Cancellation::default(),
        )
        .expect("capture plugin section");
        let decoded = decode_section(
            SectionKind::LocalPlugins,
            &carried(&plugins),
            &plugins.content_fingerprint,
        )
        .expect("decode plugin section");
        assert_eq!(decoded, plugin_rows);

        // Entry order follows the encoded key, so the permission sorts first.
        let setting_rows = [
            SectionRow {
                key1: "pluginPermission".into(),
                key2: "c".repeat(64),
                key3: "network".into(),
                value: SectionValueRow::PluginPermission { granted: true },
                write_clock: Sequence::from(0u64),
                writer_id: String::new(),
            },
            SectionRow {
                key1: "setting".into(),
                key2: "risuNestDeviceSettings".into(),
                key3: String::new(),
                value: SectionValueRow::Setting {
                    value: "{\"startup\":\"restore\"}".into(),
                },
                write_clock: Sequence::from(0u64),
                writer_id: String::new(),
            },
        ];
        let settings = capture_section(
            SectionKind::LocalSettings,
            &setting_rows,
            false,
            Sequence::from(0u64),
            Sequence::from(0u64),
            Sequence::from(0u64),
            spool.path(),
            &Cancellation::default(),
        )
        .expect("capture settings section");
        let decoded = decode_section(
            SectionKind::LocalSettings,
            &carried(&settings),
            &settings.content_fingerprint,
        )
        .expect("decode settings section");
        assert_eq!(decoded, setting_rows);
    }

    #[test]
    fn a_selected_but_empty_section_still_has_its_own_fingerprint() {
        let spool = tempfile::tempdir().expect("create spool");
        let empty = |kind| {
            capture_section(
                kind,
                &[],
                true,
                Sequence::from(1u64),
                Sequence::from(0u64),
                Sequence::from(0u64),
                spool.path(),
                &Cancellation::default(),
            )
            .expect("capture empty section")
        };
        let hypa = empty(SectionKind::Hypa);
        let plugins = empty(SectionKind::LocalPlugins);
        assert!(hypa.sources.is_empty());
        assert_ne!(hypa.content_fingerprint, plugins.content_fingerprint);
        assert!(decode_section(SectionKind::Hypa, &[], &hypa.content_fingerprint).is_ok());
        assert!(decode_section(SectionKind::Hypa, &[], &plugins.content_fingerprint).is_err());
    }

    fn prepared(section: &CapturedSection) -> super::super::snapshot_restore::PreparedSection {
        super::super::snapshot_restore::PreparedSection {
            kind: section.kind,
            generation: section.generation.clone(),
            gc_floor: section.gc_floor.clone(),
            max_write_clock: section.max_write_clock.clone(),
            content_fingerprint: section.content_fingerprint,
            entries: carried(section),
        }
    }

    fn published_plugin_values(section: &CapturedSection) -> Vec<(String, Option<String>)> {
        decode_section(
            SectionKind::LocalPlugins,
            &carried(section),
            &section.content_fingerprint,
        )
        .expect("decode the published section")
        .into_iter()
        .map(|row| {
            (
                row.key3,
                match row.value {
                    SectionValueRow::Plugin { value, .. } => Some(value),
                    _ => None,
                },
            )
        })
        .collect()
    }

    /// A device that takes a section back on merges the remote rows before it
    /// publishes. The keys only the remote holds join the published section and
    /// the keys both sides hold keep the local value, so no other device's
    /// value is dropped by the device that rejoined.
    #[test]
    fn rejoining_a_section_publishes_both_devices_keys_and_keeps_the_local_value() {
        let spool = tempfile::tempdir().expect("create spool");
        let root = tempfile::tempdir().expect("create store root");
        let mut store = PersistentStore::open(root.path()).expect("open persistent store");
        {
            let device = store.device_store_mut().expect("open device store");
            device
                .set_section_participating(Section::LocalPlugins, true)
                .expect("take part in the plugin section");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[
                        PluginDeviceMutation::Set {
                            space: "string".into(),
                            key: "shared".into(),
                            value: "from-b".into(),
                        },
                        PluginDeviceMutation::Set {
                            space: "string".into(),
                            key: "only-b".into(),
                            value: "from-b".into(),
                        },
                    ],
                )
                .expect("write local plugin values");
        }
        let remote = capture_section(
            SectionKind::LocalPlugins,
            &[
                plugin_row("only-a", "from-a", 40, "writer-a"),
                plugin_row("shared", "from-a", 41, "writer-a"),
            ],
            true,
            Sequence::from(3u64),
            Sequence::from(0u64),
            Sequence::from(41u64),
            &spool.path().join("remote"),
            &Cancellation::default(),
        )
        .expect("capture the remote section");

        assert_eq!(
            rejoining_sections(&mut store, "connection", "library").expect("read rejoining"),
            BTreeSet::from(["hypa".to_owned(), "local-plugins".to_owned()])
        );
        apply_received_section(
            &mut store,
            "connection",
            "library",
            SectionArrival::Rejoining,
            &prepared(&remote),
        )
        .expect("rejoin the plugin section");

        let (published, _) = capture_state_sections(
            &mut store,
            &Sequence::from(4u64),
            &spool.path().join("published"),
            &Cancellation::default(),
        )
        .expect("capture the state sections");
        let plugins = published
            .iter()
            .find(|section| section.kind == SectionKind::LocalPlugins)
            .expect("published plugin section");
        assert_eq!(
            published_plugin_values(plugins),
            vec![
                ("only-a".to_owned(), Some("from-a".to_owned())),
                ("only-b".to_owned(), Some("from-b".to_owned())),
                ("shared".to_owned(), Some("from-b".to_owned())),
            ]
        );

        // The merged section still owes the remote a publication, and the
        // section the remote never carried keeps no cursor, so it is rejoined
        // whenever that remote does carry it.
        let device = store.device_store_mut().expect("open device store");
        assert!(device
            .sections_await_publication("connection", "library")
            .expect("read awaiting publication"));
        let settled = device
            .section_state(Section::LocalPlugins)
            .expect("read section state")
            .max_write_clock;
        assert_eq!(
            rejoining_sections(&mut store, "connection", "library").expect("read rejoining"),
            BTreeSet::from(["hypa".to_owned()])
        );

        // An automatic retry is not a fresh reactivation, so it stamps no
        // second version and publishes the same section again.
        apply_received_section(
            &mut store,
            "connection",
            "library",
            SectionArrival::Rejoining,
            &prepared(&remote),
        )
        .expect("rejoin the plugin section again");
        assert_eq!(
            store
                .device_store_mut()
                .expect("open device store")
                .section_state(Section::LocalPlugins)
                .expect("read section state")
                .max_write_clock,
            settled
        );
        let (republished, _) = capture_state_sections(
            &mut store,
            &Sequence::from(5u64),
            &spool.path().join("republished"),
            &Cancellation::default(),
        )
        .expect("capture the state sections again");
        assert_eq!(
            published_plugin_values(
                republished
                    .iter()
                    .find(|section| section.kind == SectionKind::LocalPlugins)
                    .expect("published plugin section")
            ),
            published_plugin_values(plugins)
        );
    }

    /// A confirmed publication records the versions it carried, so the next
    /// cycle stops offering the same section. A write that landed after the
    /// capture is outside that record and still owes a publication.
    #[test]
    fn a_recorded_publication_does_not_make_every_cycle_capture_again() {
        let spool = tempfile::tempdir().expect("create spool");
        let root = tempfile::tempdir().expect("create store root");
        let mut store = PersistentStore::open(root.path()).expect("open persistent store");
        {
            let device = store.device_store_mut().expect("open device store");
            device
                .set_section_participating(Section::LocalPlugins, true)
                .expect("take part in the plugin section");
            device
                .set_section_participating(Section::Hypa, false)
                .expect("leave the embedding section out");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "mine".into(),
                        value: "local".into(),
                    }],
                )
                .expect("write a local plugin value");
            device
                .write_section_cursor(
                    "connection",
                    "library",
                    Section::LocalPlugins,
                    &SectionCursor {
                        applied_generation: Sequence::from(3u64),
                        applied_gc_floor: Sequence::from(0u64),
                        observed_max_write_clock: Sequence::from(1u64),
                    },
                )
                .expect("record what this lineage carried");
        }
        let (_, publications) = capture_state_sections(
            &mut store,
            &Sequence::from(4u64),
            &spool.path().join("published"),
            &Cancellation::default(),
        )
        .expect("capture the state sections");
        assert_eq!(publications.len(), 1);

        let device = store.device_store_mut().expect("open device store");
        assert!(device
            .sections_await_publication("connection", "library")
            .expect("read awaiting publication"));
        for publication in &publications {
            device
                .note_section_published(
                    publication.section,
                    &publication.published,
                    &publication.stamped,
                    &publication.first_published,
                )
                .expect("record the confirmed publication");
        }
        assert!(!device
            .sections_await_publication("connection", "library")
            .expect("read awaiting publication"));

        device
            .write_plugin_device_values(
                "plugin-a",
                &[PluginDeviceMutation::Set {
                    space: "string".into(),
                    key: "mine".into(),
                    value: "changed".into(),
                }],
            )
            .expect("write over the published value");
        assert!(device
            .sections_await_publication("connection", "library")
            .expect("read awaiting publication"));
    }

    fn removal_marker(store: &mut PersistentStore, key: &str) -> Option<TombstonePublication> {
        store
            .device_store_mut()
            .expect("open device store")
            .read_section_rows(Section::LocalPlugins)
            .expect("read section rows")
            .into_iter()
            .find(|row| row.key3 == key)
            .and_then(|row| match row.value {
                SectionValueRow::Tombstone { first_published } => first_published,
                _ => None,
            })
    }

    /// A removal this device has not published yet takes the commit number and
    /// the time of the publication that carries it, and the device file records
    /// the same marker once that publication is confirmed. A removal that
    /// already carries one keeps it, so every device judges its age alike.
    #[test]
    fn a_removal_takes_the_marker_of_the_publication_that_carries_it() {
        let spool = tempfile::tempdir().expect("create spool");
        let root = tempfile::tempdir().expect("create store root");
        let mut store = PersistentStore::open(root.path()).expect("open persistent store");
        {
            let device = store.device_store_mut().expect("open device store");
            device
                .set_section_participating(Section::LocalPlugins, true)
                .expect("take part in the plugin section");
            device
                .set_section_participating(Section::Hypa, false)
                .expect("leave the embedding section out");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "gone".into(),
                        value: "value".into(),
                    }],
                )
                .expect("write a local plugin value");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Delete {
                        space: "string".into(),
                        key: "gone".into(),
                    }],
                )
                .expect("remove the local plugin value");
        }
        assert!(removal_marker(&mut store, "gone").is_none());

        let (captured, publications) = capture_state_sections(
            &mut store,
            &Sequence::from(4u64),
            &spool.path().join("published"),
            &Cancellation::default(),
        )
        .expect("capture the state sections");
        let plugins = captured
            .iter()
            .find(|section| section.kind == SectionKind::LocalPlugins)
            .expect("published plugin section");
        let decoded = decode_section(
            SectionKind::LocalPlugins,
            &carried(plugins),
            &plugins.content_fingerprint,
        )
        .expect("decode the published section");
        let SectionValueRow::Tombstone { first_published } = &decoded[0].value else {
            panic!("the removal did not travel as one");
        };
        let carried_marker = first_published.clone().expect("a published marker");
        assert_eq!(carried_marker.generation, Sequence::from(4u64));
        assert!(carried_marker.at_ms > 0);
        // Nothing is recorded before the remote holds the capture.
        assert!(removal_marker(&mut store, "gone").is_none());

        for publication in &publications {
            store
                .device_store_mut()
                .expect("open device store")
                .note_section_published(
                    publication.section,
                    &publication.published,
                    &publication.stamped,
                    &publication.first_published,
                )
                .expect("record the confirmed publication");
        }
        assert_eq!(removal_marker(&mut store, "gone"), Some(carried_marker));

        let (republished, _) = capture_state_sections(
            &mut store,
            &Sequence::from(5u64),
            &spool.path().join("republished"),
            &Cancellation::default(),
        )
        .expect("capture the state sections again");
        let plugins = republished
            .iter()
            .find(|section| section.kind == SectionKind::LocalPlugins)
            .expect("published plugin section");
        let decoded = decode_section(
            SectionKind::LocalPlugins,
            &carried(plugins),
            &plugins.content_fingerprint,
        )
        .expect("decode the republished section");
        assert_eq!(decoded[0].value, removal_marker_row(&mut store, "gone"));
    }

    fn removal_marker_row(store: &mut PersistentStore, key: &str) -> SectionValueRow {
        SectionValueRow::Tombstone {
            first_published: removal_marker(store, key),
        }
    }

    /// A removal the remote reclaimed goes, and one it still carries stays even
    /// when the floor stands above it: a floor is the boundary for rejoining,
    /// not a verdict on every removal below it. A removal this device has not
    /// published is its own new one and is never judged by a remote's floor,
    /// and a section from a lineage this device never exchanged with decides
    /// nothing, because commit numbers mean nothing across lineages.
    #[test]
    fn only_the_removals_a_remote_reclaimed_leave_this_device() {
        let spool = tempfile::tempdir().expect("create spool");
        let root = tempfile::tempdir().expect("create store root");
        let mut store = participating_plugin_store(root.path());
        {
            let device = store.device_store_mut().expect("open device store");
            device
                .apply_section_rows(
                    Section::LocalPlugins,
                    &[
                        plugin_tombstone("reclaimed", 10, "writer-a", Some((10, 1))),
                        plugin_tombstone("held", 11, "writer-a", Some((10, 2))),
                        plugin_row("kept", "from-a", 12, "writer-a"),
                    ],
                )
                .expect("take the remote removals");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "fresh".into(),
                        value: "value".into(),
                    }],
                )
                .expect("write a local plugin value");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Delete {
                        space: "string".into(),
                        key: "fresh".into(),
                    }],
                )
                .expect("remove the local plugin value");
        }
        joined(&mut store, 11, 13);
        // The remote reclaimed the removal at commit 10 and kept the other,
        // so its floor stands at 11 while it still carries the held one.
        let remote = remote_section(
            &[
                plugin_tombstone("held", 11, "writer-a", Some((10, 2))),
                plugin_row("kept", "from-a", 12, "writer-a"),
            ],
            12,
            11,
            13,
            &spool.path().join("remote"),
        );

        // Another lineage numbers its commits differently, so its floor says
        // nothing about markers this lineage issued.
        apply_received_section(
            &mut store,
            "connection",
            "other-library",
            SectionArrival::Continuing,
            &prepared(&remote),
        )
        .expect("apply the section of another lineage");
        assert!(held_plugin_keys(&mut store).contains(&("reclaimed".to_owned(), true)));

        apply_received_section(
            &mut store,
            "connection",
            "library",
            SectionArrival::Continuing,
            &prepared(&remote),
        )
        .expect("apply the received section");
        assert_eq!(
            held_plugin_keys(&mut store),
            vec![
                ("fresh".to_owned(), true),
                ("held".to_owned(), true),
                ("kept".to_owned(), false),
            ]
        );

        // The next full capture carries exactly what is left: the removal the
        // remote still holds, this device's own unpublished removal, and the
        // value. The reclaimed key does not come back in any form.
        let (captured, _) = capture_state_sections(
            &mut store,
            &Sequence::from(13u64),
            &spool.path().join("published"),
            &Cancellation::default(),
        )
        .expect("capture the state sections");
        let plugins = captured
            .iter()
            .find(|section| section.kind == SectionKind::LocalPlugins)
            .expect("published plugin section");
        assert_eq!(
            published_plugin_values(plugins),
            vec![
                ("fresh".to_owned(), None),
                ("held".to_owned(), None),
                ("kept".to_owned(), Some("from-a".to_owned())),
            ]
        );
    }

    /// A device behind a remote's floor has never seen the removals the floor
    /// covers, so the section arrives as a rejoin however it was offered. Its
    /// own rows are reissued above everything the remote carries rather than
    /// published as an increment over a state it never applied.
    #[test]
    fn a_floor_above_what_this_device_applied_turns_the_section_into_a_rejoin() {
        let spool = tempfile::tempdir().expect("create spool");
        let root = tempfile::tempdir().expect("create store root");
        let mut store = participating_plugin_store(root.path());
        {
            let device = store.device_store_mut().expect("open device store");
            device
                .write_plugin_device_values(
                    "plugin-a",
                    &[PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "mine".into(),
                        value: "local".into(),
                    }],
                )
                .expect("write a local plugin value");
            device
                .apply_section_rows(
                    Section::LocalPlugins,
                    &[plugin_tombstone("reclaimed", 10, "writer-a", Some((10, 1)))],
                )
                .expect("take the remote removal");
        }
        joined(&mut store, 5, 1);
        let remote = remote_section(
            &[plugin_row("theirs", "from-a", 50, "writer-a")],
            12,
            11,
            50,
            &spool.path().join("remote"),
        );

        apply_received_section(
            &mut store,
            "connection",
            "library",
            SectionArrival::Continuing,
            &prepared(&remote),
        )
        .expect("apply the received section");
        let held = store
            .device_store_mut()
            .expect("open device store")
            .read_section_rows(Section::LocalPlugins)
            .expect("read section rows");
        let mine = held
            .iter()
            .find(|row| row.key3 == "mine")
            .expect("this device keeps its own value");
        assert!(mine.write_clock > Sequence::from(50u64));
        // Reissuing turns this device's rows into its own newest writes, so a
        // removal the remote reclaimed has to be gone before that happens or it
        // returns to the remote under a new version.
        assert!(held.iter().all(|row| row.key3 != "reclaimed"));
    }

    #[test]
    fn section_content_that_differs_from_its_reference_is_refused() {
        let spool = tempfile::tempdir().expect("create spool");
        let rows = [hypa_row(&"d".repeat(64), 1, "writer-a", 4)];
        let captured = capture_section(
            SectionKind::Hypa,
            &rows,
            true,
            Sequence::from(1u64),
            Sequence::from(0u64),
            Sequence::from(1u64),
            spool.path(),
            &Cancellation::default(),
        )
        .expect("capture hypa section");
        let mut entries = carried(&captured);
        entries.clear();
        assert!(decode_section(SectionKind::Hypa, &entries, &captured.content_fingerprint).is_err());
    }
}
