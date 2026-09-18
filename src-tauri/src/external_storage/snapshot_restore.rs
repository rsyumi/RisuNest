//! Direct-locator snapshot download and full verification into native staging.
//! No PDS generation is activated here; the caller owns the subsequent staged
//! apply and eventual cleanup of this durable verified directory.
use super::{
    contract::{
        Cancellation, ErrorKind, ObjectRole, Provider, ProviderError, ReadReceipt,
        RepositoryHandle, Result,
    },
    packaging::{cpu_permit, RemoteObject},
    sections::{CapturedSection, SectionSource},
    transfer::SpoolSink,
};
use risunest_external_storage_format::{
    content_identity::hash_reader, crypto::derive_key, pack, snapshot as wire,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

#[cfg(test)]
static TEST_READ_COUNTS: std::sync::LazyLock<
    std::sync::Mutex<BTreeMap<PathBuf, (u64, u64)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeMap::new()));

#[cfg(test)]
pub(super) fn reset_test_read_counts(staging_root: &Path) {
    TEST_READ_COUNTS
        .lock()
        .unwrap()
        .insert(staging_root.to_path_buf(), (0, 0));
}

#[cfg(test)]
pub(super) fn take_test_read_counts(staging_root: &Path) -> (u64, u64) {
    TEST_READ_COUNTS
        .lock()
        .unwrap()
        .remove(staging_root)
        .expect("test read counter root was registered")
}

#[cfg(test)]
fn record_test_read(downloads: &Path, pack: bool) {
    let staging_root = downloads
        .parent()
        .expect("snapshot downloads directory has a staging root");
    let mut reads = TEST_READ_COUNTS.lock().unwrap();
    if let Some((network_reads, pack_reads)) = reads.get_mut(staging_root) {
        *network_reads += 1;
        if pack {
            *pack_reads += 1;
        }
    }
}

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedRecord {
    pub key: String,
    pub content_hash: String,
    pub byte_length: u64,
    pub path: PathBuf,
}
#[derive(Clone, Debug)]
pub(crate) struct PreparedObject {
    pub content_hash: String,
    pub byte_length: u64,
    pub path: PathBuf,
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

fn ensure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(transient)?;
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(path).map_err(transient)?) {
        return Err(corrupt("snapshot staging directory is a link"));
    }
    Ok(())
}
fn verify(path: &Path, length: u64, sha256: &str) -> Result<bool> {
    let mut file = match crate::trust_boundary::open_regular_source(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(corrupt(error)),
    };
    if file.metadata().map_err(corrupt)?.len() != length {
        return Ok(false);
    }
    Ok(hex::encode(hash_reader(&mut file, length).map_err(corrupt)?) == sha256)
}

fn publish_verified(
    partial: &Path,
    destination: &Path,
    expected_length: u64,
    expected_sha256: &str,
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
    crate::trust_boundary::sync_directory(destination.parent().unwrap()).map_err(transient)?;
    Ok(())
}

async fn download_ciphertext(
    object: &RemoteObject,
    downloads: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
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
    #[cfg(test)]
    record_test_read(downloads, object.role == ObjectRole::Pack);
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
    publish_verified(
        &partial,
        &destination,
        object.receipt.byte_length,
        &object.ciphertext_sha256,
    )?;
    Ok(destination)
}

