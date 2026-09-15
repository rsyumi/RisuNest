use crate::asset_repository::PayloadCas;
use crate::trust_boundary::{is_lower_hex_byte, sync_directory};
use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::metadata::{LoopCount, Orientation};
use image::{
    AnimationDecoder, Delay, DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage,
};
use serde::{
    de::{self, Visitor},
    Deserialize, Deserializer, Serialize,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::http::{
    header::{self, HeaderValue},
    Method, Request, Response, StatusCode,
};
use tauri::{AppHandle, Manager};

pub(crate) mod ipc;

const MAX_BODY_BYTES: u64 = 1024 * 1024;
const EXPOSED_HEADERS: &str = "Accept-Ranges, Content-Length, Content-Range, Content-Type, ETag";
static INLAY_WRITE_LOCK: Mutex<()> = Mutex::new(());
const MAX_INLAY_DIMENSION: u32 = u32::MAX;
const MAX_INLAY_ANIMATION_FPS: u32 = 240;
const MAX_ANIMATION_FRAMES: usize = 600;
const MAX_ANIMATION_RGBA_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ANIMATION_CANVAS_SIDE: u32 = 4096;
/// Browsers play a GIF frame at or under this delay as `SLOW_FRAME_DELAY_MS`.
const GIF_FAST_FRAME_DELAY_MS: u32 = 10;
const SLOW_FRAME_DELAY_MS: u32 = 100;
/// A leading frame without a delay of its own still has to advance the timeline.
const FIRST_FRAME_MIN_DELAY_MS: u32 = 10;

#[derive(Deserialize)]
struct BlobMetadata {
    key: String,
    kind: String,
    size: u64,
    mime: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayImageMetadata {
    key: String,
    kind: String,
    size: u64,
    mime: String,
    name: String,
    ext: String,
    inlay_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EncodedInlayImage {
    data: Vec<u8>,
    metadata: InlayImageMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlayEncodeOptions {
    format: InlayEncodeFormat,
    #[serde(deserialize_with = "deserialize_inlay_quality")]
    quality: u8,
    #[serde(deserialize_with = "deserialize_inlay_max_dimension")]
    max_dimension: u32,
    skip_reencode: bool,
    /// Frames per second an animation is thinned down to, or 0 to keep the original rate.
    #[serde(deserialize_with = "deserialize_inlay_animation_fps")]
    animation_max_fps: u32,
}

fn deserialize_inlay_animation_fps<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u32::deserialize(deserializer)?.min(MAX_INLAY_ANIMATION_FPS))
}

fn deserialize_inlay_quality<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(u8::deserialize(deserializer)?.clamp(1, 100))
}

fn deserialize_inlay_max_dimension<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    struct MaxDimensionVisitor;

    impl Visitor<'_> for MaxDimensionVisitor {
        type Value = u32;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a numeric Inlay maximum dimension")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.clamp(0, i64::from(MAX_INLAY_DIMENSION)) as u32)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(value.min(u64::from(MAX_INLAY_DIMENSION)) as u32)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite() {
                return Ok(0);
            }
            Ok(value.round().clamp(0.0, f64::from(MAX_INLAY_DIMENSION)) as u32)
        }
    }

    deserializer.deserialize_any(MaxDimensionVisitor)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InlayEncodeFormat {
    Webp,
    Png,
    Original,
}

impl Default for InlayEncodeOptions {
    fn default() -> Self {
        Self {
            format: InlayEncodeFormat::Webp,
            quality: 85,
            max_dimension: 0,
            skip_reencode: false,
            animation_max_fps: 0,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InlayWriteTransaction {
    id: String,
    suffix: String,
    had_payload: bool,
    had_metadata: bool,
    payload_sha256: String,
    metadata_sha256: String,
    #[serde(default)]
    phase: InlayWritePhase,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum InlayWritePhase {
    #[default]
    Prepared,
    Committed,
}

struct ResolvedBlob {
    payload_path: PathBuf,
    mime: String,
    size: u64,
    validator: String,
    cache_control: &'static str,
}

enum RequestedRange {
    Full,
    Partial { start: u64, end: u64 },
}

pub(crate) fn decode_physical_key(uri: &str) -> Option<String> {
    let parsed = url::Url::parse(uri).ok()?;
    let valid_origin = match parsed.scheme() {
        "risuasset" => parsed.host_str() == Some("localhost"),
        "http" | "https" => parsed.host_str() == Some("risuasset.localhost"),
        _ => false,
    };
    if !valid_origin {
        return None;
    }
    let encoded = parsed.path().strip_prefix('/')?;
    if encoded.is_empty() || encoded.contains('/') || encoded.len() % 2 != 0 {
        return None;
    }
    let physical_key = String::from_utf8(hex::decode(encoded).ok()?).ok()?;
    if valid_physical_key(&physical_key) {
        Some(physical_key)
    } else {
        None
    }
}

fn valid_physical_key(key: &str) -> bool {
    if cas_content_hash(key).is_some() {
        return true;
    }
    if let Some(rest) = key.strip_prefix("assets/") {
        return !rest.is_empty() && rest.split('/').all(safe_segment);
    }
    let Some(encoded) = key
        .strip_prefix("blobstore/inlays/")
        .and_then(|value| value.strip_suffix(".bin"))
    else {
        return false;
    };
    !encoded.is_empty() && encoded.len() % 2 == 0 && encoded.bytes().all(is_lower_hex_byte)
}

fn cas_content_hash(key: &str) -> Option<String> {
    let (shard, suffix) = key.strip_prefix("assets-v2/objects/")?.split_once('/')?;
    if shard.len() != 2
        || suffix.len() != 62
        || !shard.bytes().chain(suffix.bytes()).all(is_lower_hex_byte)
    {
        return None;
    }
    Some(format!("{shard}{suffix}"))
}

fn safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.ends_with(['.', ' '])
        && !segment
            .chars()
            .any(|character| matches!(character, '\0' | '\\' | ':'))
}

fn logical_key(physical_key: &str) -> Option<(String, &'static str)> {
    if physical_key.starts_with("assets/") {
        return Some((physical_key.to_owned(), "asset"));
    }
    let encoded = physical_key
        .strip_prefix("blobstore/inlays/")?
        .strip_suffix(".bin")?;
    Some((String::from_utf8(hex::decode(encoded).ok()?).ok()?, "inlay"))
}

fn cas_descriptor(uri: &str) -> Option<(String, u64)> {
    let parsed = url::Url::parse(uri).ok()?;
    let mut mime = None;
    let mut size = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "mime" if mime.is_none() => mime = Some(value.into_owned()),
            "size" if size.is_none() => size = Some(value.parse::<u64>().ok()?),
            _ => return None,
        }
    }
    let mime = mime?;
    if mime.is_empty() || HeaderValue::from_str(&mime).is_err() {
        return None;
    }
    Some((mime, size?))
}

fn resolve_blob(root: &Path, uri: &str, physical_key: String) -> Option<ResolvedBlob> {
    if let Some(content_hash) = cas_content_hash(&physical_key) {
        let (mime, expected_size) = cas_descriptor(uri)?;
        let payload_path = PayloadCas::new(root)
            .ok()?
            .object_path(&content_hash)
            .ok()??;
        let file_metadata = fs::metadata(&payload_path).ok()?;
        if !file_metadata.is_file() || file_metadata.len() != expected_size {
            return None;
        }
        return Some(ResolvedBlob {
            payload_path,
            mime,
            size: expected_size,
            validator: format!("\"{content_hash}\""),
            cache_control: "public, max-age=31536000, immutable",
        });
    }
    let (logical_key, expected_kind) = logical_key(&physical_key)?;
    let metadata_path = root
        .join("blobstore")
        .join("metadata")
        .join(format!("{}.json", hex::encode(logical_key.as_bytes())));
    reject_link_components(root, &metadata_path)?;
    let blob_metadata: BlobMetadata =
        serde_json::from_slice(&fs::read(metadata_path).ok()?).ok()?;
    let payload_path = physical_key
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment));
    reject_link_components(root, &payload_path)?;
    let file_metadata = fs::metadata(&payload_path).ok()?;
    if !file_metadata.is_file()
        || blob_metadata.key != logical_key
        || blob_metadata.kind != expected_kind
        || blob_metadata.size != file_metadata.len()
        || HeaderValue::from_str(&blob_metadata.mime).is_err()
    {
        return None;
    }
    let modified = file_metadata.modified().ok()?;
    Some(ResolvedBlob {
        payload_path,
        mime: blob_metadata.mime,
        size: file_metadata.len(),
        validator: etag(modified, file_metadata.len()),
        cache_control: "no-cache",
    })
}

