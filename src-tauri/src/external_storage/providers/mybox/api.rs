//! Response shapes, status meanings and the documented request cost model.
use crate::external_storage::{contract::*, providers::common};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Daily allowance of the download API. The service documents no remaining
/// counter, so the owning ledger holds the plan limit and this consumption.
pub(super) const DAILY_DOWNLOAD: &str = "mybox-download-day";
/// The per-minute allowance is documented per API, so each endpoint family
/// reserves its own rolling bucket.
pub(super) const MINUTE_METADATA: &str = "mybox-metadata-minute";
pub(super) const MINUTE_LIST: &str = "mybox-list-minute";
pub(super) const MINUTE_FOLDER: &str = "mybox-folder-minute";
pub(super) const MINUTE_UPLOAD_URL: &str = "mybox-upload-url-minute";
pub(super) const MINUTE_DOWNLOAD_URL: &str = "mybox-download-url-minute";

const MINUTE_MS: u64 = 60 * 1000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// Service timestamps and the documented search filters are KST.
const KST_OFFSET_MS: u64 = 9 * 60 * 60 * 1000;
/// An issued upload URL is valid for 48 hours and cannot be reused afterwards.
pub(super) const UPLOAD_URL_LIFETIME_MS: u64 = 48 * 60 * 60 * 1000;

pub(super) fn next_daily_reset_ms(now_ms: u64) -> u64 {
    let local = now_ms.saturating_add(KST_OFFSET_MS);
    (local - local % DAY_MS)
        .saturating_add(DAY_MS)
        .saturating_sub(KST_OFFSET_MS)
}

/// One call may charge both the daily download allowance and a per-minute API
/// allowance. Whether the daily counter follows the URL issuance or the body
/// transfer is not documented, so both reserve it.
pub(super) fn costs(account: &str, operation: ProviderOperation, now_ms: u64) -> Vec<RequestCost> {
    let minute = |bucket: &str| RequestCost {
        bucket: bucket.into(),
        shared_account: account.into(),
        units: 1,
        reset: QuotaReset::Rolling {
            window_ms: MINUTE_MS,
        },
    };
    let daily = || RequestCost {
        bucket: DAILY_DOWNLOAD.into(),
        shared_account: account.into(),
        units: 1,
        reset: QuotaReset::At {
            unix_ms: next_daily_reset_ms(now_ms),
        },
    };
    match operation {
        ProviderOperation::Metadata => vec![minute(MINUTE_METADATA)],
        ProviderOperation::List => vec![minute(MINUTE_LIST)],
        ProviderOperation::Create => vec![minute(MINUTE_FOLDER)],
        ProviderOperation::UploadSession | ProviderOperation::ReconcileUpload => {
            vec![minute(MINUTE_UPLOAD_URL)]
        }
        ProviderOperation::DownloadUrl => vec![minute(MINUTE_DOWNLOAD_URL), daily()],
        ProviderOperation::Get | ProviderOperation::Range => vec![daily()],
        // Bodies go to an issued storage URL, which is outside the documented
        // Open API allowances.
        ProviderOperation::UploadChunk
        | ProviderOperation::CompleteUpload
        | ProviderOperation::ReplaceHead
        | ProviderOperation::CompareExchangeHead
        | ProviderOperation::Authenticate => Vec::new(),
    }
}

/// 401 is an unusable token the user has to replace, and 423 marks a locked
/// resource rather than a wrong request. Everything else keeps the shared
/// meanings, including 507 for an exhausted account.
pub(super) fn classify(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    match status {
        401 => ProviderError {
            kind: ErrorKind::ReauthRequired,
            http_status: Some(status),
            retry_at_ms: None,
        },
        423 => common::error(ErrorKind::Transient, status),
        _ => common::classify_status(status, headers, now_ms),
    }
}
/// An issued transfer URL is single use and short lived, so a rejection on the
/// storage domain means a fresh issuance is needed, not a missing file.
pub(super) fn classify_transfer(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    match status {
        401 | 403 | 404 | 410 => common::error(ErrorKind::Transient, status),
        _ => classify(status, headers, now_ms),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Storage {
    pub max_file_bytes: u64,
    pub quota_bytes: u64,
    pub used_bytes: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Resource {
    pub resource_id: String,
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "type")]
    pub kind: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Meta {
    #[serde(default)]
    pub next_cursor: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Listing {
    pub resources: Vec<Resource>,
    #[serde(default)]
    pub response_meta_data: Option<Meta>,
}
impl Listing {
    pub fn cursor(&self) -> Option<&str> {
        self.response_meta_data
            .as_ref()?
            .next_cursor
            .as_deref()
            .filter(|cursor| !cursor.is_empty())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Folder {
    pub resource_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Upload {
    #[serde(default)]
    pub offset: u64,
    pub upload_url: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Download {
    pub download_url: String,
}

pub(super) struct Form {
    pub content_type: String,
    pub prefix: Vec<u8>,
    pub suffix: Vec<u8>,
}
/// The issued upload URL takes the bytes as the `Filedata` part of a
/// `multipart/form-data` POST.
pub(super) fn form(file_name: &str) -> Form {
    let boundary = format!("risunest{}", uuid::Uuid::new_v4().simple());
    Form {
        content_type: format!("multipart/form-data; boundary={boundary}"),
        prefix: format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"Filedata\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes(),
        suffix: format!("\r\n--{boundary}--\r\n").into_bytes(),
    }
}

/// KST wall clock, matching the `modifiedTime` examples of the upload API.
pub(super) fn modified_time(now_ms: u64) -> Result<String> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let offset = time::UtcOffset::from_hms(9, 0, 0).map_err(|_| corrupt())?;
    let moment = time::OffsetDateTime::from_unix_timestamp((now_ms / 1000) as i64)
        .map_err(|_| corrupt())?
        .to_offset(offset);
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+09:00",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    ))
}
