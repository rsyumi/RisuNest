//! Connection settings, credential payload and the remote naming layout.
use crate::external_storage::contract::*;
use crate::external_storage::quota::AccountKey;
use base64::Engine as _;
use serde::Deserialize;
use zeroize::Zeroizing;

pub(super) const PROVIDER_ID: &str = "gitlab_packages";
/// GitLab.com publishes a 5 GB per-file limit for generic packages; the
/// instance limit is stored as 5 GiB, so the smaller binary value is used.
const GITLAB_COM_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Every object lives in its own package version. The prefix keeps the version
/// inside the documented version charset even when the token starts with `-`.
const VERSION_PREFIX: &str = "v0-";
const MARKER_SUFFIX: &str = "repository";
const MARKER_VERSION: &str = "v0";
const MARKER_FILE: &str = "repository";
const MAX_ENCODED_ID: usize = 180;
const LOCATION_KEYS: [&str; 3] = ["projectId", "packageName", "maxFileBytes"];

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Profile {
    GitlabCom,
    SelfManaged,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialPayload {
    token: String,
}
/// No Debug, Serialize or Display. The token never leaves this struct except as
/// a request header value.
pub(super) struct Credential {
    pub(super) token: Zeroizing<String>,
}

pub(super) fn credential(bytes: &[u8]) -> Result<Credential> {
    let reauth = || ProviderError::new(ErrorKind::ReauthRequired);
    let mut payload: CredentialPayload = serde_json::from_slice(bytes).map_err(|_| reauth())?;
    let token = Zeroizing::new(std::mem::take(&mut payload.token));
    if token.is_empty() || token.len() > 512 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(reauth());
    }
    Ok(Credential { token })
}

pub(super) fn role_name(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => "descriptor",
        ObjectRole::Pack => "pack",
        ObjectRole::Catalog => "catalog",
        ObjectRole::SyncState => "state",
        ObjectRole::BackupBundle => "bundle",
        ObjectRole::BackupPoint => "point",
        ObjectRole::Lease => "lease",
    }
}
pub(super) fn role_from_name(name: &str) -> Option<ObjectRole> {
    Some(match name {
        "descriptor" => ObjectRole::Descriptor,
        "pack" => ObjectRole::Pack,
        "catalog" => ObjectRole::Catalog,
        "state" => ObjectRole::SyncState,
        "bundle" => ObjectRole::BackupBundle,
        "point" => ObjectRole::BackupPoint,
        "lease" => ObjectRole::Lease,
        _ => return None,
    })
}
/// Every package a collection is spread over. Roles live in separate packages
/// here, so a snapshot listing has to walk both of them; the role named for a
/// package never classifies a listed object, the authenticated envelope header
/// does that.
pub(super) fn collection_roles(collection: Collection) -> &'static [ObjectRole] {
    match collection {
        Collection::Snapshots => &[ObjectRole::SyncState, ObjectRole::BackupBundle],
        Collection::BackupPoints => &[ObjectRole::BackupPoint],
        Collection::Descriptors => &[ObjectRole::Descriptor],
        Collection::Leases => &[ObjectRole::Lease],
    }
}

/// Object identifiers may contain characters GitLab rejects in a version or a
/// file name, so they are carried as base64url without padding: its alphabet is
/// inside both documented charsets and cannot produce the forbidden `..`.
fn encode_id(object_id: &str) -> Result<String> {
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(object_id.as_bytes());
    if token.is_empty() || token.len() > MAX_ENCODED_ID {
        return Err(corrupt());
    }
    Ok(token)
}
fn is_encoded_id(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_ENCODED_ID
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) struct Placement {
    pub(super) package: String,
    pub(super) version: String,
    pub(super) file: String,
}

pub(super) struct Settings {
    pub(super) endpoint: url::Url,
    /// Numeric project id or the plain namespace path; percent-encoded per request.
    pub(super) project: String,
    pub(super) package_base: String,
    pub(super) account: AccountKey,
    pub(super) max_stored_bytes: Option<u64>,
    /// Project and package identity without the instance address, so the marker
    /// stays byte-identical for every device that opens the same root.
    root_binding: String,
    pub(super) connection_identity: String,
}