fn reject_link_components(root: &Path, path: &Path) -> Option<()> {
    let mut current = root.to_path_buf();
    for part in path.strip_prefix(root).ok()?.components() {
        current.push(part);
        if crate::trust_boundary::is_link_like(&fs::symlink_metadata(&current).ok()?) {
            return None;
        }
    }
    Some(())
}

fn etag(modified: SystemTime, size: u64) -> String {
    let elapsed = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!(
        "W/\"{:x}-{:x}-{:x}\"",
        elapsed.as_secs(),
        elapsed.subsec_nanos(),
        size
    )
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn sync_parent(path: &Path) -> std::io::Result<()> {
    sync_directory(path.parent().expect("managed media path has a parent"))?;
    Ok(())
}

fn create_directory_synced(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "managed directory path is not a directory",
        ));
    }
    let parent = path.parent().expect("managed directory has a parent");
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .expect("managed directory name is UTF-8");
    let staged = parent.join(format!(".{name}.replace-next-dir"));
    match fs::remove_dir(&staged) {
        Ok(()) => {
            sync_directory(parent)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir(&staged)?;
    sync_directory(parent)?;
    rename_synced(&staged, path)
}

fn rename_synced(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from_wide.as_ptr(), to_wide.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)?;
        sync_parent(to)?;
        if from.parent() != to.parent() {
            sync_parent(from)?;
        }
    }
    Ok(())
}

