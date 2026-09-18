//! Microsoft OneDrive through Microsoft Graph v1.0 with a user owned Microsoft
//! Entra application. Objects are path addressed under one repository root
//! folder, small objects are written with a single content `PUT` and larger ones
//! through an upload session.
//!
//! `ConnectionConfig`:
//!
//! - `provider` is `onedrive`.
//! - `endpoint` is the Graph service root, normally
//!   `https://graph.microsoft.com/v1.0`. Only `https` is accepted, except for the
//!   loopback host the wire fixture serves.
//! - `profile` is optional and, when present, repeats `location["accountType"]`.
//! - `account_id` is the signed in account identity. It takes part in the
//!   connection identity and in the quota account, never in a request path.
//! - `location["accountType"]` is `personal`, `business` or `appFolder` and
//!   selects the requested file scope. `Files.ReadWrite.AppFolder` for
//!   `appFolder`, `Files.ReadWrite` otherwise. Every profile also requests
//!   `User.Read` to bind the connection to the authenticated Graph user and
//!   `offline_access` for refresh tokens.
//! - `location["tenant"]` is `consumers`, `common`, `organizations` or a tenant
//!   identifier, and forms the token endpoint path. A `personal` connection
//!   accepts only `consumers` or `common`; a `business` connection rejects
//!   `consumers`.
//! - `location["driveId"]` is the target drive.
//! - `location["rootItemId"]` is the repository root folder item, or the literal
//!   `special/approot` when the account type is `appFolder`.
//! - `location["redirectUri"]` is the registered reply URL and is required only
//!   for `authorization_policy`.
//! - `oauth_profile` is required: `project_id` is the application (client) ID and
//!   `platform_client_ids` holds per platform client IDs.
//!
//! The authority is fixed to `https://login.microsoftonline.com`, so an imported
//! connection cannot redirect a refresh token to another host. Only a loopback
//! endpoint moves the authority, which is how the synthetic fixture is served.
//!
//! Vault payload, as UTF-8 JSON:
//! `{"refreshToken":"…","accessToken":"…"?,"accessTokenExpiresAtMs":1234?}`.
//! Microsoft may rotate the refresh token on every grant, so a refreshed
//! document is written back with `vault.replace` and the connection keeps one
//! `SecretRef`.
//!
//! Graph exposes no SHA-256 for stored content, so a receipt never carries a
//! provider verified checksum and a lost response is reconciled against the
//! remote length.

mod config;
mod graph;
#[cfg(test)]
mod tests;
mod tokens;

use super::{common, Dependencies};
use crate::external_storage::{
    auth::{AuthorizationCode, AuthorizationPolicy},
    capabilities::Capabilities,
    contract::*,
    http::{self, HttpRequest, HttpResponse},
    quota::AccountKey,
};
use config::Settings;
use std::{collections::BTreeMap, sync::Arc};

#[cfg(any(target_os = "ios", test))]
pub(crate) const IOS_REDIRECT_URI: &str = "msauth.io.github.rsyumi.risunest://auth";

/// External browser policy for a connection: authority, client id for the
/// platform, registered reply URL and the scopes of its account type.
pub(crate) fn authorization_policy(
    config: &ConnectionConfig,
    platform: &str,
) -> Result<AuthorizationPolicy> {
    tokens::authorization_policy(config, platform)
}

/// Guard against a server that acknowledges fragments without making progress.
const MAX_FRAGMENT_REQUESTS: u32 = 4096;
const MAX_LAYOUT_LIST_PAGES: usize = 256;
/// The one mutable head, a root member beside the role folders.
const HEAD_OBJECT: &str = "head";

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(OneDrive::new(dependencies)))
}

pub(crate) struct OneDrive {
    deps: Dependencies,
}

pub(crate) struct AuthorizedSecret {
    pub secret: SecretRef,
    pub account_id: String,
}

