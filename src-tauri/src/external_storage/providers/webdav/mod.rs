//! Generic WebDAV over HTTPS with an application password, plus a Koofr preset.
//!
//! `ConnectionConfig`:
//! - `provider` is `webdav`.
//! - `profile` is absent for a generic server, or `koofr` for Koofr's own
//!   space, which then requires the documented `https://app.koofr.net/dav/…`
//!   endpoint.
//! - `endpoint` is the DAV base URL, `https` (`http` only for `127.0.0.1`),
//!   without credentials, query or fragment. Trailing slashes are normalised
//!   away so two devices derive the same connection identity.
//! - `account_id` is the WebDAV user name, sent as the Basic credentials
//!   user-id, so it may not contain a colon.
//! - `location` holds exactly one key, `root`: the collection path below the
//!   endpoint that holds this repository. Segments are raw names; every one of
//!   them is percent-encoded per request.
//! - `oauth_profile` must be absent. WebDAV has no authorization-code flow, so
//!   this module exposes no authorization policy and no token refresh.
//!
//! The secret is the application password as raw UTF-8 bytes. It is not JSON,
//! not base64 and carries no other field; the adapter builds the Basic
//! credentials from `account_id` and those bytes.
//!
//! Below the root, each object role owns a collection (`descriptors`, `packs`,
//! `catalogs`, `snapshots`, `points`, `inventory`, `leases`) and the head is the root member
//! `head`.
//! A `RemoteLocator.object` is the slash-joined raw path of an object relative
//! to the root, which any device can resolve on its own.
mod multistatus;
mod paths;
#[cfg(test)]
mod tests;

use super::{common, Dependencies};
use crate::external_storage::{
    auth::SecretBytes,
    capabilities::Capabilities,
    contract::*,
    http::{self, HttpRequest, HttpResponse},
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use url::Url;
use zeroize::Zeroizing;

pub(crate) const PROVIDER_ID: &str = "webdav";
/// The repository head. `OpenMode::Create` refuses a root that already holds
/// this member or the descriptor collection.
pub(crate) const HEAD_OBJECT: &str = "head";
/// Koofr's documented WebDAV address for the user's own space.
pub(crate) const KOOFR_ENDPOINT: &str = "https://app.koofr.net/dav/Koofr";

const KOOFR_PROFILE: &str = "koofr";
const KOOFR_HOST: &str = "app.koofr.net";
const KOOFR_DAV_PREFIX: &str = "/dav/";
const ROOT_KEY: &str = "root";
const DESCRIPTOR_FOLDER: &str = "descriptors";
const ROLE_FOLDERS: [&str; 7] = [
    "catalogs",
    DESCRIPTOR_FOLDER,
    "inventory",
    "leases",
    "packs",
    "points",
    "snapshots",
];
const MAX_ACCOUNT_BYTES: usize = 255;
const MAX_PASSWORD_BYTES: usize = 1024;
const OCTET_STREAM: &str = "application/octet-stream";
const PROPFIND_BODY: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><getetag/><getcontentlength/><resourcetype/></prop></propfind>"#;

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(WebdavProvider { dependencies, listings: Mutex::new(BTreeMap::new()) }))
}

fn role_folder(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => DESCRIPTOR_FOLDER,
        ObjectRole::Pack => "packs",
        ObjectRole::Catalog => "catalogs",
        // A published state and a backup bundle share one collection. The
        // authenticated envelope header, not the path, tells them apart.
        ObjectRole::SyncState | ObjectRole::BackupBundle => "snapshots",
        ObjectRole::BackupPoint => "points",
        ObjectRole::InventoryPage => "inventory",
        ObjectRole::Lease => "leases",
    }
}
fn collection_folder(collection: Collection) -> &'static str {
    match collection {
        Collection::Snapshots => role_folder(ObjectRole::SyncState),
        Collection::BackupPoints => role_folder(ObjectRole::BackupPoint),
        Collection::InventoryPages => role_folder(ObjectRole::InventoryPage),
        Collection::Descriptors => role_folder(ObjectRole::Descriptor),
        Collection::Leases => role_folder(ObjectRole::Lease),
    }
}
fn dav_method(name: &str) -> reqwest::Method {
    reqwest::Method::from_bytes(name.as_bytes()).expect("static DAV method token")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    Generic,
    Koofr,
}
struct Settings {
    endpoint: Url,
    account_id: String,
    root: Vec<String>,
    profile: Profile,
}

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}

