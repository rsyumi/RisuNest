//! Connection configuration, personal access token payload and remote naming.
use crate::external_storage::{
    auth::SecretVault,
    contract::*,
    quota_profiles::MyboxPlan,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use zeroize::Zeroizing;

pub(super) const PROVIDER_ID: &str = "mybox";

pub(super) const BACKUPS: &str = "backups";
pub(super) const CATALOGS: &str = "catalogs";
pub(super) const DESCRIPTORS: &str = "descriptors";
pub(super) const HEADS: &str = "heads";
pub(super) const LEASES: &str = "leases";
pub(super) const PACKS: &str = "packs";
pub(super) const SNAPSHOTS: &str = "snapshots";
/// Sorted so the repository root listing can be compared against it directly.
pub(super) const FOLDERS: [&str; 7] =
    [BACKUPS, CATALOGS, DESCRIPTORS, HEADS, LEASES, PACKS, SNAPSHOTS];

/// Remote names stay inside a charset that needs no quoting in a multipart
/// header and no escaping in a locator. `%` is reserved for the encoding below.
const NAME_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'.').remove(b'-').remove(b'_');
const NAME_MAX: usize = 255;
const SUFFIX: &str = ".bin";

pub(super) fn role_folder(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => DESCRIPTORS,
        ObjectRole::Pack => PACKS,
        ObjectRole::Catalog => CATALOGS,
        // A published state and a backup bundle share one collection. The
        // authenticated envelope header, not the path, tells them apart.
        ObjectRole::SyncState | ObjectRole::BackupBundle => SNAPSHOTS,
        ObjectRole::BackupPoint => BACKUPS,
        ObjectRole::Lease => LEASES,
    }
}
pub(super) fn collection_folder(collection: Collection) -> &'static str {
    match collection {
        Collection::Snapshots => SNAPSHOTS,
        Collection::BackupPoints => BACKUPS,
        Collection::Descriptors => DESCRIPTORS,
        Collection::Leases => LEASES,
    }
}

/// An explicit extension keeps the stored name exactly what the adapter asked
/// for, so a locator resolves back to the same file on any device.
pub(super) fn object_name(object_id: &str) -> Result<String> {
    let name = format!("{}{SUFFIX}", utf8_percent_encode(object_id, NAME_SET));
    if valid_name(&name) {
        Ok(name)
    } else {
        Err(ProviderError::new(ErrorKind::Corrupt))
    }
}
pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'%'))
}
fn valid_resource_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'~'))
}

pub(super) fn locator(identity: &str, folder: &str, name: &str) -> RemoteLocator {
    RemoteLocator {
        connection_identity: identity.into(),
        collection: Some(folder.into()),
        object: format!("{folder}/{name}"),
    }
}
/// Role folders a cleanup may remove from. Descriptors identify the repository
/// and the head folder holds the one mutable object, so neither is a target.
pub(super) fn removable_folder(folder: &str) -> bool {
    !matches!(folder, DESCRIPTORS | HEADS) && FOLDERS.contains(&folder)
}

/// `<role folder>/<file name>` relative to the repository root. The optional
/// collection is a redundant hint and must agree when it is present.
pub(super) fn parse_locator(locator: &RemoteLocator) -> Result<(&'static str, &str)> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let (folder, name) = locator.object.split_once('/').ok_or_else(corrupt)?;
    let folder = FOLDERS
        .iter()
        .find(|known| **known == folder)
        .ok_or_else(corrupt)?;
    if !valid_name(name)
        || locator
            .collection
            .as_deref()
            .is_some_and(|hint| hint != *folder)
    {
        return Err(corrupt());
    }
    Ok((folder, name))
}

pub(super) struct Connection {
    pub identity: String,
    pub account_id: String,
    pub plan: MyboxPlan,
    pub base: url::Url,
    pub root_folder_name: String,
    pub root_folder_id: Option<String>,
}

