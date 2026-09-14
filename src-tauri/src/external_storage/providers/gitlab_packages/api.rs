//! URL construction, per-request quota costs, response shapes and the GitLab
//! specific status classification.
use super::config::{Credential, Placement, Settings};
use crate::external_storage::{contract::*, http::HttpRequest, providers::common};
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

/// Authenticated API traffic is counted per token principal; package registry
/// traffic is additionally throttled per client address of one instance.
const USER_BUCKET: &str = "apiRequests";
const ADDRESS_BUCKET: &str = "packageRegistryRequests";
const USER_SCOPE: &str = "gitlab:user";
const ADDRESS_SCOPE: &str = "gitlab:address";
const MINUTE_MS: u64 = 60 * 1000;

fn registry_path(operation: ProviderOperation) -> bool {
    matches!(
        operation,
        ProviderOperation::Metadata
            | ProviderOperation::Get
            | ProviderOperation::Range
            | ProviderOperation::Create
            | ProviderOperation::CompareExchangeHead
            | ProviderOperation::ReplaceHead
    )
}

/// The trait method has no connection, so it reports the bucket model with the
/// scope prefixes. Dispatched requests carry the resolved account identities.
pub(super) fn cost_model(operation: ProviderOperation) -> Vec<RequestCost> {
    let mut costs = vec![RequestCost {
        bucket: USER_BUCKET.into(),
        shared_account: USER_SCOPE.into(),
        units: 1,
        reset: QuotaReset::Rolling {
            window_ms: MINUTE_MS,
        },
    }];
    if registry_path(operation) {
        costs.push(RequestCost {
            bucket: ADDRESS_BUCKET.into(),
            shared_account: ADDRESS_SCOPE.into(),
            units: 1,
            reset: QuotaReset::Rolling {
                window_ms: MINUTE_MS,
            },
        });
    }
    costs
}

pub(super) fn costs(settings: &Settings, operation: ProviderOperation) -> Vec<RequestCost> {
    let host = settings.endpoint.host_str().unwrap_or_default();
    cost_model(operation)
        .into_iter()
        .map(|mut cost| {
            if cost.bucket == ADDRESS_BUCKET {
                cost.shared_account = format!("{ADDRESS_SCOPE}:{host}");
            } else {
                cost.shared_account = format!("{USER_SCOPE}:{}", settings.account_id);
            }
            cost
        })
        .collect()
}

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
        headers.insert(
            credential.kind.header().to_owned(),
            credential.token.to_string(),
        );
    }
    HttpRequest {
        method: outgoing.method,
        url: outgoing.url,
        headers,
        body: outgoing.body,
        content_length: outgoing.content_length,
        operation: outgoing.operation,
        costs: costs(settings, outgoing.operation),
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

pub(super) fn page_cursor(cursor: Option<&str>) -> Result<Option<String>> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    let page = cursor
        .parse::<u32>()
        .ok()
        .filter(|page| (1..=100_000).contains(page))
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    Ok(Some(page.to_string()))
}
