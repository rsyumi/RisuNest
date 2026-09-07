//! PocketRisu v1.10.0 and v1.12.0 export raw `inlay/<id>.<ext>` files before their
//! `inlay_sidecar/<id>` JSON. Pair staged paths, never retain media in memory.
use super::{
    cancellation_io, check_cancelled, invalid, open_staged, to_alias_size, AssetAlias,
    CancellationProbe, CancellationReader, CasObjectRole, DurableCasJob, HashMap,
    LegacyPrepareObserver, LocalBackupError, LocalBackupErrorCode, Map, PayloadCas,
    StagedLocalBackupEntry, Value, MAX_METADATA_BYTES,
};
use serde::Deserialize;
use std::io::{BufReader, Read};

pub(super) enum Entry<'a> {
    Payload { id: &'a str, ext: &'a str },
    Metadata { id: &'a str },
    Provenance { id: &'a str },
    Cache,
}

pub(super) fn classify(name: &str) -> Result<Option<Entry<'_>>, LocalBackupError> {
    let Some((namespace, rest)) = name.split_once('/') else {
        return Ok(None);
    };
    if !matches!(
        namespace,
        "inlay" | "inlay_sidecar" | "inlay_info" | "inlay_meta" | "inlay_thumb"
    ) {
        return Ok(None);
    }
    if rest.is_empty() || rest.contains(['/', '\\']) {
        return Err(invalid(
            "PocketRisu Inlay entry must have a single filename",
        ));
    }
    Ok(Some(match namespace {
        "inlay" => {
            let Some((id, ext)) = rest
                .rsplit_once('.')
                .filter(|(id, ext)| !id.is_empty() && !ext.is_empty())
            else {
                return Err(LocalBackupError::new(
                    LocalBackupErrorCode::UnsupportedFormat,
                    "PocketRisu legacy JSON Inlays require the compatibility importer",
                ));
            };
            Entry::Payload { id, ext }
        }
        "inlay_sidecar" | "inlay_info" => Entry::Metadata { id: rest },
        "inlay_meta" => Entry::Provenance { id: rest },
        _ => Entry::Cache,
    }))
}

// Store only references to files. Even sidecar JSON is read one item at a time.
#[derive(Default)]
pub(super) struct MetadataIndex<'a> {
    sidecars: HashMap<&'a str, &'a StagedLocalBackupEntry>,
    provenance: HashMap<&'a str, &'a StagedLocalBackupEntry>,
}

pub(super) fn index_metadata<'a>(
    entries: &'a [StagedLocalBackupEntry],
    cancellation: &dyn CancellationProbe,
) -> Result<MetadataIndex<'a>, LocalBackupError> {
    let mut index = MetadataIndex::default();
    for entry in entries {
        check_cancelled(cancellation)?;
        let target = match classify(&entry.logical_name)? {
            Some(Entry::Metadata { id }) => Some((&mut index.sidecars, id)),
            Some(Entry::Provenance { id }) => Some((&mut index.provenance, id)),
            _ => None,
        };
        if let Some((target, id)) = target {
            if target.insert(id, entry).is_some() {
                return Err(invalid(
                    "PocketRisu backup contains duplicate Inlay metadata",
                ));
            }
        }
    }
    Ok(index)
}

#[derive(Deserialize)]
struct Metadata {
    ext: String,
    name: String,
    #[serde(rename = "type")]
    inlay_type: String,
    width: Option<i64>,
    height: Option<i64>,
}

pub(super) fn prepare(
    entry: &StagedLocalBackupEntry,
    id: &str,
    ext: &str,
    metadata: &MetadataIndex<'_>,
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
) -> Result<AssetAlias, LocalBackupError> {
    check_cancelled(cancellation)?;
    let mut retained_metadata = Map::new();
    if let Some(provenance) = metadata.provenance.get(id) {
        let value: Map<String, Value> = read_metadata(provenance, cancellation)?;
        // RisuNest has no PocketRisu gallery UI, but retain its timestamps and
        // original character/chat ownership instead of discarding user metadata.
        retained_metadata.insert("pocketRisu".to_owned(), Value::Object(value));
    }
    let mut metadata = if let Some(sidecar) = metadata.sidecars.get(id) {
        read_metadata::<Metadata>(sidecar, cancellation)?
    } else {
        let normalized = normalize_extension(ext);
        let (inlay_type, _) = media_type(&normalized)
            .ok_or_else(|| invalid("PocketRisu Inlay needs metadata for this extension"))?;
        Metadata {
            name: format!("{id}.{normalized}"),
            ext: normalized,
            inlay_type: inlay_type.to_owned(),
            width: None,
            height: None,
        }
    };
    metadata.ext = normalize_extension(&metadata.ext);
    if !matches!(
        metadata.inlay_type.as_str(),
        "image" | "audio" | "video" | "signature"
    ) || metadata.width.is_some_and(|n| n < 0)
        || metadata.height.is_some_and(|n| n < 0)
    {
        return Err(invalid("PocketRisu Inlay metadata is invalid"));
    }
    let mime = if metadata.inlay_type == "signature" {
        "application/json"
    } else {
        media_type(&metadata.ext)
            .map(|(_, mime)| mime)
            .unwrap_or("application/octet-stream")
    };
    let mut reader = CancellationReader::observed(open_staged(entry)?, cancellation, observer);
    let payload = durable
        .prepare_reader_expected(
            cas,
            &mut reader,
            &entry.sha256,
            entry.byte_length,
            CasObjectRole::DirectObject,
        )
        .map_err(|error| cancellation_io(error, cancellation))?;
    Ok(AssetAlias {
        key: id.to_owned(),
        object_hash: Some(payload.content_hash),
        kind: "inlay".to_owned(),
        size: to_alias_size(payload.byte_size)?,
        mime: mime.to_owned(),
        name: metadata.name,
        ext: metadata.ext,
        inlay_type: Some(metadata.inlay_type),
        width: metadata.width,
        height: metadata.height,
        metadata: Value::Object(retained_metadata),
    })
}

fn read_metadata<T: serde::de::DeserializeOwned>(
    entry: &StagedLocalBackupEntry,
    cancellation: &dyn CancellationProbe,
) -> Result<T, LocalBackupError> {
    if entry.byte_length > u64::from(MAX_METADATA_BYTES) {
        return Err(invalid("PocketRisu Inlay metadata is too large"));
    }
    let reader = CancellationReader::new(open_staged(entry)?, cancellation)
        .take(u64::from(MAX_METADATA_BYTES) + 1);
    serde_json::from_reader(BufReader::new(reader)).map_err(|_| {
        check_cancelled(cancellation)
            .err()
            .unwrap_or_else(|| invalid("PocketRisu Inlay metadata is invalid"))
    })
}

fn normalize_extension(ext: &str) -> String {
    ext.trim_start_matches('.').to_ascii_lowercase()
}

fn media_type(ext: &str) -> Option<(&'static str, &'static str)> {
    Some(match ext {
        "avif" => ("image", "image/avif"),
        "gif" => ("image", "image/gif"),
        "jpeg" | "jpg" => ("image", "image/jpeg"),
        "png" => ("image", "image/png"),
        "webp" => ("image", "image/webp"),
        "flac" => ("audio", "audio/flac"),
        "mp3" => ("audio", "audio/mpeg"),
        "ogg" => ("audio", "audio/ogg"),
        "wav" => ("audio", "audio/wav"),
        "mkv" => ("video", "video/x-matroska"),
        "mp4" => ("video", "video/mp4"),
        "webm" => ("video", "video/webm"),
        _ => return None,
    })
}
