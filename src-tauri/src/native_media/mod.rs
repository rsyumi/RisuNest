use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use std::{
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

const MAX_BODY_BYTES: u64 = 1024 * 1024;
const EXPOSED_HEADERS: &str = "Accept-Ranges, Content-Length, Content-Range, Content-Type, ETag";
static INLAY_WRITE_LOCK: Mutex<()> = Mutex::new(());

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
    width: u32,
    height: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InlayWriteTransaction {
    id: String,
    suffix: String,
    had_payload: bool,
    had_metadata: bool,
}

struct ResolvedBlob {
    payload_path: PathBuf,
    mime: String,
    size: u64,
    modified: SystemTime,
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
    if let Some(rest) = key.strip_prefix("assets/") {
        return !rest.is_empty() && rest.split('/').all(safe_segment);
    }
    let Some(encoded) = key
        .strip_prefix("blobstore/inlays/")
        .and_then(|value| value.strip_suffix(".bin"))
    else {
        return false;
    };
    !encoded.is_empty()
        && encoded.len() % 2 == 0
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

fn resolve_blob(root: &Path, physical_key: String) -> Option<ResolvedBlob> {
    let (logical_key, expected_kind) = logical_key(&physical_key)?;
    let metadata_path = root
        .join("blobstore")
        .join("metadata")
        .join(format!("{}.json", hex::encode(logical_key.as_bytes())));
    let blob_metadata: BlobMetadata =
        serde_json::from_slice(&fs::read(metadata_path).ok()?).ok()?;
    let payload_path = physical_key
        .split('/')
        .fold(root.to_path_buf(), |path, segment| path.join(segment));
    let file_metadata = fs::metadata(&payload_path).ok()?;
    if !file_metadata.is_file()
        || blob_metadata.key != logical_key
        || blob_metadata.kind != expected_kind
        || blob_metadata.size != file_metadata.len()
        || HeaderValue::from_str(&blob_metadata.mime).is_err()
    {
        return None;
    }
    Some(ResolvedBlob {
        payload_path,
        mime: blob_metadata.mime,
        size: file_metadata.len(),
        modified: file_metadata.modified().ok()?,
    })
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

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
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
        fs::rename(previous_payload, payload_path)?;
    }
    if previous_metadata.exists() {
        remove_file_if_exists(metadata_path)?;
        fs::rename(previous_metadata, metadata_path)?;
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

fn metadata_matches_payload(metadata_path: &Path, payload_path: &Path, id: &str) -> bool {
    let Ok(metadata_bytes) = fs::read(metadata_path) else {
        return false;
    };
    let Ok(metadata) = serde_json::from_slice::<InlayImageMetadata>(&metadata_bytes) else {
        return false;
    };
    let Ok(payload_metadata) = fs::metadata(payload_path) else {
        return false;
    };
    metadata.key == id
        && metadata.kind == "inlay"
        && metadata.inlay_type == "image"
        && metadata.mime == "image/webp"
        && metadata.ext == "webp"
        && metadata.size == payload_metadata.len()
}

fn recover_inlay_transaction(root: &Path, transaction_path: &Path) -> Result<(), String> {
    let transaction: InlayWriteTransaction = serde_json::from_slice(
        &fs::read(transaction_path)
            .map_err(|error| format!("failed to read Inlay transaction: {error}"))?,
    )
    .map_err(|error| format!("failed to decode Inlay transaction: {error}"))?;
    let (payload_path, metadata_path, expected_transaction_path) =
        inlay_paths(root, &transaction.id);
    if expected_transaction_path != transaction_path {
        return Err("Inlay transaction id does not match its path".to_owned());
    }
    let previous_payload = payload_path.with_extension("bin.replace-previous");
    let previous_metadata = metadata_path.with_extension("json.replace-previous");
    let encoded_id = hex::encode(transaction.id.as_bytes());
    let next_payload = payload_path.parent().unwrap().join(format!(
        ".{encoded_id}.{}.bin.replace-next",
        transaction.suffix
    ));
    let next_metadata = metadata_path.parent().unwrap().join(format!(
        ".{encoded_id}.{}.json.replace-next",
        transaction.suffix
    ));

    if !metadata_matches_payload(&metadata_path, &payload_path, &transaction.id) {
        if previous_payload.exists() {
            remove_file_if_exists(&payload_path).map_err(|error| error.to_string())?;
            fs::rename(&previous_payload, &payload_path)
                .map_err(|error| format!("failed to restore prior Inlay payload: {error}"))?;
        } else if !transaction.had_payload {
            remove_file_if_exists(&payload_path).map_err(|error| error.to_string())?;
        }
        if previous_metadata.exists() {
            remove_file_if_exists(&metadata_path).map_err(|error| error.to_string())?;
            fs::rename(&previous_metadata, &metadata_path)
                .map_err(|error| format!("failed to restore prior Inlay metadata: {error}"))?;
        } else if !transaction.had_metadata {
            remove_file_if_exists(&metadata_path).map_err(|error| error.to_string())?;
        }
    }

    remove_file_if_exists(&previous_payload).map_err(|error| error.to_string())?;
    remove_file_if_exists(&previous_metadata).map_err(|error| error.to_string())?;
    remove_file_if_exists(&next_payload).map_err(|error| error.to_string())?;
    remove_file_if_exists(&next_metadata).map_err(|error| error.to_string())?;
    remove_file_if_exists(transaction_path).map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) fn recover_inlay_writes(root: &Path) -> Result<(), String> {
    let transaction_dir = root.join("blobstore/inlay-transactions");
    let entries = match fs::read_dir(&transaction_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("failed to read Inlay transactions: {error}")),
    };
    let _guard = INLAY_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to read Inlay transaction entry: {error}"))?;
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            recover_inlay_transaction(root, &entry.path())?;
        }
    }
    Ok(())
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
        fs::rename(payload_path, &previous_payload)
            .map_err(|error| format!("failed to preserve prior Inlay payload: {error}"))?;
    }
    if metadata_path.exists() {
        if let Err(error) = fs::rename(metadata_path, &previous_metadata) {
            let _ = rollback_inlay_pair(
                payload_path,
                metadata_path,
                &previous_payload,
                &previous_metadata,
            );
            return Err(format!("failed to preserve prior Inlay metadata: {error}"));
        }
    }

    let promotion = fs::rename(next_payload, payload_path)
        .map_err(|error| format!("failed to activate Inlay payload: {error}"))
        .and_then(|()| {
            fs::rename(next_metadata, metadata_path)
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

    let _ = remove_file_if_exists(&previous_payload);
    let _ = remove_file_if_exists(&previous_metadata);
    Ok(())
}

pub(crate) fn write_inlay_image(
    root: &Path,
    id: &str,
    data: &[u8],
    name: &str,
) -> Result<InlayImageMetadata, String> {
    if id.is_empty() || id.starts_with("assets/") {
        return Err("invalid Inlay image id".to_owned());
    }
    let format = image::guess_format(data)
        .map_err(|error| format!("unsupported Inlay image format: {error}"))?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err(format!("unsupported new Inlay image format: {format:?}"));
    }
    if format == ImageFormat::WebP
        && webp::BitstreamFeatures::new(data).is_some_and(|features| features.has_animation())
    {
        return Err("animated WebP Inlay images are unsupported".to_owned());
    }

    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| format!("failed to inspect Inlay image: {error}"))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("failed to decode Inlay image: {error}"))?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("failed to decode Inlay image: {error}"))?;
    decoded.apply_orientation(orientation);
    let rgba = decoded.to_rgba8();
    let encoded = webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height()).encode(85.0);
    let metadata = InlayImageMetadata {
        key: id.to_owned(),
        kind: "inlay".to_owned(),
        size: encoded.len() as u64,
        mime: "image/webp".to_owned(),
        name: name.to_owned(),
        ext: "webp".to_owned(),
        inlay_type: "image".to_owned(),
        width: rgba.width(),
        height: rgba.height(),
    };
    let metadata_bytes = serde_json::to_vec(&metadata)
        .map_err(|error| format!("failed to encode Inlay metadata: {error}"))?;
    let encoded_id = hex::encode(id.as_bytes());
    let payload_dir = root.join("blobstore/inlays");
    let metadata_dir = root.join("blobstore/metadata");
    let transaction_dir = root.join("blobstore/inlay-transactions");
    fs::create_dir_all(&payload_dir)
        .map_err(|error| format!("failed to create Inlay payload directory: {error}"))?;
    fs::create_dir_all(&metadata_dir)
        .map_err(|error| format!("failed to create Inlay metadata directory: {error}"))?;
    fs::create_dir_all(&transaction_dir)
        .map_err(|error| format!("failed to create Inlay transaction directory: {error}"))?;
    let (payload_path, metadata_path, transaction_path) = inlay_paths(root, id);
    let suffix = uuid::Uuid::new_v4().to_string();
    let next_payload = payload_dir.join(format!(".{encoded_id}.{suffix}.bin.replace-next"));
    let next_metadata = metadata_dir.join(format!(".{encoded_id}.{suffix}.json.replace-next"));

    let _guard = INLAY_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if transaction_path.exists() {
        recover_inlay_transaction(root, &transaction_path)?;
    }
    let transaction = InlayWriteTransaction {
        id: id.to_owned(),
        suffix,
        had_payload: payload_path.exists(),
        had_metadata: metadata_path.exists(),
    };
    let transaction_bytes = serde_json::to_vec(&transaction)
        .map_err(|error| format!("failed to encode Inlay transaction: {error}"))?;
    if let Err(error) = write_synced(&transaction_path, &transaction_bytes) {
        return Err(format!("failed to persist Inlay transaction: {error}"));
    }
    if let Err(error) = write_synced(&next_payload, encoded.as_ref()) {
        let _ = recover_inlay_transaction(root, &transaction_path);
        return Err(format!("failed to stage Inlay payload: {error}"));
    }
    if let Err(error) = write_synced(&next_metadata, &metadata_bytes) {
        let _ = recover_inlay_transaction(root, &transaction_path);
        return Err(format!("failed to stage Inlay metadata: {error}"));
    }
    if let Err(error) =
        promote_inlay_pair(&payload_path, &metadata_path, &next_payload, &next_metadata)
    {
        let _ = recover_inlay_transaction(root, &transaction_path);
        return Err(error);
    }
    let _ = recover_inlay_transaction(root, &transaction_path);
    Ok(metadata)
}

