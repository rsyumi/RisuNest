//! Provider-owned modules. Only integrated factories enter the product registry.
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
    pub mybox_budget: Arc<dyn super::http::MyboxRequestBudget>,
    pub requests: Arc<super::http::RequestState>,
    pub clock: Arc<dyn super::http::Clock>,
    pub vault: Arc<dyn super::auth::SecretVault>,
}

impl Dependencies {
    pub async fn wait_until_ready(
        &self,
        account: &super::quota::AccountKey,
        control: bool,
        cancel: &super::contract::Cancellation,
    ) -> Result<Option<std::time::Instant>> {
        super::http::wait_until_ready(
            self.clock.as_ref(),
            self.requests.as_ref(),
            account,
            control,
            cancel,
        )
        .await
    }

    pub async fn send(&self, request: super::http::HttpRequest, cancel: &super::contract::Cancellation) -> Result<super::http::HttpResponse> {
        super::http::send(self.http.as_ref(), self.mybox_budget.as_ref(), self.clock.as_ref(),
            self.requests.as_ref(), request, cancel).await
    }

    pub async fn send_signed(
        &self,
        request: super::http::HttpRequest,
        cancel: &super::contract::Cancellation,
        deadline: Option<std::time::Instant>,
    ) -> Result<super::http::HttpResponse> {
        super::http::send_signed(
            self.http.as_ref(),
            self.clock.as_ref(),
            self.requests.as_ref(),
            request,
            cancel,
            deadline,
        )
        .await
    }
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