impl Settings {
    pub(super) fn package(&self, role: ObjectRole) -> String {
        format!("{}.{}", self.package_base, role_name(role))
    }
    /// Deterministic control object proving the root exists.
    pub(super) fn marker(&self) -> Placement {
        Placement {
            package: format!("{}.{MARKER_SUFFIX}", self.package_base),
            version: MARKER_VERSION.into(),
            file: MARKER_FILE.into(),
        }
    }
    pub(super) fn marker_body(&self) -> Vec<u8> {
        format!("{PROVIDER_ID}|{}", self.root_binding).into_bytes()
    }
    pub(super) fn place(&self, role: ObjectRole, object_id: &str) -> Result<Placement> {
        let token = encode_id(object_id)?;
        Ok(Placement {
            package: self.package(role),
            version: format!("{VERSION_PREFIX}{token}"),
            file: format!("{}-{token}", role_name(role)),
        })
    }
    /// A listed version is mapped back to the single file name it must hold.
    pub(super) fn place_version(&self, role: ObjectRole, version: &str) -> Result<Placement> {
        let token = version.strip_prefix(VERSION_PREFIX).ok_or_else(corrupt)?;
        if !is_encoded_id(token) {
            return Err(corrupt());
        }
        Ok(Placement {
            package: self.package(role),
            version: version.to_owned(),
            file: format!("{}-{token}", role_name(role)),
        })
    }
    pub(super) fn locator_object(&self, placement: &Placement) -> String {
        format!(
            "{}/{}/{}",
            placement.package, placement.version, placement.file
        )
    }
    pub(super) fn locator(&self, placement: &Placement) -> RemoteLocator {
        RemoteLocator {
            connection_identity: self.connection_identity.clone(),
            collection: None,
            object: self.locator_object(placement),
        }
    }
    /// Rejects any object string that was not produced for this root.
    pub(super) fn parse_object(&self, object: &str) -> Result<(ObjectRole, Placement)> {
        let mut parts = object.split('/');
        let (package, version, file) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(package), Some(version), Some(file), None) => (package, version, file),
                _ => return Err(corrupt()),
            };
        let suffix = package
            .strip_prefix(&self.package_base)
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or_else(corrupt)?;
        let role = role_from_name(suffix).ok_or_else(corrupt)?;
        let placement = self.place_version(role, version)?;
        if placement.file != file {
            return Err(corrupt());
        }
        Ok((role, placement))
    }
}

fn location<'a>(config: &'a ConnectionConfig, key: &str) -> Result<&'a str> {
    config
        .location
        .get(key)
        .map(String::as_str)
        .ok_or_else(unsupported)
}

fn project(value: &str) -> Result<String> {
    let shaped = !value.is_empty()
        && value.len() <= 255
        && !value.contains("..")
        && !value.starts_with('/')
        && !value.ends_with('/')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'));
    if shaped {
        Ok(value.to_owned())
    } else {
        Err(unsupported())
    }
}

fn package_base(value: &str) -> Result<String> {
    let shaped = !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'));
    if shaped {
        Ok(value.to_owned())
    } else {
        Err(unsupported())
    }
}

pub(super) fn settings(config: &ConnectionConfig) -> Result<Settings> {
    if config.provider != PROVIDER_ID || config.oauth_profile.is_some() {
        return Err(unsupported());
    }
    if config.account_id.is_empty()
        || config.account_id.len() > 200
        || !config
            .account_id
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(unsupported());
    }
    for key in config.location.keys() {
        if !LOCATION_KEYS.contains(&key.as_str()) {
            return Err(unsupported());
        }
    }
    let endpoint = url::Url::parse(&config.endpoint).map_err(|_| unsupported())?;
    let loopback = endpoint.host_str() == Some("127.0.0.1");
    let secure = endpoint.scheme() == "https" || (endpoint.scheme() == "http" && loopback);
    if !secure
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(unsupported());
    }
    let profile = match config.profile.as_deref() {
        None => {
            if endpoint.host_str() == Some("gitlab.com") {
                Profile::GitlabCom
            } else {
                Profile::SelfManaged
            }
        }
        Some("gitlabCom") => Profile::GitlabCom,
        Some("selfManaged") => Profile::SelfManaged,
        Some(_) => return Err(unsupported()),
    };
    let project = project(location(config, "projectId")?)?;
    let package_base = package_base(location(config, "packageName")?)?;
    let configured_max = config
        .location
        .get("maxFileBytes")
        .map(|value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|bytes| *bytes > 0)
                .ok_or_else(unsupported)
        })
        .transpose()?;
    let profile_max = match profile {
        Profile::GitlabCom => Some(GITLAB_COM_MAX_FILE_BYTES),
        Profile::SelfManaged => None,
    };
    let max_stored_bytes = match (profile_max, configured_max) {
        (Some(limit), Some(configured)) => Some(limit.min(configured)),
        (limit, None) => limit,
        (None, configured) => configured,
    };
    let origin = endpoint.as_str().trim_end_matches('/').to_owned();
    let project_key = if project.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("id:{project}")
    } else {
        format!("path:{project}")
    };
    let root_binding = format!("{project_key}|package:{package_base}");
    let connection_identity = format!("{PROVIDER_ID}|{origin}|{root_binding}");
    let account = AccountKey::new(PROVIDER_ID, &endpoint, &config.account_id)?;
    Ok(Settings {
        endpoint,
        project,
        package_base,
        account,
        max_stored_bytes,
        root_binding,
        connection_identity,
    })
}
