use crate::{validate_endpoint, ConnectError, Result, MAX_URL_BYTES};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};

pub const MAX_ENVELOPE_BYTES: usize = MAX_URL_BYTES + 12 + 16;
pub const MAX_ENVELOPE_TEXT: usize = 5499;

/// The key is an address-discovery secret, never a sync or admin credential.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Directory {
    pub base_url: String,
    pub uuid: String,
    pub key: String,
}
impl Directory {
    pub fn validate(&self) -> Result<()> {
        validate_endpoint(&self.base_url, true)?;
        normalized_uuid(&self.uuid)?;
        key_bytes(&self.key)?;
        Ok(())
    }
    pub fn record_url(&self) -> Result<url::Url> {
        self.validate()?;
        validate_endpoint(&self.base_url, true)?
            .join(&format!("endpoints/{}", normalized_uuid(&self.uuid)?))
            .map_err(|_| ConnectError("invalid-directory-url"))
    }
}
pub(crate) fn normalized_uuid(value: &str) -> Result<String> {
    let uuid = uuid::Uuid::parse_str(value).map_err(|_| ConnectError("invalid-directory-uuid"))?;
    let normalized = uuid.to_string();
    if uuid.get_version() != Some(uuid::Version::Random)
        || uuid.get_variant() != uuid::Variant::RFC4122
        || normalized != value.to_ascii_lowercase()
    {
        return Err(ConnectError("invalid-directory-uuid"));
    }
    Ok(normalized)
}
fn key_bytes(value: &str) -> Result<Vec<u8>> {
    let key = crate::decode(value, 43).map_err(|_| ConnectError("invalid-directory-key"))?;
    if key.len() != 32 {
        return Err(ConnectError("invalid-directory-key"));
    }
    Ok(key)
}
fn aead_key(value: &str) -> Result<aead::LessSafeKey> {
    aead::UnboundKey::new(&aead::AES_256_GCM, &key_bytes(value)?)
        .map(aead::LessSafeKey::new)
        .map_err(|_| ConnectError("invalid-directory-key"))
}
pub fn generate_directory(base_url: String) -> Result<Directory> {
    validate_endpoint(&base_url, true)?;
    let mut key = [0; 32];
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| ConnectError("random-unavailable"))?;
    Ok(Directory {
        base_url,
        uuid: uuid::Uuid::new_v4().to_string(),
        key: URL_SAFE_NO_PAD.encode(key),
    })
}
pub fn seal_endpoint(uuid: &str, key: &str, endpoint: &str) -> Result<String> {
    validate_endpoint(endpoint, false)?;
    let aad = normalized_uuid(uuid)?;
    let mut nonce = [0; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| ConnectError("random-unavailable"))?;
    let mut bytes = endpoint.as_bytes().to_vec();
    aead_key(key)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad.as_bytes()),
            &mut bytes,
        )
        .map_err(|_| ConnectError("directory-encryption-failed"))?;
    let mut envelope = nonce.to_vec();
    envelope.extend(bytes);
    Ok(URL_SAFE_NO_PAD.encode(envelope))
}
pub fn open_endpoint(uuid: &str, key: &str, envelope: &str) -> Result<String> {
    let aad = normalized_uuid(uuid)?;
    let mut bytes = crate::decode(envelope, MAX_ENVELOPE_TEXT)
        .map_err(|_| ConnectError("invalid-directory-envelope"))?;
    if bytes.len() < 29 || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(ConnectError("invalid-directory-envelope"));
    }
    let nonce = aead::Nonce::try_assume_unique_for_key(&bytes[..12])
        .map_err(|_| ConnectError("invalid-directory-envelope"))?;
    let plain = aead_key(key)?
        .open_in_place(nonce, aead::Aad::from(aad.as_bytes()), &mut bytes[12..])
        .map_err(|_| ConnectError("directory-authentication-failed"))?;
    let endpoint =
        std::str::from_utf8(plain).map_err(|_| ConnectError("invalid-directory-endpoint"))?;
    validate_endpoint(endpoint, false)?;
    Ok(endpoint.to_owned())
}
