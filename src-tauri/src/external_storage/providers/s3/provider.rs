//! The single S3 core every preset shares: signing, addressing, object reads,
//! immutable creates, multipart sessions, head writes and listing.
use super::{
    config::{self, collection_folder, role_folder, RepositoryContext, Target},
    profiles::{Profile, MAX_PARTS},
    sigv4::{self, hex_sha256, Credentials, EMPTY_PAYLOAD_SHA256, UNSIGNED_PAYLOAD},
    xml,
};
use crate::external_storage::{
    capabilities::Capabilities,
    contract::*,
    http::{self, HttpRequest, HttpResponse},
    providers::{common, Dependencies},
};
use base64::Engine as _;
use std::{collections::{BTreeMap, BTreeSet}, pin::Pin, sync::Arc, time::{Duration, Instant}};
use tokio::io::AsyncRead;

/// Redirects are followed only where the service documents them, and only far
/// enough to reach the edge it names.
const MAX_REDIRECTS: usize = 2;
const MAX_LIST_KEYS: u16 = 1000;

pub(crate) struct S3Provider {
    dependencies: Dependencies,
}

impl S3Provider {
    pub(crate) fn new(dependencies: Dependencies) -> Self {
        Self { dependencies }
    }
}

/// One outgoing S3 request before signing.
struct Call {
    method: reqwest::Method,
    key: Option<String>,
    query: Vec<(String, String)>,
    headers: BTreeMap<String, String>,
    body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    content_length: Option<u64>,
    payload_hash: String,
    operation: ProviderOperation,
}

impl Call {
    fn object(method: reqwest::Method, key: &str, operation: ProviderOperation) -> Self {
        Self {
            method,
            key: Some(key.to_owned()),
            query: Vec::new(),
            headers: BTreeMap::new(),
            body: None,
            content_length: None,
            payload_hash: EMPTY_PAYLOAD_SHA256.into(),
            operation,
        }
    }
    fn bucket(method: reqwest::Method, operation: ProviderOperation) -> Self {
        Self {
            method,
            key: None,
            query: Vec::new(),
            headers: BTreeMap::new(),
            body: None,
            content_length: None,
            payload_hash: EMPTY_PAYLOAD_SHA256.into(),
            operation,
        }
    }
    fn query(mut self, name: &str, value: impl Into<String>) -> Self {
        self.query.push((name.to_owned(), value.into()));
        self
    }
    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.insert(name.to_owned(), value.into());
        self
    }
    fn payload(mut self, hash: &str) -> Self {
        self.payload_hash = hash.to_owned();
        self
    }
    fn body(mut self, body: Pin<Box<dyn AsyncRead + Send>>, length: u64) -> Self {
        self.body = Some(body);
        self.content_length = Some(length);
        self
    }
}

/// What `HeadObject` tells us about an object that already exists.
struct RemoteObject {
    byte_length: u64,
    version: Option<VersionToken>,
    checksum_sha256: Option<String>,
}

/// Sealed multipart session. Serialized only to be stored in the vault; it is
/// never logged, journalled in the clear or given a Debug formatter.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionState {
    upload_id: String,
    key: String,
    part_size: u64,
    parts: Vec<SessionPart>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionPart {
    number: u32,
    etag: String,
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn precondition(status: Option<u16>) -> ProviderError {
    ProviderError {
        kind: ErrorKind::PreconditionFailed,
        http_status: status,
        retry_at_ms: None,
    }
}

/// Only a strong entity tag can be replayed as a write precondition.
fn strong_etag(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    (bytes.len() >= 2
        && bytes.len() <= 256
        && bytes[0] == b'"'
        && bytes[bytes.len() - 1] == b'"'
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|byte| byte.is_ascii_graphic() && *byte != b'"'))
    .then(|| value.to_owned())
}

fn version_of(headers: &BTreeMap<String, String>) -> Option<VersionToken> {
    strong_etag(headers.get("etag")?.trim()).map(VersionToken)
}

