//! Common registration point. Provider implementations are supplied separately.
use super::contract::{ErrorKind, Provider, ProviderError, Result};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default)]
pub(crate) struct Registry(BTreeMap<String, Arc<dyn Provider>>);
impl Registry {
    pub fn register(&mut self, id: &str, provider: Arc<dyn Provider>) -> Result<()> {
        if self.0.contains_key(id) || !PROVIDER_IDS.contains(&id) {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        self.0.insert(id.into(), provider);
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<&Arc<dyn Provider>> {
        self.0
            .get(id)
            .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))
    }
    pub fn available(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}
pub(crate) const PROVIDER_IDS: &[&str] = &[
    "webdav",
    "s3",
    "google_drive",
    "onedrive",
    "mybox",
    "github_releases",
    "gitlab_packages",
];
