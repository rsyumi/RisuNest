//! Connection settings: validation, endpoint derivation and the stable
//! connection identity another device computes from the same configuration.
use crate::external_storage::contract::{ConnectionConfig, ErrorKind, ProviderError, Result};

pub(super) const PROVIDER_ID: &str = "google_drive";
pub(super) const DEFAULT_ENDPOINT: &str = "https://www.googleapis.com";
pub(super) const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub(super) const AUTHORIZE_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(super) const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
pub(super) const APPDATA_SCOPE: &str = "https://www.googleapis.com/auth/drive.appdata";
/// Documented alias for the root of the application data space.
pub(super) const APP_DATA_FOLDER: &str = "appDataFolder";
/// Drive counts both the key and the value of one property against this limit.
pub(super) const MAX_PROPERTY_BYTES: usize = 124;

pub(super) fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Space {
    Drive,
    AppData,
}
impl Space {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Drive => "drive",
            Self::AppData => APP_DATA_FOLDER,
        }
    }
}

pub(super) struct Settings {
    pub endpoint: String,
    pub loopback: bool,
    pub folder_id: String,
    pub space: Space,
    pub account_id: String,
    pub project_id: String,
    pub client_id: String,
    pub connection_identity: String,
}

/// Client identifiers are registered per platform, so a connection opened on
/// this device refreshes with the identifier its own registration issued.
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

pub(super) fn is_drive_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

impl Settings {
    pub(super) fn parse(config: &ConnectionConfig) -> Result<Self> {
        if config.provider != PROVIDER_ID {
            return Err(unsupported());
        }
        if !matches!(config.profile.as_deref(), None | Some("drive")) {
            return Err(unsupported());
        }
        let endpoint = match config.endpoint.trim_end_matches('/') {
            "" => DEFAULT_ENDPOINT.to_owned(),
            given => given.to_owned(),
        };
        let parsed = url::Url::parse(&endpoint).map_err(|_| unsupported())?;
        let loopback = parsed.host_str() == Some("127.0.0.1");
        let scheme_allowed = parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback);
        if !scheme_allowed
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(unsupported());
        }
        for key in config.location.keys() {
            if !matches!(key.as_str(), "folderId" | "space") {
                return Err(unsupported());
            }
        }
        let space = match config.location.get("space").map(String::as_str) {
            None | Some("drive") => Space::Drive,
            Some(APP_DATA_FOLDER) => Space::AppData,
            Some(_) => return Err(unsupported()),
        };
        let folder_id = config
            .location
            .get("folderId")
            .cloned()
            .ok_or_else(unsupported)?;
        if !is_drive_id(&folder_id) {
            return Err(unsupported());
        }
        // The application data space is addressed by its documented alias; a
        // folder inside it would still be invisible and app-deletable.
        if space == Space::AppData && folder_id != APP_DATA_FOLDER {
            return Err(unsupported());
        }
        if space == Space::Drive && folder_id == APP_DATA_FOLDER {
            return Err(unsupported());
        }
        if config.account_id.is_empty() || config.account_id.len() > 128 {
            return Err(unsupported());
        }
        let oauth = config.oauth_profile.as_ref().ok_or_else(unsupported)?;
        if oauth.project_id.is_empty() || oauth.project_id.len() > 128 {
            return Err(unsupported());
        }
        let client_id = oauth
            .platform_client_ids
            .get(platform_key())
            .cloned()
            .ok_or_else(unsupported)?;
        if client_id.is_empty() || client_id.len() > 512 {
            return Err(unsupported());
        }
        let connection_identity = format!(
            "{PROVIDER_ID}|{endpoint}|{}|{}|{folder_id}",
            space.as_str(),
            config.account_id
        );
        Ok(Self {
            endpoint,
            loopback,
            folder_id,
            space,
            account_id: config.account_id.clone(),
            project_id: oauth.project_id.clone(),
            client_id,
            connection_identity,
        })
    }

    pub(super) fn api(&self, path: &str) -> Result<url::Url> {
        url::Url::parse(&format!("{}/drive/v3{path}", self.endpoint)).map_err(|_| unsupported())
    }
    pub(super) fn upload(&self, path: &str) -> Result<url::Url> {
        url::Url::parse(&format!("{}/upload/drive/v3{path}", self.endpoint))
            .map_err(|_| unsupported())
    }
    /// Tokens only ever reach Google's documented endpoint; the loopback form
    /// exists so the synthetic fixture can answer without a network.
    pub(super) fn token_endpoint(&self) -> Result<url::Url> {
        if self.loopback {
            url::Url::parse(&format!("{}/token", self.endpoint)).map_err(|_| unsupported())
        } else {
            url::Url::parse(TOKEN_ENDPOINT).map_err(|_| unsupported())
        }
    }
    pub(super) fn same_origin(&self, other: &url::Url) -> bool {
        url::Url::parse(&self.endpoint).is_ok_and(|endpoint| {
            endpoint.scheme() == other.scheme()
                && endpoint.host_str() == other.host_str()
                && endpoint.port_or_known_default() == other.port_or_known_default()
        })
    }
}

/// Drive search literals escape a backslash and a single quote. Control
/// characters never appear in an identifier this adapter wrote.
pub(super) fn escape_query_literal(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(value.replace('\\', "\\\\").replace('\'', "\\'"))
}
