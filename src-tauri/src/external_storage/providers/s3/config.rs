//! Connection settings, credential payload and the remote key layout.
use super::{
    profiles::{self, Addressing, Profile},
    sigv4::{host_header, uri_encode, Credentials},
};
use crate::external_storage::{
    auth::SecretVault,
    contract::{
        Collection, ConnectionConfig, ErrorKind, ObjectRole, ProviderError, Result, SecretRef,
    },
};
use zeroize::Zeroize;

pub(crate) const PROVIDER_ID: &str = "s3";
/// R2 documents 1,024 bytes as the maximum object key length; the other
/// presets are not more permissive than that.
const MAX_KEY_BYTES: usize = 1024;
const HEAD_OBJECT: &str = "head";
const HEAD_PREFIX: &str = "heads/";

/// Validated connection, carried as the opaque provider context of the handle.
pub(crate) struct RepositoryContext {
    pub profile: &'static Profile,
    /// Scheme, host and port only. Never a path, query or credential.
    pub origin: url::Url,
    /// Endpoint path the service requires, without a trailing slash.
    pub base_path: String,
    pub bucket: String,
    pub prefix: String,
    pub region: String,
    pub addressing: Addressing,
    pub account: crate::external_storage::quota::AccountKey,
    pub connection_identity: String,
    pub secret: SecretRef,
}

pub(crate) enum Target<'a> {
    Bucket,
    Object(&'a str),
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SecretPayload {
    access_key_id: String,
    secret_access_key: String,
}

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

/// One path segment of a key. The accepted set is also what the Hugging Face
/// gateway documents as legal, so one layout works on every preset.
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 200
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !segment.starts_with('.')
        && !segment.ends_with('.')
}

fn valid_bucket(bucket: &str, addressing: Addressing) -> bool {
    // Virtual-hosted addressing turns the bucket into one DNS label, where a
    // dot would move it to another certificate.
    let allowed = |byte: u8| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || byte == b'-'
            || (byte == b'.' && addressing == Addressing::Path)
    };
    !bucket.is_empty()
        && bucket.len() <= 63
        && bucket.bytes().all(allowed)
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.'])
}

pub(crate) fn role_folder(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => "descriptors",
        ObjectRole::Pack => "packs",
        ObjectRole::Catalog => "catalogs",
        // A published state and a backup bundle share one collection. The
        // authenticated envelope header, not the path, tells them apart.
        ObjectRole::SyncState | ObjectRole::BackupBundle => "snapshots",
        ObjectRole::BackupPoint => "backup-points",
        ObjectRole::Lease => "leases",
    }
}

/// Role folders a cleanup may remove from. Descriptors identify the repository
/// and the head is not a member of any folder, so neither is a target.
pub(crate) fn removable_folder(folder: &str) -> bool {
    [
        ObjectRole::Pack,
        ObjectRole::Catalog,
        ObjectRole::SyncState,
        ObjectRole::BackupPoint,
        ObjectRole::Lease,
    ]
    .iter()
    .any(|role| role_folder(*role) == folder)
}

pub(crate) fn collection_folder(collection: Collection) -> &'static str {
    match collection {
        Collection::Snapshots => role_folder(ObjectRole::SyncState),
        Collection::BackupPoints => role_folder(ObjectRole::BackupPoint),
        Collection::Descriptors => role_folder(ObjectRole::Descriptor),
        Collection::Leases => role_folder(ObjectRole::Lease),
    }
}

pub(crate) fn validate(config: &ConnectionConfig, secret: &SecretRef) -> Result<RepositoryContext> {
    if config.provider != PROVIDER_ID || config.oauth_profile.is_some() {
        return Err(unsupported());
    }
    let profile = profiles::lookup(config.profile.as_deref().ok_or_else(unsupported)?)?;
    if config.account_id.is_empty() || config.account_id.len() > 256 {
        return Err(unsupported());
    }
    for key in config.location.keys() {
        if !matches!(key.as_str(), "bucket" | "prefix" | "region" | "addressing") {
            return Err(unsupported());
        }
    }
    let (origin, base_path) = endpoint(&config.endpoint, profile)?;
    let addressing = match config.location.get("addressing") {
        Some(value) => Addressing::parse(value)?,
        None => profile.default_addressing,
    };
    if profile.path_addressing_only && addressing != Addressing::Path {
        return Err(unsupported());
    }
    let bucket = config.location.get("bucket").ok_or_else(unsupported)?;
    if !valid_bucket(bucket, addressing) {
        return Err(unsupported());
    }
    let prefix = normalize_prefix(config.location.get("prefix").map(String::as_str))?;
    let region = region(config.location.get("region").map(String::as_str), profile)?;
    let connection_identity = format!(
        "{PROVIDER_ID}/{}/{origin_text}{base_path}/{bucket}/{prefix}",
        profile.id,
        origin_text = origin_text(&origin)?
    );
    let account = crate::external_storage::quota::AccountKey::new(PROVIDER_ID, &origin, &config.account_id)?;
    Ok(RepositoryContext {
        profile,
        origin,
        base_path,
        bucket: bucket.clone(),
        prefix,
        region,
        addressing,
        account,
        connection_identity,
        secret: secret.clone(),
    })
}

fn origin_text(origin: &url::Url) -> Result<String> {
    Ok(format!("{}://{}", origin.scheme(), host_header(origin)?))
}

