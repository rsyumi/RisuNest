use super::{content_identity::hash, FormatError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "risunest.external-storage/v2";

/// The library and its referenced assets are the whole content of every
/// repository, so their fingerprints are separated by one fixed domain rather
/// than by a per-repository selection. What a device publishes beyond them is
/// decided per device and carried in the published state.
pub fn library_fingerprint_domain() -> [u8; 32] {
    hash(b"risunest.external-library-fingerprint/v1")
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Cas,
    Sequential,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub schema: String,
    pub repository_id: String,
    pub encrypted: bool,
    pub publication_strategy: Option<Strategy>,
}
impl Descriptor {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 64 * 1024 {
            return Err(FormatError("invalid-descriptor"));
        }
        let descriptor: Self =
            serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-descriptor"))?;
        descriptor.validate()?;
        Ok(descriptor)
    }
    pub fn new(repository_id: String, strategy: Option<Strategy>) -> Result<Self> {
        let descriptor = Self {
            schema: SCHEMA.into(),
            repository_id,
            encrypted: true,
            publication_strategy: strategy,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != SCHEMA
            || !self.encrypted
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
        {
            return Err(FormatError("invalid-descriptor"));
        }
        Ok(())
    }
}
/// Fingerprints intentionally exclude packing, locators, nonces and timestamps.
pub fn fingerprint(domain: &[u8; 32], entries: &BTreeMap<String, [u8; 32]>) -> [u8; 32] {
    let mut digest = FingerprintBuilder::new(domain);
    for (key, content) in entries {
        digest
            .push(key, content)
            .expect("BTreeMap keys are unique and sorted");
    }
    digest.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encryption_is_required_for_every_repository() {
        for strategy in [None, Some(Strategy::Cas), Some(Strategy::Sequential)] {
            let mut descriptor = Descriptor::new("synthetic".into(), strategy).unwrap();
            descriptor.encrypted = false;
            assert!(Descriptor::decode(&serde_json::to_vec(&descriptor).unwrap()).is_err());
            assert!(descriptor.validate().is_err());
        }
    }
    #[test]
    fn the_library_fingerprint_domain_is_stable_and_separates_other_domains() {
        assert_eq!(library_fingerprint_domain(), library_fingerprint_domain());
        assert_ne!(library_fingerprint_domain(), [0; 32]);
    }
    #[test]
    fn descriptor_identity_no_longer_depends_on_a_published_selection() {
        let sync = Descriptor::new("repository".into(), Some(Strategy::Cas)).unwrap();
        let backup = Descriptor::new("repository".into(), None).unwrap();
        assert_ne!(sync, backup);
        assert_eq!(
            Descriptor::decode(&serde_json::to_vec(&sync).unwrap()).unwrap(),
            sync
        );
        // A device section selection is no longer part of repository identity,
        // so a strategy-carrying repository accepts every device's choice.
        assert!(Descriptor::new("repository".into(), Some(Strategy::Sequential)).is_ok());
    }
    #[test]
    fn a_descriptor_from_the_previous_schema_is_reported_rather_than_narrowed() {
        let previous = serde_json::json!({
            "schema": "risunest.external-storage/v1",
            "repositoryId": "repository",
            "scope": {
                "library": true,
                "referencedAssets": true,
                "deviceSettings": false,
                "devicePlugins": false,
            },
            "scopeId": vec![4u8; 32],
            "encrypted": true,
            "publicationStrategy": "cas",
        });
        assert_eq!(
            Descriptor::decode(&serde_json::to_vec(&previous).unwrap()),
            Err(FormatError("invalid-descriptor"))
        );
    }
}

pub struct FingerprintBuilder {
    digest: sha2::Sha256,
    previous: Option<String>,
}
impl FingerprintBuilder {
    pub fn new(domain: &[u8; 32]) -> Self {
        use sha2::Digest;
        let mut digest = sha2::Sha256::new();
        digest.update(b"risunest.external-fingerprint/v1\0");
        digest.update(domain);
        Self {
            digest,
            previous: None,
        }
    }
    pub fn push(&mut self, key: &str, content: &[u8; 32]) -> super::Result<()> {
        use sha2::Digest;
        if self
            .previous
            .as_deref()
            .is_some_and(|previous| previous >= key)
        {
            return Err(super::FormatError("fingerprint-order"));
        }
        self.digest.update((key.len() as u64).to_le_bytes());
        self.digest.update(key.as_bytes());
        self.digest.update(content);
        self.previous = Some(key.into());
        Ok(())
    }
    pub fn finish(self) -> [u8; 32] {
        use sha2::Digest;
        self.digest.finalize().into()
    }
}
