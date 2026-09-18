//! Helpers shared by every adapter: bounded control bodies, streaming a body
//! into the job-owned sink while hashing, Retry-After parsing and the default
//! HTTP status classification. A service that documents a different meaning
//! for a status (403 as throttling, 409 as a precondition) overrides it locally.
use crate::external_storage::{contract::*, http::HttpResponse};
use std::{collections::BTreeMap, pin::Pin};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// JSON/XML pages and heads are read into memory; anything larger is refused.
pub(crate) const MAX_CONTROL_BODY: usize = 4 * 1024 * 1024;

pub(crate) fn error(kind: ErrorKind, status: u16) -> ProviderError {
    ProviderError {
        kind,
        http_status: Some(status),
        retry_at_ms: None,
    }
}
fn io_error(cancel: &Cancellation) -> ProviderError {
    match cancel.check() {
        Err(cancelled) => cancelled,
        Ok(()) => ProviderError::new(ErrorKind::Transient),
    }
}

/// Reads at most `max` bytes. One byte more is `Corrupt`, never a truncation.
pub(crate) async fn read_bounded(
    body: &mut Pin<Box<dyn AsyncRead + Send>>,
    max: usize,
    cancel: &Cancellation,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    body.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| io_error(cancel))?;
    if bytes.len() > max {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(bytes)
}

