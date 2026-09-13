use super::capabilities::Capabilities;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) type Result<T> = std::result::Result<T, ProviderError>;
pub(crate) type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ErrorKind {
    Unauthorized,
    ReauthRequired,
    NotFound,
    PreconditionFailed,
    RateLimited,
    DailyQuotaExhausted,
    StorageFull,
    FileTooLarge,
    Corrupt,
    Transient,
    Unsupported,
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderError {
    pub kind: ErrorKind,
    pub http_status: Option<u16>,
    pub retry_at_ms: Option<u64>,
}
impl ProviderError {
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            http_status: None,
            retry_at_ms: None,
        }
    }
}
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}
impl std::error::Error for ProviderError {}

/// Native vault identifier, never plaintext credentials or a signed URL.
/// No Debug/Serialize implementation: provider contexts stay outside UI DTOs.
#[derive(Clone)]
pub(crate) struct SecretRef(pub String);
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionConfig {
    pub provider: String,
    pub profile: Option<String>,
    pub endpoint: String,
    pub account_id: String,
    pub location: BTreeMap<String, String>,
    pub oauth_profile: Option<OAuthProfile>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OAuthProfile {
    pub project_id: String,
    pub platform_client_ids: BTreeMap<String, String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    Create,
    Existing,
}

pub(crate) struct RepositoryHandle {
    pub repository_id: String,
    /// Provider/account/endpoint/root identity, checked before reusing a locator.
    pub connection_identity: String,
    pub context: Box<dyn std::any::Any + Send + Sync>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemoteLocator {
    pub connection_identity: String,
    pub collection: Option<String>,
    pub object: String,
}
impl RemoteLocator {
    pub fn validate_for(&self, repository: &RepositoryHandle) -> Result<()> {
        if self.connection_identity != repository.connection_identity
            || self.object.is_empty()
            || self.object.len() > 8192
            || self.object.contains('\0')
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct VersionToken(pub String);
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExpectedHead {
    Absent,
    Exact(VersionToken),
}
pub(crate) use risunest_external_storage_format::format::Strategy as PublicationStrategy;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ObjectRole {
    Descriptor,
    Pack,
    Catalog,
    Snapshot,
    BackupPoint,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ObjectIntent {
    pub repository_id: String,
    pub job_id: String,
    pub object_id: String,
    pub role: ObjectRole,
    pub byte_length: u64,
    pub sha256: String,
}
impl ObjectIntent {
    pub fn validate(&self, repository: &RepositoryHandle) -> Result<()> {
        if self.repository_id != repository.repository_id
            || self.job_id.is_empty()
            || self.object_id.is_empty()
            || !crate::trust_boundary::is_lower_hex_256(&self.sha256)
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}
impl Cancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    pub fn check(&self) -> Result<()> {
        if self.0.cancelled.load(Ordering::Acquire) {
            Err(ProviderError::new(ErrorKind::Cancelled))
        } else {
            Ok(())
        }
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check().is_err() {
                return;
            }
            notified.await;
        }
    }
}

/// Reader/sink construction is native-job owned. No path or byte-buffer IPC.
/// A resumed request opens a new bounded reader at its remotely confirmed offset.
pub(crate) trait TransferSource: Send + Sync {
    fn byte_length(&self) -> u64;
    fn open<'a>(
        &'a self,
        offset: u64,
        length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncRead + Send>>>;
}
pub(crate) trait TransferSink: Send {
    fn open<'a>(
        &'a mut self,
        offset: u64,
        max_length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncWrite + Send>>>;
    /// Hash, length and fsync are checked before a receipt may reference this file.
    fn finish<'a>(&'a mut self, length: u64, sha256: &'a str) -> ProviderFuture<'a, ()>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checksum {
    pub algorithm: String,
    pub value: String,
    pub provider_verified: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObjectReceipt {
    pub locator: RemoteLocator,
    pub byte_length: u64,
    pub version: Option<VersionToken>,
    pub checksum: Option<Checksum>,
    pub complete: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadReceipt {
    NotModified(VersionToken),
    Body(ObjectReceipt),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadReceipt {
    pub version: Option<VersionToken>,
    pub complete: bool,
}
/// Opaque provider state is sealed before journal storage. Offset is remote-confirmed.
#[derive(Clone)]
pub(crate) struct ResumeState {
    pub sealed_state: SecretRef,
    pub confirmed_offset: u64,
    pub expires_at_ms: Option<u64>,
}
pub(crate) enum UploadResolution {
    Complete(ObjectReceipt),
    Resumable(ResumeState),
    RestartRequired,
    Conflict,
}
#[derive(Clone, Debug)]
pub(crate) struct ObjectPage {
    pub objects: Vec<ObjectReceipt>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Collection {
    Snapshots,
    BackupPoints,
    Descriptors,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ProviderOperation {
    Metadata,
    List,
    DownloadUrl,
    Get,
    Range,
    Create,
    UploadSession,
    UploadChunk,
    CompleteUpload,
    ReconcileUpload,
    CompareExchangeHead,
    ReplaceHead,
    Authenticate,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QuotaReset {
    At { unix_ms: u64 },
    Rolling { window_ms: u64 },
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RequestCost {
    pub bucket: String,
    pub shared_account: String,
    pub units: u64,
    pub reset: QuotaReset,
}

/// One call represents one provider operation. SDK subrequests, session control
/// and retries must individually pass the injected quota/HTTP boundary.
pub(crate) trait Provider: Send + Sync {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)>;
    fn read_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt>;
    fn create_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt>;
    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        expected: &'a ExpectedHead,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt>;
    /// Exactly one ordinary write attempt. Never retry after an ambiguous response.
    fn replace_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt>;
    fn list_objects<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        collection: Collection,
        cursor: Option<&'a str>,
        limit: u16,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage>;
    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        resume: &'a ResumeState,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution>;
    fn request_cost(&self, operation: ProviderOperation) -> Vec<RequestCost>;
}

#[derive(Clone)]
pub(crate) struct HeadBytes(Vec<u8>);
impl HeadBytes {
    pub const MAX_BYTES: usize = 64 * 1024;
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > Self::MAX_BYTES {
            Err(ProviderError::new(ErrorKind::FileTooLarge))
        } else {
            Ok(Self(bytes))
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
