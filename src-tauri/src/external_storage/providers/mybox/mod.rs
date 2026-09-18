//! Naver MYBOX Open API adapter, authenticated with a personal access token.
//!
//! `ConnectionConfig`
//! - `provider` is `"mybox"` and `oauth_profile` must be absent: MYBOX issues
//!   user created tokens instead of OAuth grants.
//! - `endpoint` is the Open API base, `https://open-api.mybox.naver.com/v1`.
//!   Plain HTTP is accepted only for `127.0.0.1`, which the wire fixture uses.
//! - `account_id` names the MYBOX account. The API exposes no account
//!   identifier, so this value is user supplied and only scopes the connection
//!   identity and the shared request budget.
//! - `profile` is the plan tier whose documented allowances the owning ledger
//!   configures: `plan30gb` (500 downloads/day, 60 calls/minute per API),
//!   `plan80gb` (1,000/60), `plan180gb` (1,000/240), `plan2tb` (2,000/240),
//!   `plan5tb` (5,000/240), `plan10tb` (20,000/240), `plan20tb` (50,000/240).
//!   Absent means the smallest allowance.
//! - `location["rootFolderName"]` is the repository folder directly under the
//!   drive root. `location["rootFolderId"]` optionally pins its resource id and
//!   is a hint only: it is never part of the connection identity.
//!
//! The vault secret is UTF-8 JSON with exactly two fields:
//! `{"pat":"<personal access token>","expiresAtMs":<unix milliseconds>}`.
//! Tokens are never refreshed here. A stored expiry in the past, a missing
//! secret and a 401 all report `ReauthRequired` without a retry instant, so the
//! user creates a new token.
//!
//! Objects live at `<root>/<role folder>/<object id>.bin` and a
//! `RemoteLocator.object` is that `<role folder>/<file name>` pair. Names are
//! percent encoded into `[A-Za-z0-9._%-]` so they need no escaping anywhere.
//! Heads live in `heads/` and are the only mutable files.
mod api;
mod config;
#[cfg(test)]
mod tests;

use super::{common, Dependencies};
use crate::external_storage::{
    auth::SecretBytes,
    capabilities::Capabilities,
    contract::*,
    http::{self, HttpRequest, HttpResponse},
    quota::AccountKey,
    quota_profiles::MyboxPlan,
};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The listing API pages at most 1,000 entries per request.
const PAGE_SIZE: u16 = 1000;
/// The one mutable head, kept inside the heads folder like any other head write.
const HEAD_NAME: &str = "head.bin";
const MAX_LIST_PAGES: usize = 256;

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(Mybox { deps: dependencies }))
}

struct Mybox {
    deps: Dependencies,
}

#[derive(Clone)]
struct Entry {
    id: String,
    size: u64,
}
#[derive(Default)]
struct Listed {
    files: BTreeMap<String, Entry>,
    complete: bool,
}
/// Resolved once per open. MYBOX addresses files by resource id only, so a
/// name has to be looked up in its role folder; the listing is cached and
/// dropped again whenever a write could have changed it.
struct Context {
    identity: String,
    account: AccountKey,
    plan: MyboxPlan,
    base: url::Url,
    secret: SecretRef,
    folders: BTreeMap<String, String>,
    max_file_bytes: u64,
    free_bytes: u64,
    entries: Mutex<BTreeMap<String, Listed>>,
}
impl Context {
    fn folder_id(&self, folder: &str) -> Result<&str> {
        self.folders
            .get(folder)
            .map(String::as_str)
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))
    }
    /// `None` when the folder has not been listed, otherwise its answer.
    fn cached(&self, folder: &str, name: &str) -> Option<Option<Entry>> {
        let entries = self.entries.lock().ok()?;
        let listed = entries.get(folder)?;
        listed.complete.then(|| listed.files.get(name).cloned())
    }
    fn store(&self, folder: &str, files: BTreeMap<String, Entry>) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                folder.to_owned(),
                Listed {
                    files,
                    complete: true,
                },
            );
        }
    }
    fn record(&self, folder: &str, name: &str, entry: Entry) {
        if let Ok(mut entries) = self.entries.lock() {
            entries
                .entry(folder.to_owned())
                .or_default()
                .files
                .insert(name.to_owned(), entry);
        }
    }
    fn forget(&self, folder: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(folder);
        }
    }
}

