//! Section capture and reception for file-based remotes. Device rows become
//! codec entries here and come back the same way, so the local tables can
//! change without changing what the repository holds.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use crate::persistent_store::device_store::{
    sections::{SectionRow, SectionValueRow},
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
    collections::BTreeMap,
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
        (SectionValueRow::Tombstone, _) => Ok(SectionValue::Tombstone),
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
) -> Result<Vec<CapturedSection>> {
    let device = store.device_store_mut().map_err(device_error)?;
    let mut captured = Vec::new();
    for kind in [SectionKind::Hypa, SectionKind::LocalPlugins] {
        let section = section_of(kind).expect("synchronizable section");
        let state = device.section_state(section).map_err(device_error)?;
        if !state.participating {
            continue;
        }
        let rows = device.read_section_rows(section).map_err(device_error)?;
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
    }
    Ok(captured)
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
        SectionValue::Tombstone => SectionValueRow::Tombstone,
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
