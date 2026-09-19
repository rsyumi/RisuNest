//! Libsodium-compatible secretstream framing. Verify only into staging outputs;
//! callers publish a file after successful FINAL, exact length and EOF checks.
use super::{FormatError, Result};
use dryoc::{
    classic::crypto_secretstream_xchacha20poly1305::*,
    constants::{
        CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_ABYTES as TAG_BYTES,
        CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_FINAL as FINAL,
        CRYPTO_SECRETSTREAM_XCHACHA20POLY1305_TAG_MESSAGE as MESSAGE,
    },
};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use zeroize::Zeroizing;

const MAGIC: &[u8; 4] = b"RNE1";
const FRAME_BYTES: usize = 64 * 1024;
const MAX_BINDING: usize = 16 * 1024;
pub const FIXED_OVERHEAD: u64 = 4 + 8 + 24 + 4 + TAG_BYTES as u64;

pub fn root_key() -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    getrandom::getrandom(key.as_mut()).map_err(|_| FormatError("randomness-unavailable"))?;
    Ok(key)
}
pub fn derive_key(root: &[u8; 32], repository: &str, purpose: &str) -> Result<Zeroizing<[u8; 32]>> {
    if repository.is_empty()
        || repository.len() > 128
        || !matches!(purpose, "data" | "metadata" | "recovery")
    {
        return Err(FormatError("invalid-key-context"));
    }
    let mut key = Zeroizing::new([0; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(repository.as_bytes()), root)
        .expand(
            format!("risunest.external-storage/v1/{purpose}").as_bytes(),
            key.as_mut(),
        )
        .map_err(|_| FormatError("key-derivation-failed"))?;
    Ok(key)
}
fn aad(binding: &[u8], length: u64) -> Result<Vec<u8>> {
    if binding.is_empty() || binding.len() > MAX_BINDING {
        return Err(FormatError("invalid-object-binding"));
    }
    let mut result = Vec::with_capacity(binding.len() + 16);
    result.extend_from_slice(MAGIC);
    result.extend_from_slice(&length.to_le_bytes());
    result.extend_from_slice(&(binding.len() as u32).to_le_bytes());
    result.extend_from_slice(binding);
    Ok(result)
}
pub fn ciphertext_length(length: u64) -> Result<u64> {
    let frames = length.div_ceil(FRAME_BYTES as u64);
    frames
        .checked_mul((4 + TAG_BYTES) as u64)
        .and_then(|n| n.checked_add(length))
        .and_then(|n| n.checked_add(FIXED_OVERHEAD))
        .ok_or(FormatError("length-overflow"))
}
pub fn encrypt(
    input: &mut impl Read,
    output: &mut impl Write,
    key: &[u8; 32],
    binding: &[u8],
    length: u64,
) -> Result<()> {
    let aad = aad(binding, length)?;
    let mut state = State::new();
    let mut header = Header::default();
    crypto_secretstream_xchacha20poly1305_init_push(&mut state, &mut header, key);
    output.write_all(MAGIC)?;
    output.write_all(&length.to_le_bytes())?;
    output.write_all(&header)?;
    let mut remaining = length;
    let mut plaintext = Zeroizing::new(vec![0; FRAME_BYTES]);
    let mut ciphertext = vec![0; FRAME_BYTES + TAG_BYTES];
    while remaining > 0 {
        let count = remaining.min(FRAME_BYTES as u64) as usize;
        input.read_exact(&mut plaintext[..count])?;
        crypto_secretstream_xchacha20poly1305_push(
            &mut state,
            &mut ciphertext[..count + TAG_BYTES],
            &plaintext[..count],
            Some(&aad),
            MESSAGE,
        )
        .map_err(|_| FormatError("encryption-failed"))?;
        output.write_all(&((count + TAG_BYTES) as u32).to_le_bytes())?;
        output.write_all(&ciphertext[..count + TAG_BYTES])?;
        remaining -= count as u64;
    }
    let mut extra = [0];
    if input.read(&mut extra)? != 0 {
        return Err(FormatError("object-length-mismatch"));
    }
    let mut end = vec![0; TAG_BYTES];
    crypto_secretstream_xchacha20poly1305_push(&mut state, &mut end, &[], Some(&aad), FINAL)
        .map_err(|_| FormatError("encryption-failed"))?;
    output.write_all(&(TAG_BYTES as u32).to_le_bytes())?;
    output.write_all(&end)?;
    Ok(())
}
pub fn decrypt(
    input: &mut impl Read,
    staging: &mut impl Write,
    key: &[u8; 32],
    binding: &[u8],
    max_length: u64,
) -> Result<u64> {
    let mut magic = [0; 4];
    input.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(FormatError("invalid-encrypted-object"));
    }
    let mut length = [0; 8];
    input.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length > max_length {
        return Err(FormatError("decoded-limit-exceeded"));
    }
    let aad = aad(binding, length)?;
    let mut header = Header::default();
    input.read_exact(&mut header)?;
    let mut state = State::new();
    crypto_secretstream_xchacha20poly1305_init_pull(&mut state, &header, key);
    let mut total = 0u64;
    loop {
        let mut frame_length = [0; 4];
        input.read_exact(&mut frame_length)?;
        let frame_length = u32::from_le_bytes(frame_length) as usize;
        if !(TAG_BYTES..=FRAME_BYTES + TAG_BYTES).contains(&frame_length) {
            return Err(FormatError("invalid-encrypted-frame"));
        }
        let mut ciphertext = vec![0; frame_length];
        input.read_exact(&mut ciphertext)?;
        let mut plaintext = Zeroizing::new(vec![0; frame_length - TAG_BYTES]);
        let mut tag = 0;
        crypto_secretstream_xchacha20poly1305_pull(
            &mut state,
            &mut plaintext,
            &mut tag,
            &ciphertext,
            Some(&aad),
        )
        .map_err(|_| FormatError("object-authentication-failed"))?;
        if tag == FINAL {
            if !plaintext.is_empty() || total != length {
                return Err(FormatError("object-length-mismatch"));
            }
            let mut extra = [0];
            if input.read(&mut extra)? != 0 {
                return Err(FormatError("trailing-encrypted-bytes"));
            }
            return Ok(total);
        }
        if tag != MESSAGE || plaintext.is_empty() {
            return Err(FormatError("invalid-encrypted-tag"));
        }
        total = total
            .checked_add(plaintext.len() as u64)
            .ok_or(FormatError("length-overflow"))?;
        if total > length {
            return Err(FormatError("object-length-mismatch"));
        }
        staging.write_all(&plaintext)?;
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryBootstrapEnvelope {
    pub schema: String,
    pub repository_id: String,
    pub salt: [u8; 16],
    #[serde(with = "recovery_ciphertext")]
    pub wrapped_key: Vec<u8>,
}

mod recovery_ciphertext {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&URL_SAFE_NO_PAD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > super::MAX_CONNECTION_SETTINGS_BYTES {
            return Err(serde::de::Error::custom("recovery-ciphertext-limit"));
        }
        URL_SAFE_NO_PAD.decode(encoded).map_err(|_| serde::de::Error::custom("invalid-recovery-ciphertext"))
    }
}
const RECOVERY_SCHEMA: &str = "risunest.repository-bootstrap/v1";
const CONNECTION_SETTINGS_SCHEMA: &str = "risunest.connection-settings/v1";
pub const MAX_REPOSITORY_BOOTSTRAP_BYTES: usize = 64 * 1024;
pub const MAX_CONNECTION_SETTINGS_BYTES: usize = 64 * 1024;

/// An app-generated 128-bit secret. Never serialize or debug-print this value.
pub struct RecoveryKey(Zeroizing<[u8; 16]>);
impl RecoveryKey {
    pub fn generate() -> Result<Self> {
        let mut bytes = Zeroizing::new([0; 16]);
        getrandom::getrandom(bytes.as_mut()).map_err(|_| FormatError("randomness-unavailable"))?;
        Ok(Self(bytes))
    }
    /// Ten groups of four hex digits: 128 secret bits plus a 32-bit typo checksum.
    pub fn expose(&self) -> Zeroizing<String> {
        let checksum = super::content_identity::hash(self.0.as_ref());
        let mut raw = Zeroizing::new(hex::encode(self.0.as_ref()));
        raw.push_str(&hex::encode(&checksum[..4]));
        let mut text = Zeroizing::new(String::with_capacity(49));
        for (index, byte) in raw.bytes().enumerate() {
            if index > 0 && index % 4 == 0 {
                text.push('-');
            }
            text.push(char::from(byte));
        }
        text
    }
    pub fn parse(text: &str) -> Result<Self> {
        if text.len() != 49 {
            return Err(FormatError("invalid-recovery-code"));
        }
        let mut digits = Zeroizing::new(String::with_capacity(40));
        for (index, byte) in text.bytes().enumerate() {
            if index % 5 == 4 {
                if byte != b'-' {
                    return Err(FormatError("invalid-recovery-code"));
                }
            } else if byte.is_ascii_hexdigit() {
                digits.push(char::from(byte));
            } else {
                return Err(FormatError("invalid-recovery-code"));
            }
        }
        let mut decoded = Zeroizing::new([0; 20]);
        hex::decode_to_slice(digits.as_bytes(), decoded.as_mut())
            .map_err(|_| FormatError("invalid-recovery-code"))?;
        if super::content_identity::hash(&decoded[..16])[..4] != decoded[16..] {
            return Err(FormatError("invalid-recovery-code"));
        }
        let mut secret = Zeroizing::new([0; 16]);
        secret.copy_from_slice(&decoded[..16]);
        Ok(Self(secret))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionSettingsEnvelope {
    pub schema: String,
    pub repository_id: String,
    pub salt: [u8; 16],
    #[serde(with = "recovery_ciphertext")]
    pub ciphertext: Vec<u8>,
}

impl ConnectionSettingsEnvelope {
    fn binding(&self) -> Result<Vec<u8>> {
        if self.schema != CONNECTION_SETTINGS_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
        {
            return Err(FormatError("invalid-connection-settings"));
        }
        serde_json::to_vec(&(&self.schema, &self.repository_id, self.salt))
            .map_err(|_| FormatError("invalid-connection-settings"))
    }

    fn wrapping_key(&self, code: &RecoveryKey) -> Result<Zeroizing<[u8; 32]>> {
        let mut key = Zeroizing::new([0; 32]);
        hkdf::Hkdf::<sha2::Sha256>::new(Some(&self.salt), code.0.as_ref())
            .expand(&self.binding()?, key.as_mut())
            .map_err(|_| FormatError("key-derivation-failed"))?;
        Ok(key)
    }

    pub fn protect(repository_id: String, plaintext: &[u8], code: &RecoveryKey) -> Result<Self> {
        if plaintext.is_empty() || plaintext.len() > 32 * 1024 {
            return Err(FormatError("invalid-connection-settings"));
        }
        let mut result = Self {
            schema: CONNECTION_SETTINGS_SCHEMA.into(),
            repository_id,
            salt: [0; 16],
            ciphertext: Vec::new(),
        };
        getrandom::getrandom(&mut result.salt)
            .map_err(|_| FormatError("randomness-unavailable"))?;
        let key = result.wrapping_key(code)?;
        let binding = result.binding()?;
        encrypt(
            &mut std::io::Cursor::new(plaintext),
            &mut result.ciphertext,
            &key,
            &binding,
            plaintext.len() as u64,
        )?;
        Ok(result)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.binding()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| FormatError("invalid-connection-settings"))?;
        if bytes.len() > MAX_CONNECTION_SETTINGS_BYTES {
            return Err(FormatError("invalid-connection-settings"));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_CONNECTION_SETTINGS_BYTES {
            return Err(FormatError("invalid-connection-settings"));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| FormatError("invalid-connection-settings"))?;
        value.binding()?;
        if value.ciphertext.len() > 48 * 1024 {
            return Err(FormatError("invalid-connection-settings"));
        }
        Ok(value)
    }

    pub fn open(&self, repository_id: &str, code: &RecoveryKey) -> Result<Zeroizing<Vec<u8>>> {
        if self.repository_id != repository_id {
            return Err(FormatError("invalid-connection-settings"));
        }
        let key = self.wrapping_key(code)?;
        let binding = self.binding()?;
        let mut plaintext = Zeroizing::new(Vec::new());
        decrypt(
            &mut std::io::Cursor::new(&self.ciphertext),
            &mut *plaintext,
            &key,
            &binding,
            32 * 1024,
        )?;
        if plaintext.is_empty() {
            return Err(FormatError("invalid-connection-settings"));
        }
        Ok(plaintext)
    }
}

/// Only returned after authenticating the entire repository bootstrap.
pub struct RecoveredRepository {
    pub root: Zeroizing<[u8; 32]>,
    pub connection_metadata: Zeroizing<String>,
}
impl RepositoryBootstrapEnvelope {
    fn binding(&self) -> Result<Vec<u8>> {
        if self.schema != RECOVERY_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
        {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        serde_json::to_vec(&(&self.schema, &self.repository_id, self.salt))
            .map_err(|_| FormatError("invalid-recovery-envelope"))
    }
    fn wrapping_key(&self, code: &RecoveryKey) -> Result<Zeroizing<[u8; 32]>> {
        self.binding()?;
        let mut key = Zeroizing::new([0; 32]);
        hkdf::Hkdf::<sha2::Sha256>::new(Some(&self.salt), code.0.as_ref())
            .expand(&self.binding()?, key.as_mut())
            .map_err(|_| FormatError("key-derivation-failed"))?;
        Ok(key)
    }
    pub fn protect(
        repository_id: String,
        connection_metadata: String,
        root: &[u8; 32],
        code: &RecoveryKey,
    ) -> Result<Self> {
        let connection_metadata = Zeroizing::new(connection_metadata);
        if connection_metadata.len() > 8192 {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        let mut result = Self {
            schema: RECOVERY_SCHEMA.into(),
            repository_id,
            salt: [0; 16],
            wrapped_key: Vec::new(),
        };
        getrandom::getrandom(&mut result.salt)
            .map_err(|_| FormatError("randomness-unavailable"))?;
        let key = result.wrapping_key(code)?;
        let binding = result.binding()?;
        let mut body = Zeroizing::new(Vec::with_capacity(32 + connection_metadata.len()));
        body.extend_from_slice(root);
        body.extend_from_slice(connection_metadata.as_bytes());
        encrypt(
            &mut std::io::Cursor::new(body.as_slice()),
            &mut result.wrapped_key,
            &key,
            &binding,
            body.len() as u64,
        )?;
        Ok(result)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.binding()?;
        let bytes =
            serde_json::to_vec(self).map_err(|_| FormatError("invalid-recovery-envelope"))?;
        if bytes.len() > MAX_REPOSITORY_BOOTSTRAP_BYTES {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_REPOSITORY_BOOTSTRAP_BYTES {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-recovery-envelope"))?;
        value.binding()?;
        Ok(value)
    }
    pub fn recover(&self, repository_id: &str, code: &RecoveryKey) -> Result<RecoveredRepository> {
        if self.repository_id != repository_id || self.wrapped_key.len() > 16 * 1024 {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        let key = self.wrapping_key(code)?;
        let binding = self.binding()?;
        let mut root = Zeroizing::new(Vec::new());
        decrypt(
            &mut std::io::Cursor::new(&self.wrapped_key),
            &mut *root,
            &key,
            &binding,
            32 + 8192,
        )?;
        let bytes: [u8; 32] = root
            .get(..32)
            .ok_or(FormatError("invalid-root-key"))?
            .try_into()
            .map_err(|_| FormatError("invalid-root-key"))?;
        let metadata = std::str::from_utf8(&root[32..])
            .map_err(|_| FormatError("invalid-recovery-envelope"))?;
        Ok(RecoveredRepository {
            root: Zeroizing::new(bytes),
            connection_metadata: Zeroizing::new(metadata.into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sealed(bytes: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        encrypt(
            &mut std::io::Cursor::new(bytes),
            &mut result,
            &[7; 32],
            b"repository/object/data/v1",
            bytes.len() as u64,
        )
        .unwrap();
        result
    }
    fn open(bytes: &[u8], binding: &[u8], key: &[u8; 32]) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        decrypt(
            &mut std::io::Cursor::new(bytes),
            &mut result,
            key,
            binding,
            2 * FRAME_BYTES as u64,
        )?;
        Ok(result)
    }
    #[test]
    fn secretstream_rejects_truncation_trailing_bytes_wrong_key_and_binding() {
        let plain = vec![42; FRAME_BYTES + 5];
        let encrypted = sealed(&plain);
        assert_eq!(
            encrypted.len() as u64,
            ciphertext_length(plain.len() as u64).unwrap()
        );
        assert_eq!(
            open(&encrypted, b"repository/object/data/v1", &[7; 32]).unwrap(),
            plain
        );
        for length in [0, 35, encrypted.len() - 1, encrypted.len() - TAG_BYTES - 4] {
            assert!(open(&encrypted[..length], b"repository/object/data/v1", &[7; 32]).is_err());
        }
        let mut appended = encrypted.clone();
        appended.push(0);
        assert!(open(&appended, b"repository/object/data/v1", &[7; 32]).is_err());
        assert!(open(&encrypted, b"other-repository/object/data/v1", &[7; 32]).is_err());
        assert!(open(&encrypted, b"repository/object/data/v1", &[8; 32]).is_err());
        let mut changed = encrypted;
        changed[45] ^= 1;
        assert!(open(&changed, b"repository/object/data/v1", &[7; 32]).is_err());
        assert_eq!(
            open(&sealed(&[]), b"repository/object/data/v1", &[7; 32]).unwrap(),
            Vec::<u8>::new()
        );
    }
    #[test]
    fn repository_bootstrap_authenticates_identity_root_and_descriptor_metadata() {
        let code = RecoveryKey::generate().unwrap();
        let recovered_code = RecoveryKey::parse(&code.expose()).unwrap();
        let envelope = RepositoryBootstrapEnvelope::protect(
            "synthetic-repository".into(),
            "https://synthetic.invalid/folder".into(),
            &[9; 32],
            &code,
        )
        .unwrap();
        let encoded = envelope.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("synthetic.invalid"));
        let mut imported = RepositoryBootstrapEnvelope::decode(&encoded).unwrap();
        assert_eq!(
            *imported
                .recover("synthetic-repository", &recovered_code)
                .unwrap()
                .root,
            [9; 32]
        );
        assert_eq!(
            &*imported
                .recover("synthetic-repository", &code)
                .unwrap()
                .connection_metadata,
            "https://synthetic.invalid/folder"
        );
        assert!(imported
            .recover("synthetic-repository", &RecoveryKey::generate().unwrap())
            .is_err());
        assert!(imported.recover("another-repository", &code).is_err());
        imported.salt[0] ^= 1;
        assert!(imported.recover("synthetic-repository", &code).is_err());
        let mut old: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        old["memoryKib"] = u32::MAX.into();
        assert!(RepositoryBootstrapEnvelope::decode(&serde_json::to_vec(&old).unwrap()).is_err());
        assert!(RepositoryBootstrapEnvelope::decode(&vec![b' '; MAX_REPOSITORY_BOOTSTRAP_BYTES + 1]).is_err());
    }

    #[test]
    fn connection_settings_use_the_same_key_but_a_distinct_authenticated_schema() {
        let code = RecoveryKey::generate().unwrap();
        let envelope = ConnectionSettingsEnvelope::protect(
            "provider-repository".into(),
            br#"{"provider":"webdav","password":"synthetic"}"#,
            &code,
        )
        .unwrap();
        let encoded = envelope.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("synthetic"));
        let mut decoded = ConnectionSettingsEnvelope::decode(&encoded).unwrap();
        assert_eq!(
            &*decoded.open("provider-repository", &code).unwrap(),
            br#"{"provider":"webdav","password":"synthetic"}"#,
        );
        assert!(decoded.open("another-repository", &code).is_err());
        assert!(decoded
            .open("provider-repository", &RecoveryKey::generate().unwrap())
            .is_err());
        decoded.salt[0] ^= 1;
        assert!(decoded.open("provider-repository", &code).is_err());
        assert!(ConnectionSettingsEnvelope::decode(
            &vec![b' '; MAX_CONNECTION_SETTINGS_BYTES + 1]
        )
        .is_err());
    }
    #[test]
    fn connection_settings_file_roundtrips_the_largest_supported_payload() {
        let key = RecoveryKey::generate().unwrap();
        let plaintext = vec![b'x'; 32 * 1024];
        let encoded = ConnectionSettingsEnvelope::protect("repository".into(), &plaintext, &key)
            .unwrap().encode().unwrap();
        assert!(encoded.len() > 24 * 1024);
        let decoded = ConnectionSettingsEnvelope::decode(&encoded).unwrap();
        assert_eq!(decoded.open("repository", &key).unwrap().as_slice(), plaintext);
        assert!(ConnectionSettingsEnvelope::protect("repository".into(), &vec![b'x'; 32 * 1024 + 1], &key).is_err());
        assert!(ConnectionSettingsEnvelope::decode(&vec![b' '; MAX_CONNECTION_SETTINGS_BYTES + 1]).is_err());
    }

    #[test]
    fn recovery_code_rejects_passwords_typos_and_noncanonical_grouping() {
        let code = RecoveryKey::generate().unwrap();
        let text = code.expose();
        assert!(RecoveryKey::parse(&text.to_uppercase()).is_ok());
        for bad in ["password", "123456", &text.replace('-', "")] {
            assert!(RecoveryKey::parse(bad).is_err());
        }
        let mut typo = text.as_bytes().to_vec();
        typo[0] = if typo[0] == b'0' { b'1' } else { b'0' };
        assert!(RecoveryKey::parse(std::str::from_utf8(&typo).unwrap()).is_err());
    }
}