fn checksum_of(headers: &BTreeMap<String, String>) -> Option<String> {
    let value = headers.get("x-amz-checksum-sha256")?.trim();
    (!value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_graphic()))
        .then(|| value.to_owned())
}

fn base64_sha256(lower_hex: &str) -> Result<String> {
    Ok(base64::engine::general_purpose::STANDARD
        .encode(hex::decode(lower_hex).map_err(|_| corrupt())?))
}

pub(crate) fn capabilities(profile: &Profile) -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        atomic_create_head: profile.cas_supported,
        conditional_head_update: profile.cas_supported,
        stable_head_replace: true,
        head_read_after_write: true,
        head_retry_control: true,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        conditional_get: profile.conditional_get,
        range: false,
        resumable_upload: true,
        max_stored_bytes: Some(profile.max_stored_bytes()),
        sdk_overhead_bytes: 0,
        upload_alignment: profile.part_size_bytes,
    }
}

fn control_key(context: &RepositoryContext, key: &str) -> bool {
    !key.starts_with(&context.folder_prefix("packs"))
}

fn context_of(repository: &RepositoryHandle) -> Result<&RepositoryContext> {
    repository
        .context
        .downcast_ref::<RepositoryContext>()
        .filter(|context| context.connection_identity == repository.connection_identity
            && context.account == repository.account)
        .ok_or_else(corrupt)
}

impl S3Provider {
    fn now_ms(&self) -> u64 {
        self.dependencies.clock.now_ms()
    }

    async fn credentials(&self, context: &RepositoryContext) -> Result<Credentials> {
        config::credentials(self.dependencies.vault.as_ref(), &context.secret).await
    }

    async fn dispatch(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        call: Call,
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        let url = {
            let target = match &call.key {
                Some(key) => Target::Object(key),
                None => Target::Bucket,
            };
            context.url(target, &call.query)?
        };
        let control = http::control_operation(call.operation)
            || (call.method == reqwest::Method::GET
                && call.key.as_deref().is_some_and(|key| control_key(context, key)));
        let head_interval = if matches!(call.operation,
            ProviderOperation::CompareExchangeHead | ProviderOperation::ReplaceHead) {
            context.profile.same_key_write_window_ms
        } else { None };
        // SigV4 timestamps are created only after an existing account pause.
        // The signed dispatch returns without sending if another response
        // imposed a new pause while these headers were being constructed.
        let deadline = self.dependencies
            .wait_until_ready(&context.account, control, cancel)
            .await?;
        let amz_date = sigv4::amz_date(self.now_ms())?;
        let mut headers = call.headers;
        if matches!(call.method, reqwest::Method::GET | reqwest::Method::HEAD) {
            http::bypass_cache(&mut headers);
        }
        headers.insert("x-amz-content-sha256".into(), call.payload_hash.clone());
        headers.insert("x-amz-date".into(), amz_date.clone());
        let signature = sigv4::sign(
            credentials,
            &context.region,
            call.method.as_str(),
            &url,
            &headers,
            &call.payload_hash,
            &amz_date,
        )?;
        headers.insert("authorization".into(), signature.authorization);
        let result = self.dependencies.send_signed(
            HttpRequest {
                method: call.method,
                url,
                headers,
                body: call.body,
                content_length: call.content_length,
                operation: call.operation,
                account: context.account.clone(),
                api_request: true,
                mybox_charge: None,
                control,
            },
            cancel,
            deadline,
        ).await;
        if let Some(interval) = head_interval {
            if result.is_ok() || result.as_ref().is_err_and(|error| error.kind == ErrorKind::Transient) {
                self.dependencies.requests.backoff.defer_for(&context.account,
                    Duration::from_millis(interval), Instant::now())?;
            }
        }
        result
    }

    /// Default S3 status meanings with the documented per-service overrides.
    fn classify(&self, profile: &Profile, response: &HttpResponse) -> ProviderError {
        let mut error = common::classify_status(response.status, &response.headers, self.now_ms());
        if response.status == 503 && profile.slow_down_is_rate_limit {
            error.kind = ErrorKind::RateLimited;
            error.retry_at_ms = common::retry_after_ms(&response.headers, self.now_ms());
        }
        error
    }

