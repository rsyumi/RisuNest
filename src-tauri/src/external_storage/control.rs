//! Authenticated mutable head and immutable history objects.
//!
//! Provider locators are opaque. Every control body therefore uses the shared
//! RNX1 envelope and repeats the descriptor repository identity inside the
//! authenticated plaintext. Public envelope fields are only routing hints
//! until the complete body has authenticated.
use super::{
    capabilities::Capabilities,
    contract::{
        Cancellation, Collection, ErrorKind, HeadBytes, ObjectIntent, ObjectReceipt, ObjectRole,
        Provider, ProviderError, ReadReceipt, RemoteLocator, RepositoryHandle, Result,
    },
    journal::TransferJournal,
    packaging::{native_role, wire_role, RemoteObject},
    publication::{Attempt, HeadObservation, Outcome, PublicationMode, PublicationWrite},
    transfer::SpoolSink,
    transfer_job,
};
use risunest_external_storage_format::{
    content_identity::hash,
    control as wire_control,
    crypto::derive_key,
    format::{Descriptor, Strategy},
    snapshot as wire,
};
use std::{
    fs,
    io::{Cursor, Read, Write},
};

const HEAD_OBJECT_ID: &str = "head";
const MAX_CONTROL_PLAINTEXT: usize = 48 * 1024;
const MAX_POINT_PLAINTEXT: usize = wire_control::MAX_CONTROL_BYTES;
const MAX_POINT_CIPHERTEXT: u64 = 512 * 1024;
const MAX_SNAPSHOT_CIPHERTEXT: u64 = wire::MAX_METADATA_BYTES as u64 + 512 * 1024;
const MAX_DISCOVERY_PAGES: usize = 10_000;
const SNAPSHOT_DISCOVERY_PAGE_LIMIT: u16 = 100;

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .filter(|_| crate::trust_boundary::is_lower_hex_256(value))
        .ok_or_else(|| corrupt("invalid hash"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeadDocument {
    pub repository_id: String,
    pub library_id: String,
    pub commit_id: String,
    pub parent_commit_id: Option<String>,
    /// Identifies the whole published state, control changes included.
    pub content_fingerprint: String,
    pub state: RemoteObject,
}
impl HeadDocument {
    pub(crate) fn new(
        descriptor: &Descriptor,
        library_id: String,
        commit_id: String,
        parent_commit_id: Option<String>,
        content_fingerprint: String,
        state: RemoteObject,
    ) -> Result<Self> {
        let value = Self {
            repository_id: descriptor.repository_id.clone(),
            library_id,
            commit_id,
            parent_commit_id,
            content_fingerprint,
            state,
        };
        descriptor.validate().map_err(corrupt)?;
        value.check(descriptor)?;
        Ok(value)
    }
    fn check(&self, descriptor: &Descriptor) -> Result<()> {
        if self.repository_id != descriptor.repository_id
            || !crate::trust_boundary::is_lower_hex_256(&self.content_fingerprint)
            || self.state.repository_id != descriptor.repository_id
            || self.state.role != ObjectRole::SyncState
        {
            return Err(corrupt("invalid head document"));
        }
        Ok(())
    }
    fn to_wire(
        &self,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<wire_control::HeadDocument> {
        descriptor.validate().map_err(corrupt)?;
        self.check(descriptor)?;
        wire_control::HeadDocument::new(
            self.repository_id.clone(),
            self.library_id.clone(),
            self.commit_id.clone(),
            self.parent_commit_id.clone(),
            decode_hash(&self.content_fingerprint)?,
            self.state.stored(repository)?,
        )
        .map_err(corrupt)
    }
    fn from_wire(
        value: wire_control::HeadDocument,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        if value.repository_id != descriptor.repository_id {
            return Err(corrupt("head descriptor binding differs"));
        }
        Ok(Self {
            repository_id: value.repository_id,
            library_id: value.library_id,
            commit_id: value.commit_id,
            parent_commit_id: value.parent_commit_id,
            content_fingerprint: hex::encode(value.state_fingerprint),
            state: RemoteObject::from_stored(&value.state, repository)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackupPointKind {
    Automatic,
    Manual,
    Conflict,
    RecoveryCandidate,
}

impl BackupPointKind {
    fn to_wire(self) -> wire_control::BackupPointKind {
        match self {
            Self::Automatic => wire_control::BackupPointKind::Backup,
            Self::Manual => wire_control::BackupPointKind::Manual,
            Self::Conflict => wire_control::BackupPointKind::Conflict,
            Self::RecoveryCandidate => wire_control::BackupPointKind::History,
        }
    }
}

/// A point names the one remote bundle it preserves. A conflict's local side
/// remains in the native conflict store rather than being uploaded here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackupPointDocument {
    pub repository_id: String,
    pub point_id: String,
    pub kind: BackupPointKind,
    pub created_at_ms: u64,
    pub bundle: RemoteObject,
}
impl BackupPointDocument {
    pub(crate) fn single(
        descriptor: &Descriptor,
        point_id: String,
        kind: BackupPointKind,
        created_at_ms: u64,
        bundle: RemoteObject,
    ) -> Result<Self> {
        if kind == BackupPointKind::Conflict {
            return Err(corrupt("use the conflict point constructor"));
        }
        let value = Self {
            repository_id: descriptor.repository_id.clone(),
            point_id,
            kind,
            created_at_ms,
            bundle,
        };
        descriptor.validate().map_err(corrupt)?;
        value.check(descriptor)?;
        Ok(value)
    }
    pub(crate) fn conflict(
        descriptor: &Descriptor,
        point_id: String,
        created_at_ms: u64,
        remote_bundle: RemoteObject,
    ) -> Result<Self> {
        let value = Self {
            repository_id: descriptor.repository_id.clone(),
            point_id,
            kind: BackupPointKind::Conflict,
            created_at_ms,
            bundle: remote_bundle,
        };
        descriptor.validate().map_err(corrupt)?;
        value.check(descriptor)?;
        Ok(value)
    }
    pub(crate) fn bundles(&self) -> Vec<&RemoteObject> {
        vec![&self.bundle]
    }
    fn check(&self, descriptor: &Descriptor) -> Result<()> {
        if self.repository_id != descriptor.repository_id
            || self.bundle.repository_id != descriptor.repository_id
            || self.bundle.role != ObjectRole::BackupBundle
        {
            return Err(corrupt("invalid backup point"));
        }
        Ok(())
    }
    fn to_wire(
        &self,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<wire_control::BackupPointDocument> {
        descriptor.validate().map_err(corrupt)?;
        self.check(descriptor)?;
        match self.kind {
            BackupPointKind::Conflict => wire_control::BackupPointDocument::conflict(
                self.repository_id.clone(),
                self.point_id.clone(),
                self.created_at_ms,
                self.bundle.stored(repository)?,
            ),
            _ => wire_control::BackupPointDocument::single(
                self.repository_id.clone(),
                self.point_id.clone(),
                self.kind.to_wire(),
                self.created_at_ms,
                self.bundle.stored(repository)?,
            ),
        }
        .map_err(corrupt)
    }
    fn from_wire(
        value: wire_control::BackupPointDocument,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        if value.repository_id != descriptor.repository_id {
            return Err(corrupt("backup point descriptor binding differs"));
        }
        let kind = match value.kind {
            wire_control::BackupPointKind::Backup => BackupPointKind::Automatic,
            wire_control::BackupPointKind::History => BackupPointKind::RecoveryCandidate,
            wire_control::BackupPointKind::Manual => BackupPointKind::Manual,
            wire_control::BackupPointKind::Conflict => BackupPointKind::Conflict,
        };
        Ok(Self {
            repository_id: value.repository_id,
            point_id: value.point_id,
            kind,
            created_at_ms: value.created_at_ms,
            bundle: RemoteObject::from_stored(&value.bundle, repository)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedHead {
    pub document: HeadDocument,
    pub observation: HeadObservation,
}

pub(crate) struct PreparedHead {
    pub document: HeadDocument,
    pub bytes: HeadBytes,
    pub authenticated_body_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublicationResult {
    Confirmed(ObservedHead),
    Conflict(Option<ObservedHead>),
    Unknown {
        observation: Option<ObservedHead>,
        cause: Option<ErrorKind>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListedBackupPoint {
    pub document: BackupPointDocument,
    pub reference: RemoteObject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackupPointPage {
    pub points: Vec<ListedBackupPoint>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteConflictPointDeleteOutcome {
    Deleted,
    NotFound,
}

fn seal(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    object_id: &str,
    role: wire::ObjectRole,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    descriptor.validate().map_err(corrupt)?;
    let header = wire::PublicObjectHeader::new(
        descriptor.repository_id.clone(),
        object_id.into(),
        role,
        plaintext.len() as u64,
    )
    .map_err(corrupt)?;
    let key = derive_key(root_key, &descriptor.repository_id, "metadata").map_err(corrupt)?;
    let mut output = Vec::new();
    wire::seal_envelope(&mut Cursor::new(plaintext), &mut output, &key, &header)
        .map_err(corrupt)?;
    Ok(output)
}

fn open(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    expected_id: Option<&str>,
    expected_role: wire::ObjectRole,
    ciphertext: &[u8],
    max_plaintext: usize,
) -> Result<(Vec<u8>, wire::PublicObjectHeader, String, String)> {
    descriptor.validate().map_err(corrupt)?;
    let key = derive_key(root_key, &descriptor.repository_id, "metadata").map_err(corrupt)?;
    let mut plaintext = Vec::new();
    let header = wire::open_envelope(
        &mut Cursor::new(ciphertext),
        &mut plaintext,
        &key,
        max_plaintext as u64,
    )
    .map_err(corrupt)?;
    if header.repository_id != descriptor.repository_id
        || header.role != expected_role
        || expected_id.is_some_and(|id| header.object_id != id)
    {
        return Err(corrupt("control envelope binding differs"));
    }
    let plaintext_hash = hex::encode(hash(&plaintext));
    let ciphertext_hash = hex::encode(hash(ciphertext));
    Ok((plaintext, header, plaintext_hash, ciphertext_hash))
}

pub(crate) fn prepare_head(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    repository: &RepositoryHandle,
    document: HeadDocument,
) -> Result<PreparedHead> {
    let plaintext = document
        .to_wire(descriptor, repository)?
        .encode(MAX_CONTROL_PLAINTEXT)
        .map_err(corrupt)?;
    let sealed = seal(
        descriptor,
        root_key,
        HEAD_OBJECT_ID,
        wire::ObjectRole::Head,
        &plaintext,
    )?;
    let authenticated_body_hash = hex::encode(hash(&sealed));
    Ok(PreparedHead {
        document,
        bytes: HeadBytes::new(sealed)?,
        authenticated_body_hash,
    })
}

async fn download_control(
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    locator: &RemoteLocator,
    unchanged: Option<&super::contract::VersionToken>,
    max_ciphertext: u64,
    cancel: &Cancellation,
) -> Result<(ReadReceipt, Option<Vec<u8>>)> {
    let root = tempfile::tempdir().map_err(transient)?;
    let path = root.path().join("control.partial");
    let mut sink = SpoolSink::create(&path, max_ciphertext)?;
    let receipt = provider
        .read_object(repository, locator, unchanged, &mut sink, cancel)
        .await?;
    match &receipt {
        ReadReceipt::NotModified(_) => Ok((receipt, None)),
        ReadReceipt::Body(body) => {
            if !body.complete || !sink.is_verified() || body.byte_length > max_ciphertext {
                return Err(corrupt("incomplete control object"));
            }
            let mut file = crate::trust_boundary::open_regular_source(&path).map_err(corrupt)?;
            let mut bytes = Vec::with_capacity(body.byte_length as usize);
            file.read_to_end(&mut bytes).map_err(corrupt)?;
            if bytes.len() as u64 != body.byte_length {
                return Err(corrupt("control object length differs"));
            }
            Ok((receipt, Some(bytes)))
        }
    }
}

pub(crate) async fn read_head(
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    known: Option<&ObservedHead>,
    cancel: &Cancellation,
) -> Result<Option<ObservedHead>> {
    let locator = provider.head_locator(repository)?;
    let unchanged = known.and_then(|head| head.observation.version.as_ref());
    let response = download_control(
        provider,
        repository,
        &locator,
        unchanged,
        HeadBytes::MAX_BYTES as u64,
        cancel,
    )
    .await;
    let (receipt, bytes) = match response {
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    match (receipt, bytes) {
        (ReadReceipt::NotModified(version), None) => {
            let known = known
                .cloned()
                .ok_or_else(|| corrupt("not-modified without known head"))?;
            if known.observation.version.as_ref() != Some(&version) {
                return Err(corrupt("head version changed in not-modified response"));
            }
            Ok(Some(known))
        }
        (ReadReceipt::Body(receipt), Some(bytes)) => {
            let (plaintext, _, _, ciphertext_hash) = open(
                descriptor,
                root_key,
                Some(HEAD_OBJECT_ID),
                wire::ObjectRole::Head,
                &bytes,
                MAX_CONTROL_PLAINTEXT,
            )?;
            let wire = wire_control::HeadDocument::decode(&plaintext, MAX_CONTROL_PLAINTEXT)
                .map_err(corrupt)?;
            let document = HeadDocument::from_wire(wire, descriptor, repository)?;
            Ok(Some(ObservedHead {
                observation: HeadObservation {
                    commit_id: document.commit_id.clone(),
                    authenticated_body_hash: ciphertext_hash,
                    version: receipt.version,
                },
                document,
            }))
        }
        _ => Err(corrupt("invalid control download state")),
    }
}

pub(crate) async fn publish_head(
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    capabilities: &Capabilities,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    strategy: Strategy,
    expected: Option<&ObservedHead>,
    prepared: &PreparedHead,
    mode: PublicationMode,
    cancel: &Cancellation,
) -> Result<PublicationResult> {
    publish_head_guarded(
        provider,
        repository,
        capabilities,
        descriptor,
        root_key,
        strategy,
        expected,
        prepared,
        || Ok(mode),
        |_| Ok(()),
        cancel,
    )
    .await
}

/// Revalidates the native execution session after the remote pre-read and
/// immediately before the single head write. The guard also lets the caller
/// durably enter its publishing phase at that exact boundary.
pub(crate) async fn publish_head_guarded<F, G>(
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    capabilities: &Capabilities,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    strategy: Strategy,
    expected: Option<&ObservedHead>,
    prepared: &PreparedHead,
    read_session: F,
    before_write: G,
    cancel: &Cancellation,
) -> Result<PublicationResult>
where
    F: FnOnce() -> Result<PublicationMode>,
    G: FnOnce(PublicationMode) -> Result<()>,
{
    prepared.document.to_wire(descriptor, repository)?;
    let expected_observation = expected.map(|head| head.observation.clone());
    let current = read_head(provider, repository, descriptor, root_key, None, cancel).await?;
    let mut attempt = Attempt::new(
        capabilities,
        strategy,
        expected_observation,
        prepared.document.commit_id.clone(),
        prepared.authenticated_body_hash.clone(),
    )?;
    let mode = read_session()?;
    let write = match attempt.before_write(current.as_ref().map(|h| &h.observation), mode) {
        Ok(write) => write,
        Err(error) if error.kind == ErrorKind::PreconditionFailed => {
            return Ok(PublicationResult::Conflict(current));
        }
        Err(error) => return Err(error),
    };
    let locator = provider.head_locator(repository)?;
    before_write(mode)?;
    let write_result = match write {
        PublicationWrite::Cas(expected) => {
            provider
                .compare_exchange_head(repository, &locator, &expected, &prepared.bytes, cancel)
                .await
        }
        PublicationWrite::Sequential => {
            provider
                .replace_head(repository, &locator, &prepared.bytes, cancel)
                .await
        }
    };
    if let Err(error) = &write_result {
        if attempt.write_failed(error) == Outcome::Conflict {
            return Ok(PublicationResult::Conflict(None));
        }
    }
    let post = if cancel.check().is_ok() {
        read_head(provider, repository, descriptor, root_key, None, cancel)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    if attempt.observe_result(post.as_ref().map(|h| &h.observation)) == Outcome::Confirmed {
        return Ok(PublicationResult::Confirmed(
            post.expect("confirmed observation exists"),
        ));
    }
    Ok(PublicationResult::Unknown {
        observation: post,
        cause: write_result.err().map(|error| error.kind),
    })
}

pub(crate) async fn upload_backup_point(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    document: BackupPointDocument,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let wire_document = document.to_wire(descriptor, repository)?;
    let plaintext = wire_document.encode(MAX_POINT_PLAINTEXT).map_err(corrupt)?;
    upload_control_object(
        descriptor,
        root_key,
        format!("backup-point-{}", document.point_id),
        ObjectRole::BackupPoint,
        &plaintext,
        journal,
        provider,
        repository,
        cancel,
    )
    .await
}

pub(crate) async fn ensure_remote_conflict_point(
    descriptor: &Descriptor,
    conflict_id: &str,
    created_at_ms: u64,
    remote_bundle: RemoteObject,
    journal: &mut TransferJournal,
    connected: &super::connection_commands::ConnectedRepository,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let document = BackupPointDocument::conflict(
        descriptor,
        conflict_id.to_owned(),
        created_at_ms,
        remote_bundle,
    )?;
    upload_backup_point(
        descriptor,
        &connected.root_key,
        document,
        journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await
}

/// Wraps an already published library reference in its own immutable bundle so
/// a retained point names a bundle rather than a synchronized state. A state is
/// the merged result of several devices, which is why the source says so. The
/// sections it carried travel with it, or the retained point would name a
/// library without the device data that state published.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn upload_backup_bundle(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    bundle_id: String,
    source: wire_control::BundleSource,
    captured_at_ms: u64,
    library: wire::LibrarySnapshotRef,
    sections: std::collections::BTreeMap<String, wire::SectionSnapshotRef>,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let document = wire_control::BackupBundleDocument::new(
        descriptor.repository_id.clone(),
        bundle_id.clone(),
        source,
        captured_at_ms,
        None,
        None,
        None,
        library,
        sections,
    )
    .map_err(corrupt)?;
    let plaintext = document.encode(MAX_POINT_PLAINTEXT).map_err(corrupt)?;
    upload_control_object(
        descriptor,
        root_key,
        format!("snapshot-{bundle_id}"),
        ObjectRole::BackupBundle,
        &plaintext,
        journal,
        provider,
        repository,
        cancel,
    )
    .await
}

pub(crate) async fn ensure_remote_conflict_bundle(
    connected: &super::connection_commands::ConnectedRepository,
    conflict_id: &str,
    remote_commit_id: &str,
    captured_at_ms: u64,
    remote_snapshot: &RemoteObject,
    journal: &mut TransferJournal,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    if conflict_id.is_empty()
        || conflict_id.len() > 1024
        || conflict_id.contains('\0')
        || remote_commit_id.is_empty()
        || remote_commit_id.len() > 1024
        || remote_commit_id.contains('\0')
        || remote_snapshot.repository_id != connected.stored.descriptor.repository_id
    {
        return Err(corrupt("conflict remote state identity differs"));
    }
    match remote_snapshot.role {
        ObjectRole::BackupBundle => Ok(remote_snapshot.clone()),
        ObjectRole::SyncState => {
            let view = read_snapshot_document(connected, remote_snapshot, cancel).await?;
            upload_backup_bundle(
                &connected.stored.descriptor,
                &connected.root_key,
                format!("{conflict_id}-remote"),
                wire_control::BundleSource::SyncState {
                    commit_id: remote_commit_id.to_owned(),
                },
                captured_at_ms,
                view.library,
                view.sections,
                journal,
                connected.provider.as_ref(),
                &connected.handle,
                cancel,
            )
            .await
        }
        _ => Err(corrupt("conflict remote state role differs")),
    }
}

/// Republishes an already captured library reference as a synchronized state.
/// Resolving a conflict in favour of this device publishes the preserved
/// material, and a head can only point at a state.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn upload_sync_state(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    state_id: String,
    library_id: String,
    epoch: String,
    generation: risunest_sync_wire::head::Sequence,
    parent_state_id: Option<String>,
    author_writer_id: String,
    created_at_ms: u64,
    library: wire::LibrarySnapshotRef,
    sections: std::collections::BTreeMap<String, wire::SectionSnapshotRef>,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<(RemoteObject, String)> {
    let document = wire::SyncStateDocument::new(
        state_id.clone(),
        descriptor.repository_id.clone(),
        library_id,
        epoch,
        generation,
        parent_state_id,
        author_writer_id,
        created_at_ms,
        library,
        sections,
    )
    .map_err(corrupt)?;
    let fingerprint = hex::encode(document.state_fingerprint);
    let plaintext = document
        .encode(wire::MAX_METADATA_BYTES)
        .map_err(corrupt)?;
    let object = upload_control_object(
        descriptor,
        root_key,
        format!("snapshot-{state_id}"),
        ObjectRole::SyncState,
        &plaintext,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?;
    Ok((object, fingerprint))
}

#[allow(clippy::too_many_arguments)]
async fn upload_control_object(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    object_id: String,
    role: ObjectRole,
    plaintext: &[u8],
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let plaintext_sha256 = hex::encode(hash(plaintext));
    let spool = journal.spool_path(&object_id);
    if journal.record(&object_id)?.is_none() {
        let ciphertext = seal(
            descriptor,
            root_key,
            &object_id,
            wire_role(role)?,
            plaintext,
        )?;
        if ciphertext.len() as u64 > MAX_POINT_CIPHERTEXT {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&spool)
            .map_err(transient)?;
        file.write_all(&ciphertext).map_err(transient)?;
        file.sync_all().map_err(transient)?;
        drop(file);
        let intent = ObjectIntent {
            repository_id: repository.repository_id.clone(),
            job_id: journal.job_id().into(),
            object_id: object_id.clone(),
            role,
            byte_length: ciphertext.len() as u64,
            sha256: hex::encode(hash(&ciphertext)),
        };
        journal.register(&intent)?;
    }
    let record = journal
        .record(&object_id)?
        .ok_or_else(|| corrupt("missing control journal"))?;
    if record.intent.role != role {
        return Err(corrupt("history journal role differs"));
    }
    let mut recorded = crate::trust_boundary::open_regular_source(&spool).map_err(corrupt)?;
    let mut recorded_bytes = Vec::new();
    recorded.read_to_end(&mut recorded_bytes).map_err(corrupt)?;
    if recorded_bytes.len() as u64 != record.intent.byte_length
        || hex::encode(hash(&recorded_bytes)) != record.intent.sha256
    {
        return Err(corrupt("control journal bytes differ"));
    }
    let (recorded_plaintext, _, _, _) = open(
        descriptor,
        root_key,
        Some(&object_id),
        wire_role(role)?,
        &recorded_bytes,
        MAX_POINT_PLAINTEXT,
    )?;
    if recorded_plaintext != plaintext {
        return Err(corrupt("control journal content differs"));
    }
    let receipt = transfer_job::upload(journal, &object_id, provider, repository, cancel).await?;
    Ok(RemoteObject {
        repository_id: descriptor.repository_id.clone(),
        object_id,
        role,
        receipt,
        ciphertext_sha256: record.intent.sha256,
        plaintext_length: plaintext.len() as u64,
        plaintext_sha256,
    })
}

async fn open_listed_point(
    receipt: ObjectReceipt,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<ListedBackupPoint> {
    receipt.locator.validate_for(repository)?;
    if !receipt.complete || receipt.byte_length == 0 || receipt.byte_length > MAX_POINT_CIPHERTEXT {
        return Err(corrupt("invalid listed history object"));
    }
    let (_, bytes) = download_control(
        provider,
        repository,
        &receipt.locator,
        None,
        MAX_POINT_CIPHERTEXT,
        cancel,
    )
    .await?;
    let bytes = bytes.ok_or_else(|| corrupt("listed point was not downloaded"))?;
    let (plaintext, header, plaintext_sha256, ciphertext_sha256) = open(
        descriptor,
        root_key,
        None,
        wire::ObjectRole::BackupPoint,
        &bytes,
        MAX_POINT_PLAINTEXT,
    )?;
    let wire_document = wire_control::BackupPointDocument::decode(&plaintext, MAX_POINT_PLAINTEXT)
        .map_err(corrupt)?;
    let document = BackupPointDocument::from_wire(wire_document, descriptor, repository)?;
    if header.object_id != format!("backup-point-{}", document.point_id) {
        return Err(corrupt("history object identity differs"));
    }
    Ok(ListedBackupPoint {
        reference: RemoteObject {
            repository_id: descriptor.repository_id.clone(),
            object_id: header.object_id,
            role: ObjectRole::BackupPoint,
            receipt,
            ciphertext_sha256,
            plaintext_length: header.plaintext_length,
            plaintext_sha256,
        },
        document,
    })
}

pub(crate) async fn delete_authenticated_conflict_point(
    connected: &super::connection_commands::ConnectedRepository,
    conflict_id: &str,
    point: &wire::StoredObject,
    cancel: &Cancellation,
) -> Result<RemoteConflictPointDeleteOutcome> {
    if conflict_id.is_empty()
        || conflict_id.len() > 1024
        || conflict_id.contains('\0')
        || point.header.role != wire::ObjectRole::BackupPoint
        || point.header.object_id != format!("backup-point-{conflict_id}")
    {
        return Err(corrupt("conflict point identity differs"));
    }
    let expected = RemoteObject::from_stored(point, &connected.handle)?;
    let listed = match open_listed_point(
        expected.receipt.clone(),
        &connected.stored.descriptor,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await
    {
        Err(error) if error.kind == ErrorKind::NotFound => {
            return Ok(RemoteConflictPointDeleteOutcome::NotFound)
        }
        other => other?,
    };
    if listed.reference.stored(&connected.handle)? != *point {
        return Err(corrupt("conflict point bytes differ"));
    }
    connected
        .provider
        .delete_object(&connected.handle, &expected.receipt.locator, cancel)
        .await?;
    Ok(RemoteConflictPointDeleteOutcome::Deleted)
}

pub(crate) async fn list_backup_points_page(
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cursor: Option<&str>,
    limit: u16,
    cancel: &Cancellation,
) -> Result<BackupPointPage> {
    if limit == 0 || limit > 100 {
        return Err(corrupt("invalid history page limit"));
    }
    let page = provider
        .list_objects(repository, Collection::BackupPoints, cursor, limit, cancel)
        .await?;
    let mut points = Vec::with_capacity(page.objects.len());
    for receipt in page.objects {
        cancel.check()?;
        points.push(
            open_listed_point(receipt, descriptor, root_key, provider, repository, cancel).await?,
        );
    }
    Ok(BackupPointPage {
        points,
        next_cursor: page.next_cursor,
    })
}

/// Opens one enumerated state or bundle. Its identity comes from the
/// authenticated envelope, because a listing only carries routing hints.
pub(crate) async fn open_listed_snapshot(
    connected: &super::connection_commands::ConnectedRepository,
    receipt: ObjectReceipt,
    cancel: &Cancellation,
) -> Result<(RemoteObject, SnapshotView)> {
    if !receipt.complete || receipt.byte_length == 0 || receipt.byte_length > MAX_SNAPSHOT_CIPHERTEXT
    {
        return Err(corrupt("invalid listed snapshot object"));
    }
    let (read, bytes) = download_control(
        connected.provider.as_ref(),
        &connected.handle,
        &receipt.locator,
        None,
        MAX_SNAPSHOT_CIPHERTEXT,
        cancel,
    )
    .await?;
    let ReadReceipt::Body(read_receipt) = read else {
        return Err(corrupt("listed snapshot was not downloaded"));
    };
    if read_receipt.locator != receipt.locator
        || read_receipt.byte_length != receipt.byte_length
        || !read_receipt.complete
    {
        return Err(corrupt("listed snapshot receipt differs"));
    }
    let bytes = bytes.ok_or_else(|| corrupt("listed snapshot was not downloaded"))?;
    let repository_id = &connected.stored.descriptor.repository_id;
    let key = derive_key(&connected.root_key, repository_id, "metadata").map_err(corrupt)?;
    let mut plaintext = Vec::new();
    let header = wire::open_envelope(
        &mut Cursor::new(&bytes),
        &mut plaintext,
        &key,
        wire::MAX_METADATA_BYTES as u64,
    )
    .map_err(corrupt)?;
    if header.repository_id != *repository_id {
        return Err(corrupt("snapshot discovery envelope differs"));
    }
    let view = SnapshotView::read(&plaintext, header.role, repository_id)?;
    if header.object_id != format!("snapshot-{}", view.snapshot_id) {
        return Err(corrupt("snapshot discovery document differs"));
    }
    Ok((
        RemoteObject {
            repository_id: repository_id.clone(),
            object_id: header.object_id,
            role: native_role(header.role)?,
            receipt: read_receipt,
            ciphertext_sha256: hex::encode(hash(&bytes)),
            plaintext_length: header.plaintext_length,
            plaintext_sha256: hex::encode(hash(&plaintext)),
        },
        view,
    ))
}

/// Everything one catalog node names: the nodes below it and the packs it
/// holds. A restore only needs the packs, but nothing enumerates an
/// intermediate node, so a cleanup has to see both.
pub(crate) async fn read_catalog_children(
    connected: &super::connection_commands::ConnectedRepository,
    node: &RemoteObject,
    cancel: &Cancellation,
) -> Result<Vec<RemoteObject>> {
    let repository_id = &connected.stored.descriptor.repository_id;
    if node.role != ObjectRole::Catalog || node.repository_id != *repository_id {
        return Err(corrupt("invalid catalog node"));
    }
    node.stored(&connected.handle)?;
    let (_, bytes) = download_control(
        connected.provider.as_ref(),
        &connected.handle,
        &node.receipt.locator,
        None,
        MAX_SNAPSHOT_CIPHERTEXT,
        cancel,
    )
    .await?;
    let bytes = bytes.ok_or_else(|| corrupt("catalog node was not downloaded"))?;
    let (plaintext, _, plaintext_sha256, ciphertext_sha256) = open(
        &connected.stored.descriptor,
        &connected.root_key,
        Some(&node.object_id),
        wire::ObjectRole::Catalog,
        &bytes,
        wire::MAX_METADATA_BYTES,
    )?;
    if plaintext_sha256 != node.plaintext_sha256 || ciphertext_sha256 != node.ciphertext_sha256 {
        return Err(corrupt("catalog node differs"));
    }
    let document = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES)
        .map_err(corrupt)?;
    let mut objects = Vec::new();
    for child in &document.children {
        objects.push(RemoteObject::from_stored(&child.object, &connected.handle)?);
    }
    for pack in &document.packs {
        objects.push(RemoteObject::from_stored(pack, &connected.handle)?);
    }
    Ok(objects)
}

async fn scan_snapshot(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot_id: &str,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let expected_object_id = format!("snapshot-{snapshot_id}");
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    let mut matched: Option<RemoteObject> = None;
    for _ in 0..MAX_DISCOVERY_PAGES {
        cancel.check()?;
        let page = connected
            .provider
            .list_objects(
                &connected.handle,
                Collection::Snapshots,
                cursor.as_deref(),
                SNAPSHOT_DISCOVERY_PAGE_LIMIT,
                cancel,
            )
            .await?;
        if page.objects.len() > SNAPSHOT_DISCOVERY_PAGE_LIMIT as usize {
            return Err(corrupt("snapshot discovery page exceeds limit"));
        }
        for receipt in page.objects {
            cancel.check()?;
            let (object, view) = open_listed_snapshot(connected, receipt, cancel).await?;
            if view.snapshot_id == snapshot_id {
                if object.object_id != expected_object_id {
                    return Err(corrupt("snapshot selection identity differs"));
                }
                if let Some(previous) = &matched {
                    if previous.ciphertext_sha256 != object.ciphertext_sha256
                        || previous.plaintext_sha256 != object.plaintext_sha256
                        || previous.receipt.byte_length != object.receipt.byte_length
                    {
                        return Err(corrupt("ambiguous snapshot discovery"));
                    }
                } else {
                    matched = Some(object);
                }
            }
        }
        let Some(next) = page.next_cursor else {
            return matched.ok_or_else(|| ProviderError::new(ErrorKind::NotFound));
        };
        if next.is_empty()
            || next.len() > 4096
            || next.chars().any(char::is_control)
            || !seen_cursors.insert(next.clone())
        {
            return Err(corrupt("invalid snapshot discovery cursor"));
        }
        cursor = Some(next);
    }
    Err(corrupt("snapshot discovery page limit"))
}

fn same_snapshot_identity(left: &RemoteObject, right: &RemoteObject) -> bool {
    left.repository_id == right.repository_id
        && left.object_id == right.object_id
        && left.role == right.role
        && left.receipt.byte_length == right.receipt.byte_length
        && left.receipt.complete == right.receipt.complete
        && left.ciphertext_sha256 == right.ciphertext_sha256
        && left.plaintext_length == right.plaintext_length
        && left.plaintext_sha256 == right.plaintext_sha256
}

/// Reads an authenticated locator first. Only an explicit NotFound means the
/// provider may be scanned to recover an object's current opaque locator.
pub(crate) async fn find_snapshot_with_locator(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot_id: &str,
    known: Option<&RemoteObject>,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    find_snapshot_with_locator_invalidation(connected, snapshot_id, known, || Ok(()), cancel).await
}

pub(crate) async fn find_snapshot_with_locator_invalidation<F>(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot_id: &str,
    known: Option<&RemoteObject>,
    invalidate_known: F,
    cancel: &Cancellation,
) -> Result<RemoteObject>
where
    F: FnOnce() -> Result<()>,
{
    if snapshot_id.is_empty() || snapshot_id.len() > 1024 || snapshot_id.contains('\0') {
        return Err(corrupt("invalid snapshot selection"));
    }
    if let Some(snapshot) = known {
        match open_known_snapshot_document(connected, snapshot, cancel).await {
            Ok((object, view)) => {
                if view.snapshot_id != snapshot_id {
                    return Err(corrupt("snapshot selection identity differs"));
                }
                return Ok(object);
            }
            Err(error) if error.kind == ErrorKind::NotFound => invalidate_known()?,
            Err(error) => return Err(error),
        }
    }
    let recovered = scan_snapshot(connected, snapshot_id, cancel).await?;
    if known.is_some_and(|expected| !same_snapshot_identity(expected, &recovered)) {
        return Err(corrupt("relocated snapshot identity differs"));
    }
    Ok(recovered)
}

/// ID-only recovery for callers that do not yet hold an authenticated object
/// reference. Callers with a stored locator use `find_snapshot_with_locator`.
pub(crate) async fn find_snapshot(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot_id: &str,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    find_snapshot_with_locator(connected, snapshot_id, None, cancel).await
}

/// What a listing or a restore needs from either published document. The
/// authenticated envelope role, not the object name, decides which one was
/// opened, so a bundle can never be read back as a state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotView {
    pub snapshot_id: String,
    /// The state this one replaced, when the document names one. A bundle
    /// wrapped around a capture has no lineage of its own.
    pub parent_snapshot_id: Option<String>,
    pub library_id: String,
    pub created_at_ms: u64,
    /// The local library revision for a bundle, or the remote commit order for
    /// a published state. These are different counters and are never mixed.
    pub revision: String,
    pub library: wire::LibrarySnapshotRef,
    pub sections: std::collections::BTreeMap<String, wire::SectionSnapshotRef>,
    pub is_state: bool,
    /// The device whose own values these sections are, when there is one. A
    /// published state and a bundle wrapped around one hold merged material
    /// instead, so neither names a device.
    pub captured_by_device: Option<String>,
}

impl SnapshotView {
    pub(crate) fn read(plaintext: &[u8], role: wire::ObjectRole, repository_id: &str) -> Result<Self> {
        let view = match role {
            wire::ObjectRole::SyncState => {
                let document = wire::SyncStateDocument::decode(plaintext, wire::MAX_METADATA_BYTES)
                    .map_err(corrupt)?;
                Self {
                    snapshot_id: document.state_id,
                    parent_snapshot_id: document.parent_state_id,
                    library_id: document.library_id,
                    created_at_ms: document.created_at_ms,
                    revision: document.generation.as_str().to_owned(),
                    library: document.library,
                    sections: document.sections,
                    is_state: true,
                    captured_by_device: None,
                }
            }
            wire::ObjectRole::BackupBundle => {
                let document =
                    wire_control::BackupBundleDocument::decode(plaintext, MAX_POINT_PLAINTEXT)
                        .map_err(corrupt)?;
                Self {
                    snapshot_id: document.bundle_id,
                    parent_snapshot_id: None,
                    library_id: repository_id.to_owned(),
                    created_at_ms: document.captured_at_ms,
                    revision: document
                        .local_library_revision
                        .as_ref()
                        .map(|value| value.as_str().to_owned())
                        .unwrap_or_else(|| "0".into()),
                    captured_by_device: match &document.source {
                        wire_control::BundleSource::Device { writer_id } => {
                            Some(writer_id.clone())
                        }
                        wire_control::BundleSource::SyncState { .. } => None,
                    },
                    library: document.library,
                    sections: document.sections,
                    is_state: false,
                }
            }
            _ => return Err(corrupt("unsupported snapshot document role")),
        };
        if view.library.record_catalog.header.repository_id != repository_id
            || view.library.asset_catalog.header.repository_id != repository_id
        {
            return Err(corrupt("snapshot document binding differs"));
        }
        Ok(view)
    }
}

async fn open_known_snapshot_document(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot: &RemoteObject,
    cancel: &Cancellation,
) -> Result<(RemoteObject, SnapshotView)> {
    if !matches!(
        snapshot.role,
        ObjectRole::SyncState | ObjectRole::BackupBundle
    ) || snapshot.repository_id != connected.stored.descriptor.repository_id
        || snapshot.receipt.byte_length == 0
        || snapshot.receipt.byte_length > MAX_SNAPSHOT_CIPHERTEXT
    {
        return Err(corrupt("invalid selected snapshot"));
    }
    snapshot.stored(&connected.handle)?;
    let (read, bytes) = download_control(
        connected.provider.as_ref(),
        &connected.handle,
        &snapshot.receipt.locator,
        None,
        snapshot.receipt.byte_length,
        cancel,
    )
    .await?;
    let ReadReceipt::Body(receipt) = read else {
        return Err(corrupt("selected snapshot was not downloaded"));
    };
    if receipt.locator != snapshot.receipt.locator
        || receipt.byte_length != snapshot.receipt.byte_length
        || !receipt.complete
    {
        return Err(corrupt("selected snapshot receipt differs"));
    }
    let bytes = bytes.ok_or_else(|| corrupt("selected snapshot was not downloaded"))?;
    let ciphertext_sha256 = hex::encode(hash(&bytes));
    if bytes.len() as u64 != snapshot.receipt.byte_length
        || ciphertext_sha256 != snapshot.ciphertext_sha256
    {
        return Err(corrupt("snapshot ciphertext differs"));
    }
    let key = derive_key(
        &connected.root_key,
        &connected.stored.descriptor.repository_id,
        "metadata",
    )
    .map_err(corrupt)?;
    let mut plaintext = Vec::new();
    let header = wire::open_envelope(
        &mut Cursor::new(&bytes),
        &mut plaintext,
        &key,
        wire::MAX_METADATA_BYTES as u64,
    )
    .map_err(corrupt)?;
    let plaintext_sha256 = hex::encode(hash(&plaintext));
    if header != snapshot.stored(&connected.handle)?.header
        || plaintext.len() as u64 != snapshot.plaintext_length
        || plaintext_sha256 != snapshot.plaintext_sha256
    {
        return Err(corrupt("snapshot plaintext differs"));
    }
    let view = SnapshotView::read(
        &plaintext,
        header.role,
        &connected.stored.descriptor.repository_id,
    )?;
    if snapshot.object_id != format!("snapshot-{}", view.snapshot_id) {
        return Err(corrupt("snapshot document binding differs"));
    }
    Ok((
        RemoteObject {
            repository_id: header.repository_id,
            object_id: header.object_id,
            role: native_role(header.role)?,
            receipt,
            ciphertext_sha256,
            plaintext_length: header.plaintext_length,
            plaintext_sha256,
        },
        view,
    ))
}

/// Reads a selected snapshot root through its authenticated direct locator.
/// This verifies the remote object's ciphertext, RNX1 header, plaintext hash,
/// repository and scope. Referenced packs remain unverified until restore or a
/// dedicated full-history verification downloads them.
pub(crate) async fn read_snapshot_document(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot: &RemoteObject,
    cancel: &Cancellation,
) -> Result<SnapshotView> {
    open_known_snapshot_document(connected, snapshot, cancel)
        .await
        .map(|(_, view)| view)
}

pub(crate) async fn list_connected_backup_points_page(
    connected: &super::connection_commands::ConnectedRepository,
    cursor: Option<&str>,
    limit: u16,
    cancel: &Cancellation,
) -> Result<BackupPointPage> {
    list_backup_points_page(
        &connected.stored.descriptor,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cursor,
        limit,
        cancel,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{fake, journal::JobIdentity};
    use crate::persistent_store::sync_selection::CaptureIdentity;
    use std::{collections::BTreeMap, sync::Arc};

    fn descriptor(strategy: Strategy) -> Descriptor {
        Descriptor::new("descriptor-repository".into(), Some(strategy),
        )
        .unwrap()
    }
    fn snapshot(repository: &RepositoryHandle, id: &str) -> RemoteObject {
        let descriptor_id = "descriptor-repository".to_owned();
        let header = wire::PublicObjectHeader::new(
            descriptor_id.clone(),
            format!("snapshot-{id}"),
            wire::ObjectRole::SyncState,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: descriptor_id,
            object_id: header.object_id.clone(),
            role: ObjectRole::SyncState,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: repository.connection_identity.clone(),
                    collection: None,
                    object: format!("opaque-{id}"),
                },
                byte_length: wire::envelope_length(&header).unwrap(),
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: 4,
            plaintext_sha256: "22".repeat(32),
        }
    }

    fn bundle(repository: &RepositoryHandle, id: &str) -> RemoteObject {
        let descriptor_id = "descriptor-repository".to_owned();
        let header = wire::PublicObjectHeader::new(
            descriptor_id.clone(),
            format!("snapshot-{id}"),
            wire::ObjectRole::BackupBundle,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: descriptor_id,
            object_id: header.object_id.clone(),
            role: ObjectRole::BackupBundle,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: repository.connection_identity.clone(),
                    collection: None,
                    object: format!("opaque-{id}"),
                },
                byte_length: wire::envelope_length(&header).unwrap(),
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: 4,
            plaintext_sha256: "22".repeat(32),
        }
    }
    fn head(strategy: Strategy, repository: &RepositoryHandle, commit: &str) -> HeadDocument {
        HeadDocument::new(
            &descriptor(strategy),
            "library".into(),
            commit.into(),
            None,
            "33".repeat(32),
            snapshot(repository, "s1"),
        )
        .unwrap()
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn connected(
        provider: Arc<fake::FakeProvider>,
    ) -> super::super::connection_commands::ConnectedRepository {
        let test = fake::loopback_dependencies(fake::MemoryVault::default(), 1_000);
        super::super::connection_commands::ConnectedRepository {
            stored: super::super::connection_store::StoredConnection {
                id: "connection".into(),
                config: super::super::contract::ConnectionConfig {
                    provider: "synthetic".into(),
                    profile: None,
                    endpoint: "https://synthetic.invalid".into(),
                    account_id: "account".into(),
                    location: BTreeMap::new(),
                    oauth_profile: None,
                },
                descriptor: descriptor(Strategy::Cas),
                descriptor_locator: RemoteLocator {
                    connection_identity: fake::repository().connection_identity,
                    collection: None,
                    object: "descriptor".into(),
                },
                provider_repository_id: fake::repository().repository_id,
                credential_ref: "credential".into(),
                root_key_ref: "key".into(),
                capture_policy: None,
                retention_policy: None,
                capabilities: fake::capabilities(true),
                created_at_ms: 1_000,
                last_sync_at_ms: None,
                last_backup_at_ms: None,
            },
            provider,
            handle: fake::repository(),
            dependencies: test.dependencies,
            root_key: zeroize::Zeroizing::new([7; 32]),
        }
    }

    fn catalog(repository: &RepositoryHandle, id: &str) -> wire::StoredObject {
        let header = wire::PublicObjectHeader::new(
            "descriptor-repository".into(),
            id.into(),
            wire::ObjectRole::Catalog,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: header.repository_id.clone(),
            object_id: header.object_id.clone(),
            role: ObjectRole::Catalog,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: repository.connection_identity.clone(),
                    collection: None,
                    object: format!("opaque-{id}"),
                },
                byte_length: wire::envelope_length(&header).unwrap(),
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: 4,
            plaintext_sha256: "22".repeat(32),
        }
        .stored(repository)
        .unwrap()
    }

    fn snapshot_fixture(
        connected: &super::super::connection_commands::ConnectedRepository,
        snapshot_id: &str,
        locator: &str,
        created_at_ms: u64,
    ) -> (RemoteObject, Vec<u8>) {
        let plaintext = wire_control::BackupBundleDocument::new(
            connected.stored.descriptor.repository_id.clone(),
            snapshot_id.into(),
            wire_control::BundleSource::Device {
                writer_id: "writer".into(),
            },
            created_at_ms,
            Some(risunest_sync_wire::head::Sequence::from(1u64)),
            Some(risunest_sync_wire::head::Sequence::from(1u64)),
            None,
            wire::LibrarySnapshotRef {
                record_catalog: catalog(&connected.handle, "records"),
                asset_catalog: catalog(&connected.handle, "assets"),
                content_fingerprint: [3; 32],
            },
            BTreeMap::new(),
        )
        .unwrap()
        .encode(MAX_POINT_PLAINTEXT)
        .unwrap();
        let object_id = format!("snapshot-{snapshot_id}");
        let bytes = seal(
            &connected.stored.descriptor,
            &connected.root_key,
            &object_id,
            wire::ObjectRole::BackupBundle,
            &plaintext,
        )
        .unwrap();
        (
            RemoteObject {
                repository_id: connected.stored.descriptor.repository_id.clone(),
                object_id,
                role: ObjectRole::BackupBundle,
                receipt: ObjectReceipt {
                    locator: RemoteLocator {
                        connection_identity: connected.handle.connection_identity.clone(),
                        collection: None,
                        object: locator.into(),
                    },
                    byte_length: bytes.len() as u64,
                    version: None,
                    checksum: None,
                    complete: true,
                },
                ciphertext_sha256: hex::encode(hash(&bytes)),
                plaintext_length: plaintext.len() as u64,
                plaintext_sha256: hex::encode(hash(&plaintext)),
            },
            bytes,
        )
    }

    #[test]
    fn head_uses_descriptor_identity_and_authenticates_the_exact_ciphertext() {
        let repository = fake::repository();
        let descriptor = descriptor(Strategy::Cas);
        let prepared = prepare_head(
            &descriptor,
            &[7; 32],
            &repository,
            head(Strategy::Cas, &repository, "c1"),
        )
        .unwrap();
        assert_eq!(
            prepared.authenticated_body_hash,
            hex::encode(hash(prepared.bytes.as_bytes()))
        );
        let (plaintext, header, _, ciphertext_hash) = open(
            &descriptor,
            &[7; 32],
            Some(HEAD_OBJECT_ID),
            wire::ObjectRole::Head,
            prepared.bytes.as_bytes(),
            MAX_CONTROL_PLAINTEXT,
        )
        .unwrap();
        let opened = wire_control::HeadDocument::decode(&plaintext, MAX_CONTROL_PLAINTEXT).unwrap();
        assert_eq!(opened.commit_id, "c1");
        assert_eq!(header.repository_id, descriptor.repository_id);
        assert_eq!(ciphertext_hash, prepared.authenticated_body_hash);

        let mut tampered = prepared.bytes.as_bytes().to_vec();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open(
            &descriptor,
            &[7; 32],
            Some(HEAD_OBJECT_ID),
            wire::ObjectRole::Head,
            &tampered,
            MAX_CONTROL_PLAINTEXT
        )
        .is_err());
    }

    #[test]
    fn cas_publication_rechecks_and_lost_response_is_confirmed_by_authenticated_read() {
        runtime().block_on(async {
            let provider = fake::FakeProvider::new(true);
            let repository = fake::repository();
            let descriptor = descriptor(Strategy::Cas);
            let first = prepare_head(&descriptor, &[9; 32], &repository, head(Strategy::Cas, &repository, "c1")).unwrap();
            provider.state.lock().unwrap().lose_response = true;
            let result = publish_head(
                &provider, &repository, &fake::capabilities(true), &descriptor, &[9; 32],
                Strategy::Cas, None, &first, PublicationMode::Foreground, &Cancellation::default(),
            ).await.unwrap();
            assert!(matches!(result, PublicationResult::Confirmed(ref observed) if observed.document.commit_id == "c1"));

            let observed = read_head(&provider, &repository, &descriptor, &[9; 32], None, &Cancellation::default()).await.unwrap().unwrap();
            let second = prepare_head(&descriptor, &[9; 32], &repository, head(Strategy::Cas, &repository, "c2")).unwrap();
            let mut stale = observed.clone();
            stale.observation.authenticated_body_hash = "44".repeat(32);
            let conflict = publish_head(
                &provider, &repository, &fake::capabilities(true), &descriptor, &[9; 32],
                Strategy::Cas, Some(&stale), &second, PublicationMode::Foreground, &Cancellation::default(),
            ).await.unwrap();
            assert!(matches!(conflict, PublicationResult::Conflict(Some(_))));
            assert_eq!(read_head(&provider, &repository, &descriptor, &[9; 32], None, &Cancellation::default()).await.unwrap().unwrap().document.commit_id, "c1");
        });
    }

    #[test]
    fn sequential_head_revalidates_publication_mode_after_remote_pre_read() {
        runtime().block_on(async {
            let provider = fake::FakeProvider::new(false);
            let repository = fake::repository();
            let descriptor = descriptor(Strategy::Sequential);
            let prepared = prepare_head(
                &descriptor,
                &[8; 32],
                &repository,
                head(Strategy::Sequential, &repository, "c1"),
            )
            .unwrap();
            let entered = std::sync::atomic::AtomicBool::new(false);
            let error = publish_head_guarded(
                &provider,
                &repository,
                &fake::capabilities(false),
                &descriptor,
                &[8; 32],
                Strategy::Sequential,
                None,
                &prepared,
                || Err(ProviderError::new(ErrorKind::Cancelled)),
                |_| {
                    entered.store(true, std::sync::atomic::Ordering::Release);
                    Ok(())
                },
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
            assert!(!entered.load(std::sync::atomic::Ordering::Acquire));
            assert!(provider.state.lock().unwrap().objects.is_empty());
        });
    }

    #[test]
    fn locator_hit_reads_only_the_target_among_one_thousand_snapshots() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            for index in 0..999 {
                provider.seed(
                    &format!("noise-{index:04}"),
                    ObjectRole::BackupBundle,
                    vec![index as u8],
                );
            }
            let (known, bytes) = snapshot_fixture(&connected, "target", "opaque-target", 1);
            provider.seed("opaque-target", ObjectRole::BackupBundle, bytes);

            let found = find_snapshot_with_locator(
                &connected,
                "target",
                Some(&known),
                &Cancellation::default(),
            )
            .await
            .unwrap();

            assert_eq!(found.object_id, "snapshot-target");
            assert_eq!(provider.read_attempts("opaque-target"), 1);
            assert_eq!(provider.listing_count(), 0);
        });
    }

    #[test]
    fn locator_hash_mismatch_fails_without_fallback_listing() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            let (mut known, bytes) = snapshot_fixture(&connected, "target", "opaque-target", 1);
            provider.seed("opaque-target", ObjectRole::BackupBundle, bytes);
            known.ciphertext_sha256 = "00".repeat(32);

            let error = find_snapshot_with_locator(
                &connected,
                "target",
                Some(&known),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();

            assert_eq!(error.kind, ErrorKind::Corrupt);
            assert_eq!(provider.read_attempts("opaque-target"), 1);
            assert_eq!(provider.listing_count(), 0);
        });
    }

    #[test]
    fn locator_authentication_and_permission_errors_do_not_fallback() {
        runtime().block_on(async {
            for kind in [ErrorKind::Unauthorized, ErrorKind::ReauthRequired] {
                let provider = Arc::new(fake::FakeProvider::new(false));
                let connected = connected(provider.clone());
                let (known, bytes) = snapshot_fixture(&connected, "target", "opaque-target", 1);
                provider.seed("opaque-target", ObjectRole::BackupBundle, bytes);
                provider.fail_read("opaque-target", kind);
                let invalidated = std::sync::atomic::AtomicBool::new(false);

                let error = find_snapshot_with_locator_invalidation(
                    &connected,
                    "target",
                    Some(&known),
                    || {
                        invalidated.store(true, std::sync::atomic::Ordering::Release);
                        Ok(())
                    },
                    &Cancellation::default(),
                )
                .await
                .unwrap_err();

                assert_eq!(error.kind, kind);
                assert!(!invalidated.load(std::sync::atomic::Ordering::Acquire));
                assert_eq!(provider.listing_count(), 0);
            }
        });
    }

    #[test]
    fn confirmed_locator_not_found_falls_back_to_authenticated_scan() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            let (mut known, bytes) = snapshot_fixture(&connected, "target", "current-target", 1);
            provider.seed("current-target", ObjectRole::BackupBundle, bytes);
            known.receipt.locator.object = "stale-target".into();
            let invalidated = std::sync::atomic::AtomicBool::new(false);

            let found = find_snapshot_with_locator_invalidation(
                &connected,
                "target",
                Some(&known),
                || {
                    invalidated.store(true, std::sync::atomic::Ordering::Release);
                    Ok(())
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();

            assert!(invalidated.load(std::sync::atomic::Ordering::Acquire));
            assert_eq!(found.receipt.locator.object, "current-target");
            assert_eq!(provider.read_attempts("stale-target"), 1);
            assert_eq!(provider.listing_count(), 1);
        });
    }

    #[test]
    fn scan_rejects_ambiguous_authenticated_snapshot_matches() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            let (_, first) = snapshot_fixture(&connected, "duplicate", "first", 1);
            let (_, second) = snapshot_fixture(&connected, "duplicate", "second", 2);
            provider.seed("first", ObjectRole::BackupBundle, first);
            provider.seed("second", ObjectRole::BackupBundle, second);

            let error = find_snapshot(&connected, "duplicate", &Cancellation::default())
                .await
                .unwrap_err();

            assert_eq!(error.kind, ErrorKind::Corrupt);
            assert_eq!(provider.listing_count(), 1);
        });
    }

    #[test]
    fn relocated_snapshot_with_the_same_id_but_different_bytes_is_rejected() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            let (mut known, _) = snapshot_fixture(&connected, "target", "stale-target", 1);
            let (_, changed) = snapshot_fixture(&connected, "target", "current-target", 2);
            provider.seed("current-target", ObjectRole::BackupBundle, changed);
            known.receipt.locator.object = "stale-target".into();

            let error = find_snapshot_with_locator(
                &connected,
                "target",
                Some(&known),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();

            assert_eq!(error.kind, ErrorKind::Corrupt);
            assert_eq!(provider.read_attempts("stale-target"), 1);
            assert_eq!(provider.listing_count(), 1);
        });
    }

    #[test]
    fn scan_rejects_a_repeated_discovery_cursor() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            for _ in 0..2 {
                provider.script_page(
                    Collection::Snapshots,
                    Ok(super::super::contract::ObjectPage {
                        objects: Vec::new(),
                        next_cursor: Some("repeat".into()),
                    }),
                );
            }

            let error = find_snapshot(&connected, "missing", &Cancellation::default())
                .await
                .unwrap_err();

            assert_eq!(error.kind, ErrorKind::Corrupt);
            assert_eq!(provider.listing_count(), 2);
        });
    }

    #[test]
    fn conflict_point_keeps_one_remote_bundle_and_uploads_once_at_its_fixed_id() {
        runtime().block_on(async {
            let provider = fake::FakeProvider::new(false);
            let repository = fake::repository();
            let descriptor = descriptor(Strategy::Sequential);
            assert!(BackupPointDocument::conflict(
                &descriptor,
                "conflict-1".into(),
                1,
                snapshot(&repository, "s1"),
            )
            .is_err());
            let document = BackupPointDocument::conflict(
                &descriptor,
                "conflict-1".into(),
                1,
                bundle(&repository, "s2"),
            )
            .unwrap();
            assert_eq!(document.bundles().len(), 1);
            let root = tempfile::tempdir().unwrap();
            let identity = JobIdentity {
                job_id: "job".into(),
                connection_id: "connection".into(),
                repository_id: repository.repository_id.clone(),
                capture_id: "capture".into(),
                capture: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "epoch".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
            };
            let mut journal = TransferJournal::open(root.path(), identity).unwrap();
            let uploaded = upload_backup_point(
                &descriptor,
                &[6; 32],
                document.clone(),
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(uploaded.repository_id, descriptor.repository_id);
            assert_eq!(uploaded.role, ObjectRole::BackupPoint);
            assert_eq!(uploaded.object_id, "backup-point-conflict-1");
            let repeated = upload_backup_point(
                &descriptor,
                &[6; 32],
                document,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(repeated.object_id, uploaded.object_id);
            assert_eq!(provider.state.lock().unwrap().objects.len(), 1);
            assert_eq!(provider.upload_attempts("backup-point-conflict-1"), 1);
        });
    }

    #[test]
    fn conflict_point_delete_distinguishes_authenticated_presence_from_absence() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider);
            let root = tempfile::tempdir().unwrap();
            let identity = JobIdentity {
                job_id: "job".into(),
                connection_id: "connection".into(),
                repository_id: connected.handle.repository_id.clone(),
                capture_id: "capture".into(),
                capture: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "epoch".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
            };
            let mut journal = TransferJournal::open(root.path(), identity).unwrap();
            let uploaded = ensure_remote_conflict_point(
                &connected.stored.descriptor,
                "conflict",
                1,
                bundle(&connected.handle, "remote"),
                &mut journal,
                &connected,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let stored = uploaded.stored(&connected.handle).unwrap();

            assert_eq!(
                delete_authenticated_conflict_point(
                    &connected,
                    "conflict",
                    &stored,
                    &Cancellation::default(),
                )
                .await
                .unwrap(),
                RemoteConflictPointDeleteOutcome::Deleted
            );
            assert_eq!(
                delete_authenticated_conflict_point(
                    &connected,
                    "conflict",
                    &stored,
                    &Cancellation::default(),
                )
                .await
                .unwrap(),
                RemoteConflictPointDeleteOutcome::NotFound
            );
        });
    }

    #[test]
    fn sync_state_conflict_wrapper_uploads_only_small_metadata() {
        runtime().block_on(async {
            let provider = Arc::new(fake::FakeProvider::new(false));
            let connected = connected(provider.clone());
            let root = tempfile::tempdir().unwrap();
            let identity = JobIdentity {
                job_id: "job".into(),
                connection_id: "connection".into(),
                repository_id: connected.handle.repository_id.clone(),
                capture_id: "capture".into(),
                capture: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "epoch".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
            };
            let mut journal = TransferJournal::open(root.path(), identity).unwrap();
            let library = wire::LibrarySnapshotRef {
                record_catalog: catalog(&connected.handle, "records"),
                asset_catalog: catalog(&connected.handle, "assets"),
                content_fingerprint: [3; 32],
            };
            let (state, _) = upload_sync_state(
                &connected.stored.descriptor,
                &connected.root_key,
                "remote-state".into(),
                "library".into(),
                "epoch".into(),
                risunest_sync_wire::head::Sequence::from(1u64),
                None,
                "writer".into(),
                1,
                library,
                BTreeMap::new(),
                &mut journal,
                connected.provider.as_ref(),
                &connected.handle,
                &Cancellation::default(),
            )
            .await
            .unwrap();

            let bundle = ensure_remote_conflict_bundle(
                &connected,
                "conflict",
                "remote-commit",
                2,
                &state,
                &mut journal,
                &Cancellation::default(),
            )
            .await
            .unwrap();

            assert_eq!(bundle.role, ObjectRole::BackupBundle);
            assert_eq!(bundle.object_id, "snapshot-conflict-remote");
            assert_eq!(provider.upload_attempts("snapshot-remote-state"), 1);
            assert_eq!(provider.upload_attempts("snapshot-conflict-remote"), 1);
            assert_eq!(provider.state.lock().unwrap().objects.len(), 2);

            let repeated = ensure_remote_conflict_bundle(
                &connected,
                "conflict",
                "remote-commit",
                2,
                &state,
                &mut journal,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(repeated.object_id, bundle.object_id);
            assert_eq!(provider.upload_attempts("snapshot-conflict-remote"), 1);
            assert_eq!(
                ensure_remote_conflict_bundle(
                    &connected,
                    "conflict",
                    "different-commit",
                    2,
                    &state,
                    &mut journal,
                    &Cancellation::default(),
                )
                .await
                .unwrap_err()
                .kind,
                ErrorKind::Corrupt
            );
            assert_eq!(provider.upload_attempts("snapshot-conflict-remote"), 1);
        });
    }
}
