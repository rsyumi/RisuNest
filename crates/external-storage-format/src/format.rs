use super::{content_identity::hash, FormatError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "risunest.external-storage/v1";
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scope {
    pub library: bool,
    pub referenced_assets: bool,
    pub device_settings: bool,
    pub device_plugins: bool,
}
impl Scope {
    pub fn id(&self) -> [u8; 32] {
        hash(&[
            1,
            u8::from(self.library),
            u8::from(self.referenced_assets),
            u8::from(self.device_settings),
            u8::from(self.device_plugins),
        ])
    }
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
    pub scope: Scope,
    pub scope_id: [u8; 32],
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
    pub fn new(repository_id: String, scope: Scope, strategy: Option<Strategy>) -> Result<Self> {
        let descriptor = Self {
            schema: SCHEMA.into(),
            repository_id,
            scope_id: scope.id(),
            scope,
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
            || self.scope_id != self.scope.id()
            || !self.scope.library
            || !self.scope.referenced_assets
        {
            return Err(FormatError("invalid-descriptor"));
        }
        if self.publication_strategy.is_some()
            && (self.scope.device_settings || self.scope.device_plugins)
        {
            return Err(FormatError("sync-scope-includes-device"));
        }
        Ok(())
    }
    pub fn require_scope(&self, id: &[u8; 32]) -> Result<()> {
        self.validate()?;
        if id != &self.scope_id {
            Err(FormatError("repository-scope-mismatch"))
        } else {
            Ok(())
        }
    }
}
/// Fingerprints intentionally exclude packing, locators, nonces and timestamps.
pub fn fingerprint(scope: &[u8; 32], entries: &BTreeMap<String, [u8; 32]>) -> [u8; 32] {
    let mut digest = FingerprintBuilder::new(scope);
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
        let scope = Scope {
            library: true,
            referenced_assets: true,
            device_settings: false,
            device_plugins: false,
        };
        for strategy in [None, Some(Strategy::Cas), Some(Strategy::Sequential)] {
            let mut descriptor =
                Descriptor::new("synthetic".into(), scope.clone(), strategy).unwrap();
            descriptor.encrypted = false;
            assert!(Descriptor::decode(&serde_json::to_vec(&descriptor).unwrap()).is_err());
            assert!(descriptor.validate().is_err());
            assert!(descriptor.require_scope(&scope.id()).is_err());
        }
    }
}

pub struct FingerprintBuilder {
    digest: sha2::Sha256,
    previous: Option<String>,
}
impl FingerprintBuilder {
    pub fn new(scope: &[u8; 32]) -> Self {
        use sha2::Digest;
        let mut digest = sha2::Sha256::new();
        digest.update(b"risunest.external-fingerprint/v1\0");
        digest.update(scope);
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