fn endpoint(endpoint: &str, profile: &Profile) -> Result<(url::Url, String)> {
    let parsed = url::Url::parse(endpoint).map_err(|_| unsupported())?;
    let loopback = parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");
    if (parsed.scheme() != "https" && !loopback)
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_none_or(str::is_empty)
    {
        return Err(unsupported());
    }
    let base_path = parsed.path().trim_end_matches('/').to_owned();
    let segments: Vec<&str> = base_path.split('/').skip(1).collect();
    if segments.iter().any(|segment| !valid_segment(segment)) {
        return Err(unsupported());
    }
    if profile.requires_endpoint_path && segments.len() != 1 {
        return Err(unsupported());
    }
    let mut origin = parsed.clone();
    origin.set_path("/");
    Ok((origin, base_path))
}

fn normalize_prefix(prefix: Option<&str>) -> Result<String> {
    let prefix = prefix.unwrap_or_default().trim_matches('/');
    if prefix.is_empty() {
        return Ok(String::new());
    }
    if prefix.len() > 512 || !prefix.split('/').all(valid_segment) {
        return Err(unsupported());
    }
    Ok(prefix.to_owned())
}

fn region(configured: Option<&str>, profile: &Profile) -> Result<String> {
    let region = match (configured, profile.fixed_region) {
        (Some(configured), Some(fixed)) if configured != fixed => return Err(unsupported()),
        (Some(configured), _) => configured.to_owned(),
        (None, Some(fixed)) => fixed.to_owned(),
        (None, None) => return Err(unsupported()),
    };
    if region.is_empty()
        || region.len() > 64
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(unsupported());
    }
    Ok(region)
}

impl RepositoryContext {
    /// Remote key for a locator's permanent object name.
    pub(crate) fn key(&self, object: &str) -> Result<String> {
        if object.is_empty() || !object.split('/').all(valid_segment) {
            return Err(corrupt());
        }
        let key = if self.prefix.is_empty() {
            object.to_owned()
        } else {
            format!("{}/{object}", self.prefix)
        };
        if key.len() > MAX_KEY_BYTES {
            return Err(corrupt());
        }
        Ok(key)
    }

    /// Listing prefix of one role folder, always ending in a separator so a
    /// sibling folder with a shared name prefix cannot leak into the page.
    pub(crate) fn folder_prefix(&self, folder: &str) -> String {
        if self.prefix.is_empty() {
            format!("{folder}/")
        } else {
            format!("{}/{folder}/", self.prefix)
        }
    }

    /// Turns a listed key back into the locator object name, rejecting anything
    /// nested deeper than the role folder.
    pub(crate) fn object_of(&self, folder: &str, key: &str) -> Option<String> {
        let name = key.strip_prefix(&self.folder_prefix(folder))?;
        (valid_segment(name)).then(|| format!("{folder}/{name}"))
    }

    /// Head objects live at reserved names so an ordinary object can never be
    /// replaced by a head write.
    pub(crate) fn head_key(&self, object: &str) -> Result<String> {
        if object != HEAD_OBJECT && !object.starts_with(HEAD_PREFIX) {
            return Err(corrupt());
        }
        self.key(object)
    }

    pub(crate) fn url(&self, target: Target<'_>, query: &[(String, String)]) -> Result<url::Url> {
        let mut url = self.origin.clone();
        let mut path = self.base_path.clone();
        match self.addressing {
            Addressing::Path => {
                path.push('/');
                path.push_str(&uri_encode(&self.bucket, true));
            }
            Addressing::Virtual => {
                let host = format!(
                    "{}.{}",
                    self.bucket,
                    self.origin.host_str().ok_or_else(corrupt)?
                );
                url.set_host(Some(&host)).map_err(|_| corrupt())?;
            }
        }
        if let Target::Object(key) = target {
            path.push('/');
            path.push_str(&uri_encode(key, false));
        }
        if path.is_empty() {
            path.push('/');
        }
        url.set_path(&path);
        if !query.is_empty() {
            let mut pairs: Vec<String> = query
                .iter()
                .map(|(name, value)| {
                    format!("{}={}", uri_encode(name, true), uri_encode(value, true))
                })
                .collect();
            pairs.sort();
            url.set_query(Some(&pairs.join("&")));
        }
        Ok(url)
    }
}

/// Reads the connection's access keys. A missing or unreadable payload is a
/// re-authentication, never a transport failure.
pub(crate) async fn credentials(
    vault: &dyn SecretVault,
    secret: &SecretRef,
) -> Result<Credentials> {
    let bytes = vault.read(secret).await?;
    let reauth = || ProviderError::new(ErrorKind::ReauthRequired);
    let mut payload: SecretPayload = serde_json::from_slice(&bytes.0).map_err(|_| reauth())?;
    let printable = |value: &str, max: usize| {
        !value.is_empty()
            && value.len() <= max
            && value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            && value.trim() == value
    };
    if !printable(&payload.access_key_id, 256) || !printable(&payload.secret_access_key, 1024) {
        payload.access_key_id.zeroize();
        payload.secret_access_key.zeroize();
        return Err(reauth());
    }
    Ok(Credentials {
        access_key_id: std::mem::take(&mut payload.access_key_id),
        secret_access_key: zeroize::Zeroizing::new(std::mem::take(&mut payload.secret_access_key)),
    })
}
