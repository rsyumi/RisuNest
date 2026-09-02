use super::{
    device_registry::{
        issue_outgoing_unmeasured_completion_offer, outgoing_completion_lease_matches,
        outgoing_completion_receipt_matches, seal_outgoing_completion_lease, CompletionLane,
        CompletionLeaseId, CompletionSealStatus,
    },
    PeerSyncError,
};
use crate::trust_boundary::{is_link_like, is_lower_hex_256};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const LOGICAL_COMPLETION_PROOF_SCHEMA: &str = "risunest.peer-logical-completion-proof/v1";
const LOGICAL_COMPLETION_PROOF_DIRECTORY: &str = "logical-completion-proofs";
const MAX_LOGICAL_COMPLETION_PROOF_BYTES: usize = 6 * 1024 * 1024;
const MAX_LOGICAL_COMPLETION_OBJECTS: usize = 500_000;

#[derive(Clone, Debug)]
struct LogicalCompletionObject {
    index: usize,
    size: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct LogicalCompletionManifest {
    objects: BTreeMap<String, LogicalCompletionObject>,
}

impl LogicalCompletionManifest {
    pub(crate) fn new(objects: BTreeMap<String, u64>) -> Result<Self, PeerSyncError> {
        if objects.len() > MAX_LOGICAL_COMPLETION_OBJECTS {
            return invalid("logical completion manifest exceeds the object limit");
        }
        let mut total_bytes = 0_u64;
        let mut indexed = BTreeMap::new();
        for (index, (hash, size)) in objects.into_iter().enumerate() {
            if !is_lower_hex_256(&hash) {
                return invalid("logical completion manifest has an invalid object");
            }
            total_bytes = total_bytes.checked_add(size).ok_or_else(|| {
                PeerSyncError::Validation(
                    "logical completion manifest byte count overflow".to_owned(),
                )
            })?;
            indexed.insert(hash, LogicalCompletionObject { index, size });
        }
        Ok(Self { objects: indexed })
    }

    pub(crate) fn object_size(&self, hash: &str) -> Option<u64> {
        self.objects.get(hash).map(|object| object.size)
    }

    fn object(&self, hash: &str) -> Option<&LogicalCompletionObject> {
        self.objects.get(hash)
    }

    fn object_count(&self) -> usize {
        self.objects.len()
    }

