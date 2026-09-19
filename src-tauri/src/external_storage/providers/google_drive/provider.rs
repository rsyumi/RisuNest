//! File-ID based Drive v3 adapter. Every remote object is one file in the
//! repository root folder, tagged with private application properties, so a
//! duplicate file name never decides which object is read or replaced.
use super::{
    auth::{self, CachedToken},
    config::{self, Settings, Space},
    wire::{self, About, DriveFile, FileList, GeneratedIds},
};
use crate::external_storage::{
    auth::SecretBytes,
    capabilities::Capabilities,
    contract::*,
    http::{self, HttpRequest, HttpResponse},
    providers::{common, Dependencies},
    quota::AccountKey,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Reserved control name of the mutable head. It resolves to a stable file ID
/// held by the handle, which every device rediscovers from the role property.
pub(super) const HEAD_OBJECT: &str = "control-head";
const ROLE_KEY: &str = "risunestRole";
const OBJECT_KEY: &str = "risunestObjectId";
const JOB_KEY: &str = "risunestJobId";
const HEAD_ROLE: &str = "head";
const DESCRIPTOR_ROLE: &str = "descriptor";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const OCTET_STREAM: &str = "application/octet-stream";
const FILE_FIELDS: &str = "id,size,version,sha256Checksum,appProperties";
const LIST_FIELDS: &str = "nextPageToken,incompleteSearch,files(id,size,version,sha256Checksum,appProperties)";
const UPLOAD_ALIGNMENT: u64 = 256 * 1024;
const UPLOAD_CHUNK_BYTES: u64 = 32 * UPLOAD_ALIGNMENT;
/// Documented ceiling of a single multipart or simple upload request.
const MULTIPART_MAX_BYTES: u64 = 5_000_000;
const MAX_STORED_BYTES: u64 = 5_000_000_000_000;
const SESSION_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const CONTROL_PAGE_SIZE: &str = "100";

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn role_token(role: ObjectRole) -> &'static str {
    match role {
        ObjectRole::Descriptor => DESCRIPTOR_ROLE,
        ObjectRole::Pack => "pack",
        ObjectRole::Catalog => "catalog",
        ObjectRole::SyncState => "state",
        ObjectRole::BackupBundle => "bundle",
        ObjectRole::BackupPoint => "backupPoint",
        ObjectRole::InventoryPage => "inventoryPage",
        ObjectRole::Lease => "lease",
    }
}
/// Only names the container a collection lives in. States and bundles share
/// one, so the role returned here never classifies a listed object; the
/// authenticated envelope header does that.
fn collection_role(collection: Collection) -> ObjectRole {
    match collection {
        Collection::Snapshots => ObjectRole::SyncState,
        Collection::BackupPoints => ObjectRole::BackupPoint,
        Collection::InventoryPages => ObjectRole::InventoryPage,
        Collection::Descriptors => ObjectRole::Descriptor,
        Collection::Leases => ObjectRole::Lease,
    }
}
fn collection_token(role: ObjectRole) -> Option<&'static str> {
    match role {
        ObjectRole::SyncState | ObjectRole::BackupBundle => Some("snapshots"),
        ObjectRole::BackupPoint => Some("backupPoints"),
        ObjectRole::InventoryPage => Some("inventory"),
        ObjectRole::Descriptor => Some("descriptors"),
        ObjectRole::Lease => Some("leases"),
        ObjectRole::Pack | ObjectRole::Catalog => None,
    }
}

struct HeadState {
    file_id: String,
    present: bool,
}

pub(super) struct Context {
    settings: Settings,
    secret: SecretRef,
    account: AccountKey,
    token: Mutex<Option<CachedToken>>,
    head: Mutex<HeadState>,
}
impl Context {
    fn session(&self) -> Session<'_> {
        Session {
            settings: &self.settings,
            secret: &self.secret,
            account: &self.account,
            token: &self.token,
        }
    }
}

#[derive(Clone, Copy)]
struct Session<'a> {
    settings: &'a Settings,
    secret: &'a SecretRef,
    account: &'a AccountKey,
    token: &'a Mutex<Option<CachedToken>>,
}

pub(super) struct GoogleDrive {
    dependencies: Dependencies,
}

/// Opaque upload state sealed in the vault. The session URI is a bearer
/// credential, so it never reaches a journal, a locator or a log.
struct SealedUpload {
    file_id: String,
    session_uri: url::Url,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredUpload {
    file_id: String,
    session_uri: String,
}

enum SessionStatus {
    Complete(DriveFile),
    Incomplete(u64),
    Gone,
}

fn with_query(mut url: url::Url, pairs: &[(&str, &str)]) -> url::Url {
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(name, value);
        }
    }
    url
}

fn authorized_headers(token: &str) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {token}"));
    headers
}

fn parse_confirmed_offset(headers: &BTreeMap<String, String>) -> Result<u64> {
    let Some(range) = headers.get("range") else {
        return Ok(0);
    };
    let end = range
        .trim()
        .strip_prefix("bytes=0-")
        .and_then(|end| end.parse::<u64>().ok())
        .ok_or_else(corrupt)?;
    end.checked_add(1).ok_or_else(corrupt)
}

