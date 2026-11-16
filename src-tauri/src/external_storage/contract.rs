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

#[derive(Default)]
pub(crate) struct IncompleteCleanupPage {
    pub removed: u64,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ErrorKind {
    Unauthorized,
    ReauthRequired,
    EndpointRejected,
    DeviceVaultUnavailable,
    RepositoryKeyUnavailable,
    ClockSkew,
    LocationOccupied,
    RepositoryBusy,
    LocalStorageFull,
    LocalPermissionDenied,
    /// Any other failure of a file, store or worker on this device.
    LocalFailure,
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
    FolderNameConflict,
    FolderCreateFailed,
    FolderInaccessible,
    FolderNotRepository,
    FolderUnsupportedLocation,
    /// A body this device does not hold could not be fetched from the server
    /// or external storage that holds it.
    PreviousStorageUnavailable,
    /// The recovery key is malformed or does not open the repository at this
    /// location.
    RecoveryKeyMismatch,
    /// The location holds a repository other than the one expected.
    RepositoryMismatch,
    /// The library is at a lower format level than this app writes.
    LibraryFormatSwitchRequired,
    /// The library is at a higher format level than this app reads.
    AppUpdateRequired,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderError {
    pub kind: ErrorKind,
    pub http_status: Option<u16>,
    pub retry_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_error_description: Option<String>,
    /// A redacted cause shared by command replies, job failures and device logs.
    #[serde(default, rename = "detail", skip_serializing_if = "ErrorCause::is_none")]
    pub cause: ErrorCause,
    /// The storage connection whose sign-in, key or address has to be fixed, for a
    /// failure an operation hit reading through a connection other than its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
}
/// Diagnostic wording does not change an error's classification or equality.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct ErrorCause(pub Option<String>);
impl ErrorCause {
    fn is_none(&self) -> bool { self.0.is_none() }
}
impl PartialEq for ErrorCause {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ErrorCause {}
impl ProviderError {
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            http_status: None,
            retry_at_ms: None,
            oauth_error: None,
            oauth_error_description: None,
            cause: ErrorCause::default(),
            connection_id: None,
        }
    }
    /// Retains the failure reason without credentials or signed URL parameters.
    pub fn caused<E: std::fmt::Display + ?Sized>(mut self, error: &E) -> Self {
        static URLS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(||
            regex::Regex::new(r#"(?i)https?://[^\s<>"']+"#).unwrap());
        let text = crate::native_log::failure_text(error);
        let text = URLS.replace_all(&text, |captures: &regex::Captures<'_>| {
            let Ok(mut url) = url::Url::parse(&captures[0]) else { return "[invalid URL]".to_owned(); };
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        });
        self.cause = ErrorCause(Some(crate::native_log::mask(&text)));
        self
    }
}
impl crate::native_log::CommandFailure for ProviderError {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        serde_json::to_value(self.kind)
            .ok()
            .and_then(|kind| kind.as_str().map(str::to_owned))
            .unwrap_or_default()
            .into()
    }
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        match (&self.cause.0, self.http_status) {
            (Some(cause), Some(status)) => Some(format!("{cause}; http {status}").into()),
            (Some(cause), None) => Some(cause.as_str().into()),
            (None, Some(status)) => Some(format!("http {status}").into()),
            (None, None) => None,
        }
    }
    fn expected(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Cancelled
                | ErrorKind::RepositoryBusy
                | ErrorKind::PreconditionFailed
                | ErrorKind::RateLimited
        )
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
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConnectionConfig {
    pub provider: String,
    pub profile: Option<String>,
    pub endpoint: String,
    pub account_id: String,
    pub location: BTreeMap<String, String>,
    pub oauth_profile: Option<OAuthProfile>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OAuthProfile {
    pub project_id: String,
    pub platform_client_ids: BTreeMap<String, String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenMode {
    Create,
    /// Resume the same durable create intent after an earlier attempt may
    /// have initialized part or all of the provider-owned remote layout.
    ResumeCreate,
    Existing,
}

pub(crate) struct RepositoryHandle {
    /// Provider-defined identity of the remote root, computed identically on
    /// every device from the same connection. It is not the descriptor's own
    /// repository id; `ObjectIntent.repository_id` carries this value back.
    pub repository_id: String,
    /// Provider/account/endpoint/root identity, checked before reusing a locator.
    pub connection_identity: String,
    /// Stable authenticated account scope for waits and API clock observations.
    pub account: super::quota::AccountKey,
    pub context: Box<dyn std::any::Any + Send + Sync>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemoteLocator {
    pub connection_identity: String,
    pub collection: Option<String>,
    pub object: String,
}
pub(crate) const MAX_LOCATOR_OBJECT_BYTES: usize = 8192;
impl RemoteLocator {
    pub fn validate_for(&self, repository: &RepositoryHandle) -> Result<()> {
        if self.connection_identity != repository.connection_identity
            || self.object.is_empty()
            || self.object.len() > MAX_LOCATOR_OBJECT_BYTES
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
    SyncState,
    Segment,
    Snapshot,
    BackupBundle,
    BackupPoint,
    InventoryPage,
    Lease,
}

/// What a lease in `Collection::Leases` announces. The word is plain so a
/// publisher can tell a delete marker from ordinary work by enumeration alone.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum LeaseKind {
    Work,
    Cleanup,
    Deleting,
}
impl LeaseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Cleanup => "cleanup",
            Self::Deleting => "deleting",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "work" => Self::Work,
            "cleanup" => Self::Cleanup,
            "deleting" => Self::Deleting,
            _ => return None,
        })
    }
}