/// Streams a body into the sink from offset 0 and finishes it with the hash of
/// the received bytes. `expected_length` (a Content-Length or metadata size)
/// must match exactly when present; `max_length` bounds the staging file.
/// Returns the received length and lower-hex SHA-256 after `finish` verified them.
pub(crate) async fn stream_to_sink(
    body: &mut Pin<Box<dyn AsyncRead + Send>>,
    sink: &mut dyn TransferSink,
    expected_length: Option<u64>,
    max_length: u64,
    cancel: &Cancellation,
) -> Result<(u64, String)> {
    use sha2::Digest;
    if expected_length.is_some_and(|length| length > max_length) {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
    let mut writer = sink.open(0, max_length, cancel).await?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    loop {
        let read = body.read(&mut buffer).await.map_err(|_| io_error(cancel))?;
        if read == 0 {
            break;
        }
        received += read as u64;
        if received > max_length {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        digest.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .await
            .map_err(|_| io_error(cancel))?;
    }
    writer.shutdown().await.map_err(|_| io_error(cancel))?;
    drop(writer);
    if expected_length.is_some_and(|length| length != received) {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let hash = hex::encode(digest.finalize());
    sink.finish(received, &hash).await?;
    Ok((received, hash))
}

/// Header names arrive lowercased from the transport.
pub(crate) fn content_length(headers: &BTreeMap<String, String>) -> Result<Option<u64>> {
    headers
        .get("content-length")
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .map_err(|_| ProviderError::new(ErrorKind::Corrupt))
        })
        .transpose()
}

/// Delay-seconds or an HTTP-date, preserving the server's full wait.
pub(crate) fn retry_after_ms(headers: &BTreeMap<String, String>, now_ms: u64) -> Option<u64> {
    let value = headers.get("retry-after")?.trim();
    let at = if let Ok(seconds) = value.parse::<u64>() {
        now_ms.saturating_add(seconds.saturating_mul(1000))
    } else {
        let date = httpdate::parse_http_date(value).ok()?;
        let unix = date.duration_since(std::time::UNIX_EPOCH).ok()?;
        (unix.as_millis().min(u64::MAX as u128) as u64).max(now_ms)
    };
    Some(at)
}

/// Default classification of a non-success status. Redirects and unlisted 4xx
/// mean the endpoint or request shape is wrong for this service, not a retry.
pub(crate) fn classify_status(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    let kind = match status {
        401 | 403 => ErrorKind::Unauthorized,
        404 | 410 => ErrorKind::NotFound,
        409 | 412 => ErrorKind::PreconditionFailed,
        413 => ErrorKind::FileTooLarge,
        416 => ErrorKind::Corrupt,
        429 => ErrorKind::RateLimited,
        507 => ErrorKind::StorageFull,
        408 | 500 | 502 | 503 | 504 => ErrorKind::Transient,
        200..=299 => ErrorKind::Corrupt,
        _ => ErrorKind::Unsupported,
    };
    ProviderError {
        kind,
        http_status: Some(status),
        retry_at_ms: if matches!(status, 429 | 503) {
            retry_after_ms(headers, now_ms)
        } else {
            None
        },
    }
}

pub(crate) fn require_status(response: &HttpResponse, allowed: &[u16], now_ms: u64) -> Result<()> {
    if allowed.contains(&response.status) {
        Ok(())
    } else {
        Err(classify_status(response.status, &response.headers, now_ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::transfer::SpoolSink;

    fn body(bytes: Vec<u8>) -> Pin<Box<dyn AsyncRead + Send>> {
        Box::pin(std::io::Cursor::new(bytes))
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn bounded_read_refuses_one_extra_byte_and_streaming_verifies_length_and_hash() {
        runtime().block_on(async {
            let cancel = Cancellation::default();
            assert_eq!(
                read_bounded(&mut body(vec![1; 8]), 8, &cancel)
                    .await
                    .unwrap(),
                vec![1; 8]
            );
            assert_eq!(
                read_bounded(&mut body(vec![1; 9]), 8, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt
            );
            let directory = tempfile::tempdir().unwrap();
            let bytes = vec![7; 70_000];
            let mut sink = SpoolSink::create(&directory.path().join("ok"), 70_000).unwrap();
            let (length, hash) = stream_to_sink(
                &mut body(bytes.clone()),
                &mut sink,
                Some(70_000),
                70_000,
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(length, 70_000);
            assert_eq!(hash, risunest_sync_wire::hash(&bytes));
            assert!(sink.is_verified());
            let mut short = SpoolSink::create(&directory.path().join("short"), 70_000).unwrap();
            assert_eq!(
                stream_to_sink(
                    &mut body(vec![7; 10]),
                    &mut short,
                    Some(11),
                    70_000,
                    &cancel
                )
                .await
                .unwrap_err()
                .kind,
                ErrorKind::Corrupt
            );
            assert!(!short.is_verified());
            let mut long = SpoolSink::create(&directory.path().join("long"), 4).unwrap();
            assert_eq!(
                stream_to_sink(&mut body(vec![7; 5]), &mut long, None, 4, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt
            );
            let mut declared = SpoolSink::create(&directory.path().join("declared"), 4).unwrap();
            assert_eq!(
                stream_to_sink(&mut body(vec![]), &mut declared, Some(5), 4, &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::FileTooLarge
            );
        });
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates_without_shortening_server_waits() {
        let mut headers = BTreeMap::new();
        assert_eq!(retry_after_ms(&headers, 1_000), None);
        headers.insert("retry-after".into(), "60".into());
        assert_eq!(retry_after_ms(&headers, 1_000), Some(61_000));
        headers.insert("retry-after".into(), "Thu, 01 Jan 1970 00:01:40 GMT".into());
        assert_eq!(retry_after_ms(&headers, 1_000), Some(100_000));
        assert_eq!(retry_after_ms(&headers, 200_000), Some(200_000));
        headers.insert("retry-after".into(), "999999999".into());
        assert_eq!(
            retry_after_ms(&headers, 1_000),
            Some(1_000 + 999_999_999_000)
        );
        headers.insert("retry-after".into(), "172800".into());
        assert_eq!(retry_after_ms(&headers, 1_000), Some(172_801_000));
        headers.insert("retry-after".into(), "Sat, 03 Jan 1970 00:00:00 GMT".into());
        assert_eq!(retry_after_ms(&headers, 1_000), Some(172_800_000));
        headers.insert("retry-after".into(), u64::MAX.to_string());
        assert_eq!(retry_after_ms(&headers, 1_000), Some(u64::MAX));
        headers.insert("retry-after".into(), "soon".into());
        assert_eq!(retry_after_ms(&headers, 1_000), None);
    }

    #[test]
    fn default_status_classification_keeps_retry_hints_only_for_throttling() {
        let mut headers = BTreeMap::new();
        headers.insert("retry-after".into(), "5".into());
        let cases = [
            (401, ErrorKind::Unauthorized),
            (403, ErrorKind::Unauthorized),
            (404, ErrorKind::NotFound),
            (409, ErrorKind::PreconditionFailed),
            (412, ErrorKind::PreconditionFailed),
            (413, ErrorKind::FileTooLarge),
            (429, ErrorKind::RateLimited),
            (507, ErrorKind::StorageFull),
            (500, ErrorKind::Transient),
            (503, ErrorKind::Transient),
            (302, ErrorKind::Unsupported),
            (400, ErrorKind::Unsupported),
            (204, ErrorKind::Corrupt),
        ];
        for (status, kind) in cases {
            let error = classify_status(status, &headers, 0);
            assert_eq!(error.kind, kind, "{status}");
            assert_eq!(error.http_status, Some(status));
            assert_eq!(
                error.retry_at_ms,
                matches!(status, 429 | 503).then_some(5_000),
                "{status}"
            );
        }
        let mut lengths = BTreeMap::new();
        assert_eq!(content_length(&lengths).unwrap(), None);
        lengths.insert("content-length".into(), " 42 ".into());
        assert_eq!(content_length(&lengths).unwrap(), Some(42));
        lengths.insert("content-length".into(), "-1".into());
        assert!(content_length(&lengths).is_err());
    }
}