    fn require(&self, profile: &Profile, response: &HttpResponse, allowed: &[u16]) -> Result<()> {
        if allowed.contains(&response.status) {
            Ok(())
        } else {
            Err(self.classify(profile, response))
        }
    }

    async fn head_object(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        key: &str,
        cancel: &Cancellation,
    ) -> Result<Option<RemoteObject>> {
        let response = self
            .dispatch(
                context,
                credentials,
                Call::object(reqwest::Method::HEAD, key, ProviderOperation::Metadata),
                cancel,
            )
            .await?;
        if response.status == 404 {
            return Ok(None);
        }
        self.require(context.profile, &response, &[200])?;
        Ok(Some(RemoteObject {
            byte_length: common::content_length(&response.headers)?.ok_or_else(corrupt)?,
            version: version_of(&response.headers),
            checksum_sha256: checksum_of(&response.headers),
        }))
    }

    async fn list_page(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        folder: &str,
        cursor: Option<&str>,
        limit: u16,
        cancel: &Cancellation,
    ) -> Result<xml::ListObjectsPage> {
        let mut call = Call::bucket(reqwest::Method::GET, ProviderOperation::List)
            .query("list-type", "2")
            .query("prefix", context.folder_prefix(folder))
            .query("max-keys", limit.to_string());
        if let Some(cursor) = cursor {
            if cursor.is_empty()
                || cursor.len() > 4096
                || !cursor.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(corrupt());
            }
            call = call.query("continuation-token", cursor);
        }
        let mut response = self.dispatch(context, credentials, call, cancel).await?;
        self.require(context.profile, &response, &[200])?;
        let body =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        let mut page = xml::parse_list_objects(&body)?;
        if cursor.is_some() && page.next_continuation_token.as_deref() == cursor {
            return Err(corrupt());
        }
        let prefix = context.folder_prefix(folder);
        for object in &page.objects {
            if object.key == prefix {
                if object.size != 0 { return Err(corrupt()); }
            } else if context.object_of(folder, &object.key).is_none() {
                return Err(corrupt());
            }
        }
        page.objects.retain(|object| object.key != prefix);
        Ok(page)
    }

    async fn descriptor_exists(&self, context: &RepositoryContext, credentials: &Credentials, cancel: &Cancellation) -> Result<bool> {
        let mut cursor = None;
        let mut visited = BTreeSet::new();
        loop {
            let page = self.list_page(context, credentials, collection_folder(Collection::Descriptors),
                cursor.as_deref(), 1, cancel).await?;
            if !page.objects.is_empty() { return Ok(true); }
            match page.next_continuation_token {
                None => return Ok(false),
                Some(next) if visited.len() < 10_000 && visited.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err(corrupt()),
            }
        }
    }

    async fn require_create_layout(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        allow_descriptor: bool,
        cancel: &Cancellation,
    ) -> Result<()> {
        let prefix = if context.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", context.prefix)
        };
        let mut response = self
            .dispatch(
                context,
                credentials,
                Call::bucket(reqwest::Method::GET, ProviderOperation::List)
                    .query("list-type", "2")
                    .query("prefix", prefix.as_str())
                    .query("max-keys", "3"),
                cancel,
            )
            .await?;
        self.require(context.profile, &response, &[200])?;
        let body =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        let page = xml::parse_list_objects(&body)?;
        if !allow_descriptor {
            if page.next_continuation_token.is_some() || !page.objects.is_empty() {
                return Err(precondition(None));
            }
            return Ok(());
        }
        if page.next_continuation_token.is_some() || page.objects.len() > 2 {
            return Err(precondition(None));
        }
        for object in page.objects {
            if context
                .object_of(collection_folder(Collection::Descriptors), &object.key)
                .is_none()
            {
                return Err(precondition(None));
            }
            if object.size == 0 {
                return Err(corrupt());
            }
        }
        Ok(())
    }

    /// One `GetObject`, plus the documented download redirect where a preset
    /// has one. Authentication is never carried onto the redirect target, and
    /// the caller is told whether the bytes came from the origin or an edge.
    async fn get_object(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        key: &str,
        conditional: Option<&VersionToken>,
        cancel: &Cancellation,
    ) -> Result<(HttpResponse, bool)> {
        let mut call = Call::object(reqwest::Method::GET, key, ProviderOperation::Get);
        if let Some(token) = conditional {
            call = call.header("if-none-match", strong_etag(&token.0).ok_or_else(corrupt)?);
        }
        let mut response = self.dispatch(context, credentials, call, cancel).await?;
        if !context.profile.download_redirect {
            return Ok((response, false));
        }
        let mut visited = context.url(Target::Object(key), &[])?;
        let mut redirected = false;
        for _ in 0..MAX_REDIRECTS {
            if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                return Ok((response, redirected));
            }
            let location = response.headers.get("location").ok_or_else(corrupt)?;
            let next = visited.join(location).map_err(|_| corrupt())?;
            if next.fragment().is_some() || !next.username().is_empty() || next.password().is_some()
            {
                return Err(corrupt());
            }
            let mut headers = BTreeMap::new();
            http::bypass_cache(&mut headers);
            response = self.dependencies
                .send(
                    HttpRequest {
                        method: reqwest::Method::GET,
                        url: next.clone(),
                        headers,
                        body: None,
                        content_length: None,
                        operation: ProviderOperation::Get,
                        account: context.account.clone(),
                        api_request: false,
                        mybox_charge: None,
                        control: control_key(context, key),
                    },
                    cancel,
                )
                .await?;
            visited = next;
            redirected = true;
        }
        Ok((response, redirected))
    }

    async fn session(&self, resume: &ResumeState) -> Result<SessionState> {
        let bytes = self.dependencies.vault.read(&resume.sealed_state).await?;
        let state: SessionState = serde_json::from_slice(&bytes.0).map_err(|_| corrupt())?;
        if state.upload_id.is_empty()
            || state.upload_id.len() > 1024
            || state.part_size == 0
            || state.parts.len() > MAX_PARTS as usize
        {
            return Err(corrupt());
        }
        Ok(state)
    }

    async fn seal(&self, state: &SessionState) -> Result<SecretRef> {
        let bytes = serde_json::to_vec(state).map_err(|_| corrupt())?;
        self.dependencies
            .vault
            .store(&crate::external_storage::auth::SecretBytes(
                zeroize::Zeroizing::new(bytes),
            ))
            .await
    }

    async fn reseal(&self, reference: &SecretRef, state: &SessionState) -> Result<()> {
        let bytes = serde_json::to_vec(state).map_err(|_| corrupt())?;
        self.dependencies
            .vault
            .replace(
                reference,
                &crate::external_storage::auth::SecretBytes(zeroize::Zeroizing::new(bytes)),
            )
            .await
    }

    /// An object that is already in place converges only when the remote
    /// evidence matches this intent; different bytes are never replaced.
    fn converge(
        &self,
        profile: &Profile,
        existing: &RemoteObject,
        intent: &ObjectIntent,
        locator: RemoteLocator,
    ) -> Result<ObjectReceipt> {
        if existing.byte_length != intent.byte_length {
            return Err(precondition(None));
        }
        let expected = base64_sha256(&intent.sha256)?;
        let verified = match (&existing.checksum_sha256, profile.checksum_header) {
            (Some(remote), true) if *remote != expected => return Err(precondition(None)),
            (Some(_), true) => true,
            _ => false,
        };
        Ok(ObjectReceipt {
            locator,
            byte_length: intent.byte_length,
            version: existing.version.clone(),
            checksum: Some(Checksum {
                algorithm: "sha256".into(),
                value: intent.sha256.clone(),
                provider_verified: verified,
            }),
            complete: true,
        })
    }

    async fn single_put(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        key: &str,
        locator: RemoteLocator,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let profile = context.profile;
        if intent.byte_length > profile.max_single_put_bytes {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        if !profile.conditional_put {
            // Without a create-if-absent precondition the only way to keep an
            // existing object from being replaced is to look before writing.
            if let Some(existing) = self.head_object(context, credentials, key, cancel).await? {
                return self.converge(profile, &existing, intent, locator);
            }
        }
        let expected_checksum = base64_sha256(&intent.sha256)?;
        let body = source.open(0, intent.byte_length, cancel).await?;
        let mut call = Call::object(reqwest::Method::PUT, key, ProviderOperation::Create)
            .payload(&intent.sha256)
            .body(body, intent.byte_length);
        if profile.conditional_put {
            call = call.header("if-none-match", "*");
        }
        if profile.checksum_header {
            call = call.header("x-amz-checksum-sha256", expected_checksum.clone());
        }
        let response = self.dispatch(context, credentials, call, cancel).await?;
        if profile.conditional_put && matches!(response.status, 409 | 412) {
            let existing = self
                .head_object(context, credentials, key, cancel)
                .await?
                .ok_or_else(|| precondition(Some(response.status)))?;
            return self.converge(profile, &existing, intent, locator);
        }
        self.require(profile, &response, &[200])?;
        let verified =
            checksum_of(&response.headers).is_some_and(|value| value == expected_checksum);
        Ok(ObjectReceipt {
            locator,
            byte_length: intent.byte_length,
            version: version_of(&response.headers),
            checksum: Some(Checksum {
                algorithm: "sha256".into(),
                value: intent.sha256.clone(),
                provider_verified: verified,
            }),
            complete: true,
        })
    }

    async fn continue_multipart(
        &self,
        context: &RepositoryContext,
        credentials: &Credentials,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        key: &str,
        locator: RemoteLocator,
        resume: &ResumeState,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let profile = context.profile;
        let state = self.session(resume).await?;
        if state.key != key {
            return Err(corrupt());
        }
        let part_size = state.part_size;
        let confirmed = resume.confirmed_offset;
        // Only a part boundary is resumable; the object's own end is the one
        // offset that may fall inside the last part.
        if confirmed > intent.byte_length
            || (confirmed % part_size != 0 && confirmed != intent.byte_length)
        {
            return Err(corrupt());
        }
        let done = if confirmed == intent.byte_length {
            confirmed.div_ceil(part_size)
        } else {
            confirmed / part_size
        };
        let mut parts: Vec<(u32, String)> = state
            .parts
            .iter()
            .filter(|part| u64::from(part.number) <= done)
            .map(|part| (part.number, part.etag.clone()))
            .collect();
        parts.sort_by_key(|(number, _)| *number);
        if parts.len() as u64 != done
            || parts
                .iter()
                .enumerate()
                .any(|(index, (number, _))| *number as usize != index + 1)
        {
            return Err(corrupt());
        }
        let mut offset = confirmed;
        while offset < intent.byte_length {
            cancel.check()?;
            let length = part_size.min(intent.byte_length - offset);
            let number = u32::try_from(offset / part_size + 1).map_err(|_| corrupt())?;
            if u64::from(number) > MAX_PARTS {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let body = source.open(offset, length, cancel).await?;
            let call = Call::object(reqwest::Method::PUT, key, ProviderOperation::UploadChunk)
                .query("partNumber", number.to_string())
                .query("uploadId", state.upload_id.clone())
                .payload(UNSIGNED_PAYLOAD)
                .body(body, length);
            let response = self.dispatch(context, credentials, call, cancel).await?;
            self.require(profile, &response, &[200])?;
            let etag = response
                .headers
                .get("etag")
                .and_then(|value| strong_etag(value.trim()))
                .ok_or_else(corrupt)?;
            parts.push((number, etag));
            offset += length;
        }
        let document = xml::complete_multipart_body(&parts);
        let call = Call::object(
            reqwest::Method::POST,
            key,
            ProviderOperation::CompleteUpload,
        )
        .query("uploadId", state.upload_id.clone())
        .header("content-type", "application/xml")
        .payload(&hex_sha256(document.as_bytes()))
        .body(
            Box::pin(std::io::Cursor::new(document.clone().into_bytes())),
            document.len() as u64,
        );
        let mut response = self.dispatch(context, credentials, call, cancel).await?;
        self.require(profile, &response, &[200])?;
        let body =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        let version = xml::parse_complete_multipart(&body)?.and_then(|etag| {
            // A multipart entity tag is a part digest, never the content hash.
            strong_etag(&etag).map(VersionToken)
        });
        Ok(ObjectReceipt {
            locator,
            byte_length: intent.byte_length,
            version,
            checksum: Some(Checksum {
                algorithm: "sha256".into(),
                value: intent.sha256.clone(),
                provider_verified: false,
            }),
            complete: true,
        })
    }
}

fn intent_locator(
    repository: &RepositoryHandle,
    context: &RepositoryContext,
    intent: &ObjectIntent,
) -> Result<(String, RemoteLocator)> {
    let folder = role_folder(intent.role);
    let object = format!("{folder}/{}", intent.object_id);
    let key = context.key(&object)?;
    Ok((
        key,
        RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: Some(folder.to_owned()),
            object,
        },
    ))
}

