//! Wire level pieces of the GitHub Releases adapter: connection context, URL
//! construction, response shapes, naming and the service specific meaning of a
//! status code. Nothing here performs a request; the provider owns the injected
//! HTTP boundary so every hop reserves account budget before dispatch.
use crate::external_storage::{contract::*, providers::common};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};
use zeroize::Zeroizing;

pub(super) const PROVIDER_ID: &str = "github_releases";
pub(super) const API_VERSION: &str = "2022-11-28";
pub(super) const JSON_ACCEPT: &str = "application/vnd.github+json";
pub(super) const BINARY_ACCEPT: &str = "application/octet-stream";
pub(super) const USER_AGENT: &str = "risunest-external-storage";

/// A release accepts up to 1,000 assets. The adapter closes a release far
/// earlier so one page of the release listing, which embeds every asset of the
/// releases it returns, stays inside the shared control body bound.
pub(super) const MAX_ASSETS_PER_RELEASE: usize = 100;
pub(super) const ASSET_PAGE_SIZE: usize = 100;
pub(super) const RELEASE_PAGE_SIZE: usize = 30;
/// Release listing is page numbered, so locating one tag is a bounded scan.
pub(super) const MAX_RELEASE_SCAN_PAGES: u32 = 20;
/// Assets of one release, at the service limit rather than the adapter's.
pub(super) const MAX_ASSET_SCAN_PAGES: u32 = 10;
/// Bounds the requests a single `list_objects` page may spend skipping over
/// releases that hold no object of the requested role.
pub(super) const MAX_LIST_STEPS: u32 = 64;
/// Each file in a release must stay under 2 GiB.
pub(super) const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024 * 1024 - 1;

pub(super) const PRIMARY_BUCKET: &str = "rest-primary";
pub(super) const POINT_BUCKET: &str = "rest-endpoint-points";
pub(super) const CONTENT_MINUTE_BUCKET: &str = "content-creation-minute";
pub(super) const CONTENT_HOUR_BUCKET: &str = "content-creation-hour";
pub(super) const ASSET_BODY_BUCKET: &str = "asset-body";

const HOUR_MS: u64 = 60 * 60 * 1000;
const MINUTE_MS: u64 = 60 * 1000;
const MAX_RETRY_AT_MS: u64 = 24 * HOUR_MS;

/// Descriptors live in one release so an existing root is provable with a
/// single deterministic tag. Every other role is grouped per job.
pub(super) const DESCRIPTOR_BATCH: &str = "d";
pub(super) const JOB_BATCH_PREFIX: &str = "j";

fn refused() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

/// Connection state of one opened repository. Holds the personal access token,
/// so it carries neither Debug nor Serialize and never reaches a DTO.
pub(super) struct Context {
    pub(super) api: url::Url,
    pub(super) uploads: url::Url,
    pub(super) owner: String,
    pub(super) repo: String,
    pub(super) tag_prefix: String,
    pub(super) account: String,
    pub(super) identity: String,
    token: Zeroizing<String>,
    batches: Mutex<BTreeMap<String, Batch>>,
}

/// Release currently receiving a batch's assets and its observed asset count.
#[derive(Clone, Copy)]
pub(super) struct Batch {
    pub(super) seq: u32,
    pub(super) release: u64,
    pub(super) assets: usize,
}

impl Context {
    pub(super) fn new(config: &ConnectionConfig, token: Zeroizing<String>) -> Result<Self> {
        if config.provider != PROVIDER_ID || config.oauth_profile.is_some() {
            return Err(refused());
        }
        let api = endpoint(&config.endpoint)?;
        let uploads = match config.location.get("uploadEndpoint") {
            Some(value) => endpoint(value)?,
            None => url::Url::parse("https://uploads.github.com").map_err(|_| refused())?,
        };
        let owner = location(config, "owner", 100)?;
        let repo = location(config, "repo", 100)?;
        let tag_prefix = location(config, "tagPrefix", 48)?;
        if !is_safe_name(&config.account_id, 100) {
            return Err(refused());
        }
        let identity = format!(
            "{PROVIDER_ID}|{}|{owner}/{repo}|{tag_prefix}",
            api.origin().ascii_serialization()
        );
        Ok(Self {
            api,
            uploads,
            owner,
            repo,
            tag_prefix,
            account: config.account_id.clone(),
            identity,
            token,
            batches: Mutex::new(BTreeMap::new()),
        })
    }

