//! Drive v3 response shapes, documented quota weights and the status/reason
//! classification. Response text is only ever inspected for a documented
//! machine reason string; nothing from a body reaches an error or a log.
use super::config::Settings;
use crate::external_storage::{
    contract::*,
    http::HttpResponse,
    providers::common::{self, MAX_CONTROL_BODY},
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Error envelopes stay far below the shared control-body bound.
const MAX_ERROR_BODY: usize = 8 * 1024;

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DriveFile {
    pub id: Option<String>,
    pub size: Option<String>,
    pub version: Option<String>,
    pub sha256_checksum: Option<String>,
    pub mime_type: Option<String>,
    pub trashed: Option<bool>,
    pub app_properties: Option<BTreeMap<String, String>>,
}
impl DriveFile {
    pub(super) fn file_id(&self) -> Result<&str> {
        self.id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))
    }
    pub(super) fn byte_length(&self) -> Result<u64> {
        self.size
            .as_deref()
            .and_then(|size| size.parse::<u64>().ok())
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))
    }
    pub(super) fn version_token(&self) -> Option<VersionToken> {
        self.version
            .as_deref()
            .filter(|version| !version.is_empty())
            .map(|version| VersionToken(version.to_owned()))
    }
    pub(super) fn property(&self, key: &str) -> Option<&str> {
        self.app_properties
            .as_ref()
            .and_then(|properties| properties.get(key))
            .map(String::as_str)
    }
    /// Drive computes this digest over the stored bytes. It is reported as
    /// provider verified only after it matched the digest this adapter knows.
    pub(super) fn checksum(&self, expected: Option<&str>) -> Option<Checksum> {
        let value = self.sha256_checksum.as_deref()?;
        if !crate::trust_boundary::is_lower_hex_256(value) {
            return None;
        }
        Some(Checksum {
            algorithm: "sha256".into(),
            value: value.to_owned(),
            provider_verified: expected == Some(value),
        })
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileList {
    #[serde(default)]
    pub files: Vec<DriveFile>,
    pub next_page_token: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GeneratedIds {
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AboutUser {
    pub permission_id: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct About {
    pub user: Option<AboutUser>,
}

#[derive(Deserialize)]
struct ErrorItem {
    reason: Option<String>,
}
#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    errors: Vec<ErrorItem>,
}
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: Option<ErrorBody>,
}

pub(super) async fn json<T: serde::de::DeserializeOwned>(
    response: &mut HttpResponse,
    cancel: &Cancellation,
) -> Result<T> {
    let bytes = common::read_bounded(&mut response.body, MAX_CONTROL_BODY, cancel).await?;
    serde_json::from_slice(&bytes).map_err(|_| ProviderError::new(ErrorKind::Corrupt))
}

/// The documented machine reason, never the human message.
async fn reason(response: &mut HttpResponse, cancel: &Cancellation) -> Option<String> {
    let bytes = common::read_bounded(&mut response.body, MAX_ERROR_BODY, cancel)
        .await
        .ok()?;
    let envelope: ErrorEnvelope = serde_json::from_slice(&bytes).ok()?;
    envelope
        .error?
        .errors
        .into_iter()
        .find_map(|item| item.reason)
        .filter(|reason| reason.len() <= 64 && reason.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Drive overloads 403 with throttling, storage and permission meanings, so
/// the shared default is narrowed by the documented reason string.
pub(super) async fn classify(
    response: &mut HttpResponse,
    now_ms: u64,
    cancel: &Cancellation,
) -> ProviderError {
    let status = response.status;
    let retry_at_ms = common::retry_after_ms(&response.headers, now_ms);
    let mut error = common::classify_status(status, &response.headers, now_ms);
    if status == 403 {
        let kind = match reason(response, cancel).await.as_deref() {
            Some("userRateLimitExceeded" | "rateLimitExceeded" | "sharingRateLimitExceeded") => {
                error.retry_at_ms = retry_at_ms;
                ErrorKind::RateLimited
            }
            Some("dailyLimitExceeded") => {
                error.retry_at_ms = retry_at_ms;
                ErrorKind::DailyQuotaExhausted
            }
            Some("storageQuotaExceeded") => ErrorKind::StorageFull,
            _ => ErrorKind::Unauthorized,
        };
        error.kind = kind;
    }
    error
}

pub(super) async fn require_status(
    response: &mut HttpResponse,
    allowed: &[u16],
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<()> {
    if allowed.contains(&response.status) {
        Ok(())
    } else {
        Err(classify(response, now_ms, cancel).await)
    }
}

pub(super) const BUCKET_QUERIES: &str = "queries";
pub(super) const BUCKET_UPLOAD_BYTES: &str = "uploadBytes";
pub(super) const BUCKET_DOWNLOAD_BYTES: &str = "downloadBytes";
const MINUTE_MS: u64 = 60 * 1000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Quota units are shared per project and user; transfer volume is shared per
/// user for uploads and per project for egress.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct AccountScope {
    pub queries: String,
    pub upload: String,
    pub download: String,
}
impl AccountScope {
    pub(super) fn of(settings: &Settings) -> Self {
        Self {
            queries: format!(
                "google_drive/project+user:{}:{}",
                settings.project_id, settings.account_id
            ),
            upload: format!("google_drive/user:{}", settings.account_id),
            download: format!("google_drive/project:{}", settings.project_id),
        }
    }
}

fn unit(bucket: &str, shared_account: &str, units: u64, window_ms: u64) -> RequestCost {
    RequestCost {
        bucket: bucket.into(),
        shared_account: shared_account.into(),
        units,
        reset: QuotaReset::Rolling { window_ms },
    }
}

/// Documented per-method quota units plus the transfer buckets, which count one
/// unit per byte and are therefore scaled by the bytes this request moves.
pub(super) fn costs(
    operation: ProviderOperation,
    scope: &AccountScope,
    bytes: u64,
) -> Vec<RequestCost> {
    let queries = |units: u64| unit(BUCKET_QUERIES, &scope.queries, units, MINUTE_MS);
    let upload = || unit(BUCKET_UPLOAD_BYTES, &scope.upload, bytes, DAY_MS);
    let download = || unit(BUCKET_DOWNLOAD_BYTES, &scope.download, bytes, DAY_MS);
    match operation {
        ProviderOperation::Metadata => vec![queries(5)],
        ProviderOperation::List => vec![queries(100)],
        ProviderOperation::Get | ProviderOperation::Range => vec![queries(200), download()],
        ProviderOperation::Create => vec![queries(50), upload()],
        ProviderOperation::UploadSession => vec![queries(50)],
        ProviderOperation::UploadChunk => vec![upload()],
        ProviderOperation::ReplaceHead => vec![queries(50), upload()],
        ProviderOperation::ReconcileUpload => vec![queries(5)],
        // No download URL is issued, the final chunk completes an upload, this
        // service has no head compare-and-exchange primitive, and the token
        // endpoint has no documented Drive quota weight.
        ProviderOperation::DownloadUrl
        | ProviderOperation::CompleteUpload
        | ProviderOperation::CompareExchangeHead
        | ProviderOperation::Authenticate => Vec::new(),
    }
}
