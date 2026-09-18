//! Connection settings for the Microsoft Graph adapter: configuration
//! validation, the stable connection identity and remote name construction
//! under the repository root folder.

use crate::external_storage::contract::{
    Collection, ConnectionConfig, ErrorKind, ObjectRole, ProviderError, RemoteLocator, Result,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

pub(super) const PROVIDER_ID: &str = "onedrive";
/// Microsoft identity platform authority. Deliberately not configurable so a
/// shared or imported connection cannot send a refresh token to another host.
pub(super) const AUTHORITY: &str = "https://login.microsoftonline.com";
/// The app folder is addressed by a special-folder alias, not by an item id.
pub(super) const APP_ROOT: &str = "special/approot";

const LOOPBACK: &str = "127.0.0.1";
const LOCATION_KEYS: &[&str] = &[
    "accountType",
    "tenant",
    "driveId",
    "rootItemId",
    "redirectUri",
];

/// URI `pchar` without `:`. The colon delimits the path-addressed suffix, so it
/// is escaped inside every segment we build.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b'@');

/// Characters OneDrive reserves in item names. The business set is the wider
/// one and is applied to every account type so a name stays portable.
const RESERVED_NAME_CHARS: &[char] = &['/', '\\', '*', '<', '>', '?', ':', '|', '#', '%', '"'];

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountType {
    Personal,
    Business,
    AppFolder,
}

impl AccountType {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "personal" => Ok(Self::Personal),
            "business" => Ok(Self::Business),
            "appFolder" => Ok(Self::AppFolder),
            _ => Err(unsupported()),
        }
    }
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Business => "business",
            Self::AppFolder => "appFolder",
        }
    }
    /// Least privileged delegated scopes for the operations this adapter uses.
    pub(super) fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::AppFolder => &["Files.ReadWrite.AppFolder", "User.Read", "offline_access"],
            _ => &["Files.ReadWrite", "User.Read", "offline_access"],
        }
    }
}

pub(super) fn role_folder(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => "descriptors",
        ObjectRole::Pack => "packs",
        ObjectRole::Catalog => "catalogs",
        // A published state and a backup bundle share one collection. The
        // authenticated envelope header, not the path, tells them apart.
        ObjectRole::SyncState | ObjectRole::BackupBundle => "snapshots",
        ObjectRole::BackupPoint => "points",
        ObjectRole::Lease => "leases",
    }
}

pub(super) fn collection_folder(collection: Collection) -> &'static str {
    match collection {
        Collection::Snapshots => role_folder(ObjectRole::SyncState),
        Collection::BackupPoints => role_folder(ObjectRole::BackupPoint),
        Collection::Descriptors => role_folder(ObjectRole::Descriptor),
        Collection::Leases => role_folder(ObjectRole::Lease),
    }
}

/// The relative path of a locator a cleanup may remove. The head is a root
/// member with no role folder, and descriptors identify the repository, so
/// neither is a target.
pub(super) fn removable_path(locator: &RemoteLocator) -> Result<String> {
    let unsupported = || ProviderError::new(ErrorKind::Unsupported);
    let (folder, name) = locator.object.split_once('/').ok_or_else(unsupported)?;
    let known = [
        ObjectRole::Pack,
        ObjectRole::Catalog,
        ObjectRole::SyncState,
        ObjectRole::BackupPoint,
        ObjectRole::Lease,
    ]
    .iter()
    .any(|role| role_folder(*role) == folder);
    if name.is_empty()
        || name.contains('/')
        || !known
        || locator
            .collection
            .as_deref()
            .is_some_and(|hint| hint != folder)
    {
        return Err(unsupported());
    }
    validate_relative_path(&locator.object).map_err(|_| unsupported())?;
    Ok(locator.object.clone())
}

/// Every folder a freshly created repository owns.
pub(super) const REPOSITORY_FOLDERS: &[&str] =
    &["descriptors", "packs", "catalogs", "snapshots", "points", "leases"];

pub(super) struct Settings {
    pub endpoint: url::Url,
    pub authority: url::Url,
    pub account_type: AccountType,
    pub tenant: String,
    pub drive_id: String,
    pub root_item_id: String,
    pub account_id: String,
    pub client_id: String,
    pub redirect_uri: Option<url::Url>,
    /// Provider, endpoint, account type, tenant, account, drive and root. Two
    /// devices holding the same connection configuration compute the same value.
    pub identity: String,
}

/// Length prefixed join so that no field boundary can be forged by its content.
fn joined(parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts {
        out.push_str(&part.len().to_string());
        out.push(':');
        out.push_str(part);
        out.push('|');
    }
    out
}

fn location<'a>(config: &'a ConnectionConfig, key: &str) -> Result<&'a str> {
    config
        .location
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(unsupported)
}

/// Accepts `https`, plus `http` on the loopback host that the wire fixture uses.
/// The product transport rejects any other plain `http` regardless.
fn service_url(raw: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(raw).map_err(|_| unsupported())?;
    let scheme_allowed = parsed.scheme() == "https"
        || (parsed.scheme() == "http" && parsed.host_str() == Some(LOOPBACK));
    if !scheme_allowed
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.host_str().is_none_or(str::is_empty)
    {
        return Err(unsupported());
    }
    Ok(parsed)
}

fn identifier(value: &str) -> Result<&str> {
    let acceptable = !value.is_empty()
        && value.len() <= 512
        && value
            .chars()
            .all(|c| !c.is_control() && !c.is_whitespace() && c != '/' && c != ':' && c != '?');
    if acceptable {
        Ok(value)
    } else {
        Err(unsupported())
    }
}

