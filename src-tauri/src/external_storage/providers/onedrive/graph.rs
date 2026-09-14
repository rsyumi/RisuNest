//! Microsoft Graph wire shapes: request URLs under the repository root, the
//! response fields this adapter reads and the status meanings Graph documents
//! differently from the shared default classification.

use super::config::{self, Settings};
use crate::external_storage::{
    contract::{Cancellation, ErrorKind, ProviderError, Result, VersionToken},
    http::HttpResponse,
    providers::common,
};
use std::collections::BTreeMap;

/// Fragment size for upload sessions. A multiple of 320 KiB and below the
/// 60 MiB request ceiling; Graph recommends 5-10 MiB for a byte range.
pub(super) const FRAGMENT_BYTES: u64 = 10 * 1024 * 1024;
/// Every byte range of an upload session must be a multiple of this.
pub(super) const FRAGMENT_ALIGNMENT: u64 = 320 * 1024;
/// Above this an object goes through an upload session instead of one PUT.
/// Graph documents 250 MB for a single content PUT but recommends resumable
/// transfers above 10 MiB, so the single request body stays small.
pub(super) const SIMPLE_UPLOAD_MAX_BYTES: u64 = 4 * 1024 * 1024;

const ITEM_FIELDS: &str = "$select=id,name,size,eTag,file,folder";

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Item {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub e_tag: Option<String>,
    #[serde(default)]
    pub file: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    pub folder: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
pub(super) struct ChildrenPage {
    #[serde(default)]
    pub value: Vec<Item>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionState {
    #[serde(default)]
    pub upload_url: Option<String>,
    #[serde(default)]
    pub expiration_date_time: Option<String>,
    #[serde(default)]
    pub next_expected_ranges: Option<Vec<String>>,
}

/// Control responses are small; anything larger than the shared bound is not a
/// Graph payload this adapter understands.
pub(super) async fn json<T: serde::de::DeserializeOwned>(
    response: &mut HttpResponse,
    cancel: &Cancellation,
) -> Result<T> {
    let bytes = common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
    serde_json::from_slice(&bytes).map_err(|_| corrupt())
}

/// Graph reuses the shared meanings except for a locked resource and the
/// bandwidth cap, which are waits rather than permanent failures.
pub(super) fn classify(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    match status {
        423 => ProviderError {
            kind: ErrorKind::Transient,
            http_status: Some(status),
            retry_at_ms: None,
        },
        509 => ProviderError {
            kind: ErrorKind::RateLimited,
            http_status: Some(status),
            retry_at_ms: common::retry_after_ms(headers, now_ms),
        },
        _ => common::classify_status(status, headers, now_ms),
    }
}

pub(super) fn require(response: &HttpResponse, allowed: &[u16], now_ms: u64) -> Result<()> {
    if allowed.contains(&response.status) {
        Ok(())
    } else {
        Err(classify(response.status, &response.headers, now_ms))
    }
}

/// A rejected grant means the stored refresh token can no longer be redeemed.
pub(super) fn classify_token(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    match status {
        400 | 401 | 403 => ProviderError {
            kind: ErrorKind::ReauthRequired,
            http_status: Some(status),
            retry_at_ms: None,
        },
        _ => classify(status, headers, now_ms),
    }
}

fn parse(raw: String) -> Result<url::Url> {
    url::Url::parse(&raw).map_err(|_| ProviderError::new(ErrorKind::Unsupported))
}

fn with_query(mut url: url::Url, query: Option<&str>) -> url::Url {
    url.set_query(query);
    url
}

/// Resolution target of the configured root: an item id, or the app folder
/// alias which Graph creates on first access.
pub(super) fn root_url(settings: &Settings) -> Result<url::Url> {
    let tail = if settings.root_item_id == config::APP_ROOT {
        config::APP_ROOT.to_owned()
    } else {
        format!("items/{}", config::encode_segment(&settings.root_item_id))
    };
    let url = parse(format!(
        "{}/drives/{}/{tail}",
        config::base(&settings.endpoint),
        config::encode_segment(&settings.drive_id)
    ))?;
    Ok(with_query(url, Some(ITEM_FIELDS)))
}

fn root_prefix(settings: &Settings, root_item_id: &str) -> String {
    format!(
        "{}/drives/{}/items/{}",
        config::base(&settings.endpoint),
        config::encode_segment(&settings.drive_id),
        config::encode_segment(root_item_id)
    )
}

/// `.../items/{root}:/{relative path}:{suffix}` path addressing. The closing
/// colon only appears when an action follows the path.
pub(super) fn item_url(
    settings: &Settings,
    root_item_id: &str,
    path: &str,
    suffix: &str,
    query: Option<&str>,
) -> Result<url::Url> {
    config::validate_relative_path(path)?;
    let tail = if suffix.is_empty() {
        String::new()
    } else {
        format!(":{suffix}")
    };
    let url = parse(format!(
        "{}:/{}{tail}",
        root_prefix(settings, root_item_id),
        config::encode_path(path)
    ))?;
    Ok(with_query(url, query))
}

pub(super) fn metadata_url(
    settings: &Settings,
    root_item_id: &str,
    path: &str,
) -> Result<url::Url> {
    item_url(settings, root_item_id, path, "", Some(ITEM_FIELDS))
}

pub(super) fn folder_children_url(
    settings: &Settings,
    root_item_id: &str,
    folder: &str,
    query: &str,
) -> Result<url::Url> {
    item_url(settings, root_item_id, folder, "/children", Some(query))
}

pub(super) fn root_children_url(settings: &Settings, root_item_id: &str) -> Result<url::Url> {
    parse(format!("{}/children", root_prefix(settings, root_item_id)))
}

pub(super) fn listing_query(limit: u16, cursor: Option<&str>) -> String {
    let mut query = format!("$top={limit}&{ITEM_FIELDS}&$orderby=name");
    if let Some(cursor) = cursor {
        query.push_str("&$skiptoken=");
        query.push_str(&config::encode_segment(cursor));
    }
    query
}

/// Keeps only the opaque continuation token, never the full link, and refuses a
/// link that points somewhere other than the configured endpoint.
pub(super) fn skip_token(next_link: &str, settings: &Settings) -> Result<String> {
    let link = url::Url::parse(next_link).map_err(|_| corrupt())?;
    if link.origin() != settings.endpoint.origin() {
        return Err(corrupt());
    }
    link.query_pairs()
        .find(|(name, _)| {
            name.eq_ignore_ascii_case("$skiptoken") || name.eq_ignore_ascii_case("$skipToken")
        })
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .ok_or_else(corrupt)
}

/// One documented redirect hop to a pre-authenticated URL. The caller sends it
/// without any authorization header.
pub(super) fn redirect_target(response: &HttpResponse, from: &url::Url) -> Result<url::Url> {
    let location = response.headers.get("location").ok_or_else(corrupt)?;
    let target = from.join(location).map_err(|_| corrupt())?;
    if !target.username().is_empty() || target.password().is_some() {
        return Err(corrupt());
    }
    Ok(target)
}

pub(super) fn version(item: &Item, headers: &BTreeMap<String, String>) -> Option<VersionToken> {
    item.e_tag
        .clone()
        .or_else(|| headers.get("etag").cloned())
        .filter(|token| !token.is_empty())
        .map(VersionToken)
}

pub(super) fn header_version(headers: &BTreeMap<String, String>) -> Option<VersionToken> {
    headers
        .get("etag")
        .filter(|token| !token.is_empty())
        .cloned()
        .map(VersionToken)
}

/// `expirationDateTime` is an RFC 3339 instant in UTC.
pub(super) fn expires_at_ms(value: Option<&String>) -> Option<u64> {
    let parsed =
        time::OffsetDateTime::parse(value?, &time::format_description::well_known::Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

/// The lowest start of the ranges Graph still misses. `nextExpectedRanges` does
/// not always list every gap, so only the first boundary is treated as confirmed.
pub(super) fn confirmed_offset(ranges: &[String]) -> Option<u64> {
    ranges
        .iter()
        .filter_map(|range| {
            let start = range.split('-').next()?;
            start.trim().parse::<u64>().ok()
        })
        .min()
}

pub(super) fn content_range(start: u64, length: u64, total: u64) -> Result<String> {
    let end = start
        .checked_add(length)
        .filter(|end| *end <= total && length > 0)
        .ok_or_else(corrupt)?
        - 1;
    Ok(format!("bytes {start}-{end}/{total}"))
}

/// Fragment length from a confirmed offset: a whole fragment, or the remainder
/// when the object ends inside it.
pub(super) fn fragment_length(offset: u64, total: u64) -> Result<u64> {
    let remaining = total
        .checked_sub(offset)
        .filter(|left| *left > 0)
        .ok_or_else(corrupt)?;
    Ok(remaining.min(FRAGMENT_BYTES))
}