    pub(super) fn authorization(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("Bearer {}", self.token.as_str()))
    }

    pub(super) fn batches(&self) -> MutexGuard<'_, BTreeMap<String, Batch>> {
        self.batches
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn repository_url(&self, tail: &[&str]) -> Result<url::Url> {
        let mut segments = vec!["repos", self.owner.as_str(), self.repo.as_str()];
        segments.extend_from_slice(tail);
        extend(&self.api, &segments)
    }

    pub(super) fn release_page_url(&self, page: u32) -> Result<url::Url> {
        let mut url = self.repository_url(&["releases"])?;
        url.query_pairs_mut()
            .append_pair("per_page", &RELEASE_PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        Ok(url)
    }

    pub(super) fn asset_page_url(&self, release: u64, page: u32) -> Result<url::Url> {
        let mut url = self.repository_url(&["releases", &release.to_string(), "assets"])?;
        url.query_pairs_mut()
            .append_pair("per_page", &ASSET_PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        Ok(url)
    }

    pub(super) fn asset_url(&self, asset: u64) -> Result<url::Url> {
        self.repository_url(&["releases", "assets", &asset.to_string()])
    }

    /// Built from the configured upload host rather than the `upload_url`
    /// template of the release, so an authenticated body never follows an
    /// origin chosen by a response.
    pub(super) fn upload_url(&self, release: u64, name: &str) -> Result<url::Url> {
        let mut url = extend(
            &self.uploads,
            &[
                "repos",
                self.owner.as_str(),
                self.repo.as_str(),
                "releases",
                &release.to_string(),
                "assets",
            ],
        )?;
        url.query_pairs_mut().append_pair("name", name);
        Ok(url)
    }

    pub(super) fn tag(&self, batch: &str, seq: u32) -> String {
        format!("{}-{batch}-{seq}", self.tag_prefix)
    }

    pub(super) fn descriptor_tag(&self) -> String {
        self.tag(DESCRIPTOR_BATCH, 0)
    }

    /// Releases the adapter itself created under this root.
    pub(super) fn owns_tag(&self, tag: &str) -> bool {
        tag.starts_with(&format!("{}-", self.tag_prefix))
    }

    pub(super) fn holds_collection(&self, tag: &str, collection: Collection) -> bool {
        match collection {
            Collection::Descriptors => tag == self.descriptor_tag(),
            Collection::Snapshots | Collection::BackupPoints => {
                tag.starts_with(&format!("{}-{JOB_BATCH_PREFIX}", self.tag_prefix))
            }
        }
    }

    pub(super) fn locator(&self, tag: &str, release: u64, asset: u64) -> RemoteLocator {
        RemoteLocator {
            connection_identity: self.identity.clone(),
            collection: Some(tag.to_owned()),
            object: format!("{release}/{asset}"),
        }
    }
}

fn location(config: &ConnectionConfig, key: &str, max: usize) -> Result<String> {
    let value = config.location.get(key).ok_or_else(refused)?;
    if !is_safe_name(value, max) {
        return Err(refused());
    }
    Ok(value.clone())
}

/// The product transport refuses plain HTTP; the loopback fixture is the one
/// exception and is accepted here so the same validation covers both.
fn endpoint(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value).map_err(|_| refused())?;
    let loopback = url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
    if (url.scheme() != "https" && !loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none_or(str::is_empty)
    {
        return Err(refused());
    }
    Ok(url)
}

fn extend(base: &url::Url, segments: &[&str]) -> Result<url::Url> {
    let mut url = base.clone();
    {
        let mut path = url.path_segments_mut().map_err(|_| corrupt())?;
        path.pop_if_empty();
        path.extend(segments);
    }
    Ok(url)
}

/// Git ref and asset name safe: the adapter never lets a caller supplied
/// identifier reshape a path, a tag or a query.
pub(super) fn is_safe_name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !value.contains("..")
        && !value.ends_with(".lock")
}

pub(super) fn role_prefix(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => "descriptor",
        ObjectRole::Pack => "pack",
        ObjectRole::Catalog => "catalog",
        ObjectRole::Snapshot => "snapshot",
        ObjectRole::BackupPoint => "point",
    }
}

pub(super) fn collection_prefix(collection: Collection) -> &'static str {
    match collection {
        Collection::Snapshots => "snapshot",
        Collection::BackupPoints => "point",
        Collection::Descriptors => "descriptor",
    }
}

pub(super) fn asset_name(role: ObjectRole, object_id: &str) -> String {
    format!("{}-{object_id}", role_prefix(role))
}

pub(super) fn batch_key(role: ObjectRole, job_id: &str) -> String {
    if role == ObjectRole::Descriptor {
        return DESCRIPTOR_BATCH.to_owned();
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(job_id.as_bytes());
    format!("{JOB_BATCH_PREFIX}{}", hex::encode(&digest[..8]))
}

/// `<release id>/<asset id>`, the permanent identifier another device resolves
/// directly. Canonical decimal only, so a receipt round trips unchanged.
pub(super) fn parse_locator(object: &str) -> Result<(u64, u64)> {
    let (release, asset) = object.split_once('/').ok_or_else(corrupt)?;
    let parse = |value: &str| -> Result<u64> {
        let parsed: u64 = value.parse().map_err(|_| corrupt())?;
        if parsed.to_string() != value {
            return Err(corrupt());
        }
        Ok(parsed)
    };
    Ok((parse(release)?, parse(asset)?))
}

#[derive(Deserialize)]
pub(super) struct RepositoryView {
    pub(super) private: bool,
    #[serde(default)]
    pub(super) permissions: Option<PermissionsView>,
}

#[derive(Deserialize)]
pub(super) struct PermissionsView {
    #[serde(default)]
    pub(super) push: bool,
}

#[derive(Deserialize)]
pub(super) struct ReleaseView {
    pub(super) id: u64,
    #[serde(default)]
    pub(super) tag_name: String,
}

#[derive(Deserialize)]
pub(super) struct AssetView {
    pub(super) id: u64,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) size: u64,
    #[serde(default)]
    pub(super) state: String,
    #[serde(default)]
    pub(super) digest: Option<String>,
}

