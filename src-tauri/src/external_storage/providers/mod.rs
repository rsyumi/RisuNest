//! Provider-owned modules. Only integrated factories enter the product registry.
// The product registers no provider until the connection commands exist, so
// every adapter is reachable from tests only. Remove once the registry is wired.
#![allow(dead_code)]
pub(crate) mod common;
pub(crate) mod github_releases;
pub(crate) mod gitlab_packages;
pub(crate) mod google_drive;
pub(crate) mod mybox;
pub(crate) mod onedrive;
pub(crate) mod s3;
pub(crate) mod webdav;

use super::contract::{ErrorKind, Provider, ProviderError, Result};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct Dependencies {
    pub http: Arc<dyn super::http::HttpTransport>,
    pub budget: Arc<dyn super::http::RequestBudget>,
    pub clock: Arc<dyn super::http::Clock>,
    pub vault: Arc<dyn super::auth::SecretVault>,
}

/// Factory for one of `registry::PROVIDER_IDS`. An unknown id is `Unsupported`.
pub(crate) fn create(id: &str, dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    match id {
        "webdav" => webdav::create(dependencies),
        "s3" => s3::create(dependencies),
        "google_drive" => google_drive::create(dependencies),
        "onedrive" => onedrive::create(dependencies),
        "mybox" => mybox::create(dependencies),
        "github_releases" => github_releases::create(dependencies),
        "gitlab_packages" => gitlab_packages::create(dependencies),
        _ => Err(ProviderError::new(ErrorKind::Unsupported)),
    }
}