struct Context {
    settings: Settings,
    secret: SecretRef,
    /// Root folder item resolved at open time. For an app folder connection this
    /// is the identifier behind `special/approot`.
    root_item_id: String,
    account: AccountKey,
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn capabilities(_account_type: config::AccountType) -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        // conflictBehavior=fail prevents creating a second head. A plain
        // content PUT does not provide the conditional update needed for CAS.
        atomic_create_head: true,
        conditional_head_update: false,
        stable_head_replace: true,
        head_read_after_write: true,
        head_retry_control: true,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        conditional_get: true,
        range: true,
        resumable_upload: true,
        // The single content PUT limit does not limit an upload session.
        max_stored_bytes: None,
        sdk_overhead_bytes: 0,
        upload_alignment: graph::FRAGMENT_ALIGNMENT,
    }
}

impl OneDrive {
    pub(crate) fn new(dependencies: Dependencies) -> Self {
        Self { deps: dependencies }
    }

    fn now(&self) -> u64 {
        self.deps.clock.now_ms()
    }

    async fn send(&self, mut request: HttpRequest, cancel: &Cancellation) -> Result<HttpResponse> {
        if http::control_operation(request.operation) {
            http::bypass_cache(&mut request.headers);
        }
        self.deps.send(request, cancel).await
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: url::Url,
        operation: ProviderOperation,
        account: &AccountKey,
        token: Option<&str>,
    ) -> HttpRequest {
        let mut headers = BTreeMap::new();
        if let Some(token) = token {
            headers.insert("authorization".to_owned(), tokens::bearer(token));
        }
        let api_request = url.origin().ascii_serialization() == account.authority();
        HttpRequest {
            method,
            url,
            headers,
            body: None,
            content_length: None,
            operation,
            account: account.clone(),
            api_request,
            mybox_charge: None,
            control: http::control_operation(operation),
        }
    }

    fn json_body(request: &mut HttpRequest, body: Vec<u8>) {
        request
            .headers
            .insert("content-type".to_owned(), "application/json".to_owned());
        request.content_length = Some(body.len() as u64);
        request.body = Some(Box::pin(std::io::Cursor::new(body)));
    }

    async fn create_repository_folder(
        &self,
        context: &Context,
        token: &str,
        folder: &str,
        cancel: &Cancellation,
    ) -> Result<()> {
        let url = graph::root_children_url(&context.settings, &context.root_item_id)?;
        let mut request = self.request(
            reqwest::Method::POST,
            url,
            ProviderOperation::Create,
            &context.account,
            Some(token),
        );
        let body = format!(
            "{{\"name\":\"{folder}\",\"folder\":{{}},\"@microsoft.graph.conflictBehavior\":\"fail\"}}"
        );
        Self::json_body(&mut request, body.into_bytes());
        let response = self.send(request, cancel).await?;
        if response.status == 409 {
            return Err(common::error(ErrorKind::PreconditionFailed, 409));
        }
        graph::require(&response, &[200, 201], self.now())
    }