impl AssetView {
    pub(super) fn uploaded(&self) -> bool {
        self.state == "uploaded"
    }
    /// The service computes this SHA-256 over the stored bytes at upload time.
    /// `None` means the service exposed no digest for this asset.
    pub(super) fn sha256(&self) -> Option<String> {
        let value = self.digest.as_deref()?.strip_prefix("sha256:")?;
        crate::trust_boundary::is_lower_hex_256(value).then(|| value.to_owned())
    }
    pub(super) fn matches(&self, intent: &ObjectIntent) -> bool {
        self.size == intent.byte_length
            && self.sha256().is_none_or(|digest| digest == intent.sha256)
    }
    pub(super) fn checksum(&self) -> Option<Checksum> {
        self.sha256().map(|value| Checksum {
            algorithm: "sha256".into(),
            value,
            provider_verified: true,
        })
    }
}

/// Secret payload of a connection: one fine grained personal access token.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TokenPayload {
    pub(super) token: String,
}

/// GitHub reports both rate limits as 403 or 429: a primary limit leaves
/// `x-ratelimit-remaining` at zero, a secondary limit sends `retry-after`.
/// 401 means the token itself is gone or expired, which only a new token fixes.
pub(super) fn classify(
    status: u16,
    headers: &BTreeMap<String, String>,
    now_ms: u64,
) -> ProviderError {
    if matches!(status, 403 | 429) {
        let exhausted = headers
            .get("x-ratelimit-remaining")
            .is_some_and(|value| value.trim() == "0");
        if exhausted || headers.contains_key("retry-after") {
            return ProviderError {
                kind: ErrorKind::RateLimited,
                http_status: Some(status),
                retry_at_ms: common::retry_after_ms(headers, now_ms)
                    .or_else(|| reset_at_ms(headers, now_ms)),
            };
        }
    }
    match status {
        401 => ProviderError {
            kind: ErrorKind::ReauthRequired,
            http_status: Some(status),
            retry_at_ms: None,
        },
        422 => ProviderError {
            kind: ErrorKind::PreconditionFailed,
            http_status: Some(status),
            retry_at_ms: None,
        },
        _ => common::classify_status(status, headers, now_ms),
    }
}

/// `x-ratelimit-reset` is UTC epoch seconds.
fn reset_at_ms(headers: &BTreeMap<String, String>, now_ms: u64) -> Option<u64> {
    let seconds: u64 = headers.get("x-ratelimit-reset")?.trim().parse().ok()?;
    Some(
        seconds
            .saturating_mul(1000)
            .clamp(now_ms, now_ms.saturating_add(MAX_RETRY_AT_MS)),
    )
}

fn cost(bucket: &str, account: &str, units: u64, reset: &QuotaReset) -> RequestCost {
    RequestCost {
        bucket: bucket.into(),
        shared_account: account.into(),
        units,
        reset: reset.clone(),
    }
}

/// Documented weights: 5,000 REST requests per hour for a personal access
/// token, 900 endpoint points per minute where a read costs one point and a
/// write five, and 80 per minute / 500 per hour content creating requests.
/// The redirected asset body is served by a separate host and is metered on its
/// own bucket rather than folded into the REST limit.
pub(super) fn costs(operation: ProviderOperation, account: &str) -> Vec<RequestCost> {
    let hour = QuotaReset::Rolling { window_ms: HOUR_MS };
    let minute = QuotaReset::Rolling {
        window_ms: MINUTE_MS,
    };
    match operation {
        ProviderOperation::Metadata
        | ProviderOperation::List
        | ProviderOperation::DownloadUrl
        | ProviderOperation::ReconcileUpload => vec![
            cost(PRIMARY_BUCKET, account, 1, &hour),
            cost(POINT_BUCKET, account, 1, &minute),
        ],
        ProviderOperation::Create => vec![
            cost(PRIMARY_BUCKET, account, 1, &hour),
            cost(POINT_BUCKET, account, 5, &minute),
            cost(CONTENT_MINUTE_BUCKET, account, 1, &minute),
            cost(CONTENT_HOUR_BUCKET, account, 1, &hour),
        ],
        ProviderOperation::Get => vec![cost(ASSET_BODY_BUCKET, account, 1, &QuotaReset::Unknown)],
        // Ranges, upload sessions and head writes are never dispatched here.
        ProviderOperation::Range
        | ProviderOperation::UploadSession
        | ProviderOperation::UploadChunk
        | ProviderOperation::CompleteUpload
        | ProviderOperation::CompareExchangeHead
        | ProviderOperation::ReplaceHead
        | ProviderOperation::Authenticate => Vec::new(),
    }
}
