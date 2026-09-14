//! Google Drive v3 adapter for a user-owned OAuth application.
//!
//! # Connection configuration
//!
//! - `provider` is `google_drive`.
//! - `profile` is absent or `drive`.
//! - `endpoint` is `https://www.googleapis.com`; an empty value means that
//!   default. Any other value must be `https`, or `http` on `127.0.0.1`, which
//!   is what the synthetic loopback fixture uses.
//! - `account_id` is the Drive `permissionId` of the authorized account. Every
//!   open verifies it against `about.get`, so another account's credentials
//!   never silently operate on this connection.
//! - `location["folderId"]` is the repository root folder ID. It is an ordinary
//!   My Drive folder the user can see and move.
//! - `location["space"]` is absent or `drive`, or `appDataFolder`, in which case
//!   `folderId` must be the documented `appDataFolder` alias. That space is
//!   hidden from the Drive user interface and is removed when the user deletes
//!   the application's data, so it is not a visible independent backup.
//! - `oauth_profile` is required: `project_id` plus `platform_client_ids` keyed
//!   by platform (`windows`, `android`, `macos`, `ios`, `linux`). The client
//!   identifier of the running platform is the one used to refresh.
//!
//! # Secret payload
//!
//! The vault entry is UTF-8 JSON:
//! `{"refreshToken":"…","accessToken":"…"?,"accessTokenExpiresAtMs":u64?}`.
//! Rotated tokens are written back with `vault.replace`, so one connection keeps
//! one `SecretRef`. A missing entry surfaces as `ReauthRequired`.
//!
//! # Remote layout
//!
//! Every object is one file whose single parent is the root folder, named
//! `<role>-<object_id>` and tagged with the private properties `risunestRole`,
//! `risunestObjectId` and `risunestJobId`. `RemoteLocator.object` is the Drive
//! file ID, except for the mutable head, which uses the reserved control name
//! `control-head` and is resolved to its stable file ID when the repository is
//! opened.
use super::Dependencies;
use crate::external_storage::contract::{
    ErrorKind, Provider, ProviderError, RemoteLocator, RepositoryHandle, Result,
};
use std::sync::Arc;

mod auth;
mod config;
mod provider;
#[cfg(test)]
mod tests;
mod wire;

pub(crate) use auth::{authorization_policy, exchange_authorization_code};

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(provider::provider(dependencies))
}

/// Canonical locator of the mutable head of an opened repository. The owner
/// uses it for `replace_head` and for reading the head back.
pub(crate) fn head_locator(repository: &RepositoryHandle) -> Result<RemoteLocator> {
    if repository.connection_identity.is_empty() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(RemoteLocator {
        connection_identity: repository.connection_identity.clone(),
        collection: None,
        object: provider::HEAD_OBJECT.to_owned(),
    })
}