fn settings(config: &ConnectionConfig) -> Result<Settings> {
    if config.provider != PROVIDER_ID || config.oauth_profile.is_some() {
        return Err(unsupported());
    }
    let profile = match config.profile.as_deref() {
        None => Profile::Generic,
        Some(KOOFR_PROFILE) => Profile::Koofr,
        Some(_) => return Err(unsupported()),
    };
    let mut endpoint = Url::parse(&config.endpoint).map_err(|_| unsupported())?;
    // The product transport allows plain HTTP only for the loopback fixture.
    let loopback = endpoint.scheme() == "http" && endpoint.host_str() == Some("127.0.0.1");
    if (endpoint.scheme() != "https" && !loopback)
        || endpoint.cannot_be_a_base()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(unsupported());
    }
    let normalised = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&normalised);
    if profile == Profile::Koofr
        && !(endpoint.scheme() == "https"
            && endpoint.host_str() == Some(KOOFR_HOST)
            && endpoint.path().starts_with(KOOFR_DAV_PREFIX))
    {
        return Err(unsupported());
    }
    // The user-id half of the Basic credentials cannot carry a colon.
    if config.account_id.is_empty()
        || config.account_id.len() > MAX_ACCOUNT_BYTES
        || config.account_id.contains(':')
        || config.account_id.chars().any(char::is_control)
    {
        return Err(unsupported());
    }
    if config.location.len() != 1 {
        return Err(unsupported());
    }
    let root = config
        .location
        .get(ROOT_KEY)
        .and_then(|root| paths::split_path(root))
        .ok_or_else(unsupported)?;
    Ok(Settings {
        endpoint,
        account_id: config.account_id.clone(),
        root,
        profile,
    })
}

struct RepositoryContext {
    endpoint: Url,
    /// Endpoint plus root, the prefix every locator resolves against.
    base: Url,
    identity: String,
    account: super::super::quota::AccountKey,
    authorization: Zeroizing<String>,
}
impl RepositoryContext {
    fn new(settings: &Settings, password: &SecretBytes) -> Result<Self> {
        let reauth = || ProviderError::new(ErrorKind::ReauthRequired);
        if password.0.is_empty() || password.0.len() > MAX_PASSWORD_BYTES {
            return Err(reauth());
        }
        let password = std::str::from_utf8(&password.0).map_err(|_| reauth())?;
        if password.chars().any(char::is_control) {
            return Err(reauth());
        }
        let credentials = Zeroizing::new(format!("{}:{password}", settings.account_id));
        Ok(Self {
            base: paths::object_url(&settings.endpoint, &settings.root),
            identity: paths::connection_identity(
                &settings.endpoint,
                &settings.account_id,
                &settings.root,
            ),
            account: super::super::quota::AccountKey::new(
                PROVIDER_ID,
                &settings.endpoint,
                &settings.account_id,
            )?,
            authorization: Zeroizing::new(format!(
                "Basic {}",
                STANDARD.encode(credentials.as_bytes())
            )),
            endpoint: settings.endpoint.clone(),
        })
    }
}
fn context_of(repository: &RepositoryHandle) -> Result<&RepositoryContext> {
    repository
        .context
        .downcast_ref::<RepositoryContext>()
        .filter(|context| context.identity == repository.connection_identity)
        .ok_or_else(paths::corrupt)
}
/// Head writes only ever touch the root member `head`, never a role member.
fn head_path(locator: &RemoteLocator) -> Result<Vec<String>> {
    if locator.object != HEAD_OBJECT || locator.collection.is_some() {
        return Err(paths::corrupt());
    }
    object_path(locator)
}
fn repository_id(identity: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(identity.as_bytes()))
}

/// One resolved member of a collection, or the resource a `Depth: 0` request
/// asked about.
struct Member {
    name: String,
    collection: bool,
    content_length: Option<u64>,
    version: Option<VersionToken>,
}
fn member_of(entry: &multistatus::Entry, name: String) -> Member {
    Member {
        name,
        collection: entry.collection,
        content_length: entry.content_length,
        version: entry.etag.as_deref().and_then(strong_entity_tag),
    }
}
/// Direct children of the requested collection, sorted by name. Responses for
/// the collection itself or for anything outside it are dropped.
fn members(request: &Url, entries: &[multistatus::Entry]) -> Result<Vec<Member>> {
    let collection = paths::decoded_segments(request)?;
    let mut members = Vec::new();
    for entry in entries {
        let Some(resolved) = paths::resolve_href(request, &entry.href) else {
            continue;
        };
        let target = paths::decoded_segments(&resolved)?;
        if let Some(name) = paths::direct_member(&collection, &target) {
            members.push(member_of(entry, name));
        }
    }
    members.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(members)
}