fn tenant(value: &str, account_type: AccountType) -> Result<&str> {
    let shaped = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    // A personal Microsoft account never signs in through an organization
    // endpoint, and a work account never through `consumers`.
    let consistent = match account_type {
        AccountType::Personal => value == "consumers" || value == "common",
        AccountType::Business => value != "consumers",
        AccountType::AppFolder => true,
    };
    if shaped && consistent {
        Ok(value)
    } else {
        Err(unsupported())
    }
}

/// Native and mobile redirect targets: a registered https reply URL, a loopback
/// reply URL, or the application's own scheme.
fn redirect_uri(raw: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(raw).map_err(|_| unsupported())?;
    let host = parsed.host_str().unwrap_or_default();
    let scheme_allowed = match parsed.scheme() {
        "https" => true,
        "http" => host == "localhost" || host == LOOPBACK,
        scheme => !scheme.is_empty() && scheme != "file" && scheme != "data",
    };
    if !scheme_allowed
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(unsupported());
    }
    Ok(parsed)
}

fn validate_inner(config: &ConnectionConfig, account_required: bool) -> Result<Settings> {
    if config.provider != PROVIDER_ID {
        return Err(unsupported());
    }
    if config
        .location
        .keys()
        .any(|key| !LOCATION_KEYS.contains(&key.as_str()))
    {
        return Err(unsupported());
    }
    let account_type = AccountType::parse(location(config, "accountType")?)?;
    if config
        .profile
        .as_deref()
        .is_some_and(|profile| profile != account_type.as_str())
    {
        return Err(unsupported());
    }
    let endpoint = service_url(config.endpoint.trim_end_matches('/'))?;
    let loopback = endpoint.host_str() == Some(LOOPBACK);
    // The fixture serves the identity platform from the same loopback origin.
    let authority = if loopback {
        endpoint.clone()
    } else {
        service_url(AUTHORITY)?
    };
    let tenant = tenant(location(config, "tenant")?, account_type)?.to_owned();
    let drive_id = identifier(location(config, "driveId")?)?.to_owned();
    let raw_root = location(config, "rootItemId")?;
    let root_item_id = match account_type {
        AccountType::AppFolder if raw_root == APP_ROOT => APP_ROOT.to_owned(),
        AccountType::AppFolder => return Err(unsupported()),
        _ => identifier(raw_root)?.to_owned(),
    };
    let account_id = if !account_required && config.account_id.is_empty() {
        String::new()
    } else {
        identifier(&config.account_id)?.to_owned()
    };
    let client_id = config
        .oauth_profile
        .as_ref()
        .map(|profile| profile.project_id.as_str())
        .ok_or_else(unsupported)
        .and_then(identifier)?
        .to_owned();
    let redirect_uri = config
        .location
        .get("redirectUri")
        .map(|raw| redirect_uri(raw))
        .transpose()?;
    let identity = format!(
        "{PROVIDER_ID}|{}",
        joined(&[
            base(&endpoint),
            account_type.as_str(),
            &tenant,
            &account_id,
            &drive_id,
            &root_item_id,
        ])
    );
    Ok(Settings {
        endpoint,
        authority,
        account_type,
        tenant,
        drive_id,
        root_item_id,
        account_id,
        client_id,
        redirect_uri,
        identity,
    })
}

pub(super) fn validate(config: &ConnectionConfig) -> Result<Settings> {
    validate_inner(config, true)
}

/// Authorization starts before the signed-in Graph user id is known. All
/// other connection fields remain validated, while the account id alone may be
/// empty until the token exchange resolves it from the authenticated service.
pub(super) fn validate_authorization(config: &ConnectionConfig) -> Result<Settings> {
    validate_inner(config, false)
}

pub(super) fn account_id(value: &str) -> Result<String> {
    Ok(identifier(value)?.to_owned())
}

/// The registered client id for a platform, falling back to the application id
/// when the registration uses one client for every platform.
pub(super) fn platform_client_id(config: &ConnectionConfig, platform: &str) -> Result<String> {
    let profile = config.oauth_profile.as_ref().ok_or_else(unsupported)?;
    let chosen = profile
        .platform_client_ids
        .get(platform)
        .map(String::as_str)
        .unwrap_or(profile.project_id.as_str());
    Ok(identifier(chosen)?.to_owned())
}

pub(super) fn platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "linux"
    }
}

/// A remote name relative to the repository root. Rejects anything OneDrive
/// cannot store so a locator never depends on server side name rewriting.
pub(super) fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > 400 {
        return Err(corrupt());
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() > 4 {
        return Err(corrupt());
    }
    for segment in segments {
        let acceptable = !segment.is_empty()
            && segment.len() <= 255
            && segment != "."
            && segment != ".."
            && !segment.starts_with('~')
            && !segment.ends_with('.')
            && !segment.ends_with(' ')
            && !segment.starts_with(' ')
            && segment
                .chars()
                .all(|c| !c.is_control() && !RESERVED_NAME_CHARS.contains(&c));
        if !acceptable {
            return Err(corrupt());
        }
    }
    Ok(())
}

pub(super) fn object_path(role: ObjectRole, object_id: &str) -> Result<String> {
    let path = format!("{}/{}", role_folder(role), object_id);
    validate_relative_path(&path)?;
    Ok(path)
}

pub(super) fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| utf8_percent_encode(segment, SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

pub(super) fn encode_segment(value: &str) -> String {
    utf8_percent_encode(value, SEGMENT).to_string()
}

/// Service root without its trailing separator. `Url` keeps a `/` path for an
/// origin-only address, which would otherwise double the separator.
pub(super) fn base(url: &url::Url) -> &str {
    url.as_str().trim_end_matches('/')
}