/// 128 random bits, chosen before the object is written so a retry after an
/// unclear answer names the same object again.
pub(crate) const LEASE_TAG_HEX: usize = 32;

/// `{kind}-{tag}`. No writer or job identity appears, so an observer who can
/// only enumerate the repository cannot count the devices using it.
pub(crate) fn lease_object_id(kind: LeaseKind, tag: &str) -> Result<String> {
    if tag.len() != LEASE_TAG_HEX
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(format!("{}-{tag}", kind.as_str()))
}

pub(crate) fn parse_lease_object_id(object_id: &str) -> Result<(LeaseKind, String)> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let (kind, tag) = object_id.split_once('-').ok_or_else(corrupt)?;
    let kind = LeaseKind::parse(kind).ok_or_else(corrupt)?;
    if lease_object_id(kind, tag)? != object_id {
        return Err(corrupt());
    }
    Ok((kind, tag.to_owned()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoleMemberName { Owned, Foreign, Ambiguous }

pub(crate) fn role_member_name(collection: Collection, name: &str) -> RoleMemberName {
    if name.starts_with('.') || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini") {
        return RoleMemberName::Foreign;
    }
    if collection == Collection::Leases {
        if parse_lease_object_id(name).is_ok() { return RoleMemberName::Owned; }
        if ["work-", "cleanup-", "deleting-"].iter().any(|prefix| name.starts_with(prefix)) {
            return RoleMemberName::Ambiguous;
        }
        return RoleMemberName::Foreign;
    }
    RoleMemberName::Ambiguous
}

/// Whether a member of the snapshots folder belongs to this listing. Only
/// published states and backup bundles carry the `snapshot-` prefix, so the
/// name sorts the two listings; the authenticated envelope still decides what
/// a listed object is.
pub(crate) fn lists_snapshot_member(collection: Collection, name: &str) -> bool {
    match collection {
        Collection::Snapshots => name.starts_with("snapshot-"),
        Collection::Checkpoints => !name.starts_with("snapshot-"),
        _ => true,
    }
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
        if self.role == ObjectRole::Segment {
            let (writer, seq, digest) = parse_segment_object_id(&self.object_id)?;
            if segment_object_id(writer, seq, digest)? != self.object_id || digest != self.sha256 {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
        }
        if self.role == ObjectRole::Snapshot {
            let id = uuid::Uuid::parse_str(&self.object_id).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
            if id.to_string() != self.object_id { return Err(ProviderError::new(ErrorKind::Corrupt)); }
        }
        Ok(())
    }
}

pub(crate) const MAX_SEGMENT_NAME_BYTES: usize = 122;
pub(crate) const MAX_SNAPSHOT_NAME_BYTES: usize = 36;

#[cfg(test)]
#[test]
fn segment_names_keep_complete_identity_and_reject_noncanonical_names() {
    let writer = "aaaaaaaa-aaaa-4000-8000-aaaaaaaaaaaa";
    let hash = "a".repeat(64);
    let name = segment_object_id(writer, u64::MAX, &hash).unwrap();
    assert_eq!(name.len(), MAX_SEGMENT_NAME_BYTES);
    for invalid_writer in [
        "00000000-0000-1000-8000-000000000001",
        "00000000-0000-0000-0000-000000000000",
        "00000000-0000-4000-7000-000000000001",
        "00000000-0000-4000-c000-000000000001",
    ] {
        assert!(segment_object_id(invalid_writer, 1, &hash).is_err(), "{invalid_writer}");
        assert!(parse_segment_object_id(&format!("{invalid_writer}-1-{hash}")).is_err(), "{invalid_writer}");
    }

    assert_eq!(parse_segment_object_id(&name).unwrap(), (writer, u64::MAX, hash.as_str()));
    for name in [format!("{writer}-01-{hash}"), format!("{writer}-0-{hash}"),
        format!("{writer}-18446744073709551616-{hash}"), format!("{writer}-1-{}", "A".repeat(64)),
        format!("{}-1-{hash}", writer.replace('a', "A")), format!("{writer}-1-{}", "a".repeat(63))] {
        assert!(parse_segment_object_id(&name).is_err(), "{name}");
    }
}

pub(crate) fn segment_object_id(writer: &str, seq: u64, sha256: &str) -> Result<String> {
    risunest_sync_wire::stamp::validate_writer_id(writer)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    if seq == 0 || !crate::trust_boundary::is_lower_hex_256(sha256) {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(format!("{writer}-{seq}-{sha256}"))
}

pub(crate) fn parse_segment_object_id(name: &str) -> Result<(&str, u64, &str)> {
    let bad = || ProviderError::new(ErrorKind::Corrupt);
    let writer = name.get(..36).ok_or_else(bad)?;
    let suffix = name.get(37..).filter(|_| name.as_bytes().get(36) == Some(&b'-')).ok_or_else(bad)?;
    let (seq, digest) = suffix.split_once('-').ok_or_else(bad)?;
    let seq = seq.parse::<u64>().map_err(|_| bad())?;
    if segment_object_id(writer, seq, digest)? != name { return Err(bad()); }
    Ok((writer, seq, digest))
}

pub(crate) struct IdentitySink<'a> { pub intent: &'a ObjectIntent }

pub(crate) async fn verify_source(source: &dyn TransferSource, intent: &ObjectIntent, cancel: &Cancellation) -> Result<()> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;
    if source.byte_length() != intent.byte_length { return Err(ProviderError::new(ErrorKind::Corrupt)); }
    let mut reader = source.open(0, intent.byte_length, cancel).await?;
    let mut hash = Sha256::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        cancel.check()?;
        let read = reader.read(&mut buffer).await.map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if read == 0 { break; }
        length = length.checked_add(read as u64).ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        if length > intent.byte_length { return Err(ProviderError::new(ErrorKind::Corrupt)); }
        hash.update(&buffer[..read]);
    }
    let digest: String = hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    if length != intent.byte_length || digest != intent.sha256 {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}