fn replace_synced(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{
            MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        };
        let from_wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to_wide: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe {
            MoveFileExW(
                from_wide.as_ptr(),
                to_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(from, to)?;
        sync_parent(to)?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn persist_transaction_atomic(
    transaction_path: &Path,
    transaction_temp: &Path,
    bytes: &[u8],
    replace: bool,
) -> std::io::Result<()> {
    if let Err(error) = write_synced(transaction_temp, bytes)
        .and_then(|()| sync_parent(transaction_temp))
        .and_then(|()| {
            if replace {
                replace_synced(transaction_temp, transaction_path)
            } else {
                rename_synced(transaction_temp, transaction_path)
            }
        })
    {
        let _ = remove_file_if_exists(transaction_temp);
        return Err(error);
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rollback_inlay_pair(
    payload_path: &Path,
    metadata_path: &Path,
    previous_payload: &Path,
    previous_metadata: &Path,
) -> std::io::Result<()> {
    if previous_payload.exists() {
        remove_file_if_exists(payload_path)?;
        rename_synced(previous_payload, payload_path)?;
    }
    if previous_metadata.exists() {
        remove_file_if_exists(metadata_path)?;
        rename_synced(previous_metadata, metadata_path)?;
    }
    Ok(())
}

fn inlay_paths(root: &Path, id: &str) -> (PathBuf, PathBuf, PathBuf) {
    let encoded_id = hex::encode(id.as_bytes());
    (
        root.join("blobstore/inlays")
            .join(format!("{encoded_id}.bin")),
        root.join("blobstore/metadata")
            .join(format!("{encoded_id}.json")),
        root.join("blobstore/inlay-transactions")
            .join(format!("{encoded_id}.json")),
    )
}

fn pair_matches_transaction(
    metadata_path: &Path,
    payload_path: &Path,
    transaction: &InlayWriteTransaction,
) -> bool {
    let Ok(metadata_bytes) = fs::read(metadata_path) else {
        return false;
    };
    let Ok(metadata) = serde_json::from_slice::<InlayImageMetadata>(&metadata_bytes) else {
        return false;
    };
    let Ok(payload_bytes) = fs::read(payload_path) else {
        return false;
    };
    metadata.key == transaction.id
        && metadata.kind == "inlay"
        && metadata.inlay_type == "image"
        && metadata.size == payload_bytes.len() as u64
        && sha256_hex(&payload_bytes) == transaction.payload_sha256
        && sha256_hex(&metadata_bytes) == transaction.metadata_sha256
}

fn transaction_artifact_paths(
    root: &Path,
    transaction: &InlayWriteTransaction,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let (payload_path, metadata_path, _) = inlay_paths(root, &transaction.id);
    let encoded_id = hex::encode(transaction.id.as_bytes());
    (
        payload_path.with_extension("bin.replace-previous"),
        metadata_path.with_extension("json.replace-previous"),
        payload_path.parent().unwrap().join(format!(
            ".{encoded_id}.{}.bin.replace-next",
            transaction.suffix
        )),
        metadata_path.parent().unwrap().join(format!(
            ".{encoded_id}.{}.json.replace-next",
            transaction.suffix
        )),
    )
}

fn cleanup_transaction_artifacts(
    root: &Path,
    transaction_path: &Path,
    transaction: &InlayWriteTransaction,
    best_effort: bool,
) -> Result<(), String> {
    let (previous_payload, previous_metadata, next_payload, next_metadata) =
        transaction_artifact_paths(root, transaction);
    let mut first_error = None;
    for path in [
        previous_payload,
        previous_metadata,
        next_payload,
        next_metadata,
    ] {
        if let Err(error) = remove_file_if_exists(&path) {
            first_error.get_or_insert_with(|| error.to_string());
        }
    }
    if first_error.is_none() {
        if let Err(error) = remove_file_if_exists(transaction_path) {
            first_error = Some(error.to_string());
        }
    }
    if best_effort {
        Ok(())
    } else {
        first_error.map_or(Ok(()), Err)
    }
}

fn mark_transaction_committed(
    root: &Path,
    transaction_path: &Path,
    transaction: &mut InlayWriteTransaction,
) -> Result<(), String> {
    transaction.phase = InlayWritePhase::Committed;
    let bytes = serde_json::to_vec(transaction)
        .map_err(|error| format!("failed to encode committed Inlay transaction: {error}"))?;
    let encoded_id = hex::encode(transaction.id.as_bytes());
    let transaction_temp = root.join("blobstore/inlay-transactions").join(format!(
        ".{encoded_id}.{}.committed.json.replace-next",
        transaction.suffix
    ));
    remove_file_if_exists(&transaction_temp)
        .map_err(|error| format!("failed to clear stale committed transaction stage: {error}"))?;
    persist_transaction_atomic(transaction_path, &transaction_temp, &bytes, true)
        .map_err(|error| format!("failed to commit Inlay transaction: {error}"))
}

fn recover_inlay_transaction(root: &Path, transaction_path: &Path) -> Result<(), String> {
    let mut transaction: InlayWriteTransaction = serde_json::from_slice(
        &fs::read(transaction_path)
            .map_err(|error| format!("failed to read Inlay transaction: {error}"))?,
    )
    .map_err(|error| format!("failed to decode Inlay transaction: {error}"))?;
    let (payload_path, metadata_path, expected_transaction_path) =
        inlay_paths(root, &transaction.id);
    if expected_transaction_path != transaction_path {
        return Err("Inlay transaction id does not match its path".to_owned());
    }
    if transaction.phase == InlayWritePhase::Committed {
        return cleanup_transaction_artifacts(root, transaction_path, &transaction, true);
    }
    let (previous_payload, previous_metadata, _, _) =
        transaction_artifact_paths(root, &transaction);

    if pair_matches_transaction(&metadata_path, &payload_path, &transaction) {
        mark_transaction_committed(root, transaction_path, &mut transaction)?;
        return cleanup_transaction_artifacts(root, transaction_path, &transaction, true);
    } else {
        if previous_payload.exists() {
            remove_file_if_exists(&payload_path).map_err(|error| error.to_string())?;
            rename_synced(&previous_payload, &payload_path)
                .map_err(|error| format!("failed to restore prior Inlay payload: {error}"))?;
        } else if !transaction.had_payload {
            remove_file_if_exists(&payload_path).map_err(|error| error.to_string())?;
        }
        if previous_metadata.exists() {
            remove_file_if_exists(&metadata_path).map_err(|error| error.to_string())?;
            rename_synced(&previous_metadata, &metadata_path)
                .map_err(|error| format!("failed to restore prior Inlay metadata: {error}"))?;
        } else if !transaction.had_metadata {
            remove_file_if_exists(&metadata_path).map_err(|error| error.to_string())?;
        }
    }

    cleanup_transaction_artifacts(root, transaction_path, &transaction, false)
}

fn error_after_recovery(root: &Path, transaction_path: &Path, error: String) -> String {
    if !transaction_path.exists() {
        return error;
    }
    match recover_inlay_transaction(root, transaction_path) {
        Ok(()) => error,
        Err(recovery_error) => format!("{error}; Inlay recovery also failed: {recovery_error}"),
    }
}

fn recover_malformed_transaction(root: &Path, transaction_path: &Path) -> Result<(), String> {
    let id = transaction_path
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(|value| hex::decode(value).ok())
        .and_then(|value| String::from_utf8(value).ok());
    if let Some(id) = id {
        let (payload_path, metadata_path, _) = inlay_paths(root, &id);
        let previous_payload = payload_path.with_extension("bin.replace-previous");
        let previous_metadata = metadata_path.with_extension("json.replace-previous");
        if previous_payload.exists() || previous_metadata.exists() {
            rollback_inlay_pair(
                &payload_path,
                &metadata_path,
                &previous_payload,
                &previous_metadata,
            )
            .map_err(|error| format!("failed to recover malformed Inlay transaction: {error}"))?;
        }
    }
    remove_file_if_exists(transaction_path).map_err(|error| error.to_string())
}

fn truthful_inlay_pair(payload_path: &Path, metadata_path: &Path, id: &str) -> bool {
    let Ok(payload_metadata) = fs::metadata(payload_path) else {
        return false;
    };
    let Ok(metadata_bytes) = fs::read(metadata_path) else {
        return false;
    };
    let Ok(metadata) = serde_json::from_slice::<BlobMetadata>(&metadata_bytes) else {
        return false;
    };
    payload_metadata.is_file()
        && metadata.key == id
        && metadata.kind == "inlay"
        && metadata.size == payload_metadata.len()
        && HeaderValue::from_str(&metadata.mime).is_ok()
}

fn artifact_encoded_id(name: &str, extension: &str) -> Option<String> {
    let previous_suffix = format!(".{extension}.replace-previous");
    if let Some(encoded) = name.strip_suffix(&previous_suffix) {
        return Some(encoded.to_owned());
    }
    let next_suffix = format!(".{extension}.replace-next");
    let staged = name.strip_prefix('.')?.strip_suffix(&next_suffix)?;
    Some(staged.split_once('.')?.0.to_owned())
}

fn collect_artifact_ids(
    directory: &Path,
    extension: &str,
    ids: &mut HashSet<String>,
) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            continue;
        }
        if let Some(encoded) = entry
            .file_name()
            .to_str()
            .and_then(|name| artifact_encoded_id(name, extension))
        {
            ids.insert(encoded);
        }
    }
    Ok(())
}

fn recover_journal_less_artifacts(root: &Path) -> Result<(), String> {
    let payload_dir = root.join("blobstore/inlays");
    let metadata_dir = root.join("blobstore/metadata");
    let mut encoded_ids = HashSet::new();
    collect_artifact_ids(&payload_dir, "bin", &mut encoded_ids)?;
    collect_artifact_ids(&metadata_dir, "json", &mut encoded_ids)?;
    for encoded_id in encoded_ids {
        let Some(id) = hex::decode(&encoded_id)
            .ok()
            .and_then(|value| String::from_utf8(value).ok())
        else {
            continue;
        };
        let (payload_path, metadata_path, transaction_path) = inlay_paths(root, &id);
        if transaction_path.exists() {
            continue;
        }
        let previous_payload = payload_path.with_extension("bin.replace-previous");
        let previous_metadata = metadata_path.with_extension("json.replace-previous");
        if truthful_inlay_pair(&payload_path, &metadata_path, &id) {
            remove_file_if_exists(&previous_payload).map_err(|error| error.to_string())?;
            remove_file_if_exists(&previous_metadata).map_err(|error| error.to_string())?;
        } else if previous_payload.exists() || previous_metadata.exists() {
            rollback_inlay_pair(
                &payload_path,
                &metadata_path,
                &previous_payload,
                &previous_metadata,
            )
            .map_err(|error| format!("failed to recover journal-less Inlay pair: {error}"))?;
        } else {
            remove_file_if_exists(&payload_path).map_err(|error| error.to_string())?;
            remove_file_if_exists(&metadata_path).map_err(|error| error.to_string())?;
        }
    }
    cleanup_replace_next_files(&payload_dir)?;
    cleanup_replace_next_files(&metadata_dir)
}

fn cleanup_replace_next_files(directory: &Path) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        let is_replace_next = path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.starts_with('.') && value.ends_with(".replace-next"));
        if is_replace_next && path.is_file() {
            remove_file_if_exists(&path).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn cleanup_directory_stage(path: &Path) -> Result<(), String> {
    match fs::remove_dir(path) {
        Ok(()) => sync_parent(path).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub(crate) fn recover_inlay_writes(root: &Path) -> Result<(), String> {
    let _guard = INLAY_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cleanup_directory_stage(&root.join(".blobstore.replace-next-dir"))?;
    cleanup_directory_stage(
        &root
            .join("blobstore")
            .join(".inlay-transactions.replace-next-dir"),
    )?;
    cleanup_directory_stage(&root.join("blobstore").join(".inlays.replace-next-dir"))?;
    cleanup_directory_stage(&root.join("blobstore").join(".metadata.replace-next-dir"))?;
    let transaction_dir = root.join("blobstore/inlay-transactions");
    match fs::read_dir(&transaction_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry
                    .map_err(|error| format!("failed to read Inlay transaction entry: {error}"))?;
                if entry
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                {
                    let path = entry.path();
                    let valid_transaction = fs::read(&path)
                        .ok()
                        .and_then(|bytes| {
                            serde_json::from_slice::<InlayWriteTransaction>(&bytes).ok()
                        })
                        .is_some_and(|transaction| inlay_paths(root, &transaction.id).2 == path);
                    if valid_transaction {
                        recover_inlay_transaction(root, &path)?;
                    } else {
                        recover_malformed_transaction(root, &path)?;
                    }
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("failed to read Inlay transactions: {error}")),
    }
    cleanup_replace_next_files(&transaction_dir)?;
    recover_journal_less_artifacts(root)
}

fn promote_inlay_pair(
    payload_path: &Path,
    metadata_path: &Path,
    next_payload: &Path,
    next_metadata: &Path,
) -> Result<(), String> {
    let previous_payload = payload_path.with_extension("bin.replace-previous");
    let previous_metadata = metadata_path.with_extension("json.replace-previous");
    remove_file_if_exists(&previous_payload).map_err(|error| error.to_string())?;
    remove_file_if_exists(&previous_metadata).map_err(|error| error.to_string())?;

    if payload_path.exists() {
        rename_synced(payload_path, &previous_payload)
            .map_err(|error| format!("failed to preserve prior Inlay payload: {error}"))?;
    }
    if metadata_path.exists() {
        if let Err(error) = rename_synced(metadata_path, &previous_metadata) {
            let _ = rollback_inlay_pair(
                payload_path,
                metadata_path,
                &previous_payload,
                &previous_metadata,
            );
            return Err(format!("failed to preserve prior Inlay metadata: {error}"));
        }
    }

    let promotion = rename_synced(next_payload, payload_path)
        .map_err(|error| format!("failed to activate Inlay payload: {error}"))
        .and_then(|()| {
            rename_synced(next_metadata, metadata_path)
                .map_err(|error| format!("failed to activate Inlay metadata: {error}"))
        });
    if let Err(error) = promotion {
        let rollback = rollback_inlay_pair(
            payload_path,
            metadata_path,
            &previous_payload,
            &previous_metadata,
        );
        let _ = remove_file_if_exists(next_payload);
        let _ = remove_file_if_exists(next_metadata);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; failed to roll back prior Inlay: {rollback_error}"
            )),
        };
    }

    Ok(())
}

pub(crate) fn write_inlay_image(
    root: &Path,
    id: &str,
    data: &[u8],
    name: &str,
) -> Result<InlayImageMetadata, String> {
    write_inlay_image_with_options(root, id, data, name, None)
}

fn write_inlay_image_with_options(
    root: &Path,
    id: &str,
    data: &[u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
) -> Result<InlayImageMetadata, String> {
    write_inlay_image_with_suffix(
        root,
        id,
        data,
        name,
        options,
        uuid::Uuid::new_v4().to_string(),
    )
}

fn write_inlay_image_with_suffix(
    root: &Path,
    id: &str,
    data: &[u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
    suffix: String,
) -> Result<InlayImageMetadata, String> {
    let EncodedInlayImage {
        data: encoded,
        metadata,
    } = encode_inlay_image(id, data, name, options)?;
    let metadata_bytes = serde_json::to_vec(&metadata)
        .map_err(|error| format!("failed to encode Inlay metadata: {error}"))?;
    let encoded_id = hex::encode(id.as_bytes());
    let blobstore_dir = root.join("blobstore");
    let payload_dir = blobstore_dir.join("inlays");
    let metadata_dir = blobstore_dir.join("metadata");
    let transaction_dir = blobstore_dir.join("inlay-transactions");
    let _guard = INLAY_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root_existed = root.is_dir();
    fs::create_dir_all(root)
        .map_err(|error| format!("failed to create application data directory: {error}"))?;
    if !root_existed {
        sync_parent(root)
            .map_err(|error| format!("failed to sync application data directory: {error}"))?;
    }
    create_directory_synced(&blobstore_dir)
        .map_err(|error| format!("failed to create BlobStore directory: {error}"))?;
    create_directory_synced(&payload_dir)
        .map_err(|error| format!("failed to create Inlay payload directory: {error}"))?;
    create_directory_synced(&metadata_dir)
        .map_err(|error| format!("failed to create Inlay metadata directory: {error}"))?;
    create_directory_synced(&transaction_dir)
        .map_err(|error| format!("failed to create Inlay transaction directory: {error}"))?;
    let (payload_path, metadata_path, transaction_path) = inlay_paths(root, id);
    let next_payload = payload_dir.join(format!(".{encoded_id}.{suffix}.bin.replace-next"));
    let next_metadata = metadata_dir.join(format!(".{encoded_id}.{suffix}.json.replace-next"));

    if transaction_path.exists() {
        recover_inlay_transaction(root, &transaction_path)?;
        if transaction_path.exists() {
            return Err("committed Inlay transaction cleanup is still pending".to_owned());
        }
    }
    let transaction = InlayWriteTransaction {
        id: id.to_owned(),
        suffix,
        had_payload: payload_path.exists(),
        had_metadata: metadata_path.exists(),
        payload_sha256: sha256_hex(encoded.as_ref()),
        metadata_sha256: sha256_hex(&metadata_bytes),
        phase: InlayWritePhase::Prepared,
    };
    let transaction_bytes = serde_json::to_vec(&transaction)
        .map_err(|error| format!("failed to encode Inlay transaction: {error}"))?;
    let transaction_temp = transaction_dir.join(format!(
        ".{encoded_id}.{}.json.replace-next",
        transaction.suffix
    ));
    if let Err(error) = persist_transaction_atomic(
        &transaction_path,
        &transaction_temp,
        &transaction_bytes,
        false,
    ) {
        return Err(error_after_recovery(
            root,
            &transaction_path,
            format!("failed to persist Inlay transaction: {error}"),
        ));
    }
    if let Err(error) =
        write_synced(&next_payload, encoded.as_ref()).and_then(|()| sync_parent(&next_payload))
    {
        return Err(error_after_recovery(
            root,
            &transaction_path,
            format!("failed to stage Inlay payload: {error}"),
        ));
    }
    if let Err(error) =
        write_synced(&next_metadata, &metadata_bytes).and_then(|()| sync_parent(&next_metadata))
    {
        return Err(error_after_recovery(
            root,
            &transaction_path,
            format!("failed to stage Inlay metadata: {error}"),
        ));
    }
    if let Err(error) =
        promote_inlay_pair(&payload_path, &metadata_path, &next_payload, &next_metadata)
    {
        return Err(error_after_recovery(root, &transaction_path, error));
    }
    recover_inlay_transaction(root, &transaction_path)
        .map_err(|error| format!("failed to durably commit Inlay transaction: {error}"))?;
    Ok(metadata)
}

#[derive(Clone, Copy, PartialEq)]
enum InlayAnimation {
    Gif,
    Apng,
    WebP,
}

fn inlay_animation(format: ImageFormat, data: &[u8]) -> Option<InlayAnimation> {
    match format {
        ImageFormat::Gif => Some(InlayAnimation::Gif),
        ImageFormat::WebP => webp::BitstreamFeatures::new(data)
            .is_some_and(|features| features.has_animation())
            .then_some(InlayAnimation::WebP),
        ImageFormat::Png => PngDecoder::new(Cursor::new(data))
            .ok()
            .and_then(|decoder| decoder.is_apng().ok())
            .unwrap_or(false)
            .then_some(InlayAnimation::Apng),
        _ => None,
    }
}

fn inlay_media_type(format: Option<ImageFormat>, name: &str) -> (String, String) {
    let known = format.and_then(|format| match format {
        ImageFormat::Png => Some(("image/png", "png")),
        ImageFormat::Jpeg => Some(("image/jpeg", "jpg")),
        ImageFormat::WebP => Some(("image/webp", "webp")),
        ImageFormat::Gif => Some(("image/gif", "gif")),
        ImageFormat::Avif => Some(("image/avif", "avif")),
        ImageFormat::Bmp => Some(("image/bmp", "bmp")),
        ImageFormat::Tiff => Some(("image/tiff", "tiff")),
        _ => None,
    });
    if let Some((mime, ext)) = known {
        return (mime.to_owned(), ext.to_owned());
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    };
    (mime.to_owned(), ext)
}

/// Stores the input untouched. Every image this encoder cannot improve ends up
/// here, so an attachment never fails because of its format.
fn preserved_inlay_image(
    id: &str,
    data: &[u8],
    name: &str,
    size: Option<(u32, u32)>,
) -> EncodedInlayImage {
    let (mime, ext) = inlay_media_type(image::guess_format(data).ok(), name);
    EncodedInlayImage {
        data: data.to_vec(),
        metadata: InlayImageMetadata {
            key: id.to_owned(),
            kind: "inlay".to_owned(),
            size: data.len() as u64,
            mime,
            name: name.to_owned(),
            ext,
            inlay_type: "image".to_owned(),
            width: size.map(|(width, _)| width),
            height: size.map(|(_, height)| height),
        },
    }
}

fn frame_delay_ms(delay: Delay, source: InlayAnimation) -> u32 {
    let (numerator, denominator) = delay.numer_denom_ms();
    let milliseconds = if denominator == 0 {
        0
    } else {
        numerator / denominator
    };
    match source {
        // Browsers play a GIF frame this short as a tenth of a second, so keeping
        // the stored delay would speed the animation up against what was on screen.
        InlayAnimation::Gif if milliseconds <= GIF_FAST_FRAME_DELAY_MS => SLOW_FRAME_DELAY_MS,
        InlayAnimation::Apng if milliseconds == 0 => SLOW_FRAME_DELAY_MS,
        // Animated WebP counts in milliseconds, where a short delay is meant literally.
        _ => milliseconds,
    }
}

struct DecodedInlayAnimation {
    frames: Vec<(RgbaImage, u32)>,
    loop_count: i32,
}

fn decode_inlay_animation(
    data: &[u8],
    source: InlayAnimation,
) -> Result<DecodedInlayAnimation, String> {
    let (loop_count, frames) = match source {
        InlayAnimation::Gif => {
            let decoder = GifDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read GIF Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
        InlayAnimation::Apng => {
            let decoder = PngDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read APNG Inlay image: {error}"))?
                .apng()
                .map_err(|error| format!("failed to read APNG Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
        InlayAnimation::WebP => {
            let decoder = WebPDecoder::new(Cursor::new(data))
                .map_err(|error| format!("failed to read animated WebP Inlay image: {error}"))?;
            (decoder.loop_count(), decoder.into_frames())
        }
    };
    let mut collected: Vec<(RgbaImage, u32)> = Vec::new();
    let mut rgba_bytes: u64 = 0;
    for frame in frames {
        let frame =
            frame.map_err(|error| format!("failed to decode Inlay animation frame: {error}"))?;
        if collected.len() >= MAX_ANIMATION_FRAMES {
            return Err("Inlay animation has too many frames".to_owned());
        }
        let delay = frame_delay_ms(frame.delay(), source);
        let buffer = frame.into_buffer();
        rgba_bytes += buffer.as_raw().len() as u64;
        if rgba_bytes > MAX_ANIMATION_RGBA_BYTES {
            return Err("Inlay animation needs too much memory".to_owned());
        }
        collected.push((buffer, delay));
    }
    if collected.is_empty() {
        return Err("Inlay animation has no frames".to_owned());
    }
    Ok(DecodedInlayAnimation {
        frames: collected,
        loop_count: match loop_count {
            LoopCount::Infinite => 0,
            LoopCount::Finite(count) => count.get().min(i32::MAX as u32) as i32,
        },
    })
}

/// Drops frames the timeline cannot carry and hands their time to the frame
/// before them, so the animation still runs for exactly as long as it did.
fn merge_animation_frames(
    frames: Vec<(RgbaImage, u32)>,
    min_delay_ms: u32,
) -> Vec<(RgbaImage, u32)> {
    let mut kept: Vec<(RgbaImage, u32)> = Vec::with_capacity(frames.len());
    for (buffer, delay) in frames {
        match kept.last_mut() {
            Some(previous) if delay == 0 || previous.1 < min_delay_ms => previous.1 += delay,
            _ => {
                let delay = if kept.is_empty() && delay == 0 {
                    FIRST_FRAME_MIN_DELAY_MS
                } else {
                    delay
                };
                kept.push((buffer, delay));
            }
        }
    }
    kept
}

fn animation_canvas(
    width: u32,
    height: u32,
    options: &InlayEncodeOptions,
) -> Result<(u32, u32), String> {
    let (width, height) = if options.max_dimension > 0 && width.max(height) > options.max_dimension
    {
        let scale = options.max_dimension as f64 / width.max(height) as f64;
        (
            (width as f64 * scale).round().max(1.0) as u32,
            (height as f64 * scale).round().max(1.0) as u32,
        )
    } else {
        (width, height)
    };
    if width.max(height) > MAX_ANIMATION_CANVAS_SIDE {
        return Err("Inlay animation canvas is too large".to_owned());
    }
    Ok((width, height))
}

/// libwebp reads a frame's duration from the timestamp of the frame after it,
/// and the encoder this crate exposes cannot pass an end timestamp, so it guesses
/// the last one. Writing the time that is left into the last frame keeps the
/// animation as long as it was, even where libwebp folded repeated frames together.
fn set_last_animation_frame_duration(
    data: &mut [u8],
    total_duration_ms: u32,
) -> Result<(), String> {
    let mut frames: Vec<(usize, u32)> = Vec::new();
    let mut offset = 12usize;
    while offset + 8 <= data.len() {
        let size = u32::from_le_bytes(
            data[offset + 4..offset + 8]
                .try_into()
                .map_err(|_| "unreadable WebP chunk size".to_owned())?,
        ) as usize;
        let payload = offset + 8;
        if payload + size > data.len() {
            return Err("truncated WebP chunk in the Inlay animation".to_owned());
        }
        if &data[offset..offset + 4] == b"ANMF" {
            if size < 16 {
                return Err("truncated animation frame in the Inlay animation".to_owned());
            }
            let duration = u32::from_le_bytes([
                data[payload + 12],
                data[payload + 13],
                data[payload + 14],
                0,
            ]);
            frames.push((payload, duration));
        }
        offset = payload + size + (size & 1);
    }
    let Some((payload, _)) = frames.last().copied() else {
        return Err("the encoded Inlay animation has no frames".to_owned());
    };
    let earlier: u32 = frames[..frames.len() - 1]
        .iter()
        .map(|(_, duration)| *duration)
        .sum();
    let last = total_duration_ms
        .saturating_sub(earlier)
        .clamp(1, 0x00ff_ffff);
    data[payload + 12..payload + 15].copy_from_slice(&last.to_le_bytes()[..3]);
    Ok(())
}

fn encode_animated_webp(
    frames: &[(RgbaImage, u32)],
    width: u32,
    height: u32,
    quality: u8,
    loop_count: i32,
) -> Result<Vec<u8>, String> {
    let mut config = webp::WebPConfig::new()
        .map_err(|()| "failed to prepare the WebP animation encoder".to_owned())?;
    config.quality = f32::from(quality);
    let mut encoder = webp::AnimEncoder::new(width, height, &config);
    encoder.set_loop_count(loop_count);
    let mut timestamp: i32 = 0;
    for (buffer, delay) in frames {
        encoder.add_frame(webp::AnimFrame::from_rgba(
            buffer.as_raw(),
            width,
            height,
            timestamp,
        ));
        timestamp = timestamp.saturating_add(*delay as i32);
    }
    let mut encoded = encoder
        .try_encode()
        .map(|memory| memory.to_vec())
        .map_err(|error| format!("failed to encode the Inlay animation: {error:?}"))?;
    let total: u32 = frames
        .iter()
        .map(|(_, delay)| *delay)
        .fold(0u32, u32::saturating_add);
    set_last_animation_frame_duration(&mut encoded, total)?;
    Ok(encoded)
}

fn encode_animated_inlay_image(
    id: &str,
    data: &[u8],
    name: &str,
    options: &InlayEncodeOptions,
    source: InlayAnimation,
) -> Result<EncodedInlayImage, String> {
    let decoded = decode_inlay_animation(data, source)?;
    let (source_width, source_height) = {
        let first = &decoded.frames[0].0;
        (first.width(), first.height())
    };
    let (width, height) = animation_canvas(source_width, source_height, options)?;
    let min_delay_ms = if options.animation_max_fps > 0 {
        (1000f64 / f64::from(options.animation_max_fps)).ceil() as u32
    } else {
        0
    };
    let frames = merge_animation_frames(decoded.frames, min_delay_ms);
    let frames: Vec<(RgbaImage, u32)> = frames
        .into_iter()
        .map(|(buffer, delay)| {
            let buffer = if buffer.width() == width && buffer.height() == height {
                buffer
            } else {
                image::imageops::resize(&buffer, width, height, FilterType::Lanczos3)
            };
            (buffer, delay)
        })
        .collect();
    let encoded = if frames.len() == 1 {
        let (buffer, _) = &frames[0];
        webp::Encoder::from_rgba(buffer.as_raw(), width, height)
            .encode(f32::from(options.quality))
            .to_vec()
    } else {
        encode_animated_webp(&frames, width, height, options.quality, decoded.loop_count)?
    };
    // An animation that grows is not worth the quality it loses on the way.
    if encoded.len() >= data.len() {
        return Err("re-encoding the Inlay animation saved nothing".to_owned());
    }
    Ok(EncodedInlayImage {
        metadata: InlayImageMetadata {
            key: id.to_owned(),
            kind: "inlay".to_owned(),
            size: encoded.len() as u64,
            mime: "image/webp".to_owned(),
            name: name.to_owned(),
            ext: "webp".to_owned(),
            inlay_type: "image".to_owned(),
            width: Some(width),
            height: Some(height),
        },
        data: encoded,
    })
}

fn encode_inlay_image(
    id: &str,
    data: &[u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
) -> Result<EncodedInlayImage, String> {
    if id.is_empty() || id.starts_with("assets/") {
        return Err("invalid Inlay image id".to_owned());
    }
    let options = options.unwrap_or_default();
    let Ok(format) = image::guess_format(data) else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    if let Some(source) = inlay_animation(format, data) {
        if options.format != InlayEncodeFormat::Original {
            if let Ok(encoded) = encode_animated_inlay_image(id, data, name, &options, source) {
                return Ok(encoded);
            }
        }
        return Ok(preserved_inlay_image(id, data, name, None));
    }
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Ok(preserved_inlay_image(id, data, name, None));
    }

    let Ok(reader) = ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let Ok(mut decoded) = DynamicImage::from_decoder(decoder) else {
        return Ok(preserved_inlay_image(id, data, name, None));
    };
    decoded.apply_orientation(orientation);
    let width = decoded.width();
    let height = decoded.height();
    if options.format == InlayEncodeFormat::Original {
        return Ok(preserved_inlay_image(id, data, name, Some((width, height))));
    }
    let needs_resize = options.max_dimension > 0 && width.max(height) > options.max_dimension;
    if needs_resize {
        let scale = options.max_dimension as f64 / width.max(height) as f64;
        decoded = decoded.resize(
            (width as f64 * scale).round().max(1.0) as u32,
            (height as f64 * scale).round().max(1.0) as u32,
            FilterType::Lanczos3,
        );
    }
    let rgba = decoded.to_rgba8();
    let (encoded, mime, ext) = match options.format {
        InlayEncodeFormat::Original => unreachable!(),
        InlayEncodeFormat::Png => {
            let mut value = Vec::new();
            DynamicImage::ImageRgba8(rgba.clone())
                .write_to(&mut Cursor::new(&mut value), ImageFormat::Png)
                .map_err(|error| format!("failed to encode PNG Inlay image: {error}"))?;
            (value, "image/png", "png")
        }
        InlayEncodeFormat::Webp
            if options.skip_reencode && format == ImageFormat::WebP && !needs_resize =>
        {
            (data.to_vec(), "image/webp", "webp")
        }
        InlayEncodeFormat::Webp => (
            webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height())
                .encode(options.quality as f32)
                .to_vec(),
            "image/webp",
            "webp",
        ),
    };
    let metadata = InlayImageMetadata {
        key: id.to_owned(),
        kind: "inlay".to_owned(),
        size: encoded.len() as u64,
        mime: mime.to_owned(),
        name: name.to_owned(),
        ext: ext.to_owned(),
        inlay_type: "image".to_owned(),
        width: Some(rgba.width()),
        height: Some(rgba.height()),
    };
    Ok(EncodedInlayImage {
        data: encoded,
        metadata,
    })
}

#[tauri::command(async)]
pub(crate) async fn native_media_encode_inlay_image(
    app: AppHandle,
    id: String,
    data: Vec<u8>,
    name: String,
    options: Option<InlayEncodeOptions>,
) -> Result<ipc::EncodedInlayIpcResult, String> {
    if data.len() > ipc::NATIVE_MEDIA_IPC_CHUNK_BYTES {
        return Err("native Inlay direct encoder input exceeds one IPC chunk".to_owned());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<crate::persistent_store::PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        ipc::encode_direct(
            &app.state::<ipc::NativeMediaIpcState>(),
            &id,
            &data,
            &name,
            options,
        )
    })
    .await
    .map_err(|error| format!("failed to join native Inlay image encoder: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn native_media_write_inlay_image(
    app: AppHandle,
    startup: tauri::State<'_, crate::NativeStartupState>,
    id: String,
    data: Vec<u8>,
    name: String,
    options: Option<InlayEncodeOptions>,
) -> Result<InlayImageMetadata, String> {
    if data.len() > ipc::NATIVE_MEDIA_IPC_CHUNK_BYTES {
        return Err("native Inlay direct writer input exceeds one IPC chunk".to_owned());
    }
    startup.ensure_ready()?;
    let root = crate::app_data_root::resolve(&app)
        .map_err(|error| format!("failed to resolve application data directory: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<crate::persistent_store::PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        ipc::write_direct(
            &app.state::<ipc::NativeMediaIpcState>(),
            &root,
            &id,
            &data,
            &name,
            options,
        )
    })
    .await
    .map_err(|error| format!("failed to join native Inlay image writer: {error}"))?
}

fn parse_range(value: Option<&HeaderValue>, size: u64) -> Option<RequestedRange> {
    let Some(value) = value else {
        return Some(RequestedRange::Full);
    };
    let value = value.to_str().ok()?.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    let (start, requested_end) = if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 || size == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size - 1)
    } else {
        let start = start.parse::<u64>().ok()?;
        if start >= size {
            return None;
        }
        let end = if end.is_empty() {
            size - 1
        } else {
            end.parse::<u64>().ok()?.min(size - 1)
        };
        if end < start {
            return None;
        }
        (start, end)
    };
    let end = requested_end.min(start.saturating_add(MAX_BODY_BYTES - 1));
    Some(RequestedRange::Partial { start, end })
}

fn base_response(status: StatusCode) -> tauri::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, EXPOSED_HEADERS)
}

fn not_found() -> Response<Option<std::io::Take<File>>> {
    base_response(StatusCode::NOT_FOUND).body(None).unwrap()
}

fn prepare_response(root: &Path, request: Request<()>) -> Response<Option<std::io::Take<File>>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return base_response(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(None)
            .unwrap();
    }
    let uri = request.uri().to_string();
    let Some(physical_key) = decode_physical_key(&uri) else {
        return not_found();
    };
    // Open while cleanup is excluded; the opened handle then owns the read
    // lifetime without holding a repository lock during a network transfer.
    let Ok(_guard) = crate::asset_repository::coordinator::lock_repository_mutation() else {
        return base_response(StatusCode::SERVICE_UNAVAILABLE)
            .body(None)
            .unwrap();
    };
    let Some(blob) = resolve_blob(root, &uri, physical_key) else {
        return not_found();
    };
    let Ok(mut file) = crate::trust_boundary::open_regular_source(&blob.payload_path) else {
        return not_found();
    };
    if file.metadata().ok().map(|metadata| metadata.len()) != Some(blob.size) {
        return not_found();
    }
    let validator = blob.validator.clone();
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(&validator)
    {
        return base_response(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, blob.cache_control)
            .body(None)
            .unwrap();
    }

    let range_header = request.headers().get(header::RANGE).filter(|_| {
        request.headers().get(header::IF_RANGE).is_none_or(|value| {
            !validator.starts_with("W/") && value.to_str().ok() == Some(&validator)
        })
    });
    let Some(range) = parse_range(range_header, blob.size) else {
        return base_response(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", blob.size))
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, blob.cache_control)
            .body(None)
            .unwrap();
    };
    let (status, start, end) = match range {
        RequestedRange::Full => (StatusCode::OK, 0, blob.size.saturating_sub(1)),
        RequestedRange::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end),
    };
    let length = if blob.size == 0 { 0 } else { end - start + 1 };
    let mut builder = base_response(status)
        .header(header::CONTENT_TYPE, blob.mime)
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ETAG, validator)
        .header(header::CACHE_CONTROL, blob.cache_control);
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", blob.size),
        );
    }
    if request.method() == Method::HEAD || length == 0 {
        return builder.body(None).unwrap();
    }
    if file.seek(SeekFrom::Start(start)).is_err() {
        return not_found();
    }
    builder.body(Some(file.take(length))).unwrap()
}

// Byte collection exists only in tests, never in the WebView serving path.
#[cfg(test)]
fn respond(root: &Path, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    prepare_response(root, request.map(|_| ())).map(|body| {
        let mut bytes = Vec::new();
        if let Some(mut reader) = body {
            reader.read_to_end(&mut bytes).unwrap();
        }
        bytes
    })
}

pub(crate) mod streaming;

#[cfg(test)]
mod tests;