impl GoogleDrive {
    pub(super) fn new(dependencies: Dependencies) -> Self {
        Self { dependencies }
    }
    fn now_ms(&self) -> u64 {
        self.dependencies.clock.now_ms()
    }
    async fn dispatch(&self, request: HttpRequest, cancel: &Cancellation) -> Result<HttpResponse> {
        let mut request = request;
        if http::control_operation(request.operation) {
            http::bypass_cache(&mut request.headers);
        }
        self.dependencies.send(request, cancel).await
    }
    async fn classify_response(
        &self,
        response: &mut HttpResponse,
        account: &AccountKey,
        cancel: &Cancellation,
    ) -> Result<ProviderError> {
        let now = self.now_ms();
        let error = wire::classify(response, now, cancel).await;
        self.dependencies.requests.observe_classified_error(
            account,
            &error,
            &response.headers,
            now,
        )?;
        Ok(error)
    }
    async fn require_status(
        &self,
        response: &mut HttpResponse,
        allowed: &[u16],
        account: &AccountKey,
        cancel: &Cancellation,
    ) -> Result<()> {
        if allowed.contains(&response.status) {
            Ok(())
        } else {
            Err(self.classify_response(response, account, cancel).await?)
        }
    }
    async fn token(
        &self,
        session: Session<'_>,
        force_refresh: bool,
        cancel: &Cancellation,
    ) -> Result<zeroize::Zeroizing<String>> {
        auth::access_token(
            &self.dependencies,
            session.settings,
            session.secret,
            session.account,
            session.token,
            force_refresh,
            cancel,
        )
        .await
    }

    /// Bodyless control request. A rejected access token is refreshed once and
    /// the same request is repeated; no other status is retried here.
    async fn control_request(
        &self,
        session: Session<'_>,
        method: reqwest::Method,
        url: &url::Url,
        operation: ProviderOperation,
        allowed: &[u16],
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        let mut refreshed = false;
        loop {
            let token = self.token(session, refreshed, cancel).await?;
            let request = HttpRequest {
                method: method.clone(),
                url: url.clone(),
                headers: authorized_headers(&token),
                body: None,
                content_length: None,
                operation,
                account: session.account.clone(),
                api_request: true,
                mybox_charge: None,
                control: http::control_operation(operation),
            };
            let mut response = self.dispatch(request, cancel).await?;
            if response.status == 401 && !refreshed {
                refreshed = true;
                continue;
            }
            self.require_status(&mut response, allowed, session.account, cancel).await?;
            return Ok(response);
        }
    }

    async fn control<T: serde::de::DeserializeOwned>(
        &self,
        session: Session<'_>,
        url: &url::Url,
        operation: ProviderOperation,
        cancel: &Cancellation,
    ) -> Result<T> {
        let mut response = self
            .control_request(
                session,
                reqwest::Method::GET,
                url,
                operation,
                &[200],
                cancel,
            )
            .await?;
        wire::json(&mut response, cancel).await
    }

    fn role_query(&self, settings: &Settings, role: &str) -> String {
        format!(
            "'{}' in parents and trashed = false and appProperties has {{ key='{ROLE_KEY}' and value='{role}' }}",
            settings.folder_id
        )
    }
    fn list_url(
        &self,
        settings: &Settings,
        query: &str,
        pairs: &[(&str, &str)],
    ) -> Result<url::Url> {
        let mut parameters: Vec<(&str, &str)> = vec![("q", query), ("fields", LIST_FIELDS)];
        parameters.extend_from_slice(pairs);
        if settings.space == Space::AppData {
            parameters.push(("spaces", config::APP_DATA_FOLDER));
        }
        Ok(with_query(settings.api("/files")?, &parameters))
    }

    async fn list_control_files(
        &self,
        session: Session<'_>,
        query: &str,
        maximum: usize,
        cancel: &Cancellation,
    ) -> Result<Vec<DriveFile>> {
        let mut files = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        loop {
            cancel.check()?;
            let mut parameters = vec![("pageSize", CONTROL_PAGE_SIZE), ("orderBy", "name")];
            if let Some(token) = cursor.as_deref() {
                parameters.push(("pageToken", token));
            }
            let url = self.list_url(session.settings, query, &parameters)?;
            let page: FileList = self.control(session, &url, ProviderOperation::List, cancel).await?;
            page.validate_page(cursor.as_deref())?;
            if files.len().saturating_add(page.files.len()) > maximum {
                return Err(corrupt());
            }
            files.extend(page.files);
            let Some(next) = page.next_page_token else { return Ok(files); };
            if !seen.insert(next.clone()) {
                return Err(corrupt());
            }
            cursor = Some(next);
        }
    }

    async fn generate_id(&self, session: Session<'_>, cancel: &Cancellation) -> Result<String> {
        let url = with_query(
            session.settings.api("/files/generateIds")?,
            &[("count", "1"), ("space", session.settings.space.as_str())],
        );
        let generated: GeneratedIds = self
            .control(session, &url, ProviderOperation::Metadata, cancel)
            .await?;
        generated
            .ids
            .into_iter()
            .find(|id| config::is_drive_id(id))
            .ok_or_else(corrupt)
    }

