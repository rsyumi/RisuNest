//! Read-only media capabilities and file-scoped native refresh addresses.
use crate::{ConnectError, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use risunest_sync_wire::{canonical, validate_hash, Sequence};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub const REFRESH_PATH: &str = "/_risunest_media_refresh/";
pub const ACCESS_LIFETIME_SECONDS: u64 = 3600;
const MAX_TOKEN_BYTES: usize = 8192;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaObject {
    pub hash: String,
    pub size: Sequence,
    pub mime: String,
}
impl MediaObject {
    pub fn validate(&self) -> Result<()> {
        validate_hash(&self.hash).map_err(|_| ConnectError("invalid-media-object"))?;
        if self.mime.is_empty()
            || self.mime.len() > 255
            || !self.mime.bytes().all(|b| (0x20..=0x7e).contains(&b))
        {
            return Err(ConnectError("invalid-media-mime"));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaRequest {
    pub object: MediaObject,
    pub refresh_url: String,
}
impl MediaRequest {
    pub fn validate(&self) -> Result<()> {
        self.object.validate()?;
        let url = url::Url::parse(&self.refresh_url)
            .map_err(|_| ConnectError("invalid-media-refresh"))?;
        if url.scheme() != "http"
            || url.host_str() != Some("127.0.0.1")
            || url.port().is_none_or(|v| v == 0)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ConnectError("invalid-media-refresh"));
        }
        let token = url
            .path()
            .strip_prefix(REFRESH_PATH)
            .ok_or(ConnectError("invalid-media-refresh"))?;
        // The server cannot verify the app's per-process signature. It restricts
        // redirects to a file-refresh route whose encoded object is identical.
        // It never accepts arbitrary localhost paths or an external redirect.
        let (bytes, _) = split_token(token)?;
        let object: MediaObject = decode_value(&bytes)?;
        if object != self.object {
            return Err(ConnectError("media-refresh-object-mismatch"));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaClaims {
    pub library_id: String,
    pub epoch: String,
    pub device_id: String,
    pub request: MediaRequest,
    pub expires: Sequence,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaAccess {
    pub object: MediaObject,
    pub token: String,
    pub valid_for_seconds: Sequence,
}

pub fn generate_key() -> Result<[u8; 32]> {
    let mut key = [0; 32];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| ConnectError("entropy-unavailable"))?;
    Ok(key)
}

pub struct MediaSigner(hmac::Key);
impl MediaSigner {
    pub fn new(key: &[u8]) -> Result<Self> {
        if key.len() != 32 {
            return Err(ConnectError("invalid-media-key"));
        }
        Ok(Self(hmac::Key::new(hmac::HMAC_SHA256, key)))
    }
    pub fn sign_access(&self, claims: &MediaClaims) -> Result<String> {
        claims.request.validate()?;
        self.sign(b"risunest-media-access\0", claims)
    }
    pub fn verify_access(&self, token: &str) -> Result<MediaClaims> {
        let claims: MediaClaims = self.verify(b"risunest-media-access\0", token)?;
        claims.request.validate()?;
        Ok(claims)
    }
    pub fn refresh_url(&self, origin: &str, object: &MediaObject) -> Result<String> {
        object.validate()?;
        let token = self.sign(b"risunest-media-refresh\0", object)?;
        let refresh_url = format!("{origin}{REFRESH_PATH}{token}");
        MediaRequest {
            object: object.clone(),
            refresh_url: refresh_url.clone(),
        }
        .validate()?;
        Ok(refresh_url)
    }
    pub fn verify_refresh(&self, token: &str) -> Result<MediaObject> {
        let object: MediaObject = self.verify(b"risunest-media-refresh\0", token)?;
        object.validate()?;
        Ok(object)
    }
    fn sign<T: Serialize>(&self, domain: &[u8], value: &T) -> Result<String> {
        let bytes =
            canonical::encode(value).map_err(|_| ConnectError("invalid-media-capability"))?;
        let mut context = hmac::Context::with_key(&self.0);
        context.update(domain);
        context.update(&bytes);
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&bytes),
            URL_SAFE_NO_PAD.encode(context.sign().as_ref())
        );
        if token.len() > MAX_TOKEN_BYTES {
            return Err(ConnectError("media-capability-too-large"));
        }
        Ok(token)
    }
    fn verify<T: DeserializeOwned + Serialize>(&self, domain: &[u8], token: &str) -> Result<T> {
        let (bytes, tag) = split_token(token)?;
        let mut message = Vec::with_capacity(domain.len() + bytes.len());
        message.extend_from_slice(domain);
        message.extend_from_slice(&bytes);
        hmac::verify(&self.0, &message, &tag)
            .map_err(|_| ConnectError("invalid-media-capability"))?;
        decode_value(&bytes)
    }
}
fn split_token(token: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(ConnectError("media-capability-too-large"));
    }
    let (body, signature) = token
        .split_once('.')
        .ok_or(ConnectError("invalid-media-capability"))?;
    let body = crate::decode(body, MAX_TOKEN_BYTES)?;
    let signature = crate::decode(signature, 64)?;
    if signature.len() != 32 {
        return Err(ConnectError("invalid-media-capability"));
    }
    Ok((body, signature))
}
fn decode_value<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T> {
    let value = canonical::decode(bytes, MAX_TOKEN_BYTES)
        .map_err(|_| ConnectError("invalid-media-capability"))?;
    if canonical::encode(&value).map_err(|_| ConnectError("invalid-media-capability"))? != bytes {
        return Err(ConnectError("invalid-media-capability"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
