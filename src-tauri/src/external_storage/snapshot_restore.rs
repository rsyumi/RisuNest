//! Direct-locator snapshot download and full verification into native staging.
//! No PDS generation is activated here; the caller owns the subsequent staged
//! apply and eventual cleanup of this durable verified directory.
#[cfg(test)]
use super::worker_observation::spawn_blocking;
#[cfg(not(test))]
use tokio::task::spawn_blocking;
use super::{
    content_store::{ContentStore, ObjectSource},
    package_cache::CatalogRange,
    contract::{
        Cancellation, ErrorKind, ObjectRole, Provider, ProviderError, ReadReceipt,
        RepositoryHandle, Result,
    },
    packaging::{cpu_permit, RemoteObject},
    phase_progress::PhaseProgress,
    sections::{CapturedSection, SectionSource},
    transfer::SpoolSink,
};
use crate::asset_repository::PayloadCas;
use risunest_external_storage_format::{
    content_identity::hash_reader, crypto::derive_key, pack, snapshot as wire,
};
use sha2::{Digest, Sha256};
use rusqlite::OptionalExtension;
use futures::{FutureExt, StreamExt};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

/// What one staging root had to fetch, and how much of it was packs.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TestReads {
    pub network: u64,
    pub network_bytes: u64,
    pub packs: u64,
    pub pack_bytes: u64,
    /// Packs whose stored ciphertext is at or below the small-pack size.
    pub small_packs: u64,
}

/// The stored ciphertext length below which a pack counts as small.
#[cfg(test)]
pub(super) const SMALL_PACK_BYTES: u64 = 256 * 1024;

#[cfg(test)]
static TEST_READ_COUNTS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<PathBuf, TestReads>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
pub(super) fn reset_test_read_counts(staging_root: &Path) {
    TEST_READ_COUNTS
        .lock()
        .unwrap()
        .insert(staging_root.to_path_buf(), TestReads::default());
}

#[cfg(test)]
pub(super) fn take_test_read_counts(staging_root: &Path) -> TestReads {
    TEST_READ_COUNTS
        .lock()
        .unwrap()
        .remove(staging_root)
        .expect("test read counter root was registered")
}

/// The most pack copies one staging root held at once while it turned packs
/// over.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct TestTurnover {
    pub ciphertexts: usize,
    pub plaintexts: usize,
}

#[cfg(test)]
static TEST_TURNOVER: std::sync::LazyLock<std::sync::Mutex<BTreeMap<PathBuf, TestTurnover>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
pub(super) fn reset_test_turnover(staging_root: &Path) {
    TEST_TURNOVER
        .lock()
        .unwrap()
        .insert(staging_root.to_path_buf(), TestTurnover::default());
}

#[cfg(test)]
pub(super) fn take_test_turnover(staging_root: &Path) -> TestTurnover {
    TEST_TURNOVER
        .lock()
        .unwrap()
        .remove(staging_root)
        .expect("test turnover root was registered")
}

#[cfg(test)]
fn record_turnover(staging_root: &Path, packs: &[RemoteObject]) {
    let mut turnover = TEST_TURNOVER.lock().unwrap();
    let Some(counts) = turnover.get_mut(staging_root) else {
        return;
    };
    let ciphertexts = packs
        .iter()
        .filter(|pack| {
            staging_root
                .join("downloads")
                .join(format!("{}.cipher", pack.ciphertext_sha256))
                .exists()
        })
        .count();
    let plaintexts = packs
        .iter()
        .filter(|pack| {
            staging_root
                .join("plaintext")
                .join(&pack.plaintext_sha256)
                .exists()
        })
        .count();
    counts.ciphertexts = counts.ciphertexts.max(ciphertexts);
    counts.plaintexts = counts.plaintexts.max(plaintexts);
}

#[cfg(test)]
fn record_test_read(downloads: &Path, pack: bool, byte_length: u64) {
    let staging_root = downloads
        .parent()
        .expect("snapshot downloads directory has a staging root");
    let mut reads = TEST_READ_COUNTS.lock().unwrap();
    if let Some(counts) = reads.get_mut(staging_root) {
        counts.network += 1;
        counts.network_bytes += byte_length;
        if pack {
            counts.packs += 1;
            counts.pack_bytes += byte_length;
            if byte_length <= SMALL_PACK_BYTES {
                counts.small_packs += 1;
            }
        }
    }
}

fn corrupt(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt).caused(&error)
}
fn transient(error: impl std::fmt::Display + 'static) -> ProviderError {
    super::packaging::transient(error)
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedRecord {
    pub key: String,
    pub content_hash: String,
    pub byte_length: u64,
    pub source: ObjectSource,
}
#[derive(Clone, Debug)]
pub(crate) struct PreparedObject {
    pub content_hash: String,
    pub byte_length: u64,
    pub source: ObjectSource,
}
#[derive(Clone, Debug)]
pub(crate) struct PreparedRemoteSnapshot {
    pub snapshot_id: String,
    pub repository_id: String,
    pub fingerprint: String,
    pub library_fingerprint: String,
    pub logical_revision: u64,
    pub staging_root: PathBuf,
    pub records: Vec<PreparedRecord>,
    pub objects: Vec<PreparedObject>,
    /// The device whose own values the sections are, when there is one.
    pub captured_by_device: Option<String>,
}

/// Where a downloaded snapshot's record bodies live. The apply attaches to
/// this same directory beside the staging root.
fn content_store(staging_root: &Path) -> Result<ContentStore> {
    #[cfg(test)]
    CONTENT_STORE_OPENS.lock().unwrap().push(staging_root.to_owned());
    ContentStore::open(&staging_root.join("external-storage")).map_err(transient)
}

/// Every staging directory a content store was opened for.
#[cfg(test)]
pub(super) static CONTENT_STORE_OPENS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Whether staging has to survive a restart. A restore resumes from its
/// staging directory; scratch staging is thrown away with the work that made
/// it, so its writes skip the syncs and its records the content store.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    Durable,
    Scratch,
}

/// Where a placement puts records. Durable staging opens its content store up
/// front; scratch staging opens one only when a record arrives, since bodies
/// placed as files never touch it.
struct Placement {
    root: PathBuf,
    content: Option<ContentStore>,
    durability: Durability,
}

impl Placement {
    fn open(staging_root: &Path, durability: Durability) -> Result<Self> {
        let content = match durability {
            Durability::Durable => Some(content_store(staging_root)?),
            Durability::Scratch => None,
        };
        Ok(Self {
            root: staging_root.to_path_buf(),
            content,
            durability,
        })
    }

    fn content(&mut self) -> Result<&mut ContentStore> {
        let content = match self.content.take() {
            Some(content) => content,
            None => content_store(&self.root)?,
        };
        Ok(self.content.insert(content))
    }

    fn commit(&mut self) -> Result<()> {
        match &mut self.content {
            Some(content) => content.commit().map_err(transient),
            None => Ok(()),
        }
    }

    fn durable(&self) -> bool {
        self.durability == Durability::Durable
    }
}

#[cfg(test)]
impl PreparedRemoteSnapshot {
    /// A prepared body, wherever the plan put it: a record in the content store
    /// beside the staging root, an asset in a staging file.
    pub(super) fn body(&self, source: &ObjectSource) -> Vec<u8> {
        match source {
            ObjectSource::File(path) => fs::read(path).expect("prepared staging file"),
            ObjectSource::Captured(digest) => content_store(&self.staging_root)
                .expect("prepared content store")
                .read_all(digest)
                .expect("prepared record body"),
            ObjectSource::Library(_) => panic!("a library body is not read through here"),
        }
    }
    pub(super) fn record_body(&self, index: usize) -> Vec<u8> {
        self.body(&self.records[index].source)
    }
    /// Drop every record body this plan prepared, so a run over the same
    /// staging directory has to build them again.
    pub(super) fn discard_record_bodies(&self) {
        let mut content = content_store(&self.staging_root).expect("prepared content store");
        for record in &self.records {
            if let Some(path) = content
                .file_path(&record.content_hash)
                .expect("prepared record path")
            {
                fs::remove_file(path).expect("discard prepared record file");
            }
        }
        let digests = self
            .records
            .iter()
            .map(|record| record.content_hash.as_str())
            .collect::<Vec<_>>();
        content
            .delete(&digests)
            .expect("discard prepared record bodies");
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(transient)?;
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(path).map_err(transient)?) {
        return Err(corrupt("snapshot staging directory is a link"));
    }
    Ok(())
}
pub(crate) fn verify_body_file(path:&Path,length:u64,sha256:&str)->Result<bool> {verify(path,length,sha256)}
fn verify(path: &Path, length: u64, sha256: &str) -> Result<bool> {
    let mut file = match crate::trust_boundary::open_regular_source(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(corrupt(error)),
    };
    if file.metadata().map_err(corrupt)?.len() != length {
        return Ok(false);
    }
    let hashed=hash_reader(&mut file,length);
    #[cfg(test)]
    match &hashed {
        Ok(_)=>crate::persistent_store::hash_work::observe("external_restore_file_verify",length as usize),
        Err(_)=>crate::persistent_store::hash_work::incomplete("external_restore_file_verify"),
    }
    Ok(hex::encode(hashed.map_err(corrupt)?) == sha256)
}

fn publish_verified(
    partial: &Path,
    destination: &Path,
    expected_length: u64,
    expected_sha256: &str,
) -> Result<()> {
    publish_verified_with(partial, destination, expected_length, expected_sha256, Durability::Durable)
}

fn publish_verified_with(
    partial: &Path,
    destination: &Path,
    expected_length: u64,
    expected_sha256: &str,
    durability: Durability,
) -> Result<()> {
    if destination.exists() {
        if verify(destination, expected_length, expected_sha256)? {
            fs::remove_file(partial).map_err(transient)?;
            return Ok(());
        }
        let metadata = fs::symlink_metadata(destination).map_err(corrupt)?;
        if crate::trust_boundary::is_link_like(&metadata) || !metadata.file_type().is_file() {
            return Err(corrupt(
                "snapshot staging destination is not a regular file",
            ));
        }
        fs::remove_file(destination).map_err(transient)?;
    }
    #[cfg(target_os = "android")]
    let result = crate::trust_boundary::rename_without_replace(partial, destination);
    #[cfg(not(target_os = "android"))]
    let result = fs::hard_link(partial, destination);
    match result {
        Ok(()) => {
            #[cfg(not(target_os = "android"))]
            fs::remove_file(partial).map_err(transient)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !verify(destination, expected_length, expected_sha256)? {
                return Err(corrupt(
                    "snapshot staging publication raced with corrupt data",
                ));
            }
            fs::remove_file(partial).map_err(transient)?;
        }
        Err(error) => return Err(transient(error)),
    }
    if durability == Durability::Durable {
        crate::trust_boundary::sync_directory(destination.parent().unwrap()).map_err(transient)?;
    }
    Ok(())
}

async fn download_ciphertext(
    object: &RemoteObject,
    downloads: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    durability: Durability,
    progress: Option<&std::sync::Arc<super::phase_progress::DownloadProgress>>,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    object.stored(repository)?;
    let destination = downloads.join(format!("{}.cipher", object.ciphertext_sha256));
    if verify(
        &destination,
        object.receipt.byte_length,
        &object.ciphertext_sha256,
    )? {
        return Ok(destination);
    }
    if destination.exists() {
        fs::remove_file(&destination).map_err(transient)?;
    }
    let partial = downloads.join(format!("{}.partial", object.ciphertext_sha256));
    if partial.exists() {
        fs::remove_file(&partial).map_err(transient)?;
    }
    let mut sink = SpoolSink::create(&partial, object.receipt.byte_length)?;
    if let Some(progress)=progress {
        let progress=progress.clone();
        sink.observe(std::sync::Arc::new(move |bytes|progress.advance(bytes)));
    }
    #[cfg(test)]
    record_test_read(
        downloads,
        object.role == ObjectRole::Pack,
        object.receipt.byte_length,
    );
    let receipt = provider
        .read_object(repository, &object.receipt.locator, None, &mut sink, cancel)
        .await?;
    let ReadReceipt::Body(receipt) = receipt else {
        return Err(corrupt("unexpected not-modified response"));
    };
    if !sink.is_verified() || !receipt.complete || receipt.byte_length != object.receipt.byte_length
    {
        return Err(corrupt("incomplete snapshot object"));
    }
    if !verify(
        &partial,
        object.receipt.byte_length,
        &object.ciphertext_sha256,
    )? {
        return Err(corrupt("snapshot ciphertext hash differs"));
    }
    publish_verified_with(
        &partial,
        &destination,
        object.receipt.byte_length,
        &object.ciphertext_sha256,
        durability,
    )?;
    Ok(destination)
}

/// A bounded batch shares each ciphertext destination, including duplicate references.
/// Only the owner opens plaintext or advances durable placement state.
fn stage_ciphertexts<'a>(
    objects: &[&'a RemoteObject], staging_root: &'a Path, provider: &'a dyn Provider,
    repository: &'a RepositoryHandle, durability: Durability, progress: Option<&'a PhaseProgress>, cancel: &'a Cancellation,
) -> Result<impl futures::Stream<Item = (&'a RemoteObject, Result<Option<PathBuf>>)> + 'a> {
    let downloads = staging_root.join("downloads");
    ensure_directory(&downloads)?;
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut destinations = BTreeMap::new();
    let mut pending = Vec::with_capacity(objects.len());
    for &object in objects {
        let transfer = if let Some((previous, transfer)) = destinations.get(&object.ciphertext_sha256) {
            if *previous != object { return Err(corrupt("conflicting ciphertext destination")); }
            futures::future::Shared::clone(transfer)
        } else {
            let downloads = downloads.clone();
            let stopped = stopped.clone();
            let transfer = async move {
                cancel.check()?;
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(ProviderError::new(ErrorKind::Cancelled));
                }
                let streamed=progress.map(super::phase_progress::DownloadProgress::new);
                let result = async {
                    let plaintext = staging_root.join("plaintext").join(&object.plaintext_sha256);
                    if verify(&plaintext, object.plaintext_length, &object.plaintext_sha256)? {
                        return Ok(None);
                    }
                    download_ciphertext(object, &downloads, provider, repository, durability, streamed.as_ref(), cancel).await.map(Some)
                }.await;
                if result.is_ok() {if let Some(progress)=&streamed {progress.commit(object.receipt.byte_length);}}
                if result.is_err() { stopped.store(true, std::sync::atomic::Ordering::Release); }
                result
            }.boxed().shared();
            destinations.insert(object.ciphertext_sha256.clone(), (object, transfer.clone()));
            transfer
        };
        pending.push(async move { (object, transfer.await) });
    }
    Ok(futures::stream::iter(pending).buffered(objects.len().max(1)))
}

async fn open_staged_object(
    object: &RemoteObject, root_key: &[u8; 32], staging_root: &Path,
    ciphertext: Option<PathBuf>, durability: Durability, progress: Option<&PhaseProgress>,
) -> Result<PathBuf> {
    let plaintext = staging_root.join("plaintext");
    ensure_directory(&plaintext)?;
    let destination = plaintext.join(&object.plaintext_sha256);
    if verify(&destination, object.plaintext_length, &object.plaintext_sha256)? {
        return Ok(destination);
    }
    if destination.exists() { fs::remove_file(&destination).map_err(transient)?; }
    let ciphertext = ciphertext.ok_or_else(|| corrupt("staged ciphertext is missing"))?;
    decrypt_object(object, root_key, &plaintext, &destination, &ciphertext, durability, progress).await
}

pub(super) async fn open_object(
    object: &RemoteObject,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    open_object_with(object, root_key, staging_root, provider, repository, Durability::Durable, None, cancel).await
}

/// `open_object`, counting in `progress` an object it had to fetch.
async fn open_object_counted(
    object: &RemoteObject,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    open_object_with(object, root_key, staging_root, provider, repository, Durability::Durable, Some(progress), cancel).await
}

