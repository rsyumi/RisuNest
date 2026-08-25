use image::imageops::FilterType;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
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
static THUMBNAIL_CACHE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize)]
struct BlobMetadata {
    key: String,
    kind: String,
    size: u64,
    mime: String,
}

struct ResolvedBlob {
    physical_key: String,
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
        physical_key,
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

fn requested_thumbnail(request: &Request<Vec<u8>>) -> Option<Option<u32>> {
    let query = request.uri().query()?;
    let mut thumb = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if name == "thumb" {
            if thumb.is_some() {
                return Some(None);
            }
            thumb = match value.as_ref() {
                "128" => Some(128),
                "256" => Some(256),
                "512" => Some(512),
                _ => return Some(None),
            };
        }
    }
    thumb.map(Some)
}

fn write_thumbnail_temp<T>(
    temp_path: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let result = operation();
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

fn with_thumbnail_cache_lock<T>(operation: impl FnOnce() -> T) -> T {
    let _guard = THUMBNAIL_CACHE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    operation()
}

fn thumbnail_path(root: &Path, blob: &ResolvedBlob, requested_size: u32) -> Option<PathBuf> {
    let elapsed = blob.modified.duration_since(UNIX_EPOCH).ok()?;
    let physical_key_hash = hex::encode(Sha256::digest(blob.physical_key.as_bytes()));
    let mut version_hash = Sha256::new();
    version_hash.update(elapsed.as_secs().to_le_bytes());
    version_hash.update(elapsed.subsec_nanos().to_le_bytes());
    version_hash.update(blob.size.to_le_bytes());
    let cache_dir = root.join("blobstore").join("thumbnails");
    let cache_path = cache_dir.join(format!(
        "{physical_key_hash}-{requested_size}-{}.webp",
        hex::encode(version_hash.finalize())
    ));
    with_thumbnail_cache_lock(|| {
        if cache_path.is_file() {
            return Some(cache_path);
        }

        let source = image::ImageReader::open(&blob.payload_path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()?;
        let rendered = if source.width() > requested_size || source.height() > requested_size {
            source.resize(requested_size, requested_size, FilterType::Lanczos3)
        } else {
            source
        };
        let rgba = rendered.to_rgba8();
        let encoded =
            webp::Encoder::from_rgba(rgba.as_raw(), rgba.width(), rgba.height()).encode(80.0);
        fs::create_dir_all(&cache_dir).ok()?;
        let temp_path = cache_dir.join(format!(
            ".{}.{}.tmp",
            cache_path.file_name()?.to_string_lossy(),
            uuid::Uuid::new_v4()
        ));
        write_thumbnail_temp(&temp_path, || {
            File::create(&temp_path)?.write_all(encoded.as_ref())
        })
        .ok()?;
        match fs::rename(&temp_path, &cache_path) {
            Ok(()) => Some(cache_path),
            Err(_) if cache_path.is_file() => {
                let _ = fs::remove_file(temp_path);
                Some(cache_path)
            }
            Err(_) => {
                let _ = fs::remove_file(temp_path);
                None
            }
        }
    })
}

pub(crate) fn remove_thumbnails(root: &Path, physical_key: &str) -> Result<usize, String> {
    if !valid_physical_key(physical_key) {
        return Err("invalid native media physical key".to_owned());
    }
    let prefix = format!("{}-", hex::encode(Sha256::digest(physical_key.as_bytes())));
    let temp_prefix = format!(".{prefix}");
    with_thumbnail_cache_lock(|| {
        let cache_dir = root.join("blobstore").join("thumbnails");
        let entries = match fs::read_dir(cache_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(format!(
                    "failed to read native media thumbnail cache: {error}"
                ))
            }
        };
        let mut removed = 0;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("failed to read thumbnail entry: {error}"))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let is_cached_webp = name.starts_with(&prefix) && name.ends_with(".webp");
            let is_crash_temp = name.starts_with(&temp_prefix) && name.ends_with(".tmp");
            if !is_cached_webp && !is_crash_temp {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("failed to inspect thumbnail entry: {error}"))?;
            if !file_type.is_file() {
                continue;
            }
            fs::remove_file(entry.path())
                .map_err(|error| format!("failed to remove thumbnail: {error}"))?;
            removed += 1;
        }
        Ok(removed)
    })
}

#[tauri::command(async)]
pub(crate) fn native_media_remove_thumbnails(
    app: AppHandle,
    physical_key: String,
) -> Result<usize, String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))?;
    remove_thumbnails(&root, &physical_key)
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
    let Some(mut blob) = resolve_blob(root, physical_key) else {
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

    match requested_thumbnail(&request) {
        Some(Some(size)) => {
            let Some(path) = thumbnail_path(root, &blob, size) else {
                return not_found();
            };
            let Ok(metadata) = fs::metadata(&path) else {
                return not_found();
            };
            blob.payload_path = path;
            blob.mime = "image/webp".to_owned();
            blob.size = metadata.len();
        }
        Some(None) => return not_found(),
        None => {}
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