/// Sealed upload session. The issued URL never leaves the vault and is never
/// stored in a locator.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Session {
    upload_url: String,
    folder: String,
    name: String,
    parent_id: String,
    byte_length: u64,
    modified_time: String,
}

fn receipt(context: &Context, folder: &str, name: &str, intent: &ObjectIntent) -> ObjectReceipt {
    ObjectReceipt {
        locator: config::locator(&context.identity, folder, name),
        byte_length: intent.byte_length,
        version: None,
        // MYBOX publishes no digest for a stored file; this is the intent's own
        // hash, which the service did not check.
        checksum: Some(Checksum {
            algorithm: "sha256".into(),
            value: intent.sha256.clone(),
            provider_verified: false,
        }),
        complete: true,
    }
}

impl Mybox {
    fn context<'a>(&self, repository: &'a RepositoryHandle) -> Result<&'a Context> {
        repository
            .context
            .downcast_ref::<Context>()
            .filter(|context| context.identity == repository.connection_identity)
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))
    }
    fn guard(&self, context: &Context, byte_length: u64) -> Result<()> {
        if byte_length > context.max_file_bytes {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        if byte_length > context.free_bytes {
            return Err(ProviderError::new(ErrorKind::StorageFull));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        context: &Context,
        operation: ProviderOperation,
        method: Method,
        url: url::Url,
        headers: BTreeMap<String, String>,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
        content_length: Option<u64>,
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        let mut headers = headers;
        if http::control_operation(operation) {
            http::bypass_cache(&mut headers);
        }
        let api_request = url.origin() == context.base.origin();
        let response = self.deps.send(
            HttpRequest {
                method,
                url,
                headers,
                body,
                content_length,
                operation,
                account: context.account.clone(),
                api_request,
                mybox_charge: api::charge(context.plan, operation),
                control: http::control_operation(operation),
            },
            cancel,
        )
        .await?;
        if response.status == 423 {
            let now = self.deps.clock.now_ms();
            let error = api::classify(response.status, &response.headers, now);
            self.deps.requests.observe_classified_error(
                &context.account,
                &error,
                &response.headers,
                now,
            )?;
        }
        Ok(response)
    }
    /// Open API call. The token is read from the vault per request so a revoked
    /// or expired credential stops an already open repository.
    async fn call(
        &self,
        context: &Context,
        operation: ProviderOperation,
        method: Method,
        url: url::Url,
        json: Option<serde_json::Value>,
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        let bearer = config::bearer(
            self.deps.vault.as_ref(),
            &context.secret,
            self.deps.clock.now_ms(),
        )
        .await?;
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), bearer.to_string());
        headers.insert("accept".to_owned(), "application/json".to_owned());
        let (body, content_length) = match json {
            Some(value) => {
                let bytes = serde_json::to_vec(&value)
                    .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
                headers.insert("content-type".to_owned(), "application/json".to_owned());
                let length = bytes.len() as u64;
                (
                    Some(Box::pin(std::io::Cursor::new(bytes)) as Pin<Box<dyn AsyncRead + Send>>),
                    Some(length),
                )
            }
            None => (None, None),
        };
        self.send(
            context,
            operation,
            method,
            url,
            headers,
            body,
            content_length,
            cancel,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        context: &Context,
        operation: ProviderOperation,
        method: Method,
        url: url::Url,
        body: Option<serde_json::Value>,
        allowed: &[u16],
        cancel: &Cancellation,
    ) -> Result<T> {
        let mut response = self
            .call(context, operation, method, url, body, cancel)
            .await?;
        if !allowed.contains(&response.status) {
            return Err(api::classify(
                response.status,
                &response.headers,
                self.deps.clock.now_ms(),
            ));
        }
        let bytes =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        serde_json::from_slice(&bytes).map_err(|_| ProviderError::new(ErrorKind::Corrupt))
    }

    async fn list(
        &self,
        context: &Context,
        folder_id: Option<&str>,
        cursor: Option<&str>,
        count: u16,
        cancel: &Cancellation,
    ) -> Result<api::Listing> {
        let mut url = match folder_id {
            Some(id) => config::endpoint(&context.base, &["drive", "folders", id, "resources"])?,
            None => config::endpoint(&context.base, &["drive", "resources"])?,
        };
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("count", &count.to_string());
            query.append_pair("sort", "name,asc");
            if let Some(cursor) = cursor {
                query.append_pair("cursor", cursor);
            }
        }
        self.json(
            context,
            ProviderOperation::List,
            Method::GET,
            url,
            None,
            &[200],
            cancel,
        )
        .await
    }
    async fn pages(
        &self,
        context: &Context,
        folder_id: Option<&str>,
        cancel: &Cancellation,
    ) -> Result<Vec<api::Resource>> {
        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..MAX_LIST_PAGES {
            let page = self
                .list(context, folder_id, cursor.as_deref(), PAGE_SIZE, cancel)
                .await?;
            let next = page.cursor()?.map(str::to_owned);
            collected.extend(page.resources);
            match next {
                Some(value) => {
                    if !seen.insert(value.clone()) {
                        return Err(ProviderError::new(ErrorKind::Corrupt));
                    }
                    cursor = Some(value);
                }
                None => return Ok(collected),
            }
        }
        Err(ProviderError::new(ErrorKind::Corrupt))
    }
    async fn load(&self, context: &Context, folder: &str, cancel: &Cancellation) -> Result<()> {
        let folder_id = context.folder_id(folder)?.to_owned();
        let files = self
            .pages(context, Some(&folder_id), cancel)
            .await?
            .into_iter()
            .filter(|resource| resource.kind == "file")
            .map(|resource| {
                (
                    resource.name,
                    Entry {
                        id: resource.resource_id,
                        size: resource.size,
                    },
                )
            })
            .collect();
        context.store(folder, files);
        Ok(())
    }
    async fn lookup(
        &self,
        context: &Context,
        folder: &str,
        name: &str,
        cancel: &Cancellation,
    ) -> Result<Option<Entry>> {
        if let Some(found) = context.cached(folder, name) {
            return Ok(found);
        }
        self.load(context, folder, cancel).await?;
        Ok(context.cached(folder, name).unwrap_or_default())
    }
    /// Re-reads the role folder after a write so the answer is the service's.
    async fn confirm(
        &self,
        context: &Context,
        folder: &str,
        name: &str,
        cancel: &Cancellation,
    ) -> Result<Option<Entry>> {
        context.forget(folder);
        self.load(context, folder, cancel).await?;
        Ok(context.cached(folder, name).unwrap_or_default())
    }

    async fn create_folder(
        &self,
        context: &Context,
        parent: Option<&str>,
        name: &str,
        cancel: &Cancellation,
    ) -> Result<String> {
        let mut body = serde_json::Map::new();
        body.insert("folderName".into(), name.into());
        if let Some(parent) = parent {
            body.insert("parentId".into(), parent.into());
        }
        let folder: api::Folder = self
            .json(
                context,
                ProviderOperation::Create,
                Method::POST,
                config::endpoint(&context.base, &["drive", "folders"])?,
                Some(serde_json::Value::Object(body)),
                &[200, 201],
                cancel,
            )
            .await?;
        Ok(folder.resource_id)
    }
    #[allow(clippy::too_many_arguments)]
    async fn issue_upload(
        &self,
        context: &Context,
        parent_id: &str,
        name: &str,
        byte_length: u64,
        modified_time: &str,
        overwrite: bool,
        resume: bool,
        operation: ProviderOperation,
        cancel: &Cancellation,
    ) -> Result<api::Upload> {
        let mut body = serde_json::Map::new();
        body.insert("fileName".into(), name.into());
        body.insert("fileSize".into(), byte_length.into());
        body.insert("parentId".into(), parent_id.into());
        body.insert("isOverwrite".into(), overwrite.into());
        body.insert("modifiedTime".into(), modified_time.into());
        if resume {
            body.insert("resume".into(), true.into());
        }
        self.json(
            context,
            operation,
            Method::POST,
            config::endpoint(&context.base, &["drive", "files"])?,
            Some(serde_json::Value::Object(body)),
            &[200, 201],
            cancel,
        )
        .await
    }
    /// One POST of the remaining bytes to the issued URL. No Authorization is
    /// forwarded: the URL carries its own storage token on another origin.
    async fn upload_body(
        &self,
        context: &Context,
        session: &Session,
        source: &dyn TransferSource,
        offset: u64,
        operation: ProviderOperation,
        cancel: &Cancellation,
    ) -> Result<()> {
        let url = config::transfer_url(&session.upload_url)?;
        let form = api::form(&session.name);
        let length = session
            .byte_length
            .checked_sub(offset)
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        let reader = source.open(offset, length, cancel).await?;
        let content_length = form.prefix.len() as u64 + length + form.suffix.len() as u64;
        let body = Box::pin(
            std::io::Cursor::new(form.prefix)
                .chain(reader)
                .chain(std::io::Cursor::new(form.suffix)),
        ) as Pin<Box<dyn AsyncRead + Send>>;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), form.content_type);
        let response = self
            .send(
                context,
                operation,
                Method::POST,
                url,
                headers,
                Some(body),
                Some(content_length),
                cancel,
            )
            .await?;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(api::classify_transfer(
                response.status,
                &response.headers,
                self.deps.clock.now_ms(),
            ))
        }
    }

    async fn seal(&self, session: &Session, existing: Option<&SecretRef>) -> Result<SecretRef> {
        let bytes = SecretBytes(zeroize::Zeroizing::new(
            serde_json::to_vec(session).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?,
        ));
        match existing {
            Some(reference) => {
                self.deps.vault.replace(reference, &bytes).await?;
                Ok(reference.clone())
            }
            None => self.deps.vault.store(&bytes).await,
        }
    }
    async fn unseal(
        &self,
        context: &Context,
        state: &ResumeState,
        intent: &ObjectIntent,
    ) -> Result<Session> {
        let stored = self.deps.vault.read(&state.sealed_state).await?;
        let session: Session = serde_json::from_slice(stored.0.as_slice())
            .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        let folder = config::role_folder(intent.role);
        if session.folder != folder
            || session.name != config::object_name(&intent.object_id)?
            || session.byte_length != intent.byte_length
            || session.parent_id != context.folder_id(folder)?
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(session)
    }
    async fn settle(
        &self,
        context: &Context,
        folder: &str,
        name: &str,
        intent: &ObjectIntent,
        cancel: &Cancellation,
    ) -> Result<UploadResolution> {
        Ok(match self.confirm(context, folder, name, cancel).await? {
            Some(entry) if entry.size == intent.byte_length => {
                UploadResolution::Complete(receipt(context, folder, name, intent))
            }
            Some(_) => UploadResolution::Conflict,
            None => UploadResolution::RestartRequired,
        })
    }

    async fn discover_root(
        &self,
        context: &Context,
        connection: &config::Connection,
        cancel: &Cancellation,
    ) -> Result<Option<String>> {
        if let Some(id) = &connection.root_folder_id {
            let resource: api::Resource = self
                .json(
                    context,
                    ProviderOperation::Metadata,
                    Method::GET,
                    config::endpoint(&context.base, &["drive", "resources", id.as_str()])?,
                    None,
                    &[200],
                    cancel,
                )
                .await?;
            if resource.kind != "folder" || resource.name != connection.root_folder_name {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            return Ok(Some(resource.resource_id));
        }
        let mut matches = self
            .pages(context, None, cancel)
            .await?
            .into_iter()
            .filter(|resource| resource.name == connection.root_folder_name);
        let found = matches.next();
        if matches.next().is_some() {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        match found {
            Some(resource) if resource.kind == "folder" => Ok(Some(resource.resource_id)),
            Some(_) => Err(ProviderError::new(ErrorKind::PreconditionFailed)),
            None => Ok(None),
        }
    }

    async fn resume_folders(
        &self,
        context: &mut Context,
        root_id: &str,
        children: Vec<api::Resource>,
        cancel: &Cancellation,
    ) -> Result<()> {
        for resource in children {
            let Some(folder) = config::FOLDERS
                .iter()
                .find(|folder| **folder == resource.name.as_str())
            else {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            };
            if resource.kind != "folder"
                || context
                    .folders
                    .insert((*folder).to_owned(), resource.resource_id)
                    .is_some()
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        for folder in config::FOLDERS {
            if !context.folders.contains_key(folder) {
                let id = self
                    .create_folder(context, Some(root_id), folder, cancel)
                    .await?;
                context.folders.insert(folder.to_owned(), id);
            }
        }
        Ok(())
    }

    async fn require_initial_contents(
        &self,
        context: &Context,
        cancel: &Cancellation,
    ) -> Result<()> {
        for folder in config::FOLDERS {
            let folder_id = context.folder_id(folder)?;
            let resources = self.pages(context, Some(folder_id), cancel).await?;
            if folder != config::DESCRIPTORS {
                if !resources.is_empty() {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                continue;
            }
            if resources.len() > 1 {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            if let Some(resource) = resources.into_iter().next() {
                if resource.kind != "file"
                    || resource.size == 0
                    || !config::valid_name(&resource.name)
                    || !resource.name.ends_with(".bin")
                {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
            }
        }
        Ok(())
    }
}

impl Provider for Mybox {
    fn open_repository<'a>(
        &'a self,
        settings: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let connection = config::parse(settings)?;
            let mut context = Context {
                identity: connection.identity.clone(),
                account: AccountKey::new(
                    config::PROVIDER_ID,
                    &connection.base,
                    &connection.account_id,
                )?,
                plan: connection.plan,
                base: connection.base.clone(),
                secret: secret.clone(),
                folders: BTreeMap::new(),
                max_file_bytes: 0,
                free_bytes: 0,
                entries: Mutex::new(BTreeMap::new()),
            };
            // The storage call both proves the token works and reports the
            // account's own maximum file size.
            let storage: api::Storage = self
                .json(
                    &context,
                    ProviderOperation::Metadata,
                    Method::GET,
                    config::endpoint(&context.base, &["drive", "storage"])?,
                    None,
                    &[200],
                    cancel,
                )
                .await?;
            context.max_file_bytes = storage.max_file_bytes;
            context.free_bytes = storage.quota_bytes.saturating_sub(storage.used_bytes);

            let discovered = self.discover_root(&context, &connection, cancel).await?;
            let root_id = match (discovered, mode) {
                (None, OpenMode::Existing) => return Err(ProviderError::new(ErrorKind::NotFound)),
                (None, OpenMode::Create | OpenMode::ResumeCreate) => {
                    self.create_folder(&context, None, &connection.root_folder_name, cancel)
                        .await?
                }
                (Some(root_id), mode) => {
                    let children = self.pages(&context, Some(&root_id), cancel).await?;
                    match mode {
                        // An existing folder may hold anything; adopting it
                        // would hide or reuse material this repository did not
                        // create.
                        OpenMode::Create if !children.is_empty() => {
                            return Err(ProviderError::new(ErrorKind::PreconditionFailed))
                        }
                        OpenMode::Create => {}
                        OpenMode::Existing => {
                            for folder in config::FOLDERS {
                                let found = children.iter().find(|resource| {
                                    resource.kind == "folder" && resource.name == folder
                                });
                                match found {
                                    Some(resource) => {
                                        context
                                            .folders
                                            .insert(folder.into(), resource.resource_id.clone());
                                    }
                                    None => return Err(ProviderError::new(ErrorKind::NotFound)),
                                }
                            }
                        }
                        OpenMode::ResumeCreate => {
                            self.resume_folders(&mut context, &root_id, children, cancel)
                                .await?;
                        }
                    }
                    root_id
                }
            };
            if mode == OpenMode::Create {
                for folder in config::FOLDERS {
                    let id = self
                        .create_folder(&context, Some(&root_id), folder, cancel)
                        .await?;
                    context.folders.insert(folder.into(), id);
                }
            } else if mode == OpenMode::ResumeCreate {
                if context.folders.is_empty() {
                    self.resume_folders(&mut context, &root_id, Vec::new(), cancel)
                        .await?;
                }
                self.require_initial_contents(&context, cancel).await?;
            } else {
                // A repository without a descriptor is not one this adapter may
                // hand back as existing.
                self.load(&context, config::DESCRIPTORS, cancel).await?;
                let empty = context
                    .entries
                    .lock()
                    .map_err(|_| ProviderError::new(ErrorKind::Transient))?
                    .get(config::DESCRIPTORS)
                    .is_none_or(|listed| listed.files.is_empty());
                if empty {
                    return Err(ProviderError::new(ErrorKind::NotFound));
                }
            }
            let capabilities = Capabilities {
                immutable_create: true,
                direct_complete_read: true,
                // isOverwrite is not a conditional write.
                atomic_create_head: false,
                conditional_head_update: false,
                stable_head_replace: true,
                head_read_after_write: true,
                head_retry_control: true,
                snapshot_discovery: true,
                lease_operations: true,
                delete_objects: true,
                conditional_get: false,
                range: false,
                resumable_upload: true,
                max_stored_bytes: Some(storage.max_file_bytes),
                sdk_overhead_bytes: 0,
                upload_alignment: 1,
            };
            Ok((
                RepositoryHandle {
                    repository_id: root_id,
                    connection_identity: connection.identity,
                    account: context.account.clone(),
                    context: Box::new(context),
                },
                capabilities,
            ))
        })
    }

    fn read_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        // MYBOX publishes no version token for a file, so a caller supplied
        // token can never match and the body is always fetched.
        _unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            let (folder, name) = config::parse_locator(locator)?;
            let entry = self
                .lookup(context, folder, name, cancel)
                .await?
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            // The one-time URL is issued immediately before the body request
            // and never stored.
            let issued: api::Download = self
                .json(
                    context,
                    ProviderOperation::DownloadUrl,
                    Method::GET,
                    config::endpoint(
                        &context.base,
                        &["drive", "files", entry.id.as_str(), "download"],
                    )?,
                    None,
                    &[200],
                    cancel,
                )
                .await?;
            let url = config::transfer_url(&issued.download_url)?;
            let mut response = self
                .send(
                    context,
                    ProviderOperation::Get,
                    Method::GET,
                    url,
                    BTreeMap::new(),
                    None,
                    None,
                    cancel,
                )
                .await?;
            if !(200..300).contains(&response.status) {
                return Err(api::classify_transfer(
                    response.status,
                    &response.headers,
                    self.deps.clock.now_ms(),
                ));
            }
            if common::content_length(&response.headers)?
                .is_some_and(|declared| declared != entry.size)
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let (byte_length, hash) = common::stream_to_sink(
                &mut response.body,
                sink,
                Some(entry.size),
                entry.size,
                cancel,
            )
            .await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length,
                version: None,
                checksum: Some(Checksum {
                    algorithm: "sha256".into(),
                    value: hash,
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
            let folder = config::role_folder(intent.role);
            let name = config::object_name(&intent.object_id)?;
            self.guard(context, intent.byte_length)?;
            if self.lookup(context, folder, &name, cancel).await?.is_some() {
                return Ok(None);
            }
            let now = self.deps.clock.now_ms();
            let modified_time = api::modified_time(now)?;
            let parent_id = context.folder_id(folder)?.to_owned();
            let issued = self
                .issue_upload(
                    context,
                    &parent_id,
                    &name,
                    intent.byte_length,
                    &modified_time,
                    false,
                    false,
                    ProviderOperation::UploadSession,
                    cancel,
                )
                .await?;
            config::transfer_url(&issued.upload_url)?;
            let sealed_state = self
                .seal(
                    &Session {
                        upload_url: issued.upload_url,
                        folder: folder.into(),
                        name,
                        parent_id,
                        byte_length: intent.byte_length,
                        modified_time,
                    },
                    None,
                )
                .await?;
            Ok(Some(ResumeState {
                sealed_state,
                confirmed_offset: 0,
                expires_at_ms: Some(now.saturating_add(api::UPLOAD_URL_LIFETIME_MS)),
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
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let folder = config::role_folder(intent.role);
            let name = config::object_name(&intent.object_id)?;
            let (session, offset) = match resume {
                Some(state) => {
                    let session = self.unseal(context, state, intent).await?;
                    if state.confirmed_offset > intent.byte_length
                        || state
                            .expires_at_ms
                            .is_some_and(|at| at <= self.deps.clock.now_ms())
                    {
                        // The sealed URL can no longer take bytes; the owner
                        // reconciles to obtain a new one.
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                    (session, state.confirmed_offset)
                }
                None => {
                    if let Some(entry) = self.lookup(context, folder, &name, cancel).await? {
                        return if entry.size == intent.byte_length {
                            Ok(receipt(context, folder, &name, intent))
                        } else {
                            Err(ProviderError::new(ErrorKind::PreconditionFailed))
                        };
                    }
                    self.guard(context, intent.byte_length)?;
                    let modified_time = api::modified_time(self.deps.clock.now_ms())?;
                    let parent_id = context.folder_id(folder)?.to_owned();
                    let issued = self
                        .issue_upload(
                            context,
                            &parent_id,
                            &name,
                            intent.byte_length,
                            &modified_time,
                            false,
                            false,
                            ProviderOperation::UploadSession,
                            cancel,
                        )
                        .await?;
                    (
                        Session {
                            upload_url: issued.upload_url,
                            folder: folder.into(),
                            name: name.clone(),
                            parent_id,
                            byte_length: intent.byte_length,
                            modified_time,
                        },
                        0,
                    )
                }
            };
            self.upload_body(
                context,
                &session,
                source,
                offset,
                ProviderOperation::UploadChunk,
                cancel,
            )
            .await?;
            // The storage domain's completion body is not documented, so the
            // stored file itself is the evidence.
            match self.confirm(context, folder, &name, cancel).await? {
                Some(entry) if entry.size == intent.byte_length => {
                    Ok(receipt(context, folder, &name, intent))
                }
                Some(_) => Err(ProviderError::new(ErrorKind::PreconditionFailed)),
                None => Err(ProviderError::new(ErrorKind::Transient)),
            }
        })
    }

    fn compare_exchange_head<'a>(
        &'a self,
        _repository: &'a RepositoryHandle,
        _locator: &'a RemoteLocator,
        _expected: &'a ExpectedHead,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        // MYBOX documents no conditional write primitive. Emulating one from a
        // read and a write would not be atomic.
        Box::pin(async move {
            cancel.check()?;
            Err(ProviderError::new(ErrorKind::Unsupported))
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
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            let (folder, name) = config::parse_locator(locator)?;
            if folder != config::HEADS {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let bytes = head.as_bytes().to_vec();
            let byte_length = bytes.len() as u64;
            self.guard(context, byte_length)?;
            let modified_time = api::modified_time(self.deps.clock.now_ms())?;
            let parent_id = context.folder_id(folder)?.to_owned();
            let issued = self
                .issue_upload(
                    context,
                    &parent_id,
                    name,
                    byte_length,
                    &modified_time,
                    true,
                    false,
                    ProviderOperation::UploadSession,
                    cancel,
                )
                .await?;
            let session = Session {
                upload_url: issued.upload_url,
                folder: folder.into(),
                name: name.to_owned(),
                parent_id,
                byte_length,
                modified_time,
            };
            context.forget(folder);
            // Exactly one write. An ambiguous answer stays ambiguous: the owner
            // re-reads the head instead of this adapter retrying.
            self.upload_body(
                context,
                &session,
                &HeadSource(bytes),
                0,
                ProviderOperation::ReplaceHead,
                cancel,
            )
            .await?;
            Ok(HeadReceipt {
                version: None,
                complete: true,
            })
        })
    }

    /// The service resolves a member by resource id, so the name is looked up
    /// in the role folder first. A folder whose cached entry is already gone
    /// answers 404, which is reported as a finished deletion.
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
            let (folder, name) = config::parse_locator(locator)?;
            if !config::removable_folder(folder) {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let Some(entry) = self.lookup(context, folder, name, cancel).await? else {
                return Ok(());
            };
            let response = self
                .call(
                    context,
                    ProviderOperation::Delete,
                    Method::DELETE,
                    config::endpoint(
                        &context.base,
                        &["drive", "resources", entry.id.as_str()],
                    )?,
                    None,
                    cancel,
                )
                .await?;
            context.forget(folder);
            match response.status {
                200 | 204 | 404 => Ok(()),
                status => Err(api::classify(
                    status,
                    &response.headers,
                    self.deps.clock.now_ms(),
                )),
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
            if limit == 0 || limit > PAGE_SIZE {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            if cursor.is_some_and(|cursor| {
                cursor.is_empty() || cursor.len() > 4096 || cursor.chars().any(char::is_control)
            }) {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let folder = config::collection_folder(collection);
            let folder_id = context.folder_id(folder)?.to_owned();
            let page = self
                .list(context, Some(&folder_id), cursor, limit, cancel)
                .await?;
            let next_cursor = page.cursor()?.map(str::to_owned);
            let mut objects = Vec::new();
            for resource in page.resources {
                if resource.kind != "file" || !config::valid_name(&resource.name) {
                    continue;
                }
                context.record(
                    folder,
                    &resource.name,
                    Entry {
                        id: resource.resource_id,
                        size: resource.size,
                    },
                );
                objects.push(ObjectReceipt {
                    locator: config::locator(&context.identity, folder, &resource.name),
                    byte_length: resource.size,
                    version: None,
                    checksum: None,
                    complete: true,
                });
            }
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
            let Some(resume) = resume else {
                return self
                    .settle(
                        context,
                        config::role_folder(intent.role),
                        &config::object_name(&intent.object_id)?,
                        intent,
                        cancel,
                    )
                    .await;
            };
            let session = self.unseal(context, resume, intent).await?;
            let folder = config::role_folder(intent.role);
            let name = session.name.clone();
            let now = self.deps.clock.now_ms();
            // Re-issuing with `resume` is the service's own statement about how
            // many bytes it holds; a spent or missing session is refused here.
            let issued = self
                .issue_upload(
                    context,
                    &session.parent_id,
                    &name,
                    session.byte_length,
                    &session.modified_time,
                    false,
                    true,
                    ProviderOperation::ReconcileUpload,
                    cancel,
                )
                .await;
            match issued {
                Ok(upload) if upload.offset < session.byte_length => {
                    config::transfer_url(&upload.upload_url)?;
                    let sealed_state = self
                        .seal(
                            &Session {
                                upload_url: upload.upload_url,
                                ..session
                            },
                            Some(&resume.sealed_state),
                        )
                        .await?;
                    Ok(UploadResolution::Resumable(ResumeState {
                        sealed_state,
                        confirmed_offset: upload.offset,
                        expires_at_ms: Some(now.saturating_add(api::UPLOAD_URL_LIFETIME_MS)),
                    }))
                }
                Ok(_) => self.settle(context, folder, &name, intent, cancel).await,
                Err(error) if matches!(error.http_status, Some(404 | 409 | 422)) => {
                    self.settle(context, folder, &name, intent, cancel).await
                }
                Err(error) => Err(error),
            }
        })
    }

    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        let context = self.context(repository)?;
        Ok(config::locator(&context.identity, config::HEADS, HEAD_NAME))
    }

}

/// Heads are at most 64 KiB and already in memory when they are published.
struct HeadSource(Vec<u8>);
impl TransferSource for HeadSource {
    fn byte_length(&self) -> u64 {
        self.0.len() as u64
    }
    fn open<'a>(
        &'a self,
        offset: u64,
        length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncRead + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= self.0.len() as u64)
                .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
            Ok(Box::pin(std::io::Cursor::new(
                self.0[offset as usize..end as usize].to_vec(),
            )) as Pin<Box<dyn AsyncRead + Send>>)
        })
    }
}
