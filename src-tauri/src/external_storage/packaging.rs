//! Streams a completed PDS capture into bounded encrypted immutable objects.
//! This layer starts only after the PDS read snapshot has closed.
use super::{
    capabilities::Capabilities,
    content_store::{ContentStore, ObjectSource},
    contract::{
        Cancellation, ErrorKind, ObjectIntent, ObjectReceipt, ObjectRole, Provider, ProviderError,
        ReadReceipt, RemoteLocator, RepositoryHandle, Result,
    },
    connection_store::{ConnectionStore, UnusableReason},
    journal::{
        validate_receipt, FamilyPermit, PackFamilies, SpoolAdmission, TransferJournal,
        ACTIVE_PACK_FAMILIES,
    },
    package_cache::{kind_name, CatalogRange, PackageCache},
    phase_progress::PhaseProgress,
    sections::CapturedSection,
    transfer::SpoolSink,
    transfer_job,
};
use crate::{asset_repository::PayloadCas, persistent_store::external_capture::CapturedSnapshot};
use risunest_external_storage_format::{

    control as wire_control,
    crypto::derive_key,
    format::library_fingerprint_domain,
    pack::{self, Chunk, ChunkEncoder, CompressionPolicy, ENTRY_OVERHEAD, MAX_CHUNK_BYTES},
    section::{SectionKind, SECTION_CODEC},
    snapshot as wire,
};
use risunest_sync_wire::head::Sequence;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use super::worker_observation::spawn_blocking;
#[cfg(not(test))]
use tokio::task::spawn_blocking;
use sha2::{Digest, Sha256};
#[cfg(not(test))]
use risunest_external_storage_format::content_identity::{hash,hash_reader};
#[cfg(test)]
fn hash(bytes:&[u8])->[u8;32] {
    crate::persistent_store::hash_work::observe("native_external_content_identity",bytes.len());
    risunest_external_storage_format::content_identity::hash(bytes)
}
#[cfg(test)]
fn hash_reader(reader:&mut impl std::io::Read,expected_length:u64)->risunest_external_storage_format::Result<[u8;32]> {
    struct Input<'a,R>{reader:&'a mut R,length:u64,expected:u64}
    impl<R:std::io::Read> std::io::Read for Input<'_,R> {
        fn read(&mut self,bytes:&mut [u8])->std::io::Result<usize> {
            let count=self.reader.read(bytes)?;
            self.length=self.length.saturating_add(count as u64);
            if self.length<=self.expected {crate::persistent_store::hash_work::update("native_external_content_reader",count);}
            Ok(count)
        }
    }
    crate::persistent_store::hash_work::begin("native_external_content_reader");
    let result=risunest_external_storage_format::content_identity::hash_reader(&mut Input{reader,length:0,expected:expected_length},expected_length);
    if result.is_err() {crate::persistent_store::hash_work::incomplete("native_external_content_reader");}
    result
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Arc, LazyLock},
};

const DEFAULT_MAX_STORED_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_TARGET_BYTES: u64 = 64 * 1024 * 1024;
const MIN_OBJECT_BYTES: u64 = 1024;
/// The largest pack plaintext this app seals. Asset packs are cut at it and
/// the other catalogs below it.
pub(crate) const MAX_PACK_PLAINTEXT_BYTES: u64 = 64 * 1024 * 1024;

/// Transfer spool room a job that seals packs needs before it starts: the
/// ciphertext of every pack that may wait for one registration wave, each with
/// the page room it is admitted with. An envelope adds a fixed prefix and a
/// public header of at most `MAX_PUBLIC_HEADER_BYTES` to the ciphertext.
pub(crate) fn producing_job_spool_headroom() -> Result<u64> {
    let pack = risunest_external_storage_format::crypto::ciphertext_length(MAX_PACK_PLAINTEXT_BYTES)
        .map_err(corrupt)?
        .saturating_add(8 + wire::MAX_PUBLIC_HEADER_BYTES as u64);
    Ok((0..ACTIVE_PACK_FAMILIES).fold(0u64, |room, waiting| {
        room.saturating_add(pack)
            .saturating_add(super::control::inventory_page_headroom(waiting + 1))
    }))
}
static SNAPSHOT_CPU: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

pub(super) fn corrupt(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt).caused(&error)
}
pub(super) fn transient(error: impl std::fmt::Display + 'static) -> ProviderError {
    let io = (&error as &dyn std::any::Any).downcast_ref::<std::io::Error>();
    io.map(|io| local_io_kind(io.kind()))
        .unwrap_or_else(|| ProviderError::new(ErrorKind::Transient))
        .caused(&error)
}

fn local_io_kind(kind: std::io::ErrorKind) -> ProviderError {
    use std::io::ErrorKind as IoKind;
    ProviderError::new(match kind {
        IoKind::StorageFull => ErrorKind::LocalStorageFull,
        IoKind::PermissionDenied | IoKind::ReadOnlyFilesystem => ErrorKind::LocalPermissionDenied,
        _ => ErrorKind::LocalFailure,
    })
}

pub(super) fn format_error(error: risunest_external_storage_format::FormatError) -> ProviderError {
    match error.io_kind() {
        Some(kind) => local_io_kind(kind).caused(&error),
        None => corrupt(error),
    }
}

pub(super) async fn cpu_permit() -> Result<tokio::sync::OwnedSemaphorePermit> {
    SNAPSHOT_CPU
        .clone()
        .acquire_owned()
        .await
        .map_err(transient)
}

/// Whether a publication that is already required may also repackage the small
/// live record packs a long edit history left behind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Maintenance {
    #[default]
    Declined,
    Allowed,
}

/// The stored ciphertext length at or below which a live record pack counts as
/// small for repackaging. This is not the small-object storage threshold.
const SMALL_PACK_CIPHERTEXT_BYTES: u64 = 256 * 1024;
/// How many small live packs have to exist before repackaging is worth a
/// publication's extra uploads.
const COALESCE_TRIGGER_PACKS: usize = 32;
const COALESCE_MAX_PACKS: usize = 16;
const COALESCE_MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
/// Record catalog leaves one cycle may add to the ones the required changes
/// write, counted on the layout the catalog writer produces, split leaves
/// included. A run whose keys already sit in more previous leaves than this
/// is cut before its layout is planned.
const COALESCE_MAX_LEAVES: usize = 8;
/// What the maintenance portion can spend at its cap: one pack upload and one
/// registration page each. A remaining allowance below this declines it.
pub(crate) const COALESCE_REQUEST_BUDGET: u64 = 2 * COALESCE_MAX_PACKS as u64;

/// How many small live packs start a coalescing cycle, and how many one cycle
/// may drain.
#[derive(Clone, Copy, Debug)]
struct CoalescePacks {
    trigger: usize,
    max: usize,
}
const COALESCE_PACKS: CoalescePacks = CoalescePacks {
    trigger: COALESCE_TRIGGER_PACKS,
    max: COALESCE_MAX_PACKS,
};
#[cfg(test)]
thread_local! {
    static TEST_COALESCE_PACKS: std::cell::Cell<CoalescePacks> = const { std::cell::Cell::new(COALESCE_PACKS) };
}
fn coalesce_packs() -> CoalescePacks {
    #[cfg(test)]
    return TEST_COALESCE_PACKS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    COALESCE_PACKS
}

/// What one publication's maintenance portion actually repackaged, and the
/// record catalog leaves its plan charged to it beside the ones the required
/// changes write anyway. The leaf counts stay zero when no cycle ran.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MaintenanceReport {
    pub(crate) packs: u64,
    pub(crate) source_bytes: u64,
    pub(crate) leaves: u64,
    pub(crate) required_leaves: u64,
}

/// One record pack the entries this publication would reuse still name. The
/// bytes are the live source bytes a cycle would write again, and for a small
/// pack the keys and the previous catalog leaves they sit in.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LivePack {
    object_id: String,
    ciphertext_length: u64,
    bytes: u64,
    first_key: String,
    leaves: BTreeSet<usize>,
    keys: Vec<(String, u64)>,
    /// Every entry naming it can be written again from the capture.
    rewritable: bool,
}

/// The adjacent run of small live packs one cycle repackages. Selection is by
/// first live key, so a run covers neighbouring catalog ranges rather than
/// scattered ones, and it stops at the pack, byte, leaf and trigger bounds.
fn select_small_packs(mut live: Vec<LivePack>) -> Vec<LivePack> {
    let packs = coalesce_packs();
    live.retain(|pack| pack.rewritable && pack.ciphertext_length <= SMALL_PACK_CIPHERTEXT_BYTES);
    if live.len() < packs.trigger {
        return Vec::new();
    }
    live.sort_by(|a, b| {
        (&a.first_key, &a.object_id).cmp(&(&b.first_key, &b.object_id))
    });
    let mut selected = Vec::new();
    let mut bytes = 0u64;
    let mut leaves = BTreeSet::new();
    for pack in live {
        if selected.len() >= packs.max
            || bytes.saturating_add(pack.bytes) > COALESCE_MAX_SOURCE_BYTES
            || leaves.union(&pack.leaves).count() > COALESCE_MAX_LEAVES
        {
            break;
        }
        bytes = bytes.saturating_add(pack.bytes);
        leaves.extend(pack.leaves.iter().copied());
        selected.push(pack);
    }
    selected
}

/// Whether the pack being built is full: the next entry would take it past
/// the target. The pack writer and the coalescing plan cut packs here.
fn pack_full(length: u64, stored: u64, target: u64) -> bool {
    length > 0 && length + stored > target
}

fn locator_length(object: &RemoteObject) -> usize {
    object.receipt.locator.object.len()
        + object.receipt.locator.collection.as_ref().map_or(0, String::len)
}

/// Stands in for the pack at `index` of the ones a publication is about to
/// write, whose ID and locator do not exist yet.
fn stand_in_pack_id(index: usize) -> String {
    format!("pack-{index:064x}")
}

/// The record catalog a cycle would produce, laid out before any pack is
/// built. Entries the publication writes have no placement yet, so each is
/// placed where the pack writer would place it: in key order, cut where
/// `pack_full` cuts, a moved body at the stored length its previous chunks
/// had and a changed one at the most its encoding can take. What cannot be
/// known before an upload, a new pack's hashes and locator, is given its
/// longest form, with the longest locator this connection already holds.
/// The leaves come from `catalog_leaves`, the function the writer lays them
/// out with.
struct CoalescingPlan<'a> {
    kind: wire::CatalogKind,
    sources: &'a [SourceEntry],
    /// The parent placement of every source this publication can reuse.
    reusable: &'a [Option<EntryPlan>],
    template: &'a RemoteObject,
    ranges: &'a [(String, String)],
    size: &'a StoredSize<'a>,
    target: u64,
    chunk_bytes: u64,
    format_repository_id: &'a str,
    repository: &'a RepositoryHandle,
    members: &'a BTreeSet<String>,
    cache: &'a PackageCache,
    evidence: &'a ObjectEvidence,
}

impl CoalescingPlan<'_> {
    /// The entries a publication moving the bodies of `moved` would catalog,
    /// and which of them it writes.
    fn entries(&self, moved: &BTreeSet<String>) -> Result<(Vec<EntryPlan>, BTreeSet<String>)> {
        let mut entries = Vec::with_capacity(self.sources.len());
        let mut written = BTreeSet::new();
        let mut packs: Vec<u64> = Vec::new();
        for (source, reusable) in self.sources.iter().zip(self.reusable) {
            if let Some(plan) = reusable.as_ref().filter(|plan| !moved.contains(&plan.key)) {
                entries.push(plan.clone());
                continue;
            }
            let pieces: Vec<(u64, [u8; 32], u64)> = match reusable {
                Some(plan) => plan.chunks.iter()
                    .map(|chunk| (chunk.plaintext_length, chunk.plaintext_sha256, chunk.stored_length))
                    .collect(),
                None => {
                    let mut lengths = Vec::new();
                    let mut remaining = source.byte_length;
                    loop {
                        let count = remaining.min(self.chunk_bytes);
                        lengths.push(count);
                        remaining -= count;
                        if remaining == 0 {
                            break;
                        }
                    }
                    lengths.into_iter()
                        .map(|length| (length, [u8::MAX; 32], length + 1 + ENTRY_OVERHEAD))
                        .collect()
                }
            };
            let mut chunks = Vec::with_capacity(pieces.len());
            for (plaintext_length, plaintext_sha256, stored_length) in pieces {
                if packs.last().is_none_or(|length| pack_full(*length, stored_length, self.target)) {
                    packs.push(0);
                }
                let index = packs.len() - 1;
                chunks.push(wire::StoredChunk {
                    pack_id: stand_in_pack_id(index),
                    offset: packs[index],
                    stored_length,
                    plaintext_length,
                    plaintext_sha256,
                });
                packs[index] += stored_length;
            }
            written.insert(source.key.clone());
            entries.push(EntryPlan {
                kind: source.kind,
                key: source.key.clone(),
                content_sha256: source.content_sha256.clone(),
                byte_length: source.byte_length,
                chunks,
                packs: Vec::new(),
            });
        }
        let packs = packs.into_iter().enumerate()
            .map(|(index, length)| self.stand_in(index, length))
            .collect::<Result<Vec<_>>>()?;
        for entry in entries.iter_mut().filter(|entry| written.contains(&entry.key)) {
            let named: BTreeSet<&str> = entry.chunks.iter().map(|chunk| chunk.pack_id.as_str()).collect();
            entry.packs = packs.iter()
                .filter(|pack| named.contains(pack.object_id.as_str()))
                .cloned()
                .collect();
        }
        Ok((entries, written))
    }

    fn stand_in(&self, index: usize, length: u64) -> Result<RemoteObject> {
        let object_id = stand_in_pack_id(index);
        let mut receipt = self.template.receipt.clone();
        receipt.byte_length = self.size.ciphertext(&object_id, wire::ObjectRole::Pack, length)?;
        Ok(RemoteObject {
            repository_id: self.format_repository_id.to_owned(),
            object_id,
            role: ObjectRole::Pack,
            receipt,
            ciphertext_sha256: "f".repeat(64),
            plaintext_length: length,
            plaintext_sha256: "f".repeat(64),
        })
    }

    /// The leaves of `entries` that are written rather than referenced again,
    /// each with the span of fragments it holds and whether it holds a key of
    /// `moved`.
    fn written_leaves(
        &self,
        entries: &[EntryPlan],
        written: &BTreeSet<String>,
        moved: &BTreeSet<String>,
    ) -> Result<Vec<((usize, usize), bool)>> {
        let fragments = catalog_fragments(entries)?;
        let packs = pack_locators(entries, self.repository)?;
        let mut start = 0;
        let mut leaves = Vec::new();
        for leaf in catalog_leaves(self.kind, &fragments, &packs, self.ranges, self.size)? {
            let span = (start, start + leaf.document.entries.len());
            start = span.1;
            let holds_moved = leaf.document.entries.iter().any(|entry| moved.contains(&entry.key));
            if holds_moved
                || leaf.document.entries.iter().any(|entry| written.contains(&entry.key))
                || admitted_node(
                    &leaf.bytes, Some(self.members), self.format_repository_id, self.cache,
                    self.evidence, self.repository,
                )?
                .is_none()
            {
                leaves.push((span, holds_moved));
            }
        }
        Ok(leaves)
    }

    /// How many leaves a cycle moving `moved` adds: every leaf it writes that
    /// holds a moved key, and every other leaf it writes that the required
    /// changes alone would not have written the same way.
    fn added_leaves(&self, moved: &BTreeSet<String>, required: &BTreeSet<(usize, usize)>) -> Result<usize> {
        let (entries, written) = self.entries(moved)?;
        Ok(self.written_leaves(&entries, &written, moved)?
            .into_iter()
            .filter(|(span, holds_moved)| *holds_moved || !required.contains(span))
            .count())
    }

    /// The longest leading part of `run` whose cycle adds at most the leaf
    /// bound, with the leaves it adds and the leaves the required changes
    /// write without it. Nothing is built or uploaded to find it, and a run
    /// of which no part fits is declined whole. When the catalog these
    /// sources make already has a published root, the required changes write
    /// no leaf of it. A layout the plan cannot fit, which its longest-form
    /// stand-ins can cause where the actual objects would fit, costs the cycle
    /// and never the publication.
    fn bound(
        &self,
        mut run: Vec<LivePack>,
        root_cached: bool,
        cancel: &Cancellation,
    ) -> Result<(Vec<LivePack>, usize, usize)> {
        let unfit = |error: &ProviderError| error.kind == ErrorKind::FileTooLarge;
        let required: BTreeSet<(usize, usize)> = if root_cached {
            BTreeSet::new()
        } else {
            let (entries, written) = self.entries(&BTreeSet::new())?;
            match self.written_leaves(&entries, &written, &BTreeSet::new()) {
                Ok(leaves) => leaves.into_iter().map(|(span, _)| span).collect(),
                Err(error) if unfit(&error) => return Ok((Vec::new(), 0, 0)),
                Err(error) => return Err(error),
            }
        };
        let moved = |count: usize| {
            run[..count].iter()
                .flat_map(|pack| pack.keys.iter().map(|(key, _)| key.clone()))
                .collect::<BTreeSet<_>>()
        };
        let added_by = |count: usize| match self.added_leaves(&moved(count), &required) {
            Err(error) if unfit(&error) => Ok(usize::MAX),
            other => other,
        };
        cancel.check()?;
        let mut added = added_by(run.len())?;
        if added > COALESCE_MAX_LEAVES {
            // The empty run adds nothing and the whole run adds too much.
            let (mut fits, mut exceeds) = (0, run.len());
            added = 0;
            while exceeds - fits > 1 {
                cancel.check()?;
                let middle = fits + (exceeds - fits) / 2;
                let count = added_by(middle)?;
                if count <= COALESCE_MAX_LEAVES {
                    fits = middle;
                    added = count;
                } else {
                    exceeds = middle;
                }
            }
            run.truncate(fits);
        }
        Ok((run, added, required.len()))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PackageLimits {
    /// Provider-side stored file limit, before provider SDK wrapping.
    pub max_stored_bytes: u64,
    pub sdk_overhead_bytes: u64,
    pub target_plaintext_bytes: u64,
    pub maintenance: Maintenance,
}
impl PackageLimits {
    pub(crate) fn with_maintenance(mut self, allowed: bool) -> Self {
        self.maintenance = if allowed {
            Maintenance::Allowed
        } else {
            Maintenance::Declined
        };
        self
    }
}
impl PackageLimits {
    pub(crate) fn from_capabilities(capabilities: &Capabilities) -> Result<Self> {
        let max_stored_bytes = capabilities
            .max_stored_bytes
            .unwrap_or(DEFAULT_MAX_STORED_BYTES);
        if capabilities.sdk_overhead_bytes.checked_add(MIN_OBJECT_BYTES)
            .is_none_or(|minimum| max_stored_bytes <= minimum)
        {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        Ok(Self {
            max_stored_bytes,
            sdk_overhead_bytes: capabilities.sdk_overhead_bytes,
            target_plaintext_bytes: DEFAULT_TARGET_BYTES,
            maintenance: Maintenance::Declined,
        })
    }
}

/// What the packaged object is for. A published state inherits the sections
/// the observed state carried, so a library-only publish preserves them. A
/// backup bundle instead declares its own coverage and never merges.
#[derive(Clone, Debug)]
pub(crate) enum SnapshotPurpose {
    LwwCheckpoint {
        covered_prefixes: super::lww_checkpoint::Coverage,
        state_identity: String,
        asset_catalogs: Vec<wire::StoredObject>,
        standalone_bodies: BTreeMap<String, super::lww_segment::LargeBody>,
    },
    SyncState {
        epoch: String,
        generation: Sequence,
        parent_sections: BTreeMap<String, wire::SectionSnapshotRef>,
    },
    BackupBundle {
        source: wire_control::BundleSource,
        remote_generation: Option<Sequence>,
        original_units: super::capture::OriginalBackupUnits,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotMetadata {
    pub snapshot_id: String,
    pub repository_id: String,
    pub library_id: String,
    pub author_device_id: String,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub parent_snapshot_id: Option<String>,
    pub content_fingerprint: [u8; 32],
    pub purpose: SnapshotPurpose,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RemoteObject {
    /// Random repository identity from the encrypted format descriptor.
    pub repository_id: String,
    pub object_id: String,
    pub role: ObjectRole,
    pub receipt: ObjectReceipt,
    pub ciphertext_sha256: String,
    pub plaintext_length: u64,
    pub plaintext_sha256: String,
}
impl RemoteObject {
    pub(crate) fn stored(&self, repository: &RepositoryHandle) -> Result<wire::StoredObject> {
        self.receipt.locator.validate_for(repository)?;
        if !self.receipt.complete
            || self.receipt.byte_length == 0
            || !crate::trust_boundary::is_lower_hex_256(&self.ciphertext_sha256)
            || !crate::trust_boundary::is_lower_hex_256(&self.plaintext_sha256)
        {
            return Err(corrupt("invalid remote object"));
        }
        let role = wire_role(self.role)?;
        let header = wire::PublicObjectHeader::new(
            self.repository_id.clone(),
            self.object_id.clone(),
            role,
            self.plaintext_length,
        )
        .map_err(corrupt)?;
        let stored = wire::StoredObject {
            header,
            locator: wire::WireLocator {
                connection_identity: self.receipt.locator.connection_identity.clone(),
                collection: self.receipt.locator.collection.clone(),
                object: self.receipt.locator.object.clone(),
            },
            ciphertext_length: self.receipt.byte_length,
            ciphertext_sha256: decode_hash(&self.ciphertext_sha256)?,
            plaintext_length: self.plaintext_length,
            plaintext_sha256: decode_hash(&self.plaintext_sha256)?,
        };
        stored.validate().map_err(corrupt)?;
        Ok(stored)
    }

    pub(crate) fn from_stored(
        value: &wire::StoredObject,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        value.validate().map_err(corrupt)?;
        let locator = RemoteLocator {
            connection_identity: value.locator.connection_identity.clone(),
            collection: value.locator.collection.clone(),
            object: value.locator.object.clone(),
        };
        locator.validate_for(repository)?;
        Ok(Self {
            repository_id: value.header.repository_id.clone(),
            object_id: value.header.object_id.clone(),
            role: native_role(value.header.role)?,
            receipt: ObjectReceipt {
                locator,
                byte_length: value.ciphertext_length,
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: hex::encode(value.ciphertext_sha256),
            plaintext_length: value.plaintext_length,
            plaintext_sha256: hex::encode(value.plaintext_sha256),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CompletedSnapshot {
    pub reference: RemoteObject,
    pub snapshot_id: String,
    pub repository_id: String,
    /// The whole-state identifier for a published state, or the bundle
    /// identifier for a backup. Control changes reach it; library content
    /// changes reach `library_fingerprint`.
    pub fingerprint: String,
    pub library_fingerprint: String,
    pub logical_revision: u64,
    pub record_catalog: RemoteObject,
    pub asset_catalog: RemoteObject,
    pub sections: BTreeMap<String, wire::SectionSnapshotRef>,
    pub referenced_objects: Vec<RemoteObject>,
    /// What the optional maintenance portion repackaged, reported apart from
    /// the publication's own work.
    pub maintenance: MaintenanceReport,
    pub hydration: SourceHydration,
    #[cfg(test)]
    pub compaction_catalog_merge_visits: Option<u64>,
    #[cfg(test)]
    pub compaction_capture_rows: Option<u64>,
}

/// Library bodies this publication fetched through custody into the asset
/// repository because the destination did not already hold them. They stay
/// there afterwards as local data.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SourceHydration {
    pub objects: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicationReadiness {
    Verified,
    /// A recreated object has a different locator, or a cached source is gone.
    /// Repackage from the pinned capture under a new snapshot publication intent;
    /// the old immutable snapshot must never be overwritten with different bytes.
    Repackage,
}

/// Call under the active execution's protection immediately before publishing a
/// head or backup point. Packaging a root earlier is not proof of existence now.
/// Only confirmed missing ciphertext is repaired, using the original sealed spool.
pub(crate) async fn verify_publication(
    completed: &CompletedSnapshot,
    repository_root: &Path,
    cache_root: &Path,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<PublicationReadiness> {
    if completed.reference.repository_id != completed.repository_id
        || !matches!(completed.reference.role, ObjectRole::SyncState | ObjectRole::BackupBundle)
    {
        return Err(corrupt("publication root identity differs"));
    }
    let mut cache = PackageCache::open(cache_root)?;
    let mut evidence = ObjectEvidence::open(repository_root, journal.connection_id())?;
    let mut catalogs = vec![completed.record_catalog.clone(), completed.asset_catalog.clone()];
    for section in completed.sections.values() {
        catalogs.push(RemoteObject::from_stored(&section.entries_root, repository)?);
    }
    for catalog in catalogs {
        if catalog.repository_id != completed.repository_id { return Err(corrupt("publication repository differs")); }
        let Some(objects) = revalidate_cached_catalog(
            &catalog, &mut cache, &mut evidence, journal, root_key, provider, repository, cancel,
        ).await? else {
            return Ok(PublicationReadiness::Repackage);
        };
        let current = objects.first().ok_or_else(|| corrupt("empty publication catalog"))?;
        if current.receipt.locator != catalog.receipt.locator {
            return Ok(PublicationReadiness::Repackage);
        }
    }
    let Some(root) = revalidate_cached_object(
        &completed.reference, &mut cache, &mut evidence, journal, root_key, provider, repository, cancel,
    ).await? else {
        return Ok(PublicationReadiness::Repackage);
    };
    if root.receipt.locator != completed.reference.receipt.locator {
        // The caller's immutable point or prepared head names the previous root.
        return Ok(PublicationReadiness::Repackage);
    }
    Ok(PublicationReadiness::Verified)
}

pub(crate) fn wire_role(role: ObjectRole) -> Result<wire::ObjectRole> {
    match role {
        ObjectRole::Segment => Err(ProviderError::new(ErrorKind::Unsupported)),
        ObjectRole::Snapshot => Ok(wire::ObjectRole::SyncState),
        ObjectRole::Pack => Ok(wire::ObjectRole::Pack),
        ObjectRole::Catalog => Ok(wire::ObjectRole::Catalog),
        ObjectRole::SyncState => Ok(wire::ObjectRole::SyncState),
        ObjectRole::BackupBundle => Ok(wire::ObjectRole::BackupBundle),
        ObjectRole::BackupPoint => Ok(wire::ObjectRole::BackupPoint),
        ObjectRole::InventoryPage => Ok(wire::ObjectRole::InventoryPage),
        ObjectRole::Descriptor => Ok(wire::ObjectRole::Descriptor),
        ObjectRole::Lease => Ok(wire::ObjectRole::Lease),
    }
}
pub(crate) fn native_role(role: wire::ObjectRole) -> Result<ObjectRole> {
    match role {
        wire::ObjectRole::Pack => Ok(ObjectRole::Pack),
        wire::ObjectRole::Catalog => Ok(ObjectRole::Catalog),
        wire::ObjectRole::SyncState => Ok(ObjectRole::SyncState),
        wire::ObjectRole::BackupBundle => Ok(ObjectRole::BackupBundle),
        wire::ObjectRole::BackupPoint => Ok(ObjectRole::BackupPoint),
        wire::ObjectRole::InventoryPage => Ok(ObjectRole::InventoryPage),
        wire::ObjectRole::Descriptor => Ok(ObjectRole::Descriptor),
        wire::ObjectRole::Lease => Ok(ObjectRole::Lease),
        wire::ObjectRole::Head => Err(corrupt("head is not an immutable snapshot object")),
    }
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| corrupt("invalid sha256"))
}

/// What this connection already knows it cannot reference again. It lives
/// beside the connection rather than in the package cache, so rebuilding the
/// cache from authenticated metadata cannot bring a known failure back.
pub(super) struct ObjectEvidence {
    store: ConnectionStore,
    connection: String,
}

impl ObjectEvidence {
    pub(super) fn open(root: &Path, connection: &str) -> Result<Self> {
        Ok(Self {
            store: ConnectionStore::open(root)?,
            connection: connection.to_owned(),
        })
    }
    /// Everything that names one remote object: its locator, role, lengths and
    /// both hashes. A repaired object published elsewhere is a different one.
    fn identity(value: &RemoteObject, repository: &RepositoryHandle) -> Result<String> {
        super::reachability::object_identity(value, repository)
    }
    fn known(&self, identity: &str) -> Result<bool> {
        Ok(self
            .store
            .unusable_object(&self.connection, identity)?
            .is_some())
    }
    fn record(&self, identity: &str, object_id: &str, reason: UnusableReason) -> Result<()> {
        self.store
            .record_unusable_object(&self.connection, identity, object_id, reason)
    }
    /// Remembers damage found outside a publication, such as by a check,
    /// before the cache rows naming the object go.
    pub(super) fn record_object(
        &self,
        value: &RemoteObject,
        repository: &RepositoryHandle,
        reason: UnusableReason,
    ) -> Result<()> {
        self.record(&Self::identity(value, repository)?, &value.object_id, reason)
    }
    /// This exact object answered for itself, so whatever was remembered about
    /// it no longer describes it.
    fn clear(&self, identity: &str) -> Result<()> {
        self.store.forget_unusable_object(&self.connection, identity)
    }
}

/// Which catalog of a publication one parent root is. A rebuilt cache has to
/// key what it reads the way the next publication looks it up, and only the
/// publishing side knows which section a section catalog belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CatalogRoot {
    Records,
    Assets,
    Section(String),
}

impl CatalogRoot {
    pub(super) fn key(&self) -> String {
        match self {
            Self::Records => "records".into(),
            Self::Assets => "assets".into(),
            Self::Section(id) => format!("section/{id}"),
        }
    }
    pub(super) fn kind(&self) -> wire::CatalogKind {
        match self {
            Self::Records => wire::CatalogKind::Records,
            Self::Assets => wire::CatalogKind::Assets,
            Self::Section(_) => wire::CatalogKind::Section,
        }
    }
}

/// The authenticated graph a publication may reuse from without asking the
/// provider about it again. Selected before the first reuse decision and
/// recorded on the job, so a cleanup that finds the job stopped protects it.
#[derive(Debug)]
pub(crate) struct ParentGraph {
    objects: Vec<wire::StoredObject>,
    roots: Vec<(CatalogRoot, RemoteObject)>,
    identities: BTreeSet<String>,
}

impl ParentGraph {
    pub(crate) fn new(
        roots: Vec<(CatalogRoot, wire::StoredObject)>,
        repository: &RepositoryHandle,
    ) -> Result<Self> {
        if roots.is_empty() {
            return Err(corrupt("parent graph has no roots"));
        }
        let mut objects = Vec::new();
        let mut labelled = Vec::new();
        let mut identities = BTreeSet::new();
        for (label, object) in roots {
            let remote = RemoteObject::from_stored(&object, repository)?;
            if remote.role != ObjectRole::Catalog {
                return Err(corrupt("parent root is not a catalog"));
            }
            identities.insert(super::reachability::object_identity(&remote, repository)?);
            objects.push(object);
            labelled.push((label, remote));
        }
        Ok(Self {
            objects,
            roots: labelled,
            identities,
        })
    }
    pub(crate) fn stored(&self) -> &[wire::StoredObject] {
        &self.objects
    }
    pub(super) fn roots(&self) -> &[(CatalogRoot, RemoteObject)] {
        &self.roots
    }
    pub(super) fn identities(&self) -> &BTreeSet<String> {
        &self.identities
    }
}

#[derive(Clone, Default)]
pub(super) struct AttemptVerification {
    receipts: BTreeMap<String, ObjectReceipt>,
    catalogs: BTreeMap<String, Vec<RemoteObject>>,
    catalog_children: usize,
}

fn verification_key(value: &RemoteObject) -> Result<String> {
    serde_json::to_string(&(
        &value.receipt.locator, &value.repository_id, &value.object_id, value.role,
        value.plaintext_length, &value.plaintext_sha256, &value.ciphertext_sha256,
        value.receipt.byte_length,
    )).map_err(corrupt)
}

/// Reopening a cache never proves that its old object still exists. A journal
/// can repair a missing object from the original ciphertext; a cache without
/// that source must let its caller rebuild from the pinned capture.
async fn revalidate_cached_object(
    value: &RemoteObject,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<RemoteObject>> {
    revalidate_cached_object_at(
        value, cache, evidence, journal, root_key, provider, repository, cancel, None,
    )
    .await
}

async fn revalidate_cached_object_at(
    value: &RemoteObject,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
    output: Option<&Path>,
) -> Result<Option<RemoteObject>> {
    cancel.check()?;
    value.stored(repository)?;
    // A repaired opaque locator may be newer than an entry's embedded receipt.
    // Consult the existing inventory before declaring that old locator missing;
    // the current remote bytes still have to pass a fresh verification below.
    let mut persisted_version = None;
    let mut verified = match cache.object(
        &value.repository_id, repository, &value.object_id, &value.plaintext_sha256,
    )? {
        Some(current) => {
            if current.role != value.role || current.plaintext_length != value.plaintext_length
                || current.ciphertext_sha256 != value.ciphertext_sha256
                || current.receipt.byte_length != value.receipt.byte_length
            {
                return Err(corrupt("cached immutable identity changed"));
            }
            persisted_version = current.receipt.version.clone();
            current
        }
        None => value.clone(),
    };
    let record = journal.record(&value.object_id)?;
    if let Some(record) = &record {
        if record.intent.sha256 != value.ciphertext_sha256
            || record.intent.byte_length != value.receipt.byte_length
            || record.intent.role != value.role
        { return Err(corrupt("cached object differs from its sealed source")); }
    }
    let intent = ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: journal.job_id().to_owned(),
        object_id: value.object_id.clone(),
        role: value.role,
        byte_length: value.receipt.byte_length,
        sha256: value.ciphertext_sha256.clone(),
    };
    let key = verification_key(&verified)?;
    let known = journal.verification.receipts.get(&key).cloned();
    let mut historical = known.clone().unwrap_or_else(|| verified.receipt.clone());
    historical.checksum = None;
    // Persisted inventory versions are bound to this device's verified bytes.
    let unchanged = if output.is_none() {
        known.as_ref().and_then(|receipt| receipt.version.as_ref()).or(persisted_version.as_ref())
    } else { None };
    // Keyed to the reference this attempt is about to contact, which the
    // inventory above may have moved on from the one the caller held. A
    // reference already found unusable stays that way even though the cache
    // naming it can be rebuilt at any moment, so asking the provider again
    // would only repeat an answer already paid for. The record answers for
    // inherited references; an object still held in this job's own spool is
    // checked, because only that check can say whether its upload finished.
    let contacted = ObjectEvidence::identity(&verified, repository)?;
    let mut unusable = evidence.known(&contacted)?;
    let current = if unusable && record.is_none() {
        None
    } else {
        match transfer_job::verify_remote_receipt_at(
            journal.directory(), &intent, provider, repository, historical, cancel, unchanged, output,
        ).await {
            Ok(current) => current,
            // This reference failed verification. Authorization, quota and
            // transient answers say nothing about the object itself.
            Err(error) if error.kind == ErrorKind::Corrupt => {
                evidence.record(&contacted, &value.object_id, UnusableReason::Damaged)?;
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    };
    verified.receipt = match current {
        Some(receipt) => receipt,
        None => {
            if !unusable {
                evidence.record(&contacted, &value.object_id, UnusableReason::Missing)?;
                unusable = true;
            }
            journal.verification.receipts.remove(&key);
            journal.verification.catalogs.remove(&key);
            if record.is_none() {
                cache.forget_object(&value.repository_id, repository, &value.object_id)?;
                return Ok(None);
            }
            // The repository lost an object this job already released. Its
            // ciphertext exists nowhere, so this publication cannot repair it;
            // the recorded evidence makes the next one rebuild this object
            // alone.
            if record.is_some_and(|record| record.released) {
                return Err(ProviderError::new(ErrorKind::NotFound));
            }
            let mut receipt = transfer_job::upload_registered(
                journal,
                &value.object_id,
                &value.repository_id,
                root_key,
                value.plaintext_length,
                &value.plaintext_sha256,
                provider,
                repository,
                cancel,
            )
            .await?;
            if output.is_some() {
                receipt.checksum = None;
                receipt = transfer_job::verify_remote_receipt_at(
                    journal.directory(), &intent, provider, repository, receipt, cancel, None, output,
                ).await?.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            }
            receipt
        }
    };
    if journal.verification.receipts.len() >= 8192 {
        journal.verification = Default::default();
    }
    journal.verification.receipts.insert(verification_key(&verified)?, verified.receipt.clone());
    // Inherited remote references are not this device's upload inventory.
    if journal.record(&value.object_id)?.is_some()
        || cache.object(&value.repository_id, repository, &value.object_id, &value.plaintext_sha256)?.is_some()
    {
        cache.put_object(repository, &verified)?;
    }
    // The contacted reference answered in the end, so nothing about it is
    // still open. A repair that moved the object keeps the old record, which
    // now names a locator nothing reaches.
    if unusable && ObjectEvidence::identity(&verified, repository)? == contacted {
        evidence.clear(&contacted)?;
    }
    Ok(Some(verified))
}

/// A catalog cache hit is useful only when its entire authenticated closure
/// still exists. Checking the root alone misses missing shared packs and leaves.
async fn revalidate_cached_catalog(
    root: &RemoteObject,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<Vec<RemoteObject>>> {
    let metadata_key = derive_key(root_key, &root.repository_id, "metadata").map_err(corrupt)?;
    let mut pending = vec![root.clone()];
    let mut seen = BTreeMap::new();
    let mut verified = Vec::new();
    while let Some(object) = pending.pop() {
        cancel.check()?;
        let identity = super::gc_store::locator_key(&object.receipt.locator)?;
        let stored = object.stored(repository)?;
        if let Some(previous) = seen.insert(identity, stored.clone()) {
            if previous != stored {
                return Err(corrupt("conflicting catalog references"));
            }
            continue;
        }
        if object.repository_id != root.repository_id
            || !matches!(object.role, ObjectRole::Catalog | ObjectRole::Pack)
        {
            return Err(corrupt("invalid cached catalog child"));
        }
        let previous_locator = object.receipt.locator.clone();
        let temporary = tempfile::tempdir_in(journal.directory()).map_err(transient)?;
        let path = temporary.path().join("catalog");
        let key = verification_key(&object)?;
        let decoded = journal.verification.catalogs.get(&key).cloned();
        let output = (object.role == ObjectRole::Catalog && decoded.is_none()).then_some(path.as_path());
        let Some(object) = revalidate_cached_object_at(
            &object, cache, evidence, journal, root_key, provider, repository, cancel, output,
        ).await? else {
            return Ok(None);
        };
        // Recreating an object on an ID-based provider can change its locator.
        // The existing parent still names the old one and must be rebuilt.
        if object.object_id != root.object_id && object.receipt.locator != previous_locator {
            cache.forget_object(&root.repository_id, repository, &root.object_id)?;
            return Ok(None);
        }
        if object.role == ObjectRole::Catalog {
            if let Some(children) = decoded {
                pending.extend(children);
                verified.push(object);
                continue;
            }
            let stored = object.stored(repository)?;
            if object.plaintext_length > wire::MAX_METADATA_BYTES as u64 {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let mut input = crate::trust_boundary::open_regular_source(&path).map_err(transient)?;
            if hex::encode(hash_reader(&mut input, object.receipt.byte_length).map_err(corrupt)?)
                != object.ciphertext_sha256
            {
                evidence.record(
                    &ObjectEvidence::identity(&object, repository)?,
                    &object.object_id,
                    UnusableReason::Damaged,
                )?;
                return Err(corrupt("catalog ciphertext differs"));
            }
            input.seek(SeekFrom::Start(0)).map_err(transient)?;
            let mut plaintext = Vec::new();
            let header = wire::open_envelope(
                &mut input, &mut plaintext, &metadata_key, wire::MAX_METADATA_BYTES as u64,
            ).map_err(format_error)?;
            if header != stored.header
                || hex::encode(hash(&plaintext)) != object.plaintext_sha256
                || plaintext.len() as u64 != object.plaintext_length
            {
                evidence.record(
                    &ObjectEvidence::identity(&object, repository)?,
                    &object.object_id,
                    UnusableReason::Damaged,
                )?;
                return Err(corrupt("catalog plaintext differs"));
            }
            let document = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES)
                .map_err(corrupt)?;
            let mut children = Vec::new();
            for child in document.children {
                children.push(RemoteObject::from_stored(&child.object, repository)?);
            }
            for pack in document.packs {
                children.push(RemoteObject::from_stored(&pack, repository)?);
            }
            if journal.verification.catalog_children + children.len() <= 8192 {
                journal.verification.catalog_children += children.len();
                journal.verification.catalogs.insert(verification_key(&object)?, children.clone());
            }
            pending.extend(children);
        }
        verified.push(object);
    }
    Ok(Some(verified))
}


#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct EntryPlan {
    pub(super) kind: wire::CatalogEntryKind,
    pub(super) key: String,
    pub(super) content_sha256: String,
    pub(super) byte_length: u64,
    pub(super) chunks: Vec<wire::StoredChunk>,
    pub(super) packs: Vec<RemoteObject>,
}
impl EntryPlan {
    pub(super) fn validate(&self, format_repository_id: &str, repository: &RepositoryHandle) -> Result<()> {
        if self.key.is_empty() || !crate::trust_boundary::is_lower_hex_256(&self.content_sha256) {
            return Err(corrupt("invalid cached entry"));
        }
        let total = self.chunks.iter().try_fold(0u64, |sum, chunk| {
            sum.checked_add(chunk.plaintext_length)
                .ok_or_else(|| corrupt("entry length overflow"))
        })?;
        if total != self.byte_length {
            return Err(corrupt("cached entry length"));
        }
        let ids: BTreeSet<_> = self
            .packs
            .iter()
            .map(|pack| pack.object_id.as_str())
            .collect();
        for pack in &self.packs {
            pack.stored(repository)?;
            if pack.repository_id != format_repository_id {
                return Err(corrupt("cached pack repository"));
            }
        }
        if self
            .chunks
            .iter()
            .any(|chunk| !ids.contains(chunk.pack_id.as_str()))
        {
            return Err(corrupt("cached entry misses pack"));
        }
        Ok(())
    }
}

struct SourceEntry {
    kind: wire::CatalogEntryKind,
    key: String,
    content_sha256: String,
    byte_length: u64,
    source: ObjectSource,
    file_offset: Option<u64>,
    compression: CompressionPolicy,
}
#[derive(Clone)]
enum PendingChunk {
    /// Written into a pack this publication is building.
    Placed {
        pack_index: usize,
        offset: u64,
        stored_length: u64,
        plaintext_length: u64,
        plaintext_sha256: [u8; 32],
    },
    /// Already stored under the selected parent, at the placement it has.
    Carried(wire::StoredChunk),
}

/// Where the selected parent already stored one chunk of plaintext, by what
/// the chunk is and how it was encoded. An entry that changed at one end is
/// repackaged, and the part of it that did not change points at what is
/// already there instead of being encoded and uploaded again.
type ChunkPlacements = BTreeMap<(String, u64, u8), wire::StoredChunk>;

fn compression_tag(policy: CompressionPolicy) -> u8 {
    match policy {
        CompressionPolicy::Text => 0,
        CompressionPolicy::AlreadyCompressed => 1,
    }
}
struct PendingEntry {
    source: SourceEntry,
    chunks: Vec<PendingChunk>,
}
struct PendingPack {
    file: tempfile::NamedTempFile,
    length: u64,
    family: FamilyPermit,
}

/// The same envelope and SDK calculation bounds packing, metadata and upload.
/// Alignment of resumable non-final requests remains the provider's responsibility.
struct StoredSize<'a> {
    limits: PackageLimits,
    repository_id: &'a str,
}
impl StoredSize<'_> {
    fn ciphertext(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<u64> {
        let header = wire::PublicObjectHeader::new(
            self.repository_id.into(), object_id.into(), role, plaintext,
        ).map_err(corrupt)?;
        wire::envelope_length(&header).map_err(corrupt)
    }
    fn fits(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<bool> {
        Ok(self.ciphertext(object_id, role, plaintext)?.checked_add(self.limits.sdk_overhead_bytes)
            .is_some_and(|stored| stored <= self.limits.max_stored_bytes))
    }
    fn require(&self, object_id: &str, role: wire::ObjectRole, plaintext: u64) -> Result<u64> {
        let ciphertext = self.ciphertext(object_id, role, plaintext)?;
        if ciphertext.checked_add(self.limits.sdk_overhead_bytes)
            .is_none_or(|stored| stored > self.limits.max_stored_bytes)
        {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        Ok(ciphertext)
    }
    /// Pack chunking needs a capacity before its plaintext is assembled. Metadata
    /// instead measures its actual serialized document and has no capacity probe.
    fn pack_capacity(&self, object_id: &str) -> Result<u64> {
        let mut low = 0;
        let mut high = self.limits.max_stored_bytes.checked_sub(self.limits.sdk_overhead_bytes)
            .ok_or_else(|| ProviderError::new(ErrorKind::FileTooLarge))?;
        while low < high {
            let distance = high - low;
            let middle = low + distance / 2 + distance % 2;
            if self.fits(object_id, wire::ObjectRole::Pack, middle)? { low = middle; }
            else { high = middle - 1; }
        }
        if low < 128 { return Err(ProviderError::new(ErrorKind::FileTooLarge)); }
        Ok(low)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct OriginalBackupUnit {
    pub schema: String,
    pub key: risunest_sync_wire::unit::UnitKey,
    pub value: risunest_sync_wire::unit::UnitValue,
}

fn encode_original_unit(
    key: risunest_sync_wire::unit::UnitKey,
    value: risunest_sync_wire::unit::UnitValue,
) -> Result<Vec<u8>> {
    value.validate().map_err(corrupt)?;
    let record = OriginalBackupUnit { schema: "risunest.backup-unit/v1".into(), key, value };
    let bytes = risunest_sync_wire::canonical::encode(&record).map_err(corrupt)?;
    if bytes.len() > risunest_sync_wire::MAX_METADATA_BYTES {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
    Ok(bytes)
}

/// How many original units a bundle carries and what they come to once
/// encoded, read the way the catalog build reads them but keeping nothing.
fn original_unit_totals(units: &super::capture::OriginalBackupUnits, cancel: &Cancellation) -> Result<(u64, u64)> {
    let (mut items, mut bytes) = (0u64, 0u64);
    for unit in units.units().map_err(corrupt)? {
        if items % 128 == 0 {
            cancel.check()?;
        }
        let (key, value) = unit.map_err(corrupt)?;
        bytes = bytes.saturating_add(encode_original_unit(key, value)?.len() as u64);
        items += 1;
    }
    Ok((items, bytes))
}

fn original_unit_sources(
    units: impl Iterator<Item = crate::persistent_store::StoreResult<(risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue)>>,
    directory: &Path,
) -> Result<Vec<SourceEntry>> {
    let mut spool = tempfile::NamedTempFile::new_in(directory).map_err(transient)?;
    let mut offset = 0u64;
    let mut sources = Vec::new();
    for unit in units {
        let (key,value) = unit.map_err(corrupt)?;
        let bytes = encode_original_unit(key.clone(), value)?;
        spool.write_all(&bytes).map_err(transient)?;
        sources.push(SourceEntry {
            kind: wire::CatalogEntryKind::Record,
            key: format!("original-unit/{}", hex::encode(key.as_str())),
            content_sha256: hex::encode(hash(&bytes)),
            byte_length: bytes.len() as u64,
            source: ObjectSource::File(spool.path().into()),
            file_offset: Some(offset),
            compression: CompressionPolicy::Text,
        });
        offset = offset.checked_add(bytes.len() as u64).ok_or_else(|| corrupt("original unit spool size"))?;
    }
    spool.as_file_mut().sync_all().map_err(transient)?;
    let (_file, path) = spool.keep().map_err(transient)?;
    for source in &mut sources { source.source = ObjectSource::File(path.clone()); }
    Ok(sources)
}

#[allow(clippy::too_many_arguments)]
async fn upload_original_catalog_node(
    encoded: EncodedCatalog, format_repository_id:&str,build_root:&Path,root_key:&[u8;32],metadata_key:&[u8;32],limits:PackageLimits,
    cache:&mut PackageCache,evidence:&mut ObjectEvidence,journal:&mut TransferJournal,provider:&dyn Provider,
    repository:&RepositoryHandle,cancel:&Cancellation,referenced:&mut Vec<RemoteObject>,
)->Result<wire::CatalogChild> {
    let mut waiting=Vec::new();let mut uploaded=Vec::new();let mut bytes=0;
    let sealed=seal_catalog_node(&encoded.bytes,&mut waiting,&mut bytes,&mut uploaded,format_repository_id,root_key,metadata_key,
        limits,build_root,cache,evidence,journal,provider,repository,cancel).await?;
    let remote=upload_sealed_wave(vec![sealed],format_repository_id,root_key,cache,journal,provider,repository,cancel).await?
        .pop().ok_or_else(||corrupt("original unit catalog upload"))?;
    referenced.push(remote.clone());
    Ok(wire::CatalogChild{first_key:encoded.document.first_key,last_key:encoded.document.last_key,object:remote.stored(repository)?})
}

#[allow(clippy::too_many_arguments)]
async fn build_original_unit_catalog(
    units:&super::capture::OriginalBackupUnits,external_root:&Path,format_repository_id:&str,build_root:&Path,
    root_key:&[u8;32],limits:PackageLimits,cache:&mut PackageCache,evidence:&mut ObjectEvidence,journal:&mut TransferJournal,
    provider:&dyn Provider,repository:&RepositoryHandle,prepared:&Arc<PhaseProgress>,maintenance:&mut MaintenanceReport,
    hydration:&mut SourceHydration,cancel:&Cancellation,referenced:&mut Vec<RemoteObject>,
)->Result<RemoteObject> {
    let index=tempfile::NamedTempFile::new_in(build_root).map_err(transient)?;
    let nodes=crate::sqlite_open::open(index.path()).map_err(transient)?;
    nodes.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
        CREATE TABLE nodes(level INTEGER NOT NULL,ordinal INTEGER NOT NULL,body TEXT NOT NULL,PRIMARY KEY(level,ordinal))").map_err(transient)?;
    let size=StoredSize{limits,repository_id:format_repository_id};
    let metadata_key=derive_key(root_key,format_repository_id,"metadata").map_err(corrupt)?;
    let mut rows=units.units().map_err(corrupt)?.peekable();
    let mut count=0i64;
    loop {
        cancel.check()?;
        let sources=original_unit_sources(rows.by_ref().take(128),build_root)?;
        if sources.is_empty() && count!=0 {break;}
        let spool=sources.first().and_then(|source|source.source.file()).map(Path::to_owned);
        let (entries,objects)=build_entries(wire::CatalogKind::Records,sources,external_root,format_repository_id,build_root,root_key,limits,
            None,None,false,cache,evidence,journal,provider,repository,prepared,maintenance,hydration,cancel).await?;
        referenced.extend(objects);
        let packs=pack_locators(&entries,repository)?;
        let fragments=catalog_fragments(&entries)?;
        let leaves=catalog_leaves(wire::CatalogKind::Records,&fragments,&packs,&[],&size)?;
        for leaf in leaves {
            let child=upload_original_catalog_node(leaf,format_repository_id,build_root,root_key,&metadata_key,limits,
                cache,evidence,journal,provider,repository,cancel,referenced).await?;
            nodes.execute("INSERT INTO nodes VALUES(0,?1,?2)",rusqlite::params![count,serde_json::to_string(&child).map_err(corrupt)?]).map_err(transient)?;
            count+=1;
        }
        if let Some(spool)=spool {fs::remove_file(spool).map_err(transient)?;}
        if rows.peek().is_none() {break;}
    }
    let mut level=0u16;
    while count>1 {
        let next_level=level.checked_add(1).ok_or_else(||corrupt("original catalog depth"))?;
        let previous=count;let mut after=-1i64;count=0;
        loop {
            let page={
                let mut query=nodes.prepare("SELECT ordinal,body FROM nodes WHERE level=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 128").map_err(transient)?;
                let rows=query.query_map(rusqlite::params![level,after],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?))).map_err(transient)?;
                rows.collect::<std::result::Result<Vec<_>,_>>().map_err(transient)?
            };
            if page.is_empty() {break;}
            after=page.last().unwrap().0;
            let children=page.into_iter().map(|(_,body)|serde_json::from_str::<wire::CatalogChild>(&body).map_err(corrupt)).collect::<Result<Vec<_>>>()?;
            let batches=stable_batches(&children,&[],&size,|child|child.first_key.as_str(),
                |children|wire::CatalogDocument::branch(wire::CatalogKind::Records,next_level,children.to_vec()).map_err(corrupt))?;
            for batch in batches {
                cancel.check()?;
                let child=upload_original_catalog_node(batch,format_repository_id,build_root,root_key,&metadata_key,limits,
                    cache,evidence,journal,provider,repository,cancel,referenced).await?;
                nodes.execute("INSERT INTO nodes VALUES(?1,?2,?3)",rusqlite::params![next_level,count,serde_json::to_string(&child).map_err(corrupt)?]).map_err(transient)?;
                count+=1;
            }
        }
        if count>=previous {return Err(ProviderError::new(ErrorKind::FileTooLarge));}
        nodes.execute("DELETE FROM nodes WHERE level=?1",[level]).map_err(transient)?;
        level=next_level;
    }
    let body:String=nodes.query_row("SELECT body FROM nodes WHERE level=?1 AND ordinal=0",[level],|row|row.get(0)).map_err(corrupt)?;
    let child:wire::CatalogChild=serde_json::from_str(&body).map_err(corrupt)?;
    RemoteObject::from_stored(&child.object,repository)
}

fn capture_sources(capture: &CapturedSnapshot) -> Result<(Vec<SourceEntry>, Vec<SourceEntry>)> {
    let mut records = Vec::new();
    let mut query = capture
        .catalog
        .db
        .prepare("SELECT key,hash,bytes FROM records ORDER BY key")
        .map_err(corrupt)?;
    let mut rows = query.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let key: String = row.get(0).map_err(corrupt)?;
        let digest: String = row.get(1).map_err(corrupt)?;
        let bytes: i64 = row.get(2).map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("capture record hash is invalid"));
        }
        records.push(SourceEntry {
            kind: wire::CatalogEntryKind::Record,
            key,
            content_sha256: digest.clone(),
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            source: ObjectSource::Captured(digest),
            file_offset: None,
            compression: CompressionPolicy::Text,
        });
    }
    let mut generated = capture
        .catalog
        .db
        .prepare("SELECT g.hash,g.bytes FROM generated g LEFT JOIN original_payloads p ON p.hash=g.hash WHERE p.hash IS NULL ORDER BY g.hash")
        .map_err(corrupt)?;
    for row in generated
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(corrupt)?
    {
        let (digest, bytes) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("generated object hash is invalid"));
        }
        records.push(SourceEntry {
            kind: wire::CatalogEntryKind::Object,
            key: format!("object/{digest}"),
            content_sha256: digest.clone(),
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            source: ObjectSource::Captured(digest),
            file_offset: None,
            compression: CompressionPolicy::Text,
        });
    }
    records.sort_by(|a, b| a.key.cmp(&b.key));
    let mut assets = Vec::new();
    let mut query = capture.catalog.db.prepare("SELECT DISTINCT d.hash,d.bytes,g.hash IS NOT NULL FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash LEFT JOIN original_payloads p ON p.hash=d.hash WHERE g.hash IS NULL OR p.hash IS NOT NULL ORDER BY d.hash").map_err(corrupt)?;
    for row in query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_,bool>(2)?))
        })
        .map_err(corrupt)?
    {
        let (digest, bytes, captured) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&digest) {
            return Err(corrupt("capture dependency hash is invalid"));
        }
        assets.push(SourceEntry {
            kind: wire::CatalogEntryKind::Object,
            key: format!("object/{digest}"),
            content_sha256: digest.clone(),
            byte_length: u64::try_from(bytes).map_err(corrupt)?,
            source: if captured { ObjectSource::Captured(digest) } else { ObjectSource::Library(digest) },
            file_offset: None,
            compression: CompressionPolicy::AlreadyCompressed,
        });
    }
    Ok((records, assets))
}

/// One section per catalog, so the package cache key names the section as well
/// as its content. Two sections with the same entries are still two catalogs.
fn section_catalog_fingerprint(id: &str, sources: &[SourceEntry]) -> String {
    section_fingerprint(id, &catalog_fingerprint(wire::CatalogKind::Section, sources))
}

pub(super) fn section_fingerprint(id: &str, digest: &str) -> String {
    format!("{id}-{digest}")
}

fn catalog_fingerprint(kind: wire::CatalogKind, sources: &[SourceEntry]) -> String {
    catalog_fingerprint_of(
        kind,
        sources.iter().map(|source| {
            (
                source.key.as_str(),
                source.byte_length,
                source.content_sha256.as_str(),
            )
        }),
    )
}

/// The same digest read off what a catalog already holds, so a cache rebuilt
/// from the provider names a catalog the way the next publication will.
pub(super) fn catalog_fingerprint_of<'a>(
    kind: wire::CatalogKind,
    sources: impl Iterator<Item = (&'a str, u64, &'a str)>,
) -> String {
    let mut digest = Sha256::new();
    #[cfg(test)]
    crate::persistent_store::hash_work::begin("native_external_catalog_fingerprint");
    #[cfg(test)]
    crate::persistent_store::hash_work::update("native_external_catalog_fingerprint",b"risunest.external-catalog-fingerprint/v1\0".len()+kind_name(kind).len());
    digest.update(b"risunest.external-catalog-fingerprint/v1\0");
    digest.update(kind_name(kind).as_bytes());
    for (key, byte_length, content_sha256) in sources {
        #[cfg(test)]
        crate::persistent_store::hash_work::update("native_external_catalog_fingerprint",16+key.len()+content_sha256.len());
        digest.update((key.len() as u64).to_le_bytes());
        digest.update(key.as_bytes());
        digest.update(byte_length.to_le_bytes());
        digest.update(content_sha256.as_bytes());
    }
    hex::encode(digest.finalize())
}

/// Whether every pack an entry names is in the selected parent graph and is
/// not something this connection found unusable.
fn admitted_packs(
    packs: &[RemoteObject],
    members: &BTreeSet<String>,
    evidence: &ObjectEvidence,
    settled: &mut BTreeMap<String, bool>,
    repository: &RepositoryHandle,
) -> Result<bool> {
    for pack in packs {
        let identity = ObjectEvidence::identity(pack, repository)?;
        if !members.contains(&identity) {
            return Ok(false);
        }
        let known = match settled.get(&identity) {
            Some(known) => *known,
            None => {
                let known = evidence.known(&identity)?;
                settled.insert(identity, known);
                known
            }
        };
        if known {
            return Ok(false);
        }
    }
    Ok(true)
}

fn source_totals(sources: &[SourceEntry]) -> (u64, u64) {
    (
        sources.len() as u64,
        sources.iter().fold(0u64, |sum, source| sum.saturating_add(source.byte_length)),
    )
}

fn remember_pack_failure(first: &std::sync::Mutex<Option<ProviderError>>, error: &ProviderError) {
    let mut first = first.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if first.is_none() { *first = Some(error.clone()); }
}

/// Counts each source in `prepared` once it is done. The caller planned them.
async fn build_entries(
    kind: wire::CatalogKind,
    sources: Vec<SourceEntry>,
    external_root: &Path,
    format_repository_id: &str,
    build_root: &Path,
    root_key: &[u8; 32],
    limits: PackageLimits,
    parent: Option<&BTreeSet<String>>,
    shape: Option<&CatalogShape>,
    root_cached: bool,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    prepared: &Arc<PhaseProgress>,
    maintenance: &mut MaintenanceReport,
    hydration: &mut SourceHydration,
    cancel: &Cancellation,
) -> Result<(Vec<EntryPlan>, Vec<RemoteObject>)> {
    let size = StoredSize { limits, repository_id: format_repository_id };
    let max_pack = size.pack_capacity(&format!("pack-{}", "0".repeat(64)))?;
    let preferred = match kind {
        wire::CatalogKind::Assets => MAX_PACK_PLAINTEXT_BYTES,
        wire::CatalogKind::Records | wire::CatalogKind::Section => 32 * 1024 * 1024,
    };
    let target = limits.target_plaintext_bytes.min(preferred).min(max_pack).max(1);
    let chunk_bytes = usize::try_from(
        max_pack
            .saturating_sub(ENTRY_OVERHEAD + 1)
            .min(MAX_CHUNK_BYTES as u64),
    )
    .map_err(corrupt)?;
    if chunk_bytes == 0 {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
    // Which keys this cycle writes again rather than points at where they are:
    // the record catalog only, and only where a publication that is already
    // required was told it may also spend the maintenance portion. Decided
    // before anything is uploaded, because a body the capture store cannot
    // answer for has to keep the placement it has, and declining reuse for it
    // would turn an optional cycle into a failed publication. A pack counts as
    // live only through an entry of this capture that the parent graph still
    // names, and a cycle is laid out against the leaves of the parent's
    // catalog, so without one there is nothing to select.
    let mut drained: BTreeSet<String> = BTreeSet::new();
    let mut repackage: BTreeSet<String> = BTreeSet::new();
    // One answer per pack, not per entry that names it.
    let mut settled: BTreeMap<String, bool> = BTreeMap::new();
    let ranges = shape.map(|shape| shape.level(0)).unwrap_or_default();
    if kind == wire::CatalogKind::Records
        && limits.maintenance == Maintenance::Allowed
        && !ranges.is_empty()
    {
        if let (Some(members), Some(content)) = (
            parent,
            ContentStore::open_existing(external_root).map_err(transient)?,
        ) {
            let mut live: BTreeMap<String, LivePack> = BTreeMap::new();
            let mut reusable: Vec<Option<EntryPlan>> = Vec::with_capacity(sources.len());
            let mut template: Option<RemoteObject> = None;
            for source in &sources {
                cancel.check()?;
                let Some(cached) = cache.entry(
                    format_repository_id,
                    repository,
                    kind,
                    &source.key,
                    &source.content_sha256,
                    source.byte_length,
                )?
                else {
                    reusable.push(None);
                    continue;
                };
                if !admitted_packs(&cached.packs, members, evidence, &mut settled, repository)? {
                    reusable.push(None);
                    continue;
                }
                let held = content
                    .stat(&source.content_sha256)
                    .is_ok_and(|held| held == Some(source.byte_length));
                let leaves = previous_leaves(ranges, &source.key);
                for pack in &cached.packs {
                    let named = live.entry(pack.object_id.clone()).or_insert_with(|| LivePack {
                        object_id: pack.object_id.clone(),
                        ciphertext_length: pack.receipt.byte_length,
                        bytes: 0,
                        first_key: source.key.clone(),
                        leaves: BTreeSet::new(),
                        keys: Vec::new(),
                        rewritable: true,
                    });
                    named.bytes = named.bytes.saturating_add(source.byte_length);
                    named.rewritable &= held;
                    if named.ciphertext_length <= SMALL_PACK_CIPHERTEXT_BYTES {
                        named.leaves.extend(leaves.clone());
                        named.keys.push((source.key.clone(), source.byte_length));
                    }
                    if template.as_ref().is_none_or(|kept| locator_length(kept) < locator_length(pack)) {
                        template = Some(pack.clone());
                    }
                }
                reusable.push(Some(cached));
            }
            let run = select_small_packs(live.into_values().collect());
            if let (false, Some(template)) = (run.is_empty(), template.as_ref()) {
                // The run is cut to the leaves the catalog writer would
                // produce for it before a single pack of it is built.
                let plan = CoalescingPlan {
                    kind,
                    sources: &sources,
                    reusable: &reusable,
                    template,
                    ranges,
                    size: &size,
                    target,
                    chunk_bytes: chunk_bytes as u64,
                    format_repository_id,
                    repository,
                    members,
                    cache: &*cache,
                    evidence: &*evidence,
                };
                let (run, added, required) = plan.bound(run, root_cached, cancel)?;
                if !run.is_empty() {
                    maintenance.leaves = added as u64;
                    maintenance.required_leaves = required as u64;
                }
                for pack in run {
                    for (key, bytes) in pack.keys {
                        if repackage.insert(key) {
                            maintenance.source_bytes = maintenance.source_bytes.saturating_add(bytes);
                        }
                    }
                    drained.insert(pack.object_id);
                }
            }
        }
    }
    maintenance.packs = drained.len() as u64;
    let mut ready = Vec::new();
    let mut uncached = Vec::new();
    let mut placements: ChunkPlacements = BTreeMap::new();
    let mut carried_packs: BTreeMap<String, RemoteObject> = BTreeMap::new();
    for source in sources {
        cancel.check()?;
        if let Some(mut cached) = cache.entry(
            format_repository_id,
            repository,
            kind,
            &source.key,
            &source.content_sha256,
            source.byte_length,
        )? {
            // A body sitting in one of the small packs this cycle drains is
            // written again from the capture beside it, rather than pointed at
            // where it is. The old pack is left for the snapshots that name it.
            if repackage.contains(&source.key) {
                uncached.push(source);
                continue;
            }
            match parent {
                // Every pack this entry names is in the selected parent graph,
                // which this job protects, so their existence is settled and
                // nothing is asked about them. They are not verified here. What
                // this connection already found unusable is settled by no graph
                // and never inherits.
                Some(members) => {
                    if admitted_packs(&cached.packs, members, evidence, &mut settled, repository)? {
                        prepared.completed(source.byte_length);
                        ready.push(cached);
                        continue;
                    }
                }
                // Without a parent there is nothing to reuse under, so the
                // entry is admitted only while its packs answer for themselves.
                None => {
                    let mut exists = true;
                    for pack in &mut cached.packs {
                        match revalidate_cached_object(
                            pack,
                            cache,
                            evidence,
                            journal,
                            root_key,
                            provider,
                            repository,
                            cancel,
                        )
                        .await?
                        {
                            Some(current) => *pack = current,
                            None => { exists = false; break; }
                        }
                    }
                    if exists {
                        prepared.completed(source.byte_length);
                        ready.push(cached);
                        continue;
                    }
                }
            }
        }
        // A changed entry is repackaged, but a large one usually changed at
        // one end. What the parent already placed for its previous version is
        // what the part that did not change points at.
        if let Some(members) = parent {
            if let Some(previous) =
                cache.entry_by_key(format_repository_id, repository, kind, &source.key)?
            {
                if !repackage.contains(&source.key)
                    && admitted_packs(&previous.packs, members, evidence, &mut settled, repository)?
                {
                    for chunk in &previous.chunks {
                        placements.insert(
                            (
                                hex::encode(chunk.plaintext_sha256),
                                chunk.plaintext_length,
                                compression_tag(source.compression),
                            ),
                            chunk.clone(),
                        );
                    }
                    for pack in previous.packs {
                        carried_packs.insert(pack.object_id.clone(), pack);
                    }
                }
            }
        }
        uncached.push(source);
    }
    let build_root_owned = build_root.to_path_buf();
    let external_root_owned = external_root.to_path_buf();
    let repository_root_owned = external_root
        .parent()
        .ok_or_else(|| corrupt("external storage root has no repository"))?
        .to_path_buf();
    let stop = Cancellation::default();
    let first_failure = Arc::new(std::sync::Mutex::new(None));
    let producer_failure = first_failure.clone();
    let pending_uploads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut sealing_journal = journal.preparation_handle(pending_uploads.clone())?;
    let mut uploading_cache = PackageCache::open(build_root.parent().ok_or_else(|| corrupt("build root"))?)?;
    let data_key = derive_key(root_key, format_repository_id, "data").map_err(corrupt)?;
    let cancel_owned = stop.clone();
    let producer_prepared = Arc::clone(prepared);
    let placed = placements;
    // Plaintext preparation retains the global family and CPU bounds. Sealed
    // files release the family and remain charged to the shared spool budget.
    let (built, mut arriving) = tokio::sync::mpsc::channel(1);
    let (sealed_tx, mut sealed_rx) = tokio::sync::mpsc::channel(16);
    let handle = tokio::runtime::Handle::current();
    let families = journal.families();
    let producer = spawn_blocking(move || {
        let result = prepare_packs(
            uncached, &repository_root_owned, &external_root_owned, &build_root_owned,
            target, max_pack, chunk_bytes, &placed, &built, &handle, &families,
            &producer_prepared, &cancel_owned,
        );
        if let Err(error) = &result {
            remember_pack_failure(&producer_failure, error);
            cancel_owned.cancel();
        }
        result
    });
    let sealing = async {
        let outcome: Result<()> = async {
            while let Some(mut pack) = arriving.recv().await {
                stop.check()?;
                pack.file.as_file_mut().seek(SeekFrom::Start(0)).map_err(transient)?;
                let plain_hash = hash_reader(pack.file.as_file_mut(), pack.length).map_err(corrupt)?;
                let id = wire::keyed_object_id(&data_key, sealing_journal.job_id(),
                    wire::ObjectRole::Pack, &plain_hash).map_err(corrupt)?;
                let object = seal_plain_object(
                    pack.file.path(), pack.length, plain_hash, id, ObjectRole::Pack,
                    format_repository_id, root_key, &data_key, limits, 0, cache, evidence,
                    &mut sealing_journal, provider, repository, &stop,
                ).await?.ok_or_else(|| ProviderError::new(ErrorKind::Transient))?;
                let PendingPack { file, family, .. } = pack;
                drop(file);
                drop(family);
                if matches!(object, SealedObject::Pending { .. }) {
                    pending_uploads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                tokio::select! {
                    result = sealed_tx.send(object) => result.map_err(transient)?,
                    _ = stop.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
                }
            }
            Ok(())
        }.await;
        if let Err(error) = &outcome {
            remember_pack_failure(&first_failure, error);
            stop.cancel();
        }
        drop(arriving);
        drop(sealed_tx);
        // Closing the plaintext channel releases a blocked producer; always
        // join it, including cancellation, spool pressure and upload failures.
        let produced = producer.await.map_err(transient).and_then(|result| result);
        if let Err(error) = &produced {
            remember_pack_failure(&first_failure, error);
            stop.cancel();
        }
        outcome?;
        produced
    };
    let uploading = async {
        let outcome: Result<Vec<RemoteObject>> = async {
            let mut uploaded = Vec::new();
            loop {
                let Some(first) = sealed_rx.recv().await else { break; };
                stop.check()?;
                let limit = provider.transfer_concurrency().max(1);
                let mut wave = vec![first];
                // Never await a second pack: another job may need the spool
                // held by this wave before it can produce anything else.
                while wave.len() < limit {
                    match sealed_rx.try_recv() {
                        Ok(object) => wave.push(object),
                        Err(_) => break,
                    }
                }
                let pending = waiting(&wave);
                let result = upload_sealed_wave(wave, format_repository_id, root_key,
                    &mut uploading_cache, journal, provider, repository, cancel).await;
                journal.settle_pipeline_wave(&pending_uploads, pending);
                uploaded.extend(result?);
            }
            Ok(uploaded)
        }.await;
        if let Err(error) = &outcome {
            remember_pack_failure(&first_failure, error);
            stop.cancel();
        }
        drop(sealed_rx);
        outcome
    };
    let mut pipeline = Box::pin(async { tokio::join!(sealing, uploading) });
    let (produced, uploaded) = tokio::select! {
        result = &mut pipeline => result,
        _ = cancel.cancelled() => { stop.cancel(); pipeline.as_mut().await }
    };
    // Release the future's mutable borrows before returning the verification
    // evidence collected by the sequential sealing stage to its owner.
    drop(pipeline);
    if let Some(error) = first_failure.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
        return Err(error);
    }
    let uploaded_packs = uploaded?;
    let (pending, fetched) = produced?;
    journal.verification = sealing_journal.verification;
    hydration.objects += fetched.objects;
    hydration.bytes += fetched.bytes;
    let mut new_entries = Vec::with_capacity(pending.len());
    for entry in pending {
        let mut used: BTreeMap<String, RemoteObject> = BTreeMap::new();
        let chunks = entry
            .chunks
            .into_iter()
            .map(|chunk| match chunk {
                PendingChunk::Placed {
                    pack_index,
                    offset,
                    stored_length,
                    plaintext_length,
                    plaintext_sha256,
                } => {
                    let pack = uploaded_packs
                        .get(pack_index)
                        .ok_or_else(|| corrupt("missing completed pack"))?;
                    used.insert(pack.object_id.clone(), pack.clone());
                    Ok(wire::StoredChunk {
                        pack_id: pack.object_id.clone(),
                        offset,
                        stored_length,
                        plaintext_length,
                        plaintext_sha256,
                    })
                }
                PendingChunk::Carried(stored) => {
                    let pack = carried_packs
                        .get(&stored.pack_id)
                        .ok_or_else(|| corrupt("missing carried pack"))?;
                    used.insert(pack.object_id.clone(), pack.clone());
                    Ok(stored)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = EntryPlan {
            kind: entry.source.kind,
            key: entry.source.key,
            content_sha256: entry.source.content_sha256,
            byte_length: entry.source.byte_length,
            chunks,
            packs: used.into_values().collect(),
        };
        new_entries.push(plan);
    }
    cache.put_entries(format_repository_id, repository, kind, &new_entries)?;
    ready.extend(new_entries);
    ready.sort_by(|a, b| a.key.cmp(&b.key));
    let mut referenced = BTreeMap::new();
    for entry in &ready {
        for pack in &entry.packs {
            referenced.insert(pack.object_id.clone(), pack.clone());
        }
    }
    Ok((ready, referenced.into_values().collect()))
}

/// Runs on the blocking pool, where the CPU work and all capture file I/O
/// belong, and emits plaintext pack files and bounded metadata only. Each pack
/// is handed over as soon as it is complete, and the CPU permit is held only
/// while one is being built, so sending a finished pack and preparing the next
/// one overlap.
#[allow(clippy::too_many_arguments)]
fn prepare_packs(
    sources: Vec<SourceEntry>,
    repository_root: &Path,
    external_root: &Path,
    build_root: &Path,
    target: u64,
    max_pack: u64,
    chunk_bytes: usize,
    placements: &ChunkPlacements,
    built: &tokio::sync::mpsc::Sender<PendingPack>,
    handle: &tokio::runtime::Handle,
    families: &Arc<PackFamilies>,
    prepared: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<(Vec<PendingEntry>, SourceHydration)> {
    fs::create_dir_all(build_root).map_err(transient)?;
    if crate::trust_boundary::is_link_like(&fs::symlink_metadata(build_root).map_err(transient)?) {
        return Err(corrupt("snapshot build directory is a link"));
    }
    let content = ContentStore::open(external_root).map_err(transient)?;
    let mut encoder = ChunkEncoder::new().map_err(corrupt)?;
    let mut completed = 0usize;
    let mut current: Option<PendingPack> = None;
    let mut pending = Vec::new();
    let mut cpu = Some(handle.block_on(cpu_permit())?);
    let mut hydration = SourceHydration::default();
    let mut payloads = None;
    for source in sources {
        cancel.check()?;
        let (mut input, input_length): (Box<dyn Read>, u64) = match &source.source {
            ObjectSource::Library(digest) => {
                let payloads = match &mut payloads {
                    Some(payloads) => payloads,
                    None => payloads
                        .insert(PayloadCas::new(repository_root).map_err(transient)?),
                };
                // Hydration decrypts under the same CPU budget. No producer
                // permit may be retained while opening a remote source.
                cpu.take();
                let source = open_library_source(payloads, digest, &mut hydration, cancel)?;
                cancel.check()?;
                cpu = Some(handle.block_on(cpu_permit())?);
                source
            }
            ObjectSource::File(path) if source.file_offset.is_some() => {
                let offset = source.file_offset.unwrap();
                let mut file = crate::trust_boundary::open_regular_source(path).map_err(transient)?;
                let end = offset.checked_add(source.byte_length).ok_or_else(|| corrupt("section source range"))?;
                if end > file.metadata().map_err(transient)?.len() { return Err(corrupt("section source range")); }
                file.seek(SeekFrom::Start(offset)).map_err(transient)?;
                (Box::new(file.take(source.byte_length)), source.byte_length)
            }
            other => {
                let body = content.open_source(other).map_err(corrupt)?;
                let length = body.len().map_err(transient)?;
                (Box::new(body), length)
            },
        };
        if input_length != source.byte_length {
            return Err(corrupt("capture source length differs"));
        }
        let mut remaining = source.byte_length;
        let mut source_hash = Sha256::new();
        #[cfg(test)]
        crate::persistent_store::hash_work::begin("native_external_pack_source");
        let mut chunks = Vec::new();
        loop {
            cancel.check()?;
            let count = if remaining == 0 && chunks.is_empty() {
                0
            } else {
                remaining.min(chunk_bytes as u64) as usize
            };
            let mut bytes = vec![0; count];
            input.read_exact(&mut bytes).map_err(corrupt)?;
            source_hash.update(&bytes);
            #[cfg(test)]
            crate::persistent_store::hash_work::update("native_external_pack_source",bytes.len());
            let chunk = Chunk {
                hash: hash(&bytes),
                bytes,
            };
            let placement = placements.get(&(
                hex::encode(chunk.hash),
                count as u64,
                compression_tag(source.compression),
            ));
            if let Some(stored) = placement {
                chunks.push(PendingChunk::Carried(stored.clone()));
                #[cfg(test)]
                tests::observe_boundary();
            } else {
                let mut encoded = Vec::new();
                let stored_length = pack::write_entry_with(&mut encoded, &chunk, &mut encoder, source.compression);
                #[cfg(test)]
                if stored_length.is_ok() {crate::persistent_store::hash_work::observe("native_external_pack_entry_validation",chunk.bytes.len());}
                else {crate::persistent_store::hash_work::incomplete("native_external_pack_entry_validation");}
                let stored_length=stored_length.map_err(corrupt)?;
                if current
                    .as_ref()
                    .is_some_and(|pack| pack_full(pack.length, stored_length, target))
                {
                    send_pack(current.take().unwrap(), built, &mut cpu)?;
                    completed += 1;
                }
                if stored_length > max_pack {
                    return Err(ProviderError::new(ErrorKind::FileTooLarge));
                }
                if current.is_none() {
                    let family = family_permit(families, built, handle, &mut cpu, cancel)?;
                    current = Some(PendingPack {
                        file: tempfile::NamedTempFile::new_in(build_root).map_err(transient)?,
                        length: 0,
                        family,
                    });
                }
                let pack = current.as_mut().unwrap();
                if pack.length + stored_length > max_pack {
                    return Err(ProviderError::new(ErrorKind::FileTooLarge));
                }
                let pack_index = completed;
                let offset = pack.length;
                pack.file
                    .as_file_mut()
                    .write_all(&encoded)
                    .map_err(transient)?;
                pack.length += stored_length;
                chunks.push(PendingChunk::Placed {
                    pack_index,
                    offset,
                    stored_length,
                    plaintext_length: count as u64,
                    plaintext_sha256: chunk.hash,
                });
            }
            if remaining == 0 {
                break;
            }
            remaining -= count as u64;
            if remaining == 0 {
                break;
            }
        }
        let actual = hex::encode(source_hash.finalize());
        if actual != source.content_sha256 {
            return Err(corrupt("capture source hash differs"));
        }
        prepared.completed(source.byte_length);
        pending.push(PendingEntry { source, chunks });
    }
    if let Some(pack) = current {
        send_pack(pack, built, &mut cpu)?;
    }
    Ok((pending, hydration))
}

/// A library body where the asset repository holds it, or fetched through its
/// custody first. A held body is opened where it is, as any capture source is.
/// A fetch is verified against the body's hash and custody size before
/// anything reads it, and needs that much free space on the repository volume.
struct TransientLibrarySource {
    body: crate::server_sync::residency::TransientBody,
    _scratch: tempfile::TempDir,
}
struct TransientExternalSource {
    body: tempfile::NamedTempFile,
    _scratch: tempfile::TempDir,
}
impl Read for TransientExternalSource {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.body.as_file_mut().read(buffer)
    }
}
impl Read for TransientLibrarySource {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.body.read(buffer)
    }
}

fn open_library_source(
    payloads: &PayloadCas,
    digest: &str,
    hydration: &mut SourceHydration,
    cancel: &Cancellation,
) -> Result<(Box<dyn Read>, u64)> {
    use crate::server_sync::residency::{open_transient_with_check, Residency};
    if let Some(file) = payloads.open_object(digest).map_err(transient)? {
        let length = file.metadata().map_err(transient)?.len();
        return Ok((Box::new(file), length));
    }
    let custody = |error: crate::server_sync::SyncError| {
        if cancel.check().is_err() {
            ProviderError::new(ErrorKind::Cancelled)
        } else {
            transient(error.code)
        }
    };
    let repository_root = payloads.repository_root();
    let custody_size = if Residency::exists(repository_root) {
        Residency::open(repository_root)
            .and_then(|residency| residency.object(digest, None))
            .map_err(custody)?
            .map(|object| object.size)
    } else {
        None
    };
    let Some(size) = custody_size else {
        let scratch = super::leftovers::managed_scratch(repository_root, "asset-transient-")?;
        let body = tauri::async_runtime::block_on(super::lww_residency::spool_verified_remote_body(
            repository_root,digest,scratch.path(),cancel,
        ))?.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        let length = body.as_file().metadata().map_err(transient)?.len();
        hydration.objects += 1;
        hydration.bytes += length;
        return Ok((Box::new(TransientExternalSource {body,_scratch:scratch}),length));
    };
    if fs2::available_space(repository_root).map_err(transient)? < size {
        return Err(ProviderError::new(ErrorKind::LocalStorageFull));
    }
    let check = || {
        cancel
            .check()
            .map_err(|_| crate::server_sync::SyncError::new("cancelled", 409))
    };
    let scratch = super::leftovers::managed_scratch(repository_root, "asset-transient-")?;
    let file = open_transient_with_check(repository_root, scratch.path(), digest, &check)
        .map_err(custody)?
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    hydration.objects += 1;
    hydration.bytes += size;
    let length = file.len().map_err(transient)?;
    Ok((Box::new(TransientLibrarySource { body: file, _scratch: scratch }), length))
}

/// The pack is durable before it is handed over, and the producer holds no CPU
/// permit while it waits for the consumer to take it. The permit is taken
/// again with the family of the next pack.
fn send_pack(
    mut pack: PendingPack,
    built: &tokio::sync::mpsc::Sender<PendingPack>,
    cpu: &mut Option<tokio::sync::OwnedSemaphorePermit>,
) -> Result<()> {
    pack.file.as_file_mut().sync_all().map_err(transient)?;
    cpu.take();
    built.blocking_send(pack).map_err(transient)?;
    Ok(())
}

/// Starts the family of the next pack before its plaintext file exists, then
/// takes the CPU permit to build it. A producer that has to wait for a family
/// holds no CPU permit meanwhile, so the packs already built can still be
/// sealed and sent.
fn family_permit(
    families: &Arc<PackFamilies>,
    built: &tokio::sync::mpsc::Sender<PendingPack>,
    handle: &tokio::runtime::Handle,
    cpu: &mut Option<tokio::sync::OwnedSemaphorePermit>,
    cancel: &Cancellation,
) -> Result<FamilyPermit> {
    // A consumer that stopped takes no more packs, so none is started for it.
    if built.is_closed() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    let permit = match families.try_acquire()? {
        Some(permit) => permit,
        None => {
            cpu.take();
            let permit = handle.block_on(families.acquire(cancel, built.closed()))?;
            // A consumer that stopped may have released the family this took.
            if built.is_closed() {
                return Err(ProviderError::new(ErrorKind::Cancelled));
            }
            permit
        }
    };
    if cpu.is_none() {
        *cpu = Some(handle.block_on(cpu_permit())?);
    }
    Ok(permit)
}

/// One object that either needs no upload or is sealed and registered,
/// waiting for the wave it belongs to.
enum SealedObject {
    Ready(RemoteObject),
    Pending {
        object_id: String,
        role: ObjectRole,
        plaintext_length: u64,
        plaintext_sha256: String,
        ciphertext_sha256: String,
    },
}

/// How many objects of `sealed` still wait for their registration.
fn waiting(sealed: &[SealedObject]) -> usize {
    sealed
        .iter()
        .filter(|object| matches!(object, SealedObject::Pending { .. }))
        .count()
}

fn released_plain_object(
    record: &super::journal::TransferRecord,
    intent: &ObjectIntent,
    plaintext_length: u64,
    plaintext_sha256: &str,
) -> Result<SealedObject> {
    if !record.released || record.intent != *intent {
        return Err(corrupt("journal ciphertext disappeared before release"));
    }
    let Some((length, digest)) = &record.plaintext else {
        return Err(corrupt("released object was never registered"));
    };
    if *length != plaintext_length || digest != plaintext_sha256 {
        return Err(corrupt("journal plaintext differs"));
    }
    Ok(SealedObject::Pending {
        object_id: intent.object_id.clone(),
        role: intent.role,
        plaintext_length,
        plaintext_sha256: plaintext_sha256.into(),
        ciphertext_sha256: intent.sha256.clone(),
    })
}

/// Seals one object to join a wave in which `wave` objects already wait.
/// `None` means the spool has no room for it yet: the caller sends the wave
/// it holds, which releases that room, and asks again.
#[allow(clippy::too_many_arguments)]
async fn seal_plain_object(
    path: &Path,
    plaintext_length: u64,
    plaintext_hash: [u8; 32],
    object_id: String,
    role: ObjectRole,
    format_repository_id: &str,
    root_key: &[u8; 32],
    key: &[u8; 32],
    limits: PackageLimits,
    wave: usize,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<SealedObject>> {
    let plaintext_sha256 = hex::encode(plaintext_hash);
    if let Some(value) = cache.object(
        format_repository_id,
        repository,
        &object_id,
        &plaintext_sha256,
    )? {
        if value.object_id != object_id || value.role != role
            || value.plaintext_length != plaintext_length
            || value.plaintext_sha256 != plaintext_sha256
        {
            return Err(corrupt("cached plaintext identity differs"));
        }
        if let Some(current) = revalidate_cached_object(
            &value, cache, evidence, journal, root_key, provider, repository, cancel,
        ).await? {
            return Ok(Some(SealedObject::Ready(current)));
        }
    }
    let wire_role = wire_role(role)?;
    let header = wire::PublicObjectHeader::new(
        format_repository_id.into(),
        object_id.clone(),
        wire_role,
        plaintext_length,
    )
    .map_err(corrupt)?;
    let ciphertext_length = StoredSize { limits, repository_id: format_repository_id }
        .require(&object_id, wire_role, plaintext_length)?;
    if let Some(record) = journal.record(&object_id)? {
        if record.intent.role != role || record.intent.byte_length != ciphertext_length {
            return Err(corrupt("journal role differs"));
        }
        // A released object is already durable under exactly these bytes, and
        // the registration says which. Rebuilding the same plaintext is the
        // whole proof, so nothing is read back.
        if record.released {
            return released_plain_object(&record, &record.intent, plaintext_length, &plaintext_sha256).map(Some);
        }
        let spool = journal.spool_path(&object_id);
        let downloaded;
        let cipher_path = if spool.exists() {
            spool.clone()
        } else if let Some(receipt) = &record.receipt {
            validate_receipt(&record.intent, repository, receipt)?;
            // The copy read back to prove the object is charged to the spool
            // budget too, until the directory holding it is gone.
            let admission = if wave == 0 {
                journal.reserve_spool_after_release(record.intent.byte_length, cancel).await?
            } else { journal.reserve_spool(record.intent.byte_length)? };
            let room = match admission {
                SpoolAdmission::Admitted(reservation) => reservation,
                SpoolAdmission::Full => return Ok(None),
            };
            let parent = path
                .parent()
                .ok_or_else(|| corrupt("plaintext path has no parent"))?;
            downloaded = (tempfile::tempdir_in(parent).map_err(transient)?, room);
            let downloaded_path = downloaded.0.path().join("ciphertext");
            let mut sink = SpoolSink::create(&downloaded_path, record.intent.byte_length)?;
            let read = provider
                .read_object(repository, &receipt.locator, None, &mut sink, cancel)
                .await?;
            if !matches!(read, ReadReceipt::Body(body) if body.complete && body.locator == receipt.locator && body.byte_length == record.intent.byte_length)
                || !sink.is_verified()
            {
                return Err(corrupt(
                    "completed journal object could not be reconstructed",
                ));
            }
            downloaded_path
        } else {
            let current = journal.record(&object_id)?.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            return released_plain_object(&current, &record.intent, plaintext_length, &plaintext_sha256).map(Some);
        };
        let expected_ciphertext = record.intent.sha256.clone();
        let expected_plaintext = plaintext_hash;
        let header_copy = header.clone();
        let key_copy = *key;
        let cpu = cpu_permit().await?;
        let verified = spawn_blocking(move || {
            let mut input = match crate::trust_boundary::open_regular_source(&cipher_path) {
                Ok(input) => input,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(corrupt(error)),
            };
            if input.metadata().map_err(corrupt)?.len() != ciphertext_length
                || hex::encode(hash_reader(&mut input, ciphertext_length).map_err(corrupt)?)
                    != expected_ciphertext
            {
                return Err(corrupt("journal ciphertext differs"));
            }
            input.seek(SeekFrom::Start(0)).map_err(corrupt)?;
            struct HashSink {
                digest: Sha256,
                length: u64,
            }
            impl Write for HashSink {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.digest.update(bytes);
                    #[cfg(test)]
                    crate::persistent_store::hash_work::update("native_external_journal_plaintext",bytes.len());
                    self.length = self
                        .length
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| std::io::Error::other("plaintext length overflow"))?;
                    Ok(bytes.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            #[cfg(test)]
            crate::persistent_store::hash_work::begin("native_external_journal_plaintext");
            let mut output = HashSink {
                digest: Sha256::new(),
                length: 0,
            };
            let opened = wire::open_envelope(&mut input, &mut output, &key_copy, plaintext_length)
                .map_err(format_error)?;
            let actual: [u8; 32] = output.digest.finalize().into();
            if opened != header_copy
                || output.length != plaintext_length
                || actual != expected_plaintext
            {
                return Err(corrupt("journal plaintext differs"));
            }
            Ok(true)
        })
        .await
        .map_err(transient)??;
        drop(cpu);
        if !verified {
            // Upload completion can release the spool while this worker waits
            // for CPU. Only the same registered, durable object replaces that proof.
            let current = journal.record(&object_id)?.ok_or_else(|| corrupt("journal object disappeared"))?;
            return released_plain_object(&current, &record.intent, plaintext_length, &plaintext_sha256).map(Some);
        }
        return Ok(Some(SealedObject::Pending {
            object_id,
            role,
            plaintext_length,
            plaintext_sha256,
            ciphertext_sha256: record.intent.sha256,
        }));
    }
    // Held until the journal answers for the file, so another job always sees
    // either the reservation or the bytes. The room includes the page that
    // registers this object with the `wave` already waiting beside it.
    let bytes = ciphertext_length.saturating_add(super::control::inventory_page_headroom(journal.registration_headroom_members(wave)));
    let admission = if wave == 0 {
        journal.reserve_spool_after_release(bytes, cancel).await?
    } else { journal.reserve_spool(bytes)? };
    let reservation = match admission {
        SpoolAdmission::Admitted(reservation) => reservation,
        SpoolAdmission::Full => return Ok(None),
    };
    let spool = journal.spool_path(&object_id);
    let input_path = path.to_path_buf();
    let spool_path = spool.clone();
    let key_copy = *key;
    let header_copy = header.clone();
    let cpu = cpu_permit().await?;
    let ciphertext_sha256 = spawn_blocking(move || {
        if spool_path.exists() {
            fs::remove_file(&spool_path).map_err(transient)?;
        }
        let mut input = crate::trust_boundary::open_regular_source(&input_path).map_err(corrupt)?;
        if input.metadata().map_err(corrupt)?.len() != plaintext_length {
            return Err(corrupt("plaintext length differs"));
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&spool_path)
            .map_err(transient)?;
        wire::seal_envelope(&mut input, &mut output, &key_copy, &header_copy).map_err(format_error)?;
        output.sync_all().map_err(transient)?;
        drop(output);
        let mut ciphertext =
            crate::trust_boundary::open_regular_source(&spool_path).map_err(corrupt)?;
        if ciphertext.metadata().map_err(corrupt)?.len() != ciphertext_length {
            return Err(corrupt("ciphertext length differs"));
        }
        Ok(hex::encode(
            hash_reader(&mut ciphertext, ciphertext_length).map_err(corrupt)?,
        ))
    })
    .await
    .map_err(transient)??;
    drop(cpu);
    let intent = ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: journal.job_id().to_owned(),
        object_id: object_id.clone(),
        role,
        byte_length: ciphertext_length,
        sha256: ciphertext_sha256.clone(),
    };
    journal.register(&intent)?;
    drop(reservation);
    Ok(Some(SealedObject::Pending {
        object_id,
        role,
        plaintext_length,
        plaintext_sha256,
        ciphertext_sha256,
    }))
}

/// Registers everything this wave still owes under one page, uploads it, and
/// returns the objects in the order they were sealed.
async fn upload_sealed_wave(
    sealed: Vec<SealedObject>,
    format_repository_id: &str,
    root_key: &[u8; 32],
    cache: &mut PackageCache,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Vec<RemoteObject>> {
    let members = sealed
        .iter()
        .filter_map(|object| match object {
            SealedObject::Ready(_) => None,
            SealedObject::Pending {
                object_id,
                plaintext_length,
                plaintext_sha256,
                ..
            } => Some(super::journal::WaveMember {
                object_id: object_id.clone(),
                plaintext_length: *plaintext_length,
                plaintext_sha256: plaintext_sha256.clone(),
            }),
        })
        .collect::<Vec<_>>();
    let mut receipts = transfer_job::upload_wave(
        journal,
        &members,
        format_repository_id,
        root_key,
        provider,
        repository,
        cancel,
    )
    .await?
    .into_iter();
    let mut uploaded = Vec::with_capacity(sealed.len());
    for object in sealed {
        let value = match object {
            SealedObject::Ready(value) => value,
            SealedObject::Pending {
                object_id,
                role,
                plaintext_length,
                plaintext_sha256,
                ciphertext_sha256,
            } => {
                let value = RemoteObject {
                    repository_id: format_repository_id.into(),
                    object_id,
                    role,
                    receipt: receipts
                        .next()
                        .ok_or_else(|| corrupt("registration wave receipt"))?,
                    ciphertext_sha256,
                    plaintext_length,
                    plaintext_sha256,
                };
                cache.put_object(repository, &value)?;
                value
            }
        };
        uploaded.push(value);
    }
    Ok(uploaded)
}

#[allow(clippy::too_many_arguments)]
async fn upload_plain_object(
    path: &Path,
    plaintext_length: u64,
    plaintext_hash: [u8; 32],
    object_id: String,
    role: ObjectRole,
    format_repository_id: &str,
    root_key: &[u8; 32],
    key: &[u8; 32],
    limits: PackageLimits,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<RemoteObject> {
    let sealed = seal_plain_object(
        path,
        plaintext_length,
        plaintext_hash,
        object_id,
        role,
        format_repository_id,
        root_key,
        key,
        limits,
        0,
        cache,
        evidence,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?
    .ok_or_else(|| ProviderError::new(ErrorKind::Transient))?;
    upload_sealed_wave(
        vec![sealed],
        format_repository_id,
        root_key,
        cache,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?
    .pop()
    .ok_or_else(|| corrupt("registration wave result"))
}

fn unique_packs(
    entries: &[wire::CatalogEntryFragment],
    available: &BTreeMap<String, wire::StoredObject>,
) -> Result<Vec<wire::StoredObject>> {
    let ids: BTreeSet<_> = entries
        .iter()
        .flat_map(|entry| entry.chunks.iter().map(|chunk| chunk.pack_id.as_str()))
        .collect();
    ids.into_iter()
        .map(|id| {
            available
                .get(id)
                .cloned()
                .ok_or_else(|| corrupt("catalog misses pack locator"))
        })
        .collect()
}

/// One published catalog by level: the key range each node covered, in the
/// order that level was published in.
#[derive(Debug, Default)]
pub(super) struct CatalogShape {
    levels: BTreeMap<u16, Vec<(String, String)>>,
}

impl CatalogShape {
    fn of(ranges: Vec<CatalogRange>) -> Self {
        let mut levels: BTreeMap<u16, Vec<(String, String)>> = BTreeMap::new();
        for range in ranges {
            levels
                .entry(range.level)
                .or_default()
                .push((range.first_key, range.last_key));
        }
        Self { levels }
    }
    fn level(&self, level: u16) -> &[(String, String)] {
        self.levels
            .get(&level)
            .map(|ranges| ranges.as_slice())
            .unwrap_or_default()
    }
}

/// Which node of the previous catalog a key belongs to. A key below every
/// range joins the first node, a key above every range joins the last, and a
/// key in a gap joins the node before it, so the same key lands in the same
/// node every time.
fn previous_node(ranges: &[(String, String)], key: &str) -> usize {
    match ranges.binary_search_by(|(first, _)| first.as_str().cmp(key)) {
        Ok(index) => index,
        Err(0) => 0,
        Err(index) => index - 1,
    }
}

/// The leaves of the previous catalog a key's fragments sit in: every leaf
/// whose range covers it, or the one `previous_node` places it in.
fn previous_leaves(ranges: &[(String, String)], key: &str) -> std::ops::RangeInclusive<usize> {
    let at = previous_node(ranges, key);
    let mut first = at;
    while first > 0 && ranges[first - 1].1.as_str() >= key {
        first -= 1;
    }
    let mut last = at;
    while last + 1 < ranges.len() && ranges[last + 1].0.as_str() <= key {
        last += 1;
    }
    first..=last
}

fn fits_under<T>(
    items: &[T],
    bucket: (usize, usize),
    limit: u64,
    size: &StoredSize<'_>,
    document: &impl Fn(&[T]) -> Result<wire::CatalogDocument>,
) -> Result<bool> {
    Ok(
        measure_catalog(document(&items[bucket.0..bucket.1])?, size)?
            .is_some_and(|encoded| (encoded.bytes.len() as u64) < limit),
    )
}

/// A node that no longer fits is split into even parts rather than filled to
/// the brim and left with a stub, so the next entry that lands in it has room
/// without splitting it again.
fn even_split<T>(
    items: &[T],
    size: &StoredSize<'_>,
    document: &impl Fn(&[T]) -> Result<wire::CatalogDocument>,
) -> Result<Vec<EncodedCatalog>> {
    let greedy = catalog_batches(items, size, document)?;
    if greedy.len() < 2 {
        return Ok(greedy);
    }
    for parts in greedy.len()..=greedy.len().saturating_add(2) {
        if parts > items.len() {
            break;
        }
        let mut even = Vec::with_capacity(parts);
        let mut start = 0;
        for part in 1..=parts {
            let end = if part == parts {
                items.len()
            } else {
                (items.len() * part / parts).max(start + 1).min(items.len())
            };
            match measure_catalog(document(&items[start..end])?, size)? {
                Some(encoded) => even.push(encoded),
                None => {
                    even.clear();
                    break;
                }
            }
            start = end;
        }
        if even.len() == parts && start == items.len() {
            return Ok(even);
        }
    }
    Ok(greedy)
}

/// Build one level the way the last publication built it. Every item goes to
/// the node whose range already covers its key, a node that no longer fits
/// splits inside its own range, and a node left with nothing disappears, so
/// only the nodes a change reached encode to new bytes. Without a previous
/// shape this is the plain greedy fill.
fn stable_batches<T>(
    items: &[T],
    ranges: &[(String, String)],
    size: &StoredSize<'_>,
    key_of: impl Fn(&T) -> &str,
    document: impl Fn(&[T]) -> Result<wire::CatalogDocument>,
) -> Result<Vec<EncodedCatalog>> {
    if ranges.is_empty() || items.is_empty() {
        return catalog_batches(items, size, document);
    }
    // The complete run of items sharing one key is the unit, so a key that
    // spans nodes still has one place it belongs.
    let mut buckets: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    let mut current: Option<usize> = None;
    let mut start = 0;
    while index < items.len() {
        let key = key_of(&items[index]);
        let mut end = index + 1;
        while end < items.len() && key_of(&items[end]) == key {
            end += 1;
        }
        let node = previous_node(ranges, key);
        match current {
            Some(previous) if previous == node => {}
            Some(_) => {
                buckets.push((start, index));
                start = index;
                current = Some(node);
            }
            None => {
                start = index;
                current = Some(node);
            }
        }
        index = end;
    }
    buckets.push((start, items.len()));
    // Two nodes that have both shrunk are merged when the result still fits,
    // so removals do not leave a catalog of stubs behind. Only the pair
    // changes; nothing after them moves.
    let half = size.limits.target_plaintext_bytes / 2;
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for bucket in buckets {
        if let Some(previous) = merged.last().copied() {
            if fits_under(items, previous, half, size, &document)?
                && fits_under(items, bucket, half, size, &document)?
                && measure_catalog(document(&items[previous.0..bucket.1])?, size)?.is_some()
            {
                merged.pop();
                merged.push((previous.0, bucket.1));
                continue;
            }
        }
        merged.push(bucket);
    }
    let mut result = Vec::new();
    for (start, end) in merged {
        result.extend(even_split(&items[start..end], size, &document)?);
    }
    Ok(result)
}

const TARGET_CATALOG_FRAGMENTS: usize = 512;

struct EncodedCatalog {
    document: wire::CatalogDocument,
    bytes: Vec<u8>,
}

/// Invalid metadata is an error, not an oversized leaf. The accepted bytes are
/// the exact bytes uploaded, including final fragment indices and counts.
fn measure_catalog(document: wire::CatalogDocument, size: &StoredSize<'_>) -> Result<Option<EncodedCatalog>> {
    let bytes = match document.encode(wire::MAX_METADATA_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.0 == "catalog-limit-exceeded" => return Ok(None),
        Err(error) => return Err(corrupt(error)),
    };
    // Keyed catalog IDs always contain this fixed prefix and 64 unescaped hex digits.
    if !size.fits(&format!("catalog-{}", "0".repeat(64)), wire::ObjectRole::Catalog, bytes.len() as u64)? {
        return Ok(None);
    }
    Ok(Some(EncodedCatalog { document, bytes }))
}

/// A single measured-serialization path partitions both leaves and branches.
/// The winning serialization is retained rather than probed and rebuilt again.
fn catalog_batches<T>(
    items: &[T],
    size: &StoredSize<'_>,
    document: impl Fn(&[T]) -> Result<wire::CatalogDocument>,
) -> Result<Vec<EncodedCatalog>> {
    let too_large = || ProviderError::new(ErrorKind::FileTooLarge);
    if items.is_empty() {
        return Ok(vec![measure_catalog(document(items)?, size)?.ok_or_else(too_large)?]);
    }
    let mut result = Vec::new();
    let mut start = 0;
    while start < items.len() {
        let end = items.len().min(start.saturating_add(TARGET_CATALOG_FRAGMENTS));
        if let Some(encoded) = measure_catalog(document(&items[start..end])?, size)? {
            result.push(encoded);
            start = end;
            continue;
        }
        let mut fits_end = start;
        let mut too_large_end = end;
        let mut best = None;
        while fits_end + 1 < too_large_end {
            let middle = fits_end + (too_large_end - fits_end) / 2;
            match measure_catalog(document(&items[start..middle])?, size)? {
                Some(encoded) => { fits_end = middle; best = Some(encoded); }
                None => too_large_end = middle,
            }
        }
        result.push(best.ok_or_else(too_large)?);
        start = fits_end;
    }
    Ok(result)
}

/// Every pack `entries` name, by object ID, the way a leaf names it.
fn pack_locators(
    entries: &[EntryPlan],
    repository: &RepositoryHandle,
) -> Result<BTreeMap<String, wire::StoredObject>> {
    let mut available = BTreeMap::new();
    for entry in entries {
        for pack in &entry.packs {
            let stored = pack.stored(repository)?;
            if let Some(old) = available.insert(pack.object_id.clone(), stored.clone()) {
                if old != stored {
                    return Err(corrupt("conflicting pack locator"));
                }
            }
        }
    }
    Ok(available)
}

fn catalog_fragments(entries: &[EntryPlan]) -> Result<Vec<wire::CatalogEntryFragment>> {
    let mut fragments = Vec::new();
    for entry in entries {
        // Fixed chunk fragments make the final count known before sizing. A
        // growing decimal count cannot make a previously accepted leaf overflow.
        let count = u32::try_from(entry.chunks.len()).map_err(corrupt)?;
        if count == 0 { return Err(corrupt("entry has no chunks")); }
        for (index, chunk) in entry.chunks.iter().enumerate() {
            fragments.push(wire::CatalogEntryFragment {
                kind: entry.kind, key: entry.key.clone(),
                content_sha256: decode_hash(&entry.content_sha256)?, byte_length: entry.byte_length,
                fragment_index: index as u32, fragment_count: count, chunks: vec![chunk.clone()],
            });
        }
    }
    Ok(fragments)
}

/// The leaves `fragments` are written as, laid out against the previous
/// catalog's leaf ranges. The writer and the coalescing plan both lay leaves
/// out here, so the plan counts the leaves the writer produces.
fn catalog_leaves(
    kind: wire::CatalogKind,
    fragments: &[wire::CatalogEntryFragment],
    packs: &BTreeMap<String, wire::StoredObject>,
    ranges: &[(String, String)],
    size: &StoredSize<'_>,
) -> Result<Vec<EncodedCatalog>> {
    stable_batches(
        fragments,
        ranges,
        size,
        |fragment| fragment.key.as_str(),
        |leaf| {
            wire::CatalogDocument::leaf(kind, leaf.to_vec(), unique_packs(leaf, packs)?)
                .map_err(corrupt)
        },
    )
}

async fn build_catalog(
    kind: wire::CatalogKind,
    entries: Vec<EntryPlan>,
    fingerprint: &str,
    format_repository_id: &str,
    build_root: &Path,
    root_key: &[u8; 32],
    limits: PackageLimits,
    parent: Option<&BTreeSet<String>>,
    shape: Option<&CatalogShape>,
    placements_moved: bool,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
    referenced: &mut Vec<RemoteObject>,
) -> Result<RemoteObject> {
    // Everything this call adds to the publication's reference list is a node
    // of the catalog it is about to name.
    let mark = referenced.len();
    // The cached root is keyed by what the catalog holds, not where it holds
    // it, so it cannot answer for entries this publication placed elsewhere.
    let cached = if placements_moved {
        None
    } else {
        cache.catalog(format_repository_id, repository, kind, fingerprint)?
    };
    if let Some(root) = cached {
        if let Some(objects) =
            admitted_catalog(&root, parent, format_repository_id, cache, evidence, repository)?
        {
            referenced.extend(objects);
            return Ok(root);
        }
        if let Some(objects) = revalidate_cached_catalog(
            &root, cache, evidence, journal, root_key, provider, repository, cancel,
        ).await? {
            let current = objects.first().cloned().ok_or_else(|| corrupt("empty catalog closure"))?;
            referenced.extend(objects);
            // What the cached root covered is what the repaired one covers.
            let carried = cache.shape_of(
                format_repository_id,
                repository,
                &super::reachability::object_identity(&root, repository)?,
            )?;
            record_published_graph(
                &entries, &referenced[mark..], &current, kind, format_repository_id, &carried,
                cache, repository,
            )?;
            return Ok(current);
        }
    }
    let size = StoredSize { limits, repository_id: format_repository_id };
    let metadata_key = derive_key(root_key, format_repository_id, "metadata").map_err(corrupt)?;
    let available_packs = pack_locators(&entries, repository)?;
    let fragments = catalog_fragments(&entries)?;
    let leaves = catalog_leaves(
        kind,
        &fragments,
        &available_packs,
        shape.map(|shape| shape.level(0)).unwrap_or_default(),
        &size,
    )?;
    let mut built: Vec<CatalogRange> = Vec::new();
    let mut nodes = Vec::new();
    let mut level_documents = Vec::new();
    let mut level_sealed = Vec::new();
    let mut level_uploaded = Vec::new();
    let mut wave_bytes = 0u64;
    for EncodedCatalog { document, bytes } in leaves {
        let sealed = match admitted_node(
            &bytes, parent, format_repository_id, cache, evidence, repository,
        )? {
            Some(existing) => SealedObject::Ready(existing),
            None => {
                seal_catalog_node(
                    &bytes,
                    &mut level_sealed,
                    &mut wave_bytes,
                    &mut level_uploaded,
                    format_repository_id,
                    root_key,
                    &metadata_key,
                    limits,
                    build_root,
                    cache,
                    evidence,
                    journal,
                    provider,
                    repository,
                    cancel,
                )
                .await?
            }
        };
        if matches!(sealed, SealedObject::Pending { .. }) {
            wave_bytes += bytes.len() as u64;
        }
        level_documents.push(document);
        level_sealed.push(sealed);
        if metadata_wave_due(&level_sealed, wave_bytes) {
            level_uploaded.extend(
                upload_sealed_wave(
                    std::mem::take(&mut level_sealed),
                    format_repository_id,
                    root_key,
                    cache,
                    journal,
                    provider,
                    repository,
                    cancel,
                )
                .await?,
            );
            wave_bytes = 0;
        }
    }
    // A level registers together where it fits one wave: its children are
    // already confirmed, so every node of it is ready at the same moment.
    level_uploaded.extend(
        upload_sealed_wave(
            level_sealed,
            format_repository_id,
            root_key,
            cache,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?,
    );
    for (document, remote) in level_documents.into_iter().zip(level_uploaded) {
        referenced.push(remote.clone());
        built.push(CatalogRange {
            level: 0,
            first_key: document.first_key.clone(),
            last_key: document.last_key.clone(),
        });
        nodes.push(wire::CatalogChild {
            first_key: document.first_key,
            last_key: document.last_key,
            object: remote.stored(repository)?,
        });
    }
    let mut level = 1u16;
    while nodes.len() > 1 {
        let previous_count = nodes.len();
        let mut next = Vec::new();
        let batches = stable_batches(
            &nodes,
            shape.map(|shape| shape.level(level)).unwrap_or_default(),
            &size,
            |child| child.first_key.as_str(),
            |children| wire::CatalogDocument::branch(kind, level, children.to_vec()).map_err(corrupt),
        )?;
        let mut level_documents = Vec::new();
        let mut level_sealed = Vec::new();
        let mut level_uploaded = Vec::new();
        let mut wave_bytes = 0u64;
        for EncodedCatalog { document, bytes } in batches {
            let sealed = match admitted_node(
                &bytes, parent, format_repository_id, cache, evidence, repository,
            )? {
                Some(existing) => SealedObject::Ready(existing),
                None => {
                    seal_catalog_node(
                        &bytes,
                        &mut level_sealed,
                        &mut wave_bytes,
                        &mut level_uploaded,
                        format_repository_id,
                        root_key,
                        &metadata_key,
                        limits,
                        build_root,
                        cache,
                        evidence,
                        journal,
                        provider,
                        repository,
                        cancel,
                    )
                    .await?
                }
            };
            if matches!(sealed, SealedObject::Pending { .. }) {
                wave_bytes += bytes.len() as u64;
            }
            level_documents.push(document);
            level_sealed.push(sealed);
            if metadata_wave_due(&level_sealed, wave_bytes) {
                level_uploaded.extend(
                    upload_sealed_wave(
                        std::mem::take(&mut level_sealed),
                        format_repository_id,
                        root_key,
                        cache,
                        journal,
                        provider,
                        repository,
                        cancel,
                    )
                    .await?,
                );
                wave_bytes = 0;
            }
        }
        level_uploaded.extend(
            upload_sealed_wave(
                level_sealed,
                format_repository_id,
                root_key,
                cache,
                journal,
                provider,
                repository,
                cancel,
            )
            .await?,
        );
        for (document, remote) in level_documents.into_iter().zip(level_uploaded) {
            referenced.push(remote.clone());
            built.push(CatalogRange {
                level,
                first_key: document.first_key.clone(),
                last_key: document.last_key.clone(),
            });
            next.push(wire::CatalogChild {
                first_key: document.first_key,
                last_key: document.last_key,
                object: remote.stored(repository)?,
            });
        }
        if next.len() >= previous_count {
            return Err(ProviderError::new(ErrorKind::FileTooLarge));
        }
        nodes = next;
        level = level
            .checked_add(1)
            .ok_or_else(|| corrupt("catalog depth"))?;
    }
    let root = RemoteObject::from_stored(
        &nodes
            .into_iter()
            .next()
            .ok_or_else(|| corrupt("catalog root"))?
            .object,
        repository,
    )?;
    cache.put_catalog(repository, kind, fingerprint, &root)?;
    record_published_graph(
        &entries, &referenced[mark..], &root, kind, format_repository_id, &built, cache,
        repository,
    )?;
    Ok(root)
}

/// How many newly sealed catalog nodes, and how many of their bytes, one wave
/// holds before it is sent. A wide level is registered in several waves
/// rather than sealed whole, so its ciphertext never outgrows the spool.
const METADATA_WAVE_OBJECTS: usize = 64;
const METADATA_WAVE_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(test)]
thread_local! {
    static TEST_METADATA_WAVE_OBJECTS: std::cell::Cell<usize> = const { std::cell::Cell::new(METADATA_WAVE_OBJECTS) };
}
fn metadata_wave_objects() -> usize {
    #[cfg(test)]
    return TEST_METADATA_WAVE_OBJECTS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    METADATA_WAVE_OBJECTS
}

fn metadata_wave_due(sealed: &[SealedObject], bytes: u64) -> bool {
    bytes >= METADATA_WAVE_BYTES || waiting(sealed) >= metadata_wave_objects()
}

/// A catalog whose root is in the selected parent graph is protected with it,
/// down to every pack beneath it, so what it names is read from the cache
/// rather than walked. A root the cache never recorded, or one naming
/// something this connection found unusable, is not admitted.
fn admitted_catalog(
    root: &RemoteObject,
    members: Option<&BTreeSet<String>>,
    format_repository_id: &str,
    cache: &PackageCache,
    evidence: &ObjectEvidence,
    repository: &RepositoryHandle,
) -> Result<Option<Vec<RemoteObject>>> {
    let Some(members) = members else {
        return Ok(None);
    };
    let identity = ObjectEvidence::identity(root, repository)?;
    if !members.contains(&identity) {
        return Ok(None);
    }
    let objects = cache.graph_of(format_repository_id, repository, &identity)?;
    if objects.is_empty() {
        return Ok(None);
    }
    for object in &objects {
        if evidence.known(&ObjectEvidence::identity(object, repository)?)? {
            return Ok(None);
        }
    }
    Ok(Some(objects))
}

/// A catalog node whose bytes this connection already published under the
/// selected parent, referenced again rather than uploaded under an object id
/// of this job's own. Admitted the way a pack is: in the parent graph, and
/// not something this connection found unusable.
fn admitted_node(
    bytes: &[u8],
    members: Option<&BTreeSet<String>>,
    format_repository_id: &str,
    cache: &PackageCache,
    evidence: &ObjectEvidence,
    repository: &RepositoryHandle,
) -> Result<Option<RemoteObject>> {
    let Some(members) = members else {
        return Ok(None);
    };
    let plaintext = hex::encode(hash(bytes));
    for object in cache.objects_by_plaintext(format_repository_id, repository, &plaintext)? {
        if object.role != ObjectRole::Catalog
            || object.plaintext_length != bytes.len() as u64
        {
            continue;
        }
        let identity = ObjectEvidence::identity(&object, repository)?;
        if !members.contains(&identity) || evidence.known(&identity)? {
            continue;
        }
        return Ok(Some(object));
    }
    Ok(None)
}

/// What one catalog root names: the packs its entries point at and the catalog
/// nodes beneath it, each by the identity that names one exact remote object.
/// A locator repaired since an entry was written produces a different identity
/// and simply does not match, which is the conservative direction.
#[allow(clippy::too_many_arguments)]
fn record_published_graph(
    entries: &[EntryPlan],
    added: &[RemoteObject],
    root: &RemoteObject,
    kind: wire::CatalogKind,
    format_repository_id: &str,
    shape: &[CatalogRange],
    cache: &PackageCache,
    repository: &RepositoryHandle,
) -> Result<()> {
    let mut members = BTreeMap::new();
    for object in entries.iter().flat_map(|entry| entry.packs.iter()).chain(added) {
        members.insert(
            super::reachability::object_identity(object, repository)?,
            object.stored(repository)?,
        );
    }
    let members: Vec<(String, wire::StoredObject)> = members.into_iter().collect();
    cache.record_graph(
        format_repository_id,
        repository,
        kind,
        &super::reachability::object_identity(root, repository)?,
        &members,
        shape,
    )
}

async fn seal_metadata_bytes(
    bytes: &[u8],
    _prefix: &str,
    role: ObjectRole,
    format_repository_id: &str,
    root_key: &[u8; 32],
    key: &[u8; 32],
    limits: PackageLimits,
    build_root: &Path,
    wave: usize,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<Option<SealedObject>> {
    let mut file = tempfile::NamedTempFile::new_in(build_root).map_err(transient)?;
    file.write_all(bytes).map_err(transient)?;
    file.as_file_mut().sync_all().map_err(transient)?;
    let digest = hash(bytes);
    let object_id =
        wire::keyed_object_id(key, journal.job_id(), wire_role(role)?, &digest).map_err(corrupt)?;
    seal_plain_object(
        file.path(),
        bytes.len() as u64,
        digest,
        object_id,
        role,
        format_repository_id,
        root_key,
        key,
        limits,
        wave,
        cache,
        evidence,
        journal,
        provider,
        repository,
        cancel,
    )
    .await
}

/// Seals one catalog node to join `wave`. When the spool has no room for it,
/// the nodes already waiting are sent first, which releases theirs, and the
/// node is sealed once more; a spool still full then is held by other jobs.
#[allow(clippy::too_many_arguments)]
async fn seal_catalog_node(
    bytes: &[u8],
    wave: &mut Vec<SealedObject>,
    wave_bytes: &mut u64,
    uploaded: &mut Vec<RemoteObject>,
    format_repository_id: &str,
    root_key: &[u8; 32],
    key: &[u8; 32],
    limits: PackageLimits,
    build_root: &Path,
    cache: &mut PackageCache,
    evidence: &mut ObjectEvidence,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<SealedObject> {
    loop {
        let waiting_now = waiting(wave);
        if let Some(sealed) = seal_metadata_bytes(
            bytes,
            "catalog",
            ObjectRole::Catalog,
            format_repository_id,
            root_key,
            key,
            limits,
            build_root,
            waiting_now,
            cache,
            evidence,
            journal,
            provider,
            repository,
            cancel,
        )
        .await?
        {
            return Ok(sealed);
        }
        if waiting_now == 0 {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        uploaded.extend(
            upload_sealed_wave(
                std::mem::take(wave),
                format_repository_id,
                root_key,
                cache,
                journal,
                provider,
                repository,
                cancel,
            )
            .await?,
        );
        *wave_bytes = 0;
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn package_and_upload(
    capture: CapturedSnapshot,
    sections: Vec<CapturedSection>,
    repository_root: &Path,
    cache_root: &Path,
    metadata: SnapshotMetadata,
    root_key: &[u8; 32],
    limits: PackageLimits,
    parent: Option<&ParentGraph>,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    prepared: &Arc<PhaseProgress>,
    cancel: &Cancellation,
) -> Result<CompletedSnapshot> {
    package_and_upload_protected(capture,sections,repository_root,cache_root,metadata,root_key,limits,parent,journal,provider,repository,prepared,cancel,None).await
}

pub(crate) struct AssetCatalogSource {
    pub content_hash:String,
    pub byte_length:u64,
    pub source:ObjectSource,
}
pub(crate) struct CompletedAssetCatalog {
    pub catalog:wire::StoredObject,
    pub referenced:Vec<RemoteObject>,
}

pub(super) async fn carry_asset_catalog(
    mut entries:Vec<EntryPlan>,repository_root:&Path,cache_root:&Path,root_key:&[u8;32],
    limits:PackageLimits,journal:&mut TransferJournal,provider:&dyn Provider,
    repository:&RepositoryHandle,cancel:&Cancellation,
) -> Result<CompletedAssetCatalog> {
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    entries.sort_by(|a,b|a.key.cmp(&b.key));
    for entry in &entries {entry.validate(&format_repository_id,repository)?;}
    if entries.windows(2).any(|pair|pair[0].key==pair[1].key) {return Err(corrupt("duplicate carried asset"));}
    let fingerprint=catalog_fingerprint_of(wire::CatalogKind::Assets,
        entries.iter().map(|entry|(entry.key.as_str(),entry.byte_length,entry.content_sha256.as_str())));
    let build_root=cache_root.join("build");fs::create_dir_all(&build_root).map_err(transient)?;
    let mut cache=PackageCache::open(cache_root)?;
    let mut evidence=ObjectEvidence::open(repository_root,journal.connection_id())?;
    let mut packs=BTreeMap::new();
    for entry in &entries {for pack in &entry.packs {packs.insert(pack.object_id.clone(),pack.clone());}}
    let mut referenced=packs.into_values().collect::<Vec<_>>();
    let root=build_catalog(wire::CatalogKind::Assets,entries,&fingerprint,&format_repository_id,
        &build_root,root_key,limits,None,None,true,&mut cache,&mut evidence,journal,
        provider,repository,cancel,&mut referenced).await?;
    Ok(CompletedAssetCatalog{catalog:root.stored(repository)?,referenced})
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn package_and_upload_asset_catalog(
    sources:Vec<AssetCatalogSource>,repository_root:&Path,cache_root:&Path,
    root_key:&[u8;32],limits:PackageLimits,journal:&mut TransferJournal,
    provider:&dyn Provider,repository:&RepositoryHandle,prepared:&Arc<PhaseProgress>,
    cancel:&Cancellation,
    protection:Option<(&super::leases::LeaseOwner,&super::leases::LeaseContext<'_>)>,
) -> Result<CompletedAssetCatalog> {
    package_and_upload_content_catalog(wire::CatalogKind::Assets,sources,repository_root,cache_root,root_key,limits,journal,provider,repository,prepared,cancel,protection).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn package_and_upload_control_catalog(
    sources:Vec<AssetCatalogSource>,repository_root:&Path,cache_root:&Path,
    root_key:&[u8;32],limits:PackageLimits,journal:&mut TransferJournal,
    provider:&dyn Provider,repository:&RepositoryHandle,prepared:&Arc<PhaseProgress>,
    cancel:&Cancellation,
    protection:Option<(&super::leases::LeaseOwner,&super::leases::LeaseContext<'_>)>,
) -> Result<CompletedAssetCatalog> {
    package_and_upload_content_catalog(wire::CatalogKind::Records,sources,repository_root,cache_root,root_key,limits,journal,provider,repository,prepared,cancel,protection).await
}

#[allow(clippy::too_many_arguments)]
async fn package_and_upload_content_catalog(
    kind:wire::CatalogKind,sources:Vec<AssetCatalogSource>,repository_root:&Path,cache_root:&Path,
    root_key:&[u8;32],limits:PackageLimits,journal:&mut TransferJournal,
    provider:&dyn Provider,repository:&RepositoryHandle,prepared:&Arc<PhaseProgress>,
    cancel:&Cancellation,
    protection:Option<(&super::leases::LeaseOwner,&super::leases::LeaseContext<'_>)>,
) -> Result<CompletedAssetCatalog> {
    cancel.check()?;
    let mut sources=sources.into_iter().map(|source| {
        risunest_sync_wire::validate_hash(&source.content_hash).map_err(corrupt)?;
        Ok(SourceEntry{kind:wire::CatalogEntryKind::Object,key:format!("object/{}",source.content_hash),
            content_sha256:source.content_hash,byte_length:source.byte_length,
            source:source.source,file_offset:None,compression:CompressionPolicy::AlreadyCompressed})
    }).collect::<Result<Vec<_>>>()?;
    sources.sort_by(|a,b|a.key.cmp(&b.key));
    if sources.windows(2).any(|pair|pair[0].key==pair[1].key) {
        return Err(corrupt("duplicate asset catalog source"));
    }
    let fingerprint=catalog_fingerprint(kind,&sources);
    let build_root=cache_root.join("build");
    fs::create_dir_all(&build_root).map_err(transient)?;
    let mut cache=PackageCache::open(cache_root)?;
    let mut evidence=ObjectEvidence::open(repository_root,journal.connection_id())?;
    let mut referenced=Vec::new();
    let format_repository_id=super::lww_segment::format_repository_id(&repository.repository_id);
    let cached=match cache.catalog(&format_repository_id,repository,kind,&fingerprint)? {
        Some(root)=>revalidate_cached_catalog(&root,&mut cache,&mut evidence,journal,root_key,provider,repository,cancel).await?,
        None=>None,
    };
    let root=if let Some(objects)=cached {
        let root=objects.first().cloned().ok_or_else(||corrupt("empty asset catalog closure"))?;
        referenced=objects;
        root
    } else {
        let mut hydration=SourceHydration::default();
        let mut maintenance=MaintenanceReport::default();
        let (items,bytes)=source_totals(&sources);
        prepared.plan(items,bytes);
        let (entries,uploaded)=build_entries(kind,sources,
            &repository_root.join("external-storage"),&format_repository_id,&build_root,
            root_key,limits,None,None,false,&mut cache,&mut evidence,journal,provider,repository,
            prepared,&mut maintenance,&mut hydration,cancel).await?;
        referenced.extend(uploaded);
        build_catalog(kind,entries,&fingerprint,&format_repository_id,
            &build_root,root_key,limits,None,None,false,&mut cache,&mut evidence,journal,
            provider,repository,cancel,&mut referenced).await?
    };
    if let Some((owner,context))=protection {
        owner.renew_if_due(context,cancel).await?;
        if let Some(reason)=owner.recheck(context,cancel).await? {
            return Err(super::leases::yield_error(reason));
        }
    }
    Ok(CompletedAssetCatalog{catalog:root.stored(repository)?,referenced})
}

pub(crate) async fn package_and_upload_protected(
    capture: CapturedSnapshot,
    sections: Vec<CapturedSection>,
    repository_root: &Path,
    cache_root: &Path,
    metadata: SnapshotMetadata,
    root_key: &[u8; 32],
    limits: PackageLimits,
    parent: Option<&ParentGraph>,
    journal: &mut TransferJournal,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    prepared: &Arc<PhaseProgress>,
    cancel: &Cancellation,
    protection:Option<(&super::leases::LeaseOwner,&super::leases::LeaseContext<'_>)>,
) -> Result<CompletedSnapshot> {
    let _spool_execution = journal.begin_spool_execution();
    cancel.check()?;
    journal.verification = Default::default();
    if metadata.repository_id.is_empty()
        || metadata.snapshot_id.is_empty()
        || metadata.logical_revision > i64::MAX as u64
        || capture.identity.revision < 0
        || capture.identity.revision as u64 != metadata.logical_revision
        || capture.identity.library_epoch != metadata.library_id
        || capture.identity.store_id != metadata.author_device_id
    {
        return Err(corrupt("snapshot metadata differs from repository"));
    }
    let build_root = cache_root.join("build");
    let mut cache = PackageCache::open(cache_root)?;
    // Read once for the whole publication. Nothing known about the selected
    // parent means nothing is admitted on its account, so a cache that never
    // saw this parent rebuilds what it names before the first reuse decision.
    let mut shapes: BTreeMap<String, CatalogShape> = BTreeMap::new();
    let members = match parent {
        Some(graph) => {
            super::cache_hydration::hydrate_parent(
                graph,
                &metadata.repository_id,
                cache_root,
                root_key,
                &mut cache,
                provider,
                repository,
                cancel,
            )
            .await?;
            for (label, root) in graph.roots() {
                let identity = super::reachability::object_identity(root, repository)?;
                shapes.insert(
                    label.key(),
                    CatalogShape::of(cache.shape_of(
                        &metadata.repository_id,
                        repository,
                        &identity,
                    )?),
                );
            }
            Some(cache.members_of(&metadata.repository_id, repository, graph.identities())?)
        }
        None => None,
    };
    let mut evidence = ObjectEvidence::open(repository_root, journal.connection_id())?;
    let external_root = repository_root.join("external-storage");
    let library_domain = library_fingerprint_domain();
    let cpu = cpu_permit().await?;
    let (record_sources, asset_sources, observed_fingerprint) =
        spawn_blocking(move || {
            let (records, assets) = capture_sources(&capture)?;
            let fingerprint = capture
                .catalog
                .content_fingerprint(&library_domain)
                .map_err(corrupt)?;
            Ok((records, assets, fingerprint))
        })
        .await
        .map_err(transient)??;
    drop(cpu);
    if observed_fingerprint != metadata.content_fingerprint {
        return Err(corrupt("snapshot library fingerprint differs"));
    }
    // A published state keeps the sections the observed state already had and
    // replaces only the ones this device captured. A bundle starts empty and
    // declares exactly what it covers.
    let mut published_sections = match &metadata.purpose {
        SnapshotPurpose::SyncState {
            parent_sections, ..
        } => parent_sections.clone(),
        SnapshotPurpose::BackupBundle { .. } | SnapshotPurpose::LwwCheckpoint { .. } => BTreeMap::new(),
    };
    // An unchanged section keeps the reference the observed state carried, so
    // its commit number still names the publication that changed it.
    let sections: Vec<CapturedSection> = sections
        .into_iter()
        .filter(|captured| {
            !published_sections.get(captured.kind.id()).is_some_and(|carried| {
                carried.content_fingerprint == captured.content_fingerprint
                    && carried.gc_floor == captured.gc_floor
                    && carried.max_write_clock == captured.max_write_clock
            })
        })
        .collect();
    // Everything this publication prepares is planned before the first domain
    // starts, out of what the capture already says, so the total the job
    // reports first is the one it finishes at.
    let original_totals = match &metadata.purpose {
        SnapshotPurpose::BackupBundle { original_units, .. } => {
            let units = original_units.clone();
            let totals_cancel = cancel.clone();
            let cpu = cpu_permit().await?;
            let totals = spawn_blocking(move || original_unit_totals(&units, &totals_cancel))
                .await
                .map_err(transient)??;
            drop(cpu);
            totals
        }
        SnapshotPurpose::SyncState { .. } | SnapshotPurpose::LwwCheckpoint { .. } => (0, 0),
    };
    let (asset_items, asset_bytes) = source_totals(&asset_sources);
    let mut planned = [
        source_totals(&record_sources),
        (asset_items, asset_bytes),
        original_totals,
    ]
    .into_iter()
    .fold((0u64, 0u64), |(items, bytes), (more_items, more_bytes)| {
        (items.saturating_add(more_items), bytes.saturating_add(more_bytes))
    });
    for source in sections.iter().flat_map(|captured| &captured.sources) {
        planned = (planned.0.saturating_add(1), planned.1.saturating_add(source.byte_length));
    }
    prepared.plan(planned.0, planned.1);
    let mut maintenance = MaintenanceReport::default();
    let mut hydration = SourceHydration::default();
    let record_fingerprint = catalog_fingerprint(wire::CatalogKind::Records, &record_sources);
    let asset_fingerprint = catalog_fingerprint(wire::CatalogKind::Assets, &asset_sources);
    // Without a cycle, a record catalog whose root is already published is
    // referenced again rather than written.
    let record_root_cached = cache
        .catalog(&metadata.repository_id, repository, wire::CatalogKind::Records, &record_fingerprint)?
        .is_some();
    let (record_entries, mut referenced) = build_entries(
        wire::CatalogKind::Records,
        record_sources,
        &external_root,
        &metadata.repository_id,
        &build_root,
        root_key,
        limits,
        members.as_ref(),
        shapes.get("records"),
        record_root_cached,
        &mut cache,
        &mut evidence,
        journal,
        provider,
        repository,
        prepared,
        &mut maintenance,
        &mut hydration,
        cancel,
    )
    .await?;
    let record_catalog = build_catalog(
        wire::CatalogKind::Records,
        record_entries,
        &record_fingerprint,
        &metadata.repository_id,
        &build_root,
        root_key,
        limits,
        members.as_ref(),
        shapes.get("records"),
        maintenance.packs > 0,
        &mut cache,
        &mut evidence,
        journal,
        provider,
        repository,
        cancel,
        &mut referenced,
    )
    .await?;
    let cached_assets = match cache.catalog(
        &metadata.repository_id,
        repository,
        wire::CatalogKind::Assets,
        &asset_fingerprint,
    )? {
        Some(root) => match admitted_catalog(
            &root, members.as_ref(), &metadata.repository_id, &cache, &evidence, repository,
        )? {
            Some(objects) => Some((root, objects)),
            // A revalidated root may carry a locator repaired since it was
            // cached, so the closure names the current one, not this.
            None => match revalidate_cached_catalog(
                &root, &mut cache, &mut evidence, journal, root_key, provider, repository, cancel,
            ).await? {
                Some(objects) => Some((
                    objects.first().cloned().ok_or_else(|| corrupt("empty asset catalog closure"))?,
                    objects,
                )),
                None => None,
            },
        },
        None => None,
    };
    let asset_catalog = if let Some((root, objects)) = cached_assets {
        if objects.is_empty() {
            return Err(corrupt("empty asset catalog closure"));
        }
        referenced.extend(objects);
        prepared.completed_many(asset_items, asset_bytes);
        root
    } else {
        let (asset_entries, uploaded) = build_entries(
            wire::CatalogKind::Assets,
            asset_sources,
            &external_root,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            members.as_ref(),
            None,
            false,
            &mut cache,
            &mut evidence,
            journal,
            provider,
            repository,
            prepared,
            &mut MaintenanceReport::default(),
            &mut hydration,
            cancel,
        )
        .await?;
        referenced.extend(uploaded);
        build_catalog(
            wire::CatalogKind::Assets,
            asset_entries,
            &asset_fingerprint,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            members.as_ref(),
            shapes.get("assets"),
            false,
            &mut cache,
            &mut evidence,
            journal,
            provider,
            repository,
            cancel,
            &mut referenced,
        )
        .await?
    };
    let library = wire::LibrarySnapshotRef {
        record_catalog: record_catalog.stored(repository)?,
        asset_catalog: asset_catalog.stored(repository)?,
        content_fingerprint: metadata.content_fingerprint,
    };
    let mut built_sections = BTreeSet::new();
    for captured in sections {
        let sources: Vec<SourceEntry> = captured
            .sources
            .iter()
            .map(|source| SourceEntry {
                kind: source.kind,
                key: source.key.clone(),
                content_sha256: source.content_sha256.clone(),
                byte_length: source.byte_length,
                source: ObjectSource::File(source.path.clone()),
                file_offset: source.offset,
                compression: match (source.kind, captured.kind) {
                    (wire::CatalogEntryKind::SectionObject, SectionKind::Hypa) => CompressionPolicy::AlreadyCompressed,
                    _ => CompressionPolicy::Text,
                },
            })
            .collect();
        let id = captured.kind.id();
        let section_fingerprint = section_catalog_fingerprint(id, &sources);
        let (section_entries, uploaded) = build_entries(
            wire::CatalogKind::Section,
            sources,
            &external_root,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            members.as_ref(),
            None,
            false,
            &mut cache,
            &mut evidence,
            journal,
            provider,
            repository,
            prepared,
            &mut MaintenanceReport::default(),
            &mut hydration,
            cancel,
        )
        .await?;
        referenced.extend(uploaded);
        let entries_root = build_catalog(
            wire::CatalogKind::Section,
            section_entries,
            &section_fingerprint,
            &metadata.repository_id,
            &build_root,
            root_key,
            limits,
            members.as_ref(),
            shapes.get(&format!("section/{id}")),
            false,
            &mut cache,
            &mut evidence,
            journal,
            provider,
            repository,
            cancel,
            &mut referenced,
        )
        .await?;
        built_sections.insert(id.to_owned());
        published_sections.insert(
            id.to_owned(),
            wire::SectionSnapshotRef {
                kind: captured.kind,
                codec: SECTION_CODEC.into(),
                generation: captured.generation,
                gc_floor: captured.gc_floor,
                max_write_clock: captured.max_write_clock,
                entries_root: entries_root.stored(repository)?,
                content_fingerprint: captured.content_fingerprint,
            },
        );
    }
    // Carried, unselected sections are publication references too. Never
    // silently drop them, and never publish a cached reference with a hole.
    for (id, section) in &published_sections {
        if built_sections.contains(id) { continue; }
        let root = RemoteObject::from_stored(&section.entries_root, repository)?;
        let objects = match admitted_catalog(
            &root, members.as_ref(), &metadata.repository_id, &cache, &evidence, repository,
        )? {
            Some(objects) => objects,
            None => revalidate_cached_catalog(
                &root, &mut cache, &mut evidence, journal, root_key, provider, repository, cancel,
            ).await?.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?,
        };
        referenced.extend(objects);
    }
    let original_units_root = if let SnapshotPurpose::BackupBundle { original_units, .. } = &metadata.purpose {
        Some(build_original_unit_catalog(original_units,&external_root,&metadata.repository_id,&build_root,root_key,limits,
            &mut cache,&mut evidence,journal,provider,repository,prepared,&mut maintenance,&mut hydration,cancel,&mut referenced).await?.stored(repository)?)
    } else { None };
    let (bytes, fingerprint, sections, role) = match metadata.purpose {
        SnapshotPurpose::LwwCheckpoint { covered_prefixes, state_identity, asset_catalogs, standalone_bodies } => {
            if !published_sections.is_empty() { return Err(corrupt("checkpoint device sections")); }
            let document = super::lww_checkpoint::Checkpoint {
                schema: "risunest.lww-snapshot/v1".into(),
                snapshot_id: metadata.snapshot_id.clone(),
                repository_id: metadata.repository_id.clone(),
                library_id: metadata.library_id,
                covered_prefixes,
                state_identity,
                library,
                asset_catalogs,
                standalone_bodies,
            };
            let bytes = document.encode()?;
            (bytes.clone(), hash(&bytes), BTreeMap::new(), ObjectRole::Snapshot)
        }
        SnapshotPurpose::SyncState {
            epoch, generation, ..
        } => {
            let document = wire::SyncStateDocument::new(
                metadata.snapshot_id.clone(),
                metadata.repository_id.clone(),
                metadata.library_id,
                epoch,
                generation,
                metadata.parent_snapshot_id,
                metadata.author_device_id,
                metadata.created_at_ms,
                library,
                published_sections,
            );
            #[cfg(test)] super::control::observe_state_result(&document,2);
            let document=document.map_err(corrupt)?;
            let max_state = wire::MAX_METADATA_BYTES;
            let encoded=document.encode(max_state);
            #[cfg(test)]
            super::control::observe_control_fingerprint(wire::ObjectRole::SyncState,&document.library_fingerprint,&document.sections,None,1);
            (
                encoded.map_err(corrupt)?,
                document.state_fingerprint,
                document.sections,
                ObjectRole::SyncState,
            )
        }
        SnapshotPurpose::BackupBundle {
            source,
            remote_generation,
            original_units: _,
        } => {
            let document = wire_control::BackupBundleDocument::new(
                metadata.repository_id.clone(),
                metadata.snapshot_id.clone(),
                source,
                metadata.created_at_ms,
                Some(Sequence::from(metadata.logical_revision)),
                None,
                remote_generation,
                library,
                published_sections,
                original_units_root,
            );
            #[cfg(test)] super::control::observe_bundle_result(&document,2);
            let document=document.map_err(corrupt)?;
            let max_bundle = wire::MAX_METADATA_BYTES;
            (
                document.encode(max_bundle).map_err(corrupt)?,
                document.bundle_fingerprint,
                document.sections,
                ObjectRole::BackupBundle,
            )
        }
    };
    let metadata_key =
        derive_key(root_key, &metadata.repository_id, "metadata").map_err(corrupt)?;
    let mut file = tempfile::NamedTempFile::new_in(&build_root).map_err(transient)?;
    file.write_all(&bytes).map_err(transient)?;
    file.as_file_mut().sync_all().map_err(transient)?;
    if let Some((owner,context))=protection {
        owner.renew_if_due(context,cancel).await?;
        if let Some(reason)=owner.recheck(context,cancel).await? {return Err(super::leases::yield_error(reason));}
    }
    // The stable random snapshot ID is the immutable object identity. A retry
    // with different bytes is rejected by the journal instead of overwriting it.
    let reference = upload_plain_object(
        file.path(),
        bytes.len() as u64,
        hash(&bytes),
        if role == ObjectRole::Snapshot { metadata.snapshot_id.clone() } else { format!("snapshot-{}", metadata.snapshot_id) },
        role,
        &metadata.repository_id,
        root_key,
        &metadata_key,
        limits,
        &mut cache,
        &mut evidence,
        journal,
        provider,
        repository,
        cancel,
    )
    .await?;
    referenced.sort_by(|a, b| a.object_id.cmp(&b.object_id));
    referenced.dedup_by(|a, b| a.object_id == b.object_id);
    Ok(CompletedSnapshot {
        #[cfg(test)]
        compaction_catalog_merge_visits: None,
        #[cfg(test)]
        compaction_capture_rows: None,
        maintenance,
        hydration,
        reference,
        snapshot_id: metadata.snapshot_id,
        repository_id: metadata.repository_id,
        fingerprint: hex::encode(fingerprint),
        library_fingerprint: hex::encode(metadata.content_fingerprint),
        logical_revision: metadata.logical_revision,
        record_catalog,
        asset_catalog,
        sections,
        referenced_objects: referenced,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static BOUNDARY_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
            std::cell::RefCell::new(None);
    }
    struct BoundaryHook;
    impl BoundaryHook {
        fn install(callback: impl FnMut() + 'static) -> Self {
            BOUNDARY_HOOK.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(Box::new(callback));
            });
            Self
        }
    }
    impl Drop for BoundaryHook {
        fn drop(&mut self) {
            BOUNDARY_HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }
    pub(super) fn observe_boundary() {
        BOUNDARY_HOOK.with(|slot| {
            if let Some(callback) = slot.borrow_mut().as_mut() { callback(); }
        });
    }

    #[test]
    fn cancellation_after_first_carried_chunk_stops_before_reading_the_remaining_source() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, mpsc};
        use std::time::{Duration, Instant};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("carried-source");
        let chunk = vec![7u8; MAX_CHUNK_BYTES];
        let bytes = chunk.repeat(8);
        fs::write(&path, &bytes).unwrap();
        let digest = hash(&chunk);
        let compression = CompressionPolicy::AlreadyCompressed;
        let placements = BTreeMap::from([((hex::encode(digest), chunk.len() as u64,
            compression_tag(compression)), wire::StoredChunk {
                pack_id: "parent-pack".into(), offset: 0,
                stored_length: chunk.len() as u64 + ENTRY_OVERHEAD as u64 + 1,
                plaintext_length: chunk.len() as u64, plaintext_sha256: digest,
            })]);
        let source = SourceEntry {
            kind: wire::CatalogEntryKind::Object, key: "carried-source".into(),
            content_sha256: hex::encode(hash(&bytes)), byte_length: bytes.len() as u64,
            source: ObjectSource::File(path), file_offset: Some(0), compression,
        };
        let runtime = runtime();
        let handle = runtime.handle().clone();
        let directory = root.path().to_path_buf();
        let cancel = Cancellation::default();
        let worker_cancel = cancel.clone();
        let observed = Arc::new(AtomicUsize::new(0));
        let worker_observed = observed.clone();
        let (arrived, arrival) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let (completed, completion) = mpsc::sync_channel(1);
        let (built, mut packs) = tokio::sync::mpsc::channel(2);
        let spawned = Instant::now();
        std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                let _hook = BoundaryHook::install(move || {
                    if worker_observed.fetch_add(1, Ordering::SeqCst) == 0 {
                        arrived.send(()).unwrap();
                        released.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                });
                let outcome = prepare_packs(vec![source], &directory, &directory.join("external"),
                    &directory.join("build"), DEFAULT_TARGET_BYTES, DEFAULT_MAX_STORED_BYTES,
                    MAX_CHUNK_BYTES, &placements, &built, &handle, &PackFamilies::new(1),
                    &PhaseProgress::silent(), &worker_cancel);
                completed.send(outcome.err().map(|error| error.kind)).unwrap();
            });
            // The worker first queues for the process-wide CPU permit, which concurrent tests
            // can hold for seconds, so this bound only turns a deadlock into a failure.
            arrival.recv_timeout(Duration::from_secs(300)).unwrap_or_else(|error| panic!(
                "first carried chunk did not finish: {error} after {} ms", spawned.elapsed().as_millis()));
            let stopped_at = Instant::now();
            cancel.cancel();
            release.send(()).unwrap();
            assert_eq!(completion.recv_timeout(Duration::from_secs(5)).expect("carried source kept reading after cancellation"),
                Some(ErrorKind::Cancelled));
            worker.join().unwrap();
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            assert!(packs.try_recv().is_err());
            assert_eq!(build_files(&root.path().join("build")), 0);
            println!("carried_chunks=1 remaining_chunks=7 cancellation_ms={}", stopped_at.elapsed().as_millis());
        });
    }


    #[test]
    fn local_file_and_envelope_errors_keep_their_remediation_kind() {
        for (cause, expected) in [
            (std::io::ErrorKind::StorageFull, ErrorKind::LocalStorageFull),
            (std::io::ErrorKind::PermissionDenied, ErrorKind::LocalPermissionDenied),
            (std::io::ErrorKind::ReadOnlyFilesystem, ErrorKind::LocalPermissionDenied),
            (std::io::ErrorKind::WouldBlock, ErrorKind::LocalFailure),
            (std::io::ErrorKind::NotFound, ErrorKind::LocalFailure),
        ] {
            assert_eq!(transient(std::io::Error::from(cause)).kind, expected);
            assert_eq!(format_error(std::io::Error::from(cause).into()).kind, expected);
        }
        assert_eq!(format_error(risunest_external_storage_format::FormatError("object-authentication-failed")).kind,
            ErrorKind::Corrupt);
        assert_eq!(format_error(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()).kind,
            ErrorKind::Corrupt);
    }

    use rusqlite::params;
    use crate::{
        external_storage::{
            fake::{self, FakeProvider},
            journal::JobIdentity,
            snapshot_restore,
        },
        persistent_store::{
            content_capture::ContentCaptureSink,
            device_store::sections::{SectionRow, SectionValueRow},
            sync_selection::CaptureIdentity, PersistentStore,
        },
    };
    use risunest_external_storage_format::section::SectionKind;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn captured(
        root: &Path,
        capture_id: &str,
        revision: i64,
        record: &[u8],
        asset: &[u8],
    ) -> (CapturedSnapshot, String) {
        let cas = PayloadCas::new(root).unwrap();
        let asset = cas.prepare_bytes(asset).unwrap();
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let external = root.join("external-storage");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &external, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", record).unwrap();
        catalog
            .reference("root", &asset.content_hash, asset.byte_size)
            .unwrap();
        catalog.finish().unwrap();
        (
            CapturedSnapshot {
                id: capture_id.into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            },
            asset.content_hash,
        )
    }

    /// How many plaintext files a publication is holding in the directory it
    /// builds them in, which is what bounds the overlap between preparing a
    /// pack and sending the one before it.
    fn build_files(directory: &Path) -> u64 {
        fs::read_dir(directory)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.metadata().is_ok_and(|entry| entry.is_file()))
                    .count() as u64
            })
            .unwrap_or_default()
    }

    /// Samples a job directory from another thread while a publication runs.
    fn peak_spool_sampler(
        directory: std::path::PathBuf,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        peak_sampler(directory, super::super::journal::spool_bytes)
    }

    /// The same sampling for whatever a measurement wants to watch.
    fn peak_sampler(
        directory: std::path::PathBuf,
        measure: fn(&Path) -> u64,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let peak = peak.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    peak.fetch_max(measure(&directory), std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            })
        };
        (peak, stop, handle)
    }
    /// The pack the asset catalog names. Its content does not move with the
    /// revision, so a later publication of the same library reuses it.
    fn asset_pack(
        provider: &FakeProvider,
        completed: &CompletedSnapshot,
        key: &[u8; 32],
    ) -> RemoteObject {
        let bytes = provider
            .contents(&completed.asset_catalog.receipt.locator.object)
            .unwrap();
        let metadata_key = derive_key(key, &completed.repository_id, "metadata").unwrap();
        let mut plaintext = Vec::new();
        wire::open_envelope(
            &mut std::io::Cursor::new(bytes),
            &mut plaintext,
            &metadata_key,
            wire::MAX_METADATA_BYTES as u64,
        )
        .unwrap();
        let catalog = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES).unwrap();
        let locator = catalog.packs[0].locator.object.clone();
        completed
            .referenced_objects
            .iter()
            .find(|object| object.receipt.locator.object == locator)
            .unwrap()
            .clone()
    }

    /// The packs the repository holds, which is what a publication actually
    /// placed rather than what it attempted.
    fn stored_packs(provider: &FakeProvider) -> Vec<String> {
        provider
            .state
            .lock()
            .unwrap()
            .objects
            .keys()
            .filter(|id| id.starts_with("pack-"))
            .cloned()
            .collect()
    }

    /// A capture whose payloads are large enough that one publication has to
    /// build several packs. The digests come back in the order the sources are
    /// read, so a test can reach the payload a publication gets to last.
    fn captured_packed_assets(
        root: &Path,
        capture_id: &str,
        assets: usize,
        size: usize,
    ) -> (CapturedSnapshot, Vec<String>) {
        let cas = PayloadCas::new(root).unwrap();
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let external = root.join("external-storage");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &external, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        };
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", b"record").unwrap();
        let mut digests = Vec::new();
        for index in 0..assets {
            let mut bytes = vec![0u8; size];
            let mut value = (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            for slot in bytes.chunks_mut(8) {
                value ^= value << 13;
                value ^= value >> 7;
                value ^= value << 17;
                let filled = slot.len();
                slot.copy_from_slice(&value.to_le_bytes()[..filled]);
            }
            let stored = cas.prepare_bytes(&bytes).unwrap();
            catalog
                .reference("root", &stored.content_hash, stored.byte_size)
                .unwrap();
            digests.push(stored.content_hash);
        }
        catalog.finish().unwrap();
        digests.sort();
        (
            CapturedSnapshot {
                id: capture_id.into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            },
            digests,
        )
    }

    fn captured_asset_inventory(
        root: &Path,
        capture_id: &str,
        revision: i64,
        records: usize,
        edited: usize,
        assets: usize,
    ) -> CapturedSnapshot {
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let external = root.join("external-storage");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &external, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: format!("generation-{revision}"),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        let unchanged = b"{\"value\":0}".to_vec();
        let changed = format!("{{\"value\":{revision}}}").into_bytes();
        let unchanged_hash = hex::encode(hash(&unchanged));
        let changed_hash = hex::encode(hash(&changed));
        let objects = external.join("objects");
        fs::write(objects.join(&unchanged_hash), &unchanged).unwrap();
        fs::write(objects.join(&changed_hash), &changed).unwrap();
        {
            let mut insert = catalog
                .db
                .prepare("INSERT INTO records VALUES(?1,?2,?3)")
                .unwrap();
            for index in 0..records {
                let (digest, length) = if index < edited {
                    (&changed_hash, changed.len())
                } else {
                    (&unchanged_hash, unchanged.len())
                };
                insert
                    .execute(params![format!("record/{index:06}"), digest, length as i64])
                    .unwrap();
            }
        }
        let _cas = PayloadCas::new(root).unwrap();
        let asset_root = root.join("assets").join("objects");
        for shard in 0u16..=255 {
            fs::create_dir_all(asset_root.join(format!("{shard:02x}"))).unwrap();
        }
        let mut insert_dependency = catalog
            .db
            .prepare("INSERT INTO dependencies VALUES(?1,?2,?3)")
            .unwrap();
        for index in 0..assets {
            let bytes = (index as u64).to_le_bytes();
            let digest = hex::encode(hash(&bytes));
            let shard = asset_root.join(&digest[..2]);
            let path = shard.join(&digest[2..]);
            if !path.exists() {
                fs::write(path, bytes).unwrap();
            }
            insert_dependency
                .execute(params!["record/000000", digest, bytes.len() as i64])
                .unwrap();
        }
        drop(insert_dependency);
        catalog.finish().unwrap();
        CapturedSnapshot {
            id: capture_id.into(),
            identity,
            catalog,
            projected_records: records,
            shared: false,
        }
    }

    /// A library of `records` text records over `assets` shared assets, where
    /// `edited` of them carry this revision's value starting at `offset` and
    /// wrapping. Rotating the offset each revision is what spreads a library's
    /// live records across the packs successive publications wrote.
    fn captured_record_window(
        root: &Path,
        capture_id: &str,
        revision: i64,
        records: usize,
        edited: usize,
        offset: usize,
        assets: usize,
    ) -> CapturedSnapshot {
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let external = root.join("external-storage");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &external, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: format!("generation-{revision}"),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        let objects = external.join("objects");
        {
            let mut insert = catalog
                .db
                .prepare("INSERT INTO records VALUES(?1,?2,?3)")
                .unwrap();
            for index in 0..records {
                let rotated = records.max(1);
                let distance = (index + rotated - offset % rotated) % rotated;
                // Every record carries its own body, so an edit produces content
                // no other record shares. A fixture where a whole revision's
                // edits are one body would pack into far fewer places than a
                // library does.
                let value = if distance < edited { revision } else { 0 };
                let body = format!("{{\"record\":{index},\"value\":{value}}}").into_bytes();
                let digest = hex::encode(hash(&body));
                let path = objects.join(&digest);
                if !path.exists() {
                    fs::write(path, &body).unwrap();
                }
                insert
                    .execute(params![format!("record/{index:06}"), digest, body.len() as i64])
                    .unwrap();
            }
        }
        let _cas = PayloadCas::new(root).unwrap();
        let asset_root = root.join("assets").join("objects");
        let mut insert_dependency = catalog
            .db
            .prepare("INSERT INTO dependencies VALUES(?1,?2,?3)")
            .unwrap();
        for index in 0..assets {
            let bytes = (index as u64).to_le_bytes();
            let digest = hex::encode(hash(&bytes));
            let shard = asset_root.join(&digest[..2]);
            fs::create_dir_all(&shard).unwrap();
            let path = shard.join(&digest[2..]);
            if !path.exists() {
                fs::write(path, bytes).unwrap();
            }
            insert_dependency
                .execute(params!["record/000000", digest, bytes.len() as i64])
                .unwrap();
        }
        drop(insert_dependency);
        catalog.finish().unwrap();
        CapturedSnapshot {
            id: capture_id.into(),
            identity,
            catalog,
            projected_records: records,
            shared: false,
        }
    }

    /// A capture holding exactly the records at `keys`, those in `edited` at
    /// this revision's value and the rest at their original bodies.
    fn captured_records(
        root: &Path,
        capture_id: &str,
        revision: i64,
        keys: std::ops::Range<usize>,
        edited: std::ops::Range<usize>,
    ) -> CapturedSnapshot {
        let external = root.join("external-storage");
        let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
            &external.join("captures").join(capture_id),
            &external,
            None,
        )
        .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: format!("generation-{revision}"),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        let objects = external.join("objects");
        {
            let mut insert = catalog
                .db
                .prepare("INSERT INTO records VALUES(?1,?2,?3)")
                .unwrap();
            for index in keys {
                let value = if edited.contains(&index) { revision } else { 0 };
                let body = format!("{{\"record\":{index},\"value\":{value}}}").into_bytes();
                let digest = hex::encode(hash(&body));
                let path = objects.join(&digest);
                if !path.exists() {
                    fs::write(path, &body).unwrap();
                }
                insert
                    .execute(params![format!("record/{index:06}"), digest, body.len() as i64])
                    .unwrap();
            }
        }
        catalog.finish().unwrap();
        CapturedSnapshot {
            id: capture_id.into(),
            identity,
            catalog,
            projected_records: 0,
            shared: false,
        }
    }

    /// The packs a published record catalog names, read from what the cache
    /// recorded for its root.
    fn record_graph_packs(
        cache_root: &Path,
        completed: &CompletedSnapshot,
        repository: &RepositoryHandle,
    ) -> BTreeSet<String> {
        let cache = PackageCache::open(cache_root).unwrap();
        let identity =
            super::super::reachability::object_identity(&completed.record_catalog, repository)
                .unwrap();
        let graph = cache
            .graph_of(&completed.repository_id, repository, &identity)
            .unwrap();
        assert!(!graph.is_empty(), "the record catalog's graph was not recorded");
        graph
            .into_iter()
            .filter(|object| object.role == ObjectRole::Pack)
            .map(|object| object.object_id)
            .collect()
    }

    fn metadata(id: &str, capture: &CapturedSnapshot) -> SnapshotMetadata {
        SnapshotMetadata {
            snapshot_id: id.into(),
            repository_id: "format-repository".into(),
            library_id: capture.identity.library_epoch.clone(),
            author_device_id: capture.identity.store_id.clone(),
            created_at_ms: 1,
            logical_revision: capture.identity.revision as u64,
            purpose: SnapshotPurpose::SyncState {
                epoch: "epoch".into(),
                generation: risunest_sync_wire::head::Sequence::from(1u64),
                parent_sections: std::collections::BTreeMap::new(),
            },
            parent_snapshot_id: None,
            content_fingerprint: capture.catalog.content_fingerprint(&library_fingerprint_domain()).unwrap(),
        }
    }

    fn journal(root: &Path, job: &str, capture: &CapturedSnapshot) -> TransferJournal {
        TransferJournal::open(
            root,
            JobIdentity {
                job_id: job.into(),
                connection_id: "connection".into(),
                repository_id: fake::repository().repository_id,
                capture_id: capture.id.clone(),
                capture: capture.identity.clone(),
            },
        )
        .unwrap()
    }

    /// What the next publication may reuse from, the way a sync publication
    /// builds it out of the state it observed.
    fn parent_graph(
        previous: &CompletedSnapshot,
        repository: &RepositoryHandle,
    ) -> Result<ParentGraph> {
        let mut roots = vec![
            (CatalogRoot::Records, previous.record_catalog.stored(repository)?),
            (CatalogRoot::Assets, previous.asset_catalog.stored(repository)?),
        ];
        roots.extend(previous.sections.iter().map(|(id, section)| {
            (
                CatalogRoot::Section(id.clone()),
                section.entries_root.clone(),
            )
        }));
        ParentGraph::new(roots, repository)
    }

    fn live_pack(
        id: &str,
        ciphertext: u64,
        bytes: u64,
        first: &str,
        leaves: impl IntoIterator<Item = usize>,
    ) -> LivePack {
        LivePack {
            object_id: id.into(),
            ciphertext_length: ciphertext,
            bytes,
            first_key: first.into(),
            leaves: leaves.into_iter().collect(),
            keys: vec![(first.into(), bytes)],
            rewritable: true,
        }
    }

    /// The trigger, the pack cap, the byte cap, the leaf cap and the size
    /// rule, each on its own. Selection is by first live key, so what it takes
    /// is an adjacent run rather than whichever packs happen to be cheapest.
    #[test]
    fn small_live_packs_are_selected_as_one_adjacent_bounded_run() {
        // Four neighbouring packs share a leaf.
        let small = |count: usize, bytes: u64| {
            (0..count)
                .map(|index| {
                    live_pack(
                        &format!("pack-{index:03}"), 1024, bytes,
                        &format!("record/{index:06}"), [index / 4],
                    )
                })
                .collect::<Vec<_>>()
        };
        let ids = |selected: Vec<LivePack>| {
            selected.into_iter().map(|pack| pack.object_id).collect::<Vec<_>>()
        };
        assert!(select_small_packs(small(31, 16)).is_empty());
        let selected = ids(select_small_packs(small(32, 16)));
        assert_eq!(selected.len(), 16);
        assert_eq!(selected.last().map(String::as_str), Some("pack-015"));
        assert_eq!(select_small_packs(small(64, 16)).len(), 16);

        // Four packs of a mebibyte each fill the cycle's source budget before
        // the pack cap does.
        assert_eq!(select_small_packs(small(40, 1024 * 1024)).len(), 4);

        // A pack to a leaf reaches the leaf cap first, and the run stops at the
        // pack that would bring a ninth leaf.
        let spread = (0..32)
            .map(|index| {
                live_pack(&format!("pack-{index:03}"), 1024, 16, &format!("record/{index:06}"), [index])
            })
            .collect::<Vec<_>>();
        let selected = ids(select_small_packs(spread));
        assert_eq!(selected.len(), COALESCE_MAX_LEAVES);
        assert_eq!(selected.last().map(String::as_str), Some("pack-007"));

        // A first pack whose keys sit in more leaves than a cycle may change
        // stops the run before it starts.
        let mut wide = small(32, 16);
        wide[0].leaves = (0..=COALESCE_MAX_LEAVES).collect();
        assert!(select_small_packs(wide).is_empty());

        // A pack over the small size is not eligible, and without 32 that are,
        // nothing is selected.
        let mut mixed = small(20, 16);
        mixed.extend((20..40).map(|index| {
            live_pack(
                &format!("pack-{index:03}"),
                SMALL_PACK_CIPHERTEXT_BYTES + 1,
                16,
                &format!("record/{index:06}"),
                [index / 4],
            )
        }));
        assert!(select_small_packs(mixed).is_empty());

        // Nor is a pack with an entry the capture cannot write again.
        let mut held = small(32, 16);
        held[31].rewritable = false;
        assert!(select_small_packs(held).is_empty());
    }

    fn limits(max: u64) -> PackageLimits {
        PackageLimits {
            max_stored_bytes: max,
            sdk_overhead_bytes: 0,
            target_plaintext_bytes: max,
            maintenance: Default::default(),
        }
    }

    /// Sets this thread's coalescing trigger and pack cap, so a library ages
    /// past them in fewer publications than the product's 32 and 16 take.
    fn coalesce_packs_on_this_thread(trigger: usize, max: usize) -> CoalescePacks {
        let packs = CoalescePacks { trigger, max };
        TEST_COALESCE_PACKS.with(|value| value.set(packs));
        packs
    }

    fn section_rows(kind: SectionKind, marker: &str) -> Vec<SectionRow> {
        use crate::persistent_store::device_store::sections::SectionValueRow;
        match kind {
            SectionKind::Hypa => vec![SectionRow {
                key1: "a".repeat(64),
                key2: String::new(),
                key3: String::new(),
                value: SectionValueRow::Hypa {
                    producer: "hypa-v2".into(),
                    model: marker.into(),
                    endpoint: None,
                    preprocess_version: 1,
                    dimensions: 4,
                    vector: vec![1u8; 16],
                    metadata: None,
                },
                write_clock: risunest_sync_wire::head::Sequence::from(5u64),
                writer_id: "writer-a".into(),
            }],
            _ => vec![SectionRow {
                key1: "plugin-a".into(),
                key2: "string".into(),
                key3: "token".into(),
                value: SectionValueRow::Plugin {
                    space: "string".into(),
                    value: marker.into(),
                },
                write_clock: risunest_sync_wire::head::Sequence::from(6u64),
                writer_id: "writer-a".into(),
            }],
        }
    }

    fn materialized_section_rows(
        source: &CapturedSection,
        versioned: bool,
    ) -> Vec<SectionRow> {
        if !versioned {
            crate::external_storage::sections::prepare_received_backup_sections(
                std::slice::from_ref(source),
                &std::env::temp_dir(),
                &Cancellation::default(),
            )
            .unwrap();
        }
        let entries = source
            .sources
            .iter()
            .map(|file| {
                (
                    file.kind,
                    file.key.clone(),
                    crate::external_storage::sections::read_source_bytes(file, &Cancellation::default())
                        .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        crate::external_storage::sections::decode_section(
            source.kind,
            &entries,
            &source.content_fingerprint,
        )
        .unwrap()
    }

    fn section_values(
        rows: Vec<SectionRow>,
    ) -> Vec<(String, String, String, SectionValueRow)> {
        rows.into_iter()
            .map(|row| (row.key1, row.key2, row.key3, row.value))
            .collect()
    }

    fn section(
        spool: &Path,
        kind: SectionKind,
        marker: &str,
        generation: u64,
    ) -> CapturedSection {
        crate::external_storage::sections::capture_section(
            kind,
            &section_rows(kind, marker),
            generation != 0,
            risunest_sync_wire::head::Sequence::from(generation),
            risunest_sync_wire::head::Sequence::from(0u64),
            risunest_sync_wire::head::Sequence::from(if generation == 0 { 0u64 } else { 6u64 }),
            &spool.join(format!("{}-{marker}", kind.id())),
            &Cancellation::default(),
        )
        .unwrap()
    }

    /// A23. A download reports what it moves while it is moving it. Every pack
    /// a catalog needs is planned before the first one is fetched, and a pack
    /// counts once it has arrived, so a receive is no longer a job that reports
    /// nothing from start to finish.
    #[test]
    fn a_download_counts_every_pack_it_opens() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [13; 32];
            let (capture, _) = captured(root.path(), "download-capture", 1, b"record", b"asset");
            let snapshot_metadata = metadata("download-snapshot", &capture);
            let mut transfer = journal(&root.path().join("download-job"), "download-job", &capture);
            let completed = package_and_upload(
                capture,
                Vec::new(),
                root.path(),
                &root.path().join("download-cache"),
                snapshot_metadata,
                &key,
                limits(256 * 1024),
                None,
                &mut transfer,
                &provider,
                &repository,
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let readings = Arc::new(std::sync::Mutex::new(Vec::new()));
            let collected = Arc::clone(&readings);
            let progress =
                PhaseProgress::new(move |reading| collected.lock().unwrap().push(reading));
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &root.path().join("download-restore"),
                &key,
                None,
                snapshot_restore::SourceTrust::Downloaded,
                &provider,
                &repository,
                &progress,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(restored.records.len(), 1);

            // Nothing has arrived yet and the download already says how much
            // it is about to fetch.
            let first = readings.lock().unwrap().first().copied().unwrap();
            assert_eq!(first.items, 0);
            assert!(first.total_items > 0);

            let packs: Vec<_> = completed
                .referenced_objects
                .iter()
                .filter(|object| object.role == ObjectRole::Pack)
                .collect();
            let reading = progress.read();
            assert_eq!(reading.total_items, packs.len() as u64);
            assert_eq!(reading.items, reading.total_items);
            assert_eq!(
                reading.total_bytes,
                packs.iter().map(|pack| pack.receipt.byte_length).sum::<u64>(),
            );
            assert_eq!(reading.bytes, reading.total_bytes);
        });
    }

    /// Invariant 23. A publication reads, compresses and packs everything it
    /// carries before it seals its first object, so a job that only counts
    /// what the transfer journal holds reports nothing for the longest part of
    /// its own work. Every domain the publication carries is counted, which is
    /// why adding sections to the same library raises what it plans.
    #[test]
    fn a_publication_counts_every_domain_it_places_before_it_uploads_anything() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let spool = root.path().join("section-spool");
            let carried = vec![
                section(&spool, SectionKind::Hypa, "counted", 1),
                section(&spool, SectionKind::LocalPlugins, "counted", 1),
            ];
            let section_sources: u64 =
                carried.iter().map(|part| part.sources.len() as u64).sum();
            let publish = |id: &'static str, sections: Vec<CapturedSection>| {
                let root = root.path().to_path_buf();
                async move {
                    let provider = FakeProvider::new(false);
                    let repository = fake::repository();
                    let (capture, _) =
                        captured(&root, &format!("capture-{id}"), 1, b"record", b"asset");
                    let mut transfer = journal(&root.join(id), id, &capture);
                    let snapshot_metadata = metadata(id, &capture);
                    let reports: Arc<
                        std::sync::Mutex<
                            Vec<(
                                crate::external_storage::phase_progress::PhaseCounters,
                                std::time::Instant,
                            )>,
                        >,
                    > = Arc::new(std::sync::Mutex::new(Vec::new()));
                    let collected = Arc::clone(&reports);
                    let prepared = PhaseProgress::new(move |reading| {
                        collected
                            .lock()
                            .unwrap()
                            .push((reading, std::time::Instant::now()));
                    });
                    package_and_upload(
                        capture,
                        sections,
                        &root,
                        &root.join(format!("{id}-cache")),
                        snapshot_metadata,
                        &[9; 32],
                        limits(256 * 1024),
                        None,
                        &mut transfer,
                        &provider,
                        &repository,
                        &prepared,
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap();
                    let first = reports.lock().unwrap().first().copied().unwrap();
                    (prepared.read(), first, provider.first_upload().unwrap())
                }
            };
            let (library, first_library, first_library_upload) = publish("counted-1", Vec::new()).await;

            // Nothing is placed yet and the job already knows what it is
            // working through, before anything reaches the repository.
            assert_eq!(first_library.0.items, 0);
            assert!(first_library.0.total_items > 0);
            assert!(first_library.1 < first_library_upload);

            // What was planned is what was placed.
            assert_eq!(library.items, library.total_items);
            assert_eq!(library.bytes, library.total_bytes);

            let (everything, _, _) = publish("counted-2", carried).await;
            assert_eq!(everything.items, everything.total_items);
            assert_eq!(everything.total_items, library.total_items + section_sources);
            assert!(everything.total_bytes > library.total_bytes);
        });
    }

    type PlannedReading = (crate::external_storage::phase_progress::PhaseCounters, usize);

    /// Publishes the synthetic library under `purpose`, recording each reading
    /// with the provider requests answered when it arrived.
    #[allow(clippy::too_many_arguments)]
    async fn publish_planned(
        root: &Path,
        id: &str,
        cache: &Path,
        purpose: SnapshotPurpose,
        sections: Vec<CapturedSection>,
        provider: &Arc<FakeProvider>,
        repository: &RepositoryHandle,
    ) -> (CompletedSnapshot, Vec<PlannedReading>, crate::external_storage::phase_progress::PhaseCounters, usize) {
        let (capture, _) = captured(root, &format!("capture-{id}"), 1, b"planned record", b"planned asset");
        let mut transfer = journal(&root.join(id), id, &capture);
        let mut snapshot_metadata = metadata(id, &capture);
        snapshot_metadata.purpose = purpose;
        let readings: Arc<std::sync::Mutex<Vec<PlannedReading>>> = Arc::default();
        let collected = Arc::clone(&readings);
        let observed = Arc::clone(provider);
        let prepared = PhaseProgress::new(move |reading| {
            collected.lock().unwrap().push((reading, observed.request_count()));
        });
        let before = provider.request_count();
        let completed = package_and_upload(
            capture, sections, root, cache, snapshot_metadata, &[31; 32], limits(256 * 1024), None,
            &mut transfer, provider.as_ref(), repository, &prepared, &Cancellation::default(),
        )
        .await
        .unwrap();
        let readings = readings.lock().unwrap().clone();
        (completed, readings, prepared.read(), before)
    }

    /// The first reading already carries the total the preparation finishes
    /// at, it arrives before the provider is asked anything, and no reading
    /// in between moves the total or runs past it.
    fn assert_planned_once(
        readings: &[PlannedReading],
        last: crate::external_storage::phase_progress::PhaseCounters,
        requests_before: usize,
    ) {
        let (first, requests) = readings.first().copied().unwrap();
        assert_eq!(requests, requests_before);
        assert_eq!((first.items, first.bytes), (0, 0));
        for (reading, _) in readings {
            assert_eq!((reading.total_items, reading.total_bytes), (last.total_items, last.total_bytes));
            assert!(reading.items <= reading.total_items && reading.bytes <= reading.total_bytes);
        }
        assert_eq!((last.items, last.bytes), (last.total_items, last.total_bytes));
    }

    /// A backup reads, compresses and packs its records, its assets, each
    /// section and every original unit, a page of units at a time, before it
    /// finishes preparing. All of it is planned once, out of what the capture
    /// already says, so the total the job shows first is the one it ends at.
    /// That holds when the asset catalog is already published and only
    /// referenced again, and when a state carries a section unchanged.
    #[test]
    fn a_publication_plans_its_whole_preparation_before_the_first_domain() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let spool = root.path().join("section-spool");
            let hypa = section(&spool, SectionKind::Hypa, "planned", 1);
            let plugins = section(&spool, SectionKind::LocalPlugins, "planned", 1);
            let changed = section(&spool, SectionKind::LocalPlugins, "changed", 1);
            let provider = Arc::new(FakeProvider::new(false));
            let repository = fake::repository();
            let cache = root.path().join("planned-cache");
            // More units than one page of the original unit catalog build.
            let bundle = || SnapshotPurpose::BackupBundle {
                source: wire_control::BundleSource::Device { writer_id: "synthetic-writer".into() },
                remote_generation: None,
                original_units: (0..300)
                    .map(|index| {
                        (
                            risunest_sync_wire::unit::UnitKey::new(&["future-opaque", &format!("planned-{index:03}")]).unwrap(),
                            risunest_sync_wire::unit::UnitValue::object(
                                risunest_sync_wire::descriptor::RecordDescriptor::content(hex::encode(hash(
                                    format!("synthetic planned unit {index}").as_bytes(),
                                ))),
                            )
                            .unwrap(),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
                    .into(),
            };

            let (first, readings, last, before) = publish_planned(
                root.path(), "planned-1", &cache, bundle(), vec![hypa.clone(), plugins.clone()], &provider, &repository,
            )
            .await;
            assert_planned_once(&readings, last, before);
            let section_sources = (hypa.sources.len() + plugins.sources.len()) as u64;
            assert_eq!(last.total_items, 2 + section_sources + 300);

            // The asset catalog is referenced again rather than built, and its
            // asset still counts as prepared.
            let (again, readings, last, before) = publish_planned(
                root.path(), "planned-2", &cache, bundle(), vec![hypa.clone(), plugins.clone()], &provider, &repository,
            )
            .await;
            assert_eq!(again.asset_catalog.object_id, first.asset_catalog.object_id);
            assert_planned_once(&readings, last, before);
            assert_eq!(last.total_items, 2 + section_sources + 300);

            // A state carries its unchanged section and prepares only the one
            // that changed.
            let state = SnapshotPurpose::SyncState {
                epoch: "epoch".into(),
                generation: risunest_sync_wire::head::Sequence::from(2u64),
                parent_sections: first.sections.clone(),
            };
            let (carried, readings, last, before) = publish_planned(
                root.path(), "planned-3", &cache, state, vec![hypa.clone(), changed.clone()], &provider, &repository,
            )
            .await;
            assert_eq!(carried.sections[SectionKind::Hypa.id()], first.sections[SectionKind::Hypa.id()]);
            assert_planned_once(&readings, last, before);
            assert_eq!(last.total_items, 2 + changed.sources.len() as u64);
        });
    }

    /// Plugin and setting text carried as section objects is compressed when
    /// it shrinks, while Hypa vectors are stored as they are.
    #[test]
    fn section_text_objects_are_compressed_and_hypa_vectors_are_stored_raw() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let spool = root.path().join("section-spool");
            let row = |key1: &str, value: SectionValueRow| SectionRow {
                key1: key1.into(), key2: "string".into(), key3: "token".into(), value,
                write_clock: risunest_sync_wire::head::Sequence::from(6u64),
                writer_id: "writer-a".into(),
            };
            let capture_rows = |kind: SectionKind, rows: Vec<SectionRow>| {
                crate::external_storage::sections::capture_section(
                    kind, &rows, true,
                    risunest_sync_wire::head::Sequence::from(1u64),
                    risunest_sync_wire::head::Sequence::from(0u64),
                    risunest_sync_wire::head::Sequence::from(6u64),
                    &spool.join(kind.id()), &Cancellation::default(),
                ).unwrap()
            };
            let text = "synthetic plugin text ".repeat(1024);
            let vector = vec![0u8; 4096 * 4];
            let plugins = capture_rows(SectionKind::LocalPlugins, vec![row("plugin-a", SectionValueRow::Plugin {
                space: "string".into(), value: text.clone(),
            })]);
            let mut hypa = row(&"a".repeat(64), SectionValueRow::Hypa {
                producer: "hypa-v2".into(), model: "synthetic".into(), endpoint: None,
                preprocess_version: 1, dimensions: 4096, vector: vector.clone(), metadata: None,
            });
            hypa.key2 = String::new();
            hypa.key3 = String::new();
            let hypa = capture_rows(SectionKind::Hypa, vec![hypa]);
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [23; 32];
            let (capture, _) = captured(root.path(), "section-compression", 1, b"record", b"asset");
            let mut transfer = journal(&root.path().join("section-compression-job"), "section-compression", &capture);
            let snapshot_metadata = metadata("section-compression", &capture);
            let completed = package_and_upload(
                capture, vec![plugins, hypa], root.path(), &root.path().join("section-compression-cache"),
                snapshot_metadata, &key, limits(256 * 1024), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            ).await.unwrap();
            for (kind, compressed, body) in [(SectionKind::LocalPlugins, true, text.into_bytes()), (SectionKind::Hypa, false, vector)] {
                let catalog = RemoteObject::from_stored(&completed.sections[kind.id()].entries_root, &repository).unwrap();
                let (entries, packs, _) = snapshot_restore::read_catalog(
                    &catalog, wire::CatalogKind::Section, &key, &root.path().join(format!("read-{}", kind.id())),
                    &provider, &repository, &Cancellation::default(),
                ).await.unwrap();
                let objects: Vec<_> = entries.iter()
                    .filter(|entry| entry.kind == wire::CatalogEntryKind::SectionObject)
                    .collect();
                assert_eq!(objects.len(), 1);
                let stored: u64 = objects[0].chunks.iter().map(|chunk| chunk.stored_length).sum();
                assert_eq!(stored < objects[0].byte_length, compressed, "{} object stored as {stored} of {}", kind.id(), objects[0].byte_length);
                let decoded = snapshot_restore::download_packed_body(
                    &hex::encode(objects[0].content_sha256), objects[0].byte_length, objects[0].chunks.clone(),
                    packs.values().map(|pack| pack.stored(&repository).unwrap()).collect(),
                    &root.path().join(format!("body-{}", kind.id())), &key, &provider, &repository, &Cancellation::default(),
                ).await.unwrap();
                assert_eq!(decoded, body);
            }
        });
    }

    /// Invariants 17, 30 and 33. A publication that does not carry a section
    /// keeps the reference the observed state had, including the commit number
    /// that named the publication which last changed it, and that inherited
    /// catalog is still readable from the newest state.
    #[test]
    fn a_section_this_publication_did_not_capture_keeps_the_reference_it_inherited() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let spool = root.path().join("section-spool");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [9; 32];
            let publish = |id: &'static str,
                           revision: i64,
                           generation: u64,
                           carried: Vec<CapturedSection>,
                           inherited: BTreeMap<String, wire::SectionSnapshotRef>| {
                let cache = cache.clone();
                let provider = &provider;
                let repository = &repository;
                let root = root.path().to_path_buf();
                async move {
                    let (capture, _) =
                        captured(&root, &format!("capture-{id}"), revision, b"record", b"asset");
                    let mut snapshot_metadata = metadata(id, &capture);
                    snapshot_metadata.purpose = SnapshotPurpose::SyncState {
                        epoch: "epoch".into(),
                        generation: risunest_sync_wire::head::Sequence::from(generation),
                        parent_sections: inherited,
                    };
                    let mut transfer = journal(&root.join(id), id, &capture);
                    package_and_upload(
                        capture,
                        carried,
                        &root,
                        &cache,
                        snapshot_metadata,
                        &key,
                        limits(256 * 1024),
                        None,
                        &mut transfer,
                        provider,
                        repository,
                        &PhaseProgress::silent(),
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap()
                }
            };
            let first = publish(
                "state-1",
                1,
                1,
                vec![
                    section(&spool, SectionKind::Hypa, "first", 1),
                    section(&spool, SectionKind::LocalPlugins, "first", 1),
                ],
                BTreeMap::new(),
            )
            .await;
            assert_eq!(first.sections.len(), 2);

            // Only the embeddings changed, so the plugin reference has to come
            // through untouched, commit number included.
            let second = publish(
                "state-2",
                2,
                2,
                vec![section(&spool, SectionKind::Hypa, "second", 2)],
                first.sections.clone(),
            )
            .await;
            assert_eq!(
                second.sections["local-plugins"],
                first.sections["local-plugins"]
            );
            assert_ne!(second.sections["hypa"], first.sections["hypa"]);
            assert_eq!(
                second.sections["hypa"].generation,
                risunest_sync_wire::head::Sequence::from(2u64)
            );

            // Republishing the same embeddings keeps the earlier commit number,
            // because nothing about that section changed.
            let third = publish(
                "state-3",
                3,
                3,
                vec![section(&spool, SectionKind::Hypa, "second", 3)],
                second.sections.clone(),
            )
            .await;
            assert_eq!(third.sections, second.sections);

            // A library-only publication leaves both references in place and
            // the inherited catalogs still read back.
            let fourth = publish("state-4", 4, 4, Vec::new(), third.sections.clone()).await;
            assert_eq!(fourth.sections, third.sections);
            let staging = root.path().join("receive");
            let received = snapshot_restore::download_sections(
                &fourth.reference,
                &BTreeSet::from(["local-plugins".to_owned()]),
                &staging,
                &key,
                None,
                &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(received.len(), 1);
            let rows = materialized_section_rows(&received[0], true);
            assert_eq!(
                section_values(rows),
                section_values(section_rows(SectionKind::LocalPlugins, "first"))
            );
        });
    }

    #[test]
    fn database_first_backup_validates_unindexed_present_asset_without_body_io() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();
            let body=b"synthetic present remote backup body";
            let (capture,hash)=captured(root.path(),"db-first",0,b"{}",body);
            let provider=FakeProvider::new(false);
            let repository=fake::repository();let key=[21;32];
            let mut metadata=metadata("db-first",&capture);
            metadata.purpose=SnapshotPurpose::BackupBundle{source:wire_control::BundleSource::Device{writer_id:"synthetic-writer".into()},remote_generation:None,original_units:BTreeMap::new().into()};
            let mut transfer=journal(&root.path().join("db-first-upload"),"db-first",&capture);
            let completed=package_and_upload(capture,Vec::new(),root.path(),&root.path().join("db-first-cache"),metadata,&key,limits(256*1024),None,&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let cache=PackageCache::open(&root.path().join("db-first-cache")).unwrap();
            let identity=ObjectEvidence::identity(&completed.asset_catalog,&repository).unwrap();
            let asset_packs=cache.graph_of(&completed.repository_id,&repository,&identity).unwrap().into_iter().filter(|object|object.role==ObjectRole::Pack).map(|object|(object.receipt.locator.object.clone(),provider.read_attempts(&object.receipt.locator.object))).collect::<Vec<_>>();
            let destination=tempfile::tempdir().unwrap();
            crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
            PayloadCas::new(destination.path()).unwrap().prepare_bytes(body).unwrap();
            crate::asset_repository::body_io::reset_body_io();
            crate::asset_repository::body_io::register_object_purpose(&hash,crate::asset_repository::body_io::BodyPurpose::Asset);
            crate::external_storage::worker_observation::begin();
            let database=snapshot_restore::download_snapshot_database_first(&completed.reference,&root.path().join("db-first-read"),destination.path(),root.path(),"synthetic-connection",&key,&provider,&repository,&crate::external_storage::phase_progress::PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let workers=crate::external_storage::worker_observation::take();
            let work=crate::asset_repository::body_io::take_body_io();
            assert!(work.complete());assert_eq!(work.asset_work(),Default::default());
            assert!(workers.iter().all(|receipt|receipt.completed));
            assert_eq!(database.required,BTreeSet::from([hash.clone()]));
            assert_eq!(database.present,database.required);assert!(database.missing.is_empty());
            assert!(super::super::lww_residency::packed_source(destination.path(),&hash).unwrap().is_none());
            for (pack,reads) in asset_packs {assert_eq!(provider.read_attempts(&pack),reads);}
            let object=destination.path().join("assets/objects").join(&hash[..2]).join(&hash[2..]);
            fs::write(object,b"synthetic wrong size").unwrap();
            assert!(snapshot_restore::download_snapshot_database_first(&completed.reference,&root.path().join("db-first-invalid"),destination.path(),root.path(),"synthetic-connection",&key,&provider,&repository,&crate::external_storage::phase_progress::PhaseProgress::silent(),&Cancellation::default()).await.is_err());
            assert!(super::super::lww_residency::packed_source(destination.path(),&hash).unwrap().is_none());
        });
    }

    #[test]
    fn database_first_native_restore_activates_before_actual_missing_body_completion() {
        runtime().block_on(async {
            use crate::external_storage::{runtime_restore,job_store::{DurableJob,JobStore}};
            let root=tempfile::tempdir().unwrap();
            let bytes=b"synthetic full restore managed body";
            let mut asset_bytes=vec![bytes.to_vec(),b"synthetic second managed body".to_vec(),b"synthetic present managed body".to_vec()];
            asset_bytes.extend((3..129).map(|index|format!("synthetic grouped asset {index}").into_bytes()));
            let cas=PayloadCas::new(root.path()).unwrap();
            let assets=asset_bytes.iter().map(|bytes|cas.prepare_bytes(bytes).unwrap()).collect::<Vec<_>>();
            let hash=assets[0].content_hash.clone();
            let identity=CaptureIdentity{store_id:"store".into(),library_epoch:"epoch".into(),generation:"generation".into(),selection_epoch:"selection".into(),revision:0};
            let mut catalog=crate::external_storage::capture::CaptureCatalog::create(&root.path().join("external-storage/captures/full-phase"),&root.path().join("external-storage"),None).unwrap();
            catalog.begin(&identity,None).unwrap();
            let record=crate::logical_records::encode_logical_record(&crate::logical_records::LogicalRecordEnvelope::Root{value:serde_json::json!({}),owner_heads:vec![]}).unwrap();
            let root_key=crate::logical_records::encode_logical_record_key(&crate::logical_records::LogicalRecordLocator::Root).unwrap();
            catalog.record(&root_key,&record.bytes).unwrap();
            for asset in &assets {catalog.reference(&root_key,&asset.content_hash,asset.byte_size).unwrap();}
            catalog.finish().unwrap();
            let capture=CapturedSnapshot{id:"full-phase".into(),identity,catalog,projected_records:1,shared:false};
            let provider=Arc::new(FakeProvider::new(false));let repository=fake::repository();let key=[21;32];
            let mut source=crate::persistent_store::PersistentStore::open(root.path()).unwrap();
            let sections=super::super::sections::capture_backup_sections(&mut source,&root.path().join("sections"),&Cancellation::default()).unwrap();
            let mut metadata=metadata("full-phase",&capture);
            let original_units=assets.iter().enumerate().map(|(index,asset)|(
                risunest_sync_wire::unit::UnitKey::new(&["future-opaque",&format!("full-body-{index}")]).unwrap(),
                risunest_sync_wire::unit::UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content(asset.content_hash.clone())).unwrap(),
            )).collect::<BTreeMap<_,_>>().into();
            metadata.purpose=SnapshotPurpose::BackupBundle{source:wire_control::BundleSource::Device{writer_id:"synthetic-writer".into()},remote_generation:None,original_units};
            let mut transfer=journal(&root.path().join("full-phase-upload"),"full-phase",&capture);
            let completed=package_and_upload(capture,sections.clone(),root.path(),&root.path().join("full-phase-cache"),metadata,&key,limits(256*1024),None,&mut transfer,provider.as_ref(),&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let connected=super::super::connection_commands::ConnectedRepository {
                stored:super::super::connection_store::StoredConnection {
                    id:"synthetic-connection".into(),config:super::super::contract::ConnectionConfig {provider:"synthetic".into(),profile:None,endpoint:"https://synthetic.invalid".into(),account_id:"account".into(),location:BTreeMap::new(),oauth_profile:None},
                    descriptor:risunest_external_storage_format::format::Descriptor::new("format-repository".into(),Some(risunest_external_storage_format::format::Strategy::Cas)).unwrap(),
                    descriptor_locator:RemoteLocator{connection_identity:repository.connection_identity.clone(),collection:None,object:"descriptor".into()},provider_repository_id:repository.repository_id.clone(),credential_ref:"credential".into(),root_key_ref:"key".into(),recovery_key_ref:"recovery".into(),retention_policy:None,transfer_concurrency:None,labels:Default::default(),capabilities:fake::capabilities(true),created_at_ms:1,verified_at_ms:1,last_sync_at_ms:None,last_backup_at_ms:None,
                },provider:provider.clone(),handle:repository,dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new(key),
            };
            let connected=Arc::new(connected);
            for already_present in [false,true] {
                let destination=tempfile::tempdir().unwrap();
                let mut store=crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
                let local=PayloadCas::new(destination.path()).unwrap();
                for bytes in &asset_bytes[2..] {local.prepare_bytes(bytes).unwrap();}
                if already_present {for bytes in &asset_bytes[..2] {local.prepare_bytes(bytes).unwrap();}}
                let request=serde_json::from_value(serde_json::json!({"connectionId":"synthetic-connection","kind":"restore","snapshotId":"full-phase","targetRevision":"0"})).unwrap();
                let mut job=DurableJob::new(request,1,store.external_identity().unwrap());
                job.summary["restoreSource"]=serde_json::to_value(completed.reference.stored(&connected.handle).unwrap()).unwrap();
                let jobs=JobStore::open(destination.path()).unwrap();jobs.put(&job).unwrap();
                crate::persistent_store::hash_work::reset_hash_work();
                let (prepared,received_sections)=runtime_restore::prepare_database_first_backup(destination.path(),&connected,&job,&Cancellation::default()).await.unwrap();
                let preparation_hashes=crate::persistent_store::hash_work::take_hash_work();
                assert!(preparation_hashes.incomplete.is_empty());
                assert!(preparation_hashes.domains["native_external_bundle_fingerprint"].bytes>0);
                assert_eq!(prepared.present.contains(&hash),already_present);
                jobs.freeze_restore_bodies(&job,&prepared.sources,&prepared.present).unwrap();
                let pack_ids=prepared.sources.iter().flat_map(|source|source.chunks.iter().map(|chunk|chunk.pack_id.clone())).collect::<BTreeSet<_>>();
                assert_eq!(pack_ids.len(),1);
                let first=&prepared.sources[0];
                assert!(prepared.sources.iter().all(|source|std::ptr::eq(&*source.catalog,&*first.catalog)&&std::ptr::eq(&*source.packs[0],&*first.packs[0])));
                if !already_present {
                    let connection=connected.clone();let external_source=prepared.sources[0].clone();
                    spawn_blocking(move || {
                        use crate::server_sync::lww_tests::{LocalServerFixture,put_asset,drain_publications};
                        let mixed=tempfile::tempdir().unwrap();let mut mixed_store=PersistentStore::open(mixed.path()).unwrap();
                        super::super::connection_store::ConnectionStore::open(mixed.path()).unwrap().insert(&connection.stored).unwrap();
                        let _connection=super::super::lww_residency::install_test_source_connection(mixed.path(),connection.clone()).unwrap();
                        let mut external_source=external_source;external_source.connection_root=mixed.path().to_owned();
                        super::super::lww_residency::register_packed_many(mixed.path(),std::slice::from_ref(&external_source),&connection.handle).unwrap();
                        let server=LocalServerFixture::new();let core=server.client(&mixed_store);
                        let server_hash=put_asset(&mut mixed_store,"assets/background-server.bin",&vec![71;100_000]).object_hash.unwrap();
                        drain_publications(&core,&mut mixed_store,&[]).unwrap();
                        mixed_store.asset_residency_set_policy(crate::server_sync::residency::AssetPolicy::Remote,||Ok(())).unwrap();
                        mixed_store.asset_residency_evict(||Ok(())).unwrap();
                        let cas=PayloadCas::new(mixed.path()).unwrap();assert!(cas.stat_object(&server_hash).unwrap().is_none());
                        let mut session=crate::server_sync::residency::HydrationSession::new(mixed.path(),None).unwrap();
                        let priority=BTreeSet::from([external_source.hash.clone()]);let mut completed=Vec::new();
                        assert!(session.hydrate_many_outcomes_prioritized(&[server_hash.clone(),external_source.hash.clone()],&priority,&||Ok(()),|hash,outcome| {
                            if hash==external_source.hash {assert!(cas.stat_object(&server_hash).unwrap().is_none());}
                            assert_eq!(outcome,crate::server_sync::residency::HydrationOutcome::Downloaded);
                            completed.push(hash.to_owned());
                        }).unwrap().is_empty());
                        assert_eq!(completed,[external_source.hash,server_hash]);
                    }).await.unwrap();
                }
                let bootstrap=tempfile::tempdir().unwrap();
                let bootstrap_store=crate::persistent_store::PersistentStore::open(bootstrap.path()).unwrap();
                super::super::connection_store::ConnectionStore::open(bootstrap.path()).unwrap().insert(&connected.stored).unwrap();
                let _connection=super::super::lww_residency::install_test_source_connection(bootstrap.path(),connected.clone()).unwrap();
                let bootstrap_cas=PayloadCas::new(bootstrap.path()).unwrap();
                bootstrap_cas.prepare_bytes(&asset_bytes[2]).unwrap();
                if already_present {for bytes in &asset_bytes {bootstrap_cas.prepare_bytes(bytes).unwrap();}}
                for source in &prepared.sources {
                    let mut source=source.clone();source.connection_root=bootstrap.path().to_owned();
                    super::super::lww_residency::register_packed_many(bootstrap.path(),std::slice::from_ref(&source),&connected.handle).unwrap();
                }
                crate::persistent_store::register_asset_objects_at_root(bootstrap.path(),&assets.iter().map(|asset|crate::persistent_store::asset_object_catalog::AssetObjectRegistration{object_hash:asset.content_hash.clone(),byte_size:asset.byte_size}).collect::<Vec<_>>(),1).unwrap();
                let bootstrap_pack=prepared.sources[0].packs[0].locator.object.clone();
                let refusal_provider=provider.clone();let refusal_pack=bootstrap_pack.clone();let refusal_hash=prepared.sources[0].hash.clone();
                crate::asset_repository::body_io::reset_body_io();
                for asset in &assets {crate::asset_repository::body_io::register_object_purpose(&asset.content_hash,crate::asset_repository::body_io::BodyPurpose::Asset);}
                let bootstrap_reads=spawn_blocking(move || {
                    if !already_present {
                        let before=refusal_provider.read_attempts(&refusal_pack);
                        let refused=bootstrap_store.hydrate_registered_remote_assets_prioritized(None,None,|| {
                            if refusal_provider.read_attempts(&refusal_pack)>before {Err(crate::server_sync::SyncError::new("stale-binding-authority",409))} else {Ok(())}
                        },||{}).unwrap_err();
                        assert_eq!(refused.code,"stale-binding-authority");
                        assert!(PayloadCas::new(bootstrap_store.repository_root()).unwrap().stat_object(&refusal_hash).unwrap().is_none());
                    }
                    let before=refusal_provider.read_attempts(&refusal_pack);
                    let listings=refusal_provider.listing_count();
                    bootstrap_store.hydrate_registered_remote_assets_prioritized(None,None,||Ok(()),||{}).unwrap();
                    assert!(refusal_provider.listing_count()-listings<8,"one protection check covers a pack group's bodies");
                    before
                }).await.unwrap();
                let bootstrap_bodies=crate::asset_repository::body_io::take_body_io();
                assert!(bootstrap_bodies.complete());
                if already_present {assert_eq!(bootstrap_bodies.asset_work(),Default::default());}
                else {assert!(bootstrap_bodies.asset_work().read_bytes>0);}
                assert_eq!(provider.read_attempts(&bootstrap_pack)-bootstrap_reads,usize::from(!already_present));
                let pack=prepared.sources.iter().flat_map(|source|source.packs.iter()).find(|pack|pack_ids.contains(&pack.header.object_id)).unwrap().clone();
                let reads_before=provider.read_attempts(&pack.locator.object);
                let mut different=prepared.sources.clone();different[0].protected_snapshot="different-published-source".into();
                assert!(jobs.freeze_restore_bodies(&job,&different,&prepared.present).is_err());
                jobs.freeze_restore_bodies(&job,&prepared.sources,&BTreeSet::from([hash.clone()])).unwrap();
                let admission=Arc::new(crate::native_file_jobs::admission::Admission::default());
                let state=crate::persistent_store::commands::PersistentStoreState::default();
                crate::asset_repository::body_io::reset_body_io();
                for asset in &assets {crate::asset_repository::body_io::register_object_purpose(&asset.content_hash,crate::asset_repository::body_io::BodyPurpose::Asset);}
                let (receipt,permit)=runtime_restore::activate_database_first_backup(&mut store,&state,&job,prepared,received_sections,Cancellation::default(),admission.staging().unwrap()).unwrap();
                let activation=crate::asset_repository::body_io::take_body_io();
                assert!(activation.complete());assert_eq!(activation.asset_work(),Default::default());
                assert!(admission.file(true).is_err());
                assert_eq!(store.revision().unwrap().to_string(),receipt["receivedRevision"].as_str().unwrap());
                assert_eq!(PayloadCas::new(destination.path()).unwrap().stat_object(&hash).unwrap().is_some(),already_present);
                assert_ne!(jobs.read(&job.id).unwrap().summary["restoreBodiesComplete"],true);
                crate::asset_repository::body_io::reset_body_io();crate::external_storage::worker_observation::begin();
                for asset in &assets {crate::asset_repository::body_io::register_object_purpose(&asset.content_hash,crate::asset_repository::body_io::BodyPurpose::Asset);}
                let adoption=runtime_restore::RestoreAdoptionRequest{job_id:job.id.clone(),received_revision:receipt["receivedRevision"].as_str().unwrap().into(),selected_character_id:None};
                let mut permit=Some(permit);
                let invalid_adoption=runtime_restore::RestoreAdoptionRequest{received_revision:"999".into(),..adoption.clone()};
                let refused_store=crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
                assert!(runtime_restore::settle_database_first_backup(refused_store,&connected,&job,invalid_adoption,&mut permit,&Cancellation::default()).await.is_err());
                assert!(permit.is_some());assert!(admission.file(true).is_err());
                let cancelled=Cancellation::default();cancelled.cancel();
                assert_eq!(runtime_restore::settle_database_first_backup(store,&connected,&job,adoption.clone(),&mut permit,&cancelled).await.unwrap_err().kind,ErrorKind::Cancelled);
                assert!(admission.file(true).is_ok());
                let store=crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
                assert_eq!(store.revision().unwrap().to_string(),receipt["receivedRevision"].as_str().unwrap());
                runtime_restore::settle_database_first_backup(store,&connected,&job,adoption,&mut Some(admission.file(true).unwrap()),&Cancellation::default()).await.unwrap();
                let workers=crate::external_storage::worker_observation::take();let bodies=crate::asset_repository::body_io::take_body_io();
                assert!(bodies.complete());assert!(workers.iter().all(|worker|worker.completed));
                if already_present {assert_eq!(bodies.asset_work(),Default::default());} else {assert!(bodies.asset_work().read_bytes>0);}
                assert!(admission.file(true).is_ok());assert_eq!(jobs.read(&job.id).unwrap().summary["restoreBodiesComplete"],true);
                assert!(jobs.restore_bodies_settled(&job.id).unwrap());
                assert_eq!(provider.read_attempts(&pack.locator.object)-reads_before,usize::from(!already_present));
                for asset in &assets {assert!(PayloadCas::new(destination.path()).unwrap().stat_object(&asset.content_hash).unwrap().is_some());}
            }
        });
    }

    #[test]
    fn asset_only_publication_uses_existing_authenticated_pack_catalog() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();
            let body=b"synthetic segment managed body";
            let (capture,hash)=captured(root.path(),"segment-assets",0,b"{}",body);
            let provider=FakeProvider::new(false);
            let repository=fake::repository();
            let key=[17;32];
            let mut transfer=journal(&root.path().join("asset-journal"),"segment-assets",&capture);
            let completed=package_and_upload_asset_catalog(vec![AssetCatalogSource{content_hash:hash.clone(),byte_length:body.len() as u64,source:ObjectSource::Library(hash.clone())}],root.path(),&root.path().join("asset-cache"),&key,limits(256*1024),&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default(),None).await.unwrap();
            assert_eq!(completed.catalog.header.role,wire::ObjectRole::Catalog);
            assert_eq!(completed.catalog.header.repository_id,repository.repository_id);
            assert!(completed.referenced.iter().any(|object|object.role==ObjectRole::Pack));
            let stage=root.path().join("asset-reader");
            let catalog=RemoteObject::from_stored(&completed.catalog,&repository).unwrap();
            let (entries,packs,_)=snapshot_restore::read_catalog(&catalog,wire::CatalogKind::Assets,&key,&stage,&provider,&repository,&Cancellation::default()).await.unwrap();
            assert_eq!(entries.len(),1);
            assert_eq!(hex::encode(entries[0].content_sha256),hash);
            let entry=entries.into_iter().next().unwrap();
            let decoded=snapshot_restore::download_packed_body(&hash,entry.byte_length,entry.chunks,packs.values().map(|pack|pack.stored(&repository).unwrap()).collect(),&stage,&key,&provider,&repository,&Cancellation::default()).await.unwrap();
            assert_eq!(decoded,body);
            let pack=packs.values().next().unwrap();let name=&pack.receipt.locator.object;
            let reads=provider.read_attempts(name);
            assert_eq!(snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Records,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap_err().kind,ErrorKind::Corrupt);
            assert_eq!(provider.read_attempts(name),reads,"wrong-kind metadata refuses before Asset pack reads");
            snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Assets,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap();
            assert!(provider.read_attempts(name)>reads,"aged prepared Asset roots authenticate their pack closure");
            let uploads=provider.upload_attempts(&pack.object_id);provider.forget(name);
            assert_eq!(snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Assets,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap_err().kind,ErrorKind::NotFound);
            assert_eq!(provider.upload_attempts(&pack.object_id),uploads,"aged prepared verification never repairs a saved Asset pack");
        });
    }

    #[test]
    fn control_catalog_carries_a_verified_indivisible_message_page() {
        for repository_id in super::super::lww_tests::repository_id_cases() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();
            let page=risunest_external_storage_format::logical_records::encode_message_page(&[
                serde_json::json!({"role":"user","content":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)})]).unwrap();
            let source=root.path().join("frozen-page");std::fs::write(&source,&page.bytes).unwrap();
            let (capture,_)=captured(root.path(),"segment-controls",0,b"{}",b"unrelated asset");
            let provider=FakeProvider::new(false);let mut repository=fake::repository();repository.repository_id=repository_id.clone();let key=[17;32];
            let mut transfer=TransferJournal::open(&root.path().join("data-journal"),JobIdentity {
                job_id:"segment-controls".into(),connection_id:"connection".into(),repository_id:repository_id.clone(),
                capture_id:capture.id.clone(),capture:capture.identity.clone(),
            }).unwrap();
            let completed=package_and_upload_control_catalog(vec![AssetCatalogSource{content_hash:page.hash.clone(),byte_length:page.size,source:ObjectSource::File(source)}],
                root.path(),&root.path().join("data-cache"),&key,limits(256*1024),&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default(),None).await.unwrap();
            let mut store=crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
            let hashes=snapshot_restore::admit_data_catalogs(&[completed.catalog.clone()],&mut store,"synthetic-controls",&key,None,&provider,&repository,&Cancellation::default()).await.unwrap();
            assert_eq!(hashes,vec![page.hash.clone()]);assert_eq!(store.lww_object_body(&page.hash).unwrap().unwrap(),page.bytes);
            let reads=provider.read_count();
            assert_eq!(snapshot_restore::admit_data_catalogs(&[completed.catalog.clone()],&mut store,"synthetic-controls",&key,None,&provider,&repository,&Cancellation::default()).await.unwrap(),hashes);
            assert_eq!(provider.read_count(),reads,"an exact verified root with local controls needs no remote reads");
            let pack_reads=completed.referenced.iter().filter(|object|object.role==ObjectRole::Pack).map(|object|
                (object.receipt.locator.object.clone(),provider.read_attempts(&object.receipt.locator.object))).collect::<BTreeMap<_,_>>();
            assert_eq!(snapshot_restore::admit_data_catalogs(&[completed.catalog.clone()],&mut store,"fresh-catalog-target",&key,None,&provider,&repository,&Cancellation::default()).await.unwrap(),hashes);
            assert!(provider.read_count()>reads,"an unknown root authenticates its metadata");
            for (name,reads) in pack_reads {assert_eq!(provider.read_attempts(&name),reads,"known controls are removed before pack turnover");}
            let remote=RemoteObject::from_stored(&completed.catalog,&repository).unwrap();
            assert!(snapshot_restore::read_catalog(&remote,wire::CatalogKind::Assets,&key,&root.path().join("wrong-kind"),&provider,&repository,&Cancellation::default()).await.is_err());
            let mut foreign=fake::repository();foreign.repository_id=format!("{repository_id}other");
            assert_eq!(snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Records,&key,&provider,&foreign,root.path(),&Cancellation::default()).await.unwrap_err().kind,ErrorKind::Corrupt);
            let packs=completed.referenced.iter().filter(|object|object.role==ObjectRole::Pack).collect::<Vec<_>>();
            let reads=packs.iter().map(|pack|provider.read_attempts(&pack.receipt.locator.object)).collect::<Vec<_>>();
            snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Records,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap();
            for (pack,reads) in packs.iter().zip(reads) {assert!(provider.read_attempts(&pack.receipt.locator.object)>reads,"aged proof authenticates every required pack despite local controls");}
            let pack=packs[0];let name=&pack.receipt.locator.object;
            let uploads=provider.upload_attempts(&pack.object_id);
            let mut damaged=provider.state.lock().unwrap().objects[name].0.clone();let last=damaged.len()-1;damaged[last]^=1;
            provider.seed(name,ObjectRole::Pack,damaged);
            assert_eq!(snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Records,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap_err().kind,ErrorKind::Corrupt);
            provider.forget(name);
            assert_eq!(snapshot_restore::revalidate_catalog(&completed.catalog,wire::CatalogKind::Records,&key,&provider,&repository,root.path(),&Cancellation::default()).await.unwrap_err().kind,ErrorKind::NotFound);
            assert_eq!(provider.upload_attempts(&pack.object_id),uploads,"aged immutable verification never repairs or replaces the captured source");
        });
        }
    }

    #[test]
    fn large_archive_and_manifest_controls_restore_through_data_catalogs() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();
            let (_,bodies,large)=crate::persistent_store::external_capture::large_metadata_fixture();
            let mut sources=Vec::new();
            for hash in &large {
                let path=root.path().join(hash);fs::write(&path,&bodies[hash]).unwrap();
                sources.push(AssetCatalogSource {content_hash:hash.clone(),byte_length:bodies[hash].len() as u64,source:ObjectSource::File(path)});
            }
            let (capture,_)=captured(root.path(),"large-controls",0,b"{}",b"asset");
            let provider=FakeProvider::new(false);let repository=fake::repository();let key=[17;32];
            let mut transfer=journal(&root.path().join("journal"),"large-controls",&capture);
            let completed=package_and_upload_control_catalog(sources,root.path(),&root.path().join("cache"),&key,limits(256*1024),&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default(),None).await.unwrap();
            let mut store=crate::persistent_store::PersistentStore::open(destination.path()).unwrap();
            let joined_cache=destination.path().join("joined-cache");
            snapshot_restore::admit_data_catalogs(&[completed.catalog],&mut store,"large-metadata",&key,Some(&joined_cache),&provider,&repository,&Cancellation::default()).await.unwrap();
            for hash in &large {assert_eq!(store.lww_object_body(hash).unwrap().unwrap(),bodies[hash]);}
            let before=provider.uploaded_ids().into_iter().filter(|id|id.starts_with("pack-")).collect::<Vec<_>>();
            let sources=large.iter().map(|hash|AssetCatalogSource {content_hash:hash.clone(),byte_length:bodies[hash].len() as u64,source:ObjectSource::File(root.path().join(hash))}).collect();
            let mut next=journal(&destination.path().join("journal"),"joined-first-backup",&capture);
            let reused=package_and_upload_control_catalog(sources,destination.path(),&joined_cache,&key,limits(256*1024),&mut next,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default(),None).await.unwrap();
            assert_eq!(before,provider.uploaded_ids().into_iter().filter(|id|id.starts_with("pack-")).collect::<Vec<_>>());
            assert!(reused.referenced.iter().any(|object|object.role==ObjectRole::Pack));
        });
    }

    #[test]
    fn existing_manifest_control_is_prepared_from_data_pack_independently_of_local_asset_residency() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();
            let external=root.path().join("external-storage");
            let mut catalog=crate::external_storage::capture::CaptureCatalog::create(&external.join("captures/control"),&external,None).unwrap();
            let identity=CaptureIdentity {store_id:"store".into(),library_epoch:"epoch".into(),generation:"generation".into(),selection_epoch:"selection".into(),revision:1};
            let manifest=risunest_external_storage_format::message_pages::MessageManifest {schema:risunest_external_storage_format::message_pages::MANIFEST_SCHEMA.into(),message_count:0.into(),pages:Vec::new()}.encode().unwrap();
            catalog.begin(&identity,None).unwrap();
            catalog.record("root",b"synthetic root record").unwrap();
            catalog.object(&manifest.hash,&manifest.bytes).unwrap();
            catalog.reference("root",&manifest.hash,manifest.bytes.len() as u64).unwrap();
            catalog.finish().unwrap();
            let capture=CapturedSnapshot {id:"control".into(),identity,catalog,projected_records:1,shared:false};
            let provider=FakeProvider::new(false);
            let repository=fake::repository();
            let key=[11;32];
            let snapshot_metadata=metadata("existing-control",&capture);
            let mut transfer=journal(&root.path().join("upload"),"control",&capture);
            let completed=package_and_upload(capture,Vec::new(),root.path(),&root.path().join("cache"),snapshot_metadata,&key,limits(256 * 1024),None,&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let destination=tempfile::tempdir().unwrap();
            PayloadCas::new(destination.path()).unwrap().prepare_bytes(&manifest.bytes).unwrap();
            let stage=root.path().join("download-control");
            crate::asset_repository::body_io::reset_body_io();
            crate::external_storage::worker_observation::begin();
            let prepared=snapshot_restore::download_snapshot(&completed.reference,&stage,&key,None,snapshot_restore::SourceTrust::ProvenLibrary(destination.path()),&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
            let receipts=crate::external_storage::worker_observation::take();
            let observed=crate::asset_repository::body_io::take_body_io();
            assert!(!receipts.is_empty());
            assert!(receipts.iter().all(|receipt| receipt.completed && receipt.hashes.incomplete.is_empty()));
            assert!(receipts.iter().any(|receipt| receipt.hashes.domains.get("external_content_put_verify").is_some_and(|work| work.calls>0 && work.bytes>0)));
            assert_eq!(receipts.len() as u64,observed.worker_scopes_started);
            assert_eq!(observed.worker_scopes_started,observed.worker_scopes_settled);
            assert!(observed.complete());
            assert_eq!(observed.asset_work(),Default::default());
            let object=prepared.objects.iter().find(|object| object.content_hash == manifest.hash).unwrap();
            assert!(!matches!(object.source,crate::external_storage::content_store::ObjectSource::Library(_)));
            let content=crate::external_storage::content_store::ContentStore::open(&stage.join("external-storage")).unwrap();
            let mut body=content.open_source(&object.source).unwrap();
            let mut bytes=Vec::new();
            std::io::Read::read_to_end(&mut body,&mut bytes).unwrap();
            assert_eq!(bytes,manifest.bytes);
        });
    }

    #[test]
    fn actual_pack_hash_observations_include_streamed_inputs_and_failed_prefixes() {
        crate::persistent_store::hash_work::reset_hash_work();
        let bytes=b"synthetic hashing input";
        let expected=hash(bytes);
        assert_eq!(hash_reader(&mut bytes.as_slice(),bytes.len() as u64).unwrap(),expected);
        let fingerprint=catalog_fingerprint_of(wire::CatalogKind::Assets,std::iter::once(("asset",bytes.len() as u64,"hash")));
        assert_eq!(fingerprint.len(),64);
        let observed=crate::persistent_store::hash_work::take_hash_work();
        assert!(observed.incomplete.is_empty());
        assert_eq!(observed.domains["native_external_content_identity"].bytes,bytes.len() as u64);
        assert_eq!(observed.domains["native_external_content_reader"].bytes,bytes.len() as u64);
        assert!(observed.domains["native_external_catalog_fingerprint"].bytes>0);
        crate::persistent_store::hash_work::reset_hash_work();
        assert!(hash_reader(&mut bytes.as_slice(),1).is_err());
        let failed=crate::persistent_store::hash_work::take_hash_work();
        assert!(failed.incomplete.contains_key("native_external_content_reader"));
        assert_eq!(failed.domains["native_external_content_reader"].bytes,0);
    }

    #[test]
    fn independent_full_backups_reuse_unchanged_asset_pack_without_body_retransmission() {
        runtime().block_on(async {
            let root=tempfile::tempdir().unwrap();let cache=root.path().join("cache");
            let provider=FakeProvider::new(false);let repository=fake::repository();let key=[91;32];
            let mut store=PersistentStore::open(root.path()).unwrap();
            let sections=super::super::sections::capture_backup_sections(&mut store,&root.path().join("sections"),&Cancellation::default()).unwrap();
            assert_eq!(sections.len(),3);
            let mut previous:Option<CompletedSnapshot>=None;
            for (index,id) in ["independent-first","independent-second"].into_iter().enumerate() {
                let (capture,_)=captured(root.path(),id,index as i64,b"synthetic unchanged full record",b"synthetic unchanged managed asset");
                let mut meta=metadata(id,&capture);
                meta.purpose=SnapshotPurpose::BackupBundle{source:wire_control::BundleSource::Device{writer_id:"synthetic-writer".into()},remote_generation:None,original_units:BTreeMap::new().into()};
                let asset_packs=previous.as_ref().map(|previous| {
                    let identity=ObjectEvidence::identity(&previous.asset_catalog,&repository).unwrap();
                    PackageCache::open(&cache).unwrap().graph_of(&previous.repository_id,&repository,&identity).unwrap().into_iter().filter(|object|object.role==ObjectRole::Pack).collect::<Vec<_>>()
                }).unwrap_or_default();
                let before=asset_packs.iter().map(|object|(object.receipt.locator.object.clone(),provider.state.lock().unwrap().body_bytes.get(&object.receipt.locator.object).copied().unwrap_or(0))).collect::<BTreeMap<_,_>>();
                let uploads=asset_packs.iter().map(|object|provider.upload_attempts(&object.object_id)).collect::<Vec<_>>();
                let mut transfer=journal(&root.path().join(format!("{id}-job")),id,&capture);
                let completed=package_and_upload(capture,sections.clone(),root.path(),&cache,meta,&key,limits(256*1024),None,&mut transfer,&provider,&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
                assert_eq!(completed.sections.len(),3);
                if let Some(previous)=&previous {
                    assert_ne!(completed.reference.object_id,previous.reference.object_id);
                    assert_eq!(completed.asset_catalog.object_id,previous.asset_catalog.object_id);
                    assert!(!asset_packs.is_empty());
                    for (index,object) in asset_packs.iter().enumerate() {
                        assert_eq!(provider.state.lock().unwrap().body_bytes.get(&object.receipt.locator.object).copied().unwrap_or(0),before[&object.receipt.locator.object]);
                        assert_eq!(provider.upload_attempts(&object.object_id),uploads[index]);
                        assert!(completed.referenced_objects.iter().any(|value|value.object_id==object.object_id));
                    }
                }
                previous=Some(completed);
            }
        });
    }

    /// Invariant 32. Two devices produce separate bundles. Neither carries the
    /// other's section material and nothing is merged between them.
    #[test]
    fn two_devices_keep_separate_backup_bundles() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let spool = root.path().join("section-spool");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [11; 32];
            let bundle = |id: &'static str, writer: &'static str, marker: &'static str| {
                let cache = cache.clone();
                let spool = spool.clone();
                let provider = &provider;
                let repository = &repository;
                let root = root.path().to_path_buf();
                async move {
                    let (capture, _) =
                        captured(&root, &format!("capture-{id}"), 1, b"record", b"asset");
                    let mut snapshot_metadata = metadata(id, &capture);
                    snapshot_metadata.purpose = SnapshotPurpose::BackupBundle {
                        source: wire_control::BundleSource::Device {
                            writer_id: writer.into(),
                        },
                        remote_generation: None,
                        original_units: BTreeMap::from([(
                            risunest_sync_wire::unit::UnitKey::new(&["future-backup", "synthetic"]).unwrap(),
                            risunest_sync_wire::unit::UnitValue::inline(&serde_json::to_vec(marker).unwrap()).unwrap(),
                        )]).into(),
                    };
                    let mut transfer = journal(&root.join(id), id, &capture);
                    package_and_upload(
                        capture,
                        vec![section(&spool, SectionKind::LocalPlugins, marker, 0)],
                        &root,
                        &cache,
                        snapshot_metadata,
                        &key,
                        limits(256 * 1024),
                        None,
                        &mut transfer,
                        provider,
                        repository,
                        &PhaseProgress::silent(),
                        &Cancellation::default(),
                    )
                    .await
                    .unwrap()
                }
            };
            let mine = bundle("bundle-1", "writer-a", "mine").await;
            let theirs = bundle("bundle-2", "writer-b", "theirs").await;
            assert_eq!(mine.sections.keys().collect::<Vec<_>>(), ["local-plugins"]);
            assert_eq!(theirs.sections.keys().collect::<Vec<_>>(), ["local-plugins"]);
            assert_ne!(
                mine.sections["local-plugins"].content_fingerprint,
                theirs.sections["local-plugins"].content_fingerprint
            );
            assert_ne!(mine.fingerprint, theirs.fingerprint);

            for (completed, writer, marker) in
                [(&mine, "writer-a", "mine"), (&theirs, "writer-b", "theirs")]
            {
                let document = snapshot_restore::download_snapshot(
                    &completed.reference,
                    &root.path().join(format!("read-{marker}")),
                    &key,
                    None,
                    snapshot_restore::SourceTrust::Downloaded,
                    &provider,
                    &repository,
                    &crate::external_storage::phase_progress::PhaseProgress::silent(),
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                assert_eq!(document.captured_by_device.as_deref(), Some(writer));
                let stage = root.path().join(format!("original-{marker}"));
                fs::create_dir_all(&stage).unwrap();
                let path = snapshot_restore::open_object(&completed.reference,&key,&stage,&provider,&repository,&Cancellation::default()).await.unwrap();
                let view = crate::external_storage::control::SnapshotView::read(
                    &snapshot_restore::read_bytes(&path,wire::MAX_METADATA_BYTES).unwrap(),
                    wire::ObjectRole::BackupBundle,&completed.reference.repository_id,
                ).unwrap();
                let original = RemoteObject::from_stored(view.original_units.as_ref().unwrap(),&repository).unwrap();
                let units = snapshot_restore::download_original_backup_units(&original,&stage,&key,&provider,&repository,&crate::external_storage::phase_progress::PhaseProgress::silent(),&Cancellation::default()).await.unwrap();
                let content = crate::external_storage::content_store::ContentStore::open(&stage.join("external-storage")).unwrap();
                let units = units.units(&content).map(|unit| unit.unwrap()).collect::<Vec<_>>();
                assert_eq!(units.len(),1);
                assert_eq!(units[0].1, risunest_sync_wire::unit::UnitValue::inline(&serde_json::to_vec(marker).unwrap()).unwrap());
                let received = snapshot_restore::download_sections(
                    &completed.reference,
                    &BTreeSet::from(["local-plugins".to_owned()]),
                    &root.path().join(format!("receive-{marker}")),
                    &key,
                    None,
                    &provider,
                    &repository,
                    &crate::external_storage::phase_progress::PhaseProgress::silent(),
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                let rows = materialized_section_rows(&received[0], false);
                assert_eq!(
                    section_values(rows),
                    section_values(section_rows(SectionKind::LocalPlugins, marker))
                );
            }
        });
    }

    /// Invariant 36. A cancelled capture stops where it is and publishes
    /// nothing, so no half-written section reaches a repository.
    #[test]
    fn a_cancelled_section_capture_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cancel = Cancellation::default();
        cancel.cancel();
        let spool = root.path().join("cancelled");
        assert!(crate::external_storage::sections::capture_section(
            SectionKind::Hypa,
            &section_rows(SectionKind::Hypa, "stopped"),
            true,
            risunest_sync_wire::head::Sequence::from(1u64),
            risunest_sync_wire::head::Sequence::from(0u64),
            risunest_sync_wire::head::Sequence::from(5u64),
            &spool,
            &cancel,
        )
        .is_err());
        assert!(!spool.exists());
    }

    #[test]
    fn latest_snapshot_restores_without_ancestors_and_text_change_reuses_assets() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("package-cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let asset_bytes = b"synthetic asset bytes";
            let (first_capture, asset_hash) =
                captured(root.path(), "capture-1", 1, b"record one", asset_bytes);
            let first_metadata = metadata("snapshot-1", &first_capture);
            let mut first_journal = journal(&root.path().join("job-1"), "job-1", &first_capture);
            let first = package_and_upload(
                first_capture,
                Vec::new(),
                root.path(),
                &cache,
                first_metadata,
                &key,
                limits(128 * 1024),
                None,
                &mut first_journal,
                &provider,
                &repository,
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let first_count = provider.state.lock().unwrap().objects.len();
            assert_eq!(first_count, 10);
            assert_eq!(provider.state.lock().unwrap().objects.keys()
                .filter(|id| id.starts_with("inventory-page-")).count(), 5);
            assert_ne!(first.repository_id, repository.repository_id);
            assert!(provider
                .state
                .lock()
                .unwrap()
                .objects
                .keys()
                .all(|object_id| !object_id.contains(&asset_hash)));
            let snapshot_version = provider
                .state
                .lock()
                .unwrap()
                .objects
                .get(&first.reference.receipt.locator.object)
                .unwrap()
                .1;
            assert_eq!(snapshot_version, first_count as u64);

            let (second_capture, _) =
                captured(root.path(), "capture-2", 2, b"record two", asset_bytes);
            let second_metadata = metadata("snapshot-2", &second_capture);
            let mut second_journal = journal(&root.path().join("job-2"), "job-2", &second_capture);
            let second = package_and_upload(
                second_capture,
                Vec::new(),
                root.path(),
                &cache,
                second_metadata,
                &key,
                limits(128 * 1024),
                None,
                &mut second_journal,
                &provider,
                &repository,
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                second.asset_catalog.object_id,
                first.asset_catalog.object_id
            );
            assert_eq!(
                provider.state.lock().unwrap().objects.len() - first_count,
                6
            );

            let staging = root.path().join("restore");
            assert!(snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &[8; 32],
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .is_err());
            let restored = snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &key,
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(restored.snapshot_id, "snapshot-2");
            assert_eq!(restored.records.len(), 1);
            assert_eq!(restored.record_body(0), b"record two");
            let asset = restored
                .objects
                .iter()
                .find(|object| object.content_hash == asset_hash)
                .unwrap();
            assert_eq!(fs::read(asset.source.file().unwrap()).unwrap(), asset_bytes);
            let pack = second
                .referenced_objects
                .iter()
                .find(|object| object.role == ObjectRole::Pack)
                .unwrap();
            let pack_plaintext = staging.join("plaintext").join(&pack.plaintext_sha256);
            let pack_ciphertext = staging
                .join("downloads")
                .join(format!("{}.cipher", pack.ciphertext_sha256));
            fs::write(&pack_plaintext, vec![0; pack.plaintext_length as usize]).unwrap();
            fs::write(&pack_ciphertext, vec![0; pack.receipt.byte_length as usize]).unwrap();
            let pack_reads = provider.read_attempts(&pack.receipt.locator.object);
            restored.discard_record_bodies();
            for object in &restored.objects {
                fs::remove_file(object.source.file().unwrap()).unwrap();
            }
            let repaired = snapshot_restore::download_snapshot(
                &second.reference,
                &staging,
                &key,
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(repaired.record_body(0), b"record two");
            assert_eq!(provider.read_attempts(&pack.receipt.locator.object), pack_reads + 1);
            // The damaged copies were replaced by a fetched pack, which was
            // placed and released like any other.
            assert!(!pack_plaintext.exists());
            assert!(!pack_ciphertext.exists());
        });
    }

    #[test]
    fn c_cached_asset_tree_is_rebuilt_when_a_pack_or_catalog_disappears() {
        runtime().block_on(async {
            for missing_role in [ObjectRole::Pack, ObjectRole::Catalog] {
                let root = tempfile::tempdir().unwrap();
                let cache = root.path().join("cache");
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [7; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
                let meta = metadata("first", &capture);
                let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
                let first = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(128 * 1024), None, &mut first_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
                let bytes = provider.state.lock().unwrap().objects[&first.asset_catalog.receipt.locator.object].0.clone();
                let metadata_key = derive_key(&key, &first.repository_id, "metadata").unwrap();
                let mut plaintext = Vec::new();
                wire::open_envelope(&mut std::io::Cursor::new(bytes), &mut plaintext, &metadata_key,
                    wire::MAX_METADATA_BYTES as u64).unwrap();
                let catalog = wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES).unwrap();
                let asset_pack = &catalog.packs[0];
                let missing = if missing_role == ObjectRole::Pack {
                    asset_pack.locator.object.clone()
                } else { first.asset_catalog.receipt.locator.object.clone() };
                provider.forget(&missing);
                let retained = first.referenced_objects.iter().find(|object| {
                    object.role == ObjectRole::Pack && object.object_id != asset_pack.header.object_id
                }).unwrap();
                let (capture, _) = captured(root.path(), "second", 2, b"record", b"asset");
                let meta = metadata("second", &capture);
                let mut second_journal = journal(&root.path().join("second-job"), "second-job", &capture);
                let second = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(128 * 1024), None, &mut second_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
                let restored = snapshot_restore::download_snapshot(&second.reference, &root.path().join("restored"),
                    &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
                assert_eq!(restored.record_body(0), b"record");
                assert!(restored.objects.iter().any(|object| fs::read(object.source.file().unwrap()).unwrap() == b"asset"));
                assert_eq!(provider.upload_attempts(&retained.object_id), 1);
                assert!(!first_journal.spool_path(&first.reference.object_id).exists());
                assert!(!second_journal.spool_path(&second.reference.object_id).exists());
                if missing_role == ObjectRole::Pack {
                    assert_ne!(first.asset_catalog.object_id, second.asset_catalog.object_id);
                }
            }
        });
    }

    /// A received record is a body in the bounded content store beside the
    /// staging root, not a plaintext file of its own that the apply then has
    /// to copy into another store. The staging directory holds a file per
    /// asset and none per record.
    #[test]
    fn c_a_receive_holds_records_in_its_store_and_not_in_staging_files() {
        runtime().block_on(async {
            const RECORDS: usize = 24;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [41; 32];
            let cancel = Cancellation::default();
            let cas = PayloadCas::new(root.path()).unwrap();
            let asset = cas.prepare_bytes(b"asset").unwrap();
            let external = root.path().join("external-storage");
            let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
                &external.join("captures").join("capture"), &external, None).unwrap();
            let identity = CaptureIdentity {
                store_id: "store".into(),
                library_epoch: "epoch".into(),
                generation: "generation".into(),
                selection_epoch: "selection".into(),
                revision: 1,
            };
            catalog.begin(&identity, None).unwrap();
            for index in 0..RECORDS {
                catalog.record(&format!("record-{index:02}"), format!("body {index}").as_bytes()).unwrap();
            }
            catalog.reference("record-00", &asset.content_hash, asset.byte_size).unwrap();
            catalog.finish().unwrap();
            let capture = CapturedSnapshot {
                id: "capture".into(),
                identity,
                catalog,
                projected_records: RECORDS,
                shared: false,
            };
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(128 * 1024), None, &mut journal, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();
            let staging = root.path().join("restored");
            let restored = snapshot_restore::download_snapshot(&completed.reference, &staging, &key,
                None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();

            assert_eq!(restored.records.len(), RECORDS);
            assert!(!staging.join("records").exists());
            let bodies = restored.records.iter().map(|record| {
                assert!(matches!(record.source, ObjectSource::Captured(_)), "{}", record.key);
                (record.key.clone(), restored.body(&record.source))
            }).collect::<BTreeMap<_, _>>();
            for index in 0..RECORDS {
                assert_eq!(bodies[&format!("record-{index:02}")], format!("body {index}").into_bytes());
            }
            assert_eq!(restored.objects.len(), 1);
            assert_eq!(restored.body(&restored.objects[0].source), b"asset");
            assert_eq!(fs::read_dir(staging.join("objects")).unwrap().count(), 1);
        });
    }

    /// Section 11.3 on the repository the aged measurement described: once a
    /// long rotation of edits has left more than the trigger's worth of small
    /// live record packs, the next publication that was going to run anyway
    /// also repackages a bounded run of them. Placement changes; the library
    /// does not, and nothing old is deleted.
    #[test]
    fn c_an_aged_publication_repackages_a_bounded_run_of_small_live_packs() {
        let packs = coalesce_packs_on_this_thread(8, 4);
        runtime().block_on(async {
            const RECORDS: usize = 32;
            const ASSETS: usize = 8;
            const AGED: i64 = 10;
            let mut outcomes = Vec::new();
            for allowed in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [47; 32];
                let cache = root.path().join("cache");
                let mut latest = None;
                let mut before_final = BTreeSet::new();
                for revision in 1..=AGED + 1 {
                    if revision > AGED {
                        before_final = provider.state.lock().unwrap()
                            .objects.keys().cloned().collect();
                    }
                    let step = (revision - 1) as usize;
                    let capture = captured_record_window(
                        root.path(), &format!("coalesce-capture-{revision}"), revision,
                        RECORDS, 2, step * 2, ASSETS);
                    let meta = metadata(&format!("coalesce-snapshot-{revision}"), &capture);
                    let job = format!("coalesce-job-{revision}");
                    let mut transfer = journal(&root.path().join(&job), &job, &capture);
                    let parent = latest.as_ref()
                        .map(|previous| parent_graph(previous, &repository))
                        .transpose().unwrap();
                    // Only the last publication is offered the maintenance
                    // portion, so the aging itself is identical in both runs.
                    let limits = limits(128 * 1024).with_maintenance(allowed && revision > AGED);
                    latest = Some(package_and_upload(
                        capture, Vec::new(), root.path(), &cache, meta, &key, limits,
                        parent.as_ref(), &mut transfer, &provider, &repository,
                        &PhaseProgress::silent(), &Cancellation::default(),
                    ).await.unwrap());
                }
                let completed = latest.unwrap();
                let restore_root = root.path().join("coalesce-restore");
                snapshot_restore::reset_test_read_counts(&restore_root);
                let restored = snapshot_restore::download_snapshot(
                    &completed.reference, &restore_root, &key, None,
                    snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                    &crate::external_storage::phase_progress::PhaseProgress::silent(),
                    &Cancellation::default(),
                ).await.unwrap();
                let reads = snapshot_restore::take_test_read_counts(&restore_root);
                let bodies = restored.records.iter()
                    .map(|record| (record.key.clone(), restored.body(&record.source)))
                    .collect::<BTreeMap<_, _>>();
                let after_final = provider.state.lock().unwrap()
                    .objects.keys().cloned().collect::<BTreeSet<_>>();
                // Repackaging adds; it never deletes what older snapshots name.
                assert!(before_final.is_subset(&after_final), "an object went missing");
                outcomes.push((completed.maintenance, completed.library_fingerprint.clone(),
                    reads, bodies, after_final.len() - before_final.len()));
            }
            let (plain, coalesced) = (&outcomes[0], &outcomes[1]);
            assert_eq!(plain.0, MaintenanceReport::default());
            assert_eq!(coalesced.0.packs, packs.max as u64);
            assert!(coalesced.0.source_bytes > 0);
            assert!(coalesced.0.source_bytes <= COALESCE_MAX_SOURCE_BYTES);
            // The library is the same library. Only where its bodies sit moved.
            assert_eq!(plain.1, coalesced.1);
            assert_eq!(plain.3.len(), RECORDS);
            assert_eq!(plain.3, coalesced.3);
            // The drained packs are packs the latest state no longer opens.
            assert_eq!(
                plain.2.packs - coalesced.2.packs,
                packs.max as u64,
                "plain={:?} coalesced={:?}", plain.2, coalesced.2,
            );
            // The repackaged bodies fit the pack this publication was writing
            // anyway, so the cycle costs no object the plain run did not also
            // upload. What it buys is the drained packs above.
            assert_eq!(coalesced.4, plain.4);
        });
    }

    /// Section 11.3's leaf bound, and a publication whose record catalog is
    /// logically its parent's. The library grows one contiguous range per
    /// publication, so every small pack covers about one catalog leaf and an
    /// adjacent run of ten covers more than eight. The last
    /// publication changes only a device section, and what the maintenance
    /// portion moves has to be what the published catalog names.
    #[test]
    fn c_a_coalescing_cycle_rewrites_at_most_eight_leaves_and_names_what_it_moved() {
        coalesce_packs_on_this_thread(8, COALESCE_MAX_PACKS);
        runtime().block_on(async {
            const RANGE: usize = 48;
            const GROWN: usize = 10;
            const LAST: i64 = GROWN as i64 + 1;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [53; 32];
            let cache = root.path().join("cache");
            let spool = root.path().join("sections");
            let mut previous: Option<CompletedSnapshot> = None;
            let mut latest: Option<CompletedSnapshot> = None;
            let mut before_final = BTreeSet::new();
            for revision in 1..=LAST {
                let last = revision == LAST;
                if last {
                    before_final = provider.state.lock().unwrap()
                        .objects.keys().cloned().collect();
                }
                let grown = (revision as usize).min(GROWN);
                let capture = captured_records(
                    root.path(), &format!("leaf-capture-{revision}"), grown as i64,
                    0..grown * RANGE, 0..0);
                let meta = metadata(&format!("leaf-snapshot-{revision}"), &capture);
                let job = format!("leaf-job-{revision}");
                let mut transfer = journal(&root.path().join(&job), &job, &capture);
                let parent = latest.as_ref()
                    .map(|completed| parent_graph(completed, &repository))
                    .transpose().unwrap();
                let sections = if last {
                    vec![section(&spool, SectionKind::Hypa, "changed", 1)]
                } else {
                    Vec::new()
                };
                let completed = package_and_upload(
                    capture, sections, root.path(), &cache, meta, &key,
                    limits(16 * 1024).with_maintenance(last),
                    parent.as_ref(), &mut transfer, &provider, &repository,
                    &PhaseProgress::silent(), &Cancellation::default(),
                ).await.unwrap();
                previous = latest.replace(completed);
            }
            let (previous, completed) = (previous.unwrap(), latest.unwrap());
            assert_eq!(previous.library_fingerprint, completed.library_fingerprint);

            // What the cycle drained is what the new record catalog stopped
            // naming, and nothing it reports is anything else.
            assert!(completed.maintenance.packs > 0, "{:?}", completed.maintenance);
            assert_ne!(
                completed.record_catalog.object_id, previous.record_catalog.object_id,
                "the published record catalog ignored the placements this cycle moved",
            );
            let before = record_graph_packs(&cache, &previous, &repository);
            let after = record_graph_packs(&cache, &completed, &repository);
            assert_eq!(
                before.difference(&after).count() as u64,
                completed.maintenance.packs,
                "{:?}", completed.maintenance,
            );

            // Only the leaves the moved keys sat in were written again.
            let metadata_key = derive_key(&key, &completed.repository_id, "metadata").unwrap();
            let written_leaves = {
                let state = provider.state.lock().unwrap();
                state.objects.iter()
                    .filter(|(id, _)| id.starts_with("catalog-") && !before_final.contains(*id))
                    .filter(|(_, (bytes, _))| {
                        let mut plaintext = Vec::new();
                        wire::open_envelope(
                            &mut std::io::Cursor::new(bytes.clone()), &mut plaintext,
                            &metadata_key, wire::MAX_METADATA_BYTES as u64,
                        ).unwrap();
                        let document =
                            wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES)
                                .unwrap();
                        document.kind == wire::CatalogKind::Records && document.level == 0
                    })
                    .count()
            };
            assert!(
                written_leaves >= 1 && written_leaves as u64 <= completed.maintenance.leaves
                    && completed.maintenance.leaves <= COALESCE_MAX_LEAVES as u64,
                "written_leaves={written_leaves} {:?}", completed.maintenance,
            );

            let restore_root = root.path().join("leaf-restore");
            let restored = snapshot_restore::download_snapshot(
                &completed.reference, &restore_root, &key, None,
                snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            ).await.unwrap();
            assert_eq!(restored.records.len(), GROWN * RANGE);
            for record in &restored.records {
                let index: usize = record.key.trim_start_matches("record/").parse().unwrap();
                assert_eq!(
                    restored.body(&record.source),
                    format!("{{\"record\":{index},\"value\":0}}").into_bytes(),
                );
            }
        });
    }

    /// Keys the library deleted keep their cache rows. The packs only those
    /// rows name are not live and take no place in the cycle's run.
    #[test]
    fn c_deleted_keys_take_no_place_in_a_coalescing_cycle() {
        let packs = coalesce_packs_on_this_thread(8, 4);
        runtime().block_on(async {
            const RECORDS: usize = 40;
            const AGED: i64 = 14;
            const DELETED: usize = 8;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [59; 32];
            let cache = root.path().join("cache");
            let mut previous: Option<CompletedSnapshot> = None;
            let mut latest: Option<CompletedSnapshot> = None;
            for revision in 1..=AGED + 1 {
                let last = revision > AGED;
                let window = (revision as usize - 1) * 2;
                // Each publication edits the next two keys and puts the two it
                // edited before back, so the pack it writes keeps those two
                // live. The last one deletes the first keys.
                let keys = if last { DELETED..RECORDS } else { 0..RECORDS };
                let capture = captured_records(
                    root.path(), &format!("stale-capture-{revision}"), revision,
                    keys, window..window + 2);
                let meta = metadata(&format!("stale-snapshot-{revision}"), &capture);
                let job = format!("stale-job-{revision}");
                let mut transfer = journal(&root.path().join(&job), &job, &capture);
                let parent = latest.as_ref()
                    .map(|completed| parent_graph(completed, &repository))
                    .transpose().unwrap();
                let completed = package_and_upload(
                    capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(128 * 1024).with_maintenance(last),
                    parent.as_ref(), &mut transfer, &provider, &repository,
                    &PhaseProgress::silent(), &Cancellation::default(),
                ).await.unwrap();
                previous = latest.replace(completed);
            }
            let (previous, completed) = (previous.unwrap(), latest.unwrap());
            assert_eq!(completed.maintenance.packs, packs.max as u64);
            let before = record_graph_packs(&cache, &previous, &repository);
            let after = record_graph_packs(&cache, &completed, &repository);
            // The packs of the deleted keys leave with the keys, and the run
            // drains a full cap of live ones besides.
            assert_eq!(
                before.difference(&after).count(),
                packs.max + DELETED / 2,
            );
        });
    }

    /// Keys of uneven length, so leaves fill unevenly.
    fn variable_key(index: usize) -> String {
        format!("record/{index:06}{}", "~".repeat(index % 7))
    }

    fn library_body(index: usize, value: i64) -> Vec<u8> {
        format!("{{\"record\":{index},\"value\":{value}}}").into_bytes()
    }

    /// A capture holding the records at `keys` under `variable_key`, each at
    /// the value `value` gives it.
    fn captured_library(
        root: &Path,
        capture_id: &str,
        revision: i64,
        keys: std::ops::Range<usize>,
        value: &dyn Fn(usize) -> i64,
    ) -> CapturedSnapshot {
        let external = root.join("external-storage");
        let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
            &external.join("captures").join(capture_id),
            &external,
            None,
        )
        .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: format!("generation-{revision}"),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        let objects = external.join("objects");
        {
            let mut insert = catalog
                .db
                .prepare("INSERT INTO records VALUES(?1,?2,?3)")
                .unwrap();
            for index in keys {
                let body = library_body(index, value(index));
                let digest = hex::encode(hash(&body));
                let path = objects.join(&digest);
                if !path.exists() {
                    fs::write(path, &body).unwrap();
                }
                insert
                    .execute(params![variable_key(index), digest, body.len() as i64])
                    .unwrap();
            }
        }
        catalog.finish().unwrap();
        CapturedSnapshot {
            id: capture_id.into(),
            identity,
            catalog,
            projected_records: 0,
            shared: false,
        }
    }

    /// The leaves of a published record catalog in key order, each with the
    /// object holding it, walked down from the root the provider stores.
    fn record_leaves(
        provider: &FakeProvider,
        key: &[u8; 32],
        completed: &CompletedSnapshot,
    ) -> Vec<(String, wire::CatalogDocument)> {
        let metadata_key = derive_key(key, &completed.repository_id, "metadata").unwrap();
        let state = provider.state.lock().unwrap();
        let open = |object: &str| {
            let mut plaintext = Vec::new();
            wire::open_envelope(
                &mut std::io::Cursor::new(state.objects[object].0.clone()), &mut plaintext,
                &metadata_key, wire::MAX_METADATA_BYTES as u64,
            ).unwrap();
            wire::CatalogDocument::decode(&plaintext, wire::MAX_METADATA_BYTES).unwrap()
        };
        let mut pending = vec![completed.record_catalog.receipt.locator.object.clone()];
        let mut leaves = Vec::new();
        while let Some(object) = pending.pop() {
            let document = open(&object);
            if document.level == 0 {
                leaves.push((object, document));
            } else {
                pending.extend(
                    document.children.iter().rev().map(|child| child.object.locator.object.clone()),
                );
            }
        }
        leaves
    }

    /// Where each key's chunks sit in a published record catalog.
    fn record_placements(
        leaves: &[(String, wire::CatalogDocument)],
    ) -> BTreeMap<String, BTreeSet<String>> {
        let mut placements: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (_, leaf) in leaves {
            for entry in &leaf.entries {
                placements.entry(entry.key.clone()).or_default()
                    .extend(entry.chunks.iter().map(|chunk| chunk.pack_id.clone()));
            }
        }
        placements
    }

    /// A library that grew by `range` keys a publication for `grown`
    /// publications under `history`, each writing its keys into one pack, and
    /// then one more publication under `last` of every key at the value
    /// `value` gives it, with a device section changed when `section` says so.
    /// Returns the last two publications and what the provider held before
    /// the last one.
    #[allow(clippy::too_many_arguments)]
    async fn aged_library(
        root: &Path,
        provider: &FakeProvider,
        key: &[u8; 32],
        range: usize,
        grown: usize,
        history: PackageLimits,
        last: PackageLimits,
        section: bool,
        value: &dyn Fn(usize) -> i64,
    ) -> (CompletedSnapshot, CompletedSnapshot, BTreeSet<String>) {
        let repository = fake::repository();
        let cache = root.join("cache");
        let spool = root.join("sections");
        let mut previous: Option<CompletedSnapshot> = None;
        let mut latest: Option<CompletedSnapshot> = None;
        let mut before = BTreeSet::new();
        for revision in 1..=grown as i64 + 1 {
            let final_one = revision as usize > grown;
            if final_one {
                before = provider.state.lock().unwrap().objects.keys().cloned().collect();
            }
            let size = (revision as usize).min(grown) * range;
            let capture = if final_one {
                captured_library(root, &format!("aged-capture-{revision}"), revision, 0..size, value)
            } else {
                captured_library(root, &format!("aged-capture-{revision}"), revision, 0..size, &|_| 0)
            };
            let meta = metadata(&format!("aged-snapshot-{revision}"), &capture);
            let job = format!("aged-job-{revision}");
            let mut transfer = journal(&root.join(&job), &job, &capture);
            let parent = latest.as_ref()
                .map(|completed| parent_graph(completed, &repository))
                .transpose().unwrap();
            let sections = if final_one && section {
                vec![section_of(&spool)]
            } else {
                Vec::new()
            };
            let completed = package_and_upload(
                capture, sections, root, &cache, meta, key,
                if final_one { last } else { history },
                parent.as_ref(), &mut transfer, provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            ).await.unwrap();
            previous = latest.replace(completed);
        }
        (previous.unwrap(), latest.unwrap(), before)
    }

    fn section_of(spool: &Path) -> CapturedSection {
        section(spool, SectionKind::Hypa, "changed", 1)
    }

    /// Restores `completed` and compares every body with the value `value`
    /// gives its key.
    async fn assert_restores(
        root: &Path,
        provider: &FakeProvider,
        key: &[u8; 32],
        completed: &CompletedSnapshot,
        records: usize,
        value: &dyn Fn(usize) -> i64,
    ) {
        let restore_root = root.join(format!("restore-{}", completed.snapshot_id));
        let restored = snapshot_restore::download_snapshot(
            &completed.reference, &restore_root, key, None,
            snapshot_restore::SourceTrust::Downloaded, provider, &fake::repository(),
            &PhaseProgress::silent(), &Cancellation::default(),
        ).await.unwrap();
        let actual = restored.records.iter()
            .map(|record| (record.key.clone(), restored.body(&record.source)))
            .collect::<BTreeMap<_, _>>();
        let expected = (0..records)
            .map(|index| (variable_key(index), library_body(index, value(index))))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, expected);
    }

    /// The bound is on the leaves a cycle's rewrite produces, not on the
    /// leaves its keys sat in before. This cycle writes smaller packs than the
    /// history did, so a leaf it rewrites names more packs than it did and no
    /// longer fits. A run whose keys sat in at most eight previous leaves then
    /// splits them into more than eight, and the run is cut to what the
    /// catalog writer's own layout keeps within the bound.
    #[test]
    fn c_a_coalescing_cycle_is_bounded_by_the_leaves_its_rewrite_produces() {
        coalesce_packs_on_this_thread(8, COALESCE_MAX_PACKS);
        runtime().block_on(async {
            const RANGE: usize = 48;
            const GROWN: usize = 10;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [61; 32];
            let mut last = limits(16 * 1024).with_maintenance(true);
            last.target_plaintext_bytes = 128;
            let (previous, completed, before) = aged_library(
                root.path(), &provider, &key, RANGE, GROWN, limits(16 * 1024), last, true, &|_| 0,
            ).await;
            assert_eq!(previous.library_fingerprint, completed.library_fingerprint);
            assert!(completed.maintenance.packs > 0, "{:?}", completed.maintenance);
            assert!(completed.maintenance.packs <= COALESCE_MAX_PACKS as u64);
            assert!(completed.maintenance.source_bytes <= COALESCE_MAX_SOURCE_BYTES);
            let written = record_leaves(&provider, &key, &completed).into_iter()
                .filter(|(object, _)| !before.contains(object))
                .count();
            // The record catalog is otherwise its parent's, so every leaf
            // written is the cycle's, and its plan charged it at least that.
            assert!(
                written >= 1 && written as u64 <= completed.maintenance.leaves
                    && completed.maintenance.leaves <= COALESCE_MAX_LEAVES as u64,
                "written={written} {:?}", completed.maintenance,
            );
            assert_eq!(completed.maintenance.required_leaves, 0);
            // The packs the cycle drained are the ones the catalog stopped
            // naming, and every pack it names instead is one it wrote.
            let cache = root.path().join("cache");
            let packs_before = record_graph_packs(&cache, &previous, &repository);
            let packs_after = record_graph_packs(&cache, &completed, &repository);
            assert_eq!(
                packs_before.difference(&packs_after).count() as u64,
                completed.maintenance.packs,
            );
            assert!(packs_after.difference(&packs_before).all(|pack| !before.contains(pack)));
            assert_restores(root.path(), &provider, &key, &completed, RANGE * GROWN, &|_| 0).await;
        });
    }

    /// A publication with nothing to publish, whose every candidate run would
    /// split more leaves than a cycle may produce, writes nothing at all: the
    /// run is declined before any pack is built.
    #[test]
    fn c_a_no_op_publication_whose_every_run_exceeds_the_bound_writes_nothing() {
        coalesce_packs_on_this_thread(8, COALESCE_MAX_PACKS);
        runtime().block_on(async {
            const RANGE: usize = 150;
            const GROWN: usize = 9;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let key = [67; 32];
            let mut last = limits(16 * 1024).with_maintenance(true);
            last.target_plaintext_bytes = 128;
            let (previous, completed, before) = aged_library(
                root.path(), &provider, &key, RANGE, GROWN, limits(16 * 1024), last, false, &|_| 0,
            ).await;
            let written = provider.state.lock().unwrap().objects.keys()
                .filter(|object| !before.contains(*object))
                .filter(|object| object.starts_with("pack-") || object.starts_with("catalog-"))
                .cloned()
                .collect::<Vec<_>>();
            assert!(written.is_empty(), "wrote {} objects: {:?}", written.len(), completed.maintenance);
            assert_eq!(completed.maintenance, MaintenanceReport::default());
            assert_eq!(completed.record_catalog, previous.record_catalog);
            assert_eq!(completed.asset_catalog, previous.asset_catalog);
            assert_restores(root.path(), &provider, &key, &completed, RANGE * GROWN, &|_| 0).await;
        });
    }

    /// Required edits reach far more leaves than a cycle may add, and every
    /// one of them is published. What the cycle adds on top of them stays
    /// within the bound: a leaf counts when it holds a key the cycle moved, or
    /// when the publication without the cycle would not have produced it.
    #[test]
    fn c_required_edits_beyond_the_bound_are_all_published_beside_a_bounded_cycle() {
        coalesce_packs_on_this_thread(8, COALESCE_MAX_PACKS);
        runtime().block_on(async {
            const RANGE: usize = 48;
            const GROWN: usize = 14;
            const EVERY: usize = 24;
            let edited = |index: usize| index % EVERY == 5;
            let value = |index: usize| if edited(index) { GROWN as i64 + 1 } else { 0 };
            let mut runs = Vec::new();
            for allowed in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let key = [71; 32];
                let (previous, completed, before) = aged_library(
                    root.path(), &provider, &key, RANGE, GROWN, limits(16 * 1024),
                    limits(16 * 1024).with_maintenance(allowed), false, &value,
                ).await;
                assert_restores(root.path(), &provider, &key, &completed, RANGE * GROWN, &value)
                    .await;
                let parent = record_leaves(&provider, &key, &previous);
                let leaves = record_leaves(&provider, &key, &completed);
                let written = leaves.iter().filter(|(object, _)| !before.contains(object)).count();
                runs.push((completed, parent, leaves, written, root, provider));
            }
            let (plain, coalesced) = (&runs[0], &runs[1]);
            assert_eq!(plain.0.maintenance, MaintenanceReport::default());
            // Without the cycle, the edits alone rewrite more leaves than a
            // cycle may.
            assert!(plain.3 > COALESCE_MAX_LEAVES, "plain wrote {}", plain.3);
            assert!(coalesced.0.maintenance.packs > 0, "{:?}", coalesced.0.maintenance);
            assert!(coalesced.0.maintenance.packs <= COALESCE_MAX_PACKS as u64);
            assert!(coalesced.0.maintenance.source_bytes <= COALESCE_MAX_SOURCE_BYTES);
            let before = record_placements(&coalesced.1);
            let after = record_placements(&coalesced.2);
            let moved = after.iter()
                .filter(|(key, packs)| {
                    let index: usize = key.trim_start_matches("record/")[..6].parse().unwrap();
                    !edited(index) && before.get(*key) != Some(*packs)
                })
                .map(|(key, _)| key.clone())
                .collect::<BTreeSet<_>>();
            assert!(!moved.is_empty());
            let required = plain.2.iter()
                .map(|(_, leaf)| (leaf.first_key.clone(), leaf.last_key.clone()))
                .collect::<BTreeSet<_>>();
            let added = coalesced.2.iter()
                .filter(|(_, leaf)| {
                    leaf.entries.iter().any(|entry| moved.contains(&entry.key))
                        || !required.contains(&(leaf.first_key.clone(), leaf.last_key.clone()))
                })
                .count();
            assert!(
                added as u64 <= coalesced.0.maintenance.leaves
                    && coalesced.0.maintenance.leaves <= COALESCE_MAX_LEAVES as u64,
                "added={added} {:?}", coalesced.0.maintenance,
            );
            assert!(
                coalesced.0.maintenance.required_leaves > COALESCE_MAX_LEAVES as u64,
                "{:?}", coalesced.0.maintenance,
            );
        });
    }

    /// A restore whose staging directory already answers for every entry opens
    /// no pack. The packs it would have opened are gone from the staging
    /// directory, so opening one would cost a download, and it costs none.
    #[test]
    fn c_a_restore_opens_no_pack_for_entries_it_already_holds() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [23; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(128 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap();
            let restore_root = root.path().join("restored");
            let first = snapshot_restore::download_snapshot(&completed.reference, &restore_root,
                &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
            let packs = completed.referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack).collect::<Vec<_>>();
            assert!(!packs.is_empty());
            for pack in &packs {
                assert_eq!(provider.read_attempts(&pack.receipt.locator.object), 1);
            }

            // Everything a pack was decrypted into is discarded, so a pack that
            // is still wanted has to be fetched again.
            fs::remove_dir_all(restore_root.join("plaintext")).unwrap();
            fs::remove_dir_all(restore_root.join("downloads")).unwrap();
            let second = snapshot_restore::download_snapshot(&completed.reference, &restore_root,
                &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
            for pack in &packs {
                assert_eq!(provider.read_attempts(&pack.receipt.locator.object), 1);
            }
            assert_eq!(second.records.len(), first.records.len());
            assert_eq!(second.objects.len(), first.objects.len());
            assert_eq!(second.record_body(0), b"record");
            assert!(second.objects.iter()
                .any(|object| fs::read(object.source.file().unwrap()).unwrap() == b"asset"));
        });
    }

    /// A normal receive whose library already holds an asset does not fetch the
    /// pack that carries it. The record changed, so its pack is read; the asset
    /// did not, and the repository that receives it already has the body.
    #[test]
    fn c_a_receive_reuses_an_asset_its_library_already_holds() {
        runtime().block_on(async {
            let sender = tempfile::tempdir().unwrap();
            let receiver = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [29; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(sender.path(), "first", 1, b"record", b"asset");
            let meta = metadata("first", &capture);
            let mut first_journal = journal(&sender.path().join("first-job"), "first-job", &capture);
            let first = package_and_upload(capture, vec![], sender.path(),
                &sender.path().join("cache"), meta, &key, limits(128 * 1024), None,
                &mut first_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            // The receiving library already holds the asset and has never seen
            // the record, which is what a normal changed cycle looks like.
            let held = PayloadCas::new(receiver.path()).unwrap().prepare_bytes(b"asset").unwrap();
            let asset = asset_pack(&provider, &first, &key);

            let (capture, _) = captured(sender.path(), "second", 2, b"record-v2", b"asset");
            let meta = metadata("second", &capture);
            let mut second_journal =
                journal(&sender.path().join("second-job"), "second-job", &capture);
            let second = package_and_upload(capture, vec![], sender.path(),
                &sender.path().join("cache"), meta, &key, limits(128 * 1024), None,
                &mut second_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let published_reads = provider.read_attempts(&asset.receipt.locator.object);
            let received = snapshot_restore::download_snapshot(&second.reference,
                &receiver.path().join("receive"), &key, None,
                snapshot_restore::SourceTrust::AdmittedLibrary(receiver.path()),
                &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();

            // The asset pack is never read; the record had to come from its own.
            assert_eq!(provider.read_attempts(&asset.receipt.locator.object), published_reads);
            let object = received.objects.iter()
                .find(|object| object.content_hash == held.content_hash).unwrap();
            assert!(matches!(&object.source, ObjectSource::Library(digest) if *digest == held.content_hash));
            assert_eq!(received.records.len(), 1);
            assert_eq!(received.record_body(0), b"record-v2");

            // A restore proves everything it consumes, so the same snapshot read
            // under that policy fetches the asset pack after all.
            snapshot_restore::download_snapshot(&second.reference,
                &receiver.path().join("restore"), &key, None,
                snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel)
                .await.unwrap();
            assert_eq!(
                provider.read_attempts(&asset.receipt.locator.object),
                published_reads + 1,
            );
        });
    }

    /// R04. A receive reuses its verified record cache and reads only the
    /// packs of changed records. Reading it again does not fetch any pack.
    #[test]
    fn c_a_receive_reads_only_the_packs_of_records_its_cache_does_not_hold() {
        runtime().block_on(async {
            const RECORDS: usize = 400;
            let sender = tempfile::tempdir().unwrap();
            let receiver = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [31; 32];
            let cancel = Cancellation::default();
            let cache = sender.path().join("cache");
            let staging = receiver.path().join("receive");
            let capture = captured_record_window(sender.path(), "first", 1, RECORDS, 0, 0, 0);
            let meta = metadata("first", &capture);
            let mut first_journal = journal(&sender.path().join("first-job"), "first-job", &capture);
            let first = package_and_upload(capture, vec![], sender.path(), &cache, meta, &key,
                limits(4 * 1024), None, &mut first_journal, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();
            let base = snapshot_restore::download_snapshot(&first.reference,
                &staging, &key, None, snapshot_restore::SourceTrust::Downloaded,
                &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let base: BTreeMap<String, String> = base.records.iter()
                .map(|record| (record.key.clone(), record.content_hash.clone())).collect();

            let capture = captured_record_window(sender.path(), "second", 2, RECORDS, 1, 0, 0);
            let meta = metadata("second", &capture);
            let mut second_journal = journal(&sender.path().join("second-job"), "second-job", &capture);
            let parent = parent_graph(&first, &repository).unwrap();
            let second = package_and_upload(capture, vec![], sender.path(), &cache, meta, &key,
                limits(4 * 1024), Some(&parent), &mut second_journal, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();

            snapshot_restore::reset_test_read_counts(&staging);
            let received = snapshot_restore::download_snapshot(&second.reference, &staging, &key,
                None, snapshot_restore::SourceTrust::AdmittedLibrary(receiver.path()),
                &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let skipping = snapshot_restore::take_test_read_counts(&staging);
            assert_eq!(received.records.len(), RECORDS);
            let changed: Vec<_> = received.records.iter()
                .filter(|record| base.get(&record.key) != Some(&record.content_hash)).collect();
            assert_eq!(changed.len(), 1);
            assert_eq!(changed[0].key, "record/000000");
            for (index, record) in received.records.iter().enumerate() {
                assert_eq!(record.key, format!("record/{index:06}"));
                let value = if index == 0 { 2 } else { 0 };
                assert_eq!(received.body(&record.source),
                    format!("{{\"record\":{index},\"value\":{value}}}").as_bytes());
                assert_eq!(base.get(&record.key).is_some_and(|hash| *hash == record.content_hash),
                    index != 0, "{}", record.key);
            }
            assert_eq!(skipping.packs, 1);

            snapshot_restore::reset_test_read_counts(&staging);
            let repeated = snapshot_restore::download_snapshot(&second.reference, &staging, &key,
                None, snapshot_restore::SourceTrust::AdmittedLibrary(receiver.path()),
                &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            assert_eq!(snapshot_restore::take_test_read_counts(&staging).packs, 0);
            for (index, record) in repeated.records.iter().enumerate() {
                let value = if index == 0 { 2 } else { 0 };
                assert_eq!(repeated.body(&record.source),
                    format!("{{\"record\":{index},\"value\":{value}}}").as_bytes());
            }
            assert_eq!(repeated.records.len(), RECORDS);

            let full = tempfile::tempdir().unwrap();
            snapshot_restore::reset_test_read_counts(full.path());
            snapshot_restore::reset_test_turnover(full.path());
            snapshot_restore::download_snapshot(&second.reference, full.path(), &key, None,
                snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();
            let everything = snapshot_restore::take_test_read_counts(full.path());
            assert!(everything.packs > 2, "{everything:?}");
            assert!(skipping.packs < everything.packs,
                "skipping={skipping:?} everything={everything:?}");
            // Each pack is placed and released before the next one arrives.
            assert_eq!(snapshot_restore::take_test_turnover(full.path()),
                snapshot_restore::TestTurnover { ciphertexts: 1, plaintexts: 1 });
        });
    }

    /// R04. A cold restore holds one pack at a time, including for a record
    /// and an asset whose chunks lie in several packs each, and lands both
    /// byte for byte with nothing left to resume from.
    #[test]
    fn c_a_restore_holds_one_pack_at_a_time_and_assembles_spread_entries() {
        runtime().block_on(async {
            let noise = |length: usize, mut state: u64| -> Vec<u8> {
                (0..length).map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                }).collect()
            };
            let record = noise(400 * 1024, 0x9e37_79b9_7f4a_7c15);
            let asset = noise(600 * 1024, 0x2545_f491_4f6c_dd1d);
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [37; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "spread", 1, &record, &asset);
            let meta = metadata("spread", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(128 * 1024), None, &mut journal, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();
            let packs = completed.referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack).count() as u64;
            assert!(packs >= 8, "{packs}");

            let staging = root.path().join("restored");
            snapshot_restore::reset_test_read_counts(&staging);
            snapshot_restore::reset_test_turnover(&staging);
            let restored = snapshot_restore::download_snapshot(&completed.reference, &staging,
                &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                &PhaseProgress::silent(), &cancel).await.unwrap();
            assert_eq!(snapshot_restore::take_test_read_counts(&staging).packs, packs);
            assert_eq!(snapshot_restore::take_test_turnover(&staging),
                snapshot_restore::TestTurnover { ciphertexts: 1, plaintexts: 1 });
            assert_eq!(restored.record_body(0), record);
            let object = restored.objects.iter()
                .find(|object| object.content_hash == hex::encode(hash(&asset))).unwrap();
            assert_eq!(fs::read(object.source.file().unwrap()).unwrap(), asset);
            assert!(!staging.join("assembly").exists());
            assert!(!staging.join("turnover").exists());
        });
    }

    /// A record and an asset with the same bytes go to two places, the record
    /// store and the asset's file, whether those bytes lie in one pack or are
    /// assembled from several, and a restore interrupted between packs still
    /// fills both when it resumes.
    #[test]
    fn c_a_record_and_an_asset_with_the_same_bytes_both_arrive() {
        runtime().block_on(async {
            let noise = |length: usize, mut state: u64| -> Vec<u8> {
                (0..length).map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                }).collect()
            };
            for (bytes, interrupted) in [
                (b"one pack holds both".to_vec(), false),
                (noise(400 * 1024, 0x9e37_79b9_7f4a_7c15), false),
                (noise(400 * 1024, 0x2545_f491_4f6c_dd1d), true),
            ] {
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [37; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(root.path(), "shared", 1, &bytes, &bytes);
                let meta = metadata("shared", &capture);
                let mut journal = journal(&root.path().join("job"), "job", &capture);
                let completed = package_and_upload(capture, vec![], root.path(),
                    &root.path().join("cache"), meta, &key, limits(128 * 1024), None,
                    &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                    .await.unwrap();
                let staging = root.path().join("restored");
                if interrupted {
                    let packs = completed.referenced_objects.iter()
                        .filter(|object| object.role == ObjectRole::Pack)
                        .map(|object| object.receipt.locator.object.clone())
                        .collect::<Vec<_>>();
                    assert!(packs.len() >= 4, "{packs:?}");
                    provider.fail_read(&packs[packs.len() / 2], ErrorKind::Transient);
                    assert!(snapshot_restore::download_snapshot(&completed.reference, &staging,
                        &key, None, snapshot_restore::SourceTrust::Downloaded, &provider,
                        &repository, &PhaseProgress::silent(), &cancel).await.is_err());
                }
                let restored = snapshot_restore::download_snapshot(&completed.reference, &staging,
                    &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                    &PhaseProgress::silent(), &cancel).await.unwrap();
                assert_eq!(restored.record_body(0), bytes);
                let object = restored.objects.iter()
                    .find(|object| object.content_hash == hex::encode(hash(&bytes))).unwrap();
                assert_eq!(fs::read(object.source.file().unwrap()).unwrap(), bytes);
                assert!(!staging.join("assembly").exists());
            }
        });
    }

    /// A restore proves every body it consumes, and a body the repository
    /// already holds is proved by reading it rather than by fetching the pack
    /// that carries it. One that has been changed under its own name fails that
    /// proof, and the pack arrives instead.
    #[test]
    fn c_a_restore_proves_a_library_body_before_reusing_it() {
        runtime().block_on(async {
            for corrupt_held in [false, true] {
                let sender = tempfile::tempdir().unwrap();
                let receiver = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [37; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(sender.path(), "capture", 1, b"record", b"asset");
                let meta = metadata("snapshot", &capture);
                let mut journal = journal(&sender.path().join("job"), "job", &capture);
                let completed = package_and_upload(capture, vec![], sender.path(),
                    &sender.path().join("cache"), meta, &key, limits(128 * 1024), None,
                    &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
                let cas = PayloadCas::new(receiver.path()).unwrap();
                let held = cas.prepare_bytes(b"asset").unwrap();
                if corrupt_held {
                    // The same length under the same name, which is exactly what
                    // a length check cannot tell apart.
                    fs::write(cas.object_path(&held.content_hash).unwrap().unwrap(), b"asseT")
                        .unwrap();
                }
                let asset = asset_pack(&provider, &completed, &key);
                let before = provider.read_attempts(&asset.receipt.locator.object);
                let restored = snapshot_restore::download_snapshot(&completed.reference,
                    &receiver.path().join("restore"), &key, None,
                    snapshot_restore::SourceTrust::ProvenLibrary(receiver.path()),
                    &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
                let reads = provider.read_attempts(&asset.receipt.locator.object) - before;
                let object = restored.objects.iter()
                    .find(|object| object.content_hash == held.content_hash).unwrap();
                if corrupt_held {
                    assert_eq!(reads, 1);
                    assert_eq!(fs::read(object.source.file().unwrap()).unwrap(), b"asset");
                } else {
                    assert_eq!(reads, 0);
                    assert!(matches!(&object.source, ObjectSource::Library(_)));
                }
            }
        });
    }

    /// A library body whose length no longer matches what the catalog names is
    /// not a source. The publication it belongs to fetches the pack instead.
    #[test]
    fn c_a_truncated_library_body_is_not_reused() {
        runtime().block_on(async {
            let sender = tempfile::tempdir().unwrap();
            let receiver = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [31; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(sender.path(), "capture", 1, b"record", b"asset-body");
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&sender.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], sender.path(),
                &sender.path().join("cache"), meta, &key, limits(128 * 1024), None,
                &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let cas = PayloadCas::new(receiver.path()).unwrap();
            let held = cas.prepare_bytes(b"asset-body").unwrap();
            fs::write(cas.object_path(&held.content_hash).unwrap().unwrap(), b"asset").unwrap();
            let asset = asset_pack(&provider, &completed, &key);

            let received = snapshot_restore::download_snapshot(&completed.reference,
                &receiver.path().join("receive"), &key, None,
                snapshot_restore::SourceTrust::AdmittedLibrary(receiver.path()),
                &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
            assert_eq!(provider.read_attempts(&asset.receipt.locator.object), 1);
            let object = received.objects.iter()
                .find(|object| object.content_hash == held.content_hash).unwrap();
            assert_eq!(fs::read(object.source.file().unwrap()).unwrap(), b"asset-body");
        });
    }

    #[test]
    fn c_catalog_reuse_surfaces_authorization_errors_without_publishing() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
            let meta = metadata("first", &capture);
            let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
            let first = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut first_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            provider.fail_read(&first.asset_catalog.receipt.locator.object, ErrorKind::Unauthorized);
            let (capture, _) = captured(root.path(), "second", 2, b"record", b"asset");
            let meta = metadata("second", &capture);
            let mut second_journal = journal(&root.path().join("second-job"), "second-job", &capture);
            let error = package_and_upload(capture, Vec::new(), root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut second_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unauthorized);
            assert!(!provider.holds("snapshot-second"));
            assert!(provider.holds(&first.asset_catalog.object_id));
            assert!(!first_journal.spool_path(&first.reference.object_id).exists());
        });
    }

    /// Every source is rechecked before the root is published. A body the
    /// repository lost exists nowhere once its ciphertext is released, so this
    /// publication stops instead of publishing a root that names it.
    #[test]
    fn c_publication_rechecks_every_source_and_stops_at_a_body_it_cannot_reach() {
        runtime().block_on(async {
            for role in [ObjectRole::Pack, ObjectRole::Catalog, ObjectRole::SyncState] {
                let root = tempfile::tempdir().unwrap();
                let cache = root.path().join("cache");
                let directory = root.path().join("job");
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [7; 32];
                let cancel = Cancellation::default();
                let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
                let meta = metadata("snapshot", &capture);
                let identity = JobIdentity {
                    job_id: "job".into(), connection_id: "connection".into(),
                    repository_id: repository.repository_id.clone(), capture_id: capture.id.clone(),
                    capture: capture.identity.clone(),
                };
                let mut journal = TransferJournal::open(&directory, identity.clone()).unwrap();
                let completed = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                    limits(128 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
                let missing = std::iter::once(&completed.reference).chain(completed.referenced_objects.iter())
                    .find(|object| object.role == role).unwrap().clone();
                provider.forget(&missing.receipt.locator.object);
                // No in-memory receipt or protection survives reopening this journal.
                drop(journal);
                let mut journal = TransferJournal::open(&directory, identity).unwrap();
                let error = verify_publication(&completed, root.path(), &cache, &mut journal, &key,
                    &provider, &repository, &cancel).await.unwrap_err();
                assert_eq!(error.kind, ErrorKind::NotFound);
                let evidence = ObjectEvidence::open(root.path(), journal.connection_id()).unwrap();
                assert!(evidence.known(&ObjectEvidence::identity(&missing, &repository).unwrap()).unwrap());
                for object in std::iter::once(&completed.reference).chain(completed.referenced_objects.iter()) {
                    assert_eq!(provider.upload_attempts(&object.object_id), 1);
                    assert!(!journal.spool_path(&object.object_id).exists());
                }
            }
        });
    }


    /// A resumed publication rebuilds the same bodies and proves them against
    /// what it registered. Nothing it already released is fetched back.
    #[test]
    fn c_a_resumed_publication_proves_released_bodies_without_reading_them() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(128 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap();
            let uploads = provider.upload_count();
            let reads = provider.read_count();
            for object in std::iter::once(&completed.reference).chain(completed.referenced_objects.iter()) {
                assert!(!journal.spool_path(&object.object_id).exists());
            }

            // The package cache this job wrote is gone, so every body is built
            // again and answered for by its registration alone.
            let (capture, _) = captured(root.path(), "resumed-capture", 1, b"record", b"asset");
            let meta = metadata("snapshot", &capture);
            let resumed = package_and_upload(capture, vec![], root.path(), &root.path().join("rebuilt-cache"),
                meta, &key, limits(128 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap();
            assert_eq!(resumed.reference.object_id, completed.reference.object_id);
            assert_eq!(resumed.reference.receipt.locator, completed.reference.receipt.locator);
            assert_eq!(provider.upload_count(), uploads);
            // One confirming read for each body and each page that registered
            // one. Nothing is fetched to be rebuilt from.
            let pages = provider.state.lock().unwrap().objects.keys()
                .filter(|id| id.starts_with("inventory-page-")).count();
            assert_eq!(provider.read_count() - reads, resumed.referenced_objects.len() + 1 + pages);
            for object in std::iter::once(&resumed.reference).chain(resumed.referenced_objects.iter()) {
                assert_eq!(provider.read_attempts(&object.object_id), 1);
                assert_eq!(provider.upload_attempts(&object.object_id), 1);
            }
        });
    }
    /// A refused upload ends a publication that is still building packs. The
    /// producer is let go rather than left waiting for a handover, and the
    /// failure the caller sees is the one the provider gave.
    #[test]
    fn c_a_refused_upload_ends_a_publication_that_is_still_building_packs() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [13; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured_packed_assets(root.path(), "capture", 80, 4096);
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            // The refusal lands on the first body of the second pack wave,
            // while the producer still has packs nobody has taken.
            provider.fail_upload_number(9, ErrorKind::StorageFull);
            let error = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(64 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::StorageFull);
            let placed = stored_packs(&provider);
            assert_eq!(placed.len(), 3);
            for object in &placed {
                assert!(journal.record(object).unwrap().unwrap().released);
                assert!(!journal.spool_path(object).exists());
            }
        });
    }

    /// A source the producer cannot answer for ends the publication too. What
    /// the earlier waves already placed keeps its receipt and gives up its
    /// ciphertext, and the publication that follows proves those bodies from
    /// their registration instead of sending them again.
    #[test]
    fn c_a_source_that_fails_after_a_wave_keeps_what_the_wave_placed() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [17; 32];
            let cancel = Cancellation::default();
            let (capture, digests) = captured_packed_assets(root.path(), "capture", 80, 4096);
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            // The payload read last still has its recorded length, so the
            // publication only learns of it once every earlier pack is out.
            let cas = PayloadCas::new(root.path()).unwrap();
            let last = cas.object_path(digests.last().unwrap()).unwrap().unwrap();
            let original = fs::read(&last).unwrap();
            fs::write(&last, vec![9u8; original.len()]).unwrap();
            let error = package_and_upload(capture, vec![], root.path(), &root.path().join("cache"),
                meta, &key, limits(64 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Corrupt);
            // Record and asset work already settled before the source failure
            // remains durable regardless of how the ready packs formed waves.
            let placed = stored_packs(&provider);
            assert!(placed.len() >= 2, "no settled asset prefix before the source failure");
            for object in &placed {
                assert!(journal.record(object).unwrap().unwrap().released);
                assert!(!journal.spool_path(object).exists());
            }

            fs::write(&last, &original).unwrap();
            let (capture, _) = captured_packed_assets(root.path(), "retried-capture", 80, 4096);
            let meta = metadata("snapshot", &capture);
            let completed = package_and_upload(capture, vec![], root.path(),
                &root.path().join("retried-cache"), meta, &key, limits(64 * 1024), None,
                &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            for object in &placed {
                assert_eq!(provider.upload_attempts(object), 1);
                assert_eq!(provider.read_attempts(object), 1);
            }
            assert!(completed.referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack)
                .any(|object| placed.contains(&object.object_id)));
        });
    }

    /// The publication after that one rebuilds the body nobody holds any more
    /// and reuses everything the repository still answers for.
    #[test]
    fn c_the_publication_after_a_lost_body_rebuilds_only_that_body() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "capture", 1, b"record", b"asset");
            let meta = metadata("snapshot", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let missing = asset_pack(&provider, &completed, &key);
            let retained = completed.referenced_objects.iter().find(|object| {
                object.role == ObjectRole::Pack && object.object_id != missing.object_id
            }).unwrap().clone();
            provider.forget(&missing.receipt.locator.object);
            assert_eq!(verify_publication(&completed, root.path(), &cache, &mut journal, &key,
                &provider, &repository, &cancel).await.unwrap_err().kind, ErrorKind::NotFound);

            let (capture, _) = captured(root.path(), "replacement-capture", 2, b"record", b"asset");
            let meta = metadata("replacement-snapshot", &capture);
            let mut replacement_journal =
                self::journal(&root.path().join("replacement-job"), "replacement-job", &capture);
            let replacement = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut replacement_journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                .await.unwrap();
            assert_eq!(verify_publication(&replacement, root.path(), &cache, &mut replacement_journal,
                &key, &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
            // The lost body is never sent again; the replacement rebuilds it.
            assert_eq!(provider.upload_attempts(&missing.object_id), 1);
            let rebuilt = replacement.referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack)
                .filter(|object| !completed.referenced_objects.iter()
                    .any(|previous| previous.object_id == object.object_id))
                .collect::<Vec<_>>();
            assert_eq!(rebuilt.len(), 1);
            assert_eq!(provider.upload_attempts(&rebuilt[0].object_id), 1);
            // Everything the repository still holds is reused, not sent again.
            for object in completed.referenced_objects.iter() {
                assert_eq!(provider.upload_attempts(&object.object_id), 1);
            }
            assert!(replacement.referenced_objects.iter()
                .any(|object| object.object_id == retained.object_id));
            let restored = snapshot_restore::download_snapshot(&replacement.reference,
                &root.path().join("restored"), &key, None, snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel).await.unwrap();
            assert_eq!(restored.record_body(0), b"record");
            assert!(restored.objects.iter().any(|object| fs::read(object.source.file().unwrap()).unwrap() == b"asset"));
        });
    }
    /// A capture of one record that names these payloads by hash and length,
    /// whether or not the payload store holds their bodies.
    fn captured_payloads(
        root: &Path,
        capture_id: &str,
        revision: i64,
        record: &[u8],
        payloads: &[&[u8]],
    ) -> (CapturedSnapshot, Vec<String>) {
        let directory = root
            .join("external-storage")
            .join("captures")
            .join(capture_id);
        let external = root.join("external-storage");
        let mut catalog =
            crate::external_storage::capture::CaptureCatalog::create(&directory, &external, None)
                .unwrap();
        let identity = CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision,
        };
        catalog.begin(&identity, None).unwrap();
        catalog.record("root", record).unwrap();
        let mut digests = Vec::new();
        for payload in payloads {
            let digest = hex::encode(hash(payload));
            catalog.reference("root", &digest, payload.len() as u64).unwrap();
            digests.push(digest);
        }
        catalog.finish().unwrap();
        (
            CapturedSnapshot {
                id: capture_id.into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            },
            digests,
        )
    }

    /// Leaves a payload only in custody, as a remote residency policy does.
    fn release_to_custody(root: &Path, digest: &str, size: u64) {
        let cas = PayloadCas::new(root).unwrap();
        fs::remove_file(cas.object_path(digest).unwrap().unwrap()).unwrap();
        crate::server_sync::residency::test_remote::hold(root, &[(digest, size)]);
    }

    /// A published asset held only in custody, and the parent that holds it.
    async fn published_then_released(
        root: &Path,
        provider: &FakeProvider,
        repository: &RepositoryHandle,
        key: &[u8; 32],
    ) -> (CompletedSnapshot, String) {
        let (capture, asset) = captured(root, "capture-1", 1, b"record one", b"asset one");
        let meta = metadata("snapshot-1", &capture);
        let mut journal = journal(&root.join("job-1"), "job-1", &capture);
        let first = package_and_upload(capture, vec![], root, &root.join("cache"), meta, key,
            limits(128 * 1024), None, &mut journal, provider, repository, &PhaseProgress::silent(),
            &Cancellation::default()).await.unwrap();
        release_to_custody(root, &asset, b"asset one".len() as u64);
        (first, asset)
    }

    /// The next publication, which names the released asset and `added`.
    async fn publish_with(
        root: &Path,
        first: &CompletedSnapshot,
        added: &[&[u8]],
        provider: &FakeProvider,
        repository: &RepositoryHandle,
        key: &[u8; 32],
    ) -> Result<CompletedSnapshot> {
        let mut payloads: Vec<&[u8]> = vec![b"asset one"];
        payloads.extend_from_slice(added);
        let (capture, _) = captured_payloads(root, "capture-2", 2, b"record two", &payloads);
        let meta = metadata("snapshot-2", &capture);
        let mut journal = journal(&root.join("job-2"), "job-2", &capture);
        let parent = parent_graph(first, repository).unwrap();
        package_and_upload(capture, vec![], root, &root.join("cache"), meta, key,
            limits(128 * 1024), Some(&parent), &mut journal, provider, repository,
            &PhaseProgress::silent(), &Cancellation::default()).await
    }

    fn snapshots(provider: &FakeProvider) -> usize {
        provider.state.lock().unwrap().objects.keys()
            .filter(|id| id.starts_with("snapshot-")).count()
    }

    /// An asset the destination already holds is reused without a fetch, even
    /// when only custody holds its body on this device.
    #[test]
    fn c_a_custody_only_asset_the_parent_holds_is_not_fetched() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let (first, asset) =
                published_then_released(root.path(), &provider, &repository, &key).await;
            let second = publish_with(root.path(), &first, &[], &provider, &repository, &key)
                .await.unwrap();
            assert_eq!(second.hydration, SourceHydration::default());
            assert_eq!(crate::server_sync::residency::test_remote::fetched(root.path()), 0);
            assert_ne!(second.record_catalog.object_id, first.record_catalog.object_id);
            assert!(PayloadCas::new(root.path()).unwrap().stat_object(&asset).unwrap().is_none());
        });
    }

    /// Of two custody-only assets, only the one the destination lacks is
    /// fetched, and it is fetched once, checked, and restored intact.
    #[test]
    fn c_a_publication_fetches_only_the_custody_asset_its_destination_lacks() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let (first, held) =
                published_then_released(root.path(), &provider, &repository, &key).await;
            let missing = hex::encode(hash(b"asset two"));
            crate::server_sync::residency::test_remote::hold(root.path(), &[(&missing, 9)]);
            crate::server_sync::residency::test_remote::serve(
                root.path(), &missing, b"asset two".to_vec(),
            );
            let second = publish_with(root.path(), &first, &[b"asset two"], &provider, &repository, &key)
                .await.unwrap();
            assert_eq!(second.hydration, SourceHydration { objects: 1, bytes: 9 });
            assert_eq!(crate::server_sync::residency::test_remote::fetched(root.path()), 1);
            let cas = PayloadCas::new(root.path()).unwrap();
            assert_eq!(cas.stat_object(&missing).unwrap(), None);
            assert!(cas.stat_object(&held).unwrap().is_none());
            let restored = snapshot_restore::download_snapshot(&second.reference,
                &root.path().join("restored"), &key, None, snapshot_restore::SourceTrust::Downloaded,
                &provider, &repository, &PhaseProgress::silent(), &Cancellation::default())
                .await.unwrap();
            for (digest, body) in [(&held, &b"asset one"[..]), (&missing, &b"asset two"[..])] {
                let object = restored.objects.iter()
                    .find(|object| &object.content_hash == digest).unwrap();
                assert_eq!(fs::read(object.source.file().unwrap()).unwrap(), body);
            }
        });
    }

    /// A fetched body that differs from its identity is never packaged, and
    /// the publication stops before it names a snapshot.
    #[test]
    fn c_a_fetched_asset_that_fails_its_check_publishes_nothing() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let (first, _) =
                published_then_released(root.path(), &provider, &repository, &key).await;
            let missing = hex::encode(hash(b"asset two"));
            crate::server_sync::residency::test_remote::hold(root.path(), &[(&missing, 9)]);
            crate::server_sync::residency::test_remote::serve(
                root.path(), &missing, b"asset 2!!".to_vec(),
            );
            let before = snapshots(&provider);
            let error = publish_with(root.path(), &first, &[b"asset two"], &provider, &repository, &key)
                .await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::Transient);
            assert_eq!(snapshots(&provider), before);
            assert!(PayloadCas::new(root.path()).unwrap().stat_object(&missing).unwrap().is_none());
        });
    }

    /// An asset neither held nor in custody ends the publication as not
    /// found, before it names a snapshot.
    #[test]
    fn c_an_asset_without_custody_publishes_nothing() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let (first, _) =
                published_then_released(root.path(), &provider, &repository, &key).await;
            let before = snapshots(&provider);
            let error = publish_with(root.path(), &first, &[b"asset two"], &provider, &repository, &key)
                .await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::NotFound);
            assert_eq!(snapshots(&provider), before);
            assert_eq!(crate::server_sync::residency::test_remote::fetched(root.path()), 0);
        });
    }

    /// An entry whose packs belong to the selected parent is admitted on that
    /// membership alone. The tag moves with each publication, so the test also
    /// covers a reused entry still being admissible under the next parent.
    #[test]
    fn c_entries_of_the_selected_parent_are_admitted_without_asking_the_provider() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [11; 32];
            let cancel = Cancellation::default();
            let mut published: Vec<CompletedSnapshot> = Vec::new();
            for revision in 1..=3i64 {
                let name = format!("parent-{revision}");
                // One added asset per revision, so the catalog is rebuilt and
                // the unchanged entries are the ones under test.
                let capture = captured_asset_inventory(
                    root.path(), &name, revision, 1, 1, 3 + revision as usize,
                );
                let meta = metadata(&name, &capture);
                let mut transfer = journal(&root.path().join(&name), &name, &capture);
                let parent = published
                    .last()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let reads = provider.read_count();
                let completed = package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                    parent.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap();
                if revision > 1 {
                    assert_eq!(provider.read_count(), reads);
                }
                published.push(completed);
            }
            let packs: Vec<Vec<String>> = published
                .iter()
                .map(|completed| {
                    let mut ids: Vec<String> = completed.referenced_objects.iter()
                        .filter(|object| object.role == ObjectRole::Pack)
                        .map(|object| object.object_id.clone())
                        .collect();
                    ids.sort();
                    ids
                })
                .collect();
            let carried: Vec<&String> = packs[0]
                .iter()
                .filter(|id| packs[1].contains(id) && packs[2].contains(id))
                .collect();
            assert!(!carried.is_empty());
            for id in carried {
                assert_eq!(provider.upload_attempts(id), 1);
            }

            // The same growth published twice, once under the parent that owns
            // those entries and once under one that no longer does. Only the
            // second has to establish that their packs are still there.
            let mut reads = Vec::new();
            for (name, parent) in [("fresh", &published[2]), ("stale", &published[0])] {
                let capture = captured_asset_inventory(root.path(), name, 4, 1, 1, 7);
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.path().join(name), name, &capture);
                let graph = parent_graph(parent, &repository).unwrap();
                let before = provider.read_count();
                package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                    Some(&graph), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap();
                reads.push(provider.read_count() - before);
            }
            assert_eq!(reads[0], 0);
            assert!(reads[1] > 0, "{reads:?}");
        });
    }

    /// Membership of the parent graph settles that nothing collected a pack.
    /// It says nothing about one this connection already found unusable, and
    /// that answer is not inherited by admitting the entry that names it.
    #[test]
    fn c_a_pack_already_found_unusable_is_not_admitted_by_its_graph() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [17; 32];
            let cancel = Cancellation::default();
            let capture = captured_asset_inventory(root.path(), "head", 1, 1, 0, 4);
            let meta = metadata("head", &capture);
            let mut transfer = journal(&root.path().join("head"), "head", &capture);
            let head = package_and_upload(
                capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                None, &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
            ).await.unwrap();
            let parent = parent_graph(&head, &repository).unwrap();
            // An asset pack, which the next publication admits because only the
            // asset inventory grows and the entries naming it do not change.
            let lost = head.referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack).next_back().unwrap().clone();
            // What an earlier publication learned about this exact pack.
            let evidence = ObjectEvidence::open(root.path(), transfer.connection_id()).unwrap();
            evidence.record(
                &ObjectEvidence::identity(&lost, &repository).unwrap(),
                &lost.object_id,
                UnusableReason::Missing,
            ).unwrap();
            provider.forget(&lost.receipt.locator.object);
            drop(evidence);

            let capture = captured_asset_inventory(root.path(), "next", 2, 1, 0, 5);
            let meta = metadata("next", &capture);
            let mut transfer = journal(&root.path().join("next"), "next", &capture);
            let completed = package_and_upload(
                capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                Some(&parent), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
            ).await.unwrap();
            assert_eq!(
                verify_publication(
                    &completed, root.path(), &cache, &mut transfer, &key, &provider, &repository,
                    &cancel,
                ).await.unwrap(),
                PublicationReadiness::Verified,
            );
            assert!(!completed.referenced_objects.iter()
                .any(|object| object.object_id == lost.object_id));
            for object in &completed.referenced_objects {
                assert!(provider.holds(&object.receipt.locator.object), "{}", object.object_id);
            }
        });
    }

    /// A catalog level far wider than one wave is sent in waves, so the
    /// ciphertext it holds at once fits a spool that could not take the
    /// whole level.
    #[test]
    fn c_a_wide_catalog_level_is_sent_in_waves_that_fit_the_spool() {
        TEST_METADATA_WAVE_OBJECTS.with(|value| value.set(8));
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let capture = captured_record_window(root.path(), "wide", 1, 400, 0, 0, 0);
            let meta = metadata("wide", &capture);
            let directory = root.path().join("wide-job");
            let job = format!("wide-{}", uuid::Uuid::new_v4());
            let mut transfer = journal(&directory, &job, &capture);
            let holder = (job.clone(), directory.clone());
            transfer.set_spool_budget(
                super::super::journal::SpoolBudget::new(job.clone(), move || {
                    Ok(vec![holder.clone()])
                })
                .with_limit(50 * 1024),
            );
            let completed = package_and_upload(
                capture, vec![], root.path(), &root.path().join("cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await
            .unwrap();
            let catalogs = completed
                .referenced_objects
                .iter()
                .filter(|object| object.role == ObjectRole::Catalog)
                .count();
            assert!(
                catalogs > 2 * metadata_wave_objects(),
                "{catalogs} catalog nodes do not need more than one wave"
            );
        });
    }

    /// The capture a stopped job published from, reopened for its next attempt.
    fn reopened(root: &Path, reference: &super::super::capture::DurableCaptureReference) -> CapturedSnapshot {
        let external = root.join("external-storage");
        let expected: [u8; 32] = hex::decode(&reference.catalog_hash).unwrap().try_into().unwrap();
        CapturedSnapshot {
            id: reference.capture_id.clone(),
            identity: reference.identity.clone(),
            catalog: super::super::capture::CaptureCatalog::reopen(
                &external.join(&reference.catalog_path),
                &external,
                &expected,
                &reference.identity,
            )
            .unwrap(),
            projected_records: 0,
            shared: false,
        }
    }

    fn held_spool(directory: &Path) -> u64 {
        super::super::journal::held_spool_bytes(directory).unwrap_or(0)
    }

    /// A spool with room for a couple of objects still carries a publication
    /// to the end: what is already sealed is sent to make room for the next
    /// pack or catalog node, and the spool never holds more than its budget.
    #[test]
    fn c_a_spool_with_room_for_two_objects_still_finishes_the_publication() {
        for limit in [12 * 1024, 9 * 1024] {
            runtime().block_on(async {
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let capture = captured_record_window(root.path(), "tight", 1, 400, 0, 0, 0);
                let meta = metadata("tight", &capture);
                let directory = root.path().join("tight-job");
                let job = format!("tight-{}", uuid::Uuid::new_v4());
                let mut transfer = journal(&directory, &job, &capture);
                let holder = (job.clone(), directory.clone());
                transfer.set_spool_budget(
                    super::super::journal::SpoolBudget::new(job.clone(), move || {
                        Ok(vec![holder.clone()])
                    })
                    .with_limit(limit),
                );
                let (peak, stop, sampler) = peak_sampler(directory.clone(), held_spool);
                let completed = package_and_upload(
                    capture, vec![], root.path(), &root.path().join("cache"), meta, &[5; 32],
                    limits(4096), None, &mut transfer, &provider, &repository,
                    &PhaseProgress::silent(), &Cancellation::default(),
                )
                .await;
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                sampler.join().unwrap();
                let completed = completed.unwrap_or_else(|error| {
                    panic!("a {limit}-byte spool stopped the publication: {:?}", error.kind)
                });
                assert!(
                    completed
                        .referenced_objects
                        .iter()
                        .filter(|object| object.role == ObjectRole::Pack)
                        .count()
                        > 2
                );
                let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
                assert!(peak <= limit, "{peak} bytes held against a {limit}-byte spool");
            });
        }
    }

    /// A job that another job crowds out of the spool stops without holding
    /// more than the budget, and the same journal and capture resumed beside
    /// a peer that still holds most of it finish one object at a time.
    #[test]
    fn c_a_job_crowded_out_of_the_spool_resumes_beside_the_job_that_holds_it() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let limit = 12 * 1024;
            let capture = captured_record_window(root.path(), "crowded", 1, 400, 0, 0, 0);
            let reference = capture.durable_reference(root.path()).unwrap();
            let directory = root.path().join("crowded-job");
            let peer = root.path().join("peer-job");
            fs::create_dir_all(&peer).unwrap();
            let job = format!("crowded-{}", uuid::Uuid::new_v4());
            // The peer takes the whole spool once this job has admitted a few
            // objects.
            let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let budget = |grow_after: Option<usize>| {
                let holders = vec![
                    (job.clone(), directory.clone()),
                    ("peer".to_owned(), peer.clone()),
                ];
                let asked = asked.clone();
                let peer = peer.clone();
                super::super::journal::SpoolBudget::new(job.clone(), move || {
                    let count = asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if grow_after.is_some_and(|after| count == after) {
                        fs::write(peer.join("held.spool"), vec![0u8; limit as usize])
                            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
                    }
                    Ok(holders.clone())
                })
                .with_limit(limit)
            };

            let meta = metadata("crowded", &capture);
            let mut transfer = journal(&directory, &job, &capture);
            transfer.set_spool_budget(budget(Some(8)));
            let error = package_and_upload(
                capture, vec![], root.path(), &root.path().join("cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await
            .err()
            .expect("a full spool stops the job");
            assert_eq!(error.kind, ErrorKind::Transient);
            drop(transfer);
            let first = provider.uploaded_ids().len();
            assert!(first > 0);
            assert!(held_spool(&directory) <= limit);

            // Most of the spool stays with the peer.
            fs::write(peer.join("held.spool"), vec![0u8; 5 * 1024]).unwrap();
            let capture = reopened(root.path(), &reference);
            let meta = metadata("crowded", &capture);
            let mut transfer = journal(&directory, &job, &capture);
            transfer.set_spool_budget(budget(None));
            let (peak, stop, sampler) = peak_sampler(directory.clone(), held_spool);
            let completed = package_and_upload(
                capture, vec![], root.path(), &root.path().join("cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            sampler.join().unwrap();
            let completed = completed.unwrap();
            assert!(provider.uploaded_ids().len() > first);
            assert!(completed.referenced_objects.len() > 2);
            let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
            assert!(peak + 5 * 1024 <= limit, "{peak} bytes held beside the peer's 5 KiB");
        });
    }

    /// The largest value `measure` gives while a publication runs.
    fn peak_of(
        measure: impl Fn() -> u64 + Send + 'static,
    ) -> (
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let peak = peak.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    peak.fetch_max(measure(), std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        (peak, stop, handle)
    }

    fn packs_of(completed: &CompletedSnapshot) -> usize {
        completed
            .referenced_objects
            .iter()
            .filter(|object| object.role == ObjectRole::Pack)
            .count()
    }

    /// A deadlock fails the test rather than hanging it.
    const FAMILY_TEST_BOUND: std::time::Duration = std::time::Duration::from_secs(300);

    /// Two jobs on separate connections share the application's two pack
    /// families and its spool. Neither waits for a family that only its own
    /// wait could release: both finish and every family ends. No more pack
    /// plaintexts are on disk than there are families, beside the one
    /// catalog node each job may be sealing and one more a listing can catch
    /// while a node is replaced.
    #[test]
    fn c_two_jobs_share_the_pack_families_and_both_finish() {
        runtime().block_on(async {
            const RECORDS: usize = 8;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let limit = 4 * 1024 * 1024;
            let left_job = format!("left-{}", uuid::Uuid::new_v4());
            let right_job = format!("right-{}", uuid::Uuid::new_v4());
            let left_directory = root.path().join("left-job");
            let right_directory = root.path().join("right-job");
            let holders = vec![
                (left_job.clone(), left_directory.clone()),
                (right_job.clone(), right_directory.clone()),
            ];
            let budget = |job: &str| {
                let holders = holders.clone();
                super::super::journal::SpoolBudget::new(job.to_owned(), move || Ok(holders.clone()))
                    .with_limit(limit)
                    .with_families(families.clone())
            };
            let left_capture = captured_record_window(root.path(), "left", 1, RECORDS, 0, 0, 0);
            let right_capture = captured_record_window(root.path(), "right", 1, RECORDS, 0, 0, 0);
            let left_meta = metadata("left", &left_capture);
            let right_meta = metadata("right", &right_capture);
            let mut left = journal(&left_directory, &left_job, &left_capture);
            let mut right = journal(&right_directory, &right_job, &right_capture);
            left.set_spool_budget(budget(&left_job));
            right.set_spool_budget(budget(&right_job));
            let builds = [root.path().join("left-cache").join("build"), root.path().join("right-cache").join("build")];
            let (plaintexts, stop, sampler) =
                peak_of(move || builds.iter().map(|directory| build_files(directory)).sum());
            let (left_spool, right_spool) = (left_directory.clone(), right_directory.clone());
            let (spooled, spool_stop, spool_sampler) =
                peak_of(move || held_spool(&left_spool) + held_spool(&right_spool));
            let (left_cache, right_cache) = (root.path().join("left-cache"), root.path().join("right-cache"));
            let (key, progress, cancel) = ([5; 32], PhaseProgress::silent(), Cancellation::default());
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let completed = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(
                        left_capture, vec![], root.path(), &left_cache, left_meta, &key,
                        package_limits, None, &mut left, &provider, &repository, &progress, &cancel,
                    ),
                    package_and_upload(
                        right_capture, vec![], root.path(), &right_cache, right_meta, &key,
                        package_limits, None, &mut right, &provider, &repository, &progress, &cancel,
                    ),
                )
            })
            .await;
            for stopped in [&stop, &spool_stop] {
                stopped.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            sampler.join().unwrap();
            spool_sampler.join().unwrap();
            let (left_done, right_done) = completed
                .expect("two jobs sharing the pack families stopped making progress");
            let (left_done, right_done) = (left_done.unwrap(), right_done.unwrap());
            assert_eq!(packs_of(&left_done), RECORDS);
            assert_eq!(packs_of(&right_done), RECORDS);
            assert_eq!(families.counts(), (0, 0));
            let plaintexts = plaintexts.load(std::sync::atomic::Ordering::Relaxed);
            assert!(
                plaintexts <= ACTIVE_PACK_FAMILIES as u64 + 2,
                "{plaintexts} plaintext files beside two pack families"
            );
            let spooled = spooled.load(std::sync::atomic::Ordering::Relaxed);
            assert!(spooled <= limit, "{spooled} bytes held against a {limit}-byte spool");
            assert_eq!(held_spool(&left_directory) + held_spool(&right_directory), 0);
        });
    }

    /// One globally available family still permits progress because sealed
    /// ciphertext releases its family before waiting for remote completion.
    #[test]
    fn c_a_job_left_one_family_completes_with_one_preparation_at_a_time() {
        runtime().block_on(async {
            const RECORDS: usize = 8;
            let root = tempfile::tempdir().unwrap();
            let repository = fake::repository();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let elsewhere = families.try_acquire().unwrap().unwrap();
            let capture = captured_record_window(root.path(), "single", 1, RECORDS, 0, 0, 0);
            let meta = metadata("single", &capture);
            let directory = root.path().join("single-job");
            let job = format!("single-{}", uuid::Uuid::new_v4());
            let mut transfer = journal(&directory, &job, &capture);
            let holder = (job.clone(), directory.clone());
            transfer.set_spool_budget(
                super::super::journal::SpoolBudget::new(job.clone(), move || Ok(vec![holder.clone()]))
                    .with_limit(4 * 1024 * 1024)
                    .with_families(families.clone()),
            );
            let builds = root.path().join("cache").join("build");
            let mut provider = GatedProvider::new(None);
            provider.on_pack = Some(Box::new({
                let families = families.clone();
                let builds = builds.clone();
                move |uploaded| {
                    let (active, _) = families.counts();
                    assert!((1..=ACTIVE_PACK_FAMILIES).contains(&active));
                    assert!(build_files(&builds) <= 1, "more than one available preparation family");
                    assert!(uploaded <= RECORDS);
                }
            }));
            // A tiny target forces one distinct record per pack without thousands
            // of durable body and catalog writes unrelated to family turnover.
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let completed = tokio::time::timeout(
                FAMILY_TEST_BOUND,
                package_and_upload(
                    capture, vec![], root.path(), &root.path().join("cache"), meta, &[5; 32],
                    package_limits, None, &mut transfer, &provider, &repository,
                    &PhaseProgress::silent(), &Cancellation::default(),
                ),
            )
            .await
            .expect("a job left one family stopped making progress")
            .unwrap();
            assert_eq!(packs_of(&completed), RECORDS);
            assert_eq!(provider.packs.lock().unwrap().len(), RECORDS);
            assert_eq!(families.counts(), (1, 0));
            assert_eq!(build_files(&builds), 0);
            assert_eq!(held_spool(&directory), 0);
            drop(elsewhere);
            assert_eq!(families.counts(), (0, 0));
        });
    }

    /// A producer waiting for a family stops when its job is cancelled. It is
    /// no longer counted as waiting, it wrote no spool, and the family active
    /// elsewhere is untouched.
    #[test]
    fn c_a_producer_waiting_for_a_family_stops_at_cancellation() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let families = PackFamilies::new(1);
            let _elsewhere = families.try_acquire().unwrap().unwrap();
            let capture = captured_record_window(root.path(), "waiting", 1, 200, 0, 0, 0);
            let meta = metadata("waiting", &capture);
            let directory = root.path().join("waiting-job");
            let job = format!("waiting-{}", uuid::Uuid::new_v4());
            let mut transfer = journal(&directory, &job, &capture);
            let holder = (job.clone(), directory.clone());
            transfer.set_spool_budget(
                super::super::journal::SpoolBudget::new(job.clone(), move || Ok(vec![holder.clone()]))
                    .with_limit(4 * 1024 * 1024)
                    .with_families(families.clone()),
            );
            let cancel = Cancellation::default();
            let (cache, progress) = (root.path().join("cache"), PhaseProgress::silent());
            let (outcome, ()) = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(
                        capture, vec![], root.path(), &cache, meta, &[5; 32],
                        limits(4096), None, &mut transfer, &provider, &repository,
                        &progress, &cancel,
                    ),
                    async {
                        families.contended().await;
                        cancel.cancel();
                    },
                )
            })
            .await
            .expect("a cancelled producer kept waiting for a family");
            assert_eq!(outcome.err().map(|error| error.kind), Some(ErrorKind::Cancelled));
            assert_eq!(families.counts(), (1, 0));
            assert_eq!(held_spool(&directory), 0);
            assert!(provider.uploaded_ids().is_empty());
        });
    }

    /// A job stopped by an upload whose outcome it could not observe keeps
    /// its sealed packs charged to the spool but gives up its families. A
    /// restarted budget counts those files, a peer publishes beside them
    /// without touching them, and the stopped job later sends exactly the
    /// ciphertext it kept, releasing each file only after its receipt.
    #[test]
    fn c_a_stopped_job_keeps_its_spool_but_not_its_families() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = gated::GatedProvider::new(Some(3));
            let repository = fake::repository();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let stopped_job = format!("stopped-{}", uuid::Uuid::new_v4());
            let peer_job = format!("peer-{}", uuid::Uuid::new_v4());
            let stopped_directory = root.path().join("stopped-job");
            let peer_directory = root.path().join("peer-job");
            let holders = vec![
                (stopped_job.clone(), stopped_directory.clone()),
                (peer_job.clone(), peer_directory.clone()),
            ];
            let budget = |job: &str, limit: u64| {
                let holders = holders.clone();
                super::super::journal::SpoolBudget::new(job.to_owned(), move || Ok(holders.clone()))
                    .with_limit(limit)
                    .with_families(families.clone())
            };
            let capture = captured_record_window(root.path(), "stopped", 1, 400, 0, 0, 0);
            let reference = capture.durable_reference(root.path()).unwrap();
            let meta = metadata("stopped", &capture);
            let mut transfer = journal(&stopped_directory, &stopped_job, &capture);
            transfer.set_spool_budget(budget(&stopped_job, 4 * 1024 * 1024));
            // The third pack's page is uploaded, while the pack's outcome
            // is left for a later session to observe.
            let error = package_and_upload(
                capture, vec![], root.path(), &root.path().join("stopped-cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await
            .err()
            .expect("the refused upload stops the job");
            assert_eq!(error.kind, ErrorKind::RateLimited);
            let refused = provider.packs.lock().unwrap()[2].clone();
            let record = transfer.record(&refused).unwrap().unwrap();
            assert!(record.attempted && record.receipt.is_none());
            let page = transfer.covered_by(&refused).unwrap().unwrap();
            assert!(transfer.record(&page).unwrap().unwrap().receipt.is_some());
            drop(transfer);
            assert_eq!(families.counts(), (0, 0));
            let kept: BTreeMap<String, [u8; 32]> = fs::read_dir(&stopped_directory)
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|value| value == "spool"))
                .map(|entry| {
                    (
                        entry.path().file_stem().unwrap().to_string_lossy().into_owned(),
                        hash(&fs::read(entry.path()).unwrap()),
                    )
                })
                .collect();
            assert!(!kept.is_empty());
            let held = held_spool(&stopped_directory);

            // A restarted application counts the files the stopped job left.
            let stopped_only = (stopped_job.clone(), stopped_directory.clone());
            let restarted = |limit: u64| {
                let stopped_only = stopped_only.clone();
                super::super::journal::SpoolBudget::new(peer_job.clone(), move || {
                    Ok(vec![stopped_only.clone()])
                })
                .with_limit(limit)
            };
            assert!(restarted(held).reserve(1).is_err());
            drop(restarted(held + 1).reserve(1).unwrap());

            // A peer publishes in the room the stopped job leaves.
            let room = 12 * 1024;
            let limit = held + room;
            let capture = captured_record_window(root.path(), "peer", 1, 400, 0, 0, 0);
            let meta = metadata("peer", &capture);
            let mut transfer = journal(&peer_directory, &peer_job, &capture);
            transfer.set_spool_budget(budget(&peer_job, limit));
            let (peak, stop, sampler) = peak_sampler(peer_directory.clone(), held_spool);
            let completed = package_and_upload(
                capture, vec![], root.path(), &root.path().join("peer-cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await;
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            sampler.join().unwrap();
            assert!(packs_of(&completed.unwrap()) > 2);
            let peak = peak.load(std::sync::atomic::Ordering::Relaxed);
            assert!(peak + held <= limit, "{peak} bytes beside the stopped job's {held}");
            assert_eq!(held_spool(&stopped_directory), held);
            assert_eq!(families.counts(), (0, 0));

            // The stopped job resumes and sends the bytes it kept.
            let capture = reopened(root.path(), &reference);
            let meta = metadata("stopped", &capture);
            let mut transfer = journal(&stopped_directory, &stopped_job, &capture);
            transfer.set_spool_budget(budget(&stopped_job, limit));
            package_and_upload(
                capture, vec![], root.path(), &root.path().join("stopped-cache"), meta, &[5; 32],
                limits(4096), None, &mut transfer, &provider, &repository,
                &PhaseProgress::silent(), &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(held_spool(&stopped_directory), 0);
            assert_eq!(families.counts(), (0, 0));
            let stored = provider.inner.state.lock().unwrap().objects.clone();
            for (name, digest) in &kept {
                let (object, (bytes, _)) = stored
                    .iter()
                    .find(|(object, _)| &hex::encode(hash(object.as_bytes())) == name)
                    .expect("a kept object was never sent");
                assert_eq!(&hash(bytes), digest, "{object} was sent with other bytes than it kept");
                assert!(transfer.record(object).unwrap().unwrap().released);
            }
        });
    }

    mod gated {
        use super::FakeProvider;
        use crate::external_storage::contract::*;

        /// A fake repository that refuses one pack upload by its position and
        /// holds the reconciliation of one named object until it is let go.
        pub(super) struct GatedProvider {
            pub(super) inner: FakeProvider,
            refuse_pack: Option<usize>,
            pub(super) packs: std::sync::Mutex<Vec<String>>,
            pub(super) on_pack: Option<Box<dyn Fn(usize) + Send + Sync>>,
            pub(super) gate: std::sync::Mutex<Option<String>>,
            pub(super) arrived: tokio::sync::Semaphore,
            pub(super) release: tokio::sync::Semaphore,
        }

        impl GatedProvider {
            pub(super) fn new(refuse_pack: Option<usize>) -> Self {
                Self {
                    inner: FakeProvider::new(false),
                    refuse_pack,
                    packs: Default::default(),
                    on_pack: None,
                    gate: Default::default(),
                    arrived: tokio::sync::Semaphore::new(0),
                    release: tokio::sync::Semaphore::new(0),
                }
            }
        }

        impl Provider for GatedProvider {
            fn open_repository<'a>(
                &'a self,
                config: &'a ConnectionConfig,
                secret: &'a SecretRef,
                mode: OpenMode,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, (RepositoryHandle, crate::external_storage::capabilities::Capabilities)> {
                self.inner.open_repository(config, secret, mode, cancel)
            }
            fn read_object<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                locator: &'a RemoteLocator,
                unchanged: Option<&'a VersionToken>,
                sink: &'a mut dyn TransferSink,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, ReadReceipt> {
                self.inner.read_object(repository, locator, unchanged, sink, cancel)
            }
            fn begin_upload<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                intent: &'a ObjectIntent,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, Option<ResumeState>> {
                self.inner.begin_upload(repository, intent, cancel)
            }
            fn create_object<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                intent: &'a ObjectIntent,
                source: &'a dyn TransferSource,
                resume: Option<&'a ResumeState>,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, ObjectReceipt> {
                if intent.role == ObjectRole::Pack {
                    let mut packs = self.packs.lock().unwrap();
                    packs.push(intent.object_id.clone());
                    if let Some(observe) = &self.on_pack {
                        observe(packs.len());
                    }
                    if self.refuse_pack == Some(packs.len()) {
                        return Box::pin(async { Err(ProviderError::new(ErrorKind::RateLimited)) });
                    }
                }
                self.inner.create_object(repository, intent, source, resume, cancel)
            }
            fn compare_exchange_head<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                locator: &'a RemoteLocator,
                expected: &'a ExpectedHead,
                head: &'a HeadBytes,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, HeadReceipt> {
                self.inner.compare_exchange_head(repository, locator, expected, head, cancel)
            }
            fn replace_head<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                locator: &'a RemoteLocator,
                head: &'a HeadBytes,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, HeadReceipt> {
                self.inner.replace_head(repository, locator, head, cancel)
            }
            fn list_objects<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                collection: Collection,
                cursor: Option<&'a str>,
                limit: u16,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, ObjectPage> {
                self.inner.list_objects(repository, collection, cursor, limit, cancel)
            }
            fn delete_object<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                locator: &'a RemoteLocator,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, ()> {
                self.inner.delete_object(repository, locator, cancel)
            }
            fn reconcile_upload<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                intent: &'a ObjectIntent,
                resume: Option<&'a ResumeState>,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, UploadResolution> {
                Box::pin(async move {
                    let gated = {
                        let mut gate = self.gate.lock().unwrap();
                        gate.as_deref() == Some(intent.object_id.as_str()) && gate.take().is_some()
                    };
                    if gated {
                        self.arrived.add_permits(1);
                        self.release.acquire().await.unwrap().forget();
                    }
                    self.inner.reconcile_upload(repository, intent, resume, cancel).await
                })
            }
            fn lookup_metadata<'a>(
                &'a self,
                repository: &'a RepositoryHandle,
                intent: &'a ObjectIntent,
                known: Option<&'a RemoteLocator>,
                cancel: &'a Cancellation,
            ) -> ProviderFuture<'a, Option<ObjectReceipt>> {
                self.inner.lookup_metadata(repository, intent, known, cancel)
            }
            fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
                self.inner.head_locator(repository)
            }
        }
    }
    use gated::GatedProvider;

    /// A pack the repository already holds ends its family as soon as it is
    /// sealed. When it completes a wave whose other pack still has to be
    /// sent, the producer starts the next pack while that upload is held,
    /// rather than waiting for a family nothing is using.
    #[test]
    fn c_a_pack_already_held_ends_its_family_before_its_wave_is_sent() {
        runtime().block_on(async {
            const RECORDS: usize = 8;
            let root = tempfile::tempdir().unwrap();
            let repository = fake::repository();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let job = format!("held-{}", uuid::Uuid::new_v4());
            let directory = root.path().join("held-job");
            let cache = root.path().join("cache");
            let budget = || {
                let holder = (job.clone(), directory.clone());
                super::super::journal::SpoolBudget::new(job.clone(), move || Ok(vec![holder.clone()]))
                    .with_limit(4 * 1024 * 1024)
                    .with_families(families.clone())
            };
            let capture = captured_record_window(root.path(), "held", 1, RECORDS, 0, 0, 0);
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let reference = capture.durable_reference(root.path()).unwrap();
            let meta = metadata("held", &capture);
            // The first two waves are sent and their packs cached; the fifth
            // pack stops the job.
            let provider = GatedProvider::new(Some(5));
            let mut transfer = journal(&directory, &job, &capture);
            transfer.set_spool_budget(budget());
            let error = package_and_upload(
                capture, vec![], root.path(), &cache, meta, &[5; 32], package_limits, None,
                &mut transfer, &provider, &repository, &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .err()
            .expect("the refused pack stops the job");
            assert_eq!(error.kind, ErrorKind::RateLimited);
            drop(transfer);
            assert_eq!(families.counts(), (0, 0));
            let packs = provider.packs.lock().unwrap().clone();
            assert_eq!(packs.len(), 5);
            // The second wave's first pack is sent again from its journal and
            // its partner comes back from the cache as already held.
            PackageCache::open(&cache)
                .unwrap()
                .forget_object("format-repository", &repository, &packs[2])
                .unwrap();
            *provider.gate.lock().unwrap() = Some(packs[2].clone());

            let capture = reopened(root.path(), &reference);
            let meta = metadata("held", &capture);
            let mut transfer = journal(&directory, &job, &capture);
            transfer.set_spool_budget(budget());
            let build = cache.join("build");
            let (progress, cancel) = (PhaseProgress::silent(), Cancellation::default());
            let (completed, (files, counts)) = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(
                        capture, vec![], root.path(), &cache, meta, &[5; 32], package_limits, None,
                        &mut transfer, &provider, &repository, &progress, &cancel,
                    ),
                    async {
                        provider.arrived.acquire().await.unwrap().forget();
                        // Preparation may already have sealed every remaining
                        // pack and released all families before this observation.
                        let seen = loop {
                            let (files, counts) = (build_files(&build), families.counts());
                            if files > 0 || counts.0 == 0 {
                                break (files, counts);
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        };
                        provider.release.add_permits(1);
                        seen
                    },
                )
            })
            .await
            .expect("the held wave stopped making progress");
            assert!(files > 0 || counts.0 == 0, "preparation held while a wave was held, families {counts:?}");
            assert!(counts.0 <= ACTIVE_PACK_FAMILIES);
            assert_eq!(packs_of(&completed.unwrap()), RECORDS);
            assert_eq!(families.counts(), (0, 0));
            assert_eq!(held_spool(&directory), 0);
        });
    }

    /// Two jobs sharing the pack families on a spool with room for only a
    /// few packs. Each either finishes or stops as transient, and a stopped
    /// one finishes when it runs again. Nothing either admitted stays
    /// charged, every family ends, and no spool file is left behind.
    #[test]
    fn c_two_jobs_on_a_tight_spool_finish_or_stop_without_holding_anything() {
        runtime().block_on(async {
            const RECORDS: usize = 8;
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let left_job = format!("left-{}", uuid::Uuid::new_v4());
            let right_job = format!("right-{}", uuid::Uuid::new_v4());
            let left_directory = root.path().join("left-job");
            let right_directory = root.path().join("right-job");
            let holders = vec![
                (left_job.clone(), left_directory.clone()),
                (right_job.clone(), right_directory.clone()),
            ];
            let budget = |job: &str, limit: u64| {
                let holders = holders.clone();
                super::super::journal::SpoolBudget::new(job.to_owned(), move || Ok(holders.clone()))
                    .with_limit(limit)
                    .with_families(families.clone())
            };
            let tight = 24 * 1024;
            let left_capture = captured_record_window(root.path(), "left", 1, RECORDS, 0, 0, 0);
            let right_capture = captured_record_window(root.path(), "right", 1, RECORDS, 0, 0, 0);
            let references = [
                left_capture.durable_reference(root.path()).unwrap(),
                right_capture.durable_reference(root.path()).unwrap(),
            ];
            let left_meta = metadata("left", &left_capture);
            let right_meta = metadata("right", &right_capture);
            let mut left = journal(&left_directory, &left_job, &left_capture);
            let mut right = journal(&right_directory, &right_job, &right_capture);
            left.set_spool_budget(budget(&left_job, tight));
            right.set_spool_budget(budget(&right_job, tight));
            let (left_cache, right_cache) = (root.path().join("left-cache"), root.path().join("right-cache"));
            let (key, progress, cancel) = ([5; 32], PhaseProgress::silent(), Cancellation::default());
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let (left_done, right_done) = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(
                        left_capture, vec![], root.path(), &left_cache, left_meta, &key,
                        package_limits, None, &mut left, &provider, &repository, &progress, &cancel,
                    ),
                    package_and_upload(
                        right_capture, vec![], root.path(), &right_cache, right_meta, &key,
                        package_limits, None, &mut right, &provider, &repository, &progress, &cancel,
                    ),
                )
            })
            .await
            .expect("two jobs on a tight spool stopped making progress");
            drop((left, right));
            assert_eq!(families.counts(), (0, 0));
            let runs = [
                ("left", &left_job, &left_directory, &left_cache, left_done),
                ("right", &right_job, &right_directory, &right_cache, right_done),
            ];
            for ((name, job, directory, cache, done), reference) in runs.into_iter().zip(&references) {
                assert_eq!(super::super::journal::admitted_spool_writes(job), 0);
                let done = match done {
                    Ok(done) => done,
                    Err(error) => {
                        assert_eq!(error.kind, ErrorKind::Transient, "{name} stopped otherwise");
                        let capture = reopened(root.path(), reference);
                        let meta = metadata(name, &capture);
                        let mut again = journal(directory, job, &capture);
                        again.set_spool_budget(budget(job, 4 * 1024 * 1024));
                        package_and_upload(
                            capture, vec![], root.path(), cache, meta, &key, package_limits, None,
                            &mut again, &provider, &repository, &progress, &cancel,
                        )
                        .await
                        .unwrap()
                    }
                };
                assert_eq!(packs_of(&done), RECORDS);
                assert_eq!(super::super::journal::admitted_spool_writes(job), 0);
                assert_eq!(held_spool(directory), 0);
            }
            assert_eq!(families.counts(), (0, 0));
        });
    }

    /// A catalog whose root the parent already names is protected down to
    /// every pack beneath it, so republishing the same library asks nothing.
    #[test]
    fn c_an_unchanged_catalog_under_the_parent_is_not_walked() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [29; 32];
            let cancel = Cancellation::default();
            let mut head = None;
            let mut reads = 0;
            for name in ["first", "second"] {
                let capture = captured_asset_inventory(root.path(), name, 1, 1, 0, 4);
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.path().join(name), name, &capture);
                let graph = head.as_ref().map(|previous| parent_graph(previous, &repository).unwrap());
                let before = provider.read_count();
                let completed = package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                    graph.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap();
                reads = provider.read_count() - before;
                if head.is_none() {
                    head = Some(completed);
                }
            }
            assert_eq!(reads, 0);
        });
    }

    /// A backup publishes into the same cache without selecting a parent. What
    /// the sync before it published still has to be reusable by the sync after
    /// it, so a publication records what its own graph holds and takes nothing
    /// away from another. Measured against the same sequence without it.
    #[test]
    fn c_a_publication_without_a_parent_does_not_cost_the_next_one_its_reuse() {
        runtime().block_on(async {
            let mut measured = Vec::new();
            for unparented in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let cache = root.path().join("cache");
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [23; 32];
                let cancel = Cancellation::default();
                let mut head = None;
                let mut reads = 0;
                let sequence: Vec<(&str, usize, bool)> = if unparented {
                    vec![("sync-1", 4, false), ("between", 4, false), ("sync-2", 5, true)]
                } else {
                    vec![("sync-1", 4, false), ("sync-2", 5, true)]
                };
                for (name, assets, parented) in sequence {
                    let capture = captured_asset_inventory(root.path(), name, 1, 1, 1, assets);
                    let meta = metadata(name, &capture);
                    let mut transfer = journal(&root.path().join(name), name, &capture);
                    let graph = parented
                        .then(|| parent_graph(head.as_ref().unwrap(), &repository).unwrap());
                    let before = provider.read_count();
                    let completed = package_and_upload(
                        capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                        graph.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                    ).await.unwrap();
                    reads = provider.read_count() - before;
                    if head.is_none() {
                        head = Some(completed);
                    }
                }
                let head = head.unwrap();
                for object in head.referenced_objects.iter()
                    .filter(|object| object.role == ObjectRole::Pack)
                {
                    assert_eq!(provider.upload_attempts(&object.object_id), 1);
                }
                // What the first publication holds is still on record after
                // everything published after it.
                let graph = parent_graph(&head, &repository).unwrap();
                assert!(!PackageCache::open(&cache).unwrap()
                    .members_of("format-repository", &repository, graph.identities()).unwrap()
                    .is_empty());
                measured.push(reads);
            }
            assert_eq!(measured[0], measured[1], "{measured:?}");
        });
    }

    /// A pack that went missing under the parent is found by the publication
    /// check, and the attempt after it has to repack rather than admit the
    /// same reference again on the strength of its tag.
    #[test]
    fn c_a_missing_pack_under_the_parent_does_not_repeat_forever() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [13; 32];
            let cancel = Cancellation::default();
            let capture = captured_asset_inventory(root.path(), "head", 1, 1, 1, 4);
            let meta = metadata("head", &capture);
            let mut transfer = journal(&root.path().join("head"), "head", &capture);
            let head = package_and_upload(
                capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                None, &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
            ).await.unwrap();
            let parent = parent_graph(&head, &repository).unwrap();
            let lost = head.referenced_objects.iter()
                .find(|object| object.role == ObjectRole::Pack).unwrap().clone();
            provider.forget(&lost.receipt.locator.object);

            let mut outcomes = Vec::new();
            for attempt in 2..=3i64 {
                let name = format!("attempt-{attempt}");
                let capture = captured_asset_inventory(root.path(), &name, attempt, 1, 1, 5);
                let meta = metadata(&name, &capture);
                let mut transfer = journal(&root.path().join(&name), &name, &capture);
                let completed = package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(128 * 1024),
                    Some(&parent), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap();
                outcomes.push((
                    verify_publication(
                        &completed, root.path(), &cache, &mut transfer, &key, &provider,
                        &repository, &cancel,
                    ).await.unwrap(),
                    completed,
                ));
            }
            assert_eq!(outcomes[0].0, PublicationReadiness::Repackage);
            assert_eq!(outcomes[1].0, PublicationReadiness::Verified);
            for object in &outcomes[1].1.referenced_objects {
                assert!(provider.holds(&object.receipt.locator.object), "{}", object.object_id);
            }
        });
    }

    /// A rebuilt cache has to name a catalog the way the next publication
    /// names it, and a catalog walk hands its entries back grouped by kind
    /// rather than in the order the catalog was built from.
    #[test]
    fn c_a_rebuilt_catalog_is_named_the_way_the_next_publication_names_it() {
        let source = |kind, key: &str, digest: &str| SourceEntry {
            kind,
            key: key.into(),
            content_sha256: digest.into(),
            byte_length: key.len() as u64,
            source: ObjectSource::Captured(digest.into()),
            file_offset: None,
            compression: CompressionPolicy::Text,
        };
        let plan = |kind, key: &str, digest: &str| EntryPlan {
            kind,
            key: key.into(),
            content_sha256: digest.into(),
            byte_length: key.len() as u64,
            chunks: Vec::new(),
            packs: Vec::new(),
        };
        let first = "11".repeat(32);
        let second = "22".repeat(32);
        let third = "33".repeat(32);
        // A record catalog is built from one list sorted by key alone, and a
        // generated object sorts in among the records rather than after them.
        let mut records = vec![
            source(wire::CatalogEntryKind::Record, "record/000000", &first),
            source(wire::CatalogEntryKind::Object, &format!("object/{second}"), &second),
            source(wire::CatalogEntryKind::Record, "record/000001", &third),
        ];
        records.sort_by(|a, b| a.key.cmp(&b.key));
        let walked = vec![
            plan(wire::CatalogEntryKind::Record, "record/000000", &first),
            plan(wire::CatalogEntryKind::Record, "record/000001", &third),
            plan(wire::CatalogEntryKind::Object, &format!("object/{second}"), &second),
        ];
        assert_eq!(
            super::super::cache_hydration::rebuilt_fingerprint(&CatalogRoot::Records, &walked),
            catalog_fingerprint(wire::CatalogKind::Records, &records),
        );
        // A section catalog is built from a list sorted by kind and then key,
        // and names the section as well as its content.
        let mut section = vec![
            source(wire::CatalogEntryKind::SectionObject, "b", &second),
            source(wire::CatalogEntryKind::SectionEntry, "c", &third),
        ];
        section.sort_by(|a, b| (a.kind as u8, &a.key).cmp(&(b.kind as u8, &b.key)));
        let walked = vec![
            plan(wire::CatalogEntryKind::SectionEntry, "c", &third),
            plan(wire::CatalogEntryKind::SectionObject, "b", &second),
        ];
        assert_eq!(
            super::super::cache_hydration::rebuilt_fingerprint(
                &CatalogRoot::Section("local-plugins".into()),
                &walked,
            ),
            section_catalog_fingerprint("local-plugins", &section),
        );
        assert_ne!(
            super::super::cache_hydration::rebuilt_fingerprint(
                &CatalogRoot::Section("other".into()),
                &walked,
            ),
            section_catalog_fingerprint("local-plugins", &section),
        );
    }

    /// Where a key belongs when the published ranges do not cover it. The
    /// answer has to be the same every time, or the node it lands in is
    /// rebuilt for nothing.
    #[test]
    fn c_a_key_outside_every_published_range_joins_the_node_next_to_it() {
        let ranges = vec![
            ("b".to_string(), "d".to_string()),
            ("g".to_string(), "i".to_string()),
        ];
        assert_eq!(previous_node(&ranges, "a"), 0);
        assert_eq!(previous_node(&ranges, "b"), 0);
        assert_eq!(previous_node(&ranges, "c"), 0);
        assert_eq!(previous_node(&ranges, "d"), 0);
        // A key between two nodes joins the one before it rather than sitting
        // between them.
        assert_eq!(previous_node(&ranges, "e"), 0);
        assert_eq!(previous_node(&ranges, "g"), 1);
        assert_eq!(previous_node(&ranges, "z"), 1);
    }

    /// One entry changing used to move every node after it. The leaves are
    /// filled from the front, and an entry that moves to a new pack makes its
    /// leaf carry one more pack reference, which pushes an entry out of it and
    /// every entry after that out of theirs.
    #[test]
    fn c_a_changed_entry_rebuilds_the_node_it_is_in_and_not_the_ones_after_it() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [41; 32];
            let cancel = Cancellation::default();
            let cache = root.path().join("cache");
            let mut published: Vec<CompletedSnapshot> = Vec::new();
            let mut uploads = Vec::new();
            for edited in [0usize, 1] {
                let name = format!("stable-{edited}");
                // The first record changes, so the node it is in is the first
                // of many and everything after it used to follow.
                let capture = captured_asset_inventory(
                    root.path(), &name, published.len() as i64 + 1, 128, edited, 2,
                );
                let meta = metadata(&name, &capture);
                let mut transfer = journal(&root.path().join(&name), &name, &capture);
                let parent = published
                    .last()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let before = provider.upload_count();
                published.push(package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(8 * 1024),
                    parent.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap());
                uploads.push(
                    provider.uploaded_ids()[before..].iter()
                        .filter(|id| id.starts_with("catalog-"))
                        .count(),
                );
            }
            assert!(uploads[0] > 8, "the record catalog has to have leaves: {uploads:?}");
            assert!(uploads[1] * 2 <= uploads[0], "rebuilt {uploads:?}");
            assert_ne!(published[1].record_catalog, published[0].record_catalog);
            assert_eq!(published[1].asset_catalog, published[0].asset_catalog);
        });
    }

    /// A record that grows is repackaged, but only the end of it is new.
    /// What the parent already stored for the rest of it is pointed at rather
    /// than encoded and uploaded again.
    #[test]
    fn c_a_changed_entry_points_at_the_chunks_the_parent_already_stored() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [47; 32];
            let cancel = Cancellation::default();
            let cache = root.path().join("cache");
            let body: Vec<u8> = (0..100_000u32).map(|index| (index % 251) as u8).collect();
            let mut grown = body.clone();
            grown.extend_from_slice(b"one more turn");

            let mut published: Vec<CompletedSnapshot> = Vec::new();
            for (name, record) in [("grown-1", &body), ("grown-2", &grown)] {
                let (capture, _) =
                    captured(root.path(), name, published.len() as i64 + 1, record, b"asset");
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.path().join(name), name, &capture);
                let parent = published
                    .last()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                published.push(package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(16 * 1024),
                    parent.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap());
            }
            let carried: Vec<String> = published[0].referenced_objects.iter()
                .filter(|object| object.role == ObjectRole::Pack)
                .map(|object| object.object_id.clone())
                .collect();
            let plan = PackageCache::open(&cache).unwrap()
                .entry_by_key("format-repository", &repository, wire::CatalogKind::Records, "root")
                .unwrap()
                .unwrap();
            assert!(plan.chunks.len() > 4, "the record has to span chunks");
            let reused = plan.chunks.iter()
                .filter(|chunk| carried.contains(&chunk.pack_id))
                .count();
            assert!(reused + 1 >= plan.chunks.len(), "reused {reused} of {}", plan.chunks.len());
            for id in &carried {
                assert_eq!(provider.upload_attempts(id), 1);
            }

            // What it points at has to read back as the record it published.
            let restored = snapshot_restore::download_snapshot(
                &published[1].reference, &root.path().join("restored"), &key, None,
                snapshot_restore::SourceTrust::Downloaded, &provider, &repository, &crate::external_storage::phase_progress::PhaseProgress::silent(), &cancel,
            ).await.unwrap();
            assert_eq!(
                restored.record_body(0),
                grown,
            );
        });
    }

    /// A catalog that changes still holds nodes it held before. Uploading
    /// those again under this job's own identity publishes the same bytes
    /// twice for nothing.
    #[test]
    fn c_a_catalog_node_already_published_is_referenced_instead_of_uploaded() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [37; 32];
            let cancel = Cancellation::default();
            let cache = root.path().join("cache");
            let mut published: Vec<CompletedSnapshot> = Vec::new();
            for records in [128usize, 129] {
                let name = format!("node-{records}");
                // One record appended past the last key, so the leaves before
                // it hold exactly what they held before.
                let capture = captured_asset_inventory(
                    root.path(), &name, published.len() as i64 + 1, records, 0, 2,
                );
                let meta = metadata(&name, &capture);
                let mut transfer = journal(&root.path().join(&name), &name, &capture);
                let parent = published
                    .last()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                published.push(package_and_upload(
                    capture, vec![], root.path(), &cache, meta, &key, limits(8 * 1024),
                    parent.as_ref(), &mut transfer, &provider, &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap());
            }
            let first = &published[0];
            let second = &published[1];
            assert_ne!(second.record_catalog, first.record_catalog);
            let identity =
                super::super::reachability::object_identity(&first.record_catalog, &repository)
                    .unwrap();
            let nodes: Vec<String> = PackageCache::open(&cache).unwrap()
                .graph_of("format-repository", &repository, &identity).unwrap()
                .into_iter()
                .filter(|object| object.role == ObjectRole::Catalog)
                .map(|object| object.object_id)
                .collect();
            assert!(nodes.len() > 2, "the record catalog has to have leaves to reuse");
            let reused: Vec<&String> = nodes.iter()
                .filter(|id| {
                    second.referenced_objects.iter().any(|object| &object.object_id == *id)
                })
                .collect();
            assert!(reused.len() + 2 >= nodes.len(), "reused {} of {}", reused.len(), nodes.len());
            for id in reused {
                assert_eq!(provider.upload_attempts(id), 1);
            }
        });
    }

    /// A device that has never published here holds nothing about the parent
    /// it selects. Reading what that parent names costs metadata and saves
    /// repackaging and re-uploading everything under it.
    #[test]
    fn c_a_cache_that_never_saw_the_parent_publishes_from_it_anyway() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [23; 32];
            let cancel = Cancellation::default();
            #[allow(clippy::too_many_arguments)]
            async fn published(
                root: &Path,
                name: &str,
                cache: &Path,
                parent: Option<&ParentGraph>,
                assets: usize,
                key: &[u8; 32],
                provider: &FakeProvider,
                repository: &RepositoryHandle,
                cancel: &Cancellation,
            ) -> CompletedSnapshot {
                let capture = captured_asset_inventory(root, name, 1, 2, 0, assets);
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.join(name), name, &capture);
                package_and_upload(
                    capture, vec![], root, cache, meta, key, limits(64 * 1024),
                    parent, &mut transfer, provider, repository, &PhaseProgress::silent(), cancel,
                ).await.unwrap()
            }
            let objects = |completed: &CompletedSnapshot| {
                let mut ids: Vec<String> = completed.referenced_objects.iter()
                    .filter(|object| {
                        matches!(object.role, ObjectRole::Pack | ObjectRole::Catalog)
                    })
                    .map(|object| object.object_id.clone())
                    .collect();
                ids.sort();
                ids
            };

            let first = published(root.path(), "first-device", &root.path().join("cache-a"), None, 6, &key, &provider, &repository, &cancel).await;
            let parent = parent_graph(&first, &repository).unwrap();
            let elsewhere = root.path().join("cache-b");

            // The same library, from a cache holding nothing about that parent.
            let uploads = provider.upload_count();
            let reads = provider.read_count();
            let second = published(root.path(), "second-device", &elsewhere, Some(&parent), 6, &key, &provider, &repository, &cancel).await;
            assert_eq!(objects(&second), objects(&first));
            for id in objects(&second) {
                assert_eq!(provider.upload_attempts(&id), 1);
            }
            assert_eq!(second.record_catalog, first.record_catalog);
            assert_eq!(second.asset_catalog, first.asset_catalog);
            // Its own inventory page and snapshot document, and nothing the
            // parent already holds.
            let added: Vec<String> = provider.uploaded_ids()[uploads..].to_vec();
            assert_eq!(added.len(), 2);
            assert!(added.iter().any(|id| id.starts_with("inventory-page-")));
            assert!(added.iter().any(|id| id == "snapshot-second-device"));
            // Metadata is what it paid for that.
            assert!(provider.read_count() > reads);

            // What it learned is durable: the same parent is not read again.
            let reads = provider.read_count();
            let again = published(root.path(), "second-again", &elsewhere, Some(&parent), 6, &key, &provider, &repository, &cancel).await;
            assert_eq!(provider.read_count(), reads);
            assert_eq!(objects(&again), objects(&first));

            // One asset more, from the same rebuilt cache. Everything the
            // parent already carries stays where it is, and the catalog the
            // growth did not reach is not rebuilt at all.
            let grown = published(root.path(), "second-grown", &elsewhere, Some(&parent), 7, &key, &provider, &repository, &cancel).await;
            let carried: Vec<String> = objects(&first).into_iter()
                .filter(|id| objects(&grown).contains(id))
                .collect();
            assert!(!carried.is_empty());
            for id in carried {
                assert_eq!(provider.upload_attempts(&id), 1);
            }
            assert_eq!(grown.record_catalog, first.record_catalog);
            assert_ne!(grown.asset_catalog, first.asset_catalog);
        });
    }

    /// A receive reads the catalogs of the snapshot it takes, which is the
    /// same snapshot the publication after it selects as its parent. Keeping
    /// what it read saves that publication from asking for it again.
    #[test]
    fn c_a_download_that_records_what_it_read_saves_the_next_publication_the_read() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [43; 32];
            let cancel = Cancellation::default();
            #[allow(clippy::too_many_arguments)]
            async fn published(
                root: &Path,
                name: &str,
                cache: &Path,
                parent: Option<&ParentGraph>,
                key: &[u8; 32],
                provider: &FakeProvider,
                repository: &RepositoryHandle,
                cancel: &Cancellation,
            ) -> CompletedSnapshot {
                let capture = captured_asset_inventory(root, name, 1, 2, 0, 6);
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.join(name), name, &capture);
                package_and_upload(
                    capture, vec![], root, cache, meta, key, limits(64 * 1024),
                    parent, &mut transfer, provider, repository, &PhaseProgress::silent(), cancel,
                ).await.unwrap()
            }
            let first = published(
                root.path(), "first-device", &root.path().join("cache-a"), None,
                &key, &provider, &repository, &cancel,
            ).await;
            let parent = parent_graph(&first, &repository).unwrap();

            for (name, record_into) in [("kept", true), ("discarded", false)] {
                let elsewhere = root.path().join(format!("cache-{name}"));
                snapshot_restore::download_snapshot(
                    &first.reference,
                    &root.path().join(format!("receive-{name}")),
                    &key,
                    record_into.then(|| elsewhere.as_path()),
                    snapshot_restore::SourceTrust::Downloaded,
                    &provider,
                    &repository,
                    &crate::external_storage::phase_progress::PhaseProgress::silent(),
                    &cancel,
                ).await.unwrap();
                let reads = provider.read_count();
                let second = published(
                    root.path(), name, &elsewhere, Some(&parent),
                    &key, &provider, &repository, &cancel,
                ).await;
                assert_eq!(second.record_catalog, first.record_catalog);
                if record_into {
                    assert_eq!(provider.read_count(), reads, "{name} asked again");
                } else {
                    assert!(provider.read_count() > reads, "{name} asked for nothing");
                }
            }
        });
    }

    /// A parent root this device cannot read leaves that catalog unknown.
    /// The publication then pays for it the way a cache miss already did,
    /// and the roots it could read are still reused.
    #[test]
    fn c_a_parent_root_it_cannot_read_costs_reuse_and_not_the_publication() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [29; 32];
            let cancel = Cancellation::default();
            #[allow(clippy::too_many_arguments)]
            async fn published(
                root: &Path,
                name: &str,
                cache: &Path,
                parent: Option<&ParentGraph>,
                key: &[u8; 32],
                provider: &FakeProvider,
                repository: &RepositoryHandle,
                cancel: &Cancellation,
            ) -> CompletedSnapshot {
                let capture = captured_asset_inventory(root, name, 1, 2, 0, 6);
                let meta = metadata(name, &capture);
                let mut transfer = journal(&root.join(name), name, &capture);
                package_and_upload(
                    capture, vec![], root, cache, meta, key, limits(64 * 1024),
                    parent, &mut transfer, provider, repository, &PhaseProgress::silent(), cancel,
                ).await.unwrap()
            }
            let first = published(
                root.path(), "first-device", &root.path().join("cache-a"), None,
                &key, &provider, &repository, &cancel,
            ).await;
            let parent = parent_graph(&first, &repository).unwrap();
            provider.fail_read(&first.asset_catalog.object_id, ErrorKind::NotFound);
            let second = published(
                root.path(), "second-device", &root.path().join("cache-b"), Some(&parent),
                &key, &provider, &repository, &cancel,
            ).await;
            assert_eq!(second.record_catalog, first.record_catalog);
            assert_ne!(second.asset_catalog, first.asset_catalog);
        });
    }

    /// The parent is durable on the job before anything reuses it, so a
    /// cleanup that finds the job stopped protects what it is reading.
    #[test]
    fn c_a_selected_parent_is_recorded_on_the_job_for_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let repository = fake::repository();
        let (capture, _) = captured(root.path(), "recorded", 1, b"record", b"asset");
        let directory = root.path().join("recorded-job");
        let transfer = journal(&directory, "recorded-job", &capture);
        assert!(TransferJournal::parent(&directory).unwrap().is_empty());
        let first = super::super::reachability::tests::object("catalog-a", ObjectRole::Catalog).stored(&repository).unwrap();
        let second = super::super::reachability::tests::object("catalog-b", ObjectRole::Catalog).stored(&repository).unwrap();
        let graph = ParentGraph::new(
            vec![
                (CatalogRoot::Records, first.clone()),
                (CatalogRoot::Assets, second.clone()),
            ],
            &repository,
        )
        .unwrap();
        transfer.record_parent(graph.stored()).unwrap();
        assert_eq!(TransferJournal::parent(&directory).unwrap(), vec![first, second]);
        // A pack is never a parent root; only a catalog names a graph.
        assert_eq!(
            ParentGraph::new(
                vec![(
                    CatalogRoot::Records,
                    super::super::reachability::tests::object("pack-a", ObjectRole::Pack).stored(&repository).unwrap(),
                )],
                &repository,
            ).unwrap_err().kind,
            ErrorKind::Corrupt,
        );
        assert_eq!(ParentGraph::new(vec![], &repository).unwrap_err().kind, ErrorKind::Corrupt);
    }

    #[test]
    fn c_actual_catalog_and_envelope_lengths_enforce_exact_boundaries() {
        let kind = wire::CatalogKind::Assets;
        let leaf = wire::CatalogDocument::leaf(kind, Vec::new(), Vec::new()).unwrap();
        let bytes = leaf.encode(wire::MAX_METADATA_BYTES).unwrap();
        let catalog_id = format!("catalog-{}", "0".repeat(64));
        let header = wire::PublicObjectHeader::new(
            "repository".into(), catalog_id, wire::ObjectRole::Catalog, bytes.len() as u64,
        ).unwrap();
        let exact = wire::envelope_length(&header).unwrap() + 137;
        for (maximum, fits) in [(exact - 1, false), (exact, true), (exact + 1, true)] {
            let size = StoredSize {
                limits: PackageLimits { sdk_overhead_bytes: 137, ..limits(maximum) },
                repository_id: "repository",
            };
            let encoded = measure_catalog(leaf.clone(), &size).unwrap();
            assert_eq!(encoded.is_some(), fits);
            if let Some(encoded) = encoded { assert_eq!(encoded.bytes, bytes); }
        }
        for maximum in [8191, 8192, 8193] {
            let size = StoredSize {
                limits: PackageLimits { sdk_overhead_bytes: 137, ..limits(maximum) },
                repository_id: "repository",
            };
            let plain = size.pack_capacity("pack-object").unwrap();
            assert!(size.require("pack-object", wire::ObjectRole::Pack, plain).is_ok());
            assert_eq!(size.require("pack-object", wire::ObjectRole::Pack, plain + 1).unwrap_err().kind, ErrorKind::FileTooLarge);
        }
        let mut capabilities = fake::capabilities(false);
        capabilities.max_stored_bytes = Some(u64::MAX);
        capabilities.sdk_overhead_bytes = u64::MAX;
        assert_eq!(PackageLimits::from_capabilities(&capabilities).unwrap_err().kind, ErrorKind::FileTooLarge);
    }

    #[test]
    fn c_invalid_catalog_is_not_misclassified_as_an_oversized_leaf() {
        let mut document = wire::CatalogDocument::leaf(wire::CatalogKind::Assets, vec![], vec![]).unwrap();
        document.schema = "invalid".into();
        let size = StoredSize { limits: limits(1024 * 1024), repository_id: "repository" };
        assert!(matches!(measure_catalog(document, &size), Err(error) if error.kind == ErrorKind::Corrupt));
        let empty = catalog_batches::<wire::CatalogEntryFragment>(&[], &size, |entries| {
            wire::CatalogDocument::leaf(wire::CatalogKind::Assets, entries.to_vec(), vec![]).map_err(corrupt)
        }).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].bytes, empty[0].document.encode(wire::MAX_METADATA_BYTES).unwrap());
    }

    #[test]
    fn resealing_a_pack_tolerates_release_while_waiting_for_cpu_only_with_matching_proof() {
        runtime().block_on(async {
            for case in ["released", "changed-plaintext", "unreleased", "unregistered", "damaged"] {
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let (capture, _) = captured(root.path(), "racing-capture", 1, b"record", b"asset");
                let mut journal = journal(&root.path().join("job"), "racing-job", &capture);
                let mut cache = PackageCache::open(&root.path().join("cache")).unwrap();
                let mut evidence = ObjectEvidence::open(root.path(), journal.connection_id()).unwrap();
                let path = root.path().join("plaintext");
                let body = b"first immutable snapshot";
                fs::write(&path, body).unwrap();
                let key = derive_key(&[31; 32], "format-repository", "data").unwrap();
                let cancel = Cancellation::default();
                let object_id = "pack-racing";
                seal_plain_object(&path, body.len() as u64, hash(body), object_id.into(), ObjectRole::Pack,
                    "format-repository", &[31; 32], &key, limits(128 * 1024), 0,
                    &mut cache, &mut evidence, &mut journal, &provider, &repository, &cancel)
                    .await.unwrap().unwrap();
                let mut uploader = journal.preparation_handle(Arc::new(std::sync::atomic::AtomicUsize::new(0))).unwrap();
                let intent = uploader.record(object_id).unwrap().unwrap().intent;
                if case != "unregistered" {
                    uploader.cover("inventory-racing", &[super::super::journal::WaveMember {
                        object_id: object_id.into(), plaintext_length: body.len() as u64,
                        plaintext_sha256: hex::encode(hash(body)),
                    }]).unwrap();
                }
                let requested = if case == "changed-plaintext" { b"other immutable snapshot" } else { body };
                let cpu = cpu_permit().await.unwrap();
                let repeated = seal_plain_object(&path, requested.len() as u64, hash(requested), object_id.into(), ObjectRole::Pack,
                    "format-repository", &[31; 32], &key, limits(128 * 1024), 0,
                    &mut cache, &mut evidence, &mut journal, &provider, &repository, &cancel);
                tokio::pin!(repeated);
                assert!(futures::poll!(&mut repeated).is_pending(), "{case}: revalidation must wait for CPU");
                let spool = uploader.spool_path(object_id);
                if case == "unreleased" {
                    fs::remove_file(&spool).unwrap();
                } else if case == "damaged" {
                    let mut bytes = fs::read(&spool).unwrap();
                    bytes[0] ^= 1;
                    fs::write(&spool, bytes).unwrap();
                } else {
                    let source = super::super::transfer::SpoolSource::verified(&spool, intent.byte_length, &intent.sha256).unwrap();
                    uploader.attempted(object_id, None).unwrap();
                    let receipt = provider.create_object(&repository, &intent, &source, None, &cancel).await.unwrap();
                    uploader.complete(&intent, &repository, &receipt).unwrap();
                    assert!(!spool.exists());
                }
                drop(cpu);
                let result = repeated.await;
                if case == "released" {
                    let Some(SealedObject::Pending { ciphertext_sha256, .. }) = result.unwrap() else {
                        panic!("released pack must retain its upload identity");
                    };
                    assert_eq!(ciphertext_sha256, intent.sha256);
                    assert_eq!(provider.upload_attempts(object_id), 1);
                    assert_eq!(provider.read_count(), 0, "a durable matching receipt needs no download");
                } else {
                    assert_eq!(result.err().expect(case).kind, ErrorKind::Corrupt, "{case}");
                }
            }
        });
    }

    #[test]
    fn stable_job_object_id_rejects_changed_plaintext_after_releasing_its_ciphertext() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "stable-capture", 1, b"record", b"asset");
            let mut journal = journal(&root.path().join("stable-job"), "stable-job", &capture);
            let mut cache = PackageCache::open(&root.path().join("stable-cache")).unwrap();
            let mut evidence = ObjectEvidence::open(root.path(), journal.connection_id()).unwrap();
            let build = root.path().join("stable-build");
            fs::create_dir(&build).unwrap();
            let first_path = build.join("first");
            let second_path = build.join("second");
            fs::write(&first_path, b"first immutable snapshot").unwrap();
            fs::write(&second_path, b"other immutable snapshot").unwrap();
            let key = derive_key(&[31; 32], "format-repository", "metadata").unwrap();
            let first = upload_plain_object(
                &first_path,
                24,
                hash(b"first immutable snapshot"),
                "snapshot-stable".into(),
                ObjectRole::SyncState,
                "format-repository",
                &[31; 32],
                &key,
                limits(128 * 1024),
                &mut cache,
                &mut evidence,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert!(!journal.spool_path(&first.object_id).exists());
            let before = provider.state.lock().unwrap().objects.len();
            assert!(upload_plain_object(
                &second_path,
                24,
                hash(b"other immutable snapshot"),
                "snapshot-stable".into(),
                ObjectRole::SyncState,
                "format-repository",
                &[31; 32],
                &key,
                limits(128 * 1024),
                &mut cache,
                &mut evidence,
                &mut journal,
                &provider,
                &repository,
                &Cancellation::default(),
            )
            .await
            .is_err());
            assert_eq!(provider.state.lock().unwrap().objects.len(), before);
        });
    }

    /// Uploads one object and hands back everything needed to revalidate it
    /// the way a later publication would.
    async fn inherited_object(
        root: &Path,
        provider: &FakeProvider,
        repository: &RepositoryHandle,
        capture: &CapturedSnapshot,
    ) -> (RemoteObject, PackageCache, ObjectEvidence, TransferJournal) {
        let mut journal = journal(&root.join("evidence-job"), "evidence-job", capture);
        let mut cache = PackageCache::open(&root.join("evidence-cache")).unwrap();
        let mut evidence = ObjectEvidence::open(root, journal.connection_id()).unwrap();
        let build = root.join("evidence-build");
        fs::create_dir_all(&build).unwrap();
        let path = build.join("body");
        fs::write(&path, b"inherited snapshot body").unwrap();
        let key = derive_key(&[41; 32], "format-repository", "metadata").unwrap();
        let object = upload_plain_object(
            &path,
            23,
            hash(b"inherited snapshot body"),
            "snapshot-inherited".into(),
            ObjectRole::SyncState,
            "format-repository",
            &[41; 32],
            &key,
            limits(128 * 1024),
            &mut cache,
            &mut evidence,
            &mut journal,
            provider,
            repository,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        (object, cache, evidence, journal)
    }

    /// The package cache can be deleted or rebuilt from a parent's catalog at
    /// any time, so a reference already found missing has to be remembered
    /// somewhere the cache cannot speak for.
    #[test]
    fn a_rebuilt_cache_does_not_restore_trust_in_a_reference_found_missing() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "evidence-capture", 1, b"record", b"asset");
            let (object, mut cache, mut evidence, _journal) =
                inherited_object(root.path(), &provider, &repository, &capture).await;
            let mut next = journal(&root.path().join("evidence-next"), "evidence-next", &capture);
            provider.forget(&object.receipt.locator.object);
            assert!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().is_none());
            assert!(cache.object(
                "format-repository", &repository, &object.object_id, &object.plaintext_sha256,
            ).unwrap().is_none());
            assert!(evidence.known(&ObjectEvidence::identity(&object, &repository).unwrap()).unwrap());

            cache.put_object(&repository, &object).unwrap();
            let reads = provider.read_count();
            assert!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().is_none());
            assert_eq!(provider.read_count(), reads);
        });
    }

    /// The record answers for one exact remote object, so the object answering
    /// for itself again has to end it.
    #[test]
    fn an_object_that_answers_again_stops_answering_from_its_recorded_failure() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "evidence-capture", 1, b"record", b"asset");
            let (object, mut cache, mut evidence, mut journal) =
                inherited_object(root.path(), &provider, &repository, &capture).await;
            let stored = provider.contents(&object.receipt.locator.object).unwrap();
            provider.forget(&object.receipt.locator.object);
            // Its ciphertext was released, so this publication cannot send it again.
            assert_eq!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut journal, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap_err().kind, ErrorKind::NotFound);
            assert!(evidence.known(&ObjectEvidence::identity(&object, &repository).unwrap()).unwrap());

            provider.seed(&object.receipt.locator.object, object.role, stored);
            let answered = revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut journal, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().unwrap();
            assert_eq!(answered.receipt.locator, object.receipt.locator);
            assert_eq!(answered.ciphertext_sha256, object.ciphertext_sha256);
            assert!(!evidence.known(&ObjectEvidence::identity(&object, &repository).unwrap()).unwrap());

            let reads = provider.read_count();
            assert!(revalidate_cached_object(
                &answered, &mut cache, &mut evidence, &mut journal, &[41; 32], &provider,
                &repository, &Cancellation::default(),
            ).await.unwrap().is_some());
            assert!(provider.read_count() > reads);
        });
    }
    /// The record answers for references this attempt inherited. An object
    /// this job uploaded itself is still held in its own spool, and a crash
    /// between a repair and its clear must not make the job publish it twice.
    #[test]
    fn a_record_left_by_a_crash_does_not_republish_this_job_s_own_object() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "evidence-capture", 1, b"record", b"asset");
            let (object, mut cache, mut evidence, mut journal) =
                inherited_object(root.path(), &provider, &repository, &capture).await;
            let contacted = ObjectEvidence::identity(&object, &repository).unwrap();
            evidence.record(&contacted, &object.object_id, UnusableReason::Missing).unwrap();
            assert!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut journal, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().is_some());
            assert_eq!(provider.upload_attempts(&object.object_id), 1);
            assert_eq!(provider.reconcile_attempts(&object.object_id), 0);
            assert!(!evidence.known(&contacted).unwrap());
        });
    }

    /// Bytes that fail their own verification are as unusable as bytes that
    /// are not there, and the reason is kept apart because they are not the
    /// same finding.
    #[test]
    fn bytes_that_fail_verification_are_remembered_as_damaged() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "evidence-capture", 1, b"record", b"asset");
            let (object, mut cache, mut evidence, _journal) =
                inherited_object(root.path(), &provider, &repository, &capture).await;
            let mut next = journal(&root.path().join("evidence-next"), "evidence-next", &capture);
            let mut damaged = provider.state.lock().unwrap().objects[&object.receipt.locator.object].0.clone();
            let last = damaged.len() - 1;
            damaged[last] ^= 0xff;
            provider.seed(&object.receipt.locator.object, object.role, damaged);
            assert_eq!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap_err().kind, ErrorKind::Corrupt);
            let store = ConnectionStore::open(root.path()).unwrap();
            assert_eq!(
                store.unusable_object("connection", &super::super::reachability::object_identity(&object, &repository).unwrap()).unwrap(),
                Some(UnusableReason::Damaged),
            );

            let reads = provider.read_count();
            assert!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().is_none());
            assert_eq!(provider.read_count(), reads);
        });
    }

    #[test]
    fn an_answer_about_the_provider_is_not_an_answer_about_the_object() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let (capture, _) = captured(root.path(), "evidence-capture", 1, b"record", b"asset");
            let (object, mut cache, mut evidence, _journal) =
                inherited_object(root.path(), &provider, &repository, &capture).await;
            let mut next = journal(&root.path().join("evidence-next"), "evidence-next", &capture);
            for kind in [ErrorKind::Transient, ErrorKind::Unauthorized, ErrorKind::RateLimited] {
                provider.fail_read(&object.receipt.locator.object, kind);
                assert_eq!(revalidate_cached_object(
                    &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider,
                    &repository, &Cancellation::default(),
                ).await.unwrap_err().kind, kind);
                assert!(!evidence.known(&ObjectEvidence::identity(&object, &repository).unwrap()).unwrap());
            }
            assert!(revalidate_cached_object(
                &object, &mut cache, &mut evidence, &mut next, &[41; 32], &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap().is_some());
        });
    }

    #[test]
    fn small_provider_limit_splits_large_content_and_every_ciphertext_fits() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let mut state = 0x9e37_79b9u32;
            let record: Vec<u8> = (0..40_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            let (capture, _) = captured(root.path(), "small-limit", 1, &record, b"asset");
            let snapshot_metadata = metadata("bounded", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let completed = package_and_upload(
                capture,
                Vec::new(),
                root.path(),
                &root.path().join("cache"),
                snapshot_metadata,
                &[4; 32],
                limits(8 * 1024),
                None,
                &mut journal,
                &provider,
                &repository,
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let state = provider.state.lock().unwrap();
            assert!(state
                .objects
                .values()
                .all(|(bytes, _)| bytes.len() <= 8 * 1024));
            assert!(
                state
                    .objects
                    .keys()
                    .filter(|id| id.starts_with("pack-"))
                    .count()
                    >= 6
            );
            assert!(state
                .objects
                .contains_key(&completed.reference.receipt.locator.object));
            drop(state);
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &root.path().join("bounded-restore"),
                &[4; 32],
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(restored.record_body(0), record);
        });
    }

    #[test]
    fn provider_limit_rejects_a_catalog_entry_that_cannot_fit_alone() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let directory = root
                .path()
                .join("external-storage")
                .join("captures")
                .join("oversized-entry");
            let external = root.path().join("external-storage");
            let mut catalog = crate::external_storage::capture::CaptureCatalog::create(
                &directory, &external, None,
            )
            .unwrap();
            let identity = CaptureIdentity {
                store_id: "store".into(),
                library_epoch: "epoch".into(),
                generation: "generation".into(),
                selection_epoch: "selection".into(),
                revision: 1,
            };
            catalog.begin(&identity, None).unwrap();
            catalog.record(&"k".repeat(16 * 1024), b"record").unwrap();
            catalog.finish().unwrap();
            let capture = CapturedSnapshot {
                id: "oversized-entry".into(),
                identity,
                catalog,
                projected_records: 1,
                shared: false,
            };
            let snapshot_metadata = metadata("oversized-entry", &capture);
            let mut journal = journal(&root.path().join("job"), "job", &capture);
            let error = package_and_upload(
                capture,
                Vec::new(),
                root.path(),
                &root.path().join("cache"),
                snapshot_metadata,
                &[4; 32],
                limits(8 * 1024),
                None,
                &mut journal,
                &FakeProvider::new(false),
                &fake::repository(),
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::FileTooLarge);
        });
    }

    /// What a publication does before it can build its first pack: every
    /// source the capture names is listed, which is what stands in front of
    /// the first upload at this library size.
    #[test]
    #[ignore = "explicit planning-pass measurement"]
    fn measures_the_source_pass_that_precedes_the_first_pack() {
        let root = tempfile::tempdir().unwrap();
        let capture = captured_asset_inventory(root.path(), "planning", 1, 300, 0, 100_000);
        let started = std::time::Instant::now();
        let (records, assets) = capture_sources(&capture).unwrap();
        eprintln!(
            "records={} assets={} source_pass_ms={}",
            records.len(),
            assets.len(),
            started.elapsed().as_millis(),
        );
    }

    #[test]
    #[ignore = "explicit large-inventory measurement"]
    fn measures_hundred_thousand_asset_package_and_restore() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [27; 32];
            let first_capture =
                captured_asset_inventory(root.path(), "inventory-1", 1, 300, 0, 100_000);
            let first_metadata = metadata("inventory-snapshot-1", &first_capture);
            let directory = root.path().join("inventory-job-1");
            let mut first_journal = journal(&directory, "inventory-job-1", &first_capture);
            let (peak, stop, sampler) = peak_spool_sampler(directory.clone());
            let (files, stop_files, files_sampler) =
                peak_sampler(root.path().join("inventory-cache").join("build"), build_files);
            let started = std::time::Instant::now();
            let completed = package_and_upload(
                first_capture,
                Vec::new(),
                root.path(),
                &root.path().join("inventory-cache"),
                first_metadata,
                &key,
                limits(512 * 1024),
                None,
                &mut first_journal,
                &provider,
                &repository,
                &PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let package_elapsed = started.elapsed();
            let first_upload_ms = provider.first_upload()
                .map(|at| at.duration_since(started).as_millis()).unwrap_or_default();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            sampler.join().unwrap();
            stop_files.store(true, std::sync::atomic::Ordering::Relaxed);
            files_sampler.join().unwrap();
            let (remote_objects, packs, catalogs) = {
                let state = provider.state.lock().unwrap();
                (
                    state.objects.len(),
                    state
                        .objects
                        .keys()
                        .filter(|id| id.starts_with("pack-"))
                        .count(),
                    state
                        .objects
                        .keys()
                        .filter(|id| id.starts_with("catalog-"))
                        .count(),
                )
            };
            let restore_started = std::time::Instant::now();
            let restore_root = root.path().join("inventory-restore");
            snapshot_restore::reset_test_read_counts(&restore_root);
            let restored = snapshot_restore::download_snapshot(
                &completed.reference,
                &restore_root,
                &key,
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let reads = snapshot_restore::take_test_read_counts(&restore_root);
            let (network_reads, pack_reads) = (reads.network, reads.packs);
            assert_eq!(restored.records.len(), 300);
            assert_eq!(restored.objects.len(), 100_000);
            assert!(provider
                .state
                .lock()
                .unwrap()
                .objects
                .values()
                .all(|(bytes, _)| bytes.len() <= 512 * 1024));
            eprintln!(
                "assets=100000 remote_objects={} packs={} catalogs={} package_ms={} first_upload_ms={first_upload_ms} peak_spool_bytes={} peak_build_files={} restore_reads={} restore_pack_reads={} restore_ms={}",
                remote_objects,
                packs,
                catalogs,
                package_elapsed.as_millis(),
                peak.load(std::sync::atomic::Ordering::Relaxed),
                files.load(std::sync::atomic::Ordering::Relaxed),
                network_reads,
                pack_reads,
                restore_elapsed.as_millis(),
            );
        });
    }

    /// The case external reuse has to move: an asset catalog that changes on
    /// every publication, so every unchanged pack it still names has to be
    /// admitted again.
    #[test]
    #[ignore = "explicit growing-inventory measurement"]
    fn measures_publications_that_add_one_asset_to_a_large_inventory() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [31; 32];
            let cache = root.path().join("growing-cache");
            let mut steady = Vec::new();
            let mut packs = 0;
            let mut published: Option<CompletedSnapshot> = None;
            const REVISIONS: i64 = 20;
            for revision in 1..=REVISIONS {
                let capture_id = format!("growing-capture-{revision}");
                let job_id = format!("growing-job-{revision}");
                let capture = captured_asset_inventory(
                    root.path(),
                    &capture_id,
                    revision,
                    1,
                    1,
                    5_000 + revision as usize,
                );
                let snapshot_metadata = metadata(&format!("growing-snapshot-{revision}"), &capture);
                let mut transfer = journal(&root.path().join(&job_id), &job_id, &capture);
                let before = (provider.read_count(), provider.upload_count());
                let parent = published
                    .as_ref()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let started = std::time::Instant::now();
                let completed = package_and_upload(
                    capture,
                    Vec::new(),
                    root.path(),
                    &cache,
                    snapshot_metadata,
                    &key,
                    limits(512 * 1024),
                    parent.as_ref(),
                    &mut transfer,
                    &provider,
                    &repository,
                    &PhaseProgress::silent(),
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                let elapsed = started.elapsed().as_millis();
                // What a publication actually costs includes its own check,
                // which walks the published catalogs whether or not their
                // entries were admitted.
                let verify_before = provider.read_count();
                assert_eq!(
                    verify_publication(
                        &completed, root.path(), &cache, &mut transfer, &key, &provider,
                        &repository, &Cancellation::default(),
                    ).await.unwrap(),
                    PublicationReadiness::Verified,
                );
                let verified = provider.read_count() - verify_before;
                published = Some(completed);
                packs = provider.state.lock().unwrap().objects.keys()
                    .filter(|id| id.starts_with("pack-")).count();
                let added = provider.uploaded_ids()[before.1..].to_vec();
                let nodes = published.as_ref().map(|value: &CompletedSnapshot| {
                    value.referenced_objects.iter()
                        .filter(|object| object.role == ObjectRole::Catalog).count()
                }).unwrap_or_default();
                steady.push((
                    provider.read_count() - before.0 - verified,
                    provider.upload_count() - before.1,
                    added.iter().filter(|id| id.starts_with("catalog-")).count(),
                    elapsed,
                    verified,
                    nodes,
                ));
            }
            for (revision, (reads, uploads, objects, elapsed, verified, nodes)) in steady.iter().enumerate() {
                eprintln!(
                    "revision={} assets={} packs={packs} provider_reads={reads} check_reads={verified} provider_uploads={uploads} catalog_uploads={objects} catalog_nodes={nodes} publication_ms={elapsed}",
                    revision + 1,
                    5_001 + revision,
                );
            }

            // The same library from a cache that holds nothing about it, with
            // and without the published parent to rebuild from.
            let previous = published.as_ref().unwrap();
            let parent = parent_graph(previous, &repository).unwrap();
            for (name, parent) in [("rebuilt", Some(&parent)), ("cold", None)] {
                let capture_id = format!("growing-capture-{name}");
                let job_id = format!("growing-job-{name}");
                let capture = captured_asset_inventory(
                    root.path(), &capture_id, REVISIONS, 1, 1, 5_000 + REVISIONS as usize,
                );
                let snapshot_metadata = metadata(&format!("growing-snapshot-{name}"), &capture);
                let directory = root.path().join(&job_id);
                let mut transfer = journal(&directory, &job_id, &capture);
                let before = (provider.read_count(), provider.upload_count());
                let (peak, stop, sampler) = peak_spool_sampler(directory.clone());
                let (files, stop_files, files_sampler) = peak_sampler(
                    root.path().join(format!("{name}-cache")).join("build"),
                    build_files,
                );
                provider.reset_first_upload();
                let started = std::time::Instant::now();
                package_and_upload(
                    capture, Vec::new(), root.path(), &root.path().join(format!("{name}-cache")),
                    snapshot_metadata, &key, limits(512 * 1024), parent, &mut transfer,
                    &provider, &repository, &PhaseProgress::silent(), &Cancellation::default(),
                ).await.unwrap();
                let elapsed = started.elapsed().as_millis();
                let first_upload_ms = provider.first_upload()
                    .map(|at| at.duration_since(started).as_millis()).unwrap_or_default();
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                sampler.join().unwrap();
                stop_files.store(true, std::sync::atomic::Ordering::Relaxed);
                files_sampler.join().unwrap();
                let stored: u64 = provider.state.lock().unwrap().objects.values()
                    .map(|(bytes, _)| bytes.len() as u64).sum();
                eprintln!(
                    "{name} assets={} provider_reads={} provider_uploads={} publication_ms={elapsed} first_upload_ms={first_upload_ms} peak_spool_bytes={} peak_build_files={} held_spool_bytes={} stored_bytes={stored}",
                    5_000 + REVISIONS,
                    provider.read_count() - before.0,
                    provider.upload_count() - before.1,
                    peak.load(std::sync::atomic::Ordering::Relaxed),
                    files.load(std::sync::atomic::Ordering::Relaxed),
                    super::super::journal::spool_bytes(&directory),
                );
            }
        });
    }

    /// What a warm incremental publication costs on the record axis, which is
    /// the axis the metadata budget is stated on. An asset-scaled fixture
    /// measures the dependency pass instead.
    #[test]
    #[ignore = "explicit large record library measurement"]
    fn measures_a_warm_incremental_publication_of_a_large_record_library() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [59; 32];
            let cache = root.path().join("library-cache");
            const RECORDS: usize = 130_000;
            const ASSETS: usize = 64;
            let mut published: Option<CompletedSnapshot> = None;
            for (revision, edited, name) in [(1i64, 0usize, "cold"), (2, 4, "warm")] {
                let capture_id = format!("library-capture-{revision}");
                let job_id = format!("library-job-{revision}");
                let built = std::time::Instant::now();
                let capture = captured_record_window(
                    root.path(),
                    &capture_id,
                    revision,
                    RECORDS,
                    edited,
                    0,
                    ASSETS,
                );
                let capture_ms = built.elapsed().as_millis();
                let snapshot_metadata =
                    metadata(&format!("library-snapshot-{revision}"), &capture);
                let directory = root.path().join(&job_id);
                let mut transfer = journal(&directory, &job_id, &capture);
                let parent = published
                    .as_ref()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let before = (provider.read_count(), provider.upload_count());
                provider.reset_first_upload();
                let started = std::time::Instant::now();
                let completed = package_and_upload(
                    capture,
                    Vec::new(),
                    root.path(),
                    &cache,
                    snapshot_metadata,
                    &key,
                    limits(512 * 1024),
                    parent.as_ref(),
                    &mut transfer,
                    &provider,
                    &repository,
                    &PhaseProgress::silent(),
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                let elapsed = started.elapsed().as_millis();
                let first_upload_ms = provider
                    .first_upload()
                    .map(|at| at.duration_since(started).as_millis())
                    .unwrap_or_default();
                let added = provider.uploaded_ids()[before.1..].to_vec();
                let (pack_bytes, catalog_bytes) = {
                    let state = provider.state.lock().unwrap();
                    let bytes = |prefix: &str| -> u64 {
                        added
                            .iter()
                            .filter(|id| id.starts_with(prefix))
                            .filter_map(|id| state.objects.get(id.as_str()))
                            .map(|(body, _)| body.len() as u64)
                            .sum()
                    };
                    (bytes("pack-"), bytes("catalog-"))
                };
                eprintln!(
                    "{name} records={RECORDS} assets={ASSETS} edited={edited} capture_ms={capture_ms} publication_ms={elapsed} first_upload_ms={first_upload_ms} provider_reads={} provider_uploads={} pack_uploads={} pack_bytes={pack_bytes} catalog_uploads={} catalog_bytes={catalog_bytes}",
                    provider.read_count() - before.0,
                    provider.upload_count() - before.1,
                    added.iter().filter(|id| id.starts_with("pack-")).count(),
                    added.iter().filter(|id| id.starts_with("catalog-")).count(),
                );
                published = Some(completed);
            }
        });
    }

    /// Records that grow rather than change: the case where repackaging an
    /// entry costs the whole entry instead of the part of it that is new.
    #[test]
    #[ignore = "explicit growing-record measurement"]
    fn measures_publications_that_append_to_a_large_record() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [53; 32];
            let cancel = Cancellation::default();
            let cache = root.path().join("appending-cache");
            // Content a compressor cannot fold away, so what a publication
            // stores is what its chunks actually cost.
            let mut seed = 0x243f_6a88u32;
            let mut noise = move |count: usize| -> Vec<u8> {
                (0..count)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 24) as u8
                    })
                    .collect()
            };
            let mut record = noise(4_000_000);
            let mut published: Option<CompletedSnapshot> = None;
            for revision in 1..=5i64 {
                let name = format!("appending-{revision}");
                let (capture, _) = captured(root.path(), &name, revision, &record, b"asset");
                let meta = metadata(&name, &capture);
                let mut transfer = journal(&root.path().join(&name), &name, &capture);
                let parent = published
                    .as_ref()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let before = {
                    let state = provider.state.lock().unwrap();
                    state.objects.values().map(|(bytes, _)| bytes.len()).sum::<usize>()
                };
                let started = std::time::Instant::now();
                let completed = package_and_upload(
                    capture, Vec::new(), root.path(), &cache, meta, &key,
                    limits(800 * 1024), parent.as_ref(), &mut transfer, &provider,
                    &repository, &PhaseProgress::silent(), &cancel,
                ).await.unwrap();
                let elapsed = started.elapsed().as_millis();
                let after = {
                    let state = provider.state.lock().unwrap();
                    state.objects.values().map(|(bytes, _)| bytes.len()).sum::<usize>()
                };
                eprintln!(
                    "revision={revision} record_bytes={} stored_bytes_added={} publication_ms={elapsed}",
                    record.len(),
                    after - before,
                );
                published = Some(completed);
                record.extend(noise(64));
            }
        });
    }

    /// Section 11.3's question, answered on the aged repository rather than on
    /// a fresh one: after a long sequence of edits, how much of a latest-state
    /// restore's request budget is small record packs? Same-key, rotating-key
    /// and mass-deletion edits age a library differently, so each is measured
    /// on its own repository.
    #[test]
    #[ignore = "explicit aged-repository measurement"]
    fn measures_an_aged_repository_across_edit_shapes() {
        runtime().block_on(async {
            const RECORDS: usize = 512;
            const ASSETS: usize = 64;
            const PUBLICATIONS: i64 = 120;
            const WINDOW: usize = 4;
            // The fourth shape ages exactly like the second and then lets the
            // publications that follow spend the maintenance portion, so the
            // two rows differ only in section 11.3.
            for shape in ["same-key", "rotating-key", "mass-deletion", "rotating-key-11.3"] {
                let coalescing = shape == "rotating-key-11.3";
                let publications = if coalescing { PUBLICATIONS + 10 } else { PUBLICATIONS };
                let root = tempfile::tempdir().unwrap();
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let key = [43; 32];
                let cache = root.path().join("aged-cache");
                let started = std::time::Instant::now();
                let mut latest = None;
                for revision in 1..=publications {
                    let step = (revision - 1) as usize;
                    let (records, edited, offset) = match shape {
                        "same-key" => (RECORDS, WINDOW, 0),
                        "mass-deletion" => (RECORDS - step * WINDOW, 0, 0),
                        _ => (RECORDS, WINDOW, step * WINDOW),
                    };
                    let capture = captured_record_window(
                        root.path(), &format!("aged-capture-{revision}"), revision,
                        records, edited, offset, ASSETS);
                    let snapshot_metadata = metadata(&format!("aged-snapshot-{revision}"), &capture);
                    let job = format!("aged-job-{revision}");
                    let mut transfer = journal(&root.path().join(&job), &job, &capture);
                    let parent = latest.as_ref()
                        .map(|previous| parent_graph(previous, &repository))
                        .transpose().unwrap();
                    latest = Some(package_and_upload(
                        capture, Vec::new(), root.path(), &cache, snapshot_metadata, &key,
                        limits(128 * 1024)
                            .with_maintenance(coalescing && revision > PUBLICATIONS),
                        parent.as_ref(), &mut transfer, &provider,
                        &repository, &PhaseProgress::silent(), &Cancellation::default(),
                    ).await.unwrap());
                }
                let publication_ms = started.elapsed().as_millis();
                let (remote_objects, retained_bytes) = {
                    let state = provider.state.lock().unwrap();
                    (
                        state.objects.len(),
                        state.objects.values().map(|(bytes, _)| bytes.len() as u64).sum::<u64>(),
                    )
                };
                let restore_root = root.path().join("aged-restore");
                snapshot_restore::reset_test_read_counts(&restore_root);
                let restore_started = std::time::Instant::now();
                let restored = snapshot_restore::download_snapshot(
                    &latest.unwrap().reference, &restore_root, &key, None,
                    snapshot_restore::SourceTrust::Downloaded, &provider, &repository,
                    &crate::external_storage::phase_progress::PhaseProgress::silent(),
                    &Cancellation::default(),
                ).await.unwrap();
                let restore_ms = restore_started.elapsed().as_millis();
                let reads = snapshot_restore::take_test_read_counts(&restore_root);
                let live_bytes = restored.records.iter().map(|record| record.byte_length)
                    .chain(restored.objects.iter().map(|object| object.byte_length))
                    .sum::<u64>();
                eprintln!(
                    "shape={shape} records={} publications={publications} live_records={} live_objects={} \
                     live_bytes={live_bytes} remote_objects={remote_objects} retained_bytes={retained_bytes} \
                     latest_requests={} downloaded_bytes={} live_packs={} small_live_packs={} pack_bytes={} \
                     catalog_and_snapshot_bytes={} publication_ms={publication_ms} restore_ms={restore_ms}",
                    RECORDS, restored.records.len(), restored.objects.len(),
                    reads.network, reads.network_bytes, reads.packs, reads.small_packs,
                    reads.pack_bytes, reads.network_bytes - reads.pack_bytes,
                );
            }
        });
    }

    #[test]
    #[ignore = "explicit publication-history measurement"]
    fn measures_three_hundred_small_publications_and_latest_restore() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [29; 32];
            let cache = root.path().join("publication-cache");
            let started = std::time::Instant::now();
            let mut asset_catalog_id = None;
            let mut latest = None;
            let mut steady_reads = 0;
            let mut steady_uploads = 0;
            for revision in 1..=300i64 {
                let capture_id = format!("publication-capture-{revision}");
                let snapshot_id = format!("publication-snapshot-{revision}");
                let job_id = format!("publication-job-{revision}");
                let capture = captured_asset_inventory(
                    root.path(),
                    &capture_id,
                    revision,
                    1,
                    1,
                    1_000,
                );
                let snapshot_metadata = metadata(&snapshot_id, &capture);
                let mut transfer = journal(&root.path().join(&job_id), &job_id, &capture);
                let before = provider.state.lock().unwrap().objects.len();
                let before_reads = provider.read_count();
                let before_uploads = provider.upload_count();
                let parent = latest
                    .as_ref()
                    .map(|previous| parent_graph(previous, &repository))
                    .transpose()
                    .unwrap();
                let completed = package_and_upload(
                    capture,
                    Vec::new(),
                    root.path(),
                    &cache,
                    snapshot_metadata,
                    &key,
                    limits(128 * 1024),
                    parent.as_ref(),
                    &mut transfer,
                    &provider,
                    &repository,
                    &PhaseProgress::silent(),
                    &Cancellation::default(),
                )
                .await
                .unwrap();
                if let Some(expected) = &asset_catalog_id {
                    // The asset side is unchanged, so a publication adds only
                    // what its own revision produced: one record pack, the
                    // record catalog naming it, the snapshot, and the three
                    // inventory pages that register the job's objects.
                    assert_eq!(&completed.asset_catalog.object_id, expected);
                    assert_eq!(provider.state.lock().unwrap().objects.len() - before, 6);
                    steady_reads = provider.read_count() - before_reads;
                    steady_uploads = provider.upload_count() - before_uploads;
                } else {
                    asset_catalog_id = Some(completed.asset_catalog.object_id.clone());
                }
                latest = Some(completed);
            }
            let publication_elapsed = started.elapsed();
            let physical_remote_objects = provider.state.lock().unwrap().objects.len();
            let restore_root = root.path().join("publication-restore");
            snapshot_restore::reset_test_read_counts(&restore_root);
            let restore_started = std::time::Instant::now();
            let latest = latest.unwrap();
            let restored = snapshot_restore::download_snapshot(
                &latest.reference,
                &restore_root,
                &key,
                None,
                snapshot_restore::SourceTrust::Downloaded, &provider,
                &repository,
                &crate::external_storage::phase_progress::PhaseProgress::silent(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let restore_elapsed = restore_started.elapsed();
            let reads = snapshot_restore::take_test_read_counts(&restore_root);
            let (network_reads, pack_reads) = (reads.network, reads.packs);
            assert_eq!(restored.records.len(), 1);
            assert_eq!(restored.objects.len(), 1_000);
            assert!(network_reads < 50);
            assert!(pack_reads < 20);
            eprintln!(
                "assets=1000 publications=300 physical_remote_objects={} steady_provider_reads={} steady_provider_uploads={} publication_ms={} latest_restore_reads={} latest_reachable_packs={} latest_restore_ms={}",
                physical_remote_objects,
                steady_reads,
                steady_uploads,
                publication_elapsed.as_millis(),
                network_reads,
                pack_reads,
                restore_elapsed.as_millis(),
            );
        });
    }
    #[test]
    #[ignore = "bounded synthetic section request and body-byte measurement"]
    fn measures_fresh_and_inherited_section_closure_requests_and_body_bytes() {
        fn closure(completed: &CompletedSnapshot, id: &str, cache: &Path,
            repository: &RepositoryHandle) -> Vec<RemoteObject> {
            let root = RemoteObject::from_stored(&completed.sections[id].entries_root, repository).unwrap();
            let identity = ObjectEvidence::identity(&root, repository).unwrap();
            let objects = PackageCache::open(cache).unwrap()
                .graph_of(&completed.repository_id, repository, &identity).unwrap();
            assert!(objects.iter().any(|object| object.role == ObjectRole::Catalog));
            assert!(objects.iter().any(|object| object.role == ObjectRole::Pack));
            for object in &objects {
                assert!(completed.referenced_objects.iter().any(|referenced|
                    referenced.stored(repository).unwrap() == object.stored(repository).unwrap()));
            }
            objects
        }
        fn counts(provider: &FakeProvider, objects: &[RemoteObject]) -> (usize, usize) {
            let requests = objects.iter().map(|object|
                provider.read_attempts(&object.receipt.locator.object)).sum();
            let state = provider.state.lock().unwrap();
            let bytes = objects.iter().map(|object|
                state.body_bytes.get(&object.receipt.locator.object).copied().unwrap_or(0)).sum();
            (requests, bytes)
        }
        runtime().block_on(async {
            for conditional in [true, false] {
                for fault in ["none", "missing-child", "corrupt-child", "post-upload-corruption"] {
                    let root = tempfile::tempdir().unwrap();
                    let cache = root.path().join("cache");
                    let spool = root.path().join("sections");
                    let provider = FakeProvider::new(false);
                    provider.state.lock().unwrap().ignore_unchanged = !conditional;
                    let repository = fake::repository();
                    let key = [71; 32];
                    let cancel = Cancellation::default();
                    let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
                    let mut meta = metadata("first", &capture);
                    meta.purpose = SnapshotPurpose::SyncState { epoch: "epoch".into(),
                        generation: Sequence::from(1u64), parent_sections: BTreeMap::new() };
                    let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
                    let first = package_and_upload(capture,
                        vec![section(&spool, SectionKind::Hypa, "first", 1),
                            section(&spool, SectionKind::LocalPlugins, "first", 1)],
                        root.path(), &cache, meta, &key, limits(128 * 1024), None,
                        &mut first_journal, &provider, &repository, &PhaseProgress::silent(), &cancel)
                        .await.unwrap();
                    assert_eq!(verify_publication(&first, root.path(), &cache, &mut first_journal,
                        &key, &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
                    let inherited = closure(&first, "local-plugins", &cache, &repository);
                    let inherited_before = counts(&provider, &inherited);
                    let parent = parent_graph(&first, &repository).unwrap();
                    let reads_before = provider.read_count();
                    let bytes_before = provider.state.lock().unwrap().body_bytes.clone();
                    let (capture, _) = captured(root.path(), "second", 2, b"record", b"asset");
                    let mut meta = metadata("second", &capture);
                    meta.purpose = SnapshotPurpose::SyncState { epoch: "epoch".into(),
                        generation: Sequence::from(2u64), parent_sections: first.sections.clone() };
                    let mut transfer = journal(&root.path().join("second-job"), "second-job", &capture);
                    let second = package_and_upload(capture,
                        vec![section(&spool, SectionKind::Hypa, "second", 2)], root.path(), &cache,
                        meta, &key, limits(128 * 1024), Some(&parent), &mut transfer, &provider,
                        &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
                    let fresh = closure(&second, "hypa", &cache, &repository);
                    assert_eq!(second.sections["local-plugins"], first.sections["local-plugins"]);
                    assert_eq!(provider.read_count(), reads_before);
                    assert_eq!(provider.state.lock().unwrap().body_bytes, bytes_before);
                    assert_eq!(counts(&provider, &fresh), (0, 0));
                    assert_eq!(counts(&provider, &inherited), inherited_before);
                    if fault != "none" {
                        let objects = if fault == "post-upload-corruption" { &fresh } else { &inherited };
                        let child = objects.iter().find(|object| object.role == ObjectRole::Pack).unwrap();
                        if fault == "missing-child" {
                            provider.forget(&child.receipt.locator.object);
                            assert_eq!(verify_publication(&second, root.path(), &cache, &mut transfer,
                                &key, &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Repackage);
                        } else {
                            provider.seed(&child.receipt.locator.object, ObjectRole::Pack,
                                vec![0; child.receipt.byte_length as usize]);
                            assert_eq!(verify_publication(&second, root.path(), &cache, &mut transfer,
                                &key, &provider, &repository, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
                        }
                        continue;
                    }
                    for pass in 1..=2 {
                        let before = [counts(&provider, &fresh), counts(&provider, &inherited)];
                        assert_eq!(verify_publication(&second, root.path(), &cache, &mut transfer,
                            &key, &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
                        for (index, (label, objects)) in [("fresh", &fresh), ("inherited", &inherited)].into_iter().enumerate() {
                            let after = counts(&provider, objects);
                            let requests = after.0 - before[index].0;
                            let body_bytes = after.1 - before[index].1;
                            let expected_bytes = if pass == 2 && conditional { 0 } else {
                                objects.iter().map(|object| object.receipt.byte_length as usize).sum()
                            };
                            assert_eq!(requests, objects.len());
                            assert_eq!(body_bytes, expected_bytes);
                            println!("section-closure provider={} phase=verify-{pass} kind={label} requests={requests} body_bytes={body_bytes}",
                                if conditional { "conditional" } else { "unconditional" });
                        }
                    }
                    println!("section-closure provider={} phase=packaging fresh_requests=0 fresh_body_bytes=0 inherited_requests=0 inherited_body_bytes=0",
                        if conditional { "conditional" } else { "unconditional" });
                }
            }
        });
    }

    #[test]
    fn one_attempt_reuses_authenticated_catalogs_and_checks_fresh_versions_without_duplicate_bodies() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cache = root.path().join("cache");
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let key = [7; 32];
            let cancel = Cancellation::default();
            let (capture, _) = captured(root.path(), "first", 1, b"record", b"asset");
            let meta = metadata("first", &capture);
            let mut first_journal = journal(&root.path().join("first-job"), "first-job", &capture);
            let first = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut first_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let before = provider.state.lock().unwrap().body_bytes.clone();
            let (capture, _) = captured(root.path(), "second", 2, b"changed record", b"asset");
            let meta = metadata("second", &capture);
            let mut second_journal = journal(&root.path().join("second-job"), "second-job", &capture);
            let second = package_and_upload(capture, vec![], root.path(), &cache, meta, &key,
                limits(128 * 1024), None, &mut second_journal, &provider, &repository, &PhaseProgress::silent(), &cancel).await.unwrap();
            let requests_before=second.referenced_objects.iter().map(|object|
                (object.receipt.locator.object.clone(),provider.read_attempts(&object.receipt.locator.object))).collect::<BTreeMap<_,_>>();
            assert_eq!(verify_publication(&second, root.path(), &cache, &mut second_journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
            let pack = second.referenced_objects.iter().find(|object| object.role == ObjectRole::Pack
                && first.referenced_objects.iter().any(|prior| prior.object_id == object.object_id)).unwrap();
            for object in [&second.asset_catalog, pack] {
                let name = &object.receipt.locator.object;
                let count = provider.state.lock().unwrap().body_bytes.get(name).copied().unwrap_or(0);
                let expected = if object.role == ObjectRole::Pack { 0 } else { object.receipt.byte_length as usize };
                assert_eq!(count - before.get(name).copied().unwrap_or(0), expected);
                assert!(provider.read_attempts(name)>requests_before[name]);
            }
            let bodies = provider.state.lock().unwrap().body_bytes.clone();
            assert_eq!(verify_publication(&second, root.path(), &cache, &mut second_journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
            assert_eq!(provider.state.lock().unwrap().body_bytes, bodies);
            provider.state.lock().unwrap().ignore_unchanged = true;
            assert_eq!(verify_publication(&second, root.path(), &cache, &mut second_journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Verified);
            let pack_name = &pack.receipt.locator.object;
            assert_eq!(provider.state.lock().unwrap().body_bytes[pack_name] - bodies.get(pack_name).copied().unwrap_or(0), pack.receipt.byte_length as usize);
            provider.seed(pack_name, ObjectRole::Pack, vec![0; pack.receipt.byte_length as usize]);
            assert_eq!(verify_publication(&second, root.path(), &cache, &mut second_journal, &key,
                &provider, &repository, &cancel).await.unwrap_err().kind, ErrorKind::Corrupt);
            provider.forget(&pack.receipt.locator.object);
            assert_eq!(verify_publication(&second, root.path(), &cache, &mut second_journal, &key,
                &provider, &repository, &cancel).await.unwrap(), PublicationReadiness::Repackage);
        });
    }

    include!("packaging_upload_tests.rs");
}