    fn object_metadata(&self, settings: &Settings, intent: &ObjectIntent, file_id: &str) -> String {
        let role = role_token(intent.role);
        serde_json::json!({
            "id": file_id,
            "name": format!("{role}-{}", intent.object_id),
            "mimeType": OCTET_STREAM,
            "parents": [settings.folder_id.clone()],
            "appProperties": {
                ROLE_KEY: role,
                OBJECT_KEY: intent.object_id.clone(),
                JOB_KEY: intent.job_id.clone(),
            },
        })
        .to_string()
    }

    fn check_intent(&self, intent: &ObjectIntent) -> Result<()> {
        if intent.byte_length == 0 {
            return Err(corrupt());
        }
        if intent.byte_length > MAX_STORED_BYTES {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        let fits = |key: &str, value: &str| key.len() + value.len() <= config::MAX_PROPERTY_BYTES;
        if !fits(OBJECT_KEY, &intent.object_id)
            || !fits(JOB_KEY, &intent.job_id)
            || !fits(ROLE_KEY, role_token(intent.role))
        {
            return Err(config::unsupported());
        }
        Ok(())
    }

    async fn open_session(
        &self,
        session: Session<'_>,
        intent: &ObjectIntent,
        file_id: &str,
        cancel: &Cancellation,
    ) -> Result<url::Url> {
        let url = with_query(
            session.settings.upload("/files")?,
            &[("uploadType", "resumable"), ("fields", FILE_FIELDS)],
        );
        let token = self.token(session, false, cancel).await?;
        let metadata = self.object_metadata(session.settings, intent, file_id);
        let length = metadata.len() as u64;
        let mut headers = authorized_headers(&token);
        headers.insert(
            "content-type".to_owned(),
            "application/json; charset=UTF-8".to_owned(),
        );
        headers.insert("x-upload-content-type".to_owned(), OCTET_STREAM.to_owned());
        headers.insert(
            "x-upload-content-length".to_owned(),
            intent.byte_length.to_string(),
        );
        let request = HttpRequest {
            method: reqwest::Method::POST,
            url: url.clone(),
            headers,
            body: Some(Box::pin(std::io::Cursor::new(metadata.into_bytes()))),
            content_length: Some(length),
            operation: ProviderOperation::UploadSession,
            account: session.account.clone(),
            api_request: true,
            mybox_charge: None,
            control: true,
        };
        let mut response = self.dispatch(request, cancel).await?;
        self.require_status(&mut response, &[200, 201], session.account, cancel).await?;
        let location = response.headers.get("location").ok_or_else(corrupt)?;
        let session_uri = url::Url::options()
            .base_url(Some(&url))
            .parse(location)
            .map_err(|_| corrupt())?;
        // The session URI carries its own upload credential; keeping it on the
        // configured origin also keeps the Authorization header on that origin.
        if !session.settings.same_origin(&session_uri)
            || !session_uri.username().is_empty()
            || session_uri.password().is_some()
            || session_uri.fragment().is_some()
        {
            return Err(corrupt());
        }
        Ok(session_uri)
    }

    async fn seal_upload(&self, upload: &SealedUpload) -> Result<SecretRef> {
        let payload = serde_json::json!({
            "fileId": upload.file_id.clone(),
            "sessionUri": upload.session_uri.as_str(),
        })
        .to_string();
        let bytes = SecretBytes(zeroize::Zeroizing::new(payload.into_bytes()));
        self.dependencies.vault.store(&bytes).await
    }
    async fn open_sealed(
        &self,
        reference: &SecretRef,
        settings: &Settings,
    ) -> Result<SealedUpload> {
        let bytes = self.dependencies.vault.read(reference).await?;
        let stored: StoredUpload = serde_json::from_slice(&bytes.0).map_err(|_| corrupt())?;
        let session_uri = url::Url::parse(&stored.session_uri).map_err(|_| corrupt())?;
        if !config::is_drive_id(&stored.file_id) || !settings.same_origin(&session_uri) {
            return Err(corrupt());
        }
        Ok(SealedUpload {
            file_id: stored.file_id,
            session_uri,
        })
    }

    async fn session_status(
        &self,
        session: Session<'_>,
        session_uri: &url::Url,
        total: u64,
        cancel: &Cancellation,
    ) -> Result<SessionStatus> {
        let token = self.token(session, false, cancel).await?;
        let mut headers = authorized_headers(&token);
        headers.insert("content-range".to_owned(), format!("bytes */{total}"));
        let request = HttpRequest {
            method: reqwest::Method::PUT,
            url: session_uri.clone(),
            headers,
            body: None,
            content_length: Some(0),
            operation: ProviderOperation::ReconcileUpload,
            account: session.account.clone(),
            api_request: true,
            mybox_charge: None,
            control: true,
        };
        let mut response = self.dispatch(request, cancel).await?;
        match response.status {
            200 | 201 => Ok(SessionStatus::Complete(
                wire::json(&mut response, cancel).await?,
            )),
            308 => Ok(SessionStatus::Incomplete(parse_confirmed_offset(
                &response.headers,
            )?)),
            404 | 410 => Ok(SessionStatus::Gone),
            _ => Err(self.classify_response(&mut response, session.account, cancel).await?),
        }
    }

    async fn upload_chunks(
        &self,
        session: Session<'_>,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        upload: &SealedUpload,
        start_offset: u64,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let total = intent.byte_length;
        let mut offset = start_offset;
        if offset > total {
            return Err(corrupt());
        }
        while offset < total {
            cancel.check()?;
            let length = UPLOAD_CHUNK_BYTES.min(total - offset);
            let reader = source.open(offset, length, cancel).await?;
            let token = self.token(session, false, cancel).await?;
            let mut headers = authorized_headers(&token);
            headers.insert(
                "content-range".to_owned(),
                format!("bytes {}-{}/{total}", offset, offset + length - 1),
            );
            headers.insert("content-type".to_owned(), OCTET_STREAM.to_owned());
            let request = HttpRequest {
                method: reqwest::Method::PUT,
                url: upload.session_uri.clone(),
                headers,
                body: Some(reader),
                content_length: Some(length),
                operation: ProviderOperation::UploadChunk,
                account: session.account.clone(),
                api_request: true,
                mybox_charge: None,
                control: false,
            };
            let mut response = self.dispatch(request, cancel).await?;
            match response.status {
                200 | 201 => {
                    let file: DriveFile = wire::json(&mut response, cancel).await?;
                    return self.completed_receipt(
                        session.settings,
                        intent,
                        &file,
                        Some(&upload.file_id),
                    );
                }
                308 => {
                    let confirmed = parse_confirmed_offset(&response.headers)?;
                    if confirmed <= offset {
                        return Err(corrupt());
                    }
                    offset = confirmed;
                }
                404 | 410 => {
                    return Err(common::error(ErrorKind::NotFound, response.status));
                }
                _ => return Err(self.classify_response(&mut response, session.account, cancel).await?),
            }
        }
        match self
            .session_status(session, &upload.session_uri, total, cancel)
            .await?
        {
            SessionStatus::Complete(file) => {
                self.completed_receipt(session.settings, intent, &file, Some(&upload.file_id))
            }
            SessionStatus::Incomplete(_) => Err(ProviderError::new(ErrorKind::Transient)),
            SessionStatus::Gone => Err(ProviderError::new(ErrorKind::NotFound)),
        }
    }

    /// A finished upload is only complete when the service reports the file
    /// this job addressed, its exact length, and the digest it computed itself
    /// when it exposes one.
    fn completed_receipt(
        &self,
        settings: &Settings,
        intent: &ObjectIntent,
        file: &DriveFile,
        expected_id: Option<&str>,
    ) -> Result<ObjectReceipt> {
        let file_id = file.file_id()?;
        if expected_id.is_some_and(|expected| expected != file_id)
            || file.byte_length()? != intent.byte_length
        {
            return Err(corrupt());
        }
        let checksum = file.checksum(Some(&intent.sha256));
        if checksum
            .as_ref()
            .is_some_and(|checksum| !checksum.provider_verified)
        {
            return Err(corrupt());
        }
        Ok(ObjectReceipt {
            locator: RemoteLocator {
                connection_identity: settings.connection_identity.clone(),
                collection: collection_token(intent.role).map(str::to_owned),
                object: file_id.to_owned(),
            },
            byte_length: intent.byte_length,
            version: file.version_token(),
            checksum,
            complete: true,
        })
    }

    /// An earlier attempt may have stored the object before its response was
    /// lost. Identical bytes converge; different bytes never overwrite.
    async fn existing_object(
        &self,
        session: Session<'_>,
        intent: &ObjectIntent,
        cancel: &Cancellation,
    ) -> Result<Option<DriveFile>> {
        let query = format!(
            "'{}' in parents and trashed = false and appProperties has {{ key='{OBJECT_KEY}' and value='{}' }}",
            session.settings.folder_id,
            config::escape_query_literal(&intent.object_id)?
        );
        let mut files = self.list_control_files(session, &query, 2, cancel).await?.into_iter();
        let Some(file) = files.next() else {
            return Ok(None);
        };
        if files.next().is_some() {
            return Err(corrupt());
        }
        if file.property(ROLE_KEY) != Some(role_token(intent.role))
            || file.byte_length()? != intent.byte_length
            || file
                .checksum(Some(&intent.sha256))
                .is_some_and(|checksum| !checksum.provider_verified)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(Some(file))
    }

    async fn multipart_create(
        &self,
        session: Session<'_>,
        intent: &ObjectIntent,
        source: &dyn TransferSource,
        file_id: &str,
        cancel: &Cancellation,
    ) -> Result<ObjectReceipt> {
        let url = with_query(
            session.settings.upload("/files")?,
            &[("uploadType", "multipart"), ("fields", FILE_FIELDS)],
        );
        let metadata = self.object_metadata(session.settings, intent, file_id);
        let content = source.open(0, intent.byte_length, cancel).await?;
        let token = self.token(session, false, cancel).await?;
        let (body, length, content_type) = multipart_body(&metadata, content, intent.byte_length);
        let mut headers = authorized_headers(&token);
        headers.insert("content-type".to_owned(), content_type);
        let request = HttpRequest {
            method: reqwest::Method::POST,
            url,
            headers,
            body: Some(body),
            content_length: Some(length),
            operation: ProviderOperation::Create,
            account: session.account.clone(),
            api_request: true,
            mybox_charge: None,
            control: false,
        };
        let mut response = self.dispatch(request, cancel).await?;
        self.require_status(&mut response, &[200, 201], session.account, cancel).await?;
        let file: DriveFile = wire::json(&mut response, cancel).await?;
        self.completed_receipt(session.settings, intent, &file, Some(file_id))
    }
}

fn multipart_body(
    metadata: &str,
    content: Pin<Box<dyn AsyncRead + Send>>,
    content_length: u64,
) -> (Pin<Box<dyn AsyncRead + Send>>, u64, String) {
    let boundary = format!("risunest-{}", uuid::Uuid::new_v4().simple());
    let prefix = format!(
        "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: {OCTET_STREAM}\r\n\r\n"
    );
    let suffix = format!("\r\n--{boundary}--");
    let length = prefix.len() as u64 + content_length + suffix.len() as u64;
    let body = std::io::Cursor::new(prefix.into_bytes())
        .chain(content)
        .chain(std::io::Cursor::new(suffix.into_bytes()));
    (
        Box::pin(body),
        length,
        format!("multipart/related; boundary={boundary}"),
    )
}

fn context(repository: &RepositoryHandle) -> Result<&Context> {
    let context = repository
        .context
        .downcast_ref::<Context>()
        .ok_or_else(corrupt)?;
    if context.settings.connection_identity != repository.connection_identity {
        return Err(corrupt());
    }
    Ok(context)
}

fn resolve_file_id(context: &Context, locator: &RemoteLocator) -> Result<String> {
    if locator.object == HEAD_OBJECT {
        let head = context.head.lock().unwrap();
        return if head.present {
            Ok(head.file_id.clone())
        } else {
            Err(ProviderError::new(ErrorKind::NotFound))
        };
    }
    if !config::is_drive_id(&locator.object) {
        return Err(corrupt());
    }
    Ok(locator.object.clone())
}

/// A member of this repository folder whose role is a cleanup target. The head
/// and the descriptor role are never removed here.
fn removable(settings: &Settings, file: &DriveFile) -> bool {
    let parented = file
        .parents
        .as_ref()
        .is_some_and(|parents| parents.iter().any(|id| *id == settings.folder_id));
    parented
        && file.property(ROLE_KEY).is_some_and(|role| {
            [
                ObjectRole::Pack,
                ObjectRole::Catalog,
                ObjectRole::SyncState,
                ObjectRole::BackupBundle,
                ObjectRole::BackupPoint,
                ObjectRole::InventoryPage,
                ObjectRole::Lease,
            ]
            .iter()
            .any(|known| role_token(*known) == role)
        })
}

fn validate_resumable_descriptor(file: &DriveFile) -> Result<()> {
    if !config::is_drive_id(file.file_id()?)
        || file.byte_length()? == 0
        || file.version_token().is_none()
    {
        return Err(corrupt());
    }
    let object_id = file.property(OBJECT_KEY).ok_or_else(corrupt)?;
    if !config::is_drive_id(object_id) || file.property(JOB_KEY) != Some(object_id) {
        return Err(corrupt());
    }
    if file
        .sha256_checksum
        .as_deref()
        .is_some_and(|checksum| !crate::trust_boundary::is_lower_hex_256(checksum))
    {
        return Err(corrupt());
    }
    Ok(())
}

fn capabilities() -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        // The read-only change counter is not an exchange token.
        atomic_create_head: false,
        conditional_head_update: false,
        stable_head_replace: true,
        head_read_after_write: true,
        head_retry_control: true,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        conditional_get: false,
        range: true,
        resumable_upload: true,
        max_stored_bytes: Some(MAX_STORED_BYTES),
        sdk_overhead_bytes: 0,
        upload_alignment: UPLOAD_ALIGNMENT,
    }
}