#[tauri::command(async)]
pub(crate) async fn native_media_write_inlay_image(
    app: AppHandle,
    id: String,
    data: Vec<u8>,
    name: String,
) -> Result<InlayImageMetadata, String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))?;
    tauri::async_runtime::spawn_blocking(move || write_inlay_image(&root, &id, &data, &name))
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

pub(crate) fn not_found() -> Response<Vec<u8>> {
    base_response(StatusCode::NOT_FOUND)
        .body(Vec::new())
        .unwrap()
}

pub(crate) fn respond(root: &Path, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return base_response(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(Vec::new())
            .unwrap();
    }
    let Some(physical_key) = decode_physical_key(&request.uri().to_string()) else {
        return not_found();
    };
    let Some(blob) = resolve_blob(root, physical_key) else {
        return not_found();
    };
    let validator = etag(blob.modified, blob.size);
    if request
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(&validator)
    {
        return base_response(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Vec::new())
            .unwrap();
    }

    let Some(range) = parse_range(request.headers().get(header::RANGE), blob.size) else {
        return base_response(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{}", blob.size))
            .header(header::ETAG, validator)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Vec::new())
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
        .header(header::CACHE_CONTROL, "no-cache");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", blob.size),
        );
    }
    if request.method() == Method::HEAD || length == 0 {
        return builder.body(Vec::new()).unwrap();
    }
    let Ok(mut file) = File::open(blob.payload_path) else {
        return not_found();
    };
    if file.seek(SeekFrom::Start(start)).is_err() {
        return not_found();
    }
    let mut body = vec![0; length as usize];
    if file.read_exact(&mut body).is_err() {
        return not_found();
    }
    builder.body(body).unwrap()
}

#[cfg(test)]
mod tests;