#[allow(clippy::too_many_arguments)]
async fn open_object_with(
    object: &RemoteObject,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    durability: Durability,
    progress: Option<&PhaseProgress>,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    let downloads = staging_root.join("downloads");
    let plaintext = staging_root.join("plaintext");
    ensure_directory(&downloads)?;
    ensure_directory(&plaintext)?;
    let destination = plaintext.join(&object.plaintext_sha256);
    if verify(
        &destination,
        object.plaintext_length,
        &object.plaintext_sha256,
    )? {
        return Ok(destination);
    }
    if destination.exists() {
        fs::remove_file(&destination).map_err(transient)?;
    }
    let ciphertext = download_ciphertext(object, &downloads, provider, repository, durability, None, cancel).await?;
    decrypt_object(object, root_key, &plaintext, &destination, &ciphertext, durability, progress).await
}

async fn decrypt_object(
    object: &RemoteObject, root_key: &[u8; 32], plaintext: &Path, destination: &Path,
    ciphertext: &Path, durability: Durability, progress: Option<&PhaseProgress>,
) -> Result<PathBuf> {
    let partial = plaintext.join(format!("{}.partial", object.plaintext_sha256));
    if partial.exists() {
        fs::remove_file(&partial).map_err(transient)?;
    }
    let object_copy = object.clone();
    let repository_id = object.repository_id.clone();
    let root = *root_key;
    let cipher_path = ciphertext.to_owned();
    let partial_path = partial.clone();
    let cpu = cpu_permit().await?;
    spawn_blocking(move || {
        let mut input =
            crate::trust_boundary::open_regular_source(&cipher_path).map_err(corrupt)?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial_path)
            .map_err(transient)?;
        let purpose = if object_copy.role == ObjectRole::Pack {
            "data"
        } else {
            "metadata"
        };
        let key = derive_key(&root, &repository_id, purpose).map_err(corrupt)?;
        let header =
            wire::open_envelope(&mut input, &mut output, &key, object_copy.plaintext_length)
                .map_err(super::packaging::format_error)?;
        if durability == Durability::Durable {
            output.sync_all().map_err(transient)?;
        }
        drop(output);
        let expected_role = match object_copy.role {
            ObjectRole::Segment => return Err(ProviderError::new(ErrorKind::Unsupported)),
            ObjectRole::Snapshot => wire::ObjectRole::SyncState,
            ObjectRole::Descriptor => wire::ObjectRole::Descriptor,
            ObjectRole::Pack => wire::ObjectRole::Pack,
            ObjectRole::Catalog => wire::ObjectRole::Catalog,
            ObjectRole::SyncState => wire::ObjectRole::SyncState,
            ObjectRole::BackupBundle => wire::ObjectRole::BackupBundle,
            ObjectRole::BackupPoint => wire::ObjectRole::BackupPoint,
            ObjectRole::InventoryPage => wire::ObjectRole::InventoryPage,
            ObjectRole::Lease => wire::ObjectRole::Lease,
        };
        let expected = wire::PublicObjectHeader::new(
            repository_id,
            object_copy.object_id.clone(),
            expected_role,
            object_copy.plaintext_length,
        )
        .map_err(corrupt)?;
        if header != expected
            || !verify(
                &partial_path,
                object_copy.plaintext_length,
                &object_copy.plaintext_sha256,
            )?
        {
            return Err(corrupt("snapshot plaintext differs"));
        }
        Ok(())
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    publish_verified_with(
        &partial,
        &destination,
        object.plaintext_length,
        &object.plaintext_sha256,
        durability,
    )?;
    if let Some(progress) = progress {
        progress.found_completed(object.receipt.byte_length);
    }
    Ok(destination.to_owned())
}

/// Removes what one opened object left behind. A check proves a body and has
/// no use for it afterwards, so the staging directory never grows past the
/// object being read.
pub(super) fn discard_object(staging_root: &Path, object: &RemoteObject) {
    let _ = fs::remove_file(staging_root.join("plaintext").join(&object.plaintext_sha256));
    let _ = fs::remove_file(
        staging_root
            .join("downloads")
            .join(format!("{}.cipher", object.ciphertext_sha256)),
    );
}

pub(super) fn read_bytes(path: &Path, max: usize) -> Result<Vec<u8>> {
    let mut file = crate::trust_boundary::open_regular_source(path).map_err(corrupt)?;
    let length = file.metadata().map_err(corrupt)?.len();
    if length > max as u64 {
        return Err(corrupt("metadata limit exceeded"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.read_to_end(&mut bytes).map_err(corrupt)?;
    Ok(bytes)
}

#[derive(Clone)]
pub(super) struct CompleteEntry {
    pub(super) kind: wire::CatalogEntryKind,
    pub(super) key: String,
    pub(super) content_sha256: [u8; 32],
    pub(super) byte_length: u64,
    pub(super) chunks: Vec<wire::StoredChunk>,
}

/// Keep what this download already read, so the publication that follows it
/// does not ask the provider for the same catalog again. A cache that cannot
/// be written costs that, and nothing else, so it does not fail the download.
fn record_read(
    record_into: Option<&Path>,
    label: &super::packaging::CatalogRoot,
    root: &RemoteObject,
    entries: &[CompleteEntry],
    packs: &BTreeMap<String, RemoteObject>,
    nodes: &[(RemoteObject, CatalogRange)],
    repository: &RepositoryHandle,
) {
    let Some(cache_root) = record_into else {
        return;
    };
    let _ = super::cache_hydration::record_read_catalog(
        cache_root, label, root, entries, packs, nodes, repository,
    );
}

/// Everything one catalog names: its complete entries, the packs they point
/// at, and the nodes the walk passed through with the key range each covered.
/// A restore only needs the first two, and rebuilding reuse metadata needs all
/// three. Nodes arrive in depth-first order, which keeps the nodes of one
/// level in the order that level was published in.
pub(super) async fn read_catalog(
    root: &RemoteObject,
    expected_kind: wire::CatalogKind,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<(
    Vec<CompleteEntry>,
    BTreeMap<String, RemoteObject>,
    Vec<(RemoteObject, CatalogRange)>,
)> {
    read_catalog_with(root, expected_kind, root_key, staging_root, provider, repository, &PhaseProgress::silent(), cancel).await
}

/// `read_catalog`, counting in `progress` each node the walk had to fetch.
#[allow(clippy::too_many_arguments)]
async fn read_catalog_with(
    root: &RemoteObject,
    expected_kind: wire::CatalogKind,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<(
    Vec<CompleteEntry>,
    BTreeMap<String, RemoteObject>,
    Vec<(RemoteObject, CatalogRange)>,
)> {
    let mut stack = vec![root.clone()];
    let mut nodes = Vec::new();
    let mut visited = BTreeSet::new();
    let mut fragments: BTreeMap<(u8, String), Vec<wire::CatalogEntryFragment>> = BTreeMap::new();
    let mut packs = BTreeMap::new();
    while let Some(object) = stack.pop() {
        cancel.check()?;
        if object.role != ObjectRole::Catalog
            || object.repository_id != root.repository_id
            || !visited.insert(object.object_id.clone())
        {
            return Err(corrupt("duplicate or invalid catalog node"));
        }
        let path = open_object_counted(
            &object,
            root_key,
            staging_root,
            provider,
            repository,
            progress,
            cancel,
        )
        .await?;
        let bytes = read_bytes(&path, wire::MAX_METADATA_BYTES)?;
        let document = wire::CatalogDocument::decode(
            &bytes,
            usize::try_from(object.plaintext_length).map_err(corrupt)?,
        )
        .map_err(corrupt)?;
        if document.kind != expected_kind {
            return Err(corrupt("catalog kind differs"));
        }
        nodes.push((
            object.clone(),
            CatalogRange {
                level: document.level,
                first_key: document.first_key.clone(),
                last_key: document.last_key.clone(),
            },
        ));
        if document.level == 0 {
            for pack in document.packs {
                let remote = RemoteObject::from_stored(&pack, repository)?;
                if remote.role != ObjectRole::Pack || remote.repository_id != root.repository_id {
                    return Err(corrupt("catalog references non-pack"));
                }
                if let Some(old) = packs.insert(remote.object_id.clone(), remote.clone()) {
                    if old != remote {
                        return Err(corrupt("conflicting pack reference"));
                    }
                }
            }
            for fragment in document.entries {
                let valid_kind = match expected_kind {
                    wire::CatalogKind::Records => matches!(
                        fragment.kind,
                        wire::CatalogEntryKind::Record | wire::CatalogEntryKind::Object
                    ),
                    wire::CatalogKind::Assets => fragment.kind == wire::CatalogEntryKind::Object,
                    wire::CatalogKind::Section => matches!(
                        fragment.kind,
                        wire::CatalogEntryKind::SectionEntry
                            | wire::CatalogEntryKind::SectionObject
                    ),
                };
                if !valid_kind {
                    return Err(corrupt("catalog entry kind differs"));
                }
                let tag = match fragment.kind {
                    wire::CatalogEntryKind::Record => 0,
                    wire::CatalogEntryKind::Object => 1,
                    wire::CatalogEntryKind::SectionEntry => 2,
                    wire::CatalogEntryKind::SectionObject => 3,
                };
                fragments
                    .entry((tag, fragment.key.clone()))
                    .or_default()
                    .push(fragment);
            }
        } else {
            for child in document.children.into_iter().rev() {
                stack.push(RemoteObject::from_stored(&child.object, repository)?);
            }
        }
    }
    let mut result = Vec::new();
    for ((_tag, key), mut parts) in fragments {
        parts.sort_by_key(|part| part.fragment_index);
        let first = parts
            .first()
            .ok_or_else(|| corrupt("missing catalog fragment"))?;
        let expected_count = usize::try_from(first.fragment_count).map_err(corrupt)?;
        if parts.len() != expected_count {
            return Err(corrupt("catalog fragment missing"));
        }
        let kind = first.kind;
        let content_sha256 = first.content_sha256;
        let byte_length = first.byte_length;
        let mut chunks = Vec::new();
        for (index, part) in parts.into_iter().enumerate() {
            if part.fragment_index as usize != index
                || part.fragment_count as usize != expected_count
                || part.kind != kind
                || part.key != key
                || part.content_sha256 != content_sha256
                || part.byte_length != byte_length
            {
                return Err(corrupt("conflicting catalog fragments"));
            }
            chunks.extend(part.chunks);
        }
        let total = chunks.iter().try_fold(0u64, |sum, chunk| {
            sum.checked_add(chunk.plaintext_length)
                .ok_or_else(|| corrupt("entry length overflow"))
        })?;
        if total != byte_length {
            return Err(corrupt("catalog entry length differs"));
        }
        result.push(CompleteEntry {
            kind,
            key,
            content_sha256,
            byte_length,
            chunks,
        });
    }
    Ok((result, packs, nodes))
}

fn pack_totals(packs: &BTreeMap<String, RemoteObject>) -> (u64, u64) {
    (
        packs.len() as u64,
        packs.values().fold(0u64, |sum, pack| sum.saturating_add(pack.receipt.byte_length)),
    )
}

/// Opens every pack, counting each one in `progress`, which the caller
/// planned them in.
async fn open_packs(
    packs: &BTreeMap<String, RemoteObject>,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut paths = BTreeMap::new();
    let mut pending = packs.iter().peekable();
    while pending.peek().is_some() {
        let batch: Vec<_> = pending.by_ref().take(provider.transfer_concurrency()).collect();
        let objects: Vec<_> = batch.iter().map(|(_, object)| *object).collect();
        let mut downloads = stage_ciphertexts(&objects, staging_root, provider, repository, Durability::Durable, Some(progress), cancel)?;
        let mut identities = batch.iter();
        while let Some((pack, ciphertext)) = downloads.next().await {
            let id = identities.next().ok_or_else(|| corrupt("download identity missing"))?.0;
            let path = open_staged_object(pack, root_key, staging_root, ciphertext?, Durability::Durable, None).await?;
            let cipher = staging_root.join("downloads").join(format!("{}.cipher", pack.ciphertext_sha256));
            match fs::remove_file(cipher) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(transient(error)),
                _ => {}
            }
            paths.insert(id.clone(), path);
            progress.completed(0);
        }
    }
    Ok(paths)
}

/// What a resolver may accept as a source, and what the consumer's repository
/// is. A body is only offered as a library source when the job that receives
/// the plan writes into that same repository, which a scratch store does not.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SourceTrust<'a> {
    /// Nothing local answers. Every body comes out of a pack.
    Downloaded,
    /// The repository at this root may answer, and each body it answers with
    /// is read and proved against the identity the catalog names first.
    ProvenLibrary(&'a Path),
    /// The repository at this root may answer on identity and length alone,
    /// which is what it was already admitted under.
    AdmittedLibrary(&'a Path),
}

/// The repository a plan may take bodies from, and whether each one has to be
/// read before it counts as a source.
struct LocalLibrary {
    root: PathBuf,
    prove: bool,
}

/// One catalog entry and where its content is going to come from. An entry
/// that is already here names its source; the rest name the chunks that have
/// to be assembled, and only those decide which packs are opened.
struct ResolvedEntry {
    entry: CompleteEntry,
    digest: String,
    /// Where an object's body is going to be written. A record has none,
    /// because the content store decides where its body belongs.
    destination: Option<PathBuf>,
    source: Option<ObjectSource>,
}

/// Decides per entry whether anything has to be read out of a pack. Content
/// this staging directory already holds and can prove is not assembled again.
fn resolve_entries(
    entries: Vec<CompleteEntry>,
    staging_root: &Path,
    content: &ContentStore,
    library: Option<LocalLibrary>,
    cancel: &Cancellation,
) -> Result<Vec<ResolvedEntry>> {
    resolve_entries_in(entries, staging_root, Some(content), library, cancel)
}

/// Resolves entries against `content` when there is one. Without it no
/// record counts as held, so every record is placed again.
fn resolve_entries_in(
    entries: Vec<CompleteEntry>,
    staging_root: &Path,
    content: Option<&ContentStore>,
    library: Option<LocalLibrary>,
    cancel: &Cancellation,
) -> Result<Vec<ResolvedEntry>> {
    let prove = library.as_ref().is_some_and(|library| library.prove);
    let library = library
        .map(|library| PayloadCas::new(&library.root))
        .transpose()
        .map_err(transient)?;
    let object_root = staging_root.join("objects");
    ensure_directory(&object_root)?;
    let mut plan = Vec::with_capacity(entries.len());
    for entry in entries {
        cancel.check()?;
        let digest = hex::encode(entry.content_sha256);
        let (destination, source) = match entry.kind {
            // A record already in the content store is proved the same way a
            // staging file was, by its length and then by its content.
            wire::CatalogEntryKind::Record => {
                let length = i64::try_from(entry.byte_length).map_err(corrupt)?;
                let held = content.is_some_and(|content| content.validate(&digest, length, true).is_ok());
                (None, held.then(|| ObjectSource::Captured(digest.clone())))
            }
            wire::CatalogEntryKind::Object => {
                let destination = object_root.join(&digest);
                let mut source = verify(&destination, entry.byte_length, &digest)?
                    .then(|| ObjectSource::File(destination.clone()));
                if source.is_none() {
                    if let Some(cas) = library.as_ref() {
                        source = library_body(cas, &digest, entry.byte_length, prove)?;
                    }
                }
                (Some(destination), source)
            }
            wire::CatalogEntryKind::SectionEntry | wire::CatalogEntryKind::SectionObject => {
                return Err(corrupt("section entry in library catalog"));
            }
        };
        plan.push(ResolvedEntry {
            entry,
            digest,
            destination,
            source,
        });
    }
    Ok(plan)
}

/// A body the library already holds under this exact identity and length. The
/// length is asked for as well, because a truncated file carries the name of
/// the content it no longer is.
fn library_body(
    cas: &PayloadCas,
    digest: &str,
    byte_length: u64,
    prove: bool,
) -> Result<Option<ObjectSource>> {
    let Some(path) = cas.object_path(digest).map_err(transient)? else {
        return Ok(None);
    };
    let mut file = match crate::trust_boundary::open_regular_source(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(corrupt(error)),
    };
    if file.metadata().map_err(corrupt)?.len() != byte_length {
        return Ok(None);
    }
    // Proving it costs one read of a body that is going to be consumed anyway,
    // which is what a job that answers for everything it consumes owes. A body
    // that fails is not a source, so the pack that carries it is read instead.
    if prove {
        let hashed=hash_reader(&mut file,byte_length);
        #[cfg(test)]
        match &hashed {
            Ok(_)=>crate::persistent_store::hash_work::observe("external_restore_local_source_verify",byte_length as usize),
            Err(_)=>crate::persistent_store::hash_work::incomplete("external_restore_local_source_verify"),
        }
        if hex::encode(hashed.map_err(corrupt)?) != digest {return Ok(None);}
    }
    Ok(Some(ObjectSource::Library(digest.to_owned())))
}

/// One entry that still has to be put together out of packs.
struct Pending {
    resolved: ResolvedEntry,
    /// Where each chunk starts in the assembled body.
    offsets: Vec<u64>,
    /// The packs its chunks lie in. An entry in one pack is put together while
    /// that pack is open; one spread over several is written into its own
    /// assembly file a chunk at a time, as each of its packs arrives.
    packs: BTreeSet<String>,
}

impl Pending {
    fn new(resolved: ResolvedEntry) -> Result<Self> {
        let mut offsets = Vec::with_capacity(resolved.entry.chunks.len());
        let mut length = 0u64;
        for chunk in &resolved.entry.chunks {
            offsets.push(length);
            length = length
                .checked_add(chunk.plaintext_length)
                .ok_or_else(|| corrupt("restored entry length overflow"))?;
        }
        if length != resolved.entry.byte_length {
            return Err(corrupt("restored entry length differs from its chunks"));
        }
        let packs = resolved
            .entry
            .chunks
            .iter()
            .map(|chunk| chunk.pack_id.clone())
            .collect();
        Ok(Self {
            resolved,
            offsets,
            packs,
        })
    }

    fn spread(&self) -> bool {
        self.packs.len() > 1
    }
}

/// What a pack's placement leaves in `turnover/`: its bytes are in the
/// assembly files of the entries it carries, so it is not read again.
fn marker_path(staging_root: &Path, pack_id: &str) -> Result<PathBuf> {
    if pack_id.is_empty()
        || !pack_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(corrupt("catalog pack identity is not a file name"));
    }
    Ok(staging_root.join("turnover").join(pack_id))
}

fn remove_marker(staging_root: &Path, pack_id: &str) -> Result<()> {
    match fs::remove_file(marker_path(staging_root, pack_id)?) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(transient(error)),
        _ => Ok(()),
    }
}

fn assembly_path(staging_root: &Path, digest: &str) -> PathBuf {
    staging_root.join("assembly").join(digest)
}

/// Readies what an earlier attempt left for another pass. An assembly file
/// that is not the length it was created at no longer holds what the markers
/// of its packs say it does, and an entry placed whole out of one pack is
/// only here once its pack is read again.
fn prepare_turnover(staging_root: &Path, pending: &[Pending]) -> Result<()> {
    ensure_directory(&staging_root.join("assembly"))?;
    ensure_directory(&staging_root.join("turnover"))?;
    for item in pending {
        if !item.spread() {
            for pack in &item.packs {
                remove_marker(staging_root, pack)?;
            }
            continue;
        }
        let path = assembly_path(staging_root, &item.resolved.digest);
        let intact = match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file()
                    && !crate::trust_boundary::is_link_like(&metadata)
                    && metadata.len() == item.resolved.entry.byte_length =>
            {
                true
            }
            Ok(_) => {
                fs::remove_file(&path).map_err(transient)?;
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(transient(error)),
        };
        if !intact {
            for pack in &item.packs {
                remove_marker(staging_root, pack)?;
            }
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(transient)?;
            file.set_len(item.resolved.entry.byte_length)
                .map_err(transient)?;
        }
    }
    Ok(())
}

fn read_chunk(
    input: &mut fs::File,
    input_length: u64,
    chunk: &wire::StoredChunk,
) -> Result<Vec<u8>> {
    if chunk
        .offset
        .checked_add(chunk.stored_length)
        .is_none_or(|end| end > input_length)
    {
        return Err(corrupt("chunk escaped pack"));
    }
    input.seek(SeekFrom::Start(chunk.offset)).map_err(corrupt)?;
    let mut limited = input.take(chunk.stored_length);
    let decoded = pack::read_entry(&mut limited, pack::MAX_CHUNK_BYTES);
    #[cfg(test)]
    match &decoded {
        Ok(decoded)=>crate::persistent_store::hash_work::observe("native_external_pack_decode",decoded.bytes.len()),
        Err(_)=>crate::persistent_store::hash_work::incomplete("native_external_pack_decode"),
    }
    let decoded=decoded.map_err(corrupt)?;
    if limited.limit() != 0
        || decoded.hash != chunk.plaintext_sha256
        || decoded.bytes.len() as u64 != chunk.plaintext_length
    {
        return Err(corrupt("pack chunk differs"));
    }
    Ok(decoded.bytes)
}

/// Puts an entry that lies in one pack (or in none, when it is empty) where it
/// belongs: a record into the content store, an object into its file.
fn place_whole(
    mut input: Option<(&mut fs::File, u64)>,
    item: &Pending,
    placement: &mut Placement,
) -> Result<()> {
    let ResolvedEntry {
        entry,
        digest,
        destination,
        ..
    } = &item.resolved;
    let mut next = |chunk: &wire::StoredChunk| match input.as_mut() {
        Some((file, length)) => read_chunk(file, *length, chunk),
        None => Err(corrupt("catalog pack is missing")),
    };
    let Some(destination) = destination else {
        // The store takes the body and decides where it belongs, so a record
        // never becomes a plaintext file of its own that the apply has to
        // copy into another store. The identity the catalog named is proved
        // by the store.
        let mut bytes =
            Vec::with_capacity(usize::try_from(entry.byte_length).map_err(corrupt)?);
        for chunk in &entry.chunks {
            bytes.extend_from_slice(&next(chunk)?);
        }
        if bytes.len() as u64 != entry.byte_length {
            return Err(corrupt("restored entry integrity failed"));
        }
        return placement.content()?.put(digest, &bytes).map_err(corrupt);
    };
    let partial = destination.with_extension("partial");
    if partial.exists() {
        fs::remove_file(&partial).map_err(transient)?;
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .map_err(transient)?;
    let mut output_hash = Sha256::new();
    #[cfg(test)]
    crate::persistent_store::hash_work::begin("external_restore_entry_assembly");
    let mut written = 0u64;
    for chunk in &entry.chunks {
        let bytes = next(chunk)?;
        output.write_all(&bytes).map_err(transient)?;
        #[cfg(test)]
        crate::persistent_store::hash_work::update("external_restore_entry_assembly",bytes.len());
        output_hash.update(&bytes);
        written = written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| corrupt("restored entry length overflow"))?;
    }
    if placement.durable() {
        output.sync_all().map_err(transient)?;
    }
    drop(output);
    // The bytes were hashed as they were written and the length is what was
    // written, so reading the file back to learn the same two things is a
    // pass with no evidence in it.
    let actual: [u8; 32] = output_hash.finalize().into();
    if actual != entry.content_sha256 || written != entry.byte_length {
        return Err(corrupt("restored entry integrity failed"));
    }
    publish_verified_with(&partial, destination, entry.byte_length, digest, placement.durability)
}

/// Packs placed since the last flush. Their markers wait until what they
/// placed is durable, so an interruption costs at most a group's packs read
/// again, while small packs do not each pay for a sync and a commit.
struct PlacedGroup {
    packs: Vec<String>,
    bytes: u64,
    /// Assembly files written since the last flush, by content hash.
    touched: BTreeSet<String>,
}

const PLACED_GROUP_PACKS: usize = 32;
const PLACED_GROUP_BYTES: u64 = 64 * 1024 * 1024;

impl PlacedGroup {
    fn full(&self) -> bool {
        self.packs.len() >= PLACED_GROUP_PACKS || self.bytes >= PLACED_GROUP_BYTES
    }

    /// Makes what the group placed durable, then marks its packs done. The
    /// apply reads the content store on its own connection, and a marked pack
    /// is not read again.
    fn flush(&mut self, staging_root: &Path, placement: &mut Placement) -> Result<()> {
        let touched = std::mem::take(&mut self.touched);
        if placement.durable() {
            for digest in touched {
                let file = OpenOptions::new()
                    .write(true)
                    .open(assembly_path(staging_root, &digest))
                    .map_err(transient)?;
                file.sync_all().map_err(transient)?;
            }
        }
        placement.commit()?;
        for pack in self.packs.drain(..) {
            fs::write(marker_path(staging_root, &pack)?, b"").map_err(transient)?;
        }
        self.bytes = 0;
        Ok(())
    }
}

/// Places everything one open pack carries for `users`. A pack already
/// fetched is placed whole; cancellation is honored between packs.
fn place_pack(
    staging_root: &Path,
    pack_id: &str,
    plaintext: &Path,
    pending: &[Pending],
    users: &[usize],
    placement: &mut Placement,
    touched: &mut BTreeSet<String>,
) -> Result<()> {
    let mut input = crate::trust_boundary::open_regular_source(plaintext).map_err(corrupt)?;
    let input_length = input.metadata().map_err(corrupt)?.len();
    for &index in users {
        let item = &pending[index];
        if !item.spread() {
            place_whole(Some((&mut input, input_length)), item, placement)?;
            continue;
        }
        let path = assembly_path(staging_root, &item.resolved.digest);
        let metadata = fs::symlink_metadata(&path).map_err(transient)?;
        if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_file() {
            return Err(corrupt("snapshot assembly file is not a regular file"));
        }
        let mut output = OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(transient)?;
        for (chunk, offset) in item.resolved.entry.chunks.iter().zip(&item.offsets) {
            if chunk.pack_id != pack_id {
                continue;
            }
            let bytes = read_chunk(&mut input, input_length, chunk)?;
            output.seek(SeekFrom::Start(*offset)).map_err(transient)?;
            output.write_all(&bytes).map_err(transient)?;
        }
        touched.insert(item.resolved.digest.clone());
    }
    Ok(())
}

/// Proves every assembly file whole and gives it to every entry that shares
/// it: a record's store and an object's file are separate places even when
/// the bytes are the same. One that is not what its catalog names takes the
/// markers of all its entries' packs with it, so the next attempt reads
/// exactly those packs again.
fn finish_assemblies(staging_root: &Path, pending: &[Pending], durability: Durability) -> Result<()> {
    let mut content = Placement::open(staging_root, durability)?;
    let mut shared: BTreeMap<&str, Vec<&Pending>> = BTreeMap::new();
    for item in pending {
        if item.packs.is_empty() {
            place_whole(None, item, &mut content)?;
        } else if item.spread() {
            shared
                .entry(item.resolved.digest.as_str())
                .or_default()
                .push(item);
        }
    }
    for (digest, items) in shared {
        let entry = &items[0].resolved.entry;
        if items
            .iter()
            .any(|item| item.resolved.entry.byte_length != entry.byte_length)
        {
            return Err(corrupt("entries of one content hash differ in length"));
        }
        let record = items.iter().any(|item| item.resolved.destination.is_none());
        // Every object of one hash is written to the same file.
        let file = items
            .iter()
            .find_map(|item| item.resolved.destination.as_deref());
        let path = assembly_path(staging_root, digest);
        let intact = if record {
            // A record goes into the store in memory, so it is proved from
            // the same read.
            let mut source = crate::trust_boundary::open_regular_source(&path).map_err(corrupt)?;
            let mut bytes =
                Vec::with_capacity(usize::try_from(entry.byte_length).map_err(corrupt)?);
            source.read_to_end(&mut bytes).map_err(transient)?;
            #[cfg(test)]
            crate::persistent_store::hash_work::observe("external_restore_record_verify",bytes.len());
            let actual: [u8; 32] = Sha256::digest(&bytes).into();
            (actual == entry.content_sha256 && bytes.len() as u64 == entry.byte_length)
                .then_some(Some(bytes))
        } else {
            verify(&path, entry.byte_length, digest)?.then_some(None)
        };
        let Some(bytes) = intact else {
            let _ = fs::remove_file(&path);
            for pack in items.iter().flat_map(|item| &item.packs) {
                remove_marker(staging_root, pack)?;
            }
            return Err(transient("restored entry integrity failed"));
        };
        if let Some(bytes) = bytes {
            content.content()?.put(digest, &bytes).map_err(corrupt)?;
        }
        match file {
            Some(destination) => {
                publish_verified_with(&path, destination, entry.byte_length, digest, durability)?
            }
            None => fs::remove_file(&path).map_err(transient)?,
        }
    }
    content.commit()?;
    Ok(())
}

/// Reads the packs a plan still needs one at a time, placing what each one
/// carries and releasing it before the next, so at most one pack's
/// ciphertext and plaintext are on disk however many packs the snapshot has
/// or one entry spans. A pack an interrupted attempt already placed is not
/// read again.
#[allow(clippy::too_many_arguments)]
async fn turn_over_packs(
    plan: Vec<ResolvedEntry>,
    packs: &BTreeMap<String, RemoteObject>,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<(
    BTreeMap<String, PreparedRecord>,
    BTreeMap<String, PreparedObject>,
)> {
    turn_over_packs_with(plan, packs, root_key, staging_root, provider, repository, progress, Durability::Durable, cancel).await
}

#[allow(clippy::too_many_arguments)]
async fn turn_over_packs_with(
    plan: Vec<ResolvedEntry>,
    packs: &BTreeMap<String, RemoteObject>,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    durability: Durability,
    cancel: &Cancellation,
) -> Result<(
    BTreeMap<String, PreparedRecord>,
    BTreeMap<String, PreparedObject>,
)> {
    let turnover = Turnover::new(plan, packs)?;
    let (items, bytes) = turnover.remaining();
    progress.plan(items, bytes);
    run_turnover(turnover, root_key, staging_root, provider, repository, progress, durability, cancel).await
}

/// A plan whose packs are known and none of them fetched yet.
struct Turnover {
    records: BTreeMap<String, PreparedRecord>,
    objects: BTreeMap<String, PreparedObject>,
    pending: Vec<Pending>,
    order: Vec<String>,
    users: BTreeMap<String, Vec<usize>>,
    required: Vec<RemoteObject>,
}

impl Turnover {
    fn new(plan: Vec<ResolvedEntry>, packs: &BTreeMap<String, RemoteObject>) -> Result<Self> {
        let mut records = BTreeMap::new();
        let mut objects = BTreeMap::new();
        let mut pending = Vec::new();
        for resolved in plan {
            match resolved.source.clone() {
                Some(source) => insert_prepared(
                    resolved.entry,
                    resolved.digest,
                    source,
                    &mut records,
                    &mut objects,
                )?,
                None => pending.push(Pending::new(resolved)?),
            }
        }
        // Packs in the order the plan first needs them, each with the entries it
        // carries. A pack no remaining chunk names is never opened.
        let mut order: Vec<String> = Vec::new();
        let mut users: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (index, item) in pending.iter().enumerate() {
            for chunk in &item.resolved.entry.chunks {
                let entries = users.entry(chunk.pack_id.clone()).or_insert_with(|| {
                    order.push(chunk.pack_id.clone());
                    Vec::new()
                });
                if entries.last() != Some(&index) {
                    entries.push(index);
                }
            }
        }
        let mut required = Vec::with_capacity(order.len());
        for id in &order {
            required.push(
                packs
                    .get(id)
                    .cloned()
                    .ok_or_else(|| corrupt("catalog pack is missing"))?,
            );
        }
        Ok(Self { records, objects, pending, order, users, required })
    }

    /// The packs still to fetch, and their bytes. A pack an interrupted
    /// attempt already placed is among them and counts as soon as it is found.
    fn remaining(&self) -> (u64, u64) {
        (
            self.required.len() as u64,
            self.required
                .iter()
                .fold(0u64, |sum, pack| sum.saturating_add(pack.receipt.byte_length)),
        )
    }
}

/// Fetches the packs a turnover planned, counting each one in `progress`.
#[allow(clippy::too_many_arguments)]
async fn run_turnover(
    turnover: Turnover,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    durability: Durability,
    cancel: &Cancellation,
) -> Result<(
    BTreeMap<String, PreparedRecord>,
    BTreeMap<String, PreparedObject>,
)> {
    let Turnover { mut records, mut objects, pending, order, mut users, required } = turnover;
    let pending = std::sync::Arc::new(pending);
    let prepared_root = staging_root.to_path_buf();
    let prepared_pending = pending.clone();
    let cpu = cpu_permit().await?;
    spawn_blocking(move || prepare_turnover(&prepared_root, &prepared_pending))
        .await
        .map_err(transient)??;
    drop(cpu);
    let opened_root = staging_root.to_path_buf();
    let mut content = Some(
        spawn_blocking(move || Placement::open(&opened_root, durability))
            .await
            .map_err(transient)??,
    );
    let mut group = Some(PlacedGroup {
        packs: Vec::new(),
        bytes: 0,
        touched: BTreeSet::new(),
    });
    let turned = async {
        let mut remaining = order.iter().zip(&required).peekable();
        while remaining.peek().is_some() {
            let mut batch = Vec::new();
            for (id, pack) in remaining.by_ref().take(provider.transfer_concurrency()) {
                cancel.check()?;
                if marker_path(staging_root, id)?.is_file() {
                    progress.completed(pack.receipt.byte_length);
                } else {
                    batch.push((id, pack));
                }
            }
            let objects: Vec<_> = batch.iter().map(|(_, pack)| *pack).collect();
            let mut downloads = stage_ciphertexts(&objects, staging_root, provider, repository, durability, Some(progress), cancel)?;
            let mut identities = batch.iter();
            while let Some((pack, ciphertext)) = downloads.next().await {
                let id = identities.next().ok_or_else(|| corrupt("download identity missing"))?.0;
                let plaintext = open_staged_object(pack, root_key, staging_root, ciphertext?, durability, None).await?;
                #[cfg(test)]
                record_turnover(staging_root, &required);
                // Decrypting was all the ciphertext was for.
                match fs::remove_file(
                    staging_root
                        .join("downloads")
                        .join(format!("{}.cipher", pack.ciphertext_sha256)),
                ) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(transient(error));
                    }
                    _ => {}
                }
                let place_root = staging_root.to_path_buf();
                let place_id = id.clone();
                let place_pending = pending.clone();
                let place_users = users.remove(id).unwrap_or_default();
                let mut place_content = content.take().ok_or_else(|| transient("content store"))?;
                let mut place_group = group.take().ok_or_else(|| transient("placed group"))?;
                let plaintext_length = pack.plaintext_length;
                let cpu = cpu_permit().await?;
                let (returned_content, returned_group, placed) = spawn_blocking(move || {
                    let placed = place_pack(
                        &place_root,
                        &place_id,
                        &plaintext,
                        &place_pending,
                        &place_users,
                        &mut place_content,
                        &mut place_group.touched,
                    )
                    .and_then(|()| {
                        place_group.packs.push(place_id);
                        place_group.bytes = place_group.bytes.saturating_add(plaintext_length);
                        if place_group.full() {
                            place_group.flush(&place_root, &mut place_content)?;
                        }
                        Ok(())
                    });
                    (place_content, place_group, placed)
                })
                .await
                .map_err(transient)?;
                drop(cpu);
                content = Some(returned_content);
                group = Some(returned_group);
                placed?;
                discard_object(staging_root, pack);
                #[cfg(test)]
                record_turnover(staging_root, &required);
                progress.completed(0);
            }
        }
        Ok(())
    }
    .await;
    // Packs placed before a cancellation or a failure are kept for the next
    // attempt, which is what makes it resume rather than restart.
    if let (Some(mut flush_content), Some(mut flush_group)) = (content.take(), group.take()) {
        let flush_root = staging_root.to_path_buf();
        let flushed = spawn_blocking(move || {
            flush_group.flush(&flush_root, &mut flush_content)
        })
        .await
        .map_err(transient)?;
        turned?;
        flushed?;
    } else {
        turned?;
    }
    let finish_root = staging_root.to_path_buf();
    let finish_pending = pending.clone();
    let cpu = cpu_permit().await?;
    spawn_blocking(move || finish_assemblies(&finish_root, &finish_pending, durability))
        .await
        .map_err(transient)??;
    drop(cpu);
    // Every entry is where it belongs, so nothing an interrupted attempt
    // would resume from is left to keep.
    let _ = fs::remove_dir_all(staging_root.join("assembly"));
    let _ = fs::remove_dir_all(staging_root.join("turnover"));
    let pending = std::sync::Arc::try_unwrap(pending)
        .map_err(|_| transient("snapshot placement is still running"))?;
    for item in pending {
        let ResolvedEntry {
            entry,
            digest,
            destination,
            ..
        } = item.resolved;
        let source = match destination {
            None => ObjectSource::Captured(digest.clone()),
            Some(destination) => ObjectSource::File(destination),
        };
        insert_prepared(entry, digest, source, &mut records, &mut objects)?;
    }
    Ok((records, objects))
}

/// File one materialized entry under the kind its catalog gave it.
fn insert_prepared(
    entry: CompleteEntry,
    digest: String,
    source: ObjectSource,
    records: &mut BTreeMap<String, PreparedRecord>,
    objects: &mut BTreeMap<String, PreparedObject>,
) -> Result<()> {
    match entry.kind {
        wire::CatalogEntryKind::Record => {
            if records
                .insert(
                    entry.key.clone(),
                    PreparedRecord {
                        key: entry.key,
                        content_hash: digest,
                        byte_length: entry.byte_length,
                        source,
                    },
                )
                .is_some()
            {
                return Err(corrupt("duplicate logical record key"));
            }
        }
        wire::CatalogEntryKind::Object => {
            let prepared = PreparedObject {
                content_hash: digest.clone(),
                byte_length: entry.byte_length,
                source,
            };
            if let Some(old) = objects.insert(digest, prepared.clone()) {
                if old.byte_length != prepared.byte_length {
                    return Err(corrupt("conflicting content object"));
                }
            }
        }
        wire::CatalogEntryKind::SectionEntry | wire::CatalogEntryKind::SectionObject => {
            return Err(corrupt("section entry in library catalog"));
        }
    }
    Ok(())
}

fn validate_section_lengths(entries: &[CompleteEntry]) -> Result<()> {
    use risunest_external_storage_format::section::{MAX_SECTION_ENTRY_BYTES, MAX_SECTION_OBJECT_BYTES};
    for entry in entries {
        let limit = match entry.kind {
            wire::CatalogEntryKind::SectionEntry => MAX_SECTION_ENTRY_BYTES,
            wire::CatalogEntryKind::SectionObject => MAX_SECTION_OBJECT_BYTES,
            _ => return Err(corrupt("non-section entry in section catalog")),
        };
        if entry.byte_length > limit as u64 {
            return Err(corrupt("section source exceeds its codec limit"));
        }
    }
    Ok(())
}

fn assemble(entry: &CompleteEntry, packs: &BTreeMap<String, PathBuf>) -> Result<Vec<u8>> {
    validate_section_lengths(std::slice::from_ref(entry))?;
    let mut bytes = Vec::with_capacity(usize::try_from(entry.byte_length).map_err(corrupt)?);
    let mut digest = Sha256::new();
    #[cfg(test)]
    crate::persistent_store::hash_work::begin("external_restore_section_assembly");
    for chunk in &entry.chunks {
        let pack_path = packs
            .get(&chunk.pack_id)
            .ok_or_else(|| corrupt("catalog pack is missing"))?;
        let mut input = crate::trust_boundary::open_regular_source(pack_path).map_err(corrupt)?;
        let input_length = input.metadata().map_err(corrupt)?.len();
        let decoded = read_chunk(&mut input, input_length, chunk)?;
        #[cfg(test)]
        crate::persistent_store::hash_work::update("external_restore_section_assembly",decoded.len());
        digest.update(&decoded);
        bytes.extend_from_slice(&decoded);
    }
    let actual: [u8; 32] = digest.finalize().into();
    if actual != entry.content_sha256 || bytes.len() as u64 != entry.byte_length {
        return Err(corrupt("received section entry integrity failed"));
    }
    Ok(bytes)
}

fn materialize_section_entries(
    entries: Vec<CompleteEntry>,
    packs: &BTreeMap<String, PathBuf>,
    staging_root: &Path,
    cancel: &Cancellation,
) -> Result<Vec<SectionSource>> {
    let spool = staging_root.join("section-spool");
    ensure_directory(&spool)?;
    let mut sources = Vec::with_capacity(entries.len());
    for entry in entries {
        cancel.check()?;
        let bytes = assemble(&entry, packs)?;
        let digest = hex::encode(entry.content_sha256);
        let destination = spool.join(&digest);
        if !verify(&destination, entry.byte_length, &digest)? {
            let partial = spool.join(format!(".section-{}.partial", uuid::Uuid::new_v4()));
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&partial)
                .map_err(transient)?;
            output.write_all(&bytes).map_err(transient)?;
            output.sync_all().map_err(transient)?;
            drop(output);
            if !verify(&partial, entry.byte_length, &digest)? {
                return Err(corrupt("received section spool integrity failed"));
            }
            publish_verified(&partial, &destination, entry.byte_length, &digest)?;
        }
        drop(bytes);
        sources.push(SectionSource {
            kind: entry.kind,
            key: entry.key,
            content_sha256: digest,
            byte_length: entry.byte_length,
            path: destination,
            offset: None,
        });
    }
    sources.sort_by(|a, b| (a.kind as u8, &a.key).cmp(&(b.kind as u8, &b.key)));
    Ok(sources)
}

/// Downloads only the sections the caller asked for. A section left out of
/// `wanted` is never read, so its values never reach this device.
pub(crate) async fn download_sections(
    snapshot: &RemoteObject,
    wanted: &BTreeSet<String>,
    staging_root: &Path,
    root_key: &[u8; 32],
    record_into: Option<&Path>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<Vec<CapturedSection>> {
    let mut prepared = Vec::new();
    for (id, reference) in wanted_sections(snapshot, wanted, staging_root, root_key, provider, repository, cancel).await? {
        let section = walk_section(
            &id, reference, snapshot, staging_root, root_key, record_into, provider, repository, progress, cancel,
        )
        .await?;
        let (items, bytes) = pack_totals(&section.packs);
        progress.plan(items, bytes);
        prepared.push(fetch_section(section, staging_root, root_key, provider, repository, progress, cancel).await?);
    }
    Ok(prepared)
}

/// The wanted sections of a snapshot with their catalogs read and none of
/// their packs fetched yet, so their packs can be planned with the rest of a
/// restore before any of them is.
pub(crate) struct SectionsWalk {
    sections: Vec<WalkedSection>,
}

struct WalkedSection {
    reference: wire::SectionSnapshotRef,
    complete: Vec<CompleteEntry>,
    packs: BTreeMap<String, RemoteObject>,
}

/// Reads the catalogs of the sections `download_sections` would download,
/// counting each node it fetches in `walked`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn walk_sections(
    snapshot: &RemoteObject,
    wanted: &BTreeSet<String>,
    staging_root: &Path,
    root_key: &[u8; 32],
    record_into: Option<&Path>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    walked: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<SectionsWalk> {
    let mut sections = Vec::new();
    for (id, reference) in wanted_sections(snapshot, wanted, staging_root, root_key, provider, repository, cancel).await? {
        sections.push(
            walk_section(&id, reference, snapshot, staging_root, root_key, record_into, provider, repository, walked, cancel)
                .await?,
        );
    }
    Ok(SectionsWalk { sections })
}

impl SectionsWalk {
    /// The packs still to fetch, and their bytes.
    pub(crate) fn remaining(&self) -> (u64, u64) {
        self.sections.iter().fold((0u64, 0u64), |(items, bytes), section| {
            let (more_items, more_bytes) = pack_totals(&section.packs);
            (items.saturating_add(more_items), bytes.saturating_add(more_bytes))
        })
    }

    /// Fetches what `remaining` planned, counting each pack in `progress`.
    pub(crate) async fn fetch(
        self,
        staging_root: &Path,
        root_key: &[u8; 32],
        provider: &dyn Provider,
        repository: &RepositoryHandle,
        progress: &PhaseProgress,
        cancel: &Cancellation,
    ) -> Result<Vec<CapturedSection>> {
        let mut prepared = Vec::with_capacity(self.sections.len());
        for section in self.sections {
            prepared.push(fetch_section(section, staging_root, root_key, provider, repository, progress, cancel).await?);
        }
        Ok(prepared)
    }
}

/// The section references of `snapshot` the caller asked for. A section left
/// out of `wanted` is never read.
async fn wanted_sections(
    snapshot: &RemoteObject,
    wanted: &BTreeSet<String>,
    staging_root: &Path,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Vec<(String, wire::SectionSnapshotRef)>> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    ensure_directory(staging_root)?;
    let root_path = open_object(
        snapshot,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    let root_bytes = read_bytes(&root_path, wire::MAX_METADATA_BYTES)?;
    let wire_role = match snapshot.role {
        ObjectRole::SyncState => wire::ObjectRole::SyncState,
        ObjectRole::BackupBundle => wire::ObjectRole::BackupBundle,
        _ => return Err(corrupt("snapshot root role")),
    };
    let document =
        super::control::SnapshotView::read(&root_bytes, wire_role, &snapshot.repository_id)?;
    Ok(document
        .sections
        .into_iter()
        .filter(|(id, _)| wanted.contains(id))
        .collect())
}

#[allow(clippy::too_many_arguments)]
async fn walk_section(
    id: &str,
    reference: wire::SectionSnapshotRef,
    snapshot: &RemoteObject,
    staging_root: &Path,
    root_key: &[u8; 32],
    record_into: Option<&Path>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<WalkedSection> {
    let entries_root = RemoteObject::from_stored(&reference.entries_root, repository)?;
    if entries_root.repository_id != snapshot.repository_id {
        return Err(corrupt("section catalog repository differs"));
    }
    let (complete, packs, nodes) = read_catalog_with(
        &entries_root,
        wire::CatalogKind::Section,
        root_key,
        staging_root,
        provider,
        repository,
        progress,
        cancel,
    )
    .await?;
    record_read(
        record_into,
        &super::packaging::CatalogRoot::Section(id.to_owned()),
        &entries_root,
        &complete,
        &packs,
        &nodes,
        repository,
    );
    validate_section_lengths(&complete)?;
    Ok(WalkedSection { reference, complete, packs })
}

async fn fetch_section(
    section: WalkedSection,
    staging_root: &Path,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<CapturedSection> {
    let WalkedSection { reference, complete, packs } = section;
    let pack_paths =
        open_packs(&packs, root_key, staging_root, provider, repository, progress, cancel)
            .await?;
    let cpu = cpu_permit().await?;
    let section_cancel = cancel.clone();
    let section_staging = staging_root.to_path_buf();
    let sources = spawn_blocking(move || {
        materialize_section_entries(complete, &pack_paths, &section_staging, &section_cancel)
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    Ok(CapturedSection {
        kind: reference.kind,
        generation: reference.generation,
        gc_floor: reference.gc_floor,
        max_write_clock: reference.max_write_clock,
        content_fingerprint: reference.content_fingerprint,
        sources,
    })
}

pub(crate) async fn download_checkpoint_data(
    root: &RemoteObject, staging_root: &Path, root_key: &[u8;32], record_into: Option<&Path>,
    provider: &dyn Provider, repository: &RepositoryHandle, progress: &PhaseProgress, cancel: &Cancellation,
) -> Result<(Vec<PreparedRecord>, Vec<PreparedObject>)> {
    let turnover=walk_checkpoint_data(root,staging_root,root_key,record_into,provider,repository,progress,cancel).await?;
    let (items,bytes)=turnover.remaining();
    progress.plan(items,bytes);
    let (records,objects)=run_turnover(turnover,root_key,staging_root,provider,repository,progress,Durability::Durable,cancel).await?;
    Ok((records.into_values().collect(),objects.into_values().collect()))
}

/// Reads a record catalog, counting each node it fetches in `progress`, and
/// works out which of its packs this staging directory still needs.
async fn walk_checkpoint_data(
    root: &RemoteObject, staging_root: &Path, root_key: &[u8;32], record_into: Option<&Path>,
    provider: &dyn Provider, repository: &RepositoryHandle, progress: &PhaseProgress, cancel: &Cancellation,
) -> Result<Turnover> {
    ensure_directory(staging_root)?;
    let (entries,packs,nodes) = read_catalog_with(root,wire::CatalogKind::Records,root_key,staging_root,provider,repository,progress,cancel).await?;
    record_read(record_into,&super::packaging::CatalogRoot::Records,root,&entries,&packs,&nodes,repository);
    let content=content_store(staging_root)?;
    let plan=resolve_entries(entries,staging_root,&content,None,cancel)?;
    Turnover::new(plan,&packs)
}

pub(crate) async fn download_control_catalogs(
    catalogs:&[wire::StoredObject],staging_root:&Path,root_key:&[u8;32],record_into:Option<&Path>,
    provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation,
) -> Result<Vec<PreparedObject>> {
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    let mut objects=BTreeMap::new();let mut roots=BTreeMap::new();
    for (index,catalog) in catalogs.iter().enumerate() {
        cancel.check()?;
        if catalog.header.repository_id!=format_repository_id || catalog.header.role!=wire::ObjectRole::Catalog {
            return Err(corrupt("control catalog scope differs"));
        }
        if let Some(previous)=roots.insert(catalog.header.object_id.clone(),catalog.clone()) {
            if previous!=*catalog {return Err(corrupt("control catalog identity differs"));}
            continue;
        }
        let remote=RemoteObject::from_stored(catalog,repository)?;
        let stage=staging_root.join(index.to_string());
        let (records,controls)=download_checkpoint_data(&remote,&stage,root_key,record_into,provider,repository,&PhaseProgress::silent(),cancel).await?;
        if !records.is_empty() {return Err(corrupt("control catalog contains unit records"));}
        for mut control in controls {
            if let ObjectSource::Captured(hash)=&control.source {
                let content=content_store(&stage)?;let mut reader=content.open_body(hash).map_err(transient)?;
                let path=stage.join(format!("{hash}.control"));let mut file=std::fs::File::create(&path).map_err(transient)?;
                if std::io::copy(&mut reader,&mut file).map_err(transient)?!=control.byte_length {return Err(corrupt("control file length differs"));}
                file.sync_all().map_err(transient)?;control.source=ObjectSource::File(path);
            }
            if let Some(previous)=objects.insert(control.content_hash.clone(),control.clone()) {
                if previous.byte_length!=control.byte_length {return Err(corrupt("control body length differs"));}
            }
        }
    }
    Ok(objects.into_values().collect())
}

pub(crate) async fn revalidate_catalog(
    catalog:&wire::StoredObject,kind:wire::CatalogKind,root_key:&[u8;32],provider:&dyn Provider,
    repository:&RepositoryHandle,scratch_root:&Path,cancel:&Cancellation,
) -> Result<()> {
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    if !matches!(kind,wire::CatalogKind::Records|wire::CatalogKind::Assets)
        || catalog.header.repository_id!=format_repository_id || catalog.header.role!=wire::ObjectRole::Catalog {return Err(corrupt("dependency catalog scope differs"));}
    let stage=super::leftovers::managed_scratch(scratch_root,"catalog-check-")?;
    let remote=RemoteObject::from_stored(catalog,repository)?;
    let (entries,packs,_)=read_catalog(&remote,kind,root_key,stage.path(),provider,repository,cancel).await?;
    if entries.iter().any(|entry|entry.kind!=wire::CatalogEntryKind::Object) {return Err(corrupt("dependency catalog contains unit records"));}
    for pack in packs.values() {
        cancel.check()?;
        let body=tempfile::tempdir_in(stage.path()).map_err(transient)?;
        open_object(pack,root_key,body.path(),provider,repository,cancel).await?;
    }
    Ok(())
}

pub(crate) async fn admit_data_catalogs(
    catalogs:&[wire::StoredObject],store:&mut crate::persistent_store::PersistentStore,target:&str,root_key:&[u8;32],record_into:Option<&Path>,
    provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation,
) -> Result<Vec<String>> {
    if catalogs.is_empty() {return Ok(Vec::new());}
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    let stage=super::leftovers::managed_scratch(store.repository_root(),"data-catalogs-")?;
    let mut hashes=BTreeSet::new();let mut roots=BTreeMap::new();
    for (index,catalog) in catalogs.iter().enumerate() {
        cancel.check()?;
        if catalog.header.repository_id!=format_repository_id || catalog.header.role!=wire::ObjectRole::Catalog {return Err(corrupt("control catalog scope differs"));}
        if let Some(previous)=roots.insert(catalog.header.object_id.clone(),catalog.clone()) {
            if previous!=*catalog {return Err(corrupt("control catalog identity differs"));}continue;
        }
        if let Some(known)=store.external_lww_verified_data_catalog(target,catalog).map_err(corrupt)? {
            let mut complete=true;
            for hash in &known {if !store.lww_keep_verified_object(hash).map_err(corrupt)? {complete=false;break;}}
            if complete {hashes.extend(known);continue;}
        }
        let remote=RemoteObject::from_stored(catalog,repository)?;
        let directory=stage.path().join(index.to_string());ensure_directory(&directory)?;
        let (entries,packs,nodes)=read_catalog(&remote,wire::CatalogKind::Records,root_key,&directory,provider,repository,cancel).await?;
        record_read(record_into,&super::packaging::CatalogRoot::Records,&remote,&entries,&packs,&nodes,repository);
        let mut required=BTreeSet::new();let mut missing=Vec::new();
        for entry in entries {
            if entry.kind!=wire::CatalogEntryKind::Object {return Err(corrupt("control catalog contains unit records"));}
            let hash=hex::encode(entry.content_sha256);required.insert(hash.clone());
            if let Some(size)=store.external_lww_verified_control_size(&hash).map_err(corrupt)? {
                if size!=entry.byte_length {return Err(corrupt("known control body length differs"));}
            } else {missing.push(entry);}
        }
        let content=content_store(&directory)?;
        let plan=resolve_entries(missing,&directory,&content,None,cancel)?;
        let (records,controls)=turn_over_packs(plan,&packs,root_key,&directory,provider,repository,&PhaseProgress::silent(),cancel).await?;
        if !records.is_empty() {return Err(corrupt("control catalog contains unit records"));}
        for control in controls.into_values() {
            cancel.check()?;
            let ObjectSource::File(path)=control.source else {return Err(corrupt("control file is unavailable"));};
            let mut input = crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
            if crate::persistent_store::external_capture::verified_string_control(&mut input, &control.content_hash, control.byte_length, "native_data_catalog_control_identity").map_err(corrupt)? {
                store.lww_put_object_reader(&control.content_hash, &mut input, control.byte_length).map_err(corrupt)?;
                continue;
            }
            let bytes=read_bytes(&path,usize::try_from(control.byte_length).map_err(corrupt)?)?;
            if bytes.len() as u64!=control.byte_length || crate::persistent_store::external_capture::hash_backup_body(&bytes,"native_data_catalog_control_identity")!=control.content_hash {return Err(corrupt("control body identity differs"));}
            if bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES {crate::persistent_store::external_capture::verified_oversized_control(&bytes).map_err(corrupt)?;}
            store.lww_put_object(&control.content_hash,&bytes).map_err(corrupt)?;
        }
        let required=required.into_iter().collect::<Vec<_>>();
        store.external_lww_witness_data_catalog(target,catalog,&required).map_err(corrupt)?;
        hashes.extend(required);
    }
    Ok(hashes.into_iter().collect())
}

#[cfg(test)]
pub(crate) async fn download_packed_body(hash:&str,length:u64,chunks:Vec<wire::StoredChunk>,packs:Vec<wire::StoredObject>,stage:&Path,key:&[u8;32],provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation)->Result<Vec<u8>> {
    let path=download_packed_body_file(hash,length,chunks,packs,stage,key,provider,repository,cancel).await?;
    std::fs::read(path).map_err(transient)
}
pub(crate) async fn download_packed_body_files<O:std::borrow::Borrow<wire::StoredObject>>(
    sources:&[super::lww_residency::PackedSource<O>],staging_root:&Path,root_key:&[u8;32],
    provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation,
)->Result<BTreeMap<String,PathBuf>> {
    download_packed_body_files_with(sources,staging_root,root_key,provider,repository,Durability::Durable,cancel).await
}

/// Downloads bodies into staging that is discarded with the work that asked
/// for them, so nothing here is synced.
pub(crate) async fn download_packed_body_files_scratch<O:std::borrow::Borrow<wire::StoredObject>>(
    sources:&[super::lww_residency::PackedSource<O>],staging_root:&Path,root_key:&[u8;32],
    provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation,
)->Result<BTreeMap<String,PathBuf>> {
    download_packed_body_files_with(sources,staging_root,root_key,provider,repository,Durability::Scratch,cancel).await
}

async fn download_packed_body_files_with<O:std::borrow::Borrow<wire::StoredObject>>(
    sources:&[super::lww_residency::PackedSource<O>],staging_root:&Path,root_key:&[u8;32],
    provider:&dyn Provider,repository:&RepositoryHandle,durability:Durability,cancel:&Cancellation,
)->Result<BTreeMap<String,PathBuf>> {
    ensure_directory(staging_root)?;
    let mut packs=BTreeMap::new();let mut entries=Vec::new();let mut hashes=BTreeSet::new();
    for source in sources {
        cancel.check()?;super::lww_residency::validate_packed_source(source,repository)?;
        if !hashes.insert(source.hash.clone()) {return Err(corrupt("duplicate body"));}
        for stored in &source.packs {
            let pack=RemoteObject::from_stored(stored.borrow(),repository)?;
            if let Some(previous)=packs.insert(pack.object_id.clone(),pack.clone()) {
                if previous!=pack {return Err(corrupt("conflicting pack"));}
            }
        }
        entries.push(CompleteEntry {kind:wire::CatalogEntryKind::Object,key:format!("object/{}",source.hash),content_sha256:hex::decode(&source.hash).map_err(corrupt)?.try_into().map_err(|_|corrupt("body hash"))?,byte_length:source.byte_length,chunks:source.chunks.clone()});
    }
    let content=(durability==Durability::Durable).then(||content_store(staging_root)).transpose()?;
    let mut plan=resolve_entries_in(entries,staging_root,content.as_ref(),None,cancel)?;
    for resolved in &mut plan {
        let destination=staging_root.join(format!("{}.payload",resolved.digest));
        if resolved.source.is_none() && verify(&destination,resolved.entry.byte_length,&resolved.digest)? {
            resolved.source=Some(ObjectSource::File(destination.clone()));
        }
        resolved.destination=Some(destination);
    }
    let (_,objects)=turn_over_packs_with(plan,&packs,root_key,staging_root,provider,repository,&PhaseProgress::silent(),durability,cancel).await?;
    objects.into_iter().map(|(hash,object)|match object.source {ObjectSource::File(path)=>Ok((hash,path)),_=>Err(corrupt("body source"))}).collect()
}

pub(crate) async fn download_packed_body_file(
    hash:&str, length:u64, chunks:Vec<wire::StoredChunk>, stored_packs:Vec<wire::StoredObject>, staging_root:&Path, root_key:&[u8;32],
    provider:&dyn Provider, repository:&RepositoryHandle,cancel:&Cancellation,
) -> Result<PathBuf> {
    download_packed_body_file_observed(hash,length,chunks,stored_packs,staging_root,root_key,provider,repository,&PhaseProgress::silent(),cancel).await
}
pub(crate) async fn download_packed_body_file_observed(
    hash:&str, length:u64, chunks:Vec<wire::StoredChunk>, stored_packs:Vec<wire::StoredObject>, staging_root:&Path, root_key:&[u8;32],
    provider:&dyn Provider, repository:&RepositoryHandle,progress:&PhaseProgress,cancel:&Cancellation,
) -> Result<PathBuf> {
    ensure_directory(staging_root)?;
    let mut packs=BTreeMap::new();
    for stored in &stored_packs { let pack=RemoteObject::from_stored(stored,repository)?; packs.insert(pack.object_id.clone(),pack); }
    let entry=CompleteEntry {kind:wire::CatalogEntryKind::Object,key:format!("object/{hash}"),content_sha256:hex::decode(hash).map_err(corrupt)?.try_into().map_err(|_|corrupt("body hash"))?,byte_length:length,chunks};
    // Every caller reads the body out of throwaway staging, so it is placed as scratch.
    let plan=resolve_entries_in(vec![entry],staging_root,None,None,cancel)?;
    let (_,objects)=turn_over_packs_with(plan,&packs,root_key,staging_root,provider,repository,progress,Durability::Scratch,cancel).await?;
    progress.flush();
    let body=objects.get(hash).ok_or_else(||corrupt("body missing"))?;
    match &body.source { ObjectSource::File(path)=>Ok(path.clone()), _=>Err(corrupt("body source")) }
}

pub(crate) struct RestoreBodyPlan {
    db: rusqlite::Connection,
    directory: PathBuf,
    staged: std::collections::VecDeque<(RemoteObject, Result<Option<PathBuf>>)>,
}
impl RestoreBodyPlan {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        ensure_directory(directory)?;
        let directory=directory.join("pack-plan");
        ensure_directory(&directory)?;
        ensure_directory(&directory.join("assembly"))?;
        let path=directory.join("plan.sqlite");
        for suffix in ["","-wal","-shm"] {
            let path=PathBuf::from(format!("{}{suffix}",path.display()));
            if path.exists() {crate::trust_boundary::open_regular_source(&path).map_err(transient)?;}
        }
        let db=crate::sqlite_open::open(path).map_err(transient)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
            CREATE TABLE IF NOT EXISTS bodies(hash TEXT PRIMARY KEY,bytes INTEGER NOT NULL,remaining INTEGER NOT NULL,priority INTEGER NOT NULL,source TEXT NOT NULL,settled INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS ready_bodies ON bodies(settled,remaining,priority,hash);
            CREATE TABLE IF NOT EXISTS packs(id TEXT PRIMARY KEY,body TEXT NOT NULL,priority INTEGER NOT NULL,done INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS next_pack ON packs(done,priority,id);
            CREATE TABLE IF NOT EXISTS chunks(pack TEXT NOT NULL,hash TEXT NOT NULL,ordinal INTEGER NOT NULL,offset INTEGER NOT NULL,body TEXT NOT NULL,done INTEGER NOT NULL,PRIMARY KEY(hash,ordinal));
            CREATE INDEX IF NOT EXISTS pack_chunks ON chunks(pack,done,hash,ordinal);
            CREATE TABLE IF NOT EXISTS seen(hash TEXT PRIMARY KEY); BEGIN IMMEDIATE;
            DELETE FROM seen").map_err(transient)?;
        Ok(Self {db,directory,staged:Default::default()})
    }
    pub(crate) fn push<O:std::borrow::Borrow<wire::StoredObject>>(&self,source:&super::lww_residency::PackedSource<O>,priority:bool,repository:&RepositoryHandle)->Result<()> {
        super::lww_residency::validate_packed_source(source,repository)?;
        let priority=if priority {0} else {1};
        let mut digest=Sha256::new();
        digest.update(source.hash.as_bytes());digest.update(source.byte_length.to_le_bytes());
        for chunk in &source.chunks {digest.update(serde_json::to_vec(chunk).map_err(corrupt)?);}
        let identity=hex::encode(digest.finalize());
        let previous:Option<(i64,String)>=self.db.query_row("SELECT bytes,source FROM bodies WHERE hash=?1",[&source.hash],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(transient)?;
        if let Some((size,digest))=&previous {
            if u64::try_from(*size).map_err(corrupt)?!=source.byte_length || digest!=&identity {return Err(corrupt("restore body source changed"));}
        }
        self.db.execute("INSERT INTO seen VALUES(?1)",[&source.hash]).map_err(corrupt)?;
        self.db.execute("INSERT INTO bodies VALUES(?1,?2,?3,?4,?5,0) ON CONFLICT(hash) DO UPDATE SET priority=excluded.priority,settled=0",
            rusqlite::params![source.hash,i64::try_from(source.byte_length).map_err(corrupt)?,source.chunks.len() as i64,priority,identity]).map_err(corrupt)?;
        for pack in &source.packs {
            if !source.chunks.iter().any(|chunk|chunk.pack_id==pack.borrow().header.object_id) {continue;}
            let remote=RemoteObject::from_stored(pack.borrow(),repository)?;
            let encoded=serde_json::to_string(&remote).map_err(corrupt)?;
            let previous:Option<String>=self.db.query_row("SELECT body FROM packs WHERE id=?1",[&remote.object_id],|row|row.get(0)).optional().map_err(transient)?;
            if previous.as_ref().is_some_and(|body|body!=&encoded) {return Err(corrupt("conflicting restore pack"));}
            self.db.execute("INSERT INTO packs VALUES(?1,?2,?3,0) ON CONFLICT(id) DO UPDATE SET priority=min(priority,excluded.priority)",
                rusqlite::params![remote.object_id,encoded,priority]).map_err(transient)?;
        }
        if previous.is_none() {
            let mut offset=0u64;
            for (ordinal,chunk) in source.chunks.iter().enumerate() {
                self.db.execute("INSERT INTO chunks VALUES(?1,?2,?3,?4,?5,0)",rusqlite::params![chunk.pack_id,source.hash,ordinal as i64,i64::try_from(offset).map_err(corrupt)?,serde_json::to_string(chunk).map_err(corrupt)?]).map_err(transient)?;
                self.db.execute("UPDATE packs SET done=0 WHERE id=?1",[&chunk.pack_id]).map_err(transient)?;
                offset=offset.checked_add(chunk.plaintext_length).ok_or_else(||corrupt("restore body length"))?;
            }
        }
        let path=self.body_path(&source.hash);
        let intact=match fs::symlink_metadata(&path) {
            Ok(metadata)=>metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata) && metadata.len()==source.byte_length,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>false,
            Err(error)=>return Err(transient(error)),
        };
        if !intact {
            self.reset_body(&source.hash)?;
            match fs::remove_file(&path) {Ok(())=>{},Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(transient(error))}
            let file=OpenOptions::new().create_new(true).write(true).open(&path).map_err(transient)?;
            file.set_len(source.byte_length).map_err(transient)?;
            file.sync_all().map_err(transient)?;
        }
        Ok(())
    }
    fn reset_body(&self,hash:&str)->Result<()> {
        self.db.execute("UPDATE packs SET done=0 WHERE id IN(SELECT pack FROM chunks WHERE hash=?1)",[hash]).map_err(transient)?;
        self.db.execute("UPDATE chunks SET done=0 WHERE hash=?1",[hash]).map_err(transient)?;
        self.db.execute("UPDATE bodies SET remaining=(SELECT count(*) FROM chunks WHERE hash=?1),settled=0 WHERE hash=?1",[hash]).map_err(transient)?;
        Ok(())
    }
    pub(crate) fn seal(&self)->Result<()> {
        self.db.execute_batch("DELETE FROM chunks WHERE hash NOT IN(SELECT hash FROM seen);
            DELETE FROM bodies WHERE hash NOT IN(SELECT hash FROM seen);
            DELETE FROM packs WHERE id NOT IN(SELECT pack FROM chunks)").map_err(transient)?;
        crate::trust_boundary::sync_directory(&self.directory.join("assembly")).map_err(transient)?;
        self.db.execute_batch("COMMIT").map_err(transient)?;
        crate::trust_boundary::sync_directory(&self.directory).map_err(transient)?;
        Ok(())
    }
    fn body_path(&self,hash:&str)->PathBuf {self.directory.join("assembly").join(format!("{hash}.stage"))}
    pub(crate) fn ready(&self)->Result<Option<(String,PathBuf)>> {
        let ready:Option<(String,i64)>=self.db.query_row("SELECT hash,bytes FROM bodies WHERE settled=0 AND remaining=0 ORDER BY priority,hash LIMIT 1",[],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(transient)?;
        ready.map(|(hash,length)| {
            let length=u64::try_from(length).map_err(corrupt)?;
            let path=self.body_path(&hash);
            if !verify(&path,length,&hash)? {
                self.db.execute_batch("BEGIN IMMEDIATE").map_err(transient)?;
                self.reset_body(&hash)?;
                self.db.execute_batch("COMMIT").map_err(transient)?;
                return Err(transient("restore body integrity"));
            }
            Ok((hash,path))
        }).transpose()
    }
    fn settled_retained(&self,hash:&str)->Result<()> {
        self.db.execute("UPDATE bodies SET settled=1 WHERE hash=?1 AND remaining=0",[hash]).map_err(transient)?;
        Ok(())
    }
    pub(crate) fn settled(&self,hash:&str)->Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE").map_err(transient)?;
        self.db.execute("DELETE FROM chunks WHERE hash=?1 AND EXISTS(SELECT 1 FROM bodies WHERE hash=?1 AND remaining=0)",[hash]).map_err(transient)?;
        self.db.execute("DELETE FROM bodies WHERE hash=?1 AND remaining=0",[hash]).map_err(transient)?;
        self.db.execute_batch("COMMIT").map_err(transient)?;
        match fs::remove_file(self.body_path(hash)) {Ok(())=>{},Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},Err(error)=>return Err(transient(error))}
        Ok(())
    }
    fn finish(self)->Result<()> {
        let pending:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM bodies WHERE settled=0)",[],|row|row.get(0)).map_err(transient)?;
        if pending {return Err(corrupt("restore bodies are incomplete"));}
        drop(self.db);
        fs::remove_dir_all(self.directory).map_err(transient)
    }
    /// The packs still to fetch, and their bytes.
    pub(crate) fn remaining_packs(&self)->Result<(u64,u64)> {
        let mut query=self.db.prepare("SELECT body FROM packs WHERE done=0").map_err(transient)?;
        let mut rows=query.query([]).map_err(transient)?;
        let (mut count,mut bytes)=(0u64,0u64);
        while let Some(row)=rows.next().map_err(transient)? {
            let remote:RemoteObject=serde_json::from_str(&row.get::<_,String>(0).map_err(transient)?).map_err(corrupt)?;
            count+=1;
            bytes=bytes.saturating_add(remote.receipt.byte_length);
        }
        Ok((count,bytes))
    }
    pub(crate) async fn next_pack(&mut self,key:&[u8;32],provider:&dyn Provider,repository:&RepositoryHandle,progress:&PhaseProgress,cancel:&Cancellation)->Result<bool> {
        cancel.check()?;
        if self.staged.is_empty() {
            let packs = {
                let mut query=self.db.prepare("SELECT body FROM packs WHERE done=0 ORDER BY priority,id LIMIT ?1").map_err(transient)?;
                let rows=query.query_map([provider.transfer_concurrency() as i64],|row|row.get::<_,String>(0)).map_err(transient)?;
                rows.map(|row|serde_json::from_str::<RemoteObject>(&row.map_err(transient)?).map_err(corrupt)).collect::<Result<Vec<_>>>()?
            };
            let objects:Vec<_>=packs.iter().collect();
            let mut downloads=stage_ciphertexts(&objects,&self.directory,provider,repository,Durability::Durable,Some(progress),cancel)?;
            while let Some((remote,result))=downloads.next().await {
                self.staged.push_back((remote.clone(),result));
            }
        }
        let Some((remote,ciphertext))=self.staged.pop_front() else {
            let remaining:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM bodies WHERE settled=0)",[],|row|row.get(0)).map_err(transient)?;
            if remaining {return Err(corrupt("restore bodies are incomplete"));}
            return Ok(false);
        };
        cancel.check()?;
        let ciphertext=match ciphertext {Ok(path)=>path,Err(error)=>{self.staged.clear();return Err(error);}};
        let path=match open_staged_object(&remote,key,&self.directory,ciphertext,Durability::Durable,None).await {
            Ok(path)=>path,Err(error)=>{self.staged.clear();return Err(error);}
        };
        let mut input=crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
        let tx=self.db.transaction().map_err(transient)?;
        {
            let mut query=tx.prepare("SELECT hash,offset,body FROM chunks WHERE pack=?1 AND done=0 ORDER BY hash,ordinal").map_err(transient)?;
            let mut rows=query.query([&remote.object_id]).map_err(transient)?;
            let mut output:Option<(String,fs::File)>=None;
            while let Some(row)=rows.next().map_err(transient)? {
                cancel.check()?;
                let hash:String=row.get(0).map_err(transient)?;
                let offset=u64::try_from(row.get::<_,i64>(1).map_err(transient)?).map_err(corrupt)?;
                let chunk:wire::StoredChunk=serde_json::from_str(&row.get::<_,String>(2).map_err(transient)?).map_err(corrupt)?;
                let bytes=read_chunk(&mut input,remote.plaintext_length,&chunk)?;
                if output.as_ref().is_none_or(|(current,_)|current!=&hash) {
                    if let Some((_,file))=output.take() {file.sync_all().map_err(transient)?;}
                    let path=self.directory.join("assembly").join(format!("{hash}.stage"));
                    crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
                    output=Some((hash.clone(),OpenOptions::new().write(true).open(path).map_err(transient)?));
                }
                let body=&mut output.as_mut().unwrap().1;
                body.seek(SeekFrom::Start(offset)).map_err(transient)?;
                body.write_all(&bytes).map_err(transient)?;
                tx.execute("UPDATE bodies SET remaining=remaining-1 WHERE hash=?1 AND remaining>0",[hash]).map_err(transient)?;
            }
            if let Some((_,file))=output {file.sync_all().map_err(transient)?;}
        }
        crate::trust_boundary::sync_directory(&self.directory.join("assembly")).map_err(transient)?;
        tx.execute("UPDATE chunks SET done=1 WHERE pack=?1",[&remote.object_id]).map_err(transient)?;
        tx.execute("UPDATE packs SET done=1 WHERE id=?1",[&remote.object_id]).map_err(transient)?;
        tx.commit().map_err(transient)?;
        progress.completed(0);
        drop(input);
        discard_object(&self.directory,&remote);
        Ok(true)
    }
}

/// The original units keep only a disk index while their controls remain in staging.
pub(crate) struct OriginalUnits {
    db: rusqlite::Connection,
    _file: tempfile::NamedTempFile,
}

impl OriginalUnits {
    fn new(directory: &Path) -> Result<Self> {
        let file = tempfile::NamedTempFile::new_in(directory).map_err(transient)?;
        let db = crate::sqlite_open::open(file.path()).map_err(transient)?;
        db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
            CREATE TABLE records(key TEXT PRIMARY KEY,hash TEXT NOT NULL,bytes INTEGER NOT NULL); CREATE UNIQUE INDEX original_control_hash ON records(hash)").map_err(transient)?;
        Ok(Self { db, _file: file })
    }
    fn insert(&self, key: &str, hash: &str, length: u64) -> Result<()> {
        self.db.execute("INSERT INTO records VALUES(?1,?2,?3)",rusqlite::params![key,hash,i64::try_from(length).map_err(corrupt)?]).map_err(corrupt)?;
        Ok(())
    }
    pub(crate) fn count(&self) -> Result<u64> {
        let count: i64 = self.db.query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0)).map_err(transient)?;
        u64::try_from(count).map_err(corrupt)
    }
    pub(crate) fn units<'a>(&'a self, content: &'a ContentStore) -> impl Iterator<Item = Result<(risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue)>> + 'a {
        let mut after = String::new();
        let mut page = std::collections::VecDeque::<(String,String,i64)>::new();
        let mut finished = false;
        std::iter::from_fn(move || {
            if finished { return None; }
            let read = (|| {
                if page.is_empty() {
                    let mut query = self.db.prepare("SELECT key,hash,bytes FROM records WHERE key>?1 ORDER BY key LIMIT 128").map_err(transient)?;
                    page = query.query_map([&after],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).map_err(transient)?
                        .collect::<std::result::Result<_,_>>().map_err(transient)?;
                }
                let Some((key,hash,length)) = page.pop_front() else { return Ok(None); };
                let length = u64::try_from(length).map_err(corrupt)?;
                after = key.clone();
                let unit = decode_original_unit(content,&key,&hash,length)?;
                Ok(Some((unit.key,unit.value)))
            })();
            match read { Ok(Some(value)) => Some(Ok(value)), Ok(None) => {finished=true;None}, Err(error) => {finished=true;Some(Err(error))} }
        })
    }
}

#[cfg(test)]
impl OriginalUnits {
    pub(crate) fn staged(content: &mut ContentStore, units: &BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>) -> Self {
        let records = Self::new(&std::env::temp_dir()).unwrap();
        for (key,value) in units {
            let bytes = risunest_sync_wire::canonical::encode(&super::packaging::OriginalBackupUnit {
                schema: "risunest.backup-unit/v1".into(), key:key.clone(), value:value.clone(),
            }).unwrap();
            let hash = risunest_sync_wire::hash(&bytes);
            content.put(&hash,&bytes).unwrap();
            records.insert(&format!("original-unit/{}",hex::encode(key.as_str())),&hash,bytes.len() as u64).unwrap();
        }
        content.commit().unwrap();
        records
    }
}

fn decode_original_unit(content: &ContentStore, key: &str, hash: &str, length: u64) -> Result<super::packaging::OriginalBackupUnit> {
    if length > risunest_sync_wire::MAX_METADATA_BYTES as u64 {
        return Err(corrupt("original unit control exceeds its bound"));
    }
    let mut source=content.open_body(hash).map_err(transient)?;
    let mut bytes=Vec::new();
    source.by_ref().take(risunest_sync_wire::MAX_METADATA_BYTES as u64+1).read_to_end(&mut bytes).map_err(transient)?;
    if bytes.len() as u64 != length {
        return Err(corrupt("original unit control length"));
    }
    let unit: super::packaging::OriginalBackupUnit = risunest_sync_wire::canonical::decode(
        &bytes, risunest_sync_wire::MAX_METADATA_BYTES,
    ).map_err(corrupt)?;
    if unit.schema != "risunest.backup-unit/v1"
        || key != format!("original-unit/{}", hex::encode(unit.key.as_str()))
    {
        return Err(corrupt("original unit catalog key differs from its control"));
    }
    Ok(unit)
}

#[cfg(test)]
pub(crate) async fn download_original_backup_units(
    root: &RemoteObject,
    staging_root: &Path,
    key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<OriginalUnits> {
    let walk = walk_original_backup_units(root, staging_root, key, None, provider, repository, progress, cancel).await?;
    let (packs, bytes) = walk.plan.remaining_packs()?;
    progress.plan(packs, bytes);
    fetch_original_backup_units(walk, staging_root, key, provider, repository, progress, cancel).await
}

/// An original unit catalog read into its disk index and pack plan, none of
/// its packs fetched yet.
struct OriginalUnitsWalk {
    kept: OriginalUnits,
    plan: RestoreBodyPlan,
}

/// Reads the original unit catalog, counting each node it fetches in
/// `progress`, and plans the packs its controls lie in.
async fn walk_original_backup_units(
    root: &RemoteObject,
    staging_root: &Path,
    key: &[u8; 32],
    record_into: Option<&Path>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<OriginalUnitsWalk> {
    ensure_directory(staging_root)?;
    let kept = OriginalUnits::new(staging_root)?;
    kept.db.execute_batch("CREATE TABLE nodes(id TEXT PRIMARY KEY,body TEXT NOT NULL,visited INTEGER NOT NULL);
        CREATE INDEX pending_nodes ON nodes(visited,id);
        CREATE TABLE fragments(key TEXT NOT NULL,ordinal INTEGER NOT NULL,body TEXT NOT NULL,PRIMARY KEY(key,ordinal));
        CREATE TABLE packs(id TEXT PRIMARY KEY,body TEXT NOT NULL)").map_err(transient)?;
    kept.db.execute("INSERT INTO nodes VALUES(?1,?2,0)",rusqlite::params![root.object_id,serde_json::to_string(root).map_err(corrupt)?]).map_err(transient)?;
    loop {
        cancel.check()?;
        let encoded:Option<String>=kept.db.query_row("SELECT body FROM nodes WHERE visited=0 ORDER BY id LIMIT 1",[],|row|row.get(0)).optional().map_err(transient)?;
        let Some(encoded)=encoded else {break;};
        let object:RemoteObject=serde_json::from_str(&encoded).map_err(corrupt)?;
        if object.role!=ObjectRole::Catalog || object.repository_id!=root.repository_id {return Err(corrupt("invalid original unit catalog"));}
        let path=open_object_counted(&object,key,staging_root,provider,repository,progress,cancel).await?;
        let bytes=read_bytes(&path,wire::MAX_METADATA_BYTES)?;
        let document=wire::CatalogDocument::decode(&bytes,usize::try_from(object.plaintext_length).map_err(corrupt)?).map_err(corrupt)?;
        if document.kind!=wire::CatalogKind::Records {return Err(corrupt("original unit catalog kind"));}
        kept.db.execute_batch("BEGIN").map_err(transient)?;
        if document.level==0 {
            for pack in document.packs {
                let pack=RemoteObject::from_stored(&pack,repository)?;
                if pack.role!=ObjectRole::Pack || pack.repository_id!=root.repository_id {return Err(corrupt("invalid original unit pack"));}
                let body=serde_json::to_string(&pack).map_err(corrupt)?;
                let previous:Option<String>=kept.db.query_row("SELECT body FROM packs WHERE id=?1",[&pack.object_id],|row|row.get(0)).optional().map_err(transient)?;
                if previous.as_ref().is_some_and(|old|old!=&body) {return Err(corrupt("conflicting original unit pack"));}
                kept.db.execute("INSERT OR IGNORE INTO packs VALUES(?1,?2)",rusqlite::params![pack.object_id,body]).map_err(transient)?;
            }
            for fragment in document.entries {
                if fragment.kind!=wire::CatalogEntryKind::Record || fragment.byte_length>risunest_sync_wire::MAX_METADATA_BYTES as u64 {return Err(corrupt("invalid original unit control"));}
                kept.db.execute("INSERT INTO fragments VALUES(?1,?2,?3)",rusqlite::params![fragment.key,fragment.fragment_index,serde_json::to_string(&fragment).map_err(corrupt)?]).map_err(corrupt)?;
            }
        } else {
            for child in document.children {
                let child=RemoteObject::from_stored(&child.object,repository)?;
                kept.db.execute("INSERT INTO nodes VALUES(?1,?2,0)",rusqlite::params![child.object_id,serde_json::to_string(&child).map_err(corrupt)?]).map_err(corrupt)?;
            }
        }
        kept.db.execute("UPDATE nodes SET visited=1 WHERE id=?1",[&object.object_id]).map_err(transient)?;
        kept.db.execute_batch("COMMIT").map_err(transient)?;
        discard_object(staging_root,&object);
    }
    let cache=record_into.and_then(|path|super::package_cache::PackageCache::open(path).ok());
    let mut cached_entries=Vec::new();
    let plan=RestoreBodyPlan::new(staging_root)?;
    let mut after=String::new();
    loop {
        cancel.check()?;
        let next:Option<String>=kept.db.query_row("SELECT key FROM fragments WHERE key>?1 ORDER BY key LIMIT 1",[&after],|row|row.get(0)).optional().map_err(transient)?;
        let Some(unit_key)=next else {break;};
        after=unit_key.clone();
        let mut query=kept.db.prepare("SELECT body FROM fragments WHERE key=?1 ORDER BY ordinal").map_err(transient)?;
        let mut rows=query.query([&unit_key]).map_err(transient)?;
        let mut first:Option<wire::CatalogEntryFragment>=None;
        let mut count=0u32;
        let mut chunks=Vec::new();
        let mut packs=BTreeMap::new();
        while let Some(row)=rows.next().map_err(transient)? {
            cancel.check()?;
            let fragment:wire::CatalogEntryFragment=serde_json::from_str(&row.get::<_,String>(0).map_err(transient)?).map_err(corrupt)?;
            let expected=first.get_or_insert_with(||fragment.clone());
            if fragment.fragment_index!=count || fragment.fragment_count!=expected.fragment_count || fragment.key!=expected.key
                || fragment.kind!=expected.kind || fragment.content_sha256!=expected.content_sha256 || fragment.byte_length!=expected.byte_length {
                return Err(corrupt("conflicting original unit fragments"));
            }
            count=count.checked_add(1).ok_or_else(||corrupt("original unit fragment count"))?;
            for chunk in fragment.chunks {
                if !packs.contains_key(&chunk.pack_id) {
                    let body:String=kept.db.query_row("SELECT body FROM packs WHERE id=?1",[&chunk.pack_id],|row|row.get(0)).map_err(corrupt)?;
                    let remote:RemoteObject=serde_json::from_str(&body).map_err(corrupt)?;
                    packs.insert(chunk.pack_id.clone(),remote.stored(repository)?);
                }
                chunks.push(chunk);
            }
        }
        let first=first.ok_or_else(||corrupt("missing original unit fragment"))?;
        if count!=first.fragment_count {return Err(corrupt("missing original unit fragment"));}
        let hash=hex::encode(first.content_sha256);
        kept.insert(&unit_key,&hash,first.byte_length)?;
        if let Some(cache)=&cache {
            let entry=super::packaging::EntryPlan {kind:first.kind,key:unit_key.clone(),content_sha256:hash.clone(),
                byte_length:first.byte_length,chunks:chunks.clone(),packs:packs.values().map(|pack|RemoteObject::from_stored(pack,repository)).collect::<Result<_>>()?};
            cached_entries.push(entry);
            if cached_entries.len()>=256 {
                let _=cache.put_entries(&root.repository_id,repository,wire::CatalogKind::Records,&cached_entries);
                let _=cache.put_objects(repository,cached_entries.iter().flat_map(|entry|entry.packs.iter()));
                cached_entries.clear();
            }
        }
        plan.push(&super::lww_residency::PackedSource {
            hash,byte_length:first.byte_length,chunks,packs:packs.into_values().collect(),catalog:root.stored(repository)?,
            library_id:root.repository_id.clone(),connection_id:"original-unit-controls".into(),connection_root:staging_root.into(),protected_snapshot:root.object_id.clone(),
        },false,repository)?;
    }
    if let Some(cache)=&cache {
        let _=cache.put_entries(&root.repository_id,repository,wire::CatalogKind::Records,&cached_entries);
        let _=cache.put_objects(repository,cached_entries.iter().flat_map(|entry|entry.packs.iter()));
    }
    plan.seal()?;
    Ok(OriginalUnitsWalk{kept,plan})
}

/// Fetches the packs an original unit walk planned, counting each one in
/// `progress`, and checks every control they carry.
async fn fetch_original_backup_units(
    walk: OriginalUnitsWalk,
    staging_root: &Path,
    key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<OriginalUnits> {
    let OriginalUnitsWalk{kept,mut plan}=walk;
    let mut content=content_store(staging_root)?;
    loop {
        cancel.check()?;
        while let Some((hash,path))=plan.ready()? {
            let bytes=read_bytes(&path,risunest_sync_wire::MAX_METADATA_BYTES)?;
            content.put(&hash,&bytes).map_err(transient)?;
            let (unit_key,length):(String,i64)=kept.db.query_row("SELECT key,bytes FROM records WHERE hash=?1",[&hash],|row|Ok((row.get(0)?,row.get(1)?))).map_err(corrupt)?;
            let unit=decode_original_unit(&content,&unit_key,&hash,u64::try_from(length).map_err(corrupt)?)?;
            #[cfg(test)]
            crate::persistent_store::hash_work::validation(&unit.value);
            unit.value.validate().map_err(corrupt)?;
            plan.settled_retained(&hash)?;
        }
        if !plan.next_pack(key,provider,repository,progress,cancel).await? {break;}
    }
    content.commit().map_err(transient)?;
    plan.finish()?;
    kept.db.execute_batch("DROP TABLE nodes; DROP TABLE fragments; DROP TABLE packs").map_err(transient)?;
    Ok(kept)
}

pub(crate) async fn download_backup_original_units(
    snapshot:&RemoteObject, staging_root:&Path, key:&[u8;32], provider:&dyn Provider,
    repository:&RepositoryHandle, cancel:&Cancellation,
) -> Result<OriginalUnits> {
    if snapshot.role != ObjectRole::BackupBundle {return Err(corrupt("full backup is required"))}
    let root=open_object(snapshot,key,staging_root,provider,repository,cancel).await?;
    let document=super::control::SnapshotView::read(&read_bytes(&root,wire::MAX_METADATA_BYTES)?,wire::ObjectRole::BackupBundle,&snapshot.repository_id)?;
    if snapshot.object_id != format!("snapshot-{}",document.snapshot_id) {return Err(corrupt("backup identity differs"))}
    let original=RemoteObject::from_stored(document.original_units.as_ref().ok_or_else(|| corrupt("complete original unit root is required"))?,repository)?;
    let walk=walk_original_backup_units(&original,staging_root,key,None,provider,repository,&PhaseProgress::silent(),cancel).await?;
    fetch_original_backup_units(walk,staging_root,key,provider,repository,&PhaseProgress::silent(),cancel).await
}

pub(crate) struct DatabaseFirstSnapshot {
    pub snapshot:PreparedRemoteSnapshot,
    pub original_units:OriginalUnits,
    pub required:BTreeSet<String>,
    pub present:BTreeSet<String>,
    pub missing:BTreeSet<String>,
    pub sources:Vec<super::lww_residency::SharedPackedSource>,
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn admit_asset_catalogs(
    catalogs:&[wire::StoredObject],store:&mut crate::persistent_store::PersistentStore,target:&str,
    library_id:&str,protected_segment:&str,connection_id:&str,connection_root:&Path,root_key:&[u8;32],
    provider:&dyn Provider,repository:&RepositoryHandle,cancel:&Cancellation,
) -> Result<Vec<String>> {
    let scratch=super::leftovers::managed_scratch(store.repository_root(),"asset-catalog-")?;
    let metadata=crate::sqlite_open::open_with_flags(store.repository_root().join("persistent/persistent.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).map_err(transient)?;
    let cas=PayloadCas::new(store.repository_root()).map_err(transient)?;
    let mut planned=BTreeMap::<String,super::lww_residency::SharedPackedSource>::new();let mut proofs=Vec::new();
    let mut interner=super::lww_residency::ObjectInterner::default();
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    for catalog in catalogs {
        if catalog.header.repository_id!=format_repository_id || catalog.header.role!=wire::ObjectRole::Catalog {
            return Err(corrupt("asset catalog repository or role differs"));
        }
        let root=RemoteObject::from_stored(catalog,repository)?;
        let (entries,packs,nodes)=read_catalog(&root,wire::CatalogKind::Assets,root_key,scratch.path(),provider,repository,cancel).await?;
        let cache=super::runtime::connection_directory(connection_root,connection_id).join("package-cache");
        record_read(Some(&cache),&super::packaging::CatalogRoot::Assets,&root,&entries,&packs,&nodes,repository);
        let shared_catalog=interner.intern(catalog.clone());
        let mut shared_packs=BTreeMap::<String,super::lww_residency::SharedObject>::new();
        let mut witnessed=Vec::new();
        for entry in entries {
            cancel.check()?;
            if entry.kind!=wire::CatalogEntryKind::Object {return Err(corrupt("asset catalog kind differs"));}
            let hash=hex::encode(entry.content_sha256);
            let catalogued:Option<i64>=metadata.query_row("SELECT byte_size FROM asset_objects WHERE object_hash=?1",[&hash],|row|row.get(0)).optional().map_err(transient)?;
            let catalogued=catalogued.map(u64::try_from).transpose().map_err(corrupt)?;
            if catalogued.is_some_and(|size|size!=entry.byte_length)
                || cas.stat_object(&hash).map_err(transient)?.is_some_and(|size|size!=entry.byte_length) {
                return Err(corrupt("immutable asset size differs"));
            }
            let ids=entry.chunks.iter().map(|chunk|chunk.pack_id.as_str()).collect::<BTreeSet<_>>();
            let mut references=Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(shared)=shared_packs.get(id) {references.push(shared.clone());continue;}
                let Some(pack)=packs.get(id) else {continue};
                let shared=interner.intern(pack.stored(repository)?);
                shared_packs.insert(id.to_owned(),shared.clone());references.push(shared);
            }
            let source=super::lww_residency::PackedSource{hash:hash.clone(),byte_length:entry.byte_length,
                library_id:library_id.into(),connection_id:connection_id.into(),connection_root:connection_root.into(),
                protected_snapshot:protected_segment.into(),catalog:shared_catalog.clone(),chunks:entry.chunks,packs:references};
            super::lww_residency::validate_packed_source(&source,repository)?;
            witnessed.push((hash.clone(),source.byte_length));
            if let Some(previous)=planned.get(&hash) {
                if previous.byte_length!=source.byte_length {return Err(corrupt("conflicting asset catalog body"));}
            } else {planned.insert(hash,source);}
        }
        proofs.push((catalog,witnessed));
    }
    cancel.check()?;
    for (catalog,entries) in proofs {store.external_lww_witness_asset_catalog(target,catalog,&entries).map_err(corrupt)?;}
    let registrations=planned.values().map(|source|crate::persistent_store::asset_object_catalog::AssetObjectRegistration{object_hash:source.hash.clone(),byte_size:source.byte_length}).collect::<Vec<_>>();
    for batch in registrations.chunks(crate::persistent_store::asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
        store.asset_object_catalog().register(batch,super::runtime::now_ms() as i64).map_err(transient)?;
    }
    let sources=planned.values().collect::<Vec<_>>();
    for batch in sources.chunks(super::lww_residency::PACKED_REGISTRATION_BATCH) {
        cancel.check()?;
        super::lww_residency::register_verified_packed_many(store.repository_root(),batch.iter().copied())?;
    }
    Ok(planned.into_keys().collect())
}
/// Counts in `progress` what it downloads, and plans the packs of the asset
/// bodies this device lacks, which the restore receives once it is adopted.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn download_snapshot_database_first(snapshot:&RemoteObject,staging_root:&Path,library_root:&Path,connection_root:&Path,connection_id:&str,key:&[u8;32],provider:&dyn Provider,repository:&RepositoryHandle,progress:&PhaseProgress,cancel:&Cancellation)->Result<DatabaseFirstSnapshot> {
    let walked=PhaseProgress::silent();
    let walk=walk_snapshot_database_first(snapshot,staging_root,library_root,connection_root,connection_id,key,provider,repository,&walked,cancel).await?;
    let (items,bytes)=walk.remaining()?;
    progress.plan_after(walked.read(),items,bytes);
    fetch_snapshot_database_first(walk,key,provider,repository,progress,cancel).await
}

/// A database-first restore with every catalog it needs read and none of its
/// packs fetched yet.
pub(crate) struct DatabaseFirstWalk {
    staging_root:PathBuf,
    snapshot_id:String,
    repository_id:String,
    fingerprint:String,
    revision:String,
    captured_by_device:Option<String>,
    original:OriginalUnitsWalk,
    records:Turnover,
    asset_objects:Vec<PreparedObject>,
    required:BTreeSet<String>,
    present:BTreeSet<String>,
    missing:BTreeSet<String>,
    sources:Vec<super::lww_residency::SharedPackedSource>,
    missing_packs:(u64,u64),
}

impl DatabaseFirstWalk {
    /// The packs still to fetch, the ones holding the asset bodies this device
    /// lacks included, and their bytes.
    pub(crate) fn remaining(&self)->Result<(u64,u64)> {
        let (original_items,original_bytes)=self.original.plan.remaining_packs()?;
        let (record_items,record_bytes)=self.records.remaining();
        Ok((
            original_items.saturating_add(record_items).saturating_add(self.missing_packs.0),
            original_bytes.saturating_add(record_bytes).saturating_add(self.missing_packs.1),
        ))
    }
}

/// Reads every catalog a database-first restore needs, counting each node it
/// fetches in `walked`, and works out which packs each one needs, the packs of
/// the asset bodies this device lacks among them. No pack is fetched.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn walk_snapshot_database_first(snapshot:&RemoteObject,staging_root:&Path,library_root:&Path,connection_root:&Path,connection_id:&str,key:&[u8;32],provider:&dyn Provider,repository:&RepositoryHandle,walked:&PhaseProgress,cancel:&Cancellation)->Result<DatabaseFirstWalk> {
    if snapshot.role!=ObjectRole::BackupBundle {return Err(corrupt("full backup required"));}
    ensure_directory(staging_root)?;
    let root_path=open_object_counted(snapshot,key,staging_root,provider,repository,walked,cancel).await?;
    let document=super::control::SnapshotView::read(&read_bytes(&root_path,wire::MAX_METADATA_BYTES)?,wire::ObjectRole::BackupBundle,&snapshot.repository_id)?;
    if snapshot.object_id!=format!("snapshot-{}",document.snapshot_id) {return Err(corrupt("backup identity"));}
    let original_root = document.original_units.as_ref()
        .ok_or_else(|| corrupt("full backup original unit root is required"))?;
    let original_root = RemoteObject::from_stored(original_root, repository)?;
    let original = walk_original_backup_units(
        &original_root, staging_root, key,
        Some(&super::runtime::connection_directory(connection_root,connection_id).join("package-cache")), provider, repository, walked, cancel,
    ).await?;
    let records_root=RemoteObject::from_stored(&document.library.record_catalog,repository)?;
    let cache=super::runtime::connection_directory(connection_root,connection_id).join("package-cache");
    let records=walk_checkpoint_data(&records_root,staging_root,key,Some(&cache),provider,repository,walked,cancel).await?;
    let assets_root=RemoteObject::from_stored(&document.library.asset_catalog,repository)?;
    let (entries,packs,nodes)=read_catalog_with(&assets_root,wire::CatalogKind::Assets,key,staging_root,provider,repository,walked,cancel).await?;
    record_read(Some(&cache),&super::packaging::CatalogRoot::Assets,&assets_root,&entries,&packs,&nodes,repository);
    let mut objects=Vec::new();
    let cas=PayloadCas::new(library_root).map_err(transient)?;
    let metadata = crate::sqlite_open::open_with_flags(
        library_root.join("persistent/persistent.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).map_err(transient)?;
    let mut required=BTreeSet::new();let mut present=BTreeSet::new();let mut missing=BTreeSet::new();
    let mut sources=Vec::new();let mut stored_packs=BTreeMap::<String,super::lww_residency::SharedObject>::new();
    let mut missing_packs=BTreeMap::<String,u64>::new();
    let mut interner=super::lww_residency::ObjectInterner::default();
    let catalog=interner.intern(document.library.asset_catalog.clone());
    for entry in entries {
        if entry.kind!=wire::CatalogEntryKind::Object {return Err(corrupt("asset catalog kind"));}
        let hash=hex::encode(entry.content_sha256);
        if !required.insert(hash.clone()) {return Err(corrupt("duplicate asset body"));}
        let catalogued: Option<i64> = metadata.query_row(
            "SELECT byte_size FROM asset_objects WHERE object_hash=?1", [&hash], |row| row.get(0),
        ).optional().map_err(transient)?;
        let catalogued = catalogued.map(u64::try_from).transpose().map_err(corrupt)?;
        if catalogued.is_some_and(|size| size != entry.byte_length) {
            return Err(corrupt("local immutable body size differs from backup"));
        }
        let local_size=cas.stat_object(&hash).map_err(transient)?;
        if local_size.is_some_and(|size|size!=entry.byte_length) {
            return Err(corrupt("local immutable body size differs from backup"));
        }
        let lacking=local_size!=Some(entry.byte_length);
        if lacking {missing.insert(hash.clone());} else {present.insert(hash.clone());}
        let ids=entry.chunks.iter().map(|chunk|chunk.pack_id.as_str()).collect::<BTreeSet<_>>();
        let mut references=Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(pack)=packs.get(id).filter(|_|lacking) {missing_packs.insert(id.to_owned(),pack.receipt.byte_length);}
            if let Some(stored)=stored_packs.get(id) {references.push(stored.clone());continue;}
            let Some(pack)=packs.get(id) else {continue};
            let stored=interner.intern(pack.stored(repository)?);
            stored_packs.insert(id.to_owned(),stored.clone());references.push(stored);
        }
        let source=super::lww_residency::PackedSource {hash:hash.clone(),byte_length:entry.byte_length,library_id:snapshot.repository_id.clone(),connection_id:connection_id.into(),connection_root:connection_root.into(),protected_snapshot:snapshot.object_id.clone(),catalog:catalog.clone(),chunks:entry.chunks,packs:references};
        super::lww_residency::validate_packed_source(&source,repository)?;
        sources.push(source);
        objects.push(PreparedObject{content_hash:hash.clone(),byte_length:entry.byte_length,source:ObjectSource::Library(hash)});
    }
    let missing_packs=(missing_packs.len() as u64,missing_packs.values().fold(0u64,|sum,bytes|sum.saturating_add(*bytes)));
    Ok(DatabaseFirstWalk{staging_root:staging_root.into(),snapshot_id:document.snapshot_id,repository_id:snapshot.repository_id.clone(),
        fingerprint:hex::encode(document.library.content_fingerprint),revision:document.revision,captured_by_device:document.captured_by_device,
        original,records,asset_objects:objects,required,present,missing,sources,missing_packs})
}

/// Fetches the original unit and record packs a walk planned, counting each
/// one in `progress`. The asset bodies are received once the restore is adopted.
pub(crate) async fn fetch_snapshot_database_first(walk:DatabaseFirstWalk,key:&[u8;32],provider:&dyn Provider,repository:&RepositoryHandle,progress:&PhaseProgress,cancel:&Cancellation)->Result<DatabaseFirstSnapshot> {
    let DatabaseFirstWalk{staging_root,snapshot_id,repository_id,fingerprint,revision,captured_by_device,original,records,asset_objects,required,present,missing,sources,..}=walk;
    let original_units=fetch_original_backup_units(original,&staging_root,key,provider,repository,progress,cancel).await?;
    let (records,record_objects)=run_turnover(records,key,&staging_root,provider,repository,progress,Durability::Durable,cancel).await?;
    let mut objects=record_objects.into_values().collect::<Vec<_>>();
    objects.extend(asset_objects);
    Ok(DatabaseFirstSnapshot{snapshot:PreparedRemoteSnapshot{snapshot_id,repository_id,fingerprint:fingerprint.clone(),library_fingerprint:fingerprint,logical_revision:revision.parse().map_err(corrupt)?,staging_root,records:records.into_values().collect(),objects,captured_by_device},original_units,required,present,missing,sources})
}
pub(crate) async fn download_snapshot(
    snapshot: &RemoteObject,
    staging_root: &Path,
    root_key: &[u8; 32],
    record_into: Option<&Path>,
    trust: SourceTrust<'_>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<PreparedRemoteSnapshot> {
    if !matches!(
        snapshot.role,
        ObjectRole::SyncState | ObjectRole::BackupBundle
    ) {
        return Err(corrupt("snapshot root role"));
    }
    ensure_directory(staging_root)?;
    let root_path = open_object(
        snapshot,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    let root_bytes = read_bytes(&root_path, wire::MAX_METADATA_BYTES)?;
    let wire_role = match snapshot.role {
        ObjectRole::SyncState => wire::ObjectRole::SyncState,
        _ => wire::ObjectRole::BackupBundle,
    };
    let document =
        super::control::SnapshotView::read(&root_bytes, wire_role, &snapshot.repository_id)?;
    if snapshot.object_id != format!("snapshot-{}", document.snapshot_id) {
        return Err(corrupt("snapshot identity differs"));
    }
    let record_catalog = RemoteObject::from_stored(&document.library.record_catalog, repository)?;
    let asset_catalog = RemoteObject::from_stored(&document.library.asset_catalog, repository)?;
    if record_catalog.repository_id != snapshot.repository_id
        || asset_catalog.repository_id != snapshot.repository_id
    {
        return Err(corrupt("snapshot catalog repository differs"));
    }
    let (record_entries, mut record_packs, record_nodes) = read_catalog(
        &record_catalog,
        wire::CatalogKind::Records,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    record_read(
        record_into,
        &super::packaging::CatalogRoot::Records,
        &record_catalog,
        &record_entries,
        &record_packs,
        &record_nodes,
        repository,
    );
    let (asset_entries, asset_packs, asset_nodes) = read_catalog(
        &asset_catalog,
        wire::CatalogKind::Assets,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    record_read(
        record_into,
        &super::packaging::CatalogRoot::Assets,
        &asset_catalog,
        &asset_entries,
        &asset_packs,
        &asset_nodes,
        repository,
    );
    for (id, object) in asset_packs {
        if let Some(old) = record_packs.insert(id, object.clone()) {
            if old != object {
                return Err(corrupt("conflicting snapshot pack"));
            }
        }
    }
    let library_staging = staging_root.to_path_buf();
    let library_cancel = cancel.clone();
    let library = match trust {
        SourceTrust::Downloaded => None,
        SourceTrust::ProvenLibrary(root) => Some(LocalLibrary {
            root: root.to_path_buf(),
            prove: true,
        }),
        SourceTrust::AdmittedLibrary(root) => {
            Some(LocalLibrary {
                root: root.to_path_buf(),
                prove: false,
            })
        }
    };
    let cpu = cpu_permit().await?;
    let plan = spawn_blocking(move || {
        let content = content_store(&library_staging)?;
        let mut data=resolve_entries(record_entries,&library_staging,&content,None,&library_cancel)?;
        let assets=resolve_entries(asset_entries,&library_staging,&content,library,&library_cancel)?;
        data.extend(assets);
        Ok::<_,ProviderError>(data)
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    let (records, objects) = turn_over_packs(
        plan,
        &record_packs,
        root_key,
        staging_root,
        provider,
        repository,
        progress,
        cancel,
    )
    .await?;
    Ok(PreparedRemoteSnapshot {
        snapshot_id: document.snapshot_id,
        repository_id: snapshot.repository_id.clone(),
        fingerprint: hex::encode(document.library.content_fingerprint),
        library_fingerprint: hex::encode(document.library.content_fingerprint),
        logical_revision: document.revision.parse().unwrap_or_default(),
        staging_root: staging_root.to_path_buf(),
        records: records.into_values().collect(),
        objects: objects.into_values().collect(),
        captured_by_device: document.captured_by_device,
    })
}

#[cfg(test)]
#[path = "snapshot_download_tests.rs"]
pub(super) mod download_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_body_plan_retains_completed_packs_across_quota_and_cancelled_reopens() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let root=tempfile::tempdir().unwrap();
            let stage=root.path().join("native-file-jobs/jobs/synthetic/external-restore-bodies");
            let provider=super::super::fake::FakeProvider::new(true);
            let repository=super::super::fake::repository();
            let root_key=[7;32];
            let key=derive_key(&root_key,&repository.repository_id,"data").unwrap();
            let mut body=Vec::new();let mut chunks=Vec::new();let mut packs=Vec::new();
            for (index,bytes) in [b"synthetic first chunk".as_slice(),b"synthetic second chunk",b"synthetic final chunk"].into_iter().enumerate() {
                let id=format!("pack-resume-{index}");
                let hash:[u8;32]=Sha256::digest(bytes).into();
                let mut plaintext=Vec::new();
                let stored_length=pack::write_entry(&mut plaintext,&pack::Chunk {hash,bytes:bytes.to_vec()}).unwrap();
                let header=wire::PublicObjectHeader::new(repository.repository_id.clone(),id.clone(),wire::ObjectRole::Pack,plaintext.len() as u64).unwrap();
                let mut sealed=Vec::new();
                wire::seal_envelope(&mut std::io::Cursor::new(&plaintext),&mut sealed,&key,&header).unwrap();
                packs.push(wire::StoredObject {header,locator:wire::WireLocator {connection_identity:repository.connection_identity.clone(),collection:None,object:id.clone()},
                    ciphertext_length:sealed.len() as u64,ciphertext_sha256:Sha256::digest(&sealed).into(),
                    plaintext_length:plaintext.len() as u64,plaintext_sha256:Sha256::digest(&plaintext).into()});
                provider.seed(&id,ObjectRole::Pack,sealed);
                chunks.push(wire::StoredChunk {pack_id:id,offset:0,stored_length,plaintext_length:bytes.len() as u64,plaintext_sha256:hash});
                body.extend_from_slice(bytes);
            }
            let mut catalog=packs[0].clone();
            catalog.header=wire::PublicObjectHeader::new(repository.repository_id.clone(),"catalog-resume".into(),wire::ObjectRole::Catalog,catalog.plaintext_length).unwrap();
            catalog.locator.object=catalog.header.object_id.clone();
            catalog.ciphertext_length=wire::envelope_length(&catalog.header).unwrap();
            let source=super::super::lww_residency::PackedSource {hash:risunest_sync_wire::hash(&body),byte_length:body.len() as u64,
                chunks,packs,catalog,library_id:"synthetic-resume".into(),connection_id:"synthetic".into(),
                connection_root:root.path().into(),protected_snapshot:"synthetic-snapshot".into()};
            for (index,interruption) in [ErrorKind::DailyQuotaExhausted,ErrorKind::Cancelled].into_iter().enumerate() {
                let mut plan=RestoreBodyPlan::new(&stage).unwrap();
                plan.push(&source,false,&repository).unwrap();plan.seal().unwrap();
                assert!(plan.next_pack(&root_key,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());
                assert!(plan.ready().unwrap().is_none(),"a partial multi-pack body must not settle");
                provider.fail_read(&format!("pack-resume-{}",index+1),interruption);
                assert_eq!(plan.next_pack(&root_key,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap_err().kind,interruption);
                drop(plan);
                assert!(stage.join("pack-plan/plan.sqlite").exists());
            }
            let mut plan=RestoreBodyPlan::new(&stage).unwrap();
            plan.push(&source,false,&repository).unwrap();plan.seal().unwrap();
            assert!(plan.next_pack(&root_key,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());
            let (hash,path)=plan.ready().unwrap().unwrap();
            assert_eq!(hash,source.hash);assert_eq!(fs::read(&path).unwrap(),body);
            plan.settled_retained(&hash).unwrap();
            assert!(!plan.next_pack(&root_key,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());
            drop(plan);
            let mut plan=RestoreBodyPlan::new(&stage).unwrap();
            plan.push(&source,false,&repository).unwrap();plan.seal().unwrap();
            let (hash,path)=plan.ready().unwrap().unwrap();
            assert_eq!(fs::read(&path).unwrap(),body,"uncommitted control contents can replay after reopen");
            let cas=PayloadCas::new(root.path()).unwrap();
            cas.adopt_import_payload(&path,&hash,body.len() as u64,&||false).unwrap();
            assert_eq!(cas.read_object(&hash).unwrap().unwrap(),body);
            plan.settled(&hash).unwrap();
            assert!(!plan.next_pack(&root_key,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());
            plan.finish().unwrap();
            assert!(!stage.join("pack-plan").exists());
            assert_eq!(provider.read_attempts("pack-resume-0"),1,"completed prefix must not consume the next quota window");
            assert_eq!(provider.read_attempts("pack-resume-1"),2);
            assert_eq!(provider.read_attempts("pack-resume-2"),2);
            let progress=PhaseProgress::silent();
            let path=download_packed_body_file_observed(&source.hash,source.byte_length,source.chunks.clone(),source.packs.clone(),&stage.join("export-spool"),&root_key,&provider,&repository,&progress,&Cancellation::default()).await.unwrap();
            assert_eq!(fs::read(path).unwrap(),body);
            assert_eq!(progress.read().bytes,source.packs.iter().map(|pack|pack.ciphertext_length).sum::<u64>());
        });
    }

    #[test]
    fn oversized_section_is_rejected_before_pack_access_or_allocation() {
        let entry = CompleteEntry {
            kind: wire::CatalogEntryKind::SectionObject,
            key: "object/oversized".into(),
            content_sha256: [0; 32],
            byte_length: u64::MAX,
            chunks: Vec::new(),
        };
        assert_eq!(assemble(&entry, &BTreeMap::new()).unwrap_err().kind, ErrorKind::Corrupt);
        assert!(validate_section_lengths(&[entry]).is_err());
    }


    #[test]
    fn test_read_counts_are_scoped_to_registered_staging_root() {
        let root_a = Path::new("snapshot-restore-read-count-root-a");
        let root_b = Path::new("snapshot-restore-read-count-root-b");
        reset_test_read_counts(root_a);

        record_test_read(&root_a.join("downloads"), false, 4);
        record_test_read(&root_a.join("downloads"), true, SMALL_PACK_BYTES);
        record_test_read(&root_a.join("downloads"), true, SMALL_PACK_BYTES + 1);
        record_test_read(&root_b.join("downloads"), true, 8);

        let counts = take_test_read_counts(root_a);
        assert_eq!(
            counts,
            TestReads {
                network: 3,
                network_bytes: SMALL_PACK_BYTES * 2 + 5,
                packs: 2,
                pack_bytes: SMALL_PACK_BYTES * 2 + 1,
                small_packs: 1,
            }
        );
    }
}
