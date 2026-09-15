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
    packaging::RemoteObject,
    publication::{Attempt, ExecutionSession, HeadObservation, Outcome, PublicationWrite},
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
    pub scope_id: String,
    pub content_fingerprint: String,
    pub snapshot: RemoteObject,
}
impl HeadDocument {
    pub(crate) fn new(
        descriptor: &Descriptor,
        library_id: String,
        commit_id: String,
        parent_commit_id: Option<String>,
        content_fingerprint: String,
        snapshot: RemoteObject,
    ) -> Result<Self> {
        let value = Self {
            repository_id: descriptor.repository_id.clone(),
            library_id,
            commit_id,
            parent_commit_id,
            scope_id: hex::encode(descriptor.scope_id),
            content_fingerprint,
            snapshot,
        };
        descriptor.validate().map_err(corrupt)?;
        if value.repository_id != descriptor.repository_id
            || value.scope_id != hex::encode(descriptor.scope_id)
            || !crate::trust_boundary::is_lower_hex_256(&value.content_fingerprint)
            || value.snapshot.repository_id != descriptor.repository_id
            || value.snapshot.role != ObjectRole::Snapshot
        {
            return Err(corrupt("invalid head document"));
        }
        Ok(value)
    }
    fn to_wire(
        &self,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<wire_control::HeadDocument> {
        descriptor.validate().map_err(corrupt)?;
        if self.repository_id != descriptor.repository_id
            || self.scope_id != hex::encode(descriptor.scope_id)
            || !crate::trust_boundary::is_lower_hex_256(&self.content_fingerprint)
            || self.snapshot.repository_id != descriptor.repository_id
            || self.snapshot.role != ObjectRole::Snapshot
        {
            return Err(corrupt("invalid head document"));
        }
        wire_control::HeadDocument::new(
            self.repository_id.clone(),
            self.library_id.clone(),
            self.commit_id.clone(),
            self.parent_commit_id.clone(),
            decode_hash(&self.scope_id)?,
            decode_hash(&self.content_fingerprint)?,
            self.snapshot.stored(repository)?,
        )
        .map_err(corrupt)
    }
    fn from_wire(
        value: wire_control::HeadDocument,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        if value.repository_id != descriptor.repository_id || value.scope_id != descriptor.scope_id
        {
            return Err(corrupt("head descriptor binding differs"));
        }
        Ok(Self {
            repository_id: value.repository_id,
            library_id: value.library_id,
            commit_id: value.commit_id,
            parent_commit_id: value.parent_commit_id,
            scope_id: hex::encode(value.scope_id),
            content_fingerprint: hex::encode(value.fingerprint),
            snapshot: RemoteObject::from_stored(&value.snapshot, repository)?,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackupPointDocument {
    pub repository_id: String,
    pub point_id: String,
    pub kind: BackupPointKind,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub scope_id: String,
    pub snapshots: Vec<RemoteObject>,
}
impl BackupPointDocument {
    pub(crate) fn new(
        descriptor: &Descriptor,
        point_id: String,
        kind: BackupPointKind,
        created_at_ms: u64,
        logical_revision: u64,
        snapshots: Vec<RemoteObject>,
    ) -> Result<Self> {
        let wire_kind = match kind {
            BackupPointKind::Automatic => wire_control::BackupPointKind::Backup,
            BackupPointKind::Manual => wire_control::BackupPointKind::Manual,
            BackupPointKind::Conflict => wire_control::BackupPointKind::Conflict,
            BackupPointKind::RecoveryCandidate => wire_control::BackupPointKind::History,
        };
        wire_kind
            .validate_snapshot_ids(snapshots.iter().map(|snapshot| snapshot.object_id.as_str()))
            .map_err(corrupt)?;
        let value = Self {
            repository_id: descriptor.repository_id.clone(),
            point_id,
            kind,
            created_at_ms,
            logical_revision,
            scope_id: hex::encode(descriptor.scope_id),
            snapshots,
        };
        descriptor.validate().map_err(corrupt)?;
        if value.repository_id != descriptor.repository_id
            || value.scope_id != hex::encode(descriptor.scope_id)
            || value.snapshots.iter().any(|snapshot| {
                snapshot.repository_id != descriptor.repository_id
                    || snapshot.role != ObjectRole::Snapshot
            })
        {
            return Err(corrupt("invalid backup point"));
        }
        Ok(value)
    }
    fn to_wire(
        &self,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<wire_control::BackupPointDocument> {
        descriptor.validate().map_err(corrupt)?;
        if self.repository_id != descriptor.repository_id
            || self.scope_id != hex::encode(descriptor.scope_id)
            || self.snapshots.iter().any(|snapshot| {
                snapshot.repository_id != descriptor.repository_id
                    || snapshot.role != ObjectRole::Snapshot
            })
        {
            return Err(corrupt("invalid backup point"));
        }
        let kind = match self.kind {
            BackupPointKind::Automatic => wire_control::BackupPointKind::Backup,
            BackupPointKind::Manual => wire_control::BackupPointKind::Manual,
            BackupPointKind::Conflict => wire_control::BackupPointKind::Conflict,
            BackupPointKind::RecoveryCandidate => wire_control::BackupPointKind::History,
        };
        wire_control::BackupPointDocument::new(
            self.repository_id.clone(),
            self.point_id.clone(),
            kind,
            self.created_at_ms,
            self.logical_revision,
            decode_hash(&self.scope_id)?,
            self.snapshots
                .iter()
                .map(|snapshot| snapshot.stored(repository))
                .collect::<Result<Vec<_>>>()?,
        )
        .map_err(corrupt)
    }
    fn from_wire(
        value: wire_control::BackupPointDocument,
        descriptor: &Descriptor,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        if value.repository_id != descriptor.repository_id || value.scope_id != descriptor.scope_id
        {
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
            logical_revision: value.logical_revision,
            scope_id: hex::encode(value.scope_id),
            snapshots: value
                .snapshots
                .iter()
                .map(|snapshot| RemoteObject::from_stored(snapshot, repository))
                .collect::<Result<Vec<_>>>()?,
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
    session: ExecutionSession,
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
        || Ok(session),
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
    F: FnOnce() -> Result<ExecutionSession>,
    G: FnOnce(ExecutionSession) -> Result<()>,
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
    let session = read_session()?;
    let write = match attempt.before_write(current.as_ref().map(|h| &h.observation), session) {
        Ok(write) => write,
        Err(error) if error.kind == ErrorKind::PreconditionFailed => {
            return Ok(PublicationResult::Conflict(current));
        }
        Err(error) => return Err(error),
    };
    let locator = provider.head_locator(repository)?;
    before_write(session)?;
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
    let object_id = format!("backup-point-{}", document.point_id);
    let plaintext = wire_document.encode(MAX_POINT_PLAINTEXT).map_err(corrupt)?;
    let plaintext_sha256 = hex::encode(hash(&plaintext));
    let spool = journal.spool_path(&object_id);
    if journal.record(&object_id)?.is_none() {
        let ciphertext = seal(
            descriptor,
            root_key,
            &object_id,
            wire::ObjectRole::BackupPoint,
            &plaintext,
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
            role: ObjectRole::BackupPoint,
            byte_length: ciphertext.len() as u64,
            sha256: hex::encode(hash(&ciphertext)),
        };
        journal.register(&intent)?;
    }
    let record = journal
        .record(&object_id)?
        .ok_or_else(|| corrupt("missing point journal"))?;
    if record.intent.role != ObjectRole::BackupPoint {
        return Err(corrupt("history journal role differs"));
    }
    let receipt = transfer_job::upload(journal, &object_id, provider, repository, cancel).await?;
    Ok(RemoteObject {
        repository_id: descriptor.repository_id.clone(),
        object_id,
        role: ObjectRole::BackupPoint,
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

/// Explicit history/restore discovery. Normal sync follows the snapshot locator
/// in the authenticated head and never scans this collection.
pub(crate) async fn find_snapshot(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot_id: &str,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    if snapshot_id.is_empty() || snapshot_id.len() > 1024 || snapshot_id.contains('\0') {
        return Err(corrupt("invalid snapshot selection"));
    }
    let expected_object_id = format!("snapshot-{snapshot_id}");
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::BTreeSet::new();
    for _ in 0..MAX_DISCOVERY_PAGES {
        cancel.check()?;
        let page = connected
            .provider
            .list_objects(
                &connected.handle,
                Collection::Snapshots,
                cursor.as_deref(),
                100,
                cancel,
            )
            .await?;
        for receipt in page.objects {
            if !receipt.complete
                || receipt.byte_length == 0
                || receipt.byte_length > MAX_SNAPSHOT_CIPHERTEXT
            {
                return Err(corrupt("invalid listed snapshot object"));
            }
            let (_, bytes) = download_control(
                connected.provider.as_ref(),
                &connected.handle,
                &receipt.locator,
                None,
                MAX_SNAPSHOT_CIPHERTEXT,
                cancel,
            )
            .await?;
            let bytes = bytes.ok_or_else(|| corrupt("listed snapshot was not downloaded"))?;
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
            if header.repository_id != connected.stored.descriptor.repository_id
                || header.role != wire::ObjectRole::Snapshot
            {
                return Err(corrupt("snapshot discovery envelope differs"));
            }
            let document = wire::SnapshotDocument::decode(&plaintext, wire::MAX_METADATA_BYTES)
                .map_err(corrupt)?;
            if document.repository_id != connected.stored.descriptor.repository_id
                || document.scope_id != connected.stored.descriptor.scope_id
                || header.object_id != format!("snapshot-{}", document.snapshot_id)
            {
                return Err(corrupt("snapshot discovery document differs"));
            }
            if document.snapshot_id == snapshot_id {
                if header.object_id != expected_object_id {
                    return Err(corrupt("snapshot selection identity differs"));
                }
                return Ok(RemoteObject {
                    repository_id: connected.stored.descriptor.repository_id.clone(),
                    object_id: header.object_id,
                    role: ObjectRole::Snapshot,
                    receipt,
                    ciphertext_sha256: hex::encode(hash(&bytes)),
                    plaintext_length: header.plaintext_length,
                    plaintext_sha256: hex::encode(hash(&plaintext)),
                });
            }
        }
        let Some(next) = page.next_cursor else {
            return Err(ProviderError::new(ErrorKind::NotFound));
        };
        if next.is_empty() || next.len() > 4096 || !seen_cursors.insert(next.clone()) {
            return Err(corrupt("invalid snapshot discovery cursor"));
        }
        cursor = Some(next);
    }
    Err(corrupt("snapshot discovery page limit"))
}

/// Reads a selected snapshot root through its authenticated direct locator.
/// This verifies the remote object's ciphertext, RNX1 header, plaintext hash,
/// repository and scope. Referenced packs remain unverified until restore or a
/// dedicated full-history verification downloads them.
pub(crate) async fn read_snapshot_document(
    connected: &super::connection_commands::ConnectedRepository,
    snapshot: &RemoteObject,
    cancel: &Cancellation,
) -> Result<wire::SnapshotDocument> {
    if snapshot.role != ObjectRole::Snapshot
        || snapshot.repository_id != connected.stored.descriptor.repository_id
        || snapshot.receipt.byte_length == 0
        || snapshot.receipt.byte_length > MAX_SNAPSHOT_CIPHERTEXT
    {
        return Err(corrupt("invalid selected snapshot"));
    }
    snapshot.stored(&connected.handle)?;
    let (_, bytes) = download_control(
        connected.provider.as_ref(),
        &connected.handle,
        &snapshot.receipt.locator,
        None,
        snapshot.receipt.byte_length,
        cancel,
    )
    .await?;
    let bytes = bytes.ok_or_else(|| corrupt("selected snapshot was not downloaded"))?;
    if hex::encode(hash(&bytes)) != snapshot.ciphertext_sha256 {
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
    if header != snapshot.stored(&connected.handle)?.header
        || hex::encode(hash(&plaintext)) != snapshot.plaintext_sha256
    {
        return Err(corrupt("snapshot plaintext differs"));
    }
    let document =
        wire::SnapshotDocument::decode(&plaintext, wire::MAX_METADATA_BYTES).map_err(corrupt)?;
    if document.repository_id != connected.stored.descriptor.repository_id
        || document.scope_id != connected.stored.descriptor.scope_id
        || snapshot.object_id != format!("snapshot-{}", document.snapshot_id)
    {
        return Err(corrupt("snapshot document binding differs"));
    }
    Ok(document)
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

    fn descriptor(strategy: Strategy) -> Descriptor {
        Descriptor::new(
            "descriptor-repository".into(),
            risunest_external_storage_format::format::Scope {
                library: true,
                referenced_assets: true,
                device_settings: false,
                device_plugins: false,
            },
            Some(strategy),
        )
        .unwrap()
    }
    fn snapshot(repository: &RepositoryHandle, id: &str) -> RemoteObject {
        let descriptor_id = "descriptor-repository".to_owned();
        let header = wire::PublicObjectHeader::new(
            descriptor_id.clone(),
            format!("snapshot-{id}"),
            wire::ObjectRole::Snapshot,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: descriptor_id,
            object_id: header.object_id.clone(),
            role: ObjectRole::Snapshot,
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
                Strategy::Cas, None, &first, ExecutionSession::Foreground, &Cancellation::default(),
            ).await.unwrap();
            assert!(matches!(result, PublicationResult::Confirmed(ref observed) if observed.document.commit_id == "c1"));

            let observed = read_head(&provider, &repository, &descriptor, &[9; 32], None, &Cancellation::default()).await.unwrap().unwrap();
            let second = prepare_head(&descriptor, &[9; 32], &repository, head(Strategy::Cas, &repository, "c2")).unwrap();
            let mut stale = observed.clone();
            stale.observation.authenticated_body_hash = "44".repeat(32);
            let conflict = publish_head(
                &provider, &repository, &fake::capabilities(true), &descriptor, &[9; 32],
                Strategy::Cas, Some(&stale), &second, ExecutionSession::Foreground, &Cancellation::default(),
            ).await.unwrap();
            assert!(matches!(conflict, PublicationResult::Conflict(Some(_))));
            assert_eq!(read_head(&provider, &repository, &descriptor, &[9; 32], None, &Cancellation::default()).await.unwrap().unwrap().document.commit_id, "c1");
        });
    }

    #[test]
    fn sequential_head_refuses_hidden_execution_before_any_write() {
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
            let error = publish_head(
                &provider,
                &repository,
                &fake::capabilities(false),
                &descriptor,
                &[8; 32],
                Strategy::Sequential,
                None,
                &prepared,
                ExecutionSession::Hidden,
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
            assert!(provider.state.lock().unwrap().objects.is_empty());
        });
    }

    #[test]
    fn sequential_head_revalidates_session_after_remote_pre_read() {
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
                || Ok(ExecutionSession::Hidden),
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
    fn conflict_point_requires_two_snapshots_and_uploads_as_an_immutable_object() {
        runtime().block_on(async {
            let provider = fake::FakeProvider::new(false);
            let repository = fake::repository();
            let descriptor = descriptor(Strategy::Sequential);
            assert!(BackupPointDocument::new(
                &descriptor,
                "conflict-1".into(),
                BackupPointKind::Conflict,
                1,
                1,
                vec![snapshot(&repository, "s1")],
            )
            .is_err());
            let document = BackupPointDocument::new(
                &descriptor,
                "conflict-1".into(),
                BackupPointKind::Conflict,
                1,
                1,
                vec![snapshot(&repository, "s1"), snapshot(&repository, "s2")],
            )
            .unwrap();
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
                document,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(uploaded.repository_id, descriptor.repository_id);
            assert_eq!(uploaded.role, ObjectRole::BackupPoint);
            assert_eq!(provider.state.lock().unwrap().objects.len(), 1);
        });
    }
}