async fn open_object(
    object: &RemoteObject,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
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
    let ciphertext = download_ciphertext(object, &downloads, provider, repository, cancel).await?;
    let partial = plaintext.join(format!("{}.partial", object.plaintext_sha256));
    if partial.exists() {
        fs::remove_file(&partial).map_err(transient)?;
    }
    let object_copy = object.clone();
    let repository_id = object.repository_id.clone();
    let root = *root_key;
    let cipher_path = ciphertext.clone();
    let partial_path = partial.clone();
    let cpu = cpu_permit().await?;
    tokio::task::spawn_blocking(move || {
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
                .map_err(corrupt)?;
        output.sync_all().map_err(transient)?;
        drop(output);
        let expected_role = match object_copy.role {
            ObjectRole::Descriptor => wire::ObjectRole::Descriptor,
            ObjectRole::Pack => wire::ObjectRole::Pack,
            ObjectRole::Catalog => wire::ObjectRole::Catalog,
            ObjectRole::SyncState => wire::ObjectRole::SyncState,
            ObjectRole::BackupBundle => wire::ObjectRole::BackupBundle,
            ObjectRole::BackupPoint => wire::ObjectRole::BackupPoint,
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
    publish_verified(
        &partial,
        &destination,
        object.plaintext_length,
        &object.plaintext_sha256,
    )?;
    Ok(destination)
}

fn read_bytes(path: &Path, max: usize) -> Result<Vec<u8>> {
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
struct CompleteEntry {
    kind: wire::CatalogEntryKind,
    key: String,
    content_sha256: [u8; 32],
    byte_length: u64,
    chunks: Vec<wire::StoredChunk>,
}

async fn read_catalog(
    root: &RemoteObject,
    expected_kind: wire::CatalogKind,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<(Vec<CompleteEntry>, BTreeMap<String, RemoteObject>)> {
    let mut stack = vec![root.clone()];
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
        let path = open_object(
            &object,
            root_key,
            staging_root,
            provider,
            repository,
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
    Ok((result, packs))
}

async fn open_packs(
    packs: &BTreeMap<String, RemoteObject>,
    root_key: &[u8; 32],
    staging_root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<BTreeMap<String, PathBuf>> {
    let mut paths = BTreeMap::new();
    for (id, pack) in packs {
        cancel.check()?;
        paths.insert(
            id.clone(),
            open_object(pack, root_key, staging_root, provider, repository, cancel).await?,
        );
    }
    Ok(paths)
}

fn materialize_entries(
    entries: Vec<CompleteEntry>,
    packs: &BTreeMap<String, PathBuf>,
    staging_root: &Path,
    cancel: &Cancellation,
    records: &mut BTreeMap<String, PreparedRecord>,
    objects: &mut BTreeMap<String, PreparedObject>,
) -> Result<()> {
    let record_root = staging_root.join("records");
    let object_root = staging_root.join("objects");
    ensure_directory(&record_root)?;
    ensure_directory(&object_root)?;
    for entry in entries {
        cancel.check()?;
        let digest = hex::encode(entry.content_sha256);
        let destination = match entry.kind {
            wire::CatalogEntryKind::Record => record_root.join(&digest),
            wire::CatalogEntryKind::Object => object_root.join(&digest),
            wire::CatalogEntryKind::SectionEntry | wire::CatalogEntryKind::SectionObject => {
                return Err(corrupt("section entry in library catalog"));
            }
        };
        if !verify(&destination, entry.byte_length, &digest)? {
            if destination.exists() {
                fs::remove_file(&destination).map_err(transient)?;
            }
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
            for chunk in &entry.chunks {
                let pack_path = packs
                    .get(&chunk.pack_id)
                    .ok_or_else(|| corrupt("catalog pack is missing"))?;
                let mut input =
                    crate::trust_boundary::open_regular_source(pack_path).map_err(corrupt)?;
                let input_length = input.metadata().map_err(corrupt)?.len();
                if chunk
                    .offset
                    .checked_add(chunk.stored_length)
                    .is_none_or(|end| end > input_length)
                {
                    return Err(corrupt("chunk escaped pack"));
                }
                input.seek(SeekFrom::Start(chunk.offset)).map_err(corrupt)?;
                let mut limited = input.take(chunk.stored_length);
                let decoded =
                    pack::read_entry(&mut limited, pack::MAX_CHUNK_BYTES).map_err(corrupt)?;
                if limited.limit() != 0
                    || decoded.hash != chunk.plaintext_sha256
                    || decoded.bytes.len() as u64 != chunk.plaintext_length
                {
                    return Err(corrupt("pack chunk differs"));
                }
                output.write_all(&decoded.bytes).map_err(transient)?;
                output_hash.update(&decoded.bytes);
            }
            output.sync_all().map_err(transient)?;
            drop(output);
            let actual: [u8; 32] = output_hash.finalize().into();
            if actual != entry.content_sha256 || !verify(&partial, entry.byte_length, &digest)? {
                return Err(corrupt("restored entry integrity failed"));
            }
            publish_verified(&partial, &destination, entry.byte_length, &digest)?;
        }
        match entry.kind {
            wire::CatalogEntryKind::Record => {
                if records
                    .insert(
                        entry.key.clone(),
                        PreparedRecord {
                            key: entry.key,
                            content_hash: digest,
                            byte_length: entry.byte_length,
                            path: destination,
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
                    path: destination,
                };
                if let Some(old) = objects.insert(digest, prepared.clone()) {
                    if old.byte_length != prepared.byte_length || old.path != prepared.path {
                        return Err(corrupt("conflicting content object"));
                    }
                }
            }
            wire::CatalogEntryKind::SectionEntry | wire::CatalogEntryKind::SectionObject => {
                return Err(corrupt("section entry in library catalog"));
            }
        }
    }
    Ok(())
}

fn assemble(entry: &CompleteEntry, packs: &BTreeMap<String, PathBuf>) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(usize::try_from(entry.byte_length).map_err(corrupt)?);
    let mut digest = Sha256::new();
    for chunk in &entry.chunks {
        let pack_path = packs
            .get(&chunk.pack_id)
            .ok_or_else(|| corrupt("catalog pack is missing"))?;
        let mut input = crate::trust_boundary::open_regular_source(pack_path).map_err(corrupt)?;
        let input_length = input.metadata().map_err(corrupt)?.len();
        if chunk
            .offset
            .checked_add(chunk.stored_length)
            .is_none_or(|end| end > input_length)
        {
            return Err(corrupt("chunk escaped pack"));
        }
        input.seek(SeekFrom::Start(chunk.offset)).map_err(corrupt)?;
        let mut limited = input.take(chunk.stored_length);
        let decoded = pack::read_entry(&mut limited, pack::MAX_CHUNK_BYTES).map_err(corrupt)?;
        if limited.limit() != 0
            || decoded.hash != chunk.plaintext_sha256
            || decoded.bytes.len() as u64 != chunk.plaintext_length
        {
            return Err(corrupt("pack chunk differs"));
        }
        digest.update(&decoded.bytes);
        bytes.extend_from_slice(&decoded.bytes);
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
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Vec<CapturedSection>> {
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
    let mut prepared = Vec::new();
    for (id, reference) in &document.sections {
        if !wanted.contains(id) {
            continue;
        }
        let entries_root = RemoteObject::from_stored(&reference.entries_root, repository)?;
        if entries_root.repository_id != snapshot.repository_id {
            return Err(corrupt("section catalog repository differs"));
        }
        let (complete, packs) = read_catalog(
            &entries_root,
            wire::CatalogKind::Section,
            root_key,
            staging_root,
            provider,
            repository,
            cancel,
        )
        .await?;
        let pack_paths =
            open_packs(&packs, root_key, staging_root, provider, repository, cancel).await?;
        let cpu = cpu_permit().await?;
        let section_cancel = cancel.clone();
        let section_staging = staging_root.to_path_buf();
        let sources = tokio::task::spawn_blocking(move || {
            materialize_section_entries(complete, &pack_paths, &section_staging, &section_cancel)
        })
        .await
        .map_err(transient)??;
        drop(cpu);
        prepared.push(CapturedSection {
            kind: reference.kind,
            generation: reference.generation.clone(),
            gc_floor: reference.gc_floor.clone(),
            max_write_clock: reference.max_write_clock.clone(),
            content_fingerprint: reference.content_fingerprint,
            sources,
        });
    }
    Ok(prepared)
}

pub(crate) async fn download_snapshot(
    snapshot: &RemoteObject,
    staging_root: &Path,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
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
    let (record_entries, mut record_packs) = read_catalog(
        &record_catalog,
        wire::CatalogKind::Records,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    let (asset_entries, asset_packs) = read_catalog(
        &asset_catalog,
        wire::CatalogKind::Assets,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    for (id, object) in asset_packs {
        if let Some(old) = record_packs.insert(id, object.clone()) {
            if old != object {
                return Err(corrupt("conflicting snapshot pack"));
            }
        }
    }
    let record_pack_paths = open_packs(
        &record_packs,
        root_key,
        staging_root,
        provider,
        repository,
        cancel,
    )
    .await?;
    let mut library_entries = record_entries;
    library_entries.extend(asset_entries);
    let library_staging = staging_root.to_path_buf();
    let library_cancel = cancel.clone();
    let cpu = cpu_permit().await?;
    let (records, objects) = tokio::task::spawn_blocking(move || {
        let mut records = BTreeMap::new();
        let mut objects = BTreeMap::new();
        materialize_entries(
            library_entries,
            &record_pack_paths,
            &library_staging,
            &library_cancel,
            &mut records,
            &mut objects,
        )?;
        Ok((records, objects))
    })
    .await
    .map_err(transient)??;
    drop(cpu);
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
mod tests {
    use super::*;

    #[test]
    fn test_read_counts_are_scoped_to_registered_staging_root() {
        let root_a = Path::new("snapshot-restore-read-count-root-a");
        let root_b = Path::new("snapshot-restore-read-count-root-b");
        reset_test_read_counts(root_a);

        record_test_read(&root_a.join("downloads"), false);
        record_test_read(&root_a.join("downloads"), true);
        record_test_read(&root_b.join("downloads"), true);

        assert_eq!(take_test_read_counts(root_a), (2, 1));
    }
}