impl TransferSink for IdentitySink<'_> {
    fn open<'a>(&'a mut self, offset: u64, max_length: u64, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Pin<Box<dyn AsyncWrite + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            if offset != 0 || max_length != self.intent.byte_length {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            Ok(Box::pin(tokio::io::sink()) as Pin<Box<dyn AsyncWrite + Send>>)
        })
    }
    fn finish<'a>(&'a mut self, length: u64, sha256: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            if length != self.intent.byte_length || sha256 != self.intent.sha256 {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            Ok(())
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<CancelState>);
#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
    external_flag: Option<Arc<AtomicBool>>,
}
impl Cancellation {
    pub(crate) fn with_external_flag(external_flag: Arc<AtomicBool>) -> Self {
        Self(Arc::new(CancelState { external_flag: Some(external_flag), ..Default::default() }))
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.notify.notify_waiters();
    }
    /// Whether both handles share one token.
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn check(&self) -> Result<()> {
        if self.0.cancelled.load(Ordering::Acquire)
            || self.0.external_flag.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire))
        {
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
            if self.0.external_flag.is_some() {
                tokio::select! {
                    _ = notified => {},
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {},
                }
            } else {
                notified.await;
            }
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
/// Capability URLs stay in the vault; S3 progress alone cannot authorize requests.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum ResumeData {
    Secret(String),
    S3Multipart(String),
}
impl From<SecretRef> for ResumeData {
    fn from(reference: SecretRef) -> Self { Self::Secret(reference.0) }
}
impl ResumeData {
    pub(crate) fn secret(&self) -> Result<SecretRef> {
        match self {
            Self::Secret(reference) if !reference.is_empty() => Ok(SecretRef(reference.clone())),
            _ => Err(ProviderError::new(ErrorKind::Corrupt)),
        }
    }
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::Secret(value) => !value.is_empty() && value.len() <= 1024,
            Self::S3Multipart(value) => !value.is_empty() && value.len() <= 4 * 1024 * 1024,
        }
    }
}
#[derive(Clone)]
pub(crate) struct ResumeState {
    pub data: ResumeData,
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
    Segments,
    /// Published states and backup bundles.
    Snapshots,
    /// Sync checkpoints. They are stored beside the snapshots, under the same
    /// folder, locator and tags, and only their listing is separate.
    Checkpoints,
    BackupPoints,
    InventoryPages,
    Descriptors,
    Leases,
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
    Delete,
    Authenticate,
}
/// One call represents one provider operation. Each SDK subrequest, session
/// control call and explicit owner retry passes the one-dispatch HTTP boundary.
pub(crate) trait Provider: Send + Sync {
    fn transfer_concurrency(&self) -> usize { 1 }
    fn progress_stage(&self, _stage: &'static str) {}
    fn preparation_progress(&self) -> std::sync::Arc<super::phase_progress::PhaseProgress> {
        super::phase_progress::PhaseProgress::silent()
    }
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
    /// Opens a resumable session (multipart, upload session, issued upload URL)
    /// when the service has one, so the owner journals the sealed state before
    /// any payload byte moves. `None` means the object is sent in one request
    /// and retried whole after a failure.
    fn begin_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ResumeState>>;
    /// With `resume`, continues from its remotely confirmed offset. Without it,
    /// a single-request upload; a retry with the same identity and bytes must
    /// converge on the same complete receipt, never overwrite different bytes.
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
    /// Overwrites the descriptor object at `locator` in one write attempt.
    /// Only the services a repository can sync through implement it.
    fn replace_descriptor<'a>(
        &'a self,
        _repository: &'a RepositoryHandle,
        _locator: &'a RemoteLocator,
        _bytes: &'a HeadBytes,
        _cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> { Box::pin(async { Err(ProviderError::new(ErrorKind::Unsupported)) }) }
    fn list_objects<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        collection: Collection,
        cursor: Option<&'a str>,
        limit: u16,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage>;
    /// Removes one object this repository owns. Idempotent: a target that is
    /// already gone answers `Ok`. The head locator and descriptor objects are
    /// refused with `Unsupported`. A success answer means the remote deletion
    /// finished; an accepted but still pending deletion is not a success.
    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()>;
    fn supports_incomplete_cleanup(&self) -> bool { false }
    /// Processes one bounded page while the caller owns cleanup protection. Mark a
    /// deletion in flight immediately before sending it and clear only after its
    /// completion is confirmed. Read-only failures must not mark a deletion.
    fn cleanup_incomplete_objects<'a>(
        &'a self,
        _repository: &'a RepositoryHandle,
        _protected_jobs: &'a [String],
        _cursor: Option<&'a str>,
        _limit: usize,
        _deleting: &'a (dyn Fn(bool) + Send + Sync),
        _cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, IncompleteCleanupPage> { Box::pin(async { Ok(IncompleteCleanupPage::default()) }) }
    fn delete_empty_container<'a>(
        &'a self,
        _repository: &'a RepositoryHandle,
        _locator: &'a RemoteLocator,
        _protected_jobs: &'a [String],
        _cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> { Box::pin(async { Ok(()) }) }
    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution>;
    /// Presence, byte length and version of an immutable object, read from
    /// service metadata without reading its body. `known` is the locator a
    /// receipt already names; without it the object is resolved from the
    /// intent. A checksum is present only when the service computed one, and
    /// is provider verified only when it equals the intent's digest. A missing
    /// object is `None`.
    fn lookup_metadata<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        known: Option<&'a RemoteLocator>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ObjectReceipt>>;
    /// The one mutable head of a repository. Head writes accept only this
    /// locator, so an ordinary object can never be replaced by a head write;
    /// a backup-only service answers `Unsupported`.
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator>;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_cause_reaches_replies_without_payload_values_and_preserves_equality() {
        let shape = serde_json::from_str::<u32>("\"private-payload-value\"").unwrap_err();
        let kept = ProviderError::new(ErrorKind::Corrupt).caused(&shape);
        let cause = kept.cause.0.clone().unwrap();
        assert!(cause.starts_with("json failure at line 1 column"), "{cause}");
        assert!(!cause.contains("private-payload-value"));
        assert_eq!(kept, ProviderError::new(ErrorKind::Corrupt));
        let reply = serde_json::to_value(&kept).unwrap();
        assert_eq!(reply["detail"], cause);
        assert!(reply.get("cause").is_none());
        let returned: ProviderError = serde_json::from_value(reply).unwrap();
        assert_eq!(returned.cause.0, Some(cause));
        let poisoned: std::sync::LockResult<()> = Err(std::sync::PoisonError::new(()));
        let error = poisoned.map_err(|error| ProviderError::new(ErrorKind::Transient).caused(&error)).unwrap_err();
        assert!(error.cause.0.unwrap().contains("poisoned"));
    }

    #[test]
    fn provider_failures_log_their_kind_and_routine_kinds_are_warnings() {
        use crate::native_log::CommandFailure;
        let io = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let error = ProviderError { http_status: Some(503), ..ProviderError::new(ErrorKind::Transient).caused(&io) };
        assert_eq!(error.code(), "transient");
        assert_eq!(error.detail().unwrap(), "permission denied; http 503");
        assert!(!error.expected());
        for kind in [ErrorKind::Cancelled, ErrorKind::RepositoryBusy, ErrorKind::PreconditionFailed, ErrorKind::RateLimited] {
            assert!(ProviderError::new(kind).expected(), "{kind:?}");
        }
        assert!(!ProviderError::new(ErrorKind::Corrupt).expected());
        assert_eq!(ProviderError::new(ErrorKind::DailyQuotaExhausted).code(), "dailyQuotaExhausted");
    }

    #[test]
    fn command_and_job_errors_keep_details_and_status_without_credentials() {
        let error = ProviderError { http_status: Some(502), ..ProviderError::new(ErrorKind::EndpointRejected)
            .caused("TLS handshake failed for https://synthetic-user:synthetic-password@storage.invalid/dav?signature=synthetic-signature#synthetic-fragment: certificate expired") };
        let reply = serde_json::to_value(&error).unwrap();
        let job = crate::external_storage::runtime::error_dto(&error);
        for value in [reply, job] {
            assert_eq!(value["httpStatus"], 502);
            let detail = value["detail"].as_str().unwrap();
            assert!(detail.contains("TLS handshake failed"));
            assert!(detail.contains("https://storage.invalid/dav"));
            for secret in ["synthetic-user", "synthetic-password", "synthetic-signature", "synthetic-fragment"] {
                assert!(!detail.contains(secret));
            }
        }
    }

    #[test]
    fn external_native_job_cancellation_reaches_checks_and_pending_control_requests() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let flag = Arc::new(AtomicBool::new(false));
            let cancel = Cancellation::with_external_flag(flag.clone());
            cancel.check().unwrap();
            let pending = super::super::leases::control_request(
                &cancel, std::future::pending::<Result<()>>(),
            );
            tokio::pin!(pending);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(1), &mut pending).await.is_err());
            flag.store(true, Ordering::Release);
            assert_eq!(cancel.check().unwrap_err().kind, ErrorKind::Cancelled);
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
                .await.unwrap().unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
            let local = Cancellation::with_external_flag(Arc::new(AtomicBool::new(false)));
            let clone = local.clone();
            local.cancel();
            assert_eq!(clone.check().unwrap_err().kind, ErrorKind::Cancelled);
        });
    }

    #[test]
    fn role_names_ignore_only_proven_foreign_members() {
        for collection in [Collection::Leases, Collection::Snapshots, Collection::BackupPoints,
            Collection::InventoryPages, Collection::Descriptors] {
            for name in [".DS_Store", "._metadata", "Thumbs.db", "desktop.ini"] {
                assert_eq!(role_member_name(collection, name), RoleMemberName::Foreign);
            }
        }
        assert_eq!(role_member_name(Collection::Leases, "readme.txt"), RoleMemberName::Foreign);
        let name = lease_object_id(LeaseKind::Work, &"a".repeat(32)).unwrap();
        assert_eq!(role_member_name(Collection::Leases, &name), RoleMemberName::Owned);
        for name in [format!("{name} (1)"), "work-broken".into(), "deleting-".into(), "cleanup-unknown".into()] {
            assert_eq!(role_member_name(Collection::Leases, &name), RoleMemberName::Ambiguous);
        }
        assert_eq!(role_member_name(Collection::BackupPoints, "readme.txt"), RoleMemberName::Ambiguous);
    }


    #[test]
    fn a_lease_name_carries_its_kind_and_tag_and_nothing_else() {
        let tag = "0123456789abcdef0123456789abcdef";
        for (kind, expected) in [
            (LeaseKind::Work, "work"),
            (LeaseKind::Cleanup, "cleanup"),
            (LeaseKind::Deleting, "deleting"),
        ] {
            let object_id = lease_object_id(kind, tag).unwrap();
            assert_eq!(object_id, format!("{expected}-{tag}"));
            assert_eq!(parse_lease_object_id(&object_id).unwrap(), (kind, tag.into()));
        }
        // One kind's name can never be read as another's.
        assert_ne!(
            lease_object_id(LeaseKind::Work, tag).unwrap(),
            lease_object_id(LeaseKind::Cleanup, tag).unwrap()
        );

        for bad in ["", "0123456789ABCDEF0123456789abcdef", &tag[1..], &format!("{tag}0")] {
            assert_eq!(
                lease_object_id(LeaseKind::Work, bad).unwrap_err().kind,
                ErrorKind::Corrupt
            );
        }
        for bad in [
            "work",
            &format!("working-{tag}"),
            &format!("work-{}", &tag[1..]),
            &format!("work-{tag}-1"),
        ] {
            assert_eq!(
                parse_lease_object_id(bad).unwrap_err().kind,
                ErrorKind::Corrupt
            );
        }
    }
}