fn strict_members(request: &Url, entries: &[multistatus::Entry]) -> Result<Vec<Member>> {
    let collection = paths::decoded_segments(request)?;
    let mut found = Vec::new();
    for entry in entries {
        let resolved = paths::resolve_href(request, &entry.href)
            .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
        let target = paths::decoded_segments(&resolved)?;
        if target == collection {
            continue;
        }
        let name = paths::direct_member(&collection, &target)
            .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
        found.push(member_of(entry, name));
    }
    found.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(found)
}

fn require_exact_collection(request: &Url, entries: &[multistatus::Entry]) -> Result<()> {
    let target = paths::decoded_segments(request)?;
    let mut matching = Vec::new();
    for entry in entries {
        let Some(resolved) = paths::resolve_href(request, &entry.href) else {
            continue;
        };
        if paths::decoded_segments(&resolved)? == target {
            matching.push(entry);
        }
    }
    let mut matches = matching.into_iter();
    let Some(entry) = matches.next() else {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    };
    if !entry.collection || matches.next().is_some() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

/// Only a strong entity tag becomes a version token. A weak tag or a missing
/// one leaves the object without one and is never evidence for a conditional
/// write.
fn strong_entity_tag(value: &str) -> Option<VersionToken> {
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    (!inner.contains('"') && !inner.chars().any(char::is_control))
        .then(|| VersionToken(value.to_owned()))
}
fn strong_etag(headers: &BTreeMap<String, String>) -> Option<VersionToken> {
    headers
        .get("etag")
        .map(String::as_str)
        .and_then(strong_entity_tag)
}
/// Tokens are minted from strong entity tags only, so a reshaped or foreign
/// token never reaches a conditional header.
fn entity_tag_header(token: &VersionToken) -> Result<String> {
    strong_entity_tag(&token.0)
        .map(|token| token.0)
        .ok_or_else(paths::corrupt)
}

fn object_path(locator: &RemoteLocator) -> Result<Vec<String>> {
    let segments = paths::split_path(&locator.object).ok_or_else(paths::corrupt)?;
    if let Some(collection) = locator.collection.as_deref() {
        if segments.len() < 2 || segments[0] != collection {
            return Err(paths::corrupt());
        }
    }
    Ok(segments)
}
/// The member path of a locator a cleanup may remove. The head is a root
/// member with no role folder, so it never parses here.
fn removable_path(locator: &RemoteLocator) -> Result<Vec<String>> {
    let unsupported = || ProviderError::new(ErrorKind::Unsupported);
    let segments = object_path(locator).map_err(|_| unsupported())?;
    if segments.len() != 2
        || segments[0] == DESCRIPTOR_FOLDER
        || !ROLE_FOLDERS.contains(&segments[0].as_str())
    {
        return Err(unsupported());
    }
    Ok(segments)
}
fn intent_path(intent: &ObjectIntent) -> Result<Vec<String>> {
    if !paths::valid_segment(&intent.object_id) {
        return Err(paths::corrupt());
    }
    Ok(vec![
        role_folder(intent.role).to_owned(),
        intent.object_id.clone(),
    ])
}
fn intent_locator(repository: &RepositoryHandle, path: &[String]) -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository.connection_identity.clone(),
        collection: path.first().cloned(),
        object: path.join("/"),
    }
}
fn declared_checksum(sha256: &str) -> Option<Checksum> {
    // The digest is the caller's, not the server's: DAV exposes no content hash.
    Some(Checksum {
        algorithm: "sha256".into(),
        value: sha256.to_owned(),
        provider_verified: false,
    })
}

fn capabilities(_profile: Profile) -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        // An ETag alone does not prove that this deployment honours a write
        // precondition. Only the authenticated conditional-write probe can
        // enable these operations for the connection.
        atomic_create_head: false,
        conditional_head_update: false,
        stable_head_replace: true,
        head_read_after_write: true,
        head_retry_control: true,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        conditional_get: true,
        range: false,
        resumable_upload: false,
        max_stored_bytes: None,
        sdk_overhead_bytes: 0,
        upload_alignment: 1,
    }
}