impl Provider for GoogleDrive {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let settings = Settings::parse(config)?;
            let account = AccountKey::new(
                config::PROVIDER_ID,
                &settings.api("/")?,
                &settings.account_id,
            )?;
            let token = Mutex::new(None);
            let session = Session {
                settings: &settings,
                secret,
                account: &account,
                token: &token,
            };
            let about: About = self
                .control(
                    session,
                    &with_query(settings.api("/about")?, &[("fields", "user(permissionId)")]),
                    ProviderOperation::Metadata,
                    cancel,
                )
                .await?;
            let identity = about.user.and_then(|user| user.permission_id);
            if identity.as_deref() != Some(settings.account_id.as_str()) {
                return Err(ProviderError::new(ErrorKind::Unauthorized));
            }
            if settings.space == Space::Drive {
                let folder: DriveFile = self
                    .control(
                        session,
                        &with_query(
                            settings.api(&format!("/files/{}", settings.folder_id))?,
                            &[("fields", "id,mimeType,trashed")],
                        ),
                        ProviderOperation::Metadata,
                        cancel,
                    )
                    .await?;
                if folder.mime_type.as_deref() != Some(FOLDER_MIME) || folder.trashed == Some(true)
                {
                    return Err(ProviderError::new(ErrorKind::NotFound));
                }
            }
            let query = if matches!(mode, OpenMode::Create | OpenMode::ResumeCreate) {
                format!("'{}' in parents and trashed = false", settings.folder_id)
            } else {
                format!(
                    "'{}' in parents and trashed = false and (appProperties has {{ key='{ROLE_KEY}' and value='{HEAD_ROLE}' }} or appProperties has {{ key='{ROLE_KEY}' and value='{DESCRIPTOR_ROLE}' }})",
                    settings.folder_id
                )
            };
            let control = self.list_control_files(session, &query, 3, cancel).await?;
            let heads: Vec<&DriveFile> = control
                .iter()
                .filter(|file| file.property(ROLE_KEY) == Some(HEAD_ROLE))
                .collect();
            let descriptors: Vec<&DriveFile> = control
                .iter()
                .filter(|file| file.property(ROLE_KEY) == Some(DESCRIPTOR_ROLE))
                .collect();
            if heads.len() > 1 {
                // Drive names are not unique; two control files are ambiguous
                // and must never be resolved by picking one of them.
                return Err(corrupt());
            }
            match mode {
                OpenMode::Create if !control.is_empty() => {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                OpenMode::ResumeCreate
                    if !heads.is_empty()
                        || descriptors.len() > 2
                        || heads.len() + descriptors.len() != control.len() =>
                {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                OpenMode::ResumeCreate => {
                    for descriptor in descriptors {
                        validate_resumable_descriptor(descriptor)?;
                    }
                }
                OpenMode::Existing if descriptors.is_empty() => {
                    return Err(ProviderError::new(ErrorKind::NotFound));
                }
                _ => {}
            }
            let head = match heads.first() {
                Some(file) => HeadState {
                    file_id: file.file_id()?.to_owned(),
                    present: true,
                },
                None => HeadState {
                    file_id: self.generate_id(session, cancel).await?,
                    present: false,
                },
            };
            let handle = RepositoryHandle {
                repository_id: format!(
                    "{}:{}:{}",
                    config::PROVIDER_ID,
                    settings.account_id,
                    settings.folder_id
                ),
                connection_identity: settings.connection_identity.clone(),
                account: account.clone(),
                context: Box::new(Context {
                    settings,
                    secret: secret.clone(),
                    account,
                    token,
                    head: Mutex::new(head),
                }),
            };
            Ok((handle, capabilities()))
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
            let context = context(repository)?;
            locator.validate_for(repository)?;
            let file_id = resolve_file_id(context, locator)?;
            let session = context.session();
            let metadata: DriveFile = self
                .control(
                    session,
                    &with_query(
                        context.settings.api(&format!("/files/{file_id}"))?,
                        &[("fields", "id,size,version,sha256Checksum")],
                    ),
                    ProviderOperation::Metadata,
                    cancel,
                )
                .await?;
            let version = metadata.version_token();
            if let (Some(unchanged), Some(current)) = (unchanged, version.as_ref()) {
                if unchanged == current {
                    return Ok(ReadReceipt::NotModified(current.clone()));
                }
            }
            let length = metadata.byte_length()?;
            let token = self.token(session, false, cancel).await?;
            let request = HttpRequest {
                method: reqwest::Method::GET,
                url: with_query(
                    context.settings.api(&format!("/files/{file_id}"))?,
                    &[("alt", "media")],
                ),
                headers: authorized_headers(&token),
                body: None,
                content_length: None,
                operation: ProviderOperation::Get,
                account: session.account.clone(),
                api_request: true,
                mybox_charge: None,
                control: false,
            };
            let mut response = self.dispatch(request, cancel).await?;
            self.require_status(&mut response, &[200], session.account, cancel).await?;
            let declared = common::content_length(&response.headers)?;
            if declared.is_some_and(|declared| declared != length) {
                return Err(corrupt());
            }
            let (received, hash) =
                common::stream_to_sink(&mut response.body, sink, Some(length), length, cancel)
                    .await?;
            let checksum = metadata.checksum(Some(&hash));
            if checksum
                .as_ref()
                .is_some_and(|checksum| !checksum.provider_verified)
            {
                // The service computed a different digest over the stored bytes.
                return Err(corrupt());
            }
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length: received,
                version,
                checksum,
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
            let context = context(repository)?;
            intent.validate(repository)?;
            self.check_intent(intent)?;
            let session = context.session();
            if intent.role == ObjectRole::Descriptor
                && self.existing_object(session, intent, cancel).await?.is_some()
            {
                // Descriptor retries must converge before opening a new upload
                // session, otherwise a lost completion can create a duplicate.
                return Ok(None);
            }
            let file_id = self.generate_id(session, cancel).await?;
            let session_uri = self.open_session(session, intent, &file_id, cancel).await?;
            let sealed_state = self
                .seal_upload(&SealedUpload {
                    file_id,
                    session_uri,
                })
                .await?;
            Ok(Some(ResumeState {
                sealed_state,
                confirmed_offset: 0,
                expires_at_ms: Some(self.now_ms().saturating_add(SESSION_LIFETIME_MS)),
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
            let context = context(repository)?;
            intent.validate(repository)?;
            self.check_intent(intent)?;
            if source.byte_length() != intent.byte_length {
                return Err(corrupt());
            }
            let session = context.session();
            if let Some(resume) = resume {
                let upload = self
                    .open_sealed(&resume.sealed_state, &context.settings)
                    .await?;
                return self
                    .upload_chunks(
                        session,
                        intent,
                        source,
                        &upload,
                        resume.confirmed_offset,
                        cancel,
                    )
                    .await;
            }
            if let Some(existing) = self.existing_object(session, intent, cancel).await? {
                return self.completed_receipt(&context.settings, intent, &existing, None);
            }
            let file_id = self.generate_id(session, cancel).await?;
            if intent.byte_length <= MULTIPART_MAX_BYTES {
                return self
                    .multipart_create(session, intent, source, &file_id, cancel)
                    .await;
            }
            let session_uri = self.open_session(session, intent, &file_id, cancel).await?;
            self.upload_chunks(
                session,
                intent,
                source,
                &SealedUpload {
                    file_id,
                    session_uri,
                },
                0,
                cancel,
            )
            .await
        })
    }

    /// Drive v3 documents no precondition on the path that replaces file
    /// content, so no compare and exchange is offered instead of emulating one.
    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        _expected: &'a ExpectedHead,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            context(repository)?;
            locator.validate_for(repository)?;
            Err(config::unsupported())
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
            let context = context(repository)?;
            locator.validate_for(repository)?;
            if locator.object != HEAD_OBJECT {
                return Err(corrupt());
            }
            let session = context.session();
            let (file_id, present) = {
                let head = context.head.lock().unwrap();
                (head.file_id.clone(), head.present)
            };
            let bytes = head.as_bytes().to_vec();
            let length = bytes.len() as u64;
            let token = self.token(session, false, cancel).await?;
            let mut headers = authorized_headers(&token);
            let request = if present {
                headers.insert("content-type".to_owned(), OCTET_STREAM.to_owned());
                HttpRequest {
                    method: reqwest::Method::PATCH,
                    url: with_query(
                        context.settings.upload(&format!("/files/{file_id}"))?,
                        &[("uploadType", "media"), ("fields", FILE_FIELDS)],
                    ),
                    headers,
                    body: Some(Box::pin(std::io::Cursor::new(bytes))),
                    content_length: Some(length),
                    operation: ProviderOperation::ReplaceHead,
                    account: session.account.clone(),
                    api_request: true,
                    mybox_charge: None,
                    control: true,
                }
            } else {
                let metadata = serde_json::json!({
                    "id": file_id,
                    "name": HEAD_OBJECT,
                    "mimeType": OCTET_STREAM,
                    "parents": [context.settings.folder_id.clone()],
                    "appProperties": { ROLE_KEY: HEAD_ROLE },
                })
                .to_string();
                let (body, total, content_type) =
                    multipart_body(&metadata, Box::pin(std::io::Cursor::new(bytes)), length);
                headers.insert("content-type".to_owned(), content_type);
                HttpRequest {
                    method: reqwest::Method::POST,
                    url: with_query(
                        context.settings.upload("/files")?,
                        &[("uploadType", "multipart"), ("fields", FILE_FIELDS)],
                    ),
                    headers,
                    body: Some(body),
                    content_length: Some(total),
                    operation: ProviderOperation::ReplaceHead,
                    account: session.account.clone(),
                    api_request: true,
                    mybox_charge: None,
                    control: true,
                }
            };
            // Exactly one write attempt: an ambiguous outcome is reported so the
            // owner re-observes the head instead of writing again.
            let mut response = self.dispatch(request, cancel).await?;
            self.require_status(&mut response, &[200, 201], session.account, cancel).await?;
            let file: DriveFile = wire::json(&mut response, cancel).await?;
            if file.file_id()? != file_id {
                return Err(corrupt());
            }
            context.head.lock().unwrap().present = true;
            Ok(HeadReceipt {
                version: file.version_token(),
                complete: true,
            })
        })
    }

    /// `files.delete` removes the file outright rather than trashing it, so the
    /// target is confirmed to be a removable member of this repository folder
    /// before the request goes out.
    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            cancel.check()?;
            let context = context(repository)?;
            locator.validate_for(repository)?;
            if locator.object == HEAD_OBJECT || !config::is_drive_id(&locator.object) {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let file_id = &locator.object;
            let session = context.session();
            let metadata: DriveFile = match self
                .control(
                    session,
                    &with_query(
                        context.settings.api(&format!("/files/{file_id}"))?,
                        &[("fields", "id,parents,appProperties")],
                    ),
                    ProviderOperation::Metadata,
                    cancel,
                )
                .await
            {
                Ok(metadata) => metadata,
                Err(error) if error.kind == ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if !removable(&context.settings, &metadata) {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            self.control_request(
                session,
                reqwest::Method::DELETE,
                &context.settings.api(&format!("/files/{file_id}"))?,
                ProviderOperation::Delete,
                &[200, 204, 404],
                cancel,
            )
            .await
            .map(|_| ())
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
            let context = context(repository)?;
            if limit == 0 || limit > 1000 {
                return Err(config::unsupported());
            }
            let role = collection_role(collection);
            let page_size = limit.to_string();
            let mut parameters: Vec<(&str, &str)> =
                vec![("pageSize", &page_size), ("orderBy", "name")];
            if let Some(cursor) = cursor {
                if cursor.is_empty() || cursor.len() > 4096 {
                    return Err(corrupt());
                }
                parameters.push(("pageToken", cursor));
            }
            let url = self.list_url(
                &context.settings,
                &self.role_query(&context.settings, role_token(role)),
                &parameters,
            )?;
            let page: FileList = self
                .control(context.session(), &url, ProviderOperation::List, cancel)
                .await?;
            page.validate_page(cursor)?;
            let mut objects = Vec::with_capacity(page.files.len());
            for file in &page.files {
                objects.push(ObjectReceipt {
                    locator: RemoteLocator {
                        connection_identity: context.settings.connection_identity.clone(),
                        collection: collection_token(role).map(str::to_owned),
                        object: file.file_id()?.to_owned(),
                    },
                    byte_length: file.byte_length()?,
                    version: file.version_token(),
                    // Nothing was compared here, so a reported digest stays
                    // unverified until the object is read or created.
                    checksum: file.checksum(None),
                    complete: true,
                });
            }
            Ok(ObjectPage {
                objects,
                next_cursor: page.next_page_token,
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
            let context = context(repository)?;
            intent.validate(repository)?;
            let session = context.session();
            let Some(resume) = resume else {
                return match self.existing_object(session, intent, cancel).await? {
                    Some(file) => {
                        match self.completed_receipt(&context.settings, intent, &file, None) {
                            Ok(receipt) => Ok(UploadResolution::Complete(receipt)),
                            Err(error) if error.kind == ErrorKind::PreconditionFailed => {
                                Ok(UploadResolution::Conflict)
                            }
                            Err(error) => Err(error),
                        }
                    }
                    None => Ok(UploadResolution::RestartRequired),
                };
            };
            let upload = self
                .open_sealed(&resume.sealed_state, &context.settings)
                .await?;
            let status = self
                .session_status(session, &upload.session_uri, intent.byte_length, cancel)
                .await?;
            let file = match status {
                SessionStatus::Incomplete(confirmed_offset) => {
                    if confirmed_offset > intent.byte_length {
                        return Err(corrupt());
                    }
                    return Ok(UploadResolution::Resumable(ResumeState {
                        sealed_state: resume.sealed_state.clone(),
                        confirmed_offset,
                        expires_at_ms: resume.expires_at_ms,
                    }));
                }
                SessionStatus::Complete(file) => file,
                SessionStatus::Gone => {
                    // The session is gone; only the file itself can say whether
                    // the object was stored before it expired.
                    let found: Result<DriveFile> = self
                        .control(
                            session,
                            &with_query(
                                context
                                    .settings
                                    .api(&format!("/files/{}", upload.file_id))?,
                                &[("fields", FILE_FIELDS)],
                            ),
                            ProviderOperation::Metadata,
                            cancel,
                        )
                        .await;
                    match found {
                        Ok(file) => file,
                        Err(error) if error.kind == ErrorKind::NotFound => {
                            return Ok(UploadResolution::RestartRequired)
                        }
                        Err(error) => return Err(error),
                    }
                }
            };
            let same_object = file.file_id().ok() == Some(upload.file_id.as_str())
                && file.byte_length().ok() == Some(intent.byte_length)
                && !file
                    .checksum(Some(&intent.sha256))
                    .is_some_and(|checksum| !checksum.provider_verified);
            if !same_object {
                return Ok(UploadResolution::Conflict);
            }
            Ok(UploadResolution::Complete(self.completed_receipt(
                &context.settings,
                intent,
                &file,
                Some(&upload.file_id),
            )?))
        })
    }

    /// The reserved control name resolves to the stable head file id held by
    /// the handle, so the same locator works before and after the first write.
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        context(repository)?;
        Ok(RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: HEAD_OBJECT.to_owned(),
        })
    }

}

pub(super) fn provider(dependencies: Dependencies) -> Arc<dyn Provider> {
    Arc::new(GoogleDrive::new(dependencies))
}