pub(super) fn parse(config: &ConnectionConfig) -> Result<Connection> {
    let unsupported = || ProviderError::new(ErrorKind::Unsupported);
    if config.provider != PROVIDER_ID || config.oauth_profile.is_some() {
        return Err(unsupported());
    }
    let plan = MyboxPlan::parse(config.profile.as_deref())?;
    let base = base_url(&config.endpoint)?;
    let account = config.account_id.trim();
    if account.is_empty() || account.len() > 256 || account.chars().any(char::is_control) {
        return Err(unsupported());
    }
    let mut root_folder_name = None;
    let mut root_folder_id = None;
    for (key, value) in &config.location {
        match key.as_str() {
            "rootFolderName" => root_folder_name = Some(value.clone()),
            "rootFolderId" => root_folder_id = Some(value.clone()),
            _ => return Err(unsupported()),
        }
    }
    let root_folder_name = root_folder_name.ok_or_else(unsupported)?;
    if root_folder_name.is_empty()
        || root_folder_name.len() > NAME_MAX
        || root_folder_name.contains('/')
        || root_folder_name.chars().any(char::is_control)
    {
        return Err(unsupported());
    }
    if root_folder_id
        .as_deref()
        .is_some_and(|id| !valid_resource_id(id))
    {
        return Err(unsupported());
    }
    let address = base.as_str().to_owned();
    Ok(Connection {
        identity: format!(
            "{PROVIDER_ID}/{}{}{}",
            part(&address),
            part(account),
            part(&root_folder_name)
        ),
        account_id: account.to_owned(),
        plan,
        base,
        root_folder_name,
        root_folder_id,
    })
}
/// Length prefixes keep an identity unambiguous for names containing separators.
fn part(value: &str) -> String {
    format!("{}:{value}", value.len())
}

/// The product transport refuses plain HTTP as well; `127.0.0.1` is the
/// loopback fixture used by the adapter tests.
fn allowed(url: &url::Url) -> bool {
    (url.scheme() == "https" || (url.scheme() == "http" && url.host_str() == Some("127.0.0.1")))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some()
}
pub(super) fn base_url(endpoint: &str) -> Result<url::Url> {
    let unsupported = || ProviderError::new(ErrorKind::Unsupported);
    if endpoint.len() > 2048 {
        return Err(unsupported());
    }
    let mut url = url::Url::parse(endpoint).map_err(|_| unsupported())?;
    if !allowed(&url) || url.query().is_some() {
        return Err(unsupported());
    }
    url.path_segments_mut()
        .map_err(|_| unsupported())?
        .pop_if_empty();
    Ok(url)
}
/// Upload and download URLs are issued by the service and never stored.
pub(super) fn transfer_url(issued: &str) -> Result<url::Url> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    if issued.len() > 8192 {
        return Err(corrupt());
    }
    let url = url::Url::parse(issued).map_err(|_| corrupt())?;
    if !allowed(&url) {
        return Err(corrupt());
    }
    Ok(url)
}
pub(super) fn endpoint(base: &url::Url, segments: &[&str]) -> Result<url::Url> {
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    Ok(url)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TokenPayload {
    pat: String,
    expires_at_ms: u64,
}
/// MYBOX tokens are user created and never refreshed by the adapter: a stored
/// expiry in the past, a missing secret and a 401 all mean the user must issue
/// a new token, so none of them carries a retry instant.
pub(super) async fn bearer(
    vault: &dyn SecretVault,
    secret: &SecretRef,
    now_ms: u64,
) -> Result<Zeroizing<String>> {
    let reauth = || ProviderError::new(ErrorKind::ReauthRequired);
    let stored = vault.read(secret).await?;
    let payload: TokenPayload =
        serde_json::from_slice(stored.0.as_slice()).map_err(|_| reauth())?;
    let token = Zeroizing::new(payload.pat);
    if token.is_empty()
        || token.len() > 4096
        || !token.bytes().all(|byte| byte.is_ascii_graphic())
        || payload.expires_at_ms <= now_ms
    {
        return Err(reauth());
    }
    Ok(Zeroizing::new(format!("Bearer {}", token.as_str())))
}
