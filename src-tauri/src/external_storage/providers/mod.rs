//! Provider-owned modules. Only integrated factories enter the product registry.
pub(crate) mod github_releases;
pub(crate) mod gitlab_packages;
pub(crate) mod google_drive;
pub(crate) mod mybox;
pub(crate) mod onedrive;
pub(crate) mod s3;
pub(crate) mod webdav;

pub(crate) struct Dependencies {
    pub http: std::sync::Arc<dyn super::http::HttpTransport>,
    pub budget: std::sync::Arc<dyn super::http::RequestBudget>,
    pub clock: std::sync::Arc<dyn super::http::Clock>,
    pub vault: std::sync::Arc<dyn super::auth::SecretVault>,
}
