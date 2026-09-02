use super::{
    active_generation, current_revision, logical_index::scan_compact_manifest,
    sync_device_registry::advance_sync_device_shared_ack_in_transaction, PersistentStore,
    StoreError, SyncDeviceAckState, SyncGenerationIdentity, VerifiedSharedAckLocalProof,
};
use crate::{
    asset_repository::{
        job_pins::{CasObjectRole, DurableCasJob},
        owner_manifest_codec::{decode_owner_manifest, encode_owner_manifest},
        PayloadCas, PreparedPayload,
    },
    peer_sync::{
        logical_delta::{
            decode_asset_alias_metadata, decode_logical_manifest, decode_logical_record,
            decode_logical_record_key, decode_message_page, encode_logical_record_key,
            hash_logical_manifest, LogicalManifest, LogicalManifestObject, LogicalManifestRecord,
            LogicalOwnerHead, LogicalOwnerLocator, LogicalRecordEnvelope, LogicalRecordLocator,
            LOGICAL_MESSAGE_PAGE_SIZE, MAX_LOGICAL_MANIFEST_BYTES,
        },
        maintenance::ActiveTempGuard,
        LogicalDeltaActivation, LogicalDeltaApplyOperation, LogicalDeltaObject,
        LogicalDeltaStagedTarget, PeerSyncError, ReadyLogicalDeltaPlan,
    },
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::cell::Cell;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

// Staged clone/move/delete must cover exactly the same generation-scoped record
// families as the canonical list; reference it so the two can never diverge.
use super::GENERATION_TABLES as PDS_GENERATION_TABLES;

pub(crate) struct PersistentLogicalDeltaTarget<'a> {
    store: &'a mut PersistentStore,
    cas: &'a PayloadCas,
    peer_id: String,
    library_id: String,
    local_generation_id: String,
    remote_manifest: LogicalManifest,
    remote_manifest_hash: String,
    staging_root: PathBuf,
    durable_job: Option<&'a RefCell<DurableCasJob>>,
    same_run_job_prepared_objects: BTreeSet<String>,
    conflict_policy: LogicalDeltaConflictPolicy,
    activation_mode: LogicalDeltaActivationMode,
    durable_abort_succeeded: bool,
    #[cfg(test)]
    payload_reprepare_count: Cell<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalDeltaActivationMode {
    AdvanceRemoteBase,
    DeferPeerState,
    AdvanceSharedAck,
}

struct PreparedSharedAckTransition {
    previous: SyncDeviceAckState,
    next_shared: SyncGenerationIdentity,
    local_identity: SyncGenerationIdentity,
    proof: VerifiedSharedAckLocalProof,
}

pub(crate) enum PersistentLogicalDeltaStage {
    AlreadyActive {
        revision: i64,
        changed: bool,
        deferred_result: Option<DeferredActiveResult>,
    },
    NoOp {
        expected_base: PeerBase,
        database_staged: bool,
    },
    Changed {
        expected_base: PeerBase,
        expected_local_revision: i64,
        staging_id: String,
        logical_generation_id: String,
        merged_generation_sequence: String,
        pin_lease_id: String,
        staging_directory: PathBuf,
        maintenance_guard: Option<ActiveTempGuard>,
        staged_objects: BTreeMap<String, PathBuf>,
        database_staged: bool,
        initialized: bool,
    },
}

#[derive(Clone)]
pub(crate) struct DeferredActiveResult {
    expected_base: PeerBase,
    local_identity: SyncGenerationIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PeerBase {
    generation_id: String,
    manifest_hash: String,
    generation_sequence: String,
}

impl From<PeerBase> for SyncGenerationIdentity {
    fn from(base: PeerBase) -> Self {
        Self {
            generation_id: base.generation_id,
            manifest_hash: base.manifest_hash,
            generation_sequence: base.generation_sequence,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalDeltaConflictPolicy {
    Reject,
    PreferLocal,
    PreferRemote,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogicalDeltaConflictKind {
    SameRecord,
    DeleteVsEdit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LogicalDeltaConflict {
    pub(crate) record: String,
    pub(crate) kind: LogicalDeltaConflictKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LogicalDeltaPlanResolution {
    Ready {
        plan: ReadyLogicalDeltaPlan,
        conflicts: Vec<LogicalDeltaConflict>,
    },
    Conflict {
        conflicts: Vec<LogicalDeltaConflict>,
    },
}

struct PolicyThreeWayPlan {
    apply: Vec<LogicalDeltaApplyOperation>,
    preserve_local_keys: Vec<String>,
    candidate_object_hashes: Vec<String>,
    conflicts: Vec<LogicalDeltaConflict>,
}

struct PreparedPut {
    key: String,
    locator: LogicalRecordLocator,
    envelope: LogicalRecordEnvelope,
    object_hash: String,
    object_size: u64,
    dependencies: Vec<(String, u64)>,
    pages: Vec<PreparedPage>,
}

struct PreparedPage {
    hash: String,
    size: u64,
    message_count: u64,
}

struct PreparedDelete {
    key: String,
    locator: LogicalRecordLocator,
    deleted_generation_sequence: String,
}

struct ResolvedOwnerHead {
    head: LogicalOwnerHead,
    tuples: Option<Vec<Value>>,
}

pub(crate) fn establish_logical_common_base(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    peer_id: &str,
    library_id: &str,
    local_generation_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
) -> Result<(), PeerSyncError> {
    establish_logical_common_base_with_commit_intent(
        store,
        cas,
        peer_id,
        library_id,
        local_generation_id,
        expected_revision,
        remote_manifest_bytes,
        || Ok(()),
    )
}

pub(crate) fn establish_logical_common_base_with_commit_intent<F>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    peer_id: &str,
    library_id: &str,
    local_generation_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    commit_intent: F,
) -> Result<(), PeerSyncError>
where
    F: FnOnce() -> Result<(), PeerSyncError>,
{
    if peer_id.is_empty() || library_id.is_empty() || local_generation_id.is_empty() {
        return validation("logical common-base identities must be nonempty");
    }
    if expected_revision < 0 {
        return validation("logical common-base revision must be nonnegative");
    }

    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != library_id {
        return validation("logical common-base manifest belongs to another library");
    }
    let remote_manifest_hash = hash_logical_manifest(&remote_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let prepared = cas.prepare_bytes(remote_manifest_bytes)?;
    if prepared.content_hash != remote_manifest_hash
        || prepared.byte_size != remote_manifest_bytes.len() as u64
    {
        return validation("logical common-base CAS identity is inconsistent");
    }
    let remote_base = PeerBase {
        generation_id: remote_manifest.generation.clone(),
        manifest_hash: remote_manifest_hash.clone(),
        generation_sequence: remote_manifest.generation_sequence.clone(),
    };

    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let actual_revision = current_revision(&transaction).map_err(storage_error)?;
    if actual_revision != expected_revision {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_revision.to_string()),
            actual: Some(actual_revision.to_string()),
        });
    }
    let active = active_generation(&transaction).map_err(storage_error)?;
    let local_metadata: Option<(String, i64)> = transaction
        .query_row(
            "SELECT pds_generation, source_revision
             FROM logical_sync_generations
             WHERE library_id = ?1 AND generation_id = ?2
               AND state = 'complete' AND manifest_hash IS NOT NULL",
            params![library_id, local_generation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((local_pds_generation, local_source_revision)) = local_metadata else {
        return validation("logical common-base local generation is absent or incomplete");
    };
    if local_pds_generation != active || local_source_revision != expected_revision {
        return validation(
            "logical common-base local generation is not the exact active PDS revision",
        );
    }
    let local_manifest = scan_compact_manifest(&transaction, library_id, local_generation_id, true)
        .map_err(storage_error)?;
    if local_manifest.manifest.records != remote_manifest.records
        || local_manifest.manifest.objects != remote_manifest.objects
    {
        return validation(
            "logical common-base remote content differs from the active logical generation",
        );
    }
    validate_same_generation_identity(
        &local_manifest.manifest.generation,
        &local_manifest.manifest_hash,
        &local_manifest.manifest.generation_sequence,
        &remote_manifest.generation,
        &remote_manifest_hash,
        &remote_manifest.generation_sequence,
    )?;

    let existing: Option<PeerBase> = transaction
        .query_row(
            "SELECT generation_id, manifest_hash, generation_sequence
             FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2",
            params![peer_id, library_id],
            |row| {
                Ok(PeerBase {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)?;
    match existing {
        Some(existing) if existing == remote_base => {}
        Some(existing) => {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(remote_base.manifest_hash),
                actual: Some(existing.manifest_hash),
            });
        }
        None => {
            transaction
                .execute(
                    "INSERT INTO logical_peer_common_bases (
                        peer_id, library_id, generation_id, manifest_hash,
                        generation_sequence, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        peer_id,
                        library_id,
                        remote_base.generation_id,
                        remote_base.manifest_hash,
                        remote_base.generation_sequence,
                        unix_millis()?,
                    ],
                )
                .map_err(sql_error)?;
        }
    }
    commit_intent()?;
    transaction.commit().map_err(sql_error)
}

// Generates the public constructor family for PersistentLogicalDeltaTarget.
// Every constructor is a fixed point on the durable-job x conflict-policy x
// activation-mode axes and forwards to new_inner; the table below is the
// single place those axes are enumerated.
macro_rules! logical_delta_target_constructor {
    ($name:ident, policy: $policy:expr, mode: $mode:expr) => {
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn $name(
            store: &'a mut PersistentStore,
            cas: &'a PayloadCas,
            peer_id: &str,
            library_id: &str,
            local_generation_id: &str,
            remote_manifest_bytes: &[u8],
            staging_root: &Path,
        ) -> Result<Self, PeerSyncError> {
            Self::new_inner(
                store,
                cas,
                peer_id,
                library_id,
                local_generation_id,
                remote_manifest_bytes,
                staging_root,
                None,
                $policy,
                $mode,
            )
        }
    };
    ($name:ident, policy_param, mode: $mode:expr) => {
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn $name(
            store: &'a mut PersistentStore,
            cas: &'a PayloadCas,
            peer_id: &str,
            library_id: &str,
            local_generation_id: &str,
            remote_manifest_bytes: &[u8],
            staging_root: &Path,
            conflict_policy: LogicalDeltaConflictPolicy,
        ) -> Result<Self, PeerSyncError> {
            Self::new_inner(
                store,
                cas,
                peer_id,
                library_id,
                local_generation_id,
                remote_manifest_bytes,
                staging_root,
                None,
                conflict_policy,
                $mode,
            )
        }
    };
    ($name:ident, durable_job, policy: $policy:expr, mode: $mode:expr) => {
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn $name(
            store: &'a mut PersistentStore,
            cas: &'a PayloadCas,
            peer_id: &str,
            library_id: &str,
            local_generation_id: &str,
            remote_manifest_bytes: &[u8],
            staging_root: &Path,
            durable_job: &'a RefCell<DurableCasJob>,
        ) -> Result<Self, PeerSyncError> {
            Self::new_inner(
                store,
                cas,
                peer_id,
                library_id,
                local_generation_id,
                remote_manifest_bytes,
                staging_root,
                Some(durable_job),
                $policy,
                $mode,
            )
        }
    };
    ($name:ident, durable_job, policy_param, mode: $mode:expr) => {
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn $name(
            store: &'a mut PersistentStore,
            cas: &'a PayloadCas,
            peer_id: &str,
            library_id: &str,
            local_generation_id: &str,
            remote_manifest_bytes: &[u8],
            staging_root: &Path,
            durable_job: &'a RefCell<DurableCasJob>,
            conflict_policy: LogicalDeltaConflictPolicy,
        ) -> Result<Self, PeerSyncError> {
            Self::new_inner(
                store,
                cas,
                peer_id,
                library_id,
                local_generation_id,
                remote_manifest_bytes,
                staging_root,
                Some(durable_job),
                conflict_policy,
                $mode,
            )
        }
    };
}

impl<'a> PersistentLogicalDeltaTarget<'a> {
    logical_delta_target_constructor!(
        new,
        policy: LogicalDeltaConflictPolicy::Reject,
        mode: LogicalDeltaActivationMode::AdvanceRemoteBase
    );
    logical_delta_target_constructor!(
        new_with_conflict_policy,
        policy_param,
        mode: LogicalDeltaActivationMode::AdvanceRemoteBase
    );
    logical_delta_target_constructor!(
        new_with_durable_job,
        durable_job,
        policy: LogicalDeltaConflictPolicy::Reject,
        mode: LogicalDeltaActivationMode::AdvanceRemoteBase
    );
    logical_delta_target_constructor!(
        new_with_durable_job_and_conflict_policy,
        durable_job,
        policy_param,
        mode: LogicalDeltaActivationMode::AdvanceRemoteBase
    );
    logical_delta_target_constructor!(
        new_p5_deferred,
        policy_param,
        mode: LogicalDeltaActivationMode::DeferPeerState
    );
    logical_delta_target_constructor!(
        new_p5_deferred_with_durable_job,
        durable_job,
        policy_param,
        mode: LogicalDeltaActivationMode::DeferPeerState
    );
    logical_delta_target_constructor!(
        new_p5_remote_shared_ack,
        policy_param,
        mode: LogicalDeltaActivationMode::AdvanceSharedAck
    );
    logical_delta_target_constructor!(
        new_p5_remote_shared_ack_with_durable_job,
        durable_job,
        policy_param,
        mode: LogicalDeltaActivationMode::AdvanceSharedAck
    );

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        store: &'a mut PersistentStore,
        cas: &'a PayloadCas,
        peer_id: &str,
        library_id: &str,
        local_generation_id: &str,
        remote_manifest_bytes: &[u8],
        staging_root: &Path,
        durable_job: Option<&'a RefCell<DurableCasJob>>,
        conflict_policy: LogicalDeltaConflictPolicy,
        activation_mode: LogicalDeltaActivationMode,
    ) -> Result<Self, PeerSyncError> {
        if peer_id.is_empty() || library_id.is_empty() || local_generation_id.is_empty() {
            return validation("logical delta target identities must be nonempty");
        }
        if activation_mode != LogicalDeltaActivationMode::AdvanceRemoteBase {
            store
                .sync_device_ack_state(library_id, peer_id)
                .map_err(storage_error)?;
        }
        let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        if remote_manifest.library_id != library_id {
            return validation("logical delta remote manifest belongs to another library");
        }
        let remote_manifest_hash = hash_logical_manifest(&remote_manifest)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let prepared = match durable_job {
            Some(job) => job.borrow_mut().prepare_bytes(
                cas,
                remote_manifest_bytes,
                CasObjectRole::DirectObject,
            )?,
            None => cas.prepare_bytes(remote_manifest_bytes)?,
        };
        if prepared.content_hash != remote_manifest_hash
            || prepared.byte_size != remote_manifest_bytes.len() as u64
        {
            return validation("logical delta remote manifest CAS identity is inconsistent");
        }
        Ok(Self {
            store,
            cas,
            peer_id: peer_id.to_owned(),
            library_id: library_id.to_owned(),
            local_generation_id: local_generation_id.to_owned(),
            remote_manifest,
            remote_manifest_hash,
            staging_root: staging_root.to_path_buf(),
            durable_job,
            same_run_job_prepared_objects: BTreeSet::new(),
            conflict_policy,
            activation_mode,
            durable_abort_succeeded: false,
            #[cfg(test)]
            payload_reprepare_count: Cell::new(0),
        })
    }

    fn prepare_bytes(&self, bytes: &[u8]) -> Result<PreparedPayload, PeerSyncError> {
        match self.durable_job {
            Some(job) => {
                Ok(job
                    .borrow_mut()
                    .prepare_bytes(self.cas, bytes, CasObjectRole::DirectObject)?)
            }
            None => Ok(self.cas.prepare_bytes(bytes)?),
        }
    }

    fn prepare_reader(&self, reader: &mut impl Read) -> Result<PreparedPayload, PeerSyncError> {
        match self.durable_job {
            Some(job) => Ok(job.borrow_mut().prepare_reader(
                self.cas,
                reader,
                CasObjectRole::DirectObject,
            )?),
            None => Ok(self.cas.prepare_reader(reader)?),
        }
    }

    fn seal_durable_job(&mut self) -> Result<(), PeerSyncError> {
        if let Some(job) = self.durable_job {
            job.borrow_mut().seal(self.store, unix_millis()?)?;
        }
        Ok(())
    }

    pub(crate) fn durable_abort_succeeded(&self) -> bool {
        self.durable_abort_succeeded
    }

    fn validate_plan_identity(&self, plan: &ReadyLogicalDeltaPlan) -> Result<(), PeerSyncError> {
        if plan.expected_remote_generation != self.remote_manifest.generation
            || plan.next_base_generation_sequence != self.remote_manifest.generation_sequence
            || plan.next_base_manifest_hash != self.remote_manifest_hash
        {
            return validation("logical delta plan does not match the verified remote manifest");
        }
        for operation in &plan.apply {
            let key = match operation {
                LogicalDeltaApplyOperation::Put { key, .. }
                | LogicalDeltaApplyOperation::Delete { key, .. } => key,
            };
            let index = self
                .remote_manifest
                .records
                .binary_search_by(|record| record.key().cmp(key))
                .map_err(|_| {
                    PeerSyncError::Validation(
                        "logical delta apply record is absent from the verified manifest"
                            .to_owned(),
                    )
                })?;
            let matches = match (operation, &self.remote_manifest.records[index]) {
                (
                    LogicalDeltaApplyOperation::Put {
                        object_hash,
                        dependencies,
                        ..
                    },
                    LogicalManifestRecord::Live(record),
                ) => object_hash == &record.object_hash && dependencies == &record.dependencies,
                (
                    LogicalDeltaApplyOperation::Delete {
                        deleted_generation_sequence,
                        ..
                    },
                    LogicalManifestRecord::Tombstone(record),
                ) => deleted_generation_sequence == &record.deleted_generation_sequence,
                _ => false,
            };
            if !matches {
                return validation(
                    "logical delta apply operation differs from the verified manifest",
                );
            }
        }
        Ok(())
    }

    fn common_base(&self) -> Result<Option<PeerBase>, PeerSyncError> {
        self.store
            .connection
            .query_row(
                "SELECT generation_id, manifest_hash, generation_sequence
                 FROM logical_peer_common_bases
                 WHERE peer_id = ?1 AND library_id = ?2",
                params![self.peer_id, self.library_id],
                |row| {
                    Ok(PeerBase {
                        generation_id: row.get(0)?,
                        manifest_hash: row.get(1)?,
                        generation_sequence: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    pub(crate) fn has_common_base(&self) -> Result<bool, PeerSyncError> {
        Ok(self.common_base()?.is_some())
    }

    pub(crate) fn build_ready_plan(
        &self,
        expected_local_revision: i64,
    ) -> Result<ReadyLogicalDeltaPlan, PeerSyncError> {
        if expected_local_revision < 0 {
            return validation("logical delta expected local revision must be nonnegative");
        }
        let base = self.common_base()?.ok_or_else(|| {
            PeerSyncError::Validation(
                "logical delta target has no durable common base for this peer".to_owned(),
            )
        })?;
        let base_manifest = self.load_common_base_manifest(&base)?;
        let local_manifest =
            self.load_local_manifest(&self.local_generation_id, expected_local_revision, true)?;
        validate_manifest_object_size_parity([
            &base_manifest,
            &local_manifest,
            &self.remote_manifest,
        ])?;
        let (apply, preserve_local_keys, candidate_object_hashes) =
            derive_exact_three_way_plan(&base_manifest, &local_manifest, &self.remote_manifest)?;
        Ok(ReadyLogicalDeltaPlan {
            expected_local_revision,
            expected_base_manifest_hash: base.manifest_hash,
            expected_remote_generation: self.remote_manifest.generation.clone(),
            apply,
            preserve_local_keys,
            candidate_object_hashes,
            next_base_manifest_hash: self.remote_manifest_hash.clone(),
            next_base_generation_sequence: self.remote_manifest.generation_sequence.clone(),
        })
    }

    pub(crate) fn resolve_authoritative_plan(
        &self,
        expected_local_revision: i64,
    ) -> Result<LogicalDeltaPlanResolution, PeerSyncError> {
        self.resolve_authoritative_plan_inner(expected_local_revision, true)
    }

    pub(crate) fn reconstruct_authoritative_plan(
        &self,
        expected_local_revision: i64,
    ) -> Result<LogicalDeltaPlanResolution, PeerSyncError> {
        self.resolve_authoritative_plan_inner(expected_local_revision, false)
    }

    fn resolve_authoritative_plan_inner(
        &self,
        expected_local_revision: i64,
        require_active_local_generation: bool,
    ) -> Result<LogicalDeltaPlanResolution, PeerSyncError> {
        if expected_local_revision < 0 {
            return validation("logical delta expected local revision must be nonnegative");
        }
        let base = self.common_base()?.ok_or_else(|| {
            PeerSyncError::Validation(
                "logical delta target has no durable common base for this peer".to_owned(),
            )
        })?;
        let base_manifest = self.load_common_base_manifest(&base)?;
        let local_manifest = self.load_local_manifest(
            &self.local_generation_id,
            expected_local_revision,
            require_active_local_generation,
        )?;
        validate_manifest_object_size_parity([
            &base_manifest,
            &local_manifest,
            &self.remote_manifest,
        ])?;
        let resolved = derive_policy_three_way_plan(
            &base_manifest,
            &local_manifest,
            &self.remote_manifest,
            self.conflict_policy,
        )?;
        if self.conflict_policy == LogicalDeltaConflictPolicy::Reject
            && !resolved.conflicts.is_empty()
        {
            return Ok(LogicalDeltaPlanResolution::Conflict {
                conflicts: resolved.conflicts,
            });
        }
        Ok(LogicalDeltaPlanResolution::Ready {
            plan: ReadyLogicalDeltaPlan {
                expected_local_revision,
                expected_base_manifest_hash: base.manifest_hash,
                expected_remote_generation: self.remote_manifest.generation.clone(),
                apply: resolved.apply,
                preserve_local_keys: resolved.preserve_local_keys,
                candidate_object_hashes: resolved.candidate_object_hashes,
                next_base_manifest_hash: self.remote_manifest_hash.clone(),
                next_base_generation_sequence: self.remote_manifest.generation_sequence.clone(),
            },
            conflicts: resolved.conflicts,
        })
    }

    fn remote_base(&self) -> PeerBase {
        PeerBase {
            generation_id: self.remote_manifest.generation.clone(),
            manifest_hash: self.remote_manifest_hash.clone(),
            generation_sequence: self.remote_manifest.generation_sequence.clone(),
        }
    }

    fn sync_identity_for_local_generation(
        &self,
        generation_id: &str,
    ) -> Result<SyncGenerationIdentity, PeerSyncError> {
        let built = scan_compact_manifest(
            &self.store.connection,
            &self.library_id,
            generation_id,
            true,
        )
        .map_err(storage_error)?;
        Ok(SyncGenerationIdentity {
            generation_id: built.manifest.generation,
            manifest_hash: built.manifest_hash,
            generation_sequence: built.manifest.generation_sequence,
        })
    }

    fn current_logical_head_generation(&self) -> Result<String, PeerSyncError> {
        self.store
            .connection
            .query_row(
                "SELECT generation_id FROM logical_library_head
                 WHERE singleton = 1 AND library_id = ?1",
                [&self.library_id],
                |row| row.get(0),
            )
            .map_err(sql_error)
    }

    fn verified_shared_ack_transition(
        &self,
        local_generation_id: &str,
    ) -> Result<PreparedSharedAckTransition, PeerSyncError> {
        let previous = self
            .store
            .sync_device_ack_state(&self.library_id, &self.peer_id)
            .map_err(storage_error)?;
        let next_shared = SyncGenerationIdentity::from(self.remote_base());
        let local_identity = self.sync_identity_for_local_generation(local_generation_id)?;
        let proof = self
            .store
            .verify_shared_ack_local_proof(&next_shared, &self.remote_manifest, &local_identity)
            .map_err(storage_error)?;
        Ok(PreparedSharedAckTransition {
            previous,
            next_shared,
            local_identity,
            proof,
        })
    }

    fn load_common_base_manifest(&self, base: &PeerBase) -> Result<LogicalManifest, PeerSyncError> {
        let reader = self.cas.open_object(&base.manifest_hash)?.ok_or_else(|| {
            PeerSyncError::Validation(
                "logical delta durable common-base manifest is absent from CAS".to_owned(),
            )
        })?;
        let limit = (MAX_LOGICAL_MANIFEST_BYTES as u64).saturating_add(1);
        let mut bytes = Vec::new();
        reader.take(limit).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_LOGICAL_MANIFEST_BYTES {
            return validation("logical delta durable common-base manifest exceeds its byte limit");
        }
        if hex::encode(Sha256::digest(&bytes)) != base.manifest_hash {
            return Err(PeerSyncError::WholeObjectHashMismatch {
                object: base.manifest_hash.clone(),
            });
        }
        let manifest = decode_logical_manifest(&bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        if manifest.library_id != self.library_id
            || manifest.generation != base.generation_id
            || manifest.generation_sequence != base.generation_sequence
        {
            return validation("logical delta durable common-base tuple differs from its manifest");
        }
        Ok(manifest)
    }

    fn load_local_manifest(
        &self,
        generation_id: &str,
        expected_revision: i64,
        require_active: bool,
    ) -> Result<LogicalManifest, PeerSyncError> {
        let active = active_generation(&self.store.connection).map_err(storage_error)?;
        if require_active {
            let revision = current_revision(&self.store.connection).map_err(storage_error)?;
            if revision != expected_revision {
                return Err(PeerSyncError::ActivationConflict {
                    expected: Some(expected_revision.to_string()),
                    actual: Some(revision.to_string()),
                });
            }
        }

        let metadata: Option<(String, i64)> = self
            .store
            .connection
            .query_row(
                "SELECT pds_generation, source_revision
                 FROM logical_sync_generations
                 WHERE library_id = ?1 AND generation_id = ?2
                   AND state = 'complete' AND manifest_hash IS NOT NULL",
                params![self.library_id, generation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let Some((pds_generation, source_revision)) = metadata else {
            return validation("logical delta local generation is absent or incomplete");
        };
        if source_revision != expected_revision || (require_active && pds_generation != active) {
            return validation(
                "logical delta local generation is not an exact index of the expected PDS revision",
            );
        }
        let built = scan_compact_manifest(
            &self.store.connection,
            &self.library_id,
            generation_id,
            true,
        )
        .map_err(storage_error)?;
        if built.manifest.library_id != self.library_id
            || built.manifest.generation != generation_id
            || built.manifest.source_revision != expected_revision as u64
        {
            return validation("logical delta local compact manifest identity is inconsistent");
        }
        Ok(built.manifest)
    }

    fn validate_exact_three_way_plan(
        &self,
        plan: &ReadyLogicalDeltaPlan,
        base: &LogicalManifest,
        local: &LogicalManifest,
    ) -> Result<(), PeerSyncError> {
        let local_manifest_hash = hash_logical_manifest(local)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        validate_same_generation_identity(
            &base.generation,
            &plan.expected_base_manifest_hash,
            &base.generation_sequence,
            &local.generation,
            &local_manifest_hash,
            &local.generation_sequence,
        )?;
        validate_same_generation_identity(
            &base.generation,
            &plan.expected_base_manifest_hash,
            &base.generation_sequence,
            &self.remote_manifest.generation,
            &self.remote_manifest_hash,
            &self.remote_manifest.generation_sequence,
        )?;
        validate_same_generation_identity(
            &local.generation,
            &local_manifest_hash,
            &local.generation_sequence,
            &self.remote_manifest.generation,
            &self.remote_manifest_hash,
            &self.remote_manifest.generation_sequence,
        )?;
        if compare_generation_sequences(
            &self.remote_manifest.generation_sequence,
            &base.generation_sequence,
        )
        .is_lt()
        {
            return validation("logical delta remote generation predates the common base");
        }
        if self.remote_manifest.generation_sequence == base.generation_sequence
            && (self.remote_manifest.generation != base.generation
                || self.remote_manifest_hash != plan.expected_base_manifest_hash)
        {
            return validation("logical delta remote generation reuses the common-base sequence");
        }
        validate_manifest_object_size_parity([base, local, &self.remote_manifest])?;
        let (apply, preserve_local_keys, candidate_object_hashes) =
            derive_exact_three_way_plan(base, local, &self.remote_manifest)?;
        if plan.apply != apply
            || plan.preserve_local_keys != preserve_local_keys
            || plan.candidate_object_hashes != candidate_object_hashes
        {
            return validation("logical delta caller plan differs from the authoritative merge");
        }
        Ok(())
    }

    fn validate_policy_three_way_plan(
        &self,
        plan: &ReadyLogicalDeltaPlan,
        base: &LogicalManifest,
        local: &LogicalManifest,
    ) -> Result<(), PeerSyncError> {
        if self.conflict_policy == LogicalDeltaConflictPolicy::Reject {
            return self.validate_exact_three_way_plan(plan, base, local);
        }
        let local_manifest_hash = hash_logical_manifest(local)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        validate_same_generation_identity(
            &base.generation,
            &plan.expected_base_manifest_hash,
            &base.generation_sequence,
            &local.generation,
            &local_manifest_hash,
            &local.generation_sequence,
        )?;
        validate_same_generation_identity(
            &base.generation,
            &plan.expected_base_manifest_hash,
            &base.generation_sequence,
            &self.remote_manifest.generation,
            &self.remote_manifest_hash,
            &self.remote_manifest.generation_sequence,
        )?;
        validate_same_generation_identity(
            &local.generation,
            &local_manifest_hash,
            &local.generation_sequence,
            &self.remote_manifest.generation,
            &self.remote_manifest_hash,
            &self.remote_manifest.generation_sequence,
        )?;
        if compare_generation_sequences(
            &self.remote_manifest.generation_sequence,
            &base.generation_sequence,
        )
        .is_lt()
        {
            return validation("logical delta remote generation predates the common base");
        }
        if self.remote_manifest.generation_sequence == base.generation_sequence
            && (self.remote_manifest.generation != base.generation
                || self.remote_manifest_hash != plan.expected_base_manifest_hash)
        {
            return validation("logical delta remote generation reuses the common-base sequence");
        }
        validate_manifest_object_size_parity([base, local, &self.remote_manifest])?;
        let resolved =
            derive_policy_three_way_plan(base, local, &self.remote_manifest, self.conflict_policy)?;
        if plan.apply != resolved.apply
            || plan.preserve_local_keys != resolved.preserve_local_keys
            || plan.candidate_object_hashes != resolved.candidate_object_hashes
        {
            return validation("logical delta caller plan differs from the authoritative merge");
        }
        Ok(())
    }

    fn validate_already_active(
        &self,
        plan: &ReadyLogicalDeltaPlan,
        actual_revision: i64,
    ) -> Result<(), PeerSyncError> {
        let changed = !plan.apply.is_empty();
        let expected_revision = if changed {
            plan.expected_local_revision
                .checked_add(1)
                .ok_or_else(|| PeerSyncError::Validation("logical revision overflow".to_owned()))?
        } else {
            plan.expected_local_revision
        };
        if actual_revision != expected_revision {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        let active = active_generation(&self.store.connection).map_err(storage_error)?;
        if !current_logical_head_matches(
            &self.store.connection,
            &self.library_id,
            &self.local_generation_id,
            &active,
            actual_revision,
            changed,
        )? {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        Ok(())
    }

    fn deferred_active_result(
        &self,
        plan: &ReadyLogicalDeltaPlan,
        base: &PeerBase,
        actual_revision: i64,
    ) -> Result<Option<DeferredActiveResult>, PeerSyncError> {
        if self.activation_mode != LogicalDeltaActivationMode::DeferPeerState
            || plan.apply.is_empty()
            || actual_revision == plan.expected_local_revision
        {
            return Ok(None);
        }
        let expected_revision = plan
            .expected_local_revision
            .checked_add(1)
            .ok_or_else(|| PeerSyncError::Validation("logical revision overflow".to_owned()))?;
        if actual_revision != expected_revision {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        let base_manifest = self.load_common_base_manifest(base)?;
        let local_manifest = self.load_local_manifest(
            &self.local_generation_id,
            plan.expected_local_revision,
            false,
        )?;
        self.validate_policy_three_way_plan(plan, &base_manifest, &local_manifest)?;
        let active_generation = self.current_logical_head_generation()?;
        let active_manifest =
            self.load_local_manifest(&active_generation, actual_revision, true)?;
        let expected_sequence = merged_generation_sequence(
            &local_manifest.generation_sequence,
            &self.remote_manifest.generation_sequence,
        )?;
        let (expected_records, expected_objects) =
            exact_merged_content(&local_manifest, &self.remote_manifest, plan)?;
        if active_manifest.schema != local_manifest.schema
            || active_manifest.library_id != local_manifest.library_id
            || active_manifest.parent_generation.as_deref()
                != Some(local_manifest.generation.as_str())
            || active_manifest.generation_sequence != expected_sequence
            || active_manifest.records != expected_records
            || active_manifest.objects != expected_objects
        {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        let manifest_hash = hash_logical_manifest(&active_manifest)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        Ok(Some(DeferredActiveResult {
            expected_base: base.clone(),
            local_identity: SyncGenerationIdentity {
                generation_id: active_manifest.generation,
                manifest_hash,
                generation_sequence: active_manifest.generation_sequence,
            },
        }))
    }

    fn object_size(&self, hash: &str) -> Result<u64, PeerSyncError> {
        let index = self
            .remote_manifest
            .objects
            .binary_search_by(|object| object.hash.as_str().cmp(hash))
            .map_err(|_| {
                PeerSyncError::Validation(format!(
                    "logical delta object {hash} is absent from the verified manifest"
                ))
            })?;
        Ok(self.remote_manifest.objects[index].size)
    }

    fn load_object(
        &self,
        staged_objects: &BTreeMap<String, PathBuf>,
        hash: &str,
    ) -> Result<Vec<u8>, PeerSyncError> {
        let expected_size = self.object_size(hash)?;
        let bytes = if let Some(path) = staged_objects.get(hash) {
            fs::read(path)?
        } else {
            let referenced_locally: bool = self
                .store
                .connection
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM logical_record_heads
                        WHERE library_id = ?1 AND generation_id = ?2
                          AND state = 'live' AND object_hash = ?3
                        UNION ALL
                        SELECT 1 FROM logical_record_dependencies
                        WHERE library_id = ?1 AND generation_id = ?2 AND object_hash = ?3
                     )",
                    params![self.library_id, self.local_generation_id, hash],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if referenced_locally {
                self.store
                    .reconstruct_logical_object(
                        self.cas,
                        &self.library_id,
                        &self.local_generation_id,
                        hash,
                    )
                    .map_err(storage_error)?
            } else {
                self.cas.read_object(hash)?.ok_or_else(|| {
                    PeerSyncError::Validation(format!(
                        "logical delta object {hash} is unavailable after transfer"
                    ))
                })?
            }
        };
        verify_bytes(&bytes, hash, expected_size)?;
        Ok(bytes)
    }

    fn promote_exact(&self, hash: &str, bytes: &[u8]) -> Result<(), PeerSyncError> {
        let expected_size = self.object_size(hash)?;
        verify_bytes(bytes, hash, expected_size)?;
        let prepared = self.prepare_bytes(bytes)?;
        if prepared.content_hash != hash || prepared.byte_size != expected_size {
            return validation("logical delta CAS promotion changed object identity");
        }
        Ok(())
    }

    fn promote_payload_object(
        &self,
        staged_objects: &BTreeMap<String, PathBuf>,
        hash: &str,
    ) -> Result<(), PeerSyncError> {
        let expected_size = self.object_size(hash)?;
        if self.same_run_job_prepared_objects.contains(hash) {
            if self.cas.stat_object(hash)? != Some(expected_size) {
                return validation(
                    "logical delta same-run CAS object differs from its verified preparation",
                );
            }
            return Ok(());
        }
        if let Some(path) = staged_objects.get(hash) {
            #[cfg(test)]
            self.payload_reprepare_count
                .set(self.payload_reprepare_count.get() + 1);
            let mut file = fs::File::open(path)?;
            let prepared = self.prepare_reader(&mut file)?;
            if prepared.content_hash != hash || prepared.byte_size != expected_size {
                return validation("logical delta streamed CAS promotion changed object identity");
            }
            return Ok(());
        }
        let mut reader = self.cas.open_object(hash)?.ok_or_else(|| {
            PeerSyncError::Validation(format!(
                "logical delta payload {hash} is unavailable for CAS promotion"
            ))
        })?;
        verify_reader(&mut reader, hash, expected_size)?;
        if let Some(job) = self.durable_job {
            job.borrow_mut().pin_existing(
                self.cas,
                hash,
                expected_size,
                CasObjectRole::DirectObject,
            )?;
        }
        Ok(())
    }

    fn prepare_put(
        &self,
        staged_objects: &BTreeMap<String, PathBuf>,
        key: &str,
        object_hash: &str,
        dependencies: &[String],
    ) -> Result<PreparedPut, PeerSyncError> {
        let locator = decode_logical_record_key(key)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let bytes = self.load_object(staged_objects, object_hash)?;
        let mut envelope = decode_logical_record(&bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        validate_locator_envelope(&locator, &envelope)?;
        let declared = dependencies.iter().cloned().collect::<BTreeSet<_>>();
        if declared.len() != dependencies.len() {
            return validation("logical delta dependencies are not unique");
        }
        let mut actual = BTreeSet::new();
        let mut pages = Vec::new();
        match &mut envelope {
            LogicalRecordEnvelope::Root { value, owner_heads } => {
                actual =
                    self.resolve_and_rehydrate_owners(staged_objects, value, owner_heads, None)?;
            }
            LogicalRecordEnvelope::Character {
                detail,
                owner_heads,
                ..
            } => {
                let LogicalRecordLocator::Character { character_id } = &locator else {
                    unreachable!("locator and envelope validated")
                };
                actual = self.resolve_and_rehydrate_owners(
                    staged_objects,
                    detail,
                    owner_heads,
                    Some(character_id),
                )?;
            }
            LogicalRecordEnvelope::Conversation {
                message_page_hashes,
                ..
            } => {
                for (index, page_hash) in message_page_hashes.iter().enumerate() {
                    actual.insert(page_hash.clone());
                    let page_bytes = self.load_object(staged_objects, page_hash)?;
                    let page = decode_message_page(&page_bytes)
                        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
                    let final_page = index + 1 == message_page_hashes.len();
                    if page.is_empty() || (!final_page && page.len() != LOGICAL_MESSAGE_PAGE_SIZE) {
                        return validation(
                            "logical conversation pages do not use complete 128-message boundaries",
                        );
                    }
                    pages.push(PreparedPage {
                        hash: page_hash.clone(),
                        size: self.object_size(page_hash)?,
                        message_count: page.len() as u64,
                    });
                }
            }
            LogicalRecordEnvelope::Asset {
                object_hash,
                size,
                metadata,
            }
            | LogicalRecordEnvelope::Inlay {
                object_hash,
                size,
                metadata,
            } => {
                let typed = decode_asset_alias_metadata(metadata)
                    .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
                match (
                    &locator,
                    typed.inlay_type.as_deref(),
                    typed.width,
                    typed.height,
                ) {
                    (LogicalRecordLocator::Asset { .. }, None, None, None) => {}
                    (LogicalRecordLocator::Inlay { .. }, Some(_), _, _) => {}
                    (LogicalRecordLocator::Asset { .. }, _, _, _) => {
                        return validation("ordinary asset contains Inlay-only typed metadata")
                    }
                    (LogicalRecordLocator::Inlay { .. }, _, _, _) => {
                        return validation("Inlay alias is missing its inlayType")
                    }
                    _ => unreachable!("locator and envelope validated"),
                }
                if let Some(payload_hash) = object_hash {
                    if self.object_size(payload_hash)? != *size {
                        return validation(
                            "logical asset payload size differs from its alias declaration",
                        );
                    }
                    self.promote_payload_object(staged_objects, payload_hash)?;
                    actual.insert(payload_hash.clone());
                }
            }
            LogicalRecordEnvelope::Cold {
                object_hash,
                size,
                metadata,
            } => {
                if !metadata.is_object() {
                    return validation("logical cold alias metadata must be an object");
                }
                if let Some(payload_hash) = object_hash {
                    if self.object_size(payload_hash)? != *size {
                        return validation(
                            "logical cold payload size differs from its alias declaration",
                        );
                    }
                    self.promote_payload_object(staged_objects, payload_hash)?;
                    actual.insert(payload_hash.clone());
                }
            }
            LogicalRecordEnvelope::Preset { .. } | LogicalRecordEnvelope::Plugin { .. } => {}
        }
        if actual != declared {
            return validation("logical record dependency graph does not exactly match its value");
        }
        let dependencies = dependencies
            .iter()
            .map(|hash| Ok((hash.clone(), self.object_size(hash)?)))
            .collect::<Result<Vec<_>, PeerSyncError>>()?;
        Ok(PreparedPut {
            key: key.to_owned(),
            locator,
            envelope,
            object_hash: object_hash.to_owned(),
            object_size: self.object_size(object_hash)?,
            dependencies,
            pages,
        })
    }

    fn apply_one_delete(
        &mut self,
        staging_id: &str,
        logical_generation_id: &str,
        key: &str,
        deleted_generation_sequence: &str,
    ) -> Result<(), PeerSyncError> {
        let locator = decode_logical_record_key(key)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        if matches!(locator, LogicalRecordLocator::Root) {
            return validation("logical delta cannot delete the root record");
        }
        let delete = PreparedDelete {
            key: key.to_owned(),
            locator,
            deleted_generation_sequence: deleted_generation_sequence.to_owned(),
        };
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        apply_delete(&transaction, staging_id, &delete.locator)?;
        apply_logical_index_operations(
            &transaction,
            &self.library_id,
            logical_generation_id,
            &[],
            std::slice::from_ref(&delete),
        )?;
        transaction.commit().map_err(sql_error)
    }

    fn apply_one_put(
        &mut self,
        staged_objects: &BTreeMap<String, PathBuf>,
        staging_id: &str,
        logical_generation_id: &str,
        key: &str,
        object_hash: &str,
        dependencies: &[String],
    ) -> Result<(), PeerSyncError> {
        let put = self.prepare_put(staged_objects, key, object_hash, dependencies)?;
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        apply_put(&transaction, staging_id, &put)?;
        apply_logical_index_operations(
            &transaction,
            &self.library_id,
            logical_generation_id,
            std::slice::from_ref(&put),
            &[],
        )?;
        transaction.commit().map_err(sql_error)?;
        self.insert_conversation_pages(staged_objects, staging_id, std::slice::from_ref(&put))
    }

    fn resolve_and_rehydrate_owners(
        &self,
        staged_objects: &BTreeMap<String, PathBuf>,
        parent: &mut Value,
        heads: &[LogicalOwnerHead],
        character_id: Option<&str>,
    ) -> Result<BTreeSet<String>, PeerSyncError> {
        let mut dependencies = BTreeSet::new();
        let mut resolved = Vec::with_capacity(heads.len());
        for head in heads {
            let Some(manifest_hash) = head.manifest_hash.as_deref() else {
                resolved.push(ResolvedOwnerHead {
                    head: head.clone(),
                    tuples: None,
                });
                continue;
            };
            let manifest = self.load_object(staged_objects, manifest_hash)?;
            let entries = decode_owner_manifest(&manifest)
                .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
            if encode_owner_manifest(&entries)
                .map_err(|error| PeerSyncError::Validation(error.to_string()))?
                != manifest
            {
                return validation("logical owner manifest bytes are not canonical");
            }
            if entries.len() as u64 != head.entry_count {
                return validation("logical owner manifest entry count differs from its head");
            }
            self.promote_exact(manifest_hash, &manifest)?;
            dependencies.insert(manifest_hash.to_owned());
            let mut tuples = Vec::with_capacity(entries.len());
            for entry in entries {
                tuples.push(Value::Array(
                    entry.tuple.into_iter().map(Value::String).collect(),
                ));
                if let Some(payload_hash) = entry.payload_hash {
                    let payload_hash = hex::encode(payload_hash);
                    self.promote_payload_object(staged_objects, &payload_hash)?;
                    dependencies.insert(payload_hash);
                }
            }
            resolved.push(ResolvedOwnerHead {
                head: head.clone(),
                tuples: Some(tuples),
            });
        }
        match character_id {
            Some(character_id) => {
                rehydrate_character_owner(parent, character_id, &resolved)?;
            }
            None => rehydrate_root_owners(parent, &resolved)?,
        }
        Ok(dependencies)
    }

    fn insert_conversation_pages(
        &mut self,
        staged_objects: &BTreeMap<String, PathBuf>,
        staging_id: &str,
        puts: &[PreparedPut],
    ) -> Result<(), PeerSyncError> {
        for put in puts {
            let LogicalRecordLocator::Conversation {
                character_id,
                conversation_id,
            } = &put.locator
            else {
                continue;
            };
            let mut first_message_index = 0_u64;
            for (page_index, page) in put.pages.iter().enumerate() {
                let bytes = self.load_object(staged_objects, &page.hash)?;
                let messages = decode_message_page(&bytes)
                    .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
                if messages.len() as u64 != page.message_count
                    || messages.is_empty()
                    || (page_index + 1 != put.pages.len()
                        && messages.len() != LOGICAL_MESSAGE_PAGE_SIZE)
                {
                    return validation("logical conversation page changed after staged validation");
                }
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let mut statement = transaction
                    .prepare_cached(
                        "INSERT INTO messages (
                            generation, character_id, conversation_id,
                            message_index, message_id, value
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .map_err(sql_error)?;
                for (offset, message) in messages.iter().enumerate() {
                    let message_index =
                        first_message_index
                            .checked_add(offset as u64)
                            .ok_or_else(|| {
                                PeerSyncError::Validation(
                                    "logical message index overflow".to_owned(),
                                )
                            })?;
                    statement
                        .execute(params![
                            staging_id,
                            character_id,
                            conversation_id,
                            sqlite_i64(message_index, "message index")?,
                            message.get("chatId").and_then(Value::as_str),
                            serde_json::to_string(message).map_err(json_error)?,
                        ])
                        .map_err(sql_error)?;
                }
                drop(statement);
                transaction.commit().map_err(sql_error)?;
                first_message_index = first_message_index
                    .checked_add(page.message_count)
                    .ok_or_else(|| {
                        PeerSyncError::Validation("logical message index overflow".to_owned())
                    })?;
            }
        }
        Ok(())
    }

    fn initialize_changed_stage(
        &mut self,
        stage: &mut PersistentLogicalDeltaStage,
    ) -> Result<(), PeerSyncError> {
        let (
            expected_base,
            expected_local_revision,
            staging_id,
            pin_lease_id,
            staging_directory,
            initialized,
        ) = match stage {
            PersistentLogicalDeltaStage::Changed {
                expected_base,
                expected_local_revision,
                staging_id,
                pin_lease_id,
                staging_directory,
                initialized,
                ..
            } => (
                expected_base.clone(),
                *expected_local_revision,
                staging_id.clone(),
                pin_lease_id.clone(),
                staging_directory.clone(),
                initialized,
            ),
            _ => return Ok(()),
        };
        if *initialized {
            return Ok(());
        }
        let maintenance_guard = ActiveTempGuard::acquire(&staging_directory)?;
        fs::create_dir_all(&self.staging_root)?;
        if let Err(error) = fs::create_dir(&staging_directory) {
            let _ = fs::remove_dir(&self.staging_root);
            return Err(PeerSyncError::Storage(error.to_string()));
        }
        let result = (|| {
            let transaction = self
                .store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql_error)?;
            let active = active_generation(&transaction).map_err(storage_error)?;
            let revision = current_revision(&transaction).map_err(storage_error)?;
            if revision != expected_local_revision {
                return Err(PeerSyncError::ActivationConflict {
                    expected: Some(expected_local_revision.to_string()),
                    actual: Some(revision.to_string()),
                });
            }
            let transaction_base = common_base(&transaction, &self.peer_id, &self.library_id)?;
            if transaction_base.as_ref() != Some(&expected_base) {
                return Err(PeerSyncError::ActivationConflict {
                    expected: Some(expected_base.manifest_hash.clone()),
                    actual: transaction_base.map(|base| base.manifest_hash),
                });
            }
            clone_generation(&transaction, &active, &staging_id)?;
            transaction
                .execute(
                    "INSERT INTO snapshot_leases (
                        lease, generation, revision, created_at
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![pin_lease_id, active, revision, unix_millis()?],
                )
                .map_err(sql_error)?;
            transaction.commit().map_err(sql_error)
        })();
        if let Err(error) = result {
            let _ = remove_staging_directory(&self.staging_root, &staging_directory);
            return Err(error);
        }
        let PersistentLogicalDeltaStage::Changed {
            initialized,
            maintenance_guard: stage_guard,
            ..
        } = stage
        else {
            unreachable!();
        };
        *stage_guard = Some(maintenance_guard);
        *initialized = true;
        Ok(())
    }
}

struct ExactSizeReader<'a> {
    inner: &'a mut dyn Read,
    remaining: u64,
    checked_trailing: bool,
}

impl<'a> ExactSizeReader<'a> {
    fn new(inner: &'a mut dyn Read, expected_size: u64) -> Self {
        Self {
            inner,
            remaining: expected_size,
            checked_trailing: false,
        }
    }
}

impl Read for ExactSizeReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining > 0 {
            let limit = usize::try_from(self.remaining)
                .unwrap_or(usize::MAX)
                .min(output.len());
            let read = self.inner.read(&mut output[..limit])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "logical delta staged object is incomplete",
                ));
            }
            self.remaining -= read as u64;
            return Ok(read);
        }
        if !self.checked_trailing {
            let mut trailing = [0_u8; 1];
            if self.inner.read(&mut trailing)? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "logical delta staged object exceeds its declared size",
                ));
            }
            self.checked_trailing = true;
        }
        Ok(0)
    }
}

impl LogicalDeltaStagedTarget for PersistentLogicalDeltaTarget<'_> {
    type Stage = PersistentLogicalDeltaStage;

    fn begin(&mut self, plan: &ReadyLogicalDeltaPlan) -> Result<Self::Stage, PeerSyncError> {
        self.validate_plan_identity(plan)?;
        if plan.expected_local_revision < 0 {
            return validation("logical delta expected local revision must be nonnegative");
        }
        let Some(actual_base) = self.common_base()? else {
            return validation("logical delta target has no durable common base for this peer");
        };
        let actual_revision = current_revision(&self.store.connection).map_err(storage_error)?;
        if self.activation_mode != LogicalDeltaActivationMode::DeferPeerState
            && actual_base == self.remote_base()
        {
            self.validate_already_active(plan, actual_revision)?;
            return Ok(PersistentLogicalDeltaStage::AlreadyActive {
                revision: actual_revision,
                changed: !plan.apply.is_empty(),
                deferred_result: None,
            });
        }
        if actual_base.manifest_hash != plan.expected_base_manifest_hash {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(plan.expected_base_manifest_hash.clone()),
                actual: Some(actual_base.manifest_hash),
            });
        }
        if let Some(deferred_result) =
            self.deferred_active_result(plan, &actual_base, actual_revision)?
        {
            return Ok(PersistentLogicalDeltaStage::AlreadyActive {
                revision: actual_revision,
                changed: true,
                deferred_result: Some(deferred_result),
            });
        }
        let base_manifest = self.load_common_base_manifest(&actual_base)?;
        let local_manifest = self.load_local_manifest(
            &self.local_generation_id,
            plan.expected_local_revision,
            true,
        )?;
        self.validate_policy_three_way_plan(plan, &base_manifest, &local_manifest)?;
        if plan.apply.is_empty() {
            return Ok(PersistentLogicalDeltaStage::NoOp {
                expected_base: actual_base,
                database_staged: false,
            });
        }
        let merged_generation_sequence = merged_generation_sequence(
            &local_manifest.generation_sequence,
            &self.remote_manifest.generation_sequence,
        )?;
        let staging_id = format!("staging-logical-{}", uuid::Uuid::new_v4());
        let logical_generation_id = format!("local-{}", uuid::Uuid::new_v4());
        let pin_lease_id = format!("logical-delta-pin-{}", uuid::Uuid::new_v4());
        let staging_directory = self.staging_root.join(&staging_id);
        Ok(PersistentLogicalDeltaStage::Changed {
            expected_base: actual_base,
            expected_local_revision: plan.expected_local_revision,
            staging_id,
            logical_generation_id,
            merged_generation_sequence,
            pin_lease_id,
            staging_directory,
            maintenance_guard: None,
            staged_objects: BTreeMap::new(),
            database_staged: false,
            initialized: false,
        })
    }

    fn can_activate_without_transfer(&self, stage: &Self::Stage) -> bool {
        matches!(stage, PersistentLogicalDeltaStage::AlreadyActive { .. })
    }

    fn stage_payload(
        &mut self,
        stage: &mut Self::Stage,
        object: &LogicalDeltaObject,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError> {
        self.initialize_changed_stage(stage)?;
        let (staging_directory, staged_objects, database_staged) = match stage {
            PersistentLogicalDeltaStage::AlreadyActive { .. } => return Ok(()),
            PersistentLogicalDeltaStage::NoOp { .. } => {
                return validation("a no-op logical delta cannot stage payload objects")
            }
            PersistentLogicalDeltaStage::Changed {
                staging_directory,
                staged_objects,
                database_staged,
                ..
            } => (staging_directory, staged_objects, database_staged),
        };
        if *database_staged || staged_objects.contains_key(&object.hash) {
            return validation("logical delta object was staged more than once");
        }
        if self.object_size(&object.hash)? != object.size {
            return validation("logical delta staged object size differs from its manifest");
        }
        if let Some(job) = self.durable_job {
            let mut exact = ExactSizeReader::new(reader, object.size);
            job.borrow_mut().prepare_reader_expected(
                self.cas,
                &mut exact,
                &object.hash,
                object.size,
                CasObjectRole::DirectObject,
            )?;
            let path = self.cas.object_path(&object.hash)?.ok_or_else(|| {
                PeerSyncError::Storage(
                    "logical delta streamed object is absent after CAS promotion".to_owned(),
                )
            })?;
            staged_objects.insert(object.hash.clone(), path);
            self.same_run_job_prepared_objects
                .insert(object.hash.clone());
            return Ok(());
        }
        let path = staging_directory.join(&object.hash);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let copied = {
            let mut declared = (&mut *reader).take(object.size);
            std::io::copy(&mut declared, &mut file)?
        };
        if copied != object.size {
            return validation("logical delta staged object has an incomplete declared size");
        }
        let mut trailing = [0_u8; 1];
        if reader.read(&mut trailing)? != 0 {
            return validation("logical delta staged object exceeds its declared size");
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);
        staged_objects.insert(object.hash.clone(), path);
        Ok(())
    }

    fn stage_database_changes(
        &mut self,
        stage: &mut Self::Stage,
        plan: &ReadyLogicalDeltaPlan,
    ) -> Result<(), PeerSyncError> {
        self.validate_plan_identity(plan)?;
        if let PersistentLogicalDeltaStage::NoOp { expected_base, .. }
        | PersistentLogicalDeltaStage::Changed { expected_base, .. } = &*stage
        {
            let actual_base = self.common_base()?;
            if actual_base.as_ref() != Some(expected_base) {
                return Err(PeerSyncError::ActivationConflict {
                    expected: Some(expected_base.manifest_hash.clone()),
                    actual: actual_base.map(|base| base.manifest_hash),
                });
            }
            let base_manifest = self.load_common_base_manifest(expected_base)?;
            let local_manifest = self.load_local_manifest(
                &self.local_generation_id,
                plan.expected_local_revision,
                true,
            )?;
            self.validate_policy_three_way_plan(plan, &base_manifest, &local_manifest)?;
        }
        self.initialize_changed_stage(stage)?;
        match stage {
            PersistentLogicalDeltaStage::AlreadyActive { .. } => Ok(()),
            PersistentLogicalDeltaStage::NoOp {
                database_staged, ..
            } => {
                if !plan.apply.is_empty() || !plan.candidate_object_hashes.is_empty() {
                    return validation("logical delta no-op stage contains changed objects");
                }
                *database_staged = true;
                Ok(())
            }
            PersistentLogicalDeltaStage::Changed {
                staging_id,
                logical_generation_id,
                merged_generation_sequence,
                pin_lease_id,
                staging_directory,
                staged_objects,
                database_staged,
                ..
            } => {
                if *database_staged {
                    return validation("logical delta database changes were staged twice");
                }
                let phases = classify_plan_operations(plan)?;
                validate_character_deletes(&self.store.connection, staging_id, plan)?;
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                begin_compact_logical_index(
                    &transaction,
                    &self.library_id,
                    &self.local_generation_id,
                    logical_generation_id,
                    staging_id,
                    plan.expected_local_revision + 1,
                    merged_generation_sequence,
                )?;
                transaction.commit().map_err(sql_error)?;

                for expected_phase in [
                    OperationPhase::DeleteConversation,
                    OperationPhase::DeleteCharacter,
                    OperationPhase::DeleteOther,
                    OperationPhase::PutOther,
                    OperationPhase::PutConversation,
                ] {
                    for (index, phase) in phases.iter().enumerate() {
                        if *phase != expected_phase {
                            continue;
                        }
                        match &plan.apply[index] {
                            LogicalDeltaApplyOperation::Delete {
                                key,
                                deleted_generation_sequence,
                            } => self.apply_one_delete(
                                staging_id,
                                logical_generation_id,
                                key,
                                deleted_generation_sequence,
                            )?,
                            LogicalDeltaApplyOperation::Put {
                                key,
                                object_hash,
                                dependencies,
                            } => self.apply_one_put(
                                staged_objects,
                                staging_id,
                                logical_generation_id,
                                key,
                                object_hash,
                                dependencies,
                            )?,
                        }
                    }
                }

                let cas = self.cas;
                let durable_job = self.durable_job;
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                refresh_touched_character_counts(&transaction, staging_id, plan)?;
                validate_configured_index_uniqueness(&transaction, staging_id)?;
                let built = scan_compact_manifest(
                    &transaction,
                    &self.library_id,
                    logical_generation_id,
                    false,
                )
                .map_err(storage_error)?;
                let prepared = match durable_job {
                    Some(job) => job.borrow_mut().prepare_bytes(
                        cas,
                        &built.manifest_bytes,
                        CasObjectRole::DirectObject,
                    )?,
                    None => cas.prepare_bytes(&built.manifest_bytes)?,
                };
                if prepared.content_hash != built.manifest_hash
                    || prepared.byte_size != built.manifest_bytes.len() as u64
                {
                    return validation(
                        "logical delta staged manifest CAS identity is inconsistent",
                    );
                }
                let completed = transaction
                    .execute(
                        "UPDATE logical_sync_generations
                         SET state = 'complete', manifest_hash = ?3, completed_at = ?4
                         WHERE library_id = ?1 AND generation_id = ?2 AND state = 'building'",
                        params![
                            self.library_id,
                            logical_generation_id.as_str(),
                            built.manifest_hash,
                            unix_millis()?,
                        ],
                    )
                    .map_err(sql_error)?;
                if completed != 1 {
                    return validation("logical delta staged index did not become complete");
                }
                let released = transaction
                    .execute(
                        "DELETE FROM snapshot_leases WHERE lease = ?1",
                        [pin_lease_id.as_str()],
                    )
                    .map_err(sql_error)?;
                if released != 1 {
                    return validation(
                        "logical delta PDS pin disappeared before staging completed",
                    );
                }
                transaction.commit().map_err(sql_error)?;
                remove_staging_directory(&self.staging_root, staging_directory)?;
                *database_staged = true;
                Ok(())
            }
        }
    }

    fn prepare_activation(&mut self, _stage: &mut Self::Stage) -> Result<(), PeerSyncError> {
        self.seal_durable_job()
    }

    fn activate_database_and_base_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_local_revision: i64,
        expected_base_manifest_hash: &str,
        next_base_manifest_hash: &str,
        next_base_generation_sequence: &str,
    ) -> Result<LogicalDeltaActivation, PeerSyncError> {
        if next_base_manifest_hash != self.remote_manifest_hash
            || next_base_generation_sequence != self.remote_manifest.generation_sequence
        {
            return validation("logical delta activation differs from its verified remote base");
        }
        self.seal_durable_job()?;
        match stage {
            PersistentLogicalDeltaStage::AlreadyActive {
                revision,
                changed,
                deferred_result,
            } => {
                let original_revision = if *changed {
                    revision.checked_sub(1).ok_or_else(|| {
                        PeerSyncError::Validation("logical revision underflow".to_owned())
                    })?
                } else {
                    *revision
                };
                if expected_local_revision != original_revision {
                    return validation("logical delta retry revision differs from its stage");
                }
                let remote_base = self.remote_base();
                if self.activation_mode == LogicalDeltaActivationMode::AdvanceSharedAck {
                    let local_head = self.current_logical_head_generation()?;
                    let shared_ack = self.verified_shared_ack_transition(&local_head)?;
                    if shared_ack.previous.shared_identity
                        != SyncGenerationIdentity::from(remote_base.clone())
                        || shared_ack.previous.local_identity != shared_ack.local_identity
                    {
                        return validation(
                            "P5 completed acknowledgement differs from its local witness",
                        );
                    }
                }
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let actual_revision = current_revision(&transaction).map_err(storage_error)?;
                let actual_base = common_base(&transaction, &self.peer_id, &self.library_id)?;
                let active = active_generation(&transaction).map_err(storage_error)?;
                let deferred_result = deferred_result.as_ref();
                let expected_base = deferred_result
                    .map(|result| &result.expected_base)
                    .unwrap_or(&remote_base);
                let head_matches = match deferred_result {
                    Some(result) => {
                        active_logical_identity(
                            &transaction,
                            &self.library_id,
                            &active,
                            actual_revision,
                        )?
                        .as_ref()
                            == Some(&result.local_identity)
                    }
                    None => current_logical_head_matches(
                        &transaction,
                        &self.library_id,
                        &self.local_generation_id,
                        &active,
                        actual_revision,
                        *changed,
                    )?,
                };
                if actual_revision != *revision
                    || actual_base.as_ref() != Some(expected_base)
                    || !head_matches
                {
                    return Ok(LogicalDeltaActivation::Conflict {
                        actual_revision,
                        actual_base_manifest_hash: actual_base
                            .map(|base| base.manifest_hash)
                            .unwrap_or_default(),
                    });
                }
                transaction.commit().map_err(sql_error)?;
                Ok(LogicalDeltaActivation::AlreadyActive {
                    revision: actual_revision,
                })
            }
            PersistentLogicalDeltaStage::NoOp {
                expected_base,
                database_staged,
            } => {
                if !*database_staged {
                    return validation("logical delta database stage is incomplete");
                }
                if expected_base.manifest_hash != expected_base_manifest_hash {
                    return validation("logical delta activation base differs from its stage");
                }
                let shared_ack =
                    if self.activation_mode == LogicalDeltaActivationMode::AdvanceSharedAck {
                        Some(self.verified_shared_ack_transition(&self.local_generation_id)?)
                    } else {
                        None
                    };
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let actual_revision = current_revision(&transaction).map_err(storage_error)?;
                let actual_base = common_base(&transaction, &self.peer_id, &self.library_id)?;
                if actual_revision != expected_local_revision
                    || actual_base.as_ref() != Some(&*expected_base)
                {
                    return Ok(LogicalDeltaActivation::Conflict {
                        actual_revision,
                        actual_base_manifest_hash: actual_base
                            .map(|base| base.manifest_hash)
                            .unwrap_or_default(),
                    });
                }
                advance_peer_state_in_transaction(
                    &transaction,
                    self.activation_mode,
                    &self.peer_id,
                    &self.library_id,
                    &self.remote_manifest.generation,
                    next_base_manifest_hash,
                    next_base_generation_sequence,
                    expected_base,
                    shared_ack,
                )?;
                transaction.commit().map_err(sql_error)?;
                Ok(LogicalDeltaActivation::Activated {
                    revision: actual_revision,
                })
            }
            PersistentLogicalDeltaStage::Changed {
                expected_base,
                staging_id,
                logical_generation_id,
                database_staged,
                ..
            } => {
                if !*database_staged {
                    return validation("logical delta database stage is incomplete");
                }
                if expected_base.manifest_hash != expected_base_manifest_hash {
                    return validation("logical delta activation base differs from its stage");
                }
                let shared_ack =
                    if self.activation_mode == LogicalDeltaActivationMode::AdvanceSharedAck {
                        Some(self.verified_shared_ack_transition(logical_generation_id)?)
                    } else {
                        None
                    };
                let transaction = self
                    .store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(sql_error)?;
                let actual_revision = current_revision(&transaction).map_err(storage_error)?;
                let actual_base = common_base(&transaction, &self.peer_id, &self.library_id)?;
                if actual_revision != expected_local_revision
                    || actual_base.as_ref() != Some(&*expected_base)
                {
                    return Ok(LogicalDeltaActivation::Conflict {
                        actual_revision,
                        actual_base_manifest_hash: actual_base
                            .map(|base| base.manifest_hash)
                            .unwrap_or_default(),
                    });
                }
                let indexed: bool = transaction
                    .query_row(
                        "SELECT EXISTS(
                            SELECT 1 FROM logical_sync_generations
                            WHERE library_id = ?1 AND generation_id = ?2
                              AND pds_generation = ?3 AND source_revision = ?4
                              AND state = 'complete' AND manifest_hash IS NOT NULL
                         )",
                        params![
                            self.library_id,
                            logical_generation_id.as_str(),
                            staging_id.as_str(),
                            expected_local_revision + 1,
                        ],
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                if !indexed {
                    return validation("logical delta staging index is incomplete");
                }
                let next_revision = expected_local_revision + 1;
                let active_generation = format!("revision-{next_revision}");
                let occupied: bool = transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM root WHERE generation = ?1)",
                        [&active_generation],
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                if occupied {
                    return validation("logical delta target revision generation already exists");
                }
                move_generation(&transaction, staging_id, &active_generation)?;
                let remapped = transaction
                    .execute(
                        "UPDATE logical_sync_generations SET pds_generation = ?3
                         WHERE library_id = ?1 AND generation_id = ?2
                           AND pds_generation = ?4 AND state = 'complete'",
                        params![
                            self.library_id,
                            logical_generation_id.as_str(),
                            active_generation,
                            staging_id.as_str(),
                        ],
                    )
                    .map_err(sql_error)?;
                if remapped != 1 {
                    return validation(
                        "logical delta complete index did not follow PDS activation",
                    );
                }
                let moved_head = transaction
                    .execute(
                        "UPDATE logical_library_head SET generation_id = ?3
                         WHERE singleton = 1 AND library_id = ?1 AND generation_id = ?2",
                        params![
                            self.library_id,
                            self.local_generation_id,
                            logical_generation_id.as_str(),
                        ],
                    )
                    .map_err(sql_error)?;
                if moved_head != 1 {
                    return validation("logical delta library head changed before PDS activation");
                }
                set_active(&transaction, next_revision, &active_generation)?;
                advance_peer_state_in_transaction(
                    &transaction,
                    self.activation_mode,
                    &self.peer_id,
                    &self.library_id,
                    &self.remote_manifest.generation,
                    next_base_manifest_hash,
                    next_base_generation_sequence,
                    expected_base,
                    shared_ack,
                )?;
                transaction.commit().map_err(sql_error)?;
                Ok(LogicalDeltaActivation::Activated {
                    revision: next_revision,
                })
            }
        }
    }

    fn abort(&mut self, stage: Self::Stage) -> Result<(), PeerSyncError> {
        self.durable_abort_succeeded = false;
        let PersistentLogicalDeltaStage::Changed {
            staging_id,
            logical_generation_id,
            pin_lease_id,
            staging_directory,
            maintenance_guard: _maintenance_guard,
            initialized,
            ..
        } = stage
        else {
            self.durable_abort_succeeded = true;
            return Ok(());
        };
        if !initialized {
            self.durable_abort_succeeded = true;
            return Ok(());
        }
        let transaction = self
            .store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM snapshot_leases WHERE lease = ?1",
                [&pin_lease_id],
            )
            .map_err(sql_error)?;
        delete_logical_generation(&transaction, &self.library_id, &logical_generation_id)?;
        delete_generation(&transaction, &staging_id)?;
        transaction.commit().map_err(sql_error)?;
        remove_staging_directory(&self.staging_root, &staging_directory)?;
        self.durable_abort_succeeded = true;
        Ok(())
    }
}

fn compare_generation_sequences(left: &str, right: &str) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn validate_same_generation_identity(
    left_generation: &str,
    left_hash: &str,
    left_sequence: &str,
    right_generation: &str,
    right_hash: &str,
    right_sequence: &str,
) -> Result<(), PeerSyncError> {
    if left_generation == right_generation
        && (left_hash != right_hash || left_sequence != right_sequence)
    {
        return validation(
            "logical generation ID is bound to different manifest hashes or sequences",
        );
    }
    Ok(())
}

fn increment_generation_sequence(sequence: &str) -> Result<String, PeerSyncError> {
    // Delegates to the canonical shared increment, which also validates
    // canonical-decimal input and enforces the 64-character schema cap.
    super::logical_index::increment_decimal(sequence)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))
}

fn merged_generation_sequence(local: &str, remote: &str) -> Result<String, PeerSyncError> {
    let maximum = if compare_generation_sequences(local, remote).is_lt() {
        remote
    } else {
        local
    };
    increment_generation_sequence(maximum)
}

fn validate_manifest_object_size_parity(
    manifests: [&LogicalManifest; 3],
) -> Result<(), PeerSyncError> {
    let mut positions = [0_usize; 3];
    while positions
        .iter()
        .enumerate()
        .any(|(index, position)| *position < manifests[index].objects.len())
    {
        let hash = manifests
            .iter()
            .enumerate()
            .filter_map(|(index, manifest)| manifest.objects.get(positions[index]))
            .map(|object| object.hash.as_str())
            .min()
            .expect("at least one manifest object remains");
        let mut size = None;
        for (index, manifest) in manifests.iter().enumerate() {
            let Some(object) = manifest.objects.get(positions[index]) else {
                continue;
            };
            if object.hash != hash {
                continue;
            }
            if size.is_some_and(|expected| expected != object.size) {
                return validation(format!(
                    "logical manifests disagree on object size for {hash}"
                ));
            }
            size = Some(object.size);
            positions[index] += 1;
        }
    }
    Ok(())
}

fn derive_exact_three_way_plan(
    base: &LogicalManifest,
    local: &LogicalManifest,
    remote: &LogicalManifest,
) -> Result<(Vec<LogicalDeltaApplyOperation>, Vec<String>, Vec<String>), PeerSyncError> {
    // One shared walker serves both the exact and the policy variants so the
    // merge rules cannot drift; the exact variant simply refuses the first
    // recorded conflict. The policy walker keeps walking after a conflict, so
    // a manifest that also contains a later structural violation surfaces
    // that validation error instead of the conflict - both are fail-closed
    // rejections of the same invalid input.
    let plan =
        derive_policy_three_way_plan(base, local, remote, LogicalDeltaConflictPolicy::Reject)?;
    if let Some(conflict) = plan.conflicts.into_iter().next() {
        return Err(PeerSyncError::LogicalMergeConflict {
            record: conflict.record,
        });
    }
    Ok((
        plan.apply,
        plan.preserve_local_keys,
        plan.candidate_object_hashes,
    ))
}

fn exact_merged_content(
    local: &LogicalManifest,
    remote: &LogicalManifest,
    plan: &ReadyLogicalDeltaPlan,
) -> Result<(Vec<LogicalManifestRecord>, Vec<LogicalManifestObject>), PeerSyncError> {
    let mut records = local
        .records
        .iter()
        .cloned()
        .map(|record| (record.key().to_owned(), record))
        .collect::<BTreeMap<_, _>>();
    for operation in &plan.apply {
        let key = match operation {
            LogicalDeltaApplyOperation::Put { key, .. }
            | LogicalDeltaApplyOperation::Delete { key, .. } => key,
        };
        let remote_record = remote
            .records
            .binary_search_by(|record| record.key().cmp(key))
            .ok()
            .and_then(|index| remote.records.get(index))
            .ok_or_else(|| {
                PeerSyncError::Validation(
                    "logical delta result record is absent from the remote manifest".to_owned(),
                )
            })?;
        records.insert(key.clone(), remote_record.clone());
    }

    let records = records.into_values().collect::<Vec<_>>();
    let referenced = records
        .iter()
        .filter_map(|record| match record {
            LogicalManifestRecord::Live(record) => Some(
                std::iter::once(record.object_hash.as_str())
                    .chain(record.dependencies.iter().map(String::as_str)),
            ),
            LogicalManifestRecord::Tombstone(_) => None,
        })
        .flatten()
        .collect::<BTreeSet<_>>();
    let available = local
        .objects
        .iter()
        .chain(&remote.objects)
        .map(|object| (object.hash.as_str(), object.size))
        .collect::<BTreeMap<_, _>>();
    let objects = referenced
        .into_iter()
        .map(|hash| {
            let size = available.get(hash).copied().ok_or_else(|| {
                PeerSyncError::Validation(
                    "logical delta result object is absent from its source manifests".to_owned(),
                )
            })?;
            Ok(LogicalManifestObject {
                hash: hash.to_owned(),
                size,
            })
        })
        .collect::<Result<Vec<_>, PeerSyncError>>()?;
    Ok((records, objects))
}

fn derive_policy_three_way_plan(
    base: &LogicalManifest,
    local: &LogicalManifest,
    remote: &LogicalManifest,
    policy: LogicalDeltaConflictPolicy,
) -> Result<PolicyThreeWayPlan, PeerSyncError> {
    let mut positions = [0_usize; 3];
    let manifests = [base, local, remote];
    let mut apply = Vec::new();
    let mut preserve = Vec::new();
    let mut candidates = BTreeSet::new();
    let mut conflicts = Vec::new();

    while positions
        .iter()
        .enumerate()
        .any(|(index, position)| *position < manifests[index].records.len())
    {
        let key = manifests
            .iter()
            .enumerate()
            .filter_map(|(index, manifest)| manifest.records.get(positions[index]))
            .map(LogicalManifestRecord::key)
            .min()
            .expect("at least one manifest record remains");
        let mut triple: [Option<&LogicalManifestRecord>; 3] = [None, None, None];
        for (index, manifest) in manifests.iter().enumerate() {
            if manifest
                .records
                .get(positions[index])
                .is_some_and(|record| record.key() == key)
            {
                triple[index] = manifest.records.get(positions[index]);
                positions[index] += 1;
            }
        }
        let [base_record, local_record, remote_record] = triple;
        if matches!(base_record, Some(LogicalManifestRecord::Tombstone(_)))
            && (!matches!(local_record, Some(LogicalManifestRecord::Tombstone(_)))
                || !matches!(remote_record, Some(LogicalManifestRecord::Tombstone(_))))
        {
            return validation(format!(
                "logical delta descendant must retain base tombstone {key}"
            ));
        }
        if base_record.is_some() && (local_record.is_none() || remote_record.is_none()) {
            return validation(format!(
                "logical delta descendant must retain or tombstone base record {key}"
            ));
        }
        let local_changed = local_record != base_record;
        let remote_changed = remote_record != base_record;
        if !local_changed && !remote_changed {
            continue;
        }
        if local_changed && remote_changed {
            if local_record == remote_record {
                continue;
            }
            let kind = match (local_record, remote_record) {
                (
                    Some(LogicalManifestRecord::Tombstone(_)),
                    Some(LogicalManifestRecord::Live(_)),
                )
                | (
                    Some(LogicalManifestRecord::Live(_)),
                    Some(LogicalManifestRecord::Tombstone(_)),
                ) => LogicalDeltaConflictKind::DeleteVsEdit,
                _ => LogicalDeltaConflictKind::SameRecord,
            };
            conflicts.push(LogicalDeltaConflict {
                record: key.to_owned(),
                kind,
            });
            match policy {
                LogicalDeltaConflictPolicy::Reject => continue,
                LogicalDeltaConflictPolicy::PreferLocal => {
                    preserve.push(key.to_owned());
                    continue;
                }
                LogicalDeltaConflictPolicy::PreferRemote => {}
            }
        } else if local_changed {
            preserve.push(key.to_owned());
            continue;
        } else if !remote_changed {
            continue;
        }
        match remote_record {
            Some(LogicalManifestRecord::Live(record)) => {
                apply.push(LogicalDeltaApplyOperation::Put {
                    key: record.key.clone(),
                    object_hash: record.object_hash.clone(),
                    dependencies: record.dependencies.clone(),
                });
                candidates.insert(record.object_hash.clone());
                candidates.extend(record.dependencies.iter().cloned());
            }
            Some(LogicalManifestRecord::Tombstone(record)) => {
                apply.push(LogicalDeltaApplyOperation::Delete {
                    key: record.key.clone(),
                    deleted_generation_sequence: record.deleted_generation_sequence.clone(),
                });
            }
            None => {}
        }
    }

    Ok(PolicyThreeWayPlan {
        apply,
        preserve_local_keys: preserve,
        candidate_object_hashes: candidates.into_iter().collect(),
        conflicts,
    })
}

fn validate_locator_envelope(
    locator: &LogicalRecordLocator,
    envelope: &LogicalRecordEnvelope,
) -> Result<(), PeerSyncError> {
    let kind_matches = matches!(
        (locator, envelope),
        (
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root { .. }
        ) | (
            LogicalRecordLocator::Preset { .. },
            LogicalRecordEnvelope::Preset { .. }
        ) | (
            LogicalRecordLocator::Plugin { .. },
            LogicalRecordEnvelope::Plugin { .. }
        ) | (
            LogicalRecordLocator::Character { .. },
            LogicalRecordEnvelope::Character { .. }
        ) | (
            LogicalRecordLocator::Conversation { .. },
            LogicalRecordEnvelope::Conversation { .. }
        ) | (
            LogicalRecordLocator::Asset { .. },
            LogicalRecordEnvelope::Asset { .. }
        ) | (
            LogicalRecordLocator::Inlay { .. },
            LogicalRecordEnvelope::Inlay { .. }
        ) | (
            LogicalRecordLocator::Cold { .. },
            LogicalRecordEnvelope::Cold { .. }
        )
    );
    if !kind_matches {
        return validation("logical record kind differs from its encoded key");
    }
    match (locator, envelope) {
        (LogicalRecordLocator::Root, LogicalRecordEnvelope::Root { value, .. }) => {
            let root = json_object(value, "logical root")?;
            if ["characters", "botPresets", "pluginCustomStorage"]
                .iter()
                .any(|key| root.contains_key(*key))
            {
                return validation("logical root contains a separated PDS record family");
            }
        }
        (
            LogicalRecordLocator::Character { character_id },
            LogicalRecordEnvelope::Character { detail, .. },
        ) => {
            let detail = json_object(detail, "logical character detail")?;
            if required_string(detail, "chaId", "logical character detail")? != character_id {
                return validation("logical character ID differs from its encoded key");
            }
            required_string(detail, "name", "logical character detail")?;
            if detail.contains_key("chats") {
                return validation("logical character detail contains separated conversations");
            }
        }
        (
            LogicalRecordLocator::Conversation {
                conversation_id, ..
            },
            LogicalRecordEnvelope::Conversation { detail, .. },
        ) => {
            let detail = json_object(detail, "logical conversation detail")?;
            if required_string(detail, "id", "logical conversation detail")? != conversation_id {
                return validation("logical conversation ID differs from its encoded key");
            }
            required_string(detail, "name", "logical conversation detail")?;
            if detail.contains_key("message") {
                return validation("logical conversation detail contains separated messages");
            }
        }
        _ => {}
    }
    Ok(())
}

fn rehydrate_owner_property(
    parent: &mut Map<String, Value>,
    property: &str,
    head: &ResolvedOwnerHead,
    allow_retained_property: bool,
) -> Result<(), PeerSyncError> {
    if head.head.present {
        let tuples = head.tuples.clone().ok_or_else(|| {
            PeerSyncError::Validation(
                "present logical owner head has no decoded manifest".to_owned(),
            )
        })?;
        let property_index = head.head.property_index.ok_or_else(|| {
            PeerSyncError::Validation("present logical owner head has no property index".to_owned())
        })?;
        if property_index > parent.len() as u64 {
            return validation("logical owner property index exceeds its parent object");
        }
        let property_index = usize::try_from(property_index).map_err(|_| {
            PeerSyncError::Validation(
                "logical owner property index exceeds the platform range".to_owned(),
            )
        })?;
        if let Some(retained) = parent.get(property) {
            if !allow_retained_property {
                return validation(
                    "logical owner property was not stripped from its parent envelope",
                );
            }
            if parent.keys().position(|key| key == property) != Some(property_index) {
                return validation("retained logical owner property index differs from its head");
            }
            let retained = retained.as_array().ok_or_else(|| {
                PeerSyncError::Validation(
                    "retained logical owner property must be a tuple array".to_owned(),
                )
            })?;
            if retained.len() != tuples.len() {
                return validation("retained logical owner tuple count differs from its manifest");
            }
            let mut has_trailing_fields = false;
            for (retained, expected) in retained.iter().zip(&tuples) {
                let (Some(retained), Some(expected)) = (retained.as_array(), expected.as_array())
                else {
                    return validation("retained logical owner tuple differs from its manifest");
                };
                if retained.len() < 3 || expected.len() != 3 || retained[..3] != expected[..] {
                    return validation("retained logical owner tuple differs from its manifest");
                }
                has_trailing_fields |= retained.len() > 3;
            }
            if !has_trailing_fields {
                return validation(
                    "exact-three logical owner property must use canonical stripping",
                );
            }
            return Ok(());
        }
        parent.shift_insert(property_index, property.to_owned(), Value::Array(tuples));
    } else {
        if parent.contains_key(property) {
            return validation("absent logical owner property was retained in its parent envelope");
        }
        if head.tuples.is_some() || head.head.property_index.is_some() {
            return validation("absent logical owner head contains manifest reconstruction data");
        }
    }
    Ok(())
}

fn rehydrate_root_owners(
    value: &mut Value,
    heads: &[ResolvedOwnerHead],
) -> Result<(), PeerSyncError> {
    let root = json_object_mut(value, "logical root")?;
    let mut by_identity = BTreeMap::new();
    for head in heads {
        let identity = match &head.head.owner {
            LogicalOwnerLocator::RootModule { index } => format!("module:{index}"),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => format!("persona:{index}"),
            LogicalOwnerLocator::CharacterAdditional { .. } => {
                return validation("logical root contains a character owner head")
            }
        };
        if by_identity.insert(identity, head).is_some() {
            return validation("logical root contains duplicate owner heads");
        }
    }
    let mut expected = 0_usize;
    if let Some(modules) = root.get_mut("modules") {
        let modules = modules.as_array_mut().ok_or_else(|| {
            PeerSyncError::Validation("logical root modules must be an array".to_owned())
        })?;
        for (index, module) in modules.iter_mut().enumerate() {
            let module = json_object_mut(module, "logical root module")?;
            let head = by_identity.get(&format!("module:{index}")).ok_or_else(|| {
                PeerSyncError::Validation(
                    "logical root module owner head coverage is incomplete".to_owned(),
                )
            })?;
            rehydrate_owner_property(module, "assets", head, true)?;
            expected += 1;
        }
    }
    if let Some(personas) = root.get_mut("personas") {
        let personas = personas.as_array_mut().ok_or_else(|| {
            PeerSyncError::Validation("logical root personas must be an array".to_owned())
        })?;
        for (index, persona) in personas.iter_mut().enumerate() {
            let persona = json_object_mut(persona, "logical root persona")?;
            let Some(embedded) = persona.get_mut("embeddedModule") else {
                continue;
            };
            let embedded = json_object_mut(embedded, "logical persona embedded module")?;
            let head = by_identity
                .get(&format!("persona:{index}"))
                .ok_or_else(|| {
                    PeerSyncError::Validation(
                        "logical persona owner head coverage is incomplete".to_owned(),
                    )
                })?;
            rehydrate_owner_property(embedded, "assets", head, true)?;
            expected += 1;
        }
    }
    if by_identity.len() != expected {
        return validation("logical root contains owner heads for missing occurrences");
    }
    Ok(())
}

fn rehydrate_character_owner(
    detail: &mut Value,
    character_id: &str,
    heads: &[ResolvedOwnerHead],
) -> Result<(), PeerSyncError> {
    let [head] = heads else {
        return validation("logical character owner head coverage must contain exactly one head");
    };
    if !matches!(
        &head.head.owner,
        LogicalOwnerLocator::CharacterAdditional { character_id: owner_id }
            if owner_id == character_id
    ) {
        return validation("logical character owner head differs from its encoded key");
    }
    let detail = json_object_mut(detail, "logical character detail")?;
    rehydrate_owner_property(detail, "additionalAssets", head, false)
}

fn clone_generation(
    transaction: &Transaction<'_>,
    source: &str,
    target: &str,
) -> Result<(), PeerSyncError> {
    for (table, columns) in PDS_GENERATION_TABLES {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {table} (generation, {columns})
                     SELECT ?1, {columns} FROM {table} WHERE generation = ?2"
                ),
                params![target, source],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn move_generation(
    transaction: &Transaction<'_>,
    source: &str,
    target: &str,
) -> Result<(), PeerSyncError> {
    for (table, _) in PDS_GENERATION_TABLES {
        transaction
            .execute(
                &format!("UPDATE {table} SET generation = ?2 WHERE generation = ?1"),
                params![source, target],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn delete_generation(transaction: &Transaction<'_>, generation: &str) -> Result<(), PeerSyncError> {
    for (table, _) in PDS_GENERATION_TABLES.iter().rev() {
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE generation = ?1"),
                [generation],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationPhase {
    DeleteConversation,
    DeleteCharacter,
    DeleteOther,
    PutOther,
    PutConversation,
}

fn classify_plan_operations(
    plan: &ReadyLogicalDeltaPlan,
) -> Result<Vec<OperationPhase>, PeerSyncError> {
    plan.apply
        .iter()
        .map(|operation| {
            let (key, delete) = match operation {
                LogicalDeltaApplyOperation::Put { key, .. } => (key, false),
                LogicalDeltaApplyOperation::Delete { key, .. } => (key, true),
            };
            let locator = decode_logical_record_key(key)
                .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
            Ok(match (delete, locator) {
                (true, LogicalRecordLocator::Conversation { .. }) => {
                    OperationPhase::DeleteConversation
                }
                (true, LogicalRecordLocator::Character { .. }) => OperationPhase::DeleteCharacter,
                (true, _) => OperationPhase::DeleteOther,
                (false, LogicalRecordLocator::Conversation { .. }) => {
                    OperationPhase::PutConversation
                }
                (false, _) => OperationPhase::PutOther,
            })
        })
        .collect()
}

fn validate_character_deletes(
    connection: &Connection,
    staging_id: &str,
    plan: &ReadyLogicalDeltaPlan,
) -> Result<(), PeerSyncError> {
    let deleted_keys = plan
        .apply
        .iter()
        .filter_map(|operation| match operation {
            LogicalDeltaApplyOperation::Delete { key, .. } => Some(key.as_str()),
            LogicalDeltaApplyOperation::Put { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    for operation in &plan.apply {
        let LogicalDeltaApplyOperation::Delete { key, .. } = operation else {
            continue;
        };
        let locator = decode_logical_record_key(key)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let LogicalRecordLocator::Character { character_id } = locator else {
            continue;
        };
        let mut statement = connection
            .prepare(
                "SELECT conversation_id FROM conversations
                 WHERE generation = ?1 AND character_id = ?2
                 ORDER BY conversation_id ASC",
            )
            .map_err(sql_error)?;
        let mut rows = statement
            .query(params![staging_id, &character_id])
            .map_err(sql_error)?;
        while let Some(row) = rows.next().map_err(sql_error)? {
            let conversation_id: String = row.get(0).map_err(sql_error)?;
            let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
                character_id: character_id.clone(),
                conversation_id,
            })
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
            if !deleted_keys.contains(key.as_str()) {
                return validation(
                    "logical character deletion does not explicitly delete every child conversation",
                );
            }
        }
        if plan.apply.iter().any(|operation| {
            let LogicalDeltaApplyOperation::Put { key, .. } = operation else {
                return false;
            };
            matches!(
                decode_logical_record_key(key),
                Ok(LogicalRecordLocator::Conversation {
                    character_id: child,
                    ..
                }) if child == character_id
            )
        }) {
            return validation(
                "logical character deletion conflicts with a live child conversation",
            );
        }
        if plan.preserve_local_keys.iter().any(|key| {
            matches!(
                decode_logical_record_key(key),
                Ok(LogicalRecordLocator::Conversation {
                    character_id: preserved,
                    ..
                }) if preserved == character_id
            )
        }) {
            return validation(
                "logical character deletion conflicts with a preserved conversation",
            );
        }
    }
    Ok(())
}

fn refresh_touched_character_counts(
    transaction: &Transaction<'_>,
    generation: &str,
    plan: &ReadyLogicalDeltaPlan,
) -> Result<(), PeerSyncError> {
    let mut character_ids = BTreeSet::new();
    for operation in &plan.apply {
        let key = match operation {
            LogicalDeltaApplyOperation::Put { key, .. }
            | LogicalDeltaApplyOperation::Delete { key, .. } => key,
        };
        let locator = decode_logical_record_key(key)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        match locator {
            LogicalRecordLocator::Character { character_id }
            | LogicalRecordLocator::Conversation { character_id, .. } => {
                character_ids.insert(character_id);
            }
            _ => {}
        }
    }
    for character_id in character_ids {
        transaction
            .execute(
                "UPDATE characters SET conversation_count = (
                    SELECT COUNT(*) FROM conversations
                    WHERE generation = ?1 AND character_id = ?2
                 ) WHERE generation = ?1 AND character_id = ?2",
                params![generation, character_id],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn validate_configured_index_uniqueness(
    transaction: &Transaction<'_>,
    generation: &str,
) -> Result<(), PeerSyncError> {
    for (query, family) in [
        (
            "SELECT EXISTS(
                SELECT 1 FROM bot_presets WHERE generation = ?1
                GROUP BY configured_index HAVING COUNT(*) > 1
             )",
            "preset",
        ),
        (
            "SELECT EXISTS(
                SELECT 1 FROM characters WHERE generation = ?1
                GROUP BY configured_index HAVING COUNT(*) > 1
             )",
            "character",
        ),
        (
            "SELECT EXISTS(
                SELECT 1 FROM conversations WHERE generation = ?1
                GROUP BY character_id, configured_index HAVING COUNT(*) > 1
             )",
            "conversation",
        ),
    ] {
        let duplicate: bool = transaction
            .query_row(query, [generation], |row| row.get(0))
            .map_err(sql_error)?;
        if duplicate {
            return validation(format!(
                "logical delta creates duplicate {family} configured indices"
            ));
        }
    }
    Ok(())
}

fn apply_delete(
    transaction: &Transaction<'_>,
    generation: &str,
    locator: &LogicalRecordLocator,
) -> Result<(), PeerSyncError> {
    match locator {
        LogicalRecordLocator::Root => validation("logical delta cannot delete the root record"),
        LogicalRecordLocator::Preset { preset_id } => {
            transaction
                .execute(
                    "DELETE FROM bot_presets WHERE generation = ?1 AND preset_id = ?2",
                    params![generation, preset_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Plugin { storage_key } => {
            transaction
                .execute(
                    "DELETE FROM plugin_storage WHERE generation = ?1 AND storage_key = ?2",
                    params![generation, storage_key],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Character { character_id } => {
            transaction
                .execute(
                    "DELETE FROM messages WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM conversations WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
                       AND owner_locator = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM characters WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => {
            transaction
                .execute(
                    "DELETE FROM messages
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM conversations
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Asset { logical_key } => {
            delete_alias(transaction, generation, "asset", logical_key)
        }
        LogicalRecordLocator::Inlay { logical_key } => {
            delete_alias(transaction, generation, "inlay", logical_key)
        }
        LogicalRecordLocator::Cold { logical_key } => {
            transaction
                .execute(
                    "DELETE FROM cold_aliases WHERE generation = ?1 AND key = ?2",
                    params![generation, logical_key],
                )
                .map_err(sql_error)?;
            Ok(())
        }
    }
}

fn delete_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    kind: &str,
    logical_key: &str,
) -> Result<(), PeerSyncError> {
    transaction
        .execute(
            "DELETE FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
            params![generation, kind, logical_key],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn apply_put(
    transaction: &Transaction<'_>,
    generation: &str,
    put: &PreparedPut,
) -> Result<(), PeerSyncError> {
    match (&put.locator, &put.envelope) {
        (LogicalRecordLocator::Root, LogicalRecordEnvelope::Root { value, owner_heads }) => {
            transaction
                .execute(
                    "INSERT INTO root (generation, value) VALUES (?1, ?2)
                     ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
                    params![
                        generation,
                        serde_json::to_string(value).map_err(json_error)?
                    ],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind IN (
                        'root-module-assets', 'persona-embedded-module-assets'
                     )",
                    [generation],
                )
                .map_err(sql_error)?;
            insert_owner_heads(transaction, generation, owner_heads)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Preset { preset_id },
            LogicalRecordEnvelope::Preset {
                configured_index,
                value,
            },
        ) => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let image = value.get("image").and_then(Value::as_str);
            transaction
                .execute(
                    "INSERT INTO bot_presets (
                        generation, preset_id, configured_index, name, image, value
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(generation, preset_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        name = excluded.name,
                        image = excluded.image,
                        value = excluded.value",
                    params![
                        generation,
                        preset_id,
                        sqlite_i64(*configured_index, "preset configured index")?,
                        name,
                        image,
                        serde_json::to_string(value).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Plugin { storage_key },
            LogicalRecordEnvelope::Plugin { ordinal, value },
        ) => {
            let serialized = serde_json::to_string(value).map_err(json_error)?;
            transaction
                .execute(
                    "INSERT INTO plugin_storage (
                        generation, storage_key, byte_size, ordinal, value
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(generation, storage_key) DO UPDATE SET
                        byte_size = excluded.byte_size,
                        ordinal = excluded.ordinal,
                        value = excluded.value",
                    params![
                        generation,
                        storage_key,
                        sqlite_i64(serialized.len() as u64, "plugin storage byte size")?,
                        sqlite_i64(*ordinal, "plugin storage ordinal")?,
                        serialized,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Character { character_id },
            LogicalRecordEnvelope::Character {
                configured_index,
                detail,
                owner_heads,
            },
        ) => {
            let object = json_object(detail, "logical character detail")?;
            let name = required_string(object, "name", "logical character detail")?;
            let conversation_count: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM conversations
                     WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let recent_at = object
                .get("lastInteraction")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let trash_time = object.get("trashTime").and_then(Value::as_i64);
            transaction
                .execute(
                    "INSERT INTO characters (
                        generation, character_id, configured_index, recent_at, trashed,
                        name, image, conversation_count, type, creator_notes, trash_time, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(generation, character_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        recent_at = excluded.recent_at,
                        trashed = excluded.trashed,
                        name = excluded.name,
                        image = excluded.image,
                        conversation_count = excluded.conversation_count,
                        type = excluded.type,
                        creator_notes = excluded.creator_notes,
                        trash_time = excluded.trash_time,
                        detail = excluded.detail",
                    params![
                        generation,
                        character_id,
                        sqlite_i64(*configured_index, "character configured index")?,
                        recent_at,
                        trash_time.is_some(),
                        name,
                        object.get("image").and_then(Value::as_str),
                        conversation_count,
                        object
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("character"),
                        object.get("creatorNotes").and_then(Value::as_str),
                        trash_time,
                        serde_json::to_string(detail).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
                       AND owner_locator = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            insert_owner_heads(transaction, generation, owner_heads)
        }
        (
            LogicalRecordLocator::Conversation {
                character_id,
                conversation_id,
            },
            LogicalRecordEnvelope::Conversation {
                configured_index,
                recent_at,
                detail,
                ..
            },
        ) => {
            let parent_exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM characters
                        WHERE generation = ?1 AND character_id = ?2
                     )",
                    params![generation, character_id],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if !parent_exists {
                return validation("logical conversation parent character is absent");
            }
            let object = json_object(detail, "logical conversation detail")?;
            let name = required_string(object, "name", "logical conversation detail")?;
            let message_count = put.pages.iter().try_fold(0_u64, |count, page| {
                count.checked_add(page.message_count).ok_or_else(|| {
                    PeerSyncError::Validation("logical message count overflow".to_owned())
                })
            })?;
            transaction
                .execute(
                    "DELETE FROM messages
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO conversations (
                        generation, character_id, conversation_id, configured_index,
                        recent_at, name, message_count, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(generation, character_id, conversation_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        recent_at = excluded.recent_at,
                        name = excluded.name,
                        message_count = excluded.message_count,
                        detail = excluded.detail",
                    params![
                        generation,
                        character_id,
                        conversation_id,
                        sqlite_i64(*configured_index, "conversation configured index")?,
                        recent_at,
                        name,
                        sqlite_i64(message_count, "conversation message count")?,
                        serde_json::to_string(detail).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Asset { logical_key },
            LogicalRecordEnvelope::Asset {
                object_hash,
                size,
                metadata,
            },
        ) => put_alias(
            transaction,
            generation,
            "asset",
            logical_key,
            object_hash.as_deref(),
            *size,
            metadata,
        ),
        (
            LogicalRecordLocator::Inlay { logical_key },
            LogicalRecordEnvelope::Inlay {
                object_hash,
                size,
                metadata,
            },
        ) => put_alias(
            transaction,
            generation,
            "inlay",
            logical_key,
            object_hash.as_deref(),
            *size,
            metadata,
        ),
        (
            LogicalRecordLocator::Cold { logical_key },
            LogicalRecordEnvelope::Cold {
                object_hash,
                size,
                metadata,
            },
        ) => {
            transaction
                .execute(
                    "INSERT INTO cold_aliases (
                        generation, key, object_hash, size, metadata
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(generation, key) DO UPDATE SET
                        object_hash = excluded.object_hash,
                        size = excluded.size,
                        metadata = excluded.metadata",
                    params![
                        generation,
                        logical_key,
                        object_hash,
                        sqlite_i64(*size, "cold alias size")?,
                        serde_json::to_string(metadata).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        _ => validation(format!(
            "logical prepared record {} changed kind before apply",
            put.key
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn put_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    kind: &str,
    logical_key: &str,
    object_hash: Option<&str>,
    size: u64,
    metadata: &Value,
) -> Result<(), PeerSyncError> {
    let typed = decode_asset_alias_metadata(metadata)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(generation, kind, logical_key) DO UPDATE SET
                object_hash = excluded.object_hash,
                size = excluded.size,
                mime = excluded.mime,
                name = excluded.name,
                ext = excluded.ext,
                inlay_type = excluded.inlay_type,
                width = excluded.width,
                height = excluded.height,
                metadata = excluded.metadata",
            params![
                generation,
                logical_key,
                object_hash,
                kind,
                sqlite_i64(size, "asset alias size")?,
                typed.mime,
                typed.name,
                typed.ext,
                typed.inlay_type,
                typed.width,
                typed.height,
                serde_json::to_string(&typed.metadata).map_err(json_error)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn insert_owner_heads(
    transaction: &Transaction<'_>,
    generation: &str,
    heads: &[LogicalOwnerHead],
) -> Result<(), PeerSyncError> {
    for head in heads {
        let (kind, locator) = match &head.owner {
            LogicalOwnerLocator::CharacterAdditional { character_id } => {
                ("character-additional-assets", character_id.clone())
            }
            LogicalOwnerLocator::RootModule { index } => ("root-module-assets", index.to_string()),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => {
                ("persona-embedded-module-assets", index.to_string())
            }
        };
        transaction
            .execute(
                "INSERT INTO asset_owner_heads (
                    generation, owner_kind, owner_locator, present, manifest_hash, entry_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    generation,
                    kind,
                    locator,
                    head.present,
                    head.manifest_hash,
                    sqlite_i64(head.entry_count, "owner entry count")?,
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn common_base(
    connection: &Connection,
    peer_id: &str,
    library_id: &str,
) -> Result<Option<PeerBase>, PeerSyncError> {
    connection
        .query_row(
            "SELECT generation_id, manifest_hash, generation_sequence
             FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2",
            params![peer_id, library_id],
            |row| {
                Ok(PeerBase {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

fn current_logical_head_matches(
    connection: &Connection,
    library_id: &str,
    original_generation_id: &str,
    active_pds_generation: &str,
    source_revision: i64,
    changed: bool,
) -> Result<bool, PeerSyncError> {
    let identity_column = if changed {
        "parent_generation_id"
    } else {
        "generation_id"
    };
    let query = format!(
        "SELECT COUNT(*), COALESCE(SUM(
            CASE WHEN {identity_column} = ?4 THEN 1 ELSE 0 END
         ), 0)
         FROM logical_sync_generations
         WHERE library_id = ?1 AND pds_generation = ?2 AND source_revision = ?3
           AND state = 'complete' AND manifest_hash IS NOT NULL"
    );
    let (total, matching): (i64, i64) = connection
        .query_row(
            &query,
            params![
                library_id,
                active_pds_generation,
                source_revision,
                original_generation_id
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)?;
    Ok(total == 1 && matching == 1)
}

fn active_logical_identity(
    connection: &Connection,
    library_id: &str,
    active_pds_generation: &str,
    source_revision: i64,
) -> Result<Option<SyncGenerationIdentity>, PeerSyncError> {
    connection
        .query_row(
            "SELECT generation.generation_id, generation.manifest_hash,
                    generation.generation_sequence
             FROM logical_library_head AS head
             JOIN logical_sync_generations AS generation
               ON generation.library_id = head.library_id
              AND generation.generation_id = head.generation_id
             WHERE head.singleton = 1 AND head.library_id = ?1
               AND generation.pds_generation = ?2
               AND generation.source_revision = ?3
               AND generation.state = 'complete'
               AND generation.manifest_hash IS NOT NULL",
            params![library_id, active_pds_generation, source_revision],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

#[allow(clippy::too_many_arguments)]
fn update_common_base(
    transaction: &Transaction<'_>,
    peer_id: &str,
    library_id: &str,
    generation_id: &str,
    next_hash: &str,
    next_sequence: &str,
    expected: &PeerBase,
) -> Result<(), PeerSyncError> {
    let updated = transaction
        .execute(
            "UPDATE logical_peer_common_bases
             SET generation_id = ?3, manifest_hash = ?4,
                 generation_sequence = ?5, updated_at = ?6
             WHERE peer_id = ?1 AND library_id = ?2
               AND generation_id = ?7 AND manifest_hash = ?8
               AND generation_sequence = ?9",
            params![
                peer_id,
                library_id,
                generation_id,
                next_hash,
                next_sequence,
                unix_millis()?,
                expected.generation_id,
                expected.manifest_hash,
                expected.generation_sequence,
            ],
        )
        .map_err(sql_error)?;
    if updated != 1 {
        return validation("logical delta durable common base disappeared during activation");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn advance_peer_state_in_transaction(
    transaction: &Transaction<'_>,
    activation_mode: LogicalDeltaActivationMode,
    peer_id: &str,
    library_id: &str,
    remote_generation_id: &str,
    remote_manifest_hash: &str,
    remote_generation_sequence: &str,
    expected_base: &PeerBase,
    shared_ack: Option<PreparedSharedAckTransition>,
) -> Result<(), PeerSyncError> {
    match activation_mode {
        LogicalDeltaActivationMode::AdvanceRemoteBase => update_common_base(
            transaction,
            peer_id,
            library_id,
            remote_generation_id,
            remote_manifest_hash,
            remote_generation_sequence,
            expected_base,
        ),
        LogicalDeltaActivationMode::DeferPeerState => Ok(()),
        LogicalDeltaActivationMode::AdvanceSharedAck => {
            let shared_ack = shared_ack.ok_or_else(|| {
                PeerSyncError::Validation(
                    "P5 shared acknowledgement proof was not prepared".to_owned(),
                )
            })?;
            if shared_ack.previous.shared_identity
                != SyncGenerationIdentity::from(expected_base.clone())
            {
                return validation(
                    "P5 shared acknowledgement differs from the expected common base",
                );
            }
            advance_sync_device_shared_ack_in_transaction(
                transaction,
                library_id,
                peer_id,
                &shared_ack.previous.shared_identity,
                &shared_ack.previous.local_identity,
                &shared_ack.next_shared,
                shared_ack.proof,
            )
            .map(|_| ())
            .map_err(storage_error)
        }
    }
}

fn set_active(
    transaction: &Transaction<'_>,
    revision: i64,
    generation: &str,
) -> Result<(), PeerSyncError> {
    transaction
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'currentRevision'",
            [serde_json::to_string(&revision).map_err(json_error)?],
        )
        .map_err(sql_error)?;
    transaction
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'activeGeneration'",
            [serde_json::to_string(generation).map_err(json_error)?],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn delete_logical_generation(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
) -> Result<(), PeerSyncError> {
    for table in [
        "logical_message_page_sources",
        "logical_record_dependencies",
        "logical_record_heads",
        "logical_sync_generations",
    ] {
        transaction
            .execute(
                &format!("DELETE FROM {table} WHERE library_id = ?1 AND generation_id = ?2"),
                params![library_id, generation_id],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn begin_compact_logical_index(
    transaction: &Transaction<'_>,
    library_id: &str,
    parent_generation_id: &str,
    generation_id: &str,
    pds_generation: &str,
    source_revision: i64,
    generation_sequence: &str,
) -> Result<(), PeerSyncError> {
    transaction
        .execute(
            "INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'building', NULL, ?7, NULL)",
            params![
                library_id,
                generation_id,
                generation_sequence,
                parent_generation_id,
                pds_generation,
                source_revision,
                unix_millis()?,
            ],
        )
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
             )
             SELECT library_id, ?3, record_key, record_kind, state,
                    object_hash, object_size, deleted_generation_sequence
             FROM logical_record_heads
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, parent_generation_id, generation_id],
        )
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             )
             SELECT library_id, ?3, record_key, object_hash, object_size
             FROM logical_record_dependencies
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, parent_generation_id, generation_id],
        )
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, record_kind, page_index,
                first_message_index, message_count, object_hash, object_size
             )
             SELECT library_id, ?3, record_key, record_kind, page_index,
                    first_message_index, message_count, object_hash, object_size
             FROM logical_message_page_sources
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, parent_generation_id, generation_id],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn apply_logical_index_operations(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    puts: &[PreparedPut],
    deletes: &[PreparedDelete],
) -> Result<(), PeerSyncError> {
    for key in puts
        .iter()
        .map(|put| put.key.as_str())
        .chain(deletes.iter().map(|delete| delete.key.as_str()))
    {
        transaction
            .execute(
                "DELETE FROM logical_message_page_sources
                 WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
                params![library_id, generation_id, key],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM logical_record_dependencies
                 WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
                params![library_id, generation_id, key],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM logical_record_heads
                 WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
                params![library_id, generation_id, key],
            )
            .map_err(sql_error)?;
    }
    for put in puts {
        transaction
            .execute(
                "INSERT INTO logical_record_heads (
                    library_id, generation_id, record_key, record_kind, state,
                    object_hash, object_size, deleted_generation_sequence
                 ) VALUES (?1, ?2, ?3, ?4, 'live', ?5, ?6, NULL)",
                params![
                    library_id,
                    generation_id,
                    put.key,
                    record_kind(&put.locator),
                    put.object_hash,
                    sqlite_i64(put.object_size, "logical record object size")?,
                ],
            )
            .map_err(sql_error)?;
        for (hash, size) in &put.dependencies {
            transaction
                .execute(
                    "INSERT INTO logical_record_dependencies (
                        library_id, generation_id, record_key, object_hash, object_size
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        library_id,
                        generation_id,
                        put.key,
                        hash,
                        sqlite_i64(*size, "logical dependency object size")?,
                    ],
                )
                .map_err(sql_error)?;
        }
        let mut first_message_index = 0_u64;
        for (page_index, page) in put.pages.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO logical_message_page_sources (
                        library_id, generation_id, record_key, record_kind, page_index,
                        first_message_index, message_count, object_hash, object_size
                     ) VALUES (?1, ?2, ?3, 'conversation', ?4, ?5, ?6, ?7, ?8)",
                    params![
                        library_id,
                        generation_id,
                        put.key,
                        sqlite_i64(page_index as u64, "logical page index")?,
                        sqlite_i64(first_message_index, "logical page first message")?,
                        sqlite_i64(page.message_count, "logical page message count")?,
                        page.hash,
                        sqlite_i64(page.size, "logical page object size")?,
                    ],
                )
                .map_err(sql_error)?;
            first_message_index = first_message_index
                .checked_add(page.message_count)
                .ok_or_else(|| {
                    PeerSyncError::Validation("logical message index overflow".to_owned())
                })?;
        }
    }
    for delete in deletes {
        transaction
            .execute(
                "INSERT INTO logical_record_heads (
                    library_id, generation_id, record_key, record_kind, state,
                    object_hash, object_size, deleted_generation_sequence
                 ) VALUES (?1, ?2, ?3, ?4, 'tombstone', NULL, 0, ?5)",
                params![
                    library_id,
                    generation_id,
                    delete.key,
                    record_kind(&delete.locator),
                    delete.deleted_generation_sequence,
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn record_kind(locator: &LogicalRecordLocator) -> &'static str {
    match locator {
        LogicalRecordLocator::Root => "root",
        LogicalRecordLocator::Preset { .. } => "preset",
        LogicalRecordLocator::Plugin { .. } => "plugin",
        LogicalRecordLocator::Character { .. } => "character",
        LogicalRecordLocator::Conversation { .. } => "conversation",
        LogicalRecordLocator::Asset { .. } => "asset",
        LogicalRecordLocator::Inlay { .. } => "inlay",
        LogicalRecordLocator::Cold { .. } => "cold",
    }
}

fn remove_staging_directory(root: &Path, directory: &Path) -> Result<(), PeerSyncError> {
    if directory.parent() != Some(root)
        || !directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("staging-logical-"))
    {
        return validation("logical delta staging directory escaped its configured root");
    }
    match fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(PeerSyncError::Storage(error.to_string())),
    }
    match fs::remove_dir(root) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => return Err(PeerSyncError::Storage(error.to_string())),
    }
    Ok(())
}

fn verify_bytes(bytes: &[u8], hash: &str, size: u64) -> Result<(), PeerSyncError> {
    if bytes.len() as u64 != size {
        return validation(format!(
            "logical delta object {hash} has size {}, expected {size}",
            bytes.len()
        ));
    }
    if hex::encode(Sha256::digest(bytes)) != hash {
        return Err(PeerSyncError::WholeObjectHashMismatch {
            object: hash.to_owned(),
        });
    }
    Ok(())
}

fn verify_reader(reader: &mut dyn Read, hash: &str, size: u64) -> Result<(), PeerSyncError> {
    let mut hasher = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.checked_add(read as u64).ok_or_else(|| {
            PeerSyncError::Validation("logical delta payload size overflow".to_owned())
        })?;
    }
    if bytes != size {
        return validation(format!(
            "logical delta object {hash} has size {bytes}, expected {size}"
        ));
    }
    if hex::encode(hasher.finalize()) != hash {
        return Err(PeerSyncError::WholeObjectHashMismatch {
            object: hash.to_owned(),
        });
    }
    Ok(())
}

fn sqlite_i64(value: u64, context: &str) -> Result<i64, PeerSyncError> {
    i64::try_from(value)
        .map_err(|_| PeerSyncError::Validation(format!("{context} exceeds SQLite range")))
}

fn json_object<'a>(
    value: &'a Value,
    context: &str,
) -> Result<&'a Map<String, Value>, PeerSyncError> {
    value
        .as_object()
        .ok_or_else(|| PeerSyncError::Validation(format!("{context} must be an object")))
}

fn json_object_mut<'a>(
    value: &'a mut Value,
    context: &str,
) -> Result<&'a mut Map<String, Value>, PeerSyncError> {
    value
        .as_object_mut()
        .ok_or_else(|| PeerSyncError::Validation(format!("{context} must be an object")))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, PeerSyncError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| PeerSyncError::Validation(format!("{context} requires {key}")))
}

fn unix_millis() -> Result<i64, PeerSyncError> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PeerSyncError::Storage("system clock precedes Unix epoch".to_owned()))?;
    i64::try_from(duration.as_millis())
        .map_err(|_| PeerSyncError::Storage("system clock exceeds SQLite range".to_owned()))
}

fn validation<T>(message: impl Into<String>) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.into()))
}

fn sql_error(error: rusqlite::Error) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn json_error(error: serde_json::Error) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn storage_error(error: StoreError) -> PeerSyncError {
    match error {
        StoreError::RevisionConflict { expected, actual } => PeerSyncError::ActivationConflict {
            expected: Some(expected.to_string()),
            actual: Some(actual.to_string()),
        },
        other => PeerSyncError::Storage(other.to_string()),
    }
}

#[cfg(test)]
#[path = "logical_delta_target_tests.rs"]
mod tests;
