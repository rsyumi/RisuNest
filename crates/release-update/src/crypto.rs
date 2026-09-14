use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign_verify::{PublicKey, Signature};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::{Error, Result};

#[derive(Clone, Debug)]
pub struct TrustedPublicKey(PublicKey);

impl TrustedPublicKey {
    pub fn from_tauri(encoded: &str) -> Result<Self> {
        if encoded.trim().is_empty() {
            return Err(Error::NotConfigured);
        }
        let decoded = decode_tauri_text(encoded, "public key")?;
        let key = if decoded.trim_start().starts_with("untrusted comment:") {
            PublicKey::decode(&decoded)
        } else {
            PublicKey::from_base64(decoded.trim())
        }
        .map_err(|error| Error::InvalidSignature(error.to_string()))?;
        Ok(Self(key))
    }

    pub fn verify(&self, bytes: &[u8], encoded_signature: &str) -> Result<()> {
        let signature = decode_signature(encoded_signature)?;
        self.0
            .verify(bytes, &signature, true)
            .map_err(|error| Error::InvalidSignature(error.to_string()))
    }

    pub fn verify_file(&self, path: impl AsRef<Path>, encoded_signature: &str) -> Result<()> {
        let signature = decode_signature(encoded_signature)?;
        let mut verifier = self
            .0
            .verify_stream(&signature)
            .map_err(|error| Error::InvalidSignature(error.to_string()))?;
        let mut reader = BufReader::new(File::open(path)?);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            verifier.update(&buffer[..read]);
        }
        verifier
            .finalize()
            .map_err(|error| Error::InvalidSignature(error.to_string()))
    }
}

pub fn decode_tauri_public_key(encoded: &str) -> Result<TrustedPublicKey> {
    TrustedPublicKey::from_tauri(encoded)
}

pub fn verify_tauri_signature(
    bytes: &[u8],
    encoded_signature: &str,
    public_key: &TrustedPublicKey,
) -> Result<()> {
    public_key.verify(bytes, encoded_signature)
}

fn decode_tauri_text(value: &str, kind: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.starts_with("untrusted comment:") {
        return Ok(trimmed.to_owned());
    }
    let decoded = STANDARD
        .decode(trimmed)
        .map_err(|error| Error::InvalidSignature(format!("{kind} base64: {error}")))?;
    String::from_utf8(decoded)
        .map_err(|error| Error::InvalidSignature(format!("{kind} UTF-8: {error}")))
}

fn decode_signature(value: &str) -> Result<Signature> {
    let decoded = decode_tauri_text(value, "signature")?;
    Signature::decode(&decoded).map_err(|error| Error::InvalidSignature(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW_KEY: &str = "untrusted comment: minisign public key E7620F1842B4E81F\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const RAW_SIGNATURE: &str = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1556193335\tfile:test\ny/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";

    #[test]
    fn accepts_raw_and_tauri_wrapped_key_and_signature() {
        for key in [RAW_KEY.to_owned(), STANDARD.encode(RAW_KEY)] {
            let key = TrustedPublicKey::from_tauri(&key).unwrap();
            for signature in [RAW_SIGNATURE.to_owned(), STANDARD.encode(RAW_SIGNATURE)] {
                key.verify(b"test", &signature).unwrap();
                assert!(key.verify(b"changed", &signature).is_err());
            }
        }
    }

    #[test]
    fn verifies_a_signed_file_without_buffering_it_in_memory() {
        let path = std::env::temp_dir().join(format!(
            "risunest-release-signature-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"test").unwrap();
        let key = TrustedPublicKey::from_tauri(RAW_KEY).unwrap();
        key.verify_file(&path, RAW_SIGNATURE).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert!(key.verify_file(&path, RAW_SIGNATURE).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_key_is_explicitly_not_configured() {
        assert!(matches!(
            TrustedPublicKey::from_tauri("  "),
            Err(Error::NotConfigured)
        ));
    }
}