/// What a synthetic round trip proved about one deployment's conditional
/// writes. Carries no credential and no server text, so the owner can persist
/// it beside the connection.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConditionalWriteProbe {
    pub create_if_absent: bool,
    pub exact_version_update: bool,
    pub strong_version_token: bool,
    pub probed_at_ms: u64,
}

struct Listing {
    identity: String,
    folder: String,
    expires: u64,
    objects: std::collections::VecDeque<ObjectReceipt>,
}

pub(crate) struct WebdavProvider {
    listings: Mutex<BTreeMap<String, Listing>>,
    dependencies: Dependencies,
}
impl WebdavProvider {
    fn now_ms(&self) -> u64 {
        self.dependencies.clock.now_ms()
    }
    fn request(
        &self,
        context: &RepositoryContext,
        method: reqwest::Method,
        url: Url,
        operation: ProviderOperation,
    ) -> HttpRequest {
        let mut headers = BTreeMap::new();
        headers.insert(
            "authorization".to_owned(),
            context.authorization.to_string(),
        );
        if http::control_operation(operation) {
            http::bypass_cache(&mut headers);
        }
        HttpRequest {
            method,
            url,
            headers,
            body: None,
            content_length: None,
            operation,
            account: context.account.clone(),
            api_request: true,
            mybox_charge: None,
            control: http::control_operation(operation),
        }
    }
    async fn send(&self, request: HttpRequest, cancel: &Cancellation) -> Result<HttpResponse> {
        self.dependencies.send(request, cancel).await
    }
    /// WebDAV gives two statuses a meaning the shared classification does not
    /// carry: 409 is a missing ancestor collection, 423 a write lock another
    /// client holds. Redirects stay unsupported; this adapter follows none.
    fn classify(&self, response: &HttpResponse) -> ProviderError {
        match response.status {
            409 => common::error(ErrorKind::NotFound, 409),
            423 => common::error(ErrorKind::Transient, 423),
            status => common::classify_status(status, &response.headers, self.now_ms()),
        }
    }
    fn require(&self, response: &HttpResponse, allowed: &[u16]) -> Result<()> {
        if allowed.contains(&response.status) {
            Ok(())
        } else {
            Err(self.classify(response))
        }
    }

    /// `Ok(None)` is a 404: the resource is not there. Any other non-207 status
    /// is an error.
    async fn propfind(
        &self,
        context: &RepositoryContext,
        url: Url,
        depth: &str,
        cancel: &Cancellation,
    ) -> Result<Option<Vec<multistatus::Entry>>> {
        let mut request = self.request(
            context,
            dav_method("PROPFIND"),
            url,
            ProviderOperation::Metadata,
        );
        request.headers.insert("depth".to_owned(), depth.to_owned());
        request.headers.insert(
            "content-type".to_owned(),
            "application/xml; charset=\"utf-8\"".to_owned(),
        );
        request.body = Some(Box::pin(std::io::Cursor::new(PROPFIND_BODY)) as _);
        request.content_length = Some(PROPFIND_BODY.len() as u64);
        let mut response = self.send(request, cancel).await?;
        if response.status == 404 {
            return Ok(None);
        }
        self.require(&response, &[207])?;
        let body =
            common::read_bounded(&mut response.body, if depth == "1" { 16 * 1024 * 1024 } else { common::MAX_CONTROL_BODY }, cancel).await?;
        Ok(Some(if depth == "1" { multistatus::parse_complete(&body)? } else { multistatus::parse(&body)? }))
    }

    /// The resource itself, through a `Depth: 0` request.
    async fn stored(
        &self,
        context: &RepositoryContext,
        object: &[String],
        cancel: &Cancellation,
    ) -> Result<Option<Member>> {
        let url = paths::object_url(&context.base, object);
        let Some(entries) = self.propfind(context, url.clone(), "0", cancel).await? else {
            return Ok(None);
        };
        let target = paths::decoded_segments(&url)?;
        for entry in &entries {
            let Some(resolved) = paths::resolve_href(&url, &entry.href) else {
                continue;
            };
            if paths::decoded_segments(&resolved)? == target {
                let name = target.last().cloned().unwrap_or_default();
                return Ok(Some(member_of(entry, name)));
            }
        }
        Ok(None)
    }

    async fn mkcol(
        &self,
        context: &RepositoryContext,
        url: Url,
        cancel: &Cancellation,
    ) -> Result<()> {
        let request = self.request(context, dav_method("MKCOL"), url, ProviderOperation::Create);
        let response = self.send(request, cancel).await?;
        // 405 is the answer for a collection that is already there.
        if response.status == 405 {
            return Ok(());
        }
        self.require(&response, &[200, 201])
    }

