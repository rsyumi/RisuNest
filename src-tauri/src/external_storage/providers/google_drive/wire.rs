//! Drive v3 response shapes and the status/reason
//! classification. Response text is only ever inspected for a documented
//! machine reason string; nothing from a body reaches an error or a log.
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
    pub parents: Option<Vec<String>>,
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
    #[serde(default)]
    pub incomplete_search: bool,
}
impl FileList {
    pub fn validate_page(&self, cursor: Option<&str>) -> Result<()> {
        if self.incomplete_search
            || self.next_page_token.as_deref().is_some_and(|token| {
                token.is_empty() || token.len() > 4096 || Some(token) == cursor
            })
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }
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
