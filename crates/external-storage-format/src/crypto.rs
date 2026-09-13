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
pub struct RecoveryEnvelope {
    pub repository_id: String,
    pub connection_metadata: String,
    pub salt: [u8; 16],
    pub memory_kib: u32,
    pub iterations: u32,
    pub wrapped_key: Vec<u8>,
}
impl RecoveryEnvelope {
    fn binding(&self) -> Result<Vec<u8>> {
        if self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.connection_metadata.len() > 8192
            || !(16 * 1024..=64 * 1024).contains(&self.memory_kib)
            || !(2..=4).contains(&self.iterations)
        {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        serde_json::to_vec(&(
            "risunest.recovery/v1",
            &self.repository_id,
            &self.connection_metadata,
            self.salt,
            self.memory_kib,
            self.iterations,
        ))
        .map_err(|_| FormatError("invalid-recovery-envelope"))
    }
    fn wrapping_key(&self, password: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        self.binding()?;
        if password.is_empty() || password.len() > 1024 {
            return Err(FormatError("invalid-recovery-secret"));
        }
        let params = argon2::Params::new(self.memory_kib, self.iterations, 1, Some(32))
            .map_err(|_| FormatError("invalid-kdf-parameters"))?;
        let mut key = Zeroizing::new([0; 32]);
        argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password_into(password, &self.salt, key.as_mut())
            .map_err(|_| FormatError("key-derivation-failed"))?;
        Ok(key)
    }
    pub fn protect(
        repository_id: String,
        connection_metadata: String,
        root: &[u8; 32],
        password: &[u8],
    ) -> Result<Self> {
        let mut result = Self {
            repository_id,
            connection_metadata,
            salt: [0; 16],
            memory_kib: 32 * 1024,
            iterations: 2,
            wrapped_key: Vec::new(),
        };
        getrandom::getrandom(&mut result.salt)
            .map_err(|_| FormatError("randomness-unavailable"))?;
        let key = result.wrapping_key(password)?;
        let binding = result.binding()?;
        encrypt(
            &mut std::io::Cursor::new(root),
            &mut result.wrapped_key,
            &key,
            &binding,
            32,
        )?;
        Ok(result)
    }
    pub fn recover(&self, repository_id: &str, password: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        if self.repository_id != repository_id || self.wrapped_key.len() > 1024 {
            return Err(FormatError("invalid-recovery-envelope"));
        }
        let key = self.wrapping_key(password)?;
        let binding = self.binding()?;
        let mut root = Zeroizing::new(Vec::new());
        decrypt(
            &mut std::io::Cursor::new(&self.wrapped_key),
            &mut *root,
            &key,
            &binding,
            32,
        )?;
        let bytes: [u8; 32] = root
            .as_slice()
            .try_into()
            .map_err(|_| FormatError("invalid-root-key"))?;
        Ok(Zeroizing::new(bytes))
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
    fn recovery_is_independent_of_original_os_and_authenticates_connection_metadata() {
        let envelope = RecoveryEnvelope::protect(
            "synthetic-repository".into(),
            "https://synthetic.invalid/folder".into(),
            &[9; 32],
            b"synthetic recovery password",
        )
        .unwrap();
        let encoded = serde_json::to_vec(&envelope).unwrap();
        let mut imported: RecoveryEnvelope = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            *imported
                .recover("synthetic-repository", b"synthetic recovery password")
                .unwrap(),
            [9; 32]
        );
        assert!(imported.recover("synthetic-repository", b"wrong").is_err());
        assert!(imported
            .recover("another-repository", b"synthetic recovery password")
            .is_err());
        imported.connection_metadata = "https://changed.invalid".into();
        assert!(imported
            .recover("synthetic-repository", b"synthetic recovery password")
            .is_err());
        imported.memory_kib = u32::MAX;
        assert!(imported
            .recover("synthetic-repository", b"synthetic recovery password")
            .is_err());
    }
}
