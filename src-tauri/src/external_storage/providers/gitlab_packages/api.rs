//! URL construction, request metadata, response shapes and the GitLab
//! specific status classification.
use super::config::{Credential, Placement, Settings};
use crate::external_storage::{
    contract::*,
    http::{self, HttpRequest},
    providers::common,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use std::{collections::BTreeMap, pin::Pin};
use tokio::io::AsyncRead;

/// Path segments keep the unreserved characters only, so a namespace path is
/// sent as the URL-encoded project identifier GitLab expects.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

const MINUTE_MS: u64 = 60 * 1000;

fn url(settings: &Settings, segments: &[&str], query: &[(&str, &str)]) -> Result<url::Url> {
    let mut text = settings.endpoint.as_str().trim_end_matches('/').to_owned();
    for segment in segments {
        text.push('/');
        text.extend(utf8_percent_encode(segment, SEGMENT));
    }
    let mut url = url::Url::parse(&text).map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
    if !query.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
    }
    Ok(url)
}

pub(super) fn object_url(
    settings: &Settings,
    placement: &Placement,
    query: &[(&str, &str)],
) -> Result<url::Url> {
    url(
        settings,
        &[
            "api",
            "v4",
            "projects",
            &settings.project,
            "packages",
            "generic",
            &placement.package,
            &placement.version,
            &placement.file,
        ],
        query,
    )
}

pub(super) fn packages_url(settings: &Settings, query: &[(&str, &str)]) -> Result<url::Url> {
    url(
        settings,
        &["api", "v4", "projects", &settings.project, "packages"],
        query,
    )
}

pub(super) fn package_files_url(
    settings: &Settings,
    package_id: u64,
    query: &[(&str, &str)],
) -> Result<url::Url> {
    url(
        settings,
        &[
            "api",
            "v4",
            "projects",
            &settings.project,
            "packages",
            &package_id.to_string(),
            "package_files",
        ],
        query,
    )
}

/// One stored file of a package. Removal addresses the numeric file id, which
/// only the package file listing reports.
pub(super) fn package_file_url(
    settings: &Settings,
    package_id: u64,
    file_id: u64,
) -> Result<url::Url> {
    url(
        settings,
        &[
            "api",
            "v4",
            "projects",
            &settings.project,
            "packages",
            &package_id.to_string(),
            "package_files",
            &file_id.to_string(),
        ],
        &[],
    )
}

pub(super) struct Outgoing<'a> {
    pub(super) method: reqwest::Method,
    pub(super) url: url::Url,
    pub(super) operation: ProviderOperation,
    /// Omitted when a documented redirect leaves the instance origin.
    pub(super) credential: Option<&'a Credential>,
    pub(super) body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    pub(super) content_length: Option<u64>,
}

pub(super) fn request(settings: &Settings, outgoing: Outgoing<'_>) -> HttpRequest {
    let mut headers = BTreeMap::new();
    headers.insert("accept".into(), "application/json".into());
    if let Some(credential) = outgoing.credential {
        headers.insert("PRIVATE-TOKEN".into(), credential.token.to_string());
    }
    if http::control_operation(outgoing.operation) {
        http::bypass_cache(&mut headers);
    }
    let api_request = outgoing.url.origin() == settings.endpoint.origin();
    HttpRequest {
        method: outgoing.method,
        url: outgoing.url,
        headers,
        body: outgoing.body,
        content_length: outgoing.content_length,
        operation: outgoing.operation,
        account: settings.account.clone(),
        api_request,
        mybox_charge: None,
        control: http::control_operation(outgoing.operation),
    }
}

fn retry_at(headers: &BTreeMap<String, String>, now_ms: u64) -> Option<u64> {
    common::retry_after_ms(headers, now_ms).or_else(|| {
        let reset = headers.get("ratelimit-reset")?.trim().parse::<u64>().ok()?;
        Some(
            reset
                .saturating_mul(1000)
                .max(now_ms)
                .min(now_ms.saturating_add(24 * MINUTE_MS * 60)),
        )
    })
}

/// GitLab answers an invalid, revoked or expired token with 401 and a token
/// that lacks the required scope or a protected package with 403.
pub(super) fn classify(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    match status {
        401 => ProviderError {
            kind: ErrorKind::ReauthRequired,
            http_status: Some(401),
            retry_at_ms: None,
        },
        403 => ProviderError {
            kind: ErrorKind::Unauthorized,
            http_status: Some(403),
            retry_at_ms: None,
        },
        429 => ProviderError {
            kind: ErrorKind::RateLimited,
            http_status: Some(429),
            retry_at_ms: retry_at(headers, now_ms),
        },
        _ => common::classify_status(status, headers, now_ms),
    }
}

#[derive(Deserialize)]
pub(super) struct PackageJson {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) struct PackageFileJson {
    pub(super) id: u64,
    pub(super) file_name: String,
    pub(super) size: u64,
    pub(super) file_sha256: Option<String>,
}

/// GitLab offset pagination reports the following page index in this header and
/// leaves it empty on the last page.
pub(super) fn next_page(headers: &BTreeMap<String, String>) -> Option<String> {
    let value = headers.get("x-next-page")?.trim();
    let page = value.parse::<u32>().ok()?;
    (page > 0).then(|| page.to_string())
}

/// `<package index>:<page>`. A collection is spread over several packages, so
/// resuming has to name which one the offset page belongs to. The first page of
/// a package sends no `page` parameter, which is what the service expects.
pub(super) fn list_cursor(cursor: Option<&str>, packages: usize) -> Result<(usize, Option<String>)> {
    let Some(cursor) = cursor else {
        return Ok((0, None));
    };
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let (index, page) = cursor.split_once(':').ok_or_else(corrupt)?;
    let index = index.parse::<usize>().map_err(|_| corrupt())?;
    let page = page.parse::<u32>().map_err(|_| corrupt())?;
    if index >= packages || !(1..=100_000).contains(&page) {
        return Err(corrupt());
    }
    Ok((index, (page > 1).then(|| page.to_string())))
}