    async fn collection_children(
        &self,
        context: &Context,
        token: &str,
        folder: Option<&str>,
        cancel: &Cancellation,
    ) -> Result<Vec<graph::Item>> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_LAYOUT_LIST_PAGES {
            let query = graph::listing_query(1000, cursor.as_deref());
            let url = match folder {
                Some(folder) => graph::folder_children_url(
                    &context.settings,
                    &context.root_item_id,
                    folder,
                    &query,
                )?,
                None => graph::root_children_listing_url(
                    &context.settings,
                    &context.root_item_id,
                    &query,
                )?,
            };
            let request = self.request(
                reqwest::Method::GET,
                url,
                ProviderOperation::List,
                &context.account,
                Some(token),
            );
            let mut response = self.send(request, cancel).await?;
            graph::require(&response, &[200], self.now())?;
            let page: graph::ChildrenPage = graph::json(&mut response, cancel).await?;
            items.extend(page.value);
            let Some(next) = page
                .next_link
                .as_deref()
                .map(|link| graph::skip_token(link, &context.settings))
                .transpose()?
            else {
                return Ok(items);
            };
            if !seen.insert(next.clone()) {
                return Err(corrupt());
            }
            cursor = Some(next);
        }
        Err(corrupt())
    }

    async fn require_initial_contents(
        &self,
        context: &Context,
        token: &str,
        cancel: &Cancellation,
    ) -> Result<()> {
        let descriptors = config::role_folder(ObjectRole::Descriptor);
        for folder in config::REPOSITORY_FOLDERS {
            let items = self
                .collection_children(context, token, Some(folder), cancel)
                .await?;
            if *folder != descriptors {
                if !items.is_empty() {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                continue;
            }
            if items.len() > 1 {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            if let Some(item) = items.into_iter().next() {
                let name = item.name.ok_or_else(corrupt)?;
                let path = format!("{descriptors}/{name}");
                if item.file.is_none()
                    || item.folder.is_some()
                    || item.size.is_none_or(|size| size == 0)
                    || config::validate_relative_path(&path).is_err()
                {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
            }
        }
        Ok(())
    }

    async fn resume_layout(
        &self,
        context: &Context,
        token: &str,
        cancel: &Cancellation,
    ) -> Result<()> {
        let mut folders = BTreeMap::new();
        for item in self
            .collection_children(context, token, None, cancel)
            .await?
        {
            let name = item.name.ok_or_else(corrupt)?;
            if !config::REPOSITORY_FOLDERS.contains(&name.as_str())
                || item.folder.is_none()
                || item.file.is_some()
                || folders.insert(name, ()).is_some()
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        for folder in config::REPOSITORY_FOLDERS {
            if !folders.contains_key(*folder) {
                self.create_repository_folder(context, token, folder, cancel)
                    .await?;
            }
        }
        self.require_initial_contents(context, token, cancel).await
    }

    fn context<'a>(&self, repository: &'a RepositoryHandle) -> Result<&'a Context> {
        repository
            .context
            .downcast_ref::<Context>()
            .filter(|context| context.settings.identity == repository.connection_identity)
            .ok_or_else(corrupt)
    }

    /// A stored access token is reused until it is close to expiry. A refresh
    /// writes the rotated document back to the same vault reference.
    async fn access_token(
        &self,
        context: &Context,
        cancel: &Cancellation,
    ) -> Result<zeroize::Zeroizing<String>> {
        let stored = {
            let bytes = self.deps.vault.read(&context.secret).await?;
            tokens::decode(bytes.0.as_slice())?
        };
        if let Some(token) = stored.usable_access_token(self.now()) {
            return Ok(token);
        }
        let form = tokens::refresh_form(&context.settings, &stored.refresh_token);
        let request = tokens::token_request(
            &context.settings,
            form,
            context.account.clone(),
        )?;
        let mut response = self.send(request, cancel).await?;
        if response.status != 200 {
            return Err(graph::classify_token(
                response.status,
                &response.headers,
                self.now(),
            ));
        }
        let refreshed = tokens::parse_grant(
            &mut response,
            Some(&stored.refresh_token),
            self.now(),
            cancel,
        )
        .await?;
        self.deps
            .vault
            .replace(&context.secret, &tokens::encode(&refreshed)?)
            .await?;
        refreshed
            .access_token
            .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))
    }

    /// Redeems an authorization code and stores the first token document. The
    /// returned reference is the connection's single secret.
    pub(crate) async fn exchange_authorization_code(
        &self,
        config: &ConnectionConfig,
        grant: &AuthorizationCode,
        cancel: &Cancellation,
    ) -> Result<AuthorizedSecret> {
        let settings = config::validate_authorization(config)?;
        if grant.client_id != config::platform_client_id(config, config::platform_key())? {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let account = AccountKey::pending(config::PROVIDER_ID, &settings.endpoint)?;
        let form = tokens::authorization_code_form(&settings, grant)?;
        let request = tokens::token_request(
            &settings,
            form,
            account.clone(),
        )?;
        let mut response = self.send(request, cancel).await?;
        if response.status != 200 {
            return Err(graph::classify_token(
                response.status,
                &response.headers,
                self.now(),
            ));
        }
        let granted = tokens::parse_grant(&mut response, None, self.now(), cancel).await?;
        let access_token = granted
            .access_token
            .as_ref()
            .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
        let request = self.request(
            reqwest::Method::GET,
            graph::signed_in_user_url(&settings)?,
            ProviderOperation::Metadata,
            &account,
            Some(access_token),
        );
        let mut response = self.send(request, cancel).await?;
        if response.status != 200 {
            return Err(graph::classify_token(
                response.status,
                &response.headers,
                self.now(),
            ));
        }
        let identity: graph::SignedInUser = graph::json(&mut response, cancel).await?;
        let account_id = config::account_id(&identity.id)?;
        let authenticated = AccountKey::new(config::PROVIDER_ID, &settings.endpoint, &account_id)?;
        self.deps.requests.resolve_pending(&account, &authenticated)?;
        let secret = self.deps.vault.store(&tokens::encode(&granted)?).await?;
        Ok(AuthorizedSecret { secret, account_id })
    }

    fn locator(&self, context: &Context, folder: &str, path: String) -> RemoteLocator {
        RemoteLocator {
            connection_identity: context.settings.identity.clone(),
            collection: Some(folder.to_owned()),
            object: path,
        }
    }

    fn object_receipt(
        &self,
        context: &Context,
        intent: &ObjectIntent,
        path: &str,
        item: &graph::Item,
        headers: &BTreeMap<String, String>,
    ) -> Result<ObjectReceipt> {
        if item.file.is_none() || item.size != Some(intent.byte_length) {
            return Err(corrupt());
        }
        Ok(ObjectReceipt {
            locator: self.locator(context, config::role_folder(intent.role), path.to_owned()),
            byte_length: intent.byte_length,
            version: graph::version(item, headers),
            // Graph publishes quickXorHash and sha1Hash at best and documents
            // sha256Hash as unsupported, so no digest is provider verified.
            checksum: None,
            complete: true,
        })
    }

    /// Definitive remote state for an object whose write response was lost or
    /// rejected for a name conflict.
    async fn fetch_item(
        &self,
        context: &Context,
        path: &str,
        cancel: &Cancellation,
    ) -> Result<Option<(graph::Item, BTreeMap<String, String>)>> {
        let token = self.access_token(context, cancel).await?;
        let url = graph::metadata_url(&context.settings, &context.root_item_id, path)?;
        let request = self.request(
            reqwest::Method::GET,
            url,
            ProviderOperation::Metadata,
            &context.account,
            Some(token.as_str()),
        );
        let mut response = self.send(request, cancel).await?;
        if matches!(response.status, 404 | 410) {
            return Ok(None);
        }
        graph::require(&response, &[200], self.now())?;
        let headers = response.headers.clone();
        let item: graph::Item = graph::json(&mut response, cancel).await?;
        Ok(Some((item, headers)))
    }

    fn stores(item: &graph::Item, byte_length: u64) -> bool {
        item.file.is_some() && item.size == Some(byte_length)
    }

    /// Converges an interrupted immutable create: the same identity and length
    /// completes, anything else is a precondition failure. Graph publishes no
    /// SHA-256, so the remote length is the strongest available evidence.
    async fn converge(
        &self,
        context: &Context,
        intent: &ObjectIntent,
        path: &str,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        match self.fetch_item(context, path, cancel).await? {
            Some((item, headers)) if Self::stores(&item, intent.byte_length) => {
                self.object_receipt(context, intent, path, &item, &headers)
            }
            _ => Err(common::error(ErrorKind::PreconditionFailed, 409)),
        }
    }

    async fn single_put(
        &self,
        context: &Context,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        path: &str,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        if intent.byte_length > graph::SIMPLE_UPLOAD_MAX_BYTES {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        let token = self.access_token(context, cancel).await?;
        let url = graph::item_url(
            &context.settings,
            &context.root_item_id,
            path,
            "/content",
            Some("@microsoft.graph.conflictBehavior=fail"),
        )?;
        let mut request = self.request(
            reqwest::Method::PUT,
            url,
            ProviderOperation::Create,
            &context.account,
            Some(token.as_str()),
        );
        request.headers.insert(
            "content-type".to_owned(),
            "application/octet-stream".to_owned(),
        );
        request.content_length = Some(intent.byte_length);
        request.body = Some(source.open(0, intent.byte_length, cancel).await?);
        let mut response = self.send(request, cancel).await?;
        match response.status {
            200 | 201 => {
                let headers = response.headers.clone();
                let item: graph::Item = graph::json(&mut response, cancel).await?;
                self.object_receipt(context, intent, path, &item, &headers)
            }
            // The name is taken. Converge only when the stored object already
            // matches this intent; never replace or delete another object.
            409 => self.converge(context, intent, path, cancel).await,
            status => Err(graph::classify(status, &response.headers, self.now())),
        }
    }

    async fn continue_session(
        &self,
        context: &Context,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        resume: &ResumeState,
        path: &str,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let session = self.open_session(&resume.sealed_state, intent).await?;
        let upload_url = url::Url::parse(&session.upload_url).map_err(|_| corrupt())?;
        let mut offset = resume.confirmed_offset;
        for _ in 0..MAX_FRAGMENT_REQUESTS {
            cancel.check()?;
            let length = graph::fragment_length(offset, intent.byte_length)?;
            let last = offset + length == intent.byte_length;
            let operation = if last {
                ProviderOperation::CompleteUpload
            } else {
                ProviderOperation::UploadChunk
            };
            // The upload URL is pre-authorized; a bearer token on it is
            // documented to fail with 401.
            let mut request = self.request(
                reqwest::Method::PUT,
                upload_url.clone(),
                operation,
                &context.account,
                None,
            );
            request.headers.insert(
                "content-range".to_owned(),
                graph::content_range(offset, length, intent.byte_length)?,
            );
            request.content_length = Some(length);
            request.body = Some(source.open(offset, length, cancel).await?);
            let mut response = self.send(request, cancel).await?;
            match response.status {
                202 => {
                    let state: graph::SessionState = graph::json(&mut response, cancel).await?;
                    let next = state
                        .next_expected_ranges
                        .as_deref()
                        .and_then(graph::confirmed_offset)
                        .unwrap_or(offset + length);
                    if next <= offset || next >= intent.byte_length {
                        return Err(corrupt());
                    }
                    offset = next;
                }
                200 | 201 => {
                    let headers = response.headers.clone();
                    let item: graph::Item = graph::json(&mut response, cancel).await?;
                    return self.object_receipt(context, intent, path, &item, &headers);
                }
                409 => return self.converge(context, intent, path, cancel).await,
                status => return Err(graph::classify(status, &response.headers, self.now())),
            }
        }
        Err(corrupt())
    }

    async fn open_session(
        &self,
        sealed: &SecretRef,
        intent: &ObjectIntent,
    ) -> Result<tokens::SealedSession> {
        let bytes = self.deps.vault.read(sealed).await?;
        let session = tokens::decode_session(bytes.0.as_slice())?;
        if session.object_id != intent.object_id || session.byte_length != intent.byte_length {
            return Err(corrupt());
        }
        Ok(session)
    }

    /// One conditional or plain head write. Never retried inside the adapter.
    async fn write_head(
        &self,
        repository: &RepositoryHandle,
        locator: &RemoteLocator,
        expected: Option<&ExpectedHead>,
        head: &HeadBytes,
        cancel: &Cancellation,
    ) -> Result<HeadReceipt> {
        let context = self.context(repository)?;
        locator.validate_for(repository)?;
        if locator.object != HEAD_OBJECT || locator.collection.is_some() {
            return Err(corrupt());
        }
        let token = self.access_token(context, cancel).await?;
        let condition = match expected {
            Some(ExpectedHead::Exact(version)) => Some(version.0.clone()),
            _ => None,
        };
        let query = matches!(expected, Some(ExpectedHead::Absent))
            .then_some("@microsoft.graph.conflictBehavior=fail");
        let url = graph::item_url(
            &context.settings,
            &context.root_item_id,
            &locator.object,
            "/content",
            query,
        )?;
        let operation = if expected.is_some() {
            ProviderOperation::CompareExchangeHead
        } else {
            ProviderOperation::ReplaceHead
        };
        let mut request = self.request(
            reqwest::Method::PUT,
            url,
            operation,
            &context.account,
            Some(token.as_str()),
        );
        request.headers.insert(
            "content-type".to_owned(),
            "application/octet-stream".to_owned(),
        );
        if let Some(version) = condition {
            request.headers.insert("if-match".to_owned(), version);
        }
        let body = head.as_bytes().to_vec();
        request.content_length = Some(body.len() as u64);
        request.body = Some(Box::pin(std::io::Cursor::new(body)));
        let mut response = self.send(request, cancel).await?;
        match response.status {
            200 | 201 => {
                let headers = response.headers.clone();
                let version = match graph::json::<graph::Item>(&mut response, cancel).await {
                    Ok(item) => graph::version(&item, &headers),
                    Err(_) => graph::header_version(&headers),
                };
                Ok(HeadReceipt {
                    version,
                    complete: true,
                })
            }
            status @ (409 | 412) => Err(common::error(ErrorKind::PreconditionFailed, status)),
            status => Err(graph::classify(status, &response.headers, self.now())),
        }
    }
}

impl Provider for OneDrive {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let settings = config::validate(config)?;
            let account = AccountKey::new(
                config::PROVIDER_ID,
                &settings.endpoint,
                &settings.account_id,
            )?;
            let context = Context {
                settings,
                secret: secret.clone(),
                root_item_id: String::new(),
                account,
            };
            let token = self.access_token(&context, cancel).await?;
            let request = self.request(
                reqwest::Method::GET,
                graph::root_url(&context.settings)?,
                ProviderOperation::Metadata,
                &context.account,
                Some(token.as_str()),
            );
            let mut response = self.send(request, cancel).await?;
            graph::require(&response, &[200], self.now())?;
            let root: graph::Item = graph::json(&mut response, cancel).await?;
            if root.folder.is_none() {
                return Err(corrupt());
            }
            let root_item_id = root
                .id
                .filter(|id| !id.is_empty() && !id.contains('/') && !id.contains(':'))
                .ok_or_else(corrupt)?;
            let context = Context {
                root_item_id,
                ..context
            };
            let descriptors = config::collection_folder(Collection::Descriptors);
            match mode {
                OpenMode::Existing => {
                    let url = graph::folder_children_url(
                        &context.settings,
                        &context.root_item_id,
                        descriptors,
                        &graph::listing_query(1, None),
                    )?;
                    let request = self.request(
                        reqwest::Method::GET,
                        url,
                        ProviderOperation::List,
                        &context.account,
                        Some(token.as_str()),
                    );
                    let mut response = self.send(request, cancel).await?;
                    if matches!(response.status, 404 | 410) {
                        return Err(common::error(ErrorKind::NotFound, response.status));
                    }
                    graph::require(&response, &[200], self.now())?;
                    let page: graph::ChildrenPage = graph::json(&mut response, cancel).await?;
                    if !page.value.iter().any(|item| item.file.is_some()) {
                        return Err(ProviderError::new(ErrorKind::NotFound));
                    }
                }
                OpenMode::Create => {
                    for folder in config::REPOSITORY_FOLDERS {
                        self.create_repository_folder(&context, token.as_str(), folder, cancel)
                            .await?;
                    }
                }
                OpenMode::ResumeCreate => {
                    self.resume_layout(&context, token.as_str(), cancel).await?;
                }
            }
            let account_type = context.settings.account_type;
            let handle = RepositoryHandle {
                repository_id: context.settings.identity.clone(),
                connection_identity: context.settings.identity.clone(),
                account: context.account.clone(),
                context: Box::new(context),
            };
            Ok((handle, capabilities(account_type)))
        })
    }

    fn read_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            config::validate_relative_path(&locator.object)?;
            let token = self.access_token(context, cancel).await?;
            let url = graph::item_url(
                &context.settings,
                &context.root_item_id,
                &locator.object,
                "/content",
                None,
            )?;
            let mut request = self.request(
                reqwest::Method::GET,
                url.clone(),
                ProviderOperation::DownloadUrl,
                &context.account,
                Some(token.as_str()),
            );
            if let Some(version) = unchanged {
                request
                    .headers
                    .insert("if-none-match".to_owned(), version.0.clone());
            }
            let first = self.send(request, cancel).await?;
            if first.status == 304 {
                return Ok(ReadReceipt::NotModified(
                    unchanged.cloned().ok_or_else(corrupt)?,
                ));
            }
            let announced = graph::header_version(&first.headers);
            let (mut body, headers) = match first.status {
                200 => (first.body, first.headers),
                302 | 303 | 307 => {
                    // The redirect target is pre-authenticated; the bearer token
                    // is deliberately not forwarded to another origin.
                    let hop = self.request(
                        reqwest::Method::GET,
                        graph::redirect_target(&first, &url)?,
                        ProviderOperation::Get,
                        &context.account,
                        None,
                    );
                    let redirected = self.send(hop, cancel).await?;
                    graph::require(&redirected, &[200], self.now())?;
                    (redirected.body, redirected.headers)
                }
                status => return Err(graph::classify(status, &first.headers, self.now())),
            };
            let length = common::content_length(&headers)?.ok_or_else(corrupt)?;
            let (received, sha256) =
                common::stream_to_sink(&mut body, sink, Some(length), length, cancel).await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length: received,
                version: announced.or_else(|| graph::header_version(&headers)),
                checksum: Some(Checksum {
                    algorithm: "sha256".to_owned(),
                    value: sha256,
                    provider_verified: false,
                }),
                complete: true,
            }))
        })
    }

    fn begin_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            intent.validate(repository)?;
            let path = config::object_path(intent.role, &intent.object_id)?;
            if intent.byte_length <= graph::SIMPLE_UPLOAD_MAX_BYTES {
                return Ok(None);
            }
            let token = self.access_token(context, cancel).await?;
            let url = graph::item_url(
                &context.settings,
                &context.root_item_id,
                &path,
                "/createUploadSession",
                None,
            )?;
            let mut request = self.request(
                reqwest::Method::POST,
                url,
                ProviderOperation::UploadSession,
                &context.account,
                Some(token.as_str()),
            );
            let name = serde_json::to_string(&intent.object_id).map_err(|_| corrupt())?;
            Self::json_body(
                &mut request,
                format!(
                    "{{\"item\":{{\"@microsoft.graph.conflictBehavior\":\"fail\",\"name\":{name}}},\"deferCommit\":false}}"
                )
                .into_bytes(),
            );
            let mut response = self.send(request, cancel).await?;
            graph::require(&response, &[200, 201], self.now())?;
            let session: graph::SessionState = graph::json(&mut response, cancel).await?;
            let upload_url = session
                .upload_url
                .filter(|value| url::Url::parse(value).is_ok())
                .ok_or_else(corrupt)?;
            let sealed = tokens::encode_session(&tokens::SealedSession {
                upload_url: zeroize::Zeroizing::new(upload_url),
                object_id: intent.object_id.clone(),
                byte_length: intent.byte_length,
            })?;
            Ok(Some(ResumeState {
                sealed_state: self.deps.vault.store(&sealed).await?,
                confirmed_offset: 0,
                expires_at_ms: graph::expires_at_ms(session.expiration_date_time.as_ref()),
            }))
        })
    }

    fn create_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            intent.validate(repository)?;
            if source.byte_length() != intent.byte_length {
                return Err(corrupt());
            }
            let path = config::object_path(intent.role, &intent.object_id)?;
            match resume {
                Some(state) => {
                    self.continue_session(context, intent, source, state, &path, cancel)
                        .await
                }
                None => {
                    self.single_put(context, intent, source, &path, cancel)
                        .await
                }
            }
        })
    }

    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        expected: &'a ExpectedHead,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.write_head(repository, locator, Some(expected), head, cancel)
                .await
        })
    }

    fn replace_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.write_head(repository, locator, None, head, cancel)
                .await
        })
    }

    /// Graph moves a deleted item to the recycle bin and answers 204 once the
    /// item is gone from the drive. That is the service's own behaviour and the
    /// adapter does not ask for a permanent removal on top of it.
    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            let path = config::removable_path(locator)?;
            let token = self.access_token(context, cancel).await?;
            let url = graph::item_url(
                &context.settings,
                &context.root_item_id,
                &path,
                "",
                None,
            )?;
            let request = self.request(
                reqwest::Method::DELETE,
                url,
                ProviderOperation::Delete,
                &context.account,
                Some(token.as_str()),
            );
            let response = self.send(request, cancel).await?;
            match response.status {
                200 | 204 | 404 => Ok(()),
                status => Err(graph::classify(status, &response.headers, self.now())),
            }
        })
    }

    fn list_objects<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        collection: Collection,
        cursor: Option<&'a str>,
        limit: u16,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            if limit == 0 || limit > 1000 {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            if cursor.is_some_and(|cursor| cursor.is_empty() || cursor.len() > 4096) {
                return Err(corrupt());
            }
            let folder = config::collection_folder(collection);
            let token = self.access_token(context, cancel).await?;
            let url = graph::folder_children_url(
                &context.settings,
                &context.root_item_id,
                folder,
                &graph::listing_query(limit, cursor),
            )?;
            let request = self.request(
                reqwest::Method::GET,
                url,
                ProviderOperation::List,
                &context.account,
                Some(token.as_str()),
            );
            let mut response = self.send(request, cancel).await?;
            graph::require(&response, &[200], self.now())?;
            let page: graph::ChildrenPage = graph::json(&mut response, cancel).await?;
            let mut objects = Vec::new();
            for item in &page.value {
                // Folders and names this adapter could never have written are
                // not repository objects.
                let (Some(name), Some(size)) = (item.name.as_deref(), item.size) else {
                    continue;
                };
                if item.file.is_none() {
                    continue;
                }
                let path = format!("{folder}/{name}");
                if config::validate_relative_path(&path).is_err() {
                    continue;
                }
                objects.push(ObjectReceipt {
                    locator: self.locator(context, folder, path),
                    byte_length: size,
                    version: item
                        .e_tag
                        .clone()
                        .filter(|tag| !tag.is_empty())
                        .map(VersionToken),
                    checksum: None,
                    complete: true,
                });
            }
            let next_cursor = page
                .next_link
                .as_deref()
                .map(|link| graph::skip_token(link, &context.settings))
                .transpose()?;
            Ok(ObjectPage {
                objects,
                next_cursor,
            })
        })
    }

    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            intent.validate(repository)?;
            let path = config::object_path(intent.role, &intent.object_id)?;
            if let Some(resume) = resume {
                let session = self.open_session(&resume.sealed_state, intent).await?;
                let url = url::Url::parse(&session.upload_url).map_err(|_| corrupt())?;
                let request = self.request(
                    reqwest::Method::GET,
                    url,
                    ProviderOperation::ReconcileUpload,
                    &context.account,
                    None,
                );
                let mut response = self.send(request, cancel).await?;
                match response.status {
                    200 => {
                        let state: graph::SessionState = graph::json(&mut response, cancel).await?;
                        let expires_at_ms =
                            graph::expires_at_ms(state.expiration_date_time.as_ref());
                        if let Some(offset) = state
                            .next_expected_ranges
                            .as_deref()
                            .and_then(graph::confirmed_offset)
                            .filter(|offset| *offset < intent.byte_length)
                        {
                            return Ok(UploadResolution::Resumable(ResumeState {
                                sealed_state: resume.sealed_state.clone(),
                                confirmed_offset: offset,
                                expires_at_ms,
                            }));
                        }
                    }
                    // The session is gone: it either committed or expired.
                    404 | 410 => (),
                    status => return Err(graph::classify(status, &response.headers, self.now())),
                }
            }
            match self.fetch_item(context, &path, cancel).await? {
                None => Ok(UploadResolution::RestartRequired),
                Some((item, headers)) if Self::stores(&item, intent.byte_length) => {
                    Ok(UploadResolution::Complete(
                        self.object_receipt(context, intent, &path, &item, &headers)?,
                    ))
                }
                // Another object of a different length holds this identity.
                Some(_) => Ok(UploadResolution::Conflict),
            }
        })
    }

    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        self.context(repository)?;
        Ok(RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: HEAD_OBJECT.to_owned(),
        })
    }

}