/// The key of a locator a cleanup may remove. The head object is a root member
/// with no folder, so it never parses here.
fn removable_key(context: &RepositoryContext, locator: &RemoteLocator) -> Result<String> {
    let unsupported = || ProviderError::new(ErrorKind::Unsupported);
    let (folder, name) = locator.object.split_once('/').ok_or_else(unsupported)?;
    if name.is_empty()
        || name.contains('/')
        || !config::removable_folder(folder)
        || locator
            .collection
            .as_deref()
            .is_some_and(|hint| hint != folder)
    {
        return Err(unsupported());
    }
    context.key(&locator.object)
}

impl Provider for S3Provider {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let context = config::validate(config, secret)?;
            let credentials = self.credentials(&context).await?;
            match mode {
                OpenMode::Create => {
                    self.require_create_layout(&context, &credentials, false, cancel)
                        .await?;
                }
                OpenMode::Existing => {
                    let descriptor_exists = self
                        .descriptor_exists(&context, &credentials, cancel)
                        .await?;
                    if !descriptor_exists {
                        return Err(ProviderError::new(ErrorKind::NotFound));
                    }
                }
                OpenMode::ResumeCreate => {
                    self.require_create_layout(&context, &credentials, true, cancel)
                        .await?;
                }
            }
            let capabilities = capabilities(context.profile);
            let identity = context.connection_identity.clone();
            Ok((
                RepositoryHandle {
                    repository_id: identity.clone(),
                    connection_identity: identity,
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
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let key = context.key(&locator.object)?;
            let credentials = self.credentials(context).await?;
            let conditional = unchanged.filter(|_| context.profile.conditional_get);
            let (mut response, redirected) = self
                .get_object(context, &credentials, &key, conditional, cancel)
                .await?;
            if response.status == 304 {
                if let Some(token) = conditional {
                    return Ok(ReadReceipt::NotModified(token.clone()));
                }
            }
            self.require(context.profile, &response, &[200])?;
            let length = common::content_length(&response.headers)?.ok_or_else(corrupt)?;
            // An edge answers with its own entity tag, which is not the
            // object's version token at the origin.
            let version = (!redirected)
                .then(|| version_of(&response.headers))
                .flatten();
            let (byte_length, sha256) =
                common::stream_to_sink(&mut response.body, sink, Some(length), length, cancel)
                    .await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length,
                version,
                checksum: Some(Checksum {
                    algorithm: "sha256".into(),
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
            let context = context_of(repository)?;
            intent.validate(repository)?;
            let (key, _) = intent_locator(repository, context, intent)?;
            let profile = context.profile;
            if intent.byte_length > profile.max_stored_bytes() {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            if intent.byte_length <= profile.multipart_threshold_bytes {
                return Ok(None);
            }
            let credentials = self.credentials(context).await?;
            let mut response = self
                .dispatch(
                    context,
                    &credentials,
                    Call::object(
                        reqwest::Method::POST,
                        &key,
                        ProviderOperation::UploadSession,
                    )
                    .query("uploads", String::new()),
                    cancel,
                )
                .await?;
            self.require(profile, &response, &[200])?;
            let body =
                common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
            let sealed_state = self
                .seal(&SessionState {
                    upload_id: xml::parse_initiate_multipart(&body)?,
                    key,
                    part_size: profile.part_size_bytes,
                    parts: Vec::new(),
                })
                .await?;
            Ok(Some(ResumeState {
                sealed_state,
                confirmed_offset: 0,
                expires_at_ms: profile
                    .multipart_lifetime_ms
                    .map(|lifetime| self.now_ms().saturating_add(lifetime)),
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
            let context = context_of(repository)?;
            intent.validate(repository)?;
            if source.byte_length() != intent.byte_length {
                return Err(corrupt());
            }
            if intent.byte_length > context.profile.max_stored_bytes() {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let (key, locator) = intent_locator(repository, context, intent)?;
            let credentials = self.credentials(context).await?;
            match resume {
                Some(resume) => {
                    self.continue_multipart(
                        context,
                        &credentials,
                        intent,
                        source,
                        &key,
                        locator,
                        resume,
                        cancel,
                    )
                    .await
                }
                None => {
                    self.single_put(context, &credentials, intent, source, &key, locator, cancel)
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
            let context = context_of(repository)?;
            locator.validate_for(repository)?;
            let key = context.head_key(&locator.object)?;
            if !context.profile.conditional_put {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let (name, value) = match expected {
                ExpectedHead::Absent => ("if-none-match", "*".to_owned()),
                ExpectedHead::Exact(token) => {
                    ("if-match", strong_etag(&token.0).ok_or_else(corrupt)?)
                }
            };
            let credentials = self.credentials(context).await?;
            let response = self
                .dispatch(
                    context,
                    &credentials,
                    head_call(&key, head, ProviderOperation::CompareExchangeHead)
                        .header(name, value),
                    cancel,
                )
                .await?;
            if matches!(response.status, 409 | 412)
                || (response.status == 404 && matches!(expected, ExpectedHead::Exact(_)))
            {
                return Err(precondition(Some(response.status)));
            }
            self.require(context.profile, &response, &[200])?;
            Ok(HeadReceipt {
                version: version_of(&response.headers),
                complete: true,
            })
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
            let key = context.head_key(&locator.object)?;
            let credentials = self.credentials(context).await?;
            let response = self
                .dispatch(
                    context,
                    &credentials,
                    head_call(&key, head, ProviderOperation::ReplaceHead),
                    cancel,
                )
                .await?;
            self.require(context.profile, &response, &[200])?;
            Ok(HeadReceipt {
                version: version_of(&response.headers),
                complete: true,
            })
        })
    }

    /// `DeleteObject` reports 204 for a key that was removed and for one that
    /// was never there, which is the idempotence the contract asks for.
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
            let key = removable_key(context, locator)?;
            let credentials = self.credentials(context).await?;
            let response = self
                .dispatch(
                    context,
                    &credentials,
                    Call::object(reqwest::Method::DELETE, &key, ProviderOperation::Delete),
                    cancel,
                )
                .await?;
            cancel.check()?;
            if response.status == 404 {
                return Ok(());
            }
            if response.status == 202 {
                return Err(common::error(ErrorKind::Unsupported, 202));
            }
            self.require(context.profile, &response, &[200, 204])
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
            if limit == 0 || limit > MAX_LIST_KEYS {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let context = context_of(repository)?;
            let folder = collection_folder(collection);
            let credentials = self.credentials(context).await?;
            let page = self
                .list_page(context, &credentials, folder, cursor, limit, cancel)
                .await?;
            let objects = page
                .objects
                .iter()
                .filter_map(|listed| {
                    let object = context.object_of(folder, &listed.key)?;
                    Some(ObjectReceipt {
                        locator: RemoteLocator {
                            connection_identity: repository.connection_identity.clone(),
                            collection: Some(folder.to_owned()),
                            object,
                        },
                        byte_length: listed.size,
                        version: listed
                            .etag
                            .as_deref()
                            .and_then(strong_etag)
                            .map(VersionToken),
                        checksum: None,
                        complete: true,
                    })
                })
                .collect();
            Ok(ObjectPage {
                objects,
                next_cursor: page.next_continuation_token,
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
            let context = context_of(repository)?;
            intent.validate(repository)?;
            let (key, locator) = intent_locator(repository, context, intent)?;
            let mut state = match resume {
                Some(resume) => {
                    let state = self.session(resume).await?;
                    if state.key != key {
                        return Err(corrupt());
                    }
                    Some(state)
                }
                None => None,
            };
            let credentials = self.credentials(context).await?;
            if let Some(existing) = self
                .head_object(context, &credentials, &key, cancel)
                .await?
            {
                return Ok(
                    match self.converge(context.profile, &existing, intent, locator) {
                        Ok(receipt) => UploadResolution::Complete(receipt),
                        Err(error) if error.kind == ErrorKind::PreconditionFailed => {
                            UploadResolution::Conflict
                        }
                        Err(error) => return Err(error),
                    },
                );
            }
            let (Some(resume), Some(mut state)) = (resume, state.take()) else {
                return Ok(UploadResolution::RestartRequired);
            };
            let mut response = self
                .dispatch(
                    context,
                    &credentials,
                    Call::object(
                        reqwest::Method::GET,
                        &key,
                        ProviderOperation::ReconcileUpload,
                    )
                    .query("uploadId", state.upload_id.clone())
                    .query("max-parts", MAX_LIST_KEYS.to_string()),
                    cancel,
                )
                .await?;
            if matches!(response.status, 404 | 410) {
                return Ok(UploadResolution::RestartRequired);
            }
            self.require(context.profile, &response, &[200])?;
            let body =
                common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
            let listed = xml::parse_list_parts(&body)?;
            let (confirmed_offset, parts) =
                contiguous(&listed, state.part_size, intent.byte_length);
            state.parts = parts;
            self.reseal(&resume.sealed_state, &state).await?;
            Ok(UploadResolution::Resumable(ResumeState {
                sealed_state: resume.sealed_state.clone(),
                confirmed_offset,
                expires_at_ms: resume.expires_at_ms,
            }))
        })
    }

    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        context_of(repository)?;
        Ok(RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: "head".into(),
        })
    }

}

fn head_call(key: &str, head: &HeadBytes, operation: ProviderOperation) -> Call {
    let bytes = head.as_bytes().to_vec();
    let length = bytes.len() as u64;
    Call::object(reqwest::Method::PUT, key, operation)
        .payload(&hex_sha256(&bytes))
        .body(Box::pin(std::io::Cursor::new(bytes)), length)
}

/// Only parts confirmed in an unbroken run from the first one count towards a
/// resumable offset, and every counted part but the object's last must be a
/// full part so the next offset stays aligned.
fn contiguous(
    listed: &[xml::ListedPart],
    part_size: u64,
    byte_length: u64,
) -> (u64, Vec<SessionPart>) {
    let mut parts = Vec::new();
    let mut offset = 0u64;
    for expected in 1..=MAX_PARTS as u32 {
        let Some(part) = listed.iter().find(|part| part.number == expected) else {
            break;
        };
        let end = offset.saturating_add(part.size);
        let last = end == byte_length;
        if end > byte_length || strong_etag(&part.etag).is_none() {
            break;
        }
        if part.size != part_size && !last {
            break;
        }
        offset = end;
        parts.push(SessionPart {
            number: expected,
            etag: part.etag.clone(),
        });
        if last {
            break;
        }
    }
    (offset, parts)
}

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(S3Provider::new(dependencies)))
}
