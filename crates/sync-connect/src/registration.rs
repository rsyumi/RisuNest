use crate::{ConnectError, Directory, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

pub const REGISTRATION_PREFIX: &str = "risunestlocal://sync-server/register#";
pub const MAX_REGISTRATION_URI_BYTES: usize = 2048;
pub const MAX_URL_BYTES: usize = 4096;

/// Contains bearer credentials. Never implement Debug or include in status/logs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registration {
    pub endpoint: String,
    pub library_id: String,
    pub device_id: String,
    pub token: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "directory_if_present"
    )]
    pub directory: Option<Directory>,
}
fn directory_if_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Directory>, D::Error> {
    Directory::deserialize(deserializer).map(Some)
}

pub fn validate_endpoint(value: &str, allow_loopback: bool) -> Result<Url> {
    if value.is_empty()
        || value.len() > MAX_URL_BYTES
        || value.chars().any(|c| c.is_control() || c.is_whitespace())
        || value.contains('\\')
    {
        return Err(ConnectError("invalid-endpoint"));
    }
    let authority = value
        .split_once("://")
        .ok_or(ConnectError("invalid-endpoint"))?
        .1
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.contains('@') {
        return Err(ConnectError("invalid-endpoint"));
    }
    let mut url = Url::parse(value).map_err(|_| ConnectError("invalid-endpoint"))?;
    if !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConnectError("invalid-endpoint"));
    }
    let loopback = matches!(url.host(), Some(Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(Host::Ipv6(ip)) if ip.is_loopback())
        || url.host_str() == Some("localhost");
    if url.scheme() != "https" && !(allow_loopback && url.scheme() == "http" && loopback) {
        return Err(ConnectError("https-required"));
    }
    url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
    Ok(url)
}
impl Registration {
    pub fn validate(&self) -> Result<Url> {
        risunest_sync_wire::validate_id(&self.library_id)
            .map_err(|_| ConnectError("invalid-id"))?;
        risunest_sync_wire::validate_id(&self.device_id).map_err(|_| ConnectError("invalid-id"))?;
        if self.token.len() != 64 || !self.token.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(ConnectError("invalid-device-token"));
        }
        if let Some(directory) = &self.directory {
            directory.validate()?;
        }
        validate_endpoint(&self.endpoint, true)
    }
    pub fn encode_uri(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| ConnectError("invalid-registration"))?;
        let uri = format!("{REGISTRATION_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
        if uri.len() > MAX_REGISTRATION_URI_BYTES {
            return Err(ConnectError("registration-too-large"));
        }
        Ok(uri)
    }
    pub fn parse_uri(uri: &str) -> Result<Self> {
        if uri.len() > MAX_REGISTRATION_URI_BYTES {
            return Err(ConnectError("registration-too-large"));
        }
        let encoded = uri
            .strip_prefix(REGISTRATION_PREFIX)
            .ok_or(ConnectError("invalid-registration-uri"))?;
        let bytes = crate::decode(encoded, MAX_REGISTRATION_URI_BYTES)?;
        let mut value: Self =
            serde_json::from_slice(&bytes).map_err(|_| ConnectError("invalid-registration"))?;
        value.validate()?;
        if let Some(directory) = &mut value.directory {
            directory.uuid = super::directory::normalized_uuid(&directory.uuid)?;
        }
        Ok(value)
    }
}