    async fn mkcol_new(
        &self,
        context: &RepositoryContext,
        url: Url,
        cancel: &Cancellation,
    ) -> Result<()> {
        let request = self.request(context, dav_method("MKCOL"), url, ProviderOperation::Create);
        let response = self.send(request, cancel).await?;
        if response.status == 405 {
            return Err(common::error(ErrorKind::PreconditionFailed, 405));
        }
        self.require(&response, &[200, 201])
    }

    async fn create_missing_root(
        &self,
        context: &RepositoryContext,
        root: &[String],
        cancel: &Cancellation,
    ) -> Result<()> {
        for depth in 1..=root.len() {
            let url = paths::collection_url(&context.endpoint, &root[..depth]);
            if depth == root.len() {
                self.mkcol_new(context, url, cancel).await?;
                continue;
            }
            match self.propfind(context, url.clone(), "0", cancel).await? {
                Some(entries) => require_exact_collection(&url, &entries)?,
                None => self.mkcol_new(context, url, cancel).await?,
            }
        }
        Ok(())
    }

    async fn resume_layout(
        &self,
        context: &RepositoryContext,
        root: &[String],
        root_url: &Url,
        listing: Option<Vec<multistatus::Entry>>,
        cancel: &Cancellation,
    ) -> Result<()> {
        let mut found = BTreeSet::new();
        match listing {
            Some(entries) => {
                require_exact_collection(root_url, &entries)?;
                for member in strict_members(root_url, &entries)? {
                    if !ROLE_FOLDERS.contains(&member.name.as_str())
                        || !member.collection
                        || !found.insert(member.name)
                    {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                }
            }
            None => self.create_missing_root(context, root, cancel).await?,
        }
        for folder in ROLE_FOLDERS {
            if !found.contains(folder) {
                let url = paths::collection_url(&context.base, &[folder.to_owned()]);
                self.mkcol_new(context, url, cancel).await?;
            }
        }
        self.require_initial_contents(context, cancel).await
    }

    async fn require_initial_contents(
        &self,
        context: &RepositoryContext,
        cancel: &Cancellation,
    ) -> Result<()> {
        for folder in ROLE_FOLDERS {
            let url = paths::collection_url(&context.base, &[folder.to_owned()]);
            let entries = self
                .propfind(context, url.clone(), "1", cancel)
                .await?
                .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
            require_exact_collection(&url, &entries)?;
            let contents = strict_members(&url, &entries)?;
            if folder != DESCRIPTOR_FOLDER {
                if !contents.is_empty() {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                continue;
            }
            if contents.len() > 2 {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            for descriptor in contents {
                if descriptor.collection
                    || descriptor.content_length.is_none_or(|length| length == 0)
                    || !paths::valid_segment(&descriptor.name)
                {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
            }
        }
        Ok(())
    }
    async fn create_layout(
        &self,
        context: &RepositoryContext,
        root: &[String],
        create_root: bool,
        cancel: &Cancellation,
    ) -> Result<()> {
        if create_root {
            // Ancestors may legitimately exist; only the repository root's own
            // contents decide whether this location is free.
            for depth in 1..=root.len() {
                let url = paths::collection_url(&context.endpoint, &root[..depth]);
                self.mkcol(context, url, cancel).await?;
            }
        }
        for folder in ROLE_FOLDERS {
            let url = paths::collection_url(&context.base, &[folder.to_owned()]);
            self.mkcol(context, url, cancel).await?;
        }
        Ok(())
    }

    async fn delete(
        &self,
        context: &RepositoryContext,
        url: Url,
        cancel: &Cancellation,
    ) -> Result<()> {
        let request = self.request(context, dav_method("DELETE"), url, ProviderOperation::Delete);
        let response = self.send(request, cancel).await?;
        if response.status == 404 {
            return Ok(());
        }
        self.require(&response, &[200, 202, 204])
    }

    /// Exactly one write attempt. An ambiguous answer is returned as an error
    /// so the engine re-observes the head instead of writing again.
    async fn write_head(
        &self,
        mut request: HttpRequest,
        head: &HeadBytes,
        cancel: &Cancellation,
    ) -> Result<HeadReceipt> {
        let bytes = head.as_bytes().to_vec();
        request
            .headers
            .insert("content-type".to_owned(), OCTET_STREAM.to_owned());
        request.content_length = Some(bytes.len() as u64);
        request.body = Some(Box::pin(std::io::Cursor::new(bytes)) as _);
        let response = self.send(request, cancel).await?;
        self.require(&response, &[200, 201, 204])?;
        Ok(HeadReceipt {
            version: strong_etag(&response.headers),
            complete: true,
        })
    }

    /// A name already taken by an earlier attempt. DAV exposes no content
    /// digest, so an equal stored length is the only convergence evidence
    /// available; a different length is a conflict and is never overwritten.
    async fn converged(
        &self,
        context: &RepositoryContext,
        object: &[String],
        intent: &ObjectIntent,
        locator: RemoteLocator,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let conflict = || common::error(ErrorKind::PreconditionFailed, 412);
        let stored = self
            .stored(context, object, cancel)
            .await?
            .ok_or_else(conflict)?;
        if stored.collection || stored.content_length != Some(intent.byte_length) {
            return Err(conflict());
        }
        Ok(ObjectReceipt {
            locator,
            byte_length: intent.byte_length,
            version: stored.version,
            checksum: declared_checksum(&intent.sha256),
            complete: true,
        })
    }

    async fn probe_write(
        &self,
        context: &RepositoryContext,
        url: &Url,
        condition: &str,
        value: String,
        body: &'static [u8],
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        let mut request = self.request(
            context,
            reqwest::Method::PUT,
            url.clone(),
            ProviderOperation::CompareExchangeHead,
        );
        request.headers.insert(condition.to_owned(), value);
        request
            .headers
            .insert("content-type".to_owned(), OCTET_STREAM.to_owned());
        request.body = Some(Box::pin(std::io::Cursor::new(body)) as _);
        request.content_length = Some(body.len() as u64);
        self.send(request, cancel).await
    }
    async fn run_probe(
        &self,
        context: &RepositoryContext,
        url: &Url,
        probe: &mut ConditionalWriteProbe,
        cancel: &Cancellation,
    ) -> Result<()> {
        let accepted = [200, 201, 204];
        let first = self
            .probe_write(
                context,
                url,
                "if-none-match",
                "*".into(),
                b"probe-1",
                cancel,
            )
            .await?;
        self.require(&first, &accepted)?;
        let token = strong_etag(&first.headers);
        probe.strong_version_token = token.is_some();
        let second = self
            .probe_write(
                context,
                url,
                "if-none-match",
                "*".into(),
                b"probe-2",
                cancel,
            )
            .await?;
        self.require(&second, &[200, 201, 204, 412])?;
        probe.create_if_absent = second.status == 412;
        let Some(token) = token.filter(|_| probe.create_if_absent) else {
            return Ok(());
        };
        let stale = self
            .probe_write(
                context,
                url,
                "if-match",
                "\"risunest-probe-stale\"".into(),
                b"probe-3",
                cancel,
            )
            .await?;
        self.require(&stale, &[200, 201, 204, 412])?;
        if stale.status != 412 {
            return Ok(());
        }
        // A deployment that refuses every `If-Match` is not doing CAS either,
        // so the current tag has to be accepted before this counts.
        let current = self
            .probe_write(context, url, "if-match", token.0, b"probe-4", cancel)
            .await?;
        self.require(&current, &[200, 201, 204, 412])?;
        probe.exact_version_update = current.status != 412;
        Ok(())
    }
}

/// A synthetic conditional-write round trip on a throwaway object below the
/// root, for the owner to run once during connection setup and persist.
/// `open_repository` never reports CAS on its own.
pub(crate) async fn probe_conditional_writes(
    dependencies: &Dependencies,
    config: &ConnectionConfig,
    secret: &SecretRef,
    cancel: &Cancellation,
) -> Result<ConditionalWriteProbe> {
    cancel.check()?;
    let settings = settings(config)?;
    let password = dependencies.vault.read(secret).await?;
    let context = RepositoryContext::new(&settings, &password)?;
    let provider = WebdavProvider {
        dependencies: dependencies.clone(),
        listings: Mutex::new(BTreeMap::new()),
    };
    let object = vec![format!("probe-{}", uuid::Uuid::new_v4())];
    let url = paths::object_url(&context.base, &object);
    let mut probe = ConditionalWriteProbe {
        probed_at_ms: dependencies.clock.now_ms(),
        ..ConditionalWriteProbe::default()
    };
    let outcome = provider.run_probe(&context, &url, &mut probe, cancel).await;
    let cleanup = provider.delete(&context, url, cancel).await;
    outcome?;
    cleanup?;
    Ok(probe)
}

impl Provider for WebdavProvider {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let settings = settings(config)?;
            let password = self.dependencies.vault.read(secret).await?;
            let context = RepositoryContext::new(&settings, &password)?;
            // One `Depth: 1` request proves the credentials, whether the root
            // is there and what it already holds.
            let root = paths::collection_url(&context.base, &[]);
            let listing = self.propfind(&context, root.clone(), "1", cancel).await?;
            match (mode, listing) {
                (OpenMode::Existing, None) => return Err(ProviderError::new(ErrorKind::NotFound)),
                (OpenMode::Existing, Some(entries)) => {
                    let descriptors = members(&root, &entries)?
                        .into_iter()
                        .any(|member| member.name == DESCRIPTOR_FOLDER && member.collection);
                    if !descriptors {
                        return Err(ProviderError::new(ErrorKind::NotFound));
                    }
                }
                (OpenMode::Create, listing) => {
                    let occupied = listing
                        .as_deref()
                        .map(|entries| strict_members(&root, entries))
                        .transpose()?
                        .is_some_and(|members| !members.is_empty());
                    if occupied {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                    self.create_layout(&context, &settings.root, listing.is_none(), cancel)
                        .await?;
                }
                (OpenMode::ResumeCreate, listing) => {
                    self.resume_layout(&context, &settings.root, &root, listing, cancel)
                        .await?;
                }
            }
            let handle = RepositoryHandle {
                repository_id: repository_id(&context.identity),
                connection_identity: context.identity.clone(),
                account: context.account.clone(),
                context: Box::new(context),
            };
            Ok((handle, capabilities(settings.profile)))
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
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let object = object_path(locator)?;
            let mut request = self.request(
                context,
                reqwest::Method::GET,
                paths::object_url(&context.base, &object),
                ProviderOperation::Get,
            );
            if let Some(token) = unchanged {
                request
                    .headers
                    .insert("if-none-match".to_owned(), entity_tag_header(token)?);
            }
            let mut response = self.send(request, cancel).await?;
            if response.status == 304 {
                let token = unchanged
                    .cloned()
                    .ok_or_else(|| common::error(ErrorKind::Corrupt, 304))?;
                return Ok(ReadReceipt::NotModified(token));
            }
            self.require(&response, &[200])?;
            // Without a declared length the staging file has no bound and a
            // truncated body would look complete.
            let length = common::content_length(&response.headers)?
                .ok_or_else(|| common::error(ErrorKind::Corrupt, response.status))?;
            let (byte_length, sha256) =
                common::stream_to_sink(&mut response.body, sink, Some(length), length, cancel)
                    .await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length,
                version: strong_etag(&response.headers),
                checksum: declared_checksum(&sha256),
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
            context_of(repository)?;
            intent.validate(repository)?;
            intent_path(intent)?;
            // WebDAV defines no resumable upload, so an interrupted object is
            // retransmitted whole.
            Ok(None)
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
            let context = context_of(repository)?;
            intent.validate(repository)?;
            if resume.is_some() {
                return Err(unsupported());
            }
            if source.byte_length() != intent.byte_length {
                return Err(paths::corrupt());
            }
            let object = intent_path(intent)?;
            let locator = intent_locator(repository, &object);
            let mut request = self.request(
                context,
                reqwest::Method::PUT,
                paths::object_url(&context.base, &object),
                ProviderOperation::Create,
            );
            request
                .headers
                .insert("if-none-match".to_owned(), "*".to_owned());
            request
                .headers
                .insert("content-type".to_owned(), OCTET_STREAM.to_owned());
            request.body = Some(source.open(0, intent.byte_length, cancel).await?);
            request.content_length = Some(intent.byte_length);
            let response = self.send(request, cancel).await?;
            if response.status == 412 {
                return self
                    .converged(context, &object, intent, locator, cancel)
                    .await;
            }
            self.require(&response, &[200, 201, 204])?;
            Ok(ObjectReceipt {
                locator,
                byte_length: intent.byte_length,
                version: strong_etag(&response.headers),
                checksum: declared_checksum(&intent.sha256),
                complete: true,
            })
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
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let object = head_path(locator)?;
            let mut request = self.request(
                context,
                reqwest::Method::PUT,
                paths::object_url(&context.base, &object),
                ProviderOperation::CompareExchangeHead,
            );
            match expected {
                ExpectedHead::Absent => {
                    request
                        .headers
                        .insert("if-none-match".to_owned(), "*".to_owned());
                }
                ExpectedHead::Exact(token) => {
                    request
                        .headers
                        .insert("if-match".to_owned(), entity_tag_header(token)?);
                }
            }
            self.write_head(request, head, cancel).await
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
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let object = head_path(locator)?;
            let request = self.request(
                context,
                reqwest::Method::PUT,
                paths::object_url(&context.base, &object),
                ProviderOperation::ReplaceHead,
            );
            self.write_head(request, head, cancel).await
        })
    }

    /// 202 is an accepted request, not a finished deletion, so it is reported
    /// as transient rather than as a success.
    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            cancel.check()?;
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let path = removable_path(locator)?;
            let request = self.request(
                context,
                dav_method("DELETE"),
                paths::object_url(&context.base, &path),
                ProviderOperation::Delete,
            );
            let response = self.send(request, cancel).await?;
            if response.status == 404 {
                return Ok(());
            }
            if response.status == 202 {
                return Err(common::error(ErrorKind::Transient, 202));
            }
            self.require(&response, &[200, 204])
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
            let context = context_of(repository)?;
            if limit == 0 || limit > 1000 {
                return Err(unsupported());
            }
            let folder = collection_folder(collection).to_owned();
            let mut listing = if let Some(cursor) = cursor {
                let mut listings = self.listings.lock().map_err(|_| paths::corrupt())?;
                listings.retain(|_, listing| listing.expires > self.now_ms());
                let listing = listings.remove(cursor).ok_or_else(paths::corrupt)?;
                if listing.identity != repository.connection_identity || listing.folder != folder {
                    return Err(paths::corrupt());
                }
                listing
            } else {
                let url = paths::collection_url(&context.base, std::slice::from_ref(&folder));
                let entries = self.propfind(context, url.clone(), "1", cancel).await?
                    .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
                require_exact_collection(&url, &entries)?;
                let all = strict_members(&url, &entries)?;
                if all.windows(2).any(|pair| pair[0].name == pair[1].name) { return Err(paths::corrupt()); }
                let mut objects = std::collections::VecDeque::new();
                for member in all.into_iter().filter(|member| !member.collection) {
                    objects.push_back(ObjectReceipt {
                        locator: RemoteLocator {
                            connection_identity: repository.connection_identity.clone(),
                            collection: Some(folder.clone()),
                            object: format!("{folder}/{}", member.name),
                        },
                        byte_length: member.content_length.ok_or_else(paths::corrupt)?,
                        version: member.version,
                        checksum: None,
                        complete: true,
                    });
                }
                Listing { identity: repository.connection_identity.clone(), folder,
                    expires: self.now_ms().saturating_add(5 * 60 * 1000), objects }
            };
            let count = usize::from(limit).min(listing.objects.len());
            let objects = listing.objects.drain(..count).collect();
            let next_cursor = if listing.objects.is_empty() { None } else {
                let cursor = uuid::Uuid::new_v4().to_string();
                let mut listings = self.listings.lock().map_err(|_| paths::corrupt())?;
                listings.retain(|_, listing| listing.expires > self.now_ms());
                if listings.len() >= 4 { return Err(ProviderError::new(ErrorKind::Transient)); }
                listings.insert(cursor.clone(), listing);
                Some(cursor)
            };
            Ok(ObjectPage { objects, next_cursor })
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
            let context = context_of(repository)?;
            intent.validate(repository)?;
            // This adapter opens no session, so the sealed state holds nothing
            // of its own and the stored resource is the only remote truth.
            let _ = resume;
            let object = intent_path(intent)?;
            let locator = intent_locator(repository, &object);
            let Some(stored) = self.stored(context, &object, cancel).await? else {
                return Ok(UploadResolution::RestartRequired);
            };
            if stored.collection || stored.content_length != Some(intent.byte_length) {
                return Ok(UploadResolution::Conflict);
            }
            Ok(UploadResolution::Complete(ObjectReceipt {
                locator,
                byte_length: intent.byte_length,
                version: stored.version,
                checksum: declared_checksum(&intent.sha256),
                complete: true,
            }))
        })
    }

    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        context_of(repository)?;
        Ok(RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: HEAD_OBJECT.to_owned(),
        })
    }

}