    fn object_sizes(&self) -> impl Iterator<Item = u64> + '_ {
        self.objects.values().map(|object| object.size)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IssuedLogicalObject {
    device_id: String,
    lane: &'static str,
    lease_id: String,
    manifest_id: String,
    object: String,
    index: usize,
    size: u64,
}

#[derive(Default)]
pub(crate) struct OutgoingLogicalIssuedObjects {
    state: Mutex<OutgoingLogicalIssueState>,
}

#[derive(Default)]
struct OutgoingLogicalIssueState {
    active: BTreeSet<IssuedLogicalObject>,
    pending: BTreeSet<IssuedLogicalObject>,
}

fn tuple_has_active(
    state: &OutgoingLogicalIssueState,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> bool {
    state.active.iter().any(|entry| {
        entry.device_id == device_id
            && entry.lane == lane.as_str()
            && entry.lease_id == lease_id
            && entry.manifest_id == manifest_id
    })
}

fn tuple_has_pending(
    state: &OutgoingLogicalIssueState,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> bool {
    state.pending.iter().any(|entry| {
        entry.device_id == device_id
            && entry.lane == lane.as_str()
            && entry.lease_id == lease_id
            && entry.manifest_id == manifest_id
    })
}

fn device_lane_has_active(
    state: &OutgoingLogicalIssueState,
    device_id: &str,
    lane: CompletionLane,
) -> bool {
    state
        .active
        .iter()
        .any(|entry| entry.device_id == device_id && entry.lane == lane.as_str())
}

impl OutgoingLogicalIssuedObjects {
    #[cfg(test)]
    pub(crate) fn mark_after_full_response(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
        object: &str,
        manifest: &LogicalCompletionManifest,
    ) -> Result<(), PeerSyncError> {
        validate_tuple(device_id, lane, lease_id, manifest_id)?;
        if !is_lower_hex_256(object) {
            return invalid("invalid issued logical completion object");
        }
        let descriptor = manifest.object(object).ok_or_else(|| {
            PeerSyncError::Validation(
                "logical completion object is absent from the source manifest".to_owned(),
            )
        })?;
        let mut state = self.lock_state()?;
        state.pending.insert(IssuedLogicalObject {
            device_id: device_id.to_owned(),
            lane: lane.as_str(),
            lease_id: lease_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            object: object.to_owned(),
            index: descriptor.index,
            size: descriptor.size,
        });
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn has_pending_for_test(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
    ) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| tuple_has_pending(&state, device_id, lane, lease_id, manifest_id))
    }

    fn lock_state(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, OutgoingLogicalIssueState>, PeerSyncError> {
        self.state.lock().map_err(|_| {
            PeerSyncError::Storage("logical completion issued-object lock failed".to_owned())
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogicalCompletionProofProgress {
    Partial { verified_bytes: u64 },
    Complete { verified_bytes: u64 },
}

impl LogicalCompletionProofProgress {
    pub(crate) fn verified_bytes(self) -> u64 {
        match self {
            Self::Partial { verified_bytes } | Self::Complete { verified_bytes } => verified_bytes,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LogicalCompletionProofFile {
    schema: String,
    device_id: String,
    lane: String,
    lease_id: String,
    manifest_id: String,
    object_count: usize,
    object_sizes: String,
    verified_bitmap: String,
    verified_count: usize,
    verified_bytes: u64,
    frozen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sealed_bytes: Option<u64>,
}

struct LoadedLogicalCompletionProof {
    file: LogicalCompletionProofFile,
    bitmap: Vec<u8>,
    object_sizes: Vec<u64>,
}

impl LoadedLogicalCompletionProof {
    fn empty(
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
        manifest: &LogicalCompletionManifest,
    ) -> Self {
        let object_count = manifest.object_count();
        let bitmap = vec![0; bitmap_len(object_count)];
        let object_sizes = manifest.object_sizes().collect::<Vec<_>>();
        Self {
            file: LogicalCompletionProofFile {
                schema: LOGICAL_COMPLETION_PROOF_SCHEMA.to_owned(),
                device_id: device_id.to_owned(),
                lane: lane.as_str().to_owned(),
                lease_id: lease_id.to_owned(),
                manifest_id: manifest_id.to_owned(),
                object_count,
                object_sizes: encode_object_sizes(&object_sizes),
                verified_bitmap: STANDARD_NO_PAD.encode(&bitmap),
                verified_count: 0,
                verified_bytes: 0,
                frozen: false,
                sealed_bytes: None,
            },
            bitmap,
            object_sizes,
        }
    }

    fn validate(&self, manifest: Option<&LogicalCompletionManifest>) -> Result<(), PeerSyncError> {
        if self.file.schema != LOGICAL_COMPLETION_PROOF_SCHEMA
            || !matches!(self.file.lane.as_str(), "delta" | "bidirectional")
            || self.file.object_count > MAX_LOGICAL_COMPLETION_OBJECTS
            || self.object_sizes.len() != self.file.object_count
            || encode_object_sizes(&self.object_sizes) != self.file.object_sizes
            || self.bitmap.len() != bitmap_len(self.file.object_count)
            || STANDARD_NO_PAD.encode(&self.bitmap) != self.file.verified_bitmap
            || self.file.verified_count
                != self
                    .bitmap
                    .iter()
                    .map(|byte| byte.count_ones() as usize)
                    .sum::<usize>()
            || self.file.verified_count > self.file.object_count
        {
            return invalid("invalid logical completion proof");
        }
        if let Some(last) = self.bitmap.last() {
            let unused = self.bitmap.len() * 8 - self.file.object_count;
            if unused != 0 && last & (!0_u8 << (8 - unused)) != 0 {
                return invalid("invalid logical completion proof bitmap");
            }
        }
        if let Some(manifest) = manifest {
            if manifest.object_count() != self.file.object_count
                || !manifest
                    .object_sizes()
                    .eq(self.object_sizes.iter().copied())
            {
                return invalid("logical completion proof differs from the source manifest");
            }
        }
        let mut canonical_verified_bytes = 0_u64;
        for (index, size) in self.object_sizes.iter().enumerate() {
            if self.contains(index) {
                canonical_verified_bytes =
                    canonical_verified_bytes.checked_add(*size).ok_or_else(|| {
                        PeerSyncError::Validation(
                            "logical completion proof byte count overflow".to_owned(),
                        )
                    })?;
            }
        }
        if canonical_verified_bytes != self.file.verified_bytes {
            return invalid("logical completion proof bytes differ from its bitmap");
        }
        let lane = CompletionLane::parse(&self.file.lane)?;
        if self.file.sealed_bytes.is_some_and(|sealed_bytes| {
            !self.file.frozen
                || match lane {
                    CompletionLane::Delta => sealed_bytes != self.file.verified_bytes,
                    CompletionLane::Bidirectional => sealed_bytes < self.file.verified_bytes,
                    CompletionLane::Clone => true,
                }
        }) {
            return invalid("logical completion sealed bytes differ from its proof");
        }
        validate_tuple(
            &self.file.device_id,
            lane,
            &self.file.lease_id,
            &self.file.manifest_id,
        )
    }

    fn matches(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
    ) -> bool {
        self.file.device_id == device_id
            && self.file.lane == lane.as_str()
            && self.file.lease_id == lease_id
            && self.file.manifest_id == manifest_id
    }

    fn contains(&self, index: usize) -> bool {
        self.bitmap[index / 8] & (1 << (index % 8)) != 0
    }

    fn insert(&mut self, object: &LogicalCompletionObject) -> Result<(), PeerSyncError> {
        self.bitmap[object.index / 8] |= 1 << (object.index % 8);
        self.file.verified_count = self.file.verified_count.checked_add(1).ok_or_else(|| {
            PeerSyncError::Validation("logical completion object count overflow".to_owned())
        })?;
        self.file.verified_bytes = self
            .file
            .verified_bytes
            .checked_add(object.size)
            .ok_or_else(|| {
                PeerSyncError::Validation("logical completion byte count overflow".to_owned())
            })?;
        self.file.verified_bitmap = STANDARD_NO_PAD.encode(&self.bitmap);
        Ok(())
    }

    fn progress(&self) -> LogicalCompletionProofProgress {
        if self.file.frozen {
            LogicalCompletionProofProgress::Complete {
                verified_bytes: self.file.verified_bytes,
            }
        } else {
            LogicalCompletionProofProgress::Partial {
                verified_bytes: self.file.verified_bytes,
            }
        }
    }
}

fn proof_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn prepare_outgoing_logical_completion_proof(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    manifest: &LogicalCompletionManifest,
) -> Result<(), PeerSyncError> {
    validate_logical_lane(lane)?;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, true)?;
    match read_proof(&path, None)? {
        Some(proof) if proof.matches(device_id, lane, lease_id, manifest_id) => {
            proof.validate(Some(manifest))
        }
        Some(proof) if proof.file.frozen => {
            invalid("frozen logical completion proof is unresolved")
        }
        Some(_) | None => write_proof(
            &path,
            &LoadedLogicalCompletionProof::empty(device_id, lane, lease_id, manifest_id, manifest),
        ),
    }
}

pub(crate) struct OutgoingLogicalObjectIssueGuard<'a> {
    coordinator: &'a OutgoingLogicalIssuedObjects,
    issued: Option<IssuedLogicalObject>,
}

impl OutgoingLogicalObjectIssueGuard<'_> {
    pub(crate) fn mark_after_full_response(mut self) -> Result<(), PeerSyncError> {
        let issued = self.issued.take().expect("active logical object issue");
        let mut state = self.coordinator.lock_state()?;
        if !state.active.remove(&issued) {
            return invalid("logical completion object reservation was invalidated");
        }
        state.pending.insert(issued);
        Ok(())
    }
}

impl Drop for OutgoingLogicalObjectIssueGuard<'_> {
    fn drop(&mut self) {
        let Some(issued) = self.issued.take() else {
            return;
        };
        if let Ok(mut state) = self.coordinator.state.lock() {
            state.active.remove(&issued);
        }
    }
}

pub(crate) fn begin_outgoing_logical_object_issue<'a>(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    object: &str,
    manifest: &LogicalCompletionManifest,
    coordinator: &'a OutgoingLogicalIssuedObjects,
) -> Result<OutgoingLogicalObjectIssueGuard<'a>, PeerSyncError> {
    validate_logical_lane(lane)?;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    if manifest.object(object).is_none() {
        return invalid("logical completion object is absent from the source manifest");
    }
    let issued = IssuedLogicalObject {
        device_id: device_id.to_owned(),
        lane: lane.as_str(),
        lease_id: lease_id.to_owned(),
        manifest_id: manifest_id.to_owned(),
        object: object.to_owned(),
        index: manifest
            .object(object)
            .expect("logical object checked above")
            .index,
        size: manifest
            .object(object)
            .expect("logical object checked above")
            .size,
    };
    let mut state = coordinator.lock_state()?;
    if state.active.contains(&issued) {
        return invalid("logical completion object is already being issued");
    }
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let proof = read_proof(&path, Some(manifest))?.ok_or_else(|| {
        PeerSyncError::Validation("logical completion proof is missing".to_owned())
    })?;
    if !proof.matches(device_id, lane, lease_id, manifest_id) {
        return invalid("logical completion proof tuple does not match");
    }
    if !outgoing_completion_lease_matches(app_root, device_id, lane, lease_id, manifest_id)? {
        return invalid("logical completion lease does not match its proof");
    }
    if proof.file.frozen {
        return invalid("logical completion proof is already frozen");
    }
    state.active.insert(issued.clone());
    Ok(OutgoingLogicalObjectIssueGuard {
        coordinator,
        issued: Some(issued),
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_outgoing_logical_progress(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    verified_object: Option<&str>,
    manifest: &LogicalCompletionManifest,
    issued: &OutgoingLogicalIssuedObjects,
) -> Result<LogicalCompletionProofProgress, PeerSyncError> {
    validate_logical_lane(lane)?;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    let mut issue_state = issued.lock_state()?;
    let progress = {
        let _guard = lock_proofs()?;
        let path = proof_path(app_root, device_id, lane, false)?;
        let mut proof = read_proof(&path, Some(manifest))?.ok_or_else(|| {
            PeerSyncError::Validation("logical completion proof is missing".to_owned())
        })?;
        if !proof.matches(device_id, lane, lease_id, manifest_id) {
            return invalid("logical completion proof tuple does not match");
        }
        if !outgoing_completion_lease_matches(app_root, device_id, lane, lease_id, manifest_id)? {
            return invalid("logical completion lease does not match its proof");
        }
        match verified_object {
            None => {
                if tuple_has_active(&issue_state, device_id, lane, lease_id, manifest_id)
                    || tuple_has_pending(&issue_state, device_id, lane, lease_id, manifest_id)
                {
                    return invalid("logical completion proof has unverified object responses");
                }
                if !proof.file.frozen {
                    proof.file.frozen = true;
                    write_proof(&path, &proof)?;
                }
            }
            Some(object_hash) => {
                let object = manifest.object(object_hash).ok_or_else(|| {
                    PeerSyncError::Validation(
                        "logical completion object is absent from the source manifest".to_owned(),
                    )
                })?;
                let issued_object = IssuedLogicalObject {
                    device_id: device_id.to_owned(),
                    lane: lane.as_str(),
                    lease_id: lease_id.to_owned(),
                    manifest_id: manifest_id.to_owned(),
                    object: object_hash.to_owned(),
                    index: object.index,
                    size: object.size,
                };
                if !proof.contains(object.index) {
                    if proof.file.frozen {
                        return invalid("logical completion proof is already frozen");
                    }
                    if !issue_state.pending.contains(&issued_object) {
                        return invalid(
                            "logical completion object was not issued by this source process",
                        );
                    }
                    proof.insert(object)?;
                    write_proof(&path, &proof)?;
                }
                issue_state.pending.remove(&issued_object);
            }
        }
        proof.progress()
    };
    Ok(progress)
}

pub(crate) fn issue_outgoing_logical_completion_lease(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    manifest_id: &str,
    resume_lease_id: Option<&str>,
    manifest: &LogicalCompletionManifest,
    coordinator: &OutgoingLogicalIssuedObjects,
) -> Result<CompletionLeaseId, PeerSyncError> {
    validate_logical_lane(lane)?;
    validate_device_id(device_id)?;
    if !is_lower_hex_256(manifest_id) {
        return invalid("invalid logical completion manifest");
    }
    let mut issue_state = coordinator.lock_state()?;
    if device_lane_has_active(&issue_state, device_id, lane) {
        return invalid("logical completion objects are still in flight");
    }
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, true)?;
    let existing = read_proof(&path, None)?;
    let completed_frozen = if let Some(proof) = existing.as_ref().filter(|proof| proof.file.frozen)
    {
        if proof.file.device_id != device_id || proof.file.lane != lane.as_str() {
            return invalid("frozen logical completion proof is unresolved");
        }
        let completed = match proof.file.sealed_bytes {
            Some(sealed_bytes) => outgoing_completion_receipt_matches(
                app_root,
                device_id,
                lane,
                &proof.file.lease_id,
                &proof.file.manifest_id,
                sealed_bytes,
            )?,
            None => false,
        };
        if !completed {
            if proof.file.manifest_id != manifest_id
                || resume_lease_id.is_some_and(|lease| proof.file.lease_id != lease)
                || !outgoing_completion_lease_matches(
                    app_root,
                    device_id,
                    lane,
                    &proof.file.lease_id,
                    manifest_id,
                )?
            {
                return invalid("frozen logical completion proof is unresolved");
            }
            proof.validate(Some(manifest))?;
        } else if proof.file.manifest_id == manifest_id
            && resume_lease_id == Some(proof.file.lease_id.as_str())
        {
            proof.validate(Some(manifest))?;
        }
        completed
    } else {
        false
    };
    let lease = issue_outgoing_unmeasured_completion_offer(
        app_root,
        device_id,
        lane,
        manifest_id,
        resume_lease_id,
    )?;
    match existing {
        Some(proof) if proof.matches(device_id, lane, lease.as_str(), manifest_id) => {
            proof.validate(Some(manifest))?;
        }
        Some(proof) if proof.file.frozen && !completed_frozen => {
            return invalid("frozen logical completion proof is unresolved");
        }
        Some(_) | None => write_proof(
            &path,
            &LoadedLogicalCompletionProof::empty(
                device_id,
                lane,
                lease.as_str(),
                manifest_id,
                manifest,
            ),
        )?,
    }
    issue_state.pending.retain(|entry| {
        entry.device_id != device_id
            || entry.lane != lane.as_str()
            || (entry.lease_id == lease.as_str() && entry.manifest_id == manifest_id)
    });
    Ok(lease)
}

pub(crate) fn freeze_outgoing_logical_completion_proof(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    coordinator: &OutgoingLogicalIssuedObjects,
) -> Result<u64, PeerSyncError> {
    validate_logical_lane(lane)?;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    let issue_state = coordinator.lock_state()?;
    if tuple_has_active(&issue_state, device_id, lane, lease_id, manifest_id)
        || tuple_has_pending(&issue_state, device_id, lane, lease_id, manifest_id)
    {
        return invalid("logical completion objects are still unverified");
    }
    freeze_proof_bytes(app_root, device_id, lane, lease_id, manifest_id, None)
}

fn freeze_proof_bytes(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    manifest: Option<&LogicalCompletionManifest>,
) -> Result<u64, PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let mut proof = read_proof(&path, manifest)?.ok_or_else(|| {
        PeerSyncError::Validation("logical completion proof is missing".to_owned())
    })?;
    if !proof.matches(device_id, lane, lease_id, manifest_id) {
        return invalid("logical completion proof tuple does not match");
    }
    if !outgoing_completion_lease_matches(app_root, device_id, lane, lease_id, manifest_id)? {
        return invalid("logical completion lease does not match its proof");
    }
    if !proof.file.frozen {
        proof.file.frozen = true;
        write_proof(&path, &proof)?;
    }
    Ok(proof.file.verified_bytes)
}

pub(crate) fn seal_outgoing_delta_logical_completion(
    app_root: &Path,
    device_id: &str,
    lease_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> Result<CompletionSealStatus, PeerSyncError> {
    let lane = CompletionLane::Delta;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    let proof_bytes = frozen_proof_bytes(app_root, device_id, lane, lease_id, manifest_id, None)?;
    if proof_bytes != transferred_bytes {
        return invalid("delta completion byte count differs from its source proof");
    }
    persist_proof_sealed_bytes(
        app_root,
        device_id,
        lane,
        lease_id,
        manifest_id,
        transferred_bytes,
    )?;
    seal_outgoing_completion_lease(
        app_root,
        device_id,
        lane,
        lease_id,
        manifest_id,
        transferred_bytes,
    )
}

pub(crate) fn outgoing_bidirectional_local_proof_bytes(
    app_root: &Path,
    device_id: &str,
    lease_id: &str,
    manifest_id: &str,
    manifest: &LogicalCompletionManifest,
) -> Result<u64, PeerSyncError> {
    let lane = CompletionLane::Bidirectional;
    validate_tuple(device_id, lane, lease_id, manifest_id)?;
    frozen_proof_bytes(
        app_root,
        device_id,
        lane,
        lease_id,
        manifest_id,
        Some(manifest),
    )
}

fn frozen_proof_bytes(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    manifest: Option<&LogicalCompletionManifest>,
) -> Result<u64, PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let proof = read_proof(&path, manifest)?.ok_or_else(|| {
        PeerSyncError::Validation("logical completion proof is missing".to_owned())
    })?;
    if !proof.matches(device_id, lane, lease_id, manifest_id) || !proof.file.frozen {
        return invalid("logical completion proof is not frozen");
    }
    Ok(proof.file.verified_bytes)
}

fn persist_proof_sealed_bytes(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    sealed_bytes: u64,
) -> Result<(), PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let mut proof = read_proof(&path, None)?.ok_or_else(|| {
        PeerSyncError::Validation("logical completion proof is missing".to_owned())
    })?;
    if !proof.matches(device_id, lane, lease_id, manifest_id) || !proof.file.frozen {
        return invalid("logical completion proof is not frozen");
    }
    match proof.file.sealed_bytes {
        Some(existing) if existing != sealed_bytes => {
            return invalid("logical completion proof was sealed with different bytes");
        }
        Some(_) => return Ok(()),
        None => {}
    }
    proof.file.sealed_bytes = Some(sealed_bytes);
    write_proof(&path, &proof)
}

pub(crate) fn seal_outgoing_bidirectional_logical_completion(
    app_root: &Path,
    device_id: &str,
    lease_id: &str,
    manifest_id: &str,
    remote_apply_bytes: u64,
    coordinator: &OutgoingLogicalIssuedObjects,
) -> Result<CompletionSealStatus, PeerSyncError> {
    let lane = CompletionLane::Bidirectional;
    let issue_state = coordinator.lock_state()?;
    if tuple_has_active(&issue_state, device_id, lane, lease_id, manifest_id)
        || tuple_has_pending(&issue_state, device_id, lane, lease_id, manifest_id)
    {
        return invalid("logical completion objects are still unverified");
    }
    let local_bytes = freeze_proof_bytes(app_root, device_id, lane, lease_id, manifest_id, None)?;
    let total = local_bytes.checked_add(remote_apply_bytes).ok_or_else(|| {
        PeerSyncError::Validation("bidirectional completion byte count overflow".to_owned())
    })?;
    persist_proof_sealed_bytes(app_root, device_id, lane, lease_id, manifest_id, total)?;
    seal_outgoing_completion_lease(app_root, device_id, lane, lease_id, manifest_id, total)
}

pub(crate) fn invalidate_outgoing_logical_completion_proofs(
    app_root: &Path,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    validate_device_id(device_id)?;
    let _guard = lock_proofs()?;
    let peer_root = app_root.join("peer-sync");
    ensure_directory(&fs::symlink_metadata(&peer_root)?)?;
    let proof_root = peer_root.join(LOGICAL_COMPLETION_PROOF_DIRECTORY);
    match fs::symlink_metadata(&proof_root) {
        Ok(metadata) => ensure_directory(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let scan_result: Result<(), PeerSyncError> = (|| {
        for lane in [CompletionLane::Delta, CompletionLane::Bidirectional] {
            let path = proof_root.join(format!("{device_id}-{}.json", lane.as_str()));
            invalidate_logical_completion_proof_path(&path)?;
        }
        Ok(())
    })();
    let sync_result = sync_proof_directory_after_invalidation(&proof_root);
    scan_result?;
    sync_result
}

#[cfg(windows)]
fn logical_completion_proof_tombstone_path(path: &Path) -> PathBuf {
    path.with_extension("revoked")
}

#[cfg(windows)]
fn remove_logical_completion_proof_tombstone(path: &Path) -> Result<(), PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_regular_file(&metadata)?;
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(windows)]
fn invalidate_logical_completion_proof_path(path: &Path) -> Result<(), PeerSyncError> {
    let tombstone = logical_completion_proof_tombstone_path(path);
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_regular_file(&metadata)?;
            match fs::symlink_metadata(&tombstone) {
                Ok(metadata) => ensure_regular_file(&metadata)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            replace_file_atomic(path, &tombstone)?;
            #[cfg(test)]
            if fs::remove_file(proof_tombstone_cleanup_failure_marker(path)).is_ok() {
                return Err(PeerSyncError::Storage(
                    "injected logical completion proof tombstone cleanup failure".to_owned(),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    remove_logical_completion_proof_tombstone(&tombstone)
}

#[cfg(not(windows))]
fn invalidate_logical_completion_proof_path(path: &Path) -> Result<(), PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure_regular_file(&metadata)?;
            fs::remove_file(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn invalidate_outgoing_logical_completion_state(
    app_root: &Path,
    device_id: &str,
    coordinator: &OutgoingLogicalIssuedObjects,
) -> Result<(), PeerSyncError> {
    validate_device_id(device_id)?;
    let mut issue_state = coordinator.lock_state()?;
    invalidate_outgoing_logical_completion_proofs(app_root, device_id)?;
    issue_state
        .active
        .retain(|entry| entry.device_id != device_id);
    issue_state
        .pending
        .retain(|entry| entry.device_id != device_id);
    Ok(())
}

fn sync_proof_directory_after_invalidation(proof_root: &Path) -> Result<(), PeerSyncError> {
    #[cfg(test)]
    {
        record_proof_directory_sync_attempt_for_test(proof_root);
        if fs::remove_file(proof_delete_sync_failure_marker(proof_root)).is_ok() {
            return Err(PeerSyncError::Storage(
                "injected logical completion proof delete sync failure".to_owned(),
            ));
        }
    }
    sync_parent_directory(proof_root)
}

#[cfg(test)]
fn proof_delete_sync_failure_marker(path: &Path) -> PathBuf {
    path.join(".fail-next-delete-sync")
}

#[cfg(test)]
pub(crate) fn fail_next_logical_completion_proof_delete_sync_for_test(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
) -> Result<(), PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let proof_root = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("logical completion proof has no parent directory".to_owned())
    })?;
    let marker = proof_delete_sync_failure_marker(proof_root);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options.open(marker)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
fn proof_directory_sync_attempts_for_test() -> &'static Mutex<BTreeMap<PathBuf, usize>> {
    static ATTEMPTS: OnceLock<Mutex<BTreeMap<PathBuf, usize>>> = OnceLock::new();
    ATTEMPTS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
fn record_proof_directory_sync_attempt_for_test(proof_root: &Path) {
    if let Ok(mut attempts) = proof_directory_sync_attempts_for_test().lock() {
        let count = attempts.entry(proof_root.to_owned()).or_default();
        *count = count.saturating_add(1);
    }
}

#[cfg(test)]
pub(crate) fn logical_completion_proof_directory_sync_attempts_for_test(app_root: &Path) -> usize {
    let proof_root = app_root
        .join("peer-sync")
        .join(LOGICAL_COMPLETION_PROOF_DIRECTORY);
    proof_directory_sync_attempts_for_test()
        .lock()
        .ok()
        .and_then(|attempts| attempts.get(&proof_root).copied())
        .unwrap_or_default()
}

#[cfg(all(test, windows))]
fn proof_tombstone_cleanup_failure_marker(path: &Path) -> PathBuf {
    path.with_extension("fail-next-tombstone-cleanup")
}

#[cfg(all(test, windows))]
pub(crate) fn fail_next_logical_completion_tombstone_cleanup_for_test(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
) -> Result<(), PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let marker = proof_tombstone_cleanup_failure_marker(&path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options.open(marker)?.sync_all()?;
    Ok(())
}

fn lock_proofs() -> Result<std::sync::MutexGuard<'static, ()>, PeerSyncError> {
    proof_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("logical completion proof lock failed".to_owned()))
}

fn proof_path(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    create: bool,
) -> Result<PathBuf, PeerSyncError> {
    validate_device_id(device_id)?;
    validate_logical_lane(lane)?;
    let peer_root = app_root.join("peer-sync");
    let peer_metadata = fs::symlink_metadata(&peer_root)?;
    ensure_directory(&peer_metadata)?;
    let proof_root = peer_root.join(LOGICAL_COMPLETION_PROOF_DIRECTORY);
    match fs::symlink_metadata(&proof_root) {
        Ok(metadata) => ensure_directory(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir(&proof_root)?;
            if let Err(error) = sync_parent_directory(&peer_root) {
                let _ = fs::remove_dir(&proof_root);
                return Err(error);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(proof_root.join(format!("{device_id}-{}.json", lane.as_str())))
}

fn read_proof(
    path: &Path,
    manifest: Option<&LogicalCompletionManifest>,
) -> Result<Option<LoadedLogicalCompletionProof>, PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure_regular_file(&metadata)?;
    if metadata.len() > MAX_LOGICAL_COMPLETION_PROOF_BYTES as u64 {
        return invalid("logical completion proof exceeds the size limit");
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_LOGICAL_COMPLETION_PROOF_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LOGICAL_COMPLETION_PROOF_BYTES {
        return invalid("logical completion proof exceeds the size limit");
    }
    let file: LogicalCompletionProofFile = serde_json::from_slice(&bytes)
        .map_err(|_| PeerSyncError::Validation("malformed logical completion proof".to_owned()))?;
    let bitmap = STANDARD_NO_PAD
        .decode(&file.verified_bitmap)
        .map_err(|_| PeerSyncError::Validation("invalid logical completion proof".to_owned()))?;
    let object_sizes = decode_object_sizes(&file.object_sizes)?;
    let loaded = LoadedLogicalCompletionProof {
        file,
        bitmap,
        object_sizes,
    };
    loaded.validate(manifest)?;
    Ok(Some(loaded))
}

fn write_proof(path: &Path, proof: &LoadedLogicalCompletionProof) -> Result<(), PeerSyncError> {
    proof.validate(None)?;
    let bytes = serde_json::to_vec(&proof.file)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() > MAX_LOGICAL_COMPLETION_PROOF_BYTES {
        return invalid("logical completion proof exceeds the size limit");
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("logical completion proof has no parent".to_owned())
    })?;
    #[cfg(test)]
    {
        let failure_marker = proof_write_failure_marker(path);
        if fs::remove_file(&failure_marker).is_ok() {
            return Err(PeerSyncError::Storage(
                "injected logical completion proof write failure".to_owned(),
            ));
        }
    }
    ensure_directory(&fs::symlink_metadata(parent)?)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure_regular_file(&metadata)?;
    }
    let temporary = parent.join(format!(".logical-completion-{}.tmp", uuid::Uuid::new_v4()));
    let result = write_owner_only(&temporary, &bytes).and_then(|_| {
        replace_file_atomic(&temporary, path)?;
        sync_parent_directory(parent)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
fn proof_write_failure_marker(path: &Path) -> PathBuf {
    path.with_extension("fail-next-write")
}

#[cfg(test)]
pub(crate) fn fail_next_logical_completion_proof_write_for_test(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
) -> Result<(), PeerSyncError> {
    let _guard = lock_proofs()?;
    let path = proof_path(app_root, device_id, lane, false)?;
    let marker = proof_write_failure_marker(&path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options.open(marker)?.sync_all()?;
    Ok(())
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
}

fn ensure_directory(metadata: &fs::Metadata) -> Result<(), PeerSyncError> {
    if !metadata.is_dir() || is_link_like(metadata) {
        return invalid("logical completion proof directory must be ordinary");
    }
    Ok(())
}

fn ensure_regular_file(metadata: &fs::Metadata) -> Result<(), PeerSyncError> {
    if !metadata.is_file() || is_link_like(metadata) {
        return invalid("logical completion proof must be a regular file");
    }
    Ok(())
}

fn validate_logical_lane(lane: CompletionLane) -> Result<(), PeerSyncError> {
    if matches!(lane, CompletionLane::Delta | CompletionLane::Bidirectional) {
        Ok(())
    } else {
        invalid("clone cannot use a logical completion proof")
    }
}

fn validate_tuple(
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> Result<(), PeerSyncError> {
    validate_device_id(device_id)?;
    validate_logical_lane(lane)?;
    let parsed = uuid::Uuid::parse_str(lease_id)
        .map_err(|_| PeerSyncError::Validation("invalid logical completion lease".to_owned()))?;
    if parsed.get_version_num() != 4
        || parsed.get_variant() != uuid::Variant::RFC4122
        || parsed.to_string() != lease_id
        || !is_lower_hex_256(manifest_id)
    {
        return invalid("invalid logical completion proof tuple");
    }
    Ok(())
}

fn validate_device_id(device_id: &str) -> Result<(), PeerSyncError> {
    if uuid::Uuid::parse_str(device_id)
        .map(|parsed| parsed.to_string() == device_id)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        invalid("invalid logical completion device")
    }
}

fn bitmap_len(object_count: usize) -> usize {
    object_count.div_ceil(8)
}

fn encode_object_sizes(sizes: &[u64]) -> String {
    let mut bytes = Vec::with_capacity(sizes.len().saturating_mul(std::mem::size_of::<u64>()));
    for size in sizes {
        bytes.extend_from_slice(&size.to_le_bytes());
    }
    STANDARD_NO_PAD.encode(bytes)
}

fn decode_object_sizes(encoded: &str) -> Result<Vec<u64>, PeerSyncError> {
    let bytes = STANDARD_NO_PAD
        .decode(encoded)
        .map_err(|_| PeerSyncError::Validation("invalid logical completion proof".to_owned()))?;
    if bytes.len() % std::mem::size_of::<u64>() != 0
        || bytes.len() / std::mem::size_of::<u64>() > MAX_LOGICAL_COMPLETION_OBJECTS
    {
        return invalid("invalid logical completion proof object sizes");
    }
    Ok(bytes
        .chunks_exact(std::mem::size_of::<u64>())
        .map(|bytes| u64::from_le_bytes(bytes.try_into().expect("exact u64 chunk")))
        .collect())
}

#[cfg(test)]
pub(crate) fn maximum_logical_completion_proof_size_for_test() -> (usize, usize) {
    let bitmap = vec![0; bitmap_len(MAX_LOGICAL_COMPLETION_OBJECTS)];
    let file = LogicalCompletionProofFile {
        schema: LOGICAL_COMPLETION_PROOF_SCHEMA.to_owned(),
        device_id: "00000000-0000-4000-8000-000000000000".to_owned(),
        lane: CompletionLane::Bidirectional.as_str().to_owned(),
        lease_id: "00000000-0000-4000-8000-000000000000".to_owned(),
        manifest_id: "f".repeat(64),
        object_count: MAX_LOGICAL_COMPLETION_OBJECTS,
        object_sizes: encode_object_sizes(&vec![u64::MAX; MAX_LOGICAL_COMPLETION_OBJECTS]),
        verified_bitmap: STANDARD_NO_PAD.encode(bitmap),
        verified_count: 0,
        verified_bytes: 0,
        frozen: false,
        sealed_bytes: None,
    };
    (
        serde_json::to_vec(&file)
            .expect("maximum logical completion proof serializes")
            .len(),
        MAX_LOGICAL_COMPLETION_PROOF_BYTES,
    )
}

fn invalid<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.to_owned()))
}
