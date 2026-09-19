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
    SyncState,
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
    /// Resolve an immutable object with repository credentials without opening,
    /// resuming or deleting an upload session.
    fn lookup_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async move {
            match self
                .reconcile_upload(repository, intent, None, cancel)
                .await?
            {
                UploadResolution::Complete(receipt) => Ok(Some(receipt)),
                UploadResolution::RestartRequired => Ok(None),
                UploadResolution::Conflict => {
                    Err(ProviderError::new(ErrorKind::PreconditionFailed))
                }
                UploadResolution::Resumable(_) => Err(ProviderError::new(ErrorKind::Corrupt)),
            }
        })
    }
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
