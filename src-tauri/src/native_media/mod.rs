use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::http::{
    header::{self, HeaderValue},
    Method, Request, Response, StatusCode,
};

const MAX_BODY_BYTES: u64 = 1024 * 1024;
const EXPOSED_HEADERS: &str = "Accept-Ranges, Content-Length, Content-Range, Content-Type, ETag";

#[derive(Deserialize)]
struct BlobMetadata {
    key: String,
    kind: String,
    size: u64,
    mime: String,
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
