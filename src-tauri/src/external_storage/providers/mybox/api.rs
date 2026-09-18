//! Response shapes, status meanings and the documented request limits.
use crate::external_storage::{
    contract::*,
    providers::common,
    quota_profiles::{MyboxCharge, MyboxCounter, MyboxPlan},
};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Daily allowance of the download API. The service documents no remaining
/// counter, so the owning ledger holds the plan limit and this consumption.
/// The per-minute allowance is documented per API, so each endpoint family
/// reserves its own rolling bucket.
/// Deletion is a documented allowance of its own, listed apart from the
/// remaining APIs even where the two numbers agree.
/// An issued upload URL is valid for 48 hours and cannot be reused afterwards.
pub(super) const UPLOAD_URL_LIFETIME_MS: u64 = 48 * 60 * 60 * 1000;

/// One call may charge both the daily download allowance and a per-minute API
/// allowance. Whether the daily counter follows the URL issuance or the body
/// transfer is not documented, so both reserve it.
pub(super) fn charge(plan: MyboxPlan, operation: ProviderOperation) -> Option<MyboxCharge> {
    let counters = match operation {
        ProviderOperation::Metadata => vec![MyboxCounter::MetadataMinute],
        ProviderOperation::Delete => vec![MyboxCounter::DeleteMinute],
        ProviderOperation::List => vec![MyboxCounter::ListMinute],
        ProviderOperation::Create => vec![MyboxCounter::FolderMinute],
        ProviderOperation::UploadSession | ProviderOperation::ReconcileUpload => {
            vec![MyboxCounter::UploadUrlMinute]
        }
        ProviderOperation::DownloadUrl => {
            vec![MyboxCounter::DownloadUrlMinute, MyboxCounter::DownloadDay]
        }
        ProviderOperation::Get | ProviderOperation::Range => vec![MyboxCounter::DownloadDay],
        // Bodies go to an issued storage URL, which is outside the documented
        // Open API allowances.
        ProviderOperation::UploadChunk
        | ProviderOperation::CompleteUpload
        | ProviderOperation::ReplaceHead
        | ProviderOperation::CompareExchangeHead
        | ProviderOperation::Authenticate => return None,
    };
    Some(MyboxCharge { plan, counters })
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
    pub fn cursor(&self) -> Result<Option<&str>> {
        let metadata = self
            .response_meta_data
            .as_ref()
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        match metadata.next_cursor.as_deref() {
            Some(cursor) if cursor.is_empty() || cursor.len() > 4096 => {
                Err(ProviderError::new(ErrorKind::Corrupt))
            }
            cursor => Ok(cursor),
        }
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
