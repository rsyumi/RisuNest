#[cfg(any(desktop, target_os = "android", test))]
use super::android_foreground::AndroidForegroundKey;
#[cfg(any(target_os = "android", test))]
use super::android_foreground::{registry, AndroidCancellationProbe, AndroidForegroundLane};
#[cfg_attr(target_os = "android", allow(unused_imports))]
use super::lan::discover_lan_ipv4;
#[cfg(test)]
pub(crate) use super::lan::DISCOVER_LAN_IPV4_OVERRIDE;
#[cfg(any(target_os = "android", test))]
use super::target_foreground_transition::AndroidTargetForegroundTransition;
#[cfg(test)]
use super::target_foreground_transition::AndroidTargetForegroundTransitionPhase as AndroidBidirectionalTargetPhase;
#[cfg(desktop)]
use super::tunnel::{self, SystemTunnelProcess, TunnelStartFailure};
use super::{
    command_codes::{
        code_for, finish_peer_command, finish_peer_worker, is_bounded_code, PeerCommandCode,
    },
    lan::{
        deliver_peer_completion, prepare_peer_logical_completion, validate_p5_desktop_endpoint,
        LanBidirectionalBackupReceipt, LanBidirectionalControl, LanBidirectionalGeneration,
        LanBidirectionalLogicalClient, LanBidirectionalLogicalCredential,
        LanBidirectionalRegistrationRequest, LanBidirectionalRemoteApplyReceipt,
        LanBidirectionalRemoteApplyRequest, LanBidirectionalSession, LanLogicalDeltaClient,
        PreparedBidirectionalLogicalLanSession,
    },
    logical_delta::{decode_logical_manifest, hash_logical_manifest},
    logical_delta_transfer::{
        execute_logical_delta_pull_with_pre_activation, select_missing_logical_delta_objects,
    },
    maintenance::ActiveTempGuard,
    LanCloneHost, LogicalDeltaActivation, LogicalDeltaObject, LogicalDeltaObjectSource,
    LogicalDeltaStagedTarget, PeerSyncError,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    local_backup::{CancellationProbe, NeverCancelled},
    lossless_backup::{
        create_and_verify_peer_bidirectional_backup_v1_report,
        verify_lossless_package_v1_for_production, LosslessError, LosslessErrorCode,
        LosslessPeerSourceBinding,
    },
    persistent_store::{
        self, logical_delta_source::LogicalDeltaSourceSession, LogicalDeltaConflictKind,
        LogicalDeltaConflictPolicy, LogicalDeltaPlanResolution, PersistentLogicalDeltaTarget,
        PersistentStore, StoreError, SyncGenerationIdentity, VerifiedSyncDeviceRegistration,
        PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(desktop)]
use std::{sync::LazyLock, time::Duration};
// The shared LAN discovery override the target foreground tests drive.
#[cfg(test)]
use std::net::Ipv4Addr;
use tauri::{AppHandle, Manager, State};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

const OPERATION_SCHEMA: &str = "risunest.peer-bidirectional-operation/v1";
const OPERATION_FILE: &str = "operation.json";
const ABANDON_CLAIM_FILE: &str = ".operation-abandon.claim";
const MAX_OPERATION_BYTES: u64 = 1_048_576;
const P5_SOURCE_PIN_PREFIX: &str = "logical-session-p5-source-";

#[cfg(test)]
thread_local! {
    static COMPLETE_BEFORE_ACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static COMPLETE_AFTER_ACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_COMPLETE_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_PREPARED_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_INITIAL_PREPARED_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_BACKUP_BEFORE_RECEIPT_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_PREPARED_STORE_PANIC: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_BACKUP_PUBLISH_CANCEL_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static BACKUP_CREATE_RACE_CANCEL_FAILPOINT: RefCell<Option<Arc<std::sync::atomic::AtomicBool>>> = const { RefCell::new(None) };
    static BACKUP_STAGING_MAINTENANCE_HOOK: RefCell<Option<Box<dyn FnOnce(&Path)>>> = const { RefCell::new(None) };
    static TARGET_PREPARED_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static TARGET_AFTER_PREPARED_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static TARGET_AFTER_ACTIVATION_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static TARGET_JOB_RELEASE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static TARGET_CONFLICT_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static REMOTE_RECEIPT_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static OPERATION_READ_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static ABANDON_BEFORE_CLAIM_REPLACEMENT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    static ABANDON_CLAIM_REMOVE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static ABANDON_AFTER_CLAIM_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static BACKUP_FULL_VERIFICATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(test, not(windows)))]
thread_local! {
    static ATOMIC_PARENT_SYNC_FAILPOINT: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBidirectionalConflict {
    pub(crate) key: String,
    pub(crate) conflict_type: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerBidirectionalBackupSide {
    Local,
    Remote,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerBidirectionalBackupReceipt {
    pub(crate) package_id: String,
    pub(crate) side: PeerBidirectionalBackupSide,
    pub(crate) path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerBidirectionalCompletedResult {
    pub(crate) kind: String,
    pub(crate) operation_id: String,
    pub(crate) revision: i64,
    pub(crate) remote_revision: i64,
    pub(crate) transferred_objects: u64,
    pub(crate) transferred_bytes: u64,
    pub(crate) backups: Vec<PeerBidirectionalBackupReceipt>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PeerBidirectionalCompletionMode {
    Unsupported,
    V1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBidirectionalOperationContext {
    pub(crate) operation_id: String,
    pub(crate) credential: LanBidirectionalLogicalCredential,
    pub(crate) library_id: String,
    pub(crate) expected_local_revision: i64,
    pub(crate) expected_remote_revision: i64,
    pub(crate) expected_remote_generation: LanBidirectionalGeneration,
    pub(crate) previous_shared: SyncGenerationIdentity,
    pub(crate) previous_local: SyncGenerationIdentity,
    pub(crate) durable_job_id: String,
    pub(crate) completion_mode: PeerBidirectionalCompletionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_delivery: Option<super::device_registry::PendingCompletionDelivery>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TargetPreparedConflictPolicy {
    Reject,
    PreferLocal,
    PreferRemote,
}

impl TargetPreparedConflictPolicy {
    fn logical_delta_policy(self) -> LogicalDeltaConflictPolicy {
        match self {
            Self::Reject => LogicalDeltaConflictPolicy::Reject,
            Self::PreferLocal => LogicalDeltaConflictPolicy::PreferLocal,
            Self::PreferRemote => LogicalDeltaConflictPolicy::PreferRemote,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub(crate) enum PeerBidirectionalDurableOperation {
    SourcePrepared {
        schema: String,
        operation_id: String,
        source_device_id: String,
        target_device_id: String,
        expected_source_revision: i64,
        previous_shared: SyncGenerationIdentity,
        expected_source_generation: SyncGenerationIdentity,
        shared_generation: LanBidirectionalGeneration,
        incoming_revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
        backup_required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup: Option<LanBidirectionalBackupReceipt>,
        #[serde(rename = "completionDeferredV1")]
        completion_deferred_v1: bool,
        durable_job_id: String,
    },
    TargetPrepared {
        schema: String,
        context: PeerBidirectionalOperationContext,
        local_generation: SyncGenerationIdentity,
        conflict_policy: TargetPreparedConflictPolicy,
        changed: bool,
        remote_backup_required: bool,
        transferred_objects: u64,
        transferred_bytes: u64,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    AwaitingConflict {
        schema: String,
        context: PeerBidirectionalOperationContext,
        conflicts: Vec<PeerBidirectionalConflict>,
        local_generation: SyncGenerationIdentity,
        local_manifest_hash: String,
        remote_manifest_hash: String,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    LocalCommitted {
        schema: String,
        context: PeerBidirectionalOperationContext,
        committed_revision: i64,
        shared_generation: SyncGenerationIdentity,
        changed: bool,
        remote_backup_required: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_apply_receipt: Option<LanBidirectionalRemoteApplyReceipt>,
        transferred_objects: u64,
        transferred_bytes: u64,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    Completed {
        schema: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_apply_receipt: Option<LanBidirectionalRemoteApplyReceipt>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_binding: Option<SourcePreparedEvidence>,
        result: PeerBidirectionalCompletedResult,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalConflictStatus {
    key: String,
    #[serde(rename = "type")]
    conflict_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalConflictResult {
    kind: &'static str,
    operation_id: String,
    conflicts: Vec<PeerBidirectionalConflictStatus>,
    local_manifest_hash: String,
    remote_manifest_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "phase",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PeerBidirectionalStatusOperation {
    SourcePrepared {
        operation_id: String,
    },
    TargetPrepared {
        operation_id: String,
    },
    AwaitingConflict {
        result: PeerBidirectionalConflictResult,
    },
    LocalCommitted {
        operation_id: String,
        committed_revision: i64,
    },
    Completed {
        result: PeerBidirectionalCompletedResult,
    },
}

impl PeerBidirectionalDurableOperation {
    pub(crate) fn backup_paths(&self) -> Vec<&str> {
        match self {
            Self::SourcePrepared { backup, .. } => {
                backup.iter().map(|backup| backup.path.as_str()).collect()
            }
            Self::TargetPrepared { backups, .. }
            | Self::AwaitingConflict { backups, .. }
            | Self::LocalCommitted { backups, .. } => {
                backups.iter().map(|backup| backup.path.as_str()).collect()
            }
            Self::Completed { result, .. } => result
                .backups
                .iter()
                .map(|backup| backup.path.as_str())
                .collect(),
        }
    }

    fn operation_id(&self) -> &str {
        match self {
            Self::SourcePrepared { operation_id, .. } => operation_id,
            Self::TargetPrepared { context, .. }
            | Self::AwaitingConflict { context, .. }
            | Self::LocalCommitted { context, .. } => &context.operation_id,
            Self::Completed { result, .. } => &result.operation_id,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        let schema = match self {
            Self::SourcePrepared { schema, .. }
            | Self::TargetPrepared { schema, .. }
            | Self::AwaitingConflict { schema, .. }
            | Self::LocalCommitted { schema, .. }
            | Self::Completed { schema, .. } => schema,
        };
        if schema != OPERATION_SCHEMA
            || uuid::Uuid::parse_str(self.operation_id())
                .map(|value| value.to_string() != self.operation_id())
                .unwrap_or(true)
        {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record is invalid".to_owned(),
            ));
        }
        if let Self::TargetPrepared { context, .. }
        | Self::AwaitingConflict { context, .. }
        | Self::LocalCommitted { context, .. } = self
        {
            if !valid_completion_context(context) {
                return Err(PeerSyncError::Storage(
                    "bidirectional operation has invalid completion evidence".to_owned(),
                ));
            }
            if !context.credential.endpoint.is_empty() {
                validate_p5_desktop_endpoint(&context.credential.endpoint).map_err(|_| {
                    PeerSyncError::Storage(
                        "bidirectional operation has an invalid desktop endpoint".to_owned(),
                    )
                })?;
            }
        }
        match self {
            Self::SourcePrepared {
                source_device_id,
                target_device_id,
                expected_source_revision,
                previous_shared,
                expected_source_generation,
                shared_generation,
                incoming_revision,
                backup,
                durable_job_id,
                ..
            } if *expected_source_revision < 0
                || expected_source_revision.checked_add(1).is_none()
                || *incoming_revision < 0
                || !is_canonical_uuid(source_device_id)
                || !is_canonical_uuid(target_device_id)
                || !is_canonical_uuid(durable_job_id)
                || !valid_generation(previous_shared)
                || !valid_generation(expected_source_generation)
                || !valid_lan_generation(shared_generation)
                || backup.as_ref().is_some_and(|backup| {
                    backup.package_id.is_empty() || backup.path.is_empty()
                }) =>
            {
                Err(PeerSyncError::Storage(
                    "source-prepared operation is invalid".to_owned(),
                ))
            }
            Self::TargetPrepared {
                context,
                local_generation,
                changed,
                backups,
                ..
            } if context.library_id != PRODUCT_LOGICAL_LIBRARY_ID
                || context.expected_local_revision < 0
                || (*changed && context.expected_local_revision.checked_add(1).is_none())
                || context.expected_remote_revision < 0
                || !is_canonical_uuid(&context.credential.device_id)
                || !is_canonical_uuid(&context.credential.source_device_id)
                || !is_canonical_uuid(&context.durable_job_id)
                || !valid_lan_generation(&context.expected_remote_generation)
                || context.credential.manifest_id
                    != context.expected_remote_generation.manifest_hash
                || !valid_generation(&context.previous_shared)
                || !valid_generation(&context.previous_local)
                || !valid_generation(local_generation)
                || !valid_backups(backups) =>
            {
                Err(PeerSyncError::Storage(
                    "target-prepared operation is invalid".to_owned(),
                ))
            }
            Self::AwaitingConflict { conflicts, .. } if conflicts.is_empty() => Err(
                PeerSyncError::Storage("awaiting-conflict operation has no conflicts".to_owned()),
            ),
            Self::AwaitingConflict { backups, .. } if !valid_backups(backups) => {
                Err(PeerSyncError::Storage(
                    "awaiting-conflict operation has an invalid backup".to_owned(),
                ))
            }
            Self::LocalCommitted {
                committed_revision, ..
            } if *committed_revision < 0 => Err(PeerSyncError::Storage(
                "local-committed operation has an invalid revision".to_owned(),
            )),
            Self::LocalCommitted { backups, .. } if !valid_backups(backups) => {
                Err(PeerSyncError::Storage(
                    "local-committed operation has an invalid backup".to_owned(),
                ))
            }
            Self::Completed { result, .. } if result.revision < 0 || result.remote_revision < 0 => {
                Err(PeerSyncError::Storage(
                    "completed operation has an invalid revision".to_owned(),
                ))
            }
            Self::Completed { result, .. } if !valid_backups(&result.backups) => Err(
                PeerSyncError::Storage("completed operation has an invalid backup".to_owned()),
            ),
            Self::Completed {
                source_binding: Some(binding),
                remote_apply_receipt,
                result,
                ..
            } if !binding.is_valid()
                || !remote_apply_receipt
                    .as_ref()
                    .is_some_and(|receipt| binding.matches_completed(receipt, result)) =>
            {
                Err(PeerSyncError::Storage(
                    "source completed operation has an invalid binding".to_owned(),
                ))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn status_projection(&self) -> Option<PeerBidirectionalStatusOperation> {
        match self {
            Self::SourcePrepared { operation_id, .. } => {
                Some(PeerBidirectionalStatusOperation::SourcePrepared {
                    operation_id: operation_id.clone(),
                })
            }
            Self::TargetPrepared { context, .. } => {
                Some(PeerBidirectionalStatusOperation::TargetPrepared {
                    operation_id: context.operation_id.clone(),
                })
            }
            Self::AwaitingConflict {
                context,
                conflicts,
                local_manifest_hash,
                remote_manifest_hash,
                ..
            } => Some(PeerBidirectionalStatusOperation::AwaitingConflict {
                result: PeerBidirectionalConflictResult {
                    kind: "conflict",
                    operation_id: context.operation_id.clone(),
                    conflicts: conflicts
                        .iter()
                        .map(|conflict| PeerBidirectionalConflictStatus {
                            key: conflict.key.clone(),
                            conflict_type: conflict.conflict_type.clone(),
                        })
                        .collect(),
                    local_manifest_hash: local_manifest_hash.clone(),
                    remote_manifest_hash: remote_manifest_hash.clone(),
                },
            }),
            Self::LocalCommitted {
                context,
                committed_revision,
                ..
            } => Some(PeerBidirectionalStatusOperation::LocalCommitted {
                operation_id: context.operation_id.clone(),
                committed_revision: *committed_revision,
            }),
            Self::Completed {
                source_binding,
                result,
                ..
            } => {
                let mut result = result.clone();
                if source_binding.is_some() {
                    for backup in &mut result.backups {
                        backup.side = match backup.side {
                            PeerBidirectionalBackupSide::Local => {
                                PeerBidirectionalBackupSide::Remote
                            }
                            PeerBidirectionalBackupSide::Remote => {
                                PeerBidirectionalBackupSide::Local
                            }
                        };
                    }
                }
                Some(PeerBidirectionalStatusOperation::Completed { result })
            }
        }
    }
}

fn valid_completion_context(context: &PeerBidirectionalOperationContext) -> bool {
    match context.completion_mode {
        PeerBidirectionalCompletionMode::Unsupported => context.completion_delivery.is_none(),
        PeerBidirectionalCompletionMode::V1 => {
            if super::device_registry::CompletionLeaseId::parse(&context.operation_id).is_err() {
                return false;
            }
            context.completion_delivery.as_ref().is_none_or(|delivery| {
                delivery.source_device_id == context.credential.source_device_id
                    && delivery.lane
                        == super::device_registry::CompletionLane::Bidirectional.as_str()
                    && delivery.completion_lease_id == context.operation_id
                    && delivery.manifest_id == context.credential.manifest_id
                    && delivery.receipt_id
                        == super::device_registry::completion_receipt_id(
                            "bidirectional",
                            &context.operation_id,
                            &context.credential.manifest_id,
                        )
            })
        }
    }
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn valid_generation(generation: &SyncGenerationIdentity) -> bool {
    !generation.generation_id.is_empty()
        && crate::trust_boundary::is_lower_hex_256(&generation.manifest_hash)
        && !generation.generation_sequence.is_empty()
        && (generation.generation_sequence == "0"
            || (!generation.generation_sequence.starts_with('0')
                && generation
                    .generation_sequence
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())))
}

fn valid_lan_generation(generation: &LanBidirectionalGeneration) -> bool {
    valid_generation(&SyncGenerationIdentity {
        generation_id: generation.generation_id.clone(),
        manifest_hash: generation.manifest_hash.clone(),
        generation_sequence: generation.generation_sequence.clone(),
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourcePreparedEvidence {
    operation_id: String,
    source_device_id: String,
    target_device_id: String,
    expected_source_revision: i64,
    previous_shared: SyncGenerationIdentity,
    expected_source_generation: SyncGenerationIdentity,
    shared_generation: LanBidirectionalGeneration,
    incoming_revision: i64,
    transferred_objects: u64,
    transferred_bytes: u64,
    backup_required: bool,
    backup: Option<LanBidirectionalBackupReceipt>,
    completion_deferred_v1: bool,
}

impl SourcePreparedEvidence {
    fn requires_backup(&self) -> bool {
        self.backup_required || self.backup.is_some()
    }

    fn is_valid(&self) -> bool {
        is_canonical_uuid(&self.operation_id)
            && is_canonical_uuid(&self.source_device_id)
            && is_canonical_uuid(&self.target_device_id)
            && self.expected_source_revision >= 0
            && self.expected_source_revision.checked_add(1).is_some()
            && self.incoming_revision >= 0
            && valid_generation(&self.previous_shared)
            && valid_generation(&self.expected_source_generation)
            && valid_lan_generation(&self.shared_generation)
            && self
                .backup
                .as_ref()
                .is_none_or(|backup| !backup.package_id.is_empty() && !backup.path.is_empty())
    }

    fn from_operation(operation: &PeerBidirectionalDurableOperation) -> Option<Self> {
        let PeerBidirectionalDurableOperation::SourcePrepared {
            operation_id,
            source_device_id,
            target_device_id,
            expected_source_revision,
            previous_shared,
            expected_source_generation,
            shared_generation,
            incoming_revision,
            transferred_objects,
            transferred_bytes,
            backup_required,
            backup,
            completion_deferred_v1,
            ..
        } = operation
        else {
            return None;
        };
        Some(Self {
            operation_id: operation_id.clone(),
            source_device_id: source_device_id.clone(),
            target_device_id: target_device_id.clone(),
            expected_source_revision: *expected_source_revision,
            previous_shared: previous_shared.clone(),
            expected_source_generation: expected_source_generation.clone(),
            shared_generation: shared_generation.clone(),
            incoming_revision: *incoming_revision,
            transferred_objects: *transferred_objects,
            transferred_bytes: *transferred_bytes,
            backup_required: *backup_required || backup.is_some(),
            backup: backup.clone(),
            completion_deferred_v1: *completion_deferred_v1,
        })
    }

    // Production paths compute receipts through receipt_at; tests use the
    // next-revision convenience form.
    #[cfg_attr(not(test), allow(dead_code))]
    fn receipt(&self) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
        let committed_revision = self
            .expected_source_revision
            .checked_add(1)
            .ok_or_else(|| {
                PeerSyncError::Validation("bidirectional source revision overflow".to_owned())
            })?;
        self.receipt_at(committed_revision)
    }

    fn receipt_at(
        &self,
        committed_revision: i64,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
        let changed_revision = self
            .expected_source_revision
            .checked_add(1)
            .ok_or_else(|| {
                PeerSyncError::Validation("bidirectional source revision overflow".to_owned())
            })?;
        if committed_revision != self.expected_source_revision
            && committed_revision != changed_revision
        {
            return Err(PeerSyncError::Validation(
                "bidirectional source receipt revision is not a no-op or single commit".to_owned(),
            ));
        }
        Ok(LanBidirectionalRemoteApplyReceipt {
            committed_revision,
            committed_generation: self.shared_generation.clone(),
            transferred_objects: self.transferred_objects,
            transferred_bytes: self.transferred_bytes,
            backup: self.backup.clone(),
        })
    }

    fn matches_receipt(&self, receipt: &LanBidirectionalRemoteApplyReceipt) -> bool {
        self.receipt_at(receipt.committed_revision)
            .is_ok_and(|expected| expected == *receipt)
    }

    fn matches_completed(
        &self,
        receipt: &LanBidirectionalRemoteApplyReceipt,
        result: &PeerBidirectionalCompletedResult,
    ) -> bool {
        let expected_kind = if receipt.committed_revision != self.expected_source_revision
            || receipt.transferred_objects != 0
        {
            "updated"
        } else {
            "noChanges"
        };
        let expected_backups = receipt
            .backup
            .iter()
            .map(|backup| PeerBidirectionalBackupReceipt {
                package_id: backup.package_id.clone(),
                side: PeerBidirectionalBackupSide::Remote,
                path: backup.path.clone(),
            })
            .collect::<Vec<_>>();
        self.matches_receipt(receipt)
            && result.operation_id == self.operation_id
            && result.revision >= receipt.committed_revision
            && result.remote_revision == self.incoming_revision
            && result.transferred_objects == receipt.transferred_objects
            && result.transferred_bytes == receipt.transferred_bytes
            && result.backups == expected_backups
            && result.kind == expected_kind
    }

    fn durable_operation(&self, durable_job_id: String) -> PeerBidirectionalDurableOperation {
        PeerBidirectionalDurableOperation::SourcePrepared {
            schema: OPERATION_SCHEMA.to_owned(),
            operation_id: self.operation_id.clone(),
            source_device_id: self.source_device_id.clone(),
            target_device_id: self.target_device_id.clone(),
            expected_source_revision: self.expected_source_revision,
            previous_shared: self.previous_shared.clone(),
            expected_source_generation: self.expected_source_generation.clone(),
            shared_generation: self.shared_generation.clone(),
            incoming_revision: self.incoming_revision,
            transferred_objects: self.transferred_objects,
            transferred_bytes: self.transferred_bytes,
            backup_required: self.requires_backup(),
            backup: self.backup.clone(),
            completion_deferred_v1: self.completion_deferred_v1,
            durable_job_id,
        }
    }
}

fn valid_backups(backups: &[PeerBidirectionalBackupReceipt]) -> bool {
    backups
        .iter()
        .all(|backup| !backup.package_id.is_empty() && !backup.path.is_empty())
}

fn lossless_source_binding(
    operation_id: &str,
    side: PeerBidirectionalBackupSide,
    source: &SyncGenerationIdentity,
) -> LosslessPeerSourceBinding {
    LosslessPeerSourceBinding::new(
        operation_id,
        match side {
            PeerBidirectionalBackupSide::Local => "local",
            PeerBidirectionalBackupSide::Remote => "remote",
        },
        &source.generation_id,
        &source.manifest_hash,
        &source.generation_sequence,
    )
}

fn verify_bidirectional_backup_receipt(
    path: &Path,
    operation_id: &str,
    expected_revision: i64,
    side: PeerBidirectionalBackupSide,
    expected_source: &SyncGenerationIdentity,
    expected_package_id: Option<&str>,
    cancellation: &dyn CancellationProbe,
) -> Result<PeerBidirectionalBackupReceipt, PeerSyncError> {
    #[cfg(test)]
    BACKUP_FULL_VERIFICATION_COUNT.with(|count| count.set(count.get() + 1));
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(PeerSyncError::Storage(
            "bidirectional backup path is not a regular file".to_owned(),
        ));
    }
    let verified = verify_lossless_package_v1_for_production(&mut File::open(path)?, cancellation)
        .map_err(peer_backup_error)?;
    if verified
        .manifest
        .extensions
        .get("sourceRevision")
        .and_then(serde_json::Value::as_i64)
        != Some(expected_revision)
    {
        return Err(PeerSyncError::Storage(
            "bidirectional backup belongs to another source revision".to_owned(),
        ));
    }
    if verified.manifest.extensions.get("peerBidirectionalSource")
        != Some(&lossless_source_binding(operation_id, side, expected_source).extension())
    {
        return Err(PeerSyncError::Storage(
            "bidirectional backup belongs to another source generation".to_owned(),
        ));
    }
    if expected_package_id.is_some_and(|package_id| package_id != verified.archive_sha256) {
        return Err(PeerSyncError::Storage(
            "bidirectional backup package differs from its receipt".to_owned(),
        ));
    }
    Ok(PeerBidirectionalBackupReceipt {
        package_id: verified.archive_sha256,
        side,
        path: path.to_string_lossy().into_owned(),
    })
}

fn bidirectional_backup_path(
    app_root: &Path,
    operation_id: &str,
    side: PeerBidirectionalBackupSide,
) -> PathBuf {
    let side_name = match side {
        PeerBidirectionalBackupSide::Local => "local",
        PeerBidirectionalBackupSide::Remote => "remote",
    };
    app_root
        .join("peer-bidirectional")
        .join("backups")
        .join(format!("{operation_id}-{side_name}.risulossless"))
}

fn verify_source_prepared_backup(
    app_root: &Path,
    evidence: &SourcePreparedEvidence,
) -> Result<(), PeerSyncError> {
    let Some(backup) = &evidence.backup else {
        return if evidence.requires_backup() {
            Err(PeerSyncError::Storage(
                "source-prepared operation is missing its required backup receipt".to_owned(),
            ))
        } else {
            Ok(())
        };
    };
    let expected_path = bidirectional_backup_path(
        app_root,
        &evidence.operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    if Path::new(&backup.path) != expected_path.as_path() {
        return Err(PeerSyncError::Storage(
            "source-prepared backup path differs from its operation".to_owned(),
        ));
    }
    let verified = verify_bidirectional_backup_receipt(
        &expected_path,
        &evidence.operation_id,
        evidence.expected_source_revision,
        PeerBidirectionalBackupSide::Remote,
        &evidence.expected_source_generation,
        Some(&backup.package_id),
        &NeverCancelled,
    )?;
    if verified.path != backup.path {
        return Err(PeerSyncError::Storage(
            "source-prepared backup path differs from its receipt".to_owned(),
        ));
    }
    Ok(())
}

fn bidirectional_backup_staging_paths(
    app_root: &Path,
    operation_id: &str,
    side: PeerBidirectionalBackupSide,
) -> (PathBuf, PathBuf) {
    let side_name = match side {
        PeerBidirectionalBackupSide::Local => "local",
        PeerBidirectionalBackupSide::Remote => "remote",
    };
    let staging = app_root
        .join("peer-bidirectional")
        .join("backup-staging")
        .join(format!("{operation_id}-{side_name}"));
    let temporary = staging.join("backup.risulossless");
    (staging, temporary)
}

fn remove_source_owned_backup_file(path: &Path) -> Result<(), PeerSyncError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn backup_path_is_link_like(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn backup_path_is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn remove_source_owned_backup_staging(
    app_root: &Path,
    operation_id: &str,
) -> Result<(), PeerSyncError> {
    let (staging, _) = bidirectional_backup_staging_paths(
        app_root,
        operation_id,
        PeerBidirectionalBackupSide::Remote,
    );
    let metadata = match fs::symlink_metadata(&staging) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || backup_path_is_link_like(&metadata) {
        return Err(PeerSyncError::Storage(
            "source backup staging path is not an owned plain directory".to_owned(),
        ));
    }
    let parent = staging.parent().ok_or_else(|| {
        PeerSyncError::Storage("source backup staging path has no parent".to_owned())
    })?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir() || backup_path_is_link_like(&parent_metadata) {
        return Err(PeerSyncError::Storage(
            "source backup staging parent is not an owned plain directory".to_owned(),
        ));
    }
    let canonical_root = fs::canonicalize(app_root)?;
    let canonical_parent = fs::canonicalize(parent)?;
    if canonical_parent
        != canonical_root
            .join("peer-bidirectional")
            .join("backup-staging")
    {
        return Err(PeerSyncError::Storage(
            "source backup staging parent escapes its repository".to_owned(),
        ));
    }
    let canonical_staging = fs::canonicalize(&staging)?;
    if canonical_staging.parent() != Some(canonical_parent.as_path()) {
        return Err(PeerSyncError::Storage(
            "source backup staging path escapes its owned parent".to_owned(),
        ));
    }
    match fs::remove_dir_all(canonical_staging) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn cleanup_source_prepared_backup_artifacts(
    app_root: &Path,
    evidence: &SourcePreparedEvidence,
) -> Result<(), PeerSyncError> {
    if !evidence.requires_backup() {
        return Ok(());
    }
    remove_source_owned_backup_staging(app_root, &evidence.operation_id)?;
    remove_source_owned_backup_file(&bidirectional_backup_path(
        app_root,
        &evidence.operation_id,
        PeerBidirectionalBackupSide::Remote,
    ))
}

#[derive(Debug)]
struct VerifiedBidirectionalBackupReceipt {
    receipt: PeerBidirectionalBackupReceipt,
    published: bool,
}

impl VerifiedBidirectionalBackupReceipt {
    fn receipt(&self) -> &PeerBidirectionalBackupReceipt {
        &self.receipt
    }

    fn into_receipt(self) -> PeerBidirectionalBackupReceipt {
        self.receipt
    }
}

#[derive(Debug)]
struct VerifiedRemoteApplyReceipt {
    receipt: LanBidirectionalRemoteApplyReceipt,
    _backup: Option<VerifiedBidirectionalBackupReceipt>,
}

impl VerifiedRemoteApplyReceipt {
    fn receipt(&self) -> &LanBidirectionalRemoteApplyReceipt {
        &self.receipt
    }

    fn into_receipt(self) -> LanBidirectionalRemoteApplyReceipt {
        self.receipt
    }
}

#[derive(Debug)]
struct VerifiedTargetPreparedLocalBackup {
    receipt: PeerBidirectionalBackupReceipt,
}

fn verify_target_prepared_local_backup(
    app_root: &Path,
    context: &PeerBidirectionalOperationContext,
    local_generation: &SyncGenerationIdentity,
    conflict_policy: TargetPreparedConflictPolicy,
    backups: &[PeerBidirectionalBackupReceipt],
) -> Result<Option<VerifiedTargetPreparedLocalBackup>, PeerSyncError> {
    if conflict_policy != TargetPreparedConflictPolicy::PreferRemote {
        return Ok(None);
    }
    let mut local_backups = backups
        .iter()
        .filter(|backup| backup.side == PeerBidirectionalBackupSide::Local);
    let receipt = local_backups.next().ok_or_else(|| {
        PeerSyncError::Storage(
            "remote-winner target preparation is missing its local backup".to_owned(),
        )
    })?;
    if local_backups.next().is_some() {
        return Err(PeerSyncError::Storage(
            "remote-winner target preparation has duplicate local backups".to_owned(),
        ));
    }
    let expected_path = bidirectional_backup_path(
        app_root,
        &context.operation_id,
        PeerBidirectionalBackupSide::Local,
    );
    if Path::new(&receipt.path) != expected_path {
        return Err(PeerSyncError::Storage(
            "remote-winner target backup path differs from its operation".to_owned(),
        ));
    }
    let verified = verify_bidirectional_backup_receipt(
        &expected_path,
        &context.operation_id,
        context.expected_local_revision,
        PeerBidirectionalBackupSide::Local,
        local_generation,
        Some(&receipt.package_id),
        &NeverCancelled,
    )?;
    if verified != *receipt {
        return Err(PeerSyncError::Storage(
            "remote-winner target backup differs from its receipt".to_owned(),
        ));
    }
    Ok(Some(VerifiedTargetPreparedLocalBackup {
        receipt: verified,
    }))
}

fn require_target_prepared_local_backup_proof(
    prepared: &PeerBidirectionalDurableOperation,
    proof: Option<&VerifiedTargetPreparedLocalBackup>,
) -> Result<(), PeerSyncError> {
    let PeerBidirectionalDurableOperation::TargetPrepared {
        conflict_policy,
        backups,
        ..
    } = prepared
    else {
        return Err(PeerSyncError::Storage(
            "target backup proof lacks its durable preparation".to_owned(),
        ));
    };
    if *conflict_policy != TargetPreparedConflictPolicy::PreferRemote {
        return Ok(());
    }
    let Some(proof) = proof else {
        return Err(PeerSyncError::Storage(
            "remote-winner target preparation lacks verified local backup proof".to_owned(),
        ));
    };
    if !backups.iter().any(|backup| backup == &proof.receipt) {
        return Err(PeerSyncError::Storage(
            "remote-winner target backup proof differs from its durable receipt".to_owned(),
        ));
    }
    Ok(())
}

fn published_backup_receipt(
    verified: PeerBidirectionalBackupReceipt,
    backup_path: &Path,
) -> VerifiedBidirectionalBackupReceipt {
    VerifiedBidirectionalBackupReceipt {
        receipt: PeerBidirectionalBackupReceipt {
            package_id: verified.package_id,
            side: verified.side,
            path: backup_path.to_string_lossy().into_owned(),
        },
        published: true,
    }
}

fn ensure_bidirectional_backup_receipt(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    side: PeerBidirectionalBackupSide,
    expected_source: &SyncGenerationIdentity,
    bound_package_id: Option<&str>,
    cancellation: &dyn CancellationProbe,
) -> Result<VerifiedBidirectionalBackupReceipt, PeerSyncError> {
    let backup_path = bidirectional_backup_path(app_root, operation_id, side.clone());
    let backup_root = backup_path.parent().ok_or_else(|| {
        PeerSyncError::Storage("bidirectional backup path has no parent".to_owned())
    })?;
    fs::create_dir_all(&backup_root)?;
    match fs::symlink_metadata(&backup_path) {
        Ok(_) => match verify_bidirectional_backup_receipt(
            &backup_path,
            operation_id,
            expected_revision,
            side.clone(),
            expected_source,
            bound_package_id,
            cancellation,
        ) {
            Ok(receipt) => {
                #[cfg(not(windows))]
                sync_destination_parent(&backup_path)?;
                return Ok(VerifiedBidirectionalBackupReceipt {
                    receipt,
                    published: false,
                });
            }
            Err(PeerSyncError::Cancelled) => return Err(PeerSyncError::Cancelled),
            Err(error) if bound_package_id.is_some() => return Err(error),
            Err(_) => fs::remove_file(&backup_path)?,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if bound_package_id.is_some() {
                return Err(PeerSyncError::Storage(
                    "bound bidirectional backup is missing".to_owned(),
                ));
            }
        }
        Err(error) => return Err(error.into()),
    }
    let (backup_staging, temporary) =
        bidirectional_backup_staging_paths(app_root, operation_id, side.clone());
    let _maintenance_guard = ActiveTempGuard::acquire(&backup_staging)?;
    fs::create_dir_all(&backup_staging)?;
    #[cfg(test)]
    BACKUP_STAGING_MAINTENANCE_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(&backup_staging);
        }
    });
    let temporary_created_by_invocation = match fs::symlink_metadata(&temporary) {
        Ok(_) => match verify_bidirectional_backup_receipt(
            &temporary,
            operation_id,
            expected_revision,
            side.clone(),
            expected_source,
            None,
            cancellation,
        ) {
            Ok(receipt) => {
                replace_file_atomic(&temporary, &backup_path)?;
                return Ok(published_backup_receipt(receipt, &backup_path));
            }
            Err(PeerSyncError::Cancelled) => return Err(PeerSyncError::Cancelled),
            Err(_) => {
                fs::remove_file(&temporary)?;
                true
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    let source_binding = lossless_source_binding(operation_id, side.clone(), expected_source);
    match create_and_verify_peer_bidirectional_backup_v1_report(
        &temporary,
        &backup_staging,
        cas,
        store,
        expected_revision,
        &source_binding,
        cancellation,
    ) {
        Ok(report) => {
            #[cfg(test)]
            if let Some(cancelled) =
                BACKUP_CREATE_RACE_CANCEL_FAILPOINT.with(|slot| slot.borrow_mut().take())
            {
                fs::copy(&temporary, &backup_path)?;
                cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            match fs::symlink_metadata(&backup_path) {
                Ok(_) => {
                    match verify_bidirectional_backup_receipt(
                        &backup_path,
                        operation_id,
                        expected_revision,
                        side.clone(),
                        expected_source,
                        bound_package_id,
                        cancellation,
                    ) {
                        Ok(receipt) => {
                            fs::remove_file(&temporary)?;
                            #[cfg(not(windows))]
                            sync_destination_parent(&backup_path)?;
                            return Ok(VerifiedBidirectionalBackupReceipt {
                                receipt,
                                published: false,
                            });
                        }
                        Err(PeerSyncError::Cancelled) => {
                            if temporary_created_by_invocation {
                                match fs::remove_file(&temporary) {
                                    Ok(()) => {}
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                    Err(error) => {
                                        return Err(PeerSyncError::Storage(format!(
                                            "source cancellation could not remove its backup temp: {error}"
                                        )))
                                    }
                                }
                            }
                            return Err(PeerSyncError::Cancelled);
                        }
                        Err(error) if bound_package_id.is_some() => return Err(error),
                        Err(_) => fs::remove_file(&backup_path)?,
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            replace_file_atomic(&temporary, &backup_path)?;
            Ok(VerifiedBidirectionalBackupReceipt {
                receipt: PeerBidirectionalBackupReceipt {
                    package_id: report.archive_sha256,
                    side,
                    path: backup_path.to_string_lossy().into_owned(),
                },
                published: true,
            })
        }
        Err(error) => Err(peer_backup_error(error)),
    }
}

fn peer_backup_error(error: LosslessError) -> PeerSyncError {
    if error.code == LosslessErrorCode::Cancelled {
        PeerSyncError::Cancelled
    } else {
        PeerSyncError::Storage(error.to_string())
    }
}

fn remove_newly_published_backup_after_cancellation(
    path: Option<&Path>,
) -> Result<(), PeerSyncError> {
    let Some(path) = path else {
        return Ok(());
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(PeerSyncError::Storage(format!(
            "source cancellation could not remove its newly published backup: {error}"
        ))),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LocalMergeOutcome {
    Conflict(PeerBidirectionalConflictResult),
    LocalCommitted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PeerBidirectionalConflictWinner {
    Local,
    Remote,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ResumeLocalCommittedOutcome {
    Completed(PeerBidirectionalCompletedResult),
    SourceUnavailable {
        operation_id: String,
        committed_revision: i64,
    },
    Stale {
        operation_id: String,
        reason: PeerBidirectionalStaleReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerBidirectionalStaleReason {
    // Mirrors the TypeScript stale-reason union in peerBidirectional.ts; only
    // RemoteGeneration is produced by the current Rust paths, but the wire
    // contract keeps every declared reason representable.
    #[allow(dead_code)]
    LocalRevision,
    RemoteGeneration,
    #[allow(dead_code)]
    CommonBase,
    #[allow(dead_code)]
    DeviceAcknowledgement,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PeerBidirectionalSyncResult {
    NoChanges {
        operation_id: String,
        revision: i64,
        remote_revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    Updated {
        operation_id: String,
        revision: i64,
        remote_revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    Conflict {
        operation_id: String,
        conflicts: Vec<PeerBidirectionalConflictStatus>,
        local_manifest_hash: String,
        remote_manifest_hash: String,
    },
    Stale {
        operation_id: String,
        reason: PeerBidirectionalStaleReason,
    },
    ResumeRequired {
        operation_id: String,
        phase: &'static str,
        committed_revision: i64,
    },
    SourceUnavailable {
        operation_id: String,
        committed_revision: i64,
    },
}

fn completed_result(result: PeerBidirectionalCompletedResult) -> PeerBidirectionalSyncResult {
    let PeerBidirectionalCompletedResult {
        kind,
        operation_id,
        revision,
        remote_revision,
        transferred_objects,
        transferred_bytes,
        backups,
    } = result;
    if kind == "noChanges" {
        PeerBidirectionalSyncResult::NoChanges {
            operation_id,
            revision,
            remote_revision,
            transferred_objects,
            transferred_bytes,
            backups,
        }
    } else {
        PeerBidirectionalSyncResult::Updated {
            operation_id,
            revision,
            remote_revision,
            transferred_objects,
            transferred_bytes,
            backups,
        }
    }
}

fn conflict_result(result: PeerBidirectionalConflictResult) -> PeerBidirectionalSyncResult {
    PeerBidirectionalSyncResult::Conflict {
        operation_id: result.operation_id,
        conflicts: result.conflicts,
        local_manifest_hash: result.local_manifest_hash,
        remote_manifest_hash: result.remote_manifest_hash,
    }
}

fn resumed_result(result: ResumeLocalCommittedOutcome) -> PeerBidirectionalSyncResult {
    match result {
        ResumeLocalCommittedOutcome::Completed(result) => completed_result(result),
        ResumeLocalCommittedOutcome::SourceUnavailable {
            operation_id,
            committed_revision,
        } => PeerBidirectionalSyncResult::SourceUnavailable {
            operation_id,
            committed_revision,
        },
        ResumeLocalCommittedOutcome::Stale {
            operation_id,
            reason,
        } => PeerBidirectionalSyncResult::Stale {
            operation_id,
            reason,
        },
    }
}

fn retained_result(
    operation: &PeerBidirectionalDurableOperation,
) -> Result<PeerBidirectionalSyncResult, PeerSyncError> {
    Ok(match operation {
        PeerBidirectionalDurableOperation::SourcePrepared { .. } => {
            return Err(PeerSyncError::Validation(
                "bidirectional source operation requires an authenticated retry".to_owned(),
            ));
        }
        PeerBidirectionalDurableOperation::TargetPrepared { .. } => {
            return Err(PeerSyncError::Validation(
                "bidirectional target operation requires an authenticated retry".to_owned(),
            ));
        }
        PeerBidirectionalDurableOperation::AwaitingConflict {
            context,
            conflicts,
            local_manifest_hash,
            remote_manifest_hash,
            ..
        } => PeerBidirectionalSyncResult::Conflict {
            operation_id: context.operation_id.clone(),
            conflicts: conflicts
                .iter()
                .map(|conflict| PeerBidirectionalConflictStatus {
                    key: conflict.key.clone(),
                    conflict_type: conflict.conflict_type.clone(),
                })
                .collect(),
            local_manifest_hash: local_manifest_hash.clone(),
            remote_manifest_hash: remote_manifest_hash.clone(),
        },
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            committed_revision,
            ..
        } => PeerBidirectionalSyncResult::ResumeRequired {
            operation_id: context.operation_id.clone(),
            phase: "localCommitted",
            committed_revision: *committed_revision,
        },
        PeerBidirectionalDurableOperation::Completed { result, .. } => {
            completed_result(result.clone())
        }
    })
}

fn rebind_awaiting_conflict(
    app_root: &Path,
    mut retained: PeerBidirectionalDurableOperation,
    credential: LanBidirectionalLogicalCredential,
    remote_library_id: &str,
    remote_revision: i64,
    remote_generation: &LanBidirectionalGeneration,
) -> Result<PeerBidirectionalSyncResult, PeerSyncError> {
    let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &mut retained else {
        return Err(PeerSyncError::Validation(
            "only an awaiting-conflict operation can rebind its source".to_owned(),
        ));
    };
    if credential.device_id != context.credential.device_id
        || credential.source_device_id != context.credential.source_device_id
        || remote_library_id != context.library_id
        || remote_revision != context.expected_remote_revision
        || *remote_generation != context.expected_remote_generation
    {
        return Err(PeerSyncError::Validation(
            "fresh bidirectional source differs from the retained conflict".to_owned(),
        ));
    }
    context.credential = credential;
    PeerBidirectionalOperationJournal::new(app_root).store(&retained)?;
    retained_result(&retained)
}

fn validate_fresh_awaiting_conflict_source(
    retained: &PeerBidirectionalDurableOperation,
    operation_id: &str,
    credential: &LanBidirectionalLogicalCredential,
    remote_library_id: &str,
    remote_revision: i64,
    remote_generation: &LanBidirectionalGeneration,
) -> Result<(), PeerSyncError> {
    let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = retained else {
        return Err(PeerSyncError::Validation(
            "only an awaiting-conflict operation can use a fresh source".to_owned(),
        ));
    };
    if context.operation_id != operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    if credential.device_id != context.credential.device_id
        || credential.source_device_id != context.credential.source_device_id
        || credential.manifest_id != remote_generation.manifest_hash
        || remote_library_id != context.library_id
        || remote_revision != context.expected_remote_revision
        || *remote_generation != context.expected_remote_generation
    {
        return Err(PeerSyncError::Validation(
            "fresh bidirectional source differs from the retained conflict".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn resolve_awaiting_conflict_with_fresh_source<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    retained: &PeerBidirectionalDurableOperation,
    operation_id: &str,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    credential: &LanBidirectionalLogicalCredential,
    remote_manifest_bytes: &[u8],
    source: &mut S,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    resolve_awaiting_conflict_with_fresh_source_and_cancellation(
        store,
        cas,
        app_root,
        retained,
        operation_id,
        winner,
        expected_revision,
        credential,
        remote_manifest_bytes,
        source,
        &NeverCancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_awaiting_conflict_with_fresh_source_and_cancellation<
    S: LogicalDeltaObjectSource + ?Sized,
>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    retained: &PeerBidirectionalDurableOperation,
    operation_id: &str,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    credential: &LanBidirectionalLogicalCredential,
    remote_manifest_bytes: &[u8],
    source: &mut S,
    cancellation: &dyn CancellationProbe,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let remote_revision = i64::try_from(remote_manifest.source_revision).map_err(|_| {
        PeerSyncError::Validation("bidirectional peer revision exceeds SQLite range".to_owned())
    })?;
    let remote_generation = LanBidirectionalGeneration {
        generation_id: remote_manifest.generation.clone(),
        manifest_hash: hash_logical_manifest(&remote_manifest)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?,
        generation_sequence: remote_manifest.generation_sequence.clone(),
    };
    validate_fresh_awaiting_conflict_source(
        retained,
        operation_id,
        credential,
        &remote_manifest.library_id,
        remote_revision,
        &remote_generation,
    )?;
    resolve_bidirectional_conflict_with_cancellation(
        store,
        cas,
        app_root,
        operation_id,
        winner,
        expected_revision,
        remote_manifest_bytes,
        source,
        cancellation,
    )
}

#[derive(Default)]
struct MeasuredTransferTotals {
    objects: Cell<u64>,
    bytes: Cell<u64>,
}

struct MeasuredLogicalDeltaSource<'a, S: LogicalDeltaObjectSource + ?Sized> {
    inner: &'a mut S,
    totals: Rc<MeasuredTransferTotals>,
}

impl<'a, S: LogicalDeltaObjectSource + ?Sized> MeasuredLogicalDeltaSource<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            totals: Rc::new(MeasuredTransferTotals::default()),
        }
    }

    fn totals(&self) -> (u64, u64) {
        (self.totals.objects.get(), self.totals.bytes.get())
    }
}

impl<S: LogicalDeltaObjectSource + ?Sized> LogicalDeltaObjectSource
    for MeasuredLogicalDeltaSource<'_, S>
{
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        let reader = self.inner.open_object(object)?;
        self.totals
            .objects
            .set(self.totals.objects.get().checked_add(1).ok_or_else(|| {
                PeerSyncError::Validation("bidirectional transfer object count overflow".to_owned())
            })?);
        Ok(Box::new(MeasuredLogicalDeltaReader {
            inner: reader,
            totals: Rc::clone(&self.totals),
        }))
    }
}

struct MeasuredLogicalDeltaReader {
    inner: Box<dyn Read>,
    totals: Rc<MeasuredTransferTotals>,
}

impl Read for MeasuredLogicalDeltaReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        self.totals.bytes.set(
            self.totals
                .bytes
                .get()
                .checked_add(read as u64)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bidirectional transfer byte count overflow",
                    )
                })?,
        );
        Ok(read)
    }
}

fn predicted_transfer_totals(
    selection: &super::LogicalDeltaTransferSelection,
) -> Result<(u64, u64), PeerSyncError> {
    let objects = u64::try_from(selection.missing_objects().len()).map_err(|_| {
        PeerSyncError::Validation("bidirectional transfer object count overflow".to_owned())
    })?;
    let bytes = selection
        .missing_objects()
        .iter()
        .try_fold(0_u64, |total, object| {
            total.checked_add(object.size).ok_or_else(|| {
                PeerSyncError::Validation("bidirectional transfer byte count overflow".to_owned())
            })
        })?;
    Ok((objects, bytes))
}

fn open_or_begin_target_job(
    app_root: &Path,
    durable_job_id: &str,
) -> Result<DurableCasJob, PeerSyncError> {
    let job = match DurableCasJob::open(app_root, durable_job_id) {
        Ok(job) => job,
        Err(error) if error.kind() == io::ErrorKind::NotFound => DurableCasJob::begin(
            app_root,
            durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            now_millis()?,
        )?,
        Err(error) => return Err(error.into()),
    };
    if job.kind() != CasJobKind::LogicalDeltaTarget || job.is_released() {
        return Err(PeerSyncError::Storage(
            "retained target job has an unexpected identity or state".to_owned(),
        ));
    }
    Ok(job)
}

fn release_retained_target_job(
    app_root: &Path,
    durable_job_id: &str,
    outcome: CasReleaseOutcome,
) -> Result<(), PeerSyncError> {
    let mut job = match DurableCasJob::open(app_root, durable_job_id) {
        Ok(job) => job,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if job.kind() != CasJobKind::LogicalDeltaTarget || (!job.is_sealed() && !job.is_released()) {
        return Err(PeerSyncError::Storage(
            "retained committed target job has an unexpected identity or state".to_owned(),
        ));
    }
    if job.is_released() {
        job.release(outcome)?;
        return Ok(());
    }
    #[cfg(test)]
    if TARGET_JOB_RELEASE_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated retained target job release failure".to_owned(),
        ));
    }
    job.release(outcome)?;
    Ok(())
}

fn retain_target_prepared_activation(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    prepared: &PeerBidirectionalDurableOperation,
    verified_local_backup: Option<&VerifiedTargetPreparedLocalBackup>,
    activation: Result<LogicalDeltaActivation, PeerSyncError>,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    let PeerBidirectionalDurableOperation::TargetPrepared {
        context,
        changed,
        remote_backup_required,
        transferred_objects,
        transferred_bytes,
        backups,
        ..
    } = prepared
    else {
        return Err(PeerSyncError::Storage(
            "target-prepared activation lacks its durable evidence".to_owned(),
        ));
    };
    let committed_revision = match activation {
        Ok(LogicalDeltaActivation::Activated { revision })
        | Ok(LogicalDeltaActivation::AlreadyActive { revision }) => revision,
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision,
            actual_base_manifest_hash,
        }) => {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(context.expected_local_revision.to_string()),
                actual: Some(format!("{actual_revision}:{actual_base_manifest_hash}")),
            });
        }
        Err(error) => return Err(error),
    };
    let expected_committed_revision = if *changed {
        context
            .expected_local_revision
            .checked_add(1)
            .ok_or_else(|| {
                PeerSyncError::Validation("bidirectional local revision overflow".to_owned())
            })?
    } else {
        context.expected_local_revision
    };
    if committed_revision != expected_committed_revision {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_committed_revision.to_string()),
            actual: Some(committed_revision.to_string()),
        });
    }
    require_target_prepared_local_backup_proof(prepared, verified_local_backup)?;
    #[cfg(test)]
    if TARGET_AFTER_ACTIVATION_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated process loss after target activation".to_owned(),
        ));
    }
    let shared = store
        .seal_active_logical_generation_at_revision(cas, committed_revision)
        .map_err(|error| match error {
            StoreError::RevisionConflict { expected, actual } => {
                PeerSyncError::ActivationConflict {
                    expected: Some(expected.to_string()),
                    actual: Some(actual.to_string()),
                }
            }
            error => store_error(error),
        })?;
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    if journal.load()?.as_ref() != Some(prepared) {
        return Err(PeerSyncError::Storage(
            "target-prepared operation changed before local commit".to_owned(),
        ));
    }
    journal.store(&PeerBidirectionalDurableOperation::LocalCommitted {
        schema: OPERATION_SCHEMA.to_owned(),
        context: context.clone(),
        committed_revision,
        shared_generation: SyncGenerationIdentity {
            generation_id: shared.manifest.generation,
            manifest_hash: shared.manifest_hash,
            generation_sequence: shared.manifest.generation_sequence,
        },
        changed: *changed,
        remote_backup_required: *remote_backup_required,
        remote_apply_receipt: None,
        transferred_objects: if *changed { *transferred_objects } else { 0 },
        transferred_bytes: if *changed { *transferred_bytes } else { 0 },
        backups: backups.clone(),
    })?;
    release_retained_target_job(
        app_root,
        &context.durable_job_id,
        CasReleaseOutcome::Committed,
    )?;
    Ok(LocalMergeOutcome::LocalCommitted)
}

#[allow(clippy::too_many_arguments)]
// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn retain_bidirectional_local_activation(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    context: PeerBidirectionalOperationContext,
    job: &RefCell<DurableCasJob>,
    expected_revision: i64,
    activation: Result<LogicalDeltaActivation, PeerSyncError>,
    changed: bool,
    remote_backup_required: bool,
    transferred_objects: u64,
    transferred_bytes: u64,
    backups: Vec<PeerBidirectionalBackupReceipt>,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    let committed_revision = match activation {
        Ok(LogicalDeltaActivation::Activated { revision })
        | Ok(LogicalDeltaActivation::AlreadyActive { revision }) => revision,
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision,
            actual_base_manifest_hash,
        }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(format!("{actual_revision}:{actual_base_manifest_hash}")),
            });
        }
        Err(error) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            return Err(error);
        }
    };
    let shared = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    PeerBidirectionalOperationJournal::new(app_root).store(
        &PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context,
            committed_revision,
            shared_generation: SyncGenerationIdentity {
                generation_id: shared.manifest.generation,
                manifest_hash: shared.manifest_hash,
                generation_sequence: shared.manifest.generation_sequence,
            },
            changed,
            remote_backup_required,
            remote_apply_receipt: None,
            transferred_objects: if changed { transferred_objects } else { 0 },
            transferred_bytes: if changed { transferred_bytes } else { 0 },
            backups,
        },
    )?;
    job.borrow_mut().release(CasReleaseOutcome::Committed)?;
    Ok(LocalMergeOutcome::LocalCommitted)
}

// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn begin_bidirectional_local_merge<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    credential: LanBidirectionalLogicalCredential,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    begin_bidirectional_local_merge_with_cancellation(
        store,
        cas,
        app_root,
        credential,
        expected_revision,
        remote_manifest_bytes,
        remote_source,
        &NeverCancelled,
        None,
        PeerBidirectionalCompletionMode::Unsupported,
    )
}

#[allow(clippy::too_many_arguments)]
fn begin_bidirectional_local_merge_with_cancellation<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    credential: LanBidirectionalLogicalCredential,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
    cancellation: &dyn CancellationProbe,
    completion_operation_id: Option<String>,
    completion_mode: PeerBidirectionalCompletionMode,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    if cancellation.is_cancelled() {
        return Err(PeerSyncError::Cancelled);
    }
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    if journal.load()?.is_some() {
        return Err(PeerSyncError::Validation(
            "a bidirectional operation is already retained".to_owned(),
        ));
    }
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != PRODUCT_LOGICAL_LIBRARY_ID {
        return Err(PeerSyncError::Validation(
            "bidirectional peer belongs to another logical library".to_owned(),
        ));
    }
    let remote_manifest_hash = hash_logical_manifest(&remote_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest_hash != credential.manifest_id {
        return Err(PeerSyncError::Validation(
            "bidirectional peer manifest differs from its credential".to_owned(),
        ));
    }
    let expected_remote_revision =
        i64::try_from(remote_manifest.source_revision).map_err(|_| {
            PeerSyncError::Validation("bidirectional peer revision exceeds SQLite range".to_owned())
        })?;
    let local = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let previous = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, &credential.source_device_id)
        .map_err(store_error)?;
    let operation_id = completion_operation_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let durable_job_id = operation_id.clone();
    let context = PeerBidirectionalOperationContext {
        operation_id: operation_id.clone(),
        credential,
        library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
        expected_local_revision: expected_revision,
        expected_remote_revision,
        expected_remote_generation: LanBidirectionalGeneration {
            generation_id: remote_manifest.generation.clone(),
            manifest_hash: remote_manifest_hash.clone(),
            generation_sequence: remote_manifest.generation_sequence.clone(),
        },
        previous_shared: previous.shared_identity,
        previous_local: previous.local_identity,
        durable_job_id,
        completion_mode,
        completion_delivery: None,
    };
    let target = PersistentLogicalDeltaTarget::new_p5_deferred(
        store,
        cas,
        &context.credential.source_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &local.manifest.generation,
        remote_manifest_bytes,
        &app_root.join("peer-bidirectional").join("staging"),
        LogicalDeltaConflictPolicy::Reject,
    )?;
    let resolution = target.resolve_authoritative_plan(expected_revision);
    match resolution {
        Ok(LogicalDeltaPlanResolution::Conflict { conflicts }) => {
            drop(target);
            let conflicts = conflicts
                .into_iter()
                .map(|conflict| PeerBidirectionalConflict {
                    key: conflict.record,
                    conflict_type: match conflict.kind {
                        LogicalDeltaConflictKind::SameRecord => "sameRecord",
                        LogicalDeltaConflictKind::DeleteVsEdit => "deleteVsEdit",
                    }
                    .to_owned(),
                })
                .collect::<Vec<_>>();
            let durable = PeerBidirectionalDurableOperation::AwaitingConflict {
                schema: OPERATION_SCHEMA.to_owned(),
                context,
                conflicts: conflicts.clone(),
                local_generation: SyncGenerationIdentity {
                    generation_id: local.manifest.generation.clone(),
                    manifest_hash: local.manifest_hash.clone(),
                    generation_sequence: local.manifest.generation_sequence.clone(),
                },
                local_manifest_hash: local.manifest_hash.clone(),
                remote_manifest_hash: remote_manifest_hash.clone(),
                backups: vec![],
            };
            #[cfg(test)]
            if TARGET_CONFLICT_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated awaiting-conflict journal store failure".to_owned(),
                ));
            }
            store_initial_registered_target_operation(app_root, &journal, &durable)?;
            let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &durable
            else {
                unreachable!();
            };
            open_or_begin_target_job(app_root, &context.durable_job_id)?;
            if cancellation.is_cancelled() {
                return Err(PeerSyncError::Cancelled);
            }
            Ok(LocalMergeOutcome::Conflict(
                PeerBidirectionalConflictResult {
                    kind: "conflict",
                    operation_id,
                    conflicts: conflicts
                        .into_iter()
                        .map(|conflict| PeerBidirectionalConflictStatus {
                            key: conflict.key,
                            conflict_type: conflict.conflict_type,
                        })
                        .collect(),
                    local_manifest_hash: local.manifest_hash,
                    remote_manifest_hash,
                },
            ))
        }
        Ok(LogicalDeltaPlanResolution::Ready { plan, .. }) => {
            drop(target);
            let changed = !plan.apply.is_empty();
            let local_hashes = local
                .manifest
                .objects
                .iter()
                .map(|object| object.hash.clone())
                .collect::<BTreeSet<_>>();
            let remote_sizes = remote_manifest
                .objects
                .iter()
                .map(|object| (object.hash.clone(), object.size))
                .collect::<BTreeMap<_, _>>();
            let selection =
                select_missing_logical_delta_objects(&plan, &local_hashes, cas, &remote_sizes)?;
            let (transferred_objects, transferred_bytes) = predicted_transfer_totals(&selection)?;
            let prepared = PeerBidirectionalDurableOperation::TargetPrepared {
                schema: OPERATION_SCHEMA.to_owned(),
                context: context.clone(),
                local_generation: SyncGenerationIdentity {
                    generation_id: local.manifest.generation.clone(),
                    manifest_hash: local.manifest_hash.clone(),
                    generation_sequence: local.manifest.generation_sequence.clone(),
                },
                conflict_policy: TargetPreparedConflictPolicy::Reject,
                changed,
                remote_backup_required: false,
                transferred_objects,
                transferred_bytes,
                backups: vec![],
            };
            #[cfg(test)]
            if TARGET_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated target-prepared journal store failure".to_owned(),
                ));
            }
            store_initial_registered_target_operation(app_root, &journal, &prepared)?;
            #[cfg(test)]
            if TARGET_AFTER_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated process loss after target-prepared journal store".to_owned(),
                ));
            }
            let job = RefCell::new(open_or_begin_target_job(app_root, &context.durable_job_id)?);
            let mut target = PersistentLogicalDeltaTarget::new_p5_deferred_with_durable_job(
                store,
                cas,
                &context.credential.source_device_id,
                PRODUCT_LOGICAL_LIBRARY_ID,
                &local.manifest.generation,
                remote_manifest_bytes,
                &app_root.join("peer-bidirectional").join("staging"),
                &job,
                LogicalDeltaConflictPolicy::Reject,
            )?;
            let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
            let remaining_totals = Cell::new(None);
            let activation = execute_logical_delta_pull_with_pre_activation(
                &plan,
                &local_hashes,
                cas,
                &remote_sizes,
                &mut measured_source,
                &mut target,
                |selection| {
                    let totals = predicted_transfer_totals(selection)?;
                    if totals.0 > transferred_objects || totals.1 > transferred_bytes {
                        return Err(PeerSyncError::Storage(
                            "target transfer exceeds its durable selection".to_owned(),
                        ));
                    }
                    remaining_totals.set(Some(totals));
                    Ok(())
                },
                cancellation,
            );
            let measured_totals = measured_source.totals();
            drop(target);
            drop(job);
            if activation.is_ok()
                && remaining_totals
                    .get()
                    .is_none_or(|remaining| measured_totals != remaining)
            {
                return Err(PeerSyncError::Storage(
                    "target transfer totals differ from the selected remainder".to_owned(),
                ));
            }
            retain_target_prepared_activation(store, cas, app_root, &prepared, None, activation)
        }
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn resume_bidirectional_target_prepared<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    replacement_credential: Option<LanBidirectionalLogicalCredential>,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    resume_bidirectional_target_prepared_with_cancellation(
        store,
        cas,
        app_root,
        operation_id,
        expected_revision,
        replacement_credential,
        remote_manifest_bytes,
        remote_source,
        &NeverCancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn resume_bidirectional_target_prepared_with_cancellation<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    replacement_credential: Option<LanBidirectionalLogicalCredential>,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
    cancellation: &dyn CancellationProbe,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let mut prepared = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
    })?;
    if prepared.operation_id() != operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    let (
        mut context,
        local_generation,
        conflict_policy,
        changed,
        transferred_objects,
        transferred_bytes,
    ) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            local_generation,
            conflict_policy,
            changed,
            transferred_objects,
            transferred_bytes,
            ..
        } => (
            context.clone(),
            local_generation.clone(),
            *conflict_policy,
            *changed,
            *transferred_objects,
            *transferred_bytes,
        ),
        _ => {
            return Err(PeerSyncError::Validation(
                "bidirectional operation is not target-prepared".to_owned(),
            ));
        }
    };
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision != expected_revision {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_revision.to_string()),
            actual: Some(actual_revision.to_string()),
        });
    }
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let remote_manifest_hash = hash_logical_manifest(&remote_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let remote_revision = i64::try_from(remote_manifest.source_revision).map_err(|_| {
        PeerSyncError::Validation("bidirectional peer revision exceeds SQLite range".to_owned())
    })?;
    if remote_manifest.library_id != context.library_id
        || remote_revision != context.expected_remote_revision
        || remote_manifest.generation != context.expected_remote_generation.generation_id
        || remote_manifest.generation_sequence
            != context.expected_remote_generation.generation_sequence
        || remote_manifest_hash != context.expected_remote_generation.manifest_hash
    {
        return Err(PeerSyncError::StaleManifest {
            expected: context.expected_remote_generation.manifest_hash.clone(),
            received: remote_manifest_hash,
        });
    }
    if let Some(credential) = replacement_credential {
        if credential.device_id != context.credential.device_id
            || credential.source_device_id != context.credential.source_device_id
            || credential.manifest_id != remote_manifest_hash
        {
            return Err(PeerSyncError::Validation(
                "fresh bidirectional pairing differs from target-prepared identity".to_owned(),
            ));
        }
        context.credential = credential;
        let PeerBidirectionalDurableOperation::TargetPrepared {
            context: retained_context,
            ..
        } = &mut prepared
        else {
            unreachable!();
        };
        *retained_context = context.clone();
        journal.store(&prepared)?;
    }
    let local = store
        .build_indexed_logical_manifest(&context.library_id, &local_generation.generation_id)
        .map_err(store_error)?;
    if local.manifest_hash != local_generation.manifest_hash
        || local.manifest.generation_sequence != local_generation.generation_sequence
        || i64::try_from(local.manifest.source_revision).ok()
            != Some(context.expected_local_revision)
    {
        return Err(PeerSyncError::Storage(
            "target-prepared local generation differs from its indexed witness".to_owned(),
        ));
    }
    let verified_local_backup = verify_target_prepared_local_backup(
        app_root,
        &context,
        &local_generation,
        conflict_policy,
        match &prepared {
            PeerBidirectionalDurableOperation::TargetPrepared { backups, .. } => backups,
            _ => unreachable!(),
        },
    )?;
    let durable_job = open_or_begin_target_job(app_root, &context.durable_job_id)?;
    let recovered_after_staging = durable_job.is_sealed();
    if recovered_after_staging
        && !durable_job
            .root_set()?
            .object_hashes
            .contains(&remote_manifest_hash)
    {
        return Err(PeerSyncError::Storage(
            "target-prepared job does not own the remote manifest".to_owned(),
        ));
    }
    let job = RefCell::new(durable_job);
    let staging_root = app_root.join("peer-bidirectional").join("staging");
    let target = if recovered_after_staging {
        PersistentLogicalDeltaTarget::new_p5_deferred(
            store,
            cas,
            &context.credential.source_device_id,
            &context.library_id,
            &local_generation.generation_id,
            remote_manifest_bytes,
            &staging_root,
            conflict_policy.logical_delta_policy(),
        )
    } else {
        PersistentLogicalDeltaTarget::new_p5_deferred_with_durable_job(
            store,
            cas,
            &context.credential.source_device_id,
            &context.library_id,
            &local_generation.generation_id,
            remote_manifest_bytes,
            &staging_root,
            &job,
            conflict_policy.logical_delta_policy(),
        )
    };
    let mut target = target?;
    let plan = match target.reconstruct_authoritative_plan(context.expected_local_revision)? {
        LogicalDeltaPlanResolution::Ready { plan, .. } => plan,
        LogicalDeltaPlanResolution::Conflict { .. } => {
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(context.expected_local_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
    };
    if (!plan.apply.is_empty()) != changed {
        return Err(PeerSyncError::Storage(
            "target-prepared plan changed during recovery".to_owned(),
        ));
    }
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let remote_sizes = remote_manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
    let remaining_totals = Cell::new(None);
    let activation = execute_logical_delta_pull_with_pre_activation(
        &plan,
        &local_hashes,
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
        |selection| {
            if recovered_after_staging {
                return Ok(());
            }
            let totals = predicted_transfer_totals(selection)?;
            if totals.0 > transferred_objects || totals.1 > transferred_bytes {
                return Err(PeerSyncError::Storage(
                    "target-prepared remaining transfer exceeds durable selection".to_owned(),
                ));
            }
            remaining_totals.set(Some(totals));
            Ok(())
        },
        cancellation,
    );
    let measured_totals = measured_source.totals();
    drop(target);
    drop(job);
    if activation.is_ok()
        && !recovered_after_staging
        && remaining_totals
            .get()
            .is_none_or(|remaining| measured_totals != remaining)
    {
        return Err(PeerSyncError::Storage(
            "target recovery totals differ from the remaining selection".to_owned(),
        ));
    }
    retain_target_prepared_activation(
        store,
        cas,
        app_root,
        &prepared,
        verified_local_backup.as_ref(),
        activation,
    )
}

fn promote_target_prepared_for_status(
    store: &mut PersistentStore,
    app_root: &Path,
) -> Result<bool, PeerSyncError> {
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let Some(prepared) = journal.load()? else {
        return Ok(false);
    };
    let (context, local_generation, conflict_policy, changed) = match &prepared {
        PeerBidirectionalDurableOperation::TargetPrepared {
            context,
            local_generation,
            conflict_policy,
            changed,
            ..
        } => (
            context.clone(),
            local_generation.clone(),
            *conflict_policy,
            *changed,
        ),
        _ => return Ok(false),
    };
    let committed_revision = if changed {
        context
            .expected_local_revision
            .checked_add(1)
            .ok_or_else(|| {
                PeerSyncError::Validation("bidirectional local revision overflow".to_owned())
            })?
    } else {
        context.expected_local_revision
    };
    if store.revision().map_err(store_error)? != committed_revision {
        return Ok(false);
    }
    let durable_job = match DurableCasJob::open(app_root, &context.durable_job_id) {
        Ok(job) => job,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if durable_job.kind() != CasJobKind::LogicalDeltaTarget
        || durable_job.is_released()
        || !durable_job.is_sealed()
    {
        return Ok(false);
    }
    let remote_manifest_hash = &context.expected_remote_generation.manifest_hash;
    if !durable_job
        .root_set()?
        .object_hashes
        .contains(remote_manifest_hash)
    {
        return Ok(false);
    }
    let cas = PayloadCas::new(app_root)?;
    let Some(remote_manifest_bytes) = cas.read_object(remote_manifest_hash)? else {
        return Ok(false);
    };
    let remote_manifest = decode_logical_manifest(&remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != context.library_id
        || remote_manifest.generation != context.expected_remote_generation.generation_id
        || remote_manifest.generation_sequence
            != context.expected_remote_generation.generation_sequence
        || i64::try_from(remote_manifest.source_revision).ok()
            != Some(context.expected_remote_revision)
        || hash_logical_manifest(&remote_manifest)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?
            != *remote_manifest_hash
    {
        return Ok(false);
    }
    let local = store
        .build_indexed_logical_manifest(&context.library_id, &local_generation.generation_id)
        .map_err(store_error)?;
    if local.manifest_hash != local_generation.manifest_hash
        || local.manifest.generation_sequence != local_generation.generation_sequence
        || i64::try_from(local.manifest.source_revision).ok()
            != Some(context.expected_local_revision)
    {
        return Ok(false);
    }
    let staging_root = app_root.join("peer-bidirectional").join("staging");
    let mut target = PersistentLogicalDeltaTarget::new_p5_deferred(
        store,
        &cas,
        &context.credential.source_device_id,
        &context.library_id,
        &local_generation.generation_id,
        &remote_manifest_bytes,
        &staging_root,
        conflict_policy.logical_delta_policy(),
    )?;
    let plan = match target.reconstruct_authoritative_plan(context.expected_local_revision)? {
        LogicalDeltaPlanResolution::Ready { plan, .. } => plan,
        LogicalDeltaPlanResolution::Conflict { .. } => return Ok(false),
    };
    if (!plan.apply.is_empty()) != changed {
        return Ok(false);
    }
    let mut stage = target.begin(&plan)?;
    if !target.can_activate_without_transfer(&stage) {
        target.abort(stage)?;
        return Ok(false);
    }
    target.prepare_activation(&mut stage)?;
    let activation = target.activate_database_and_base_if_current(
        &mut stage,
        plan.expected_local_revision,
        &plan.expected_base_manifest_hash,
        &plan.next_base_manifest_hash,
        &plan.next_base_generation_sequence,
    )?;
    if !matches!(activation, LogicalDeltaActivation::AlreadyActive { .. }) {
        target.abort(stage)?;
        return Ok(false);
    }
    target.abort(stage)?;
    drop(target);
    drop(durable_job);
    let verified_local_backup = verify_target_prepared_local_backup(
        app_root,
        &context,
        &local_generation,
        conflict_policy,
        match &prepared {
            PeerBidirectionalDurableOperation::TargetPrepared { backups, .. } => backups,
            _ => unreachable!(),
        },
    )?;
    retain_target_prepared_activation(
        store,
        &cas,
        app_root,
        &prepared,
        verified_local_backup.as_ref(),
        Ok(activation),
    )?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn resolve_bidirectional_conflict<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    resolve_bidirectional_conflict_with_cancellation(
        store,
        cas,
        app_root,
        operation_id,
        winner,
        expected_revision,
        remote_manifest_bytes,
        remote_source,
        &NeverCancelled,
    )
}

#[allow(clippy::too_many_arguments)]
fn resolve_bidirectional_conflict_with_cancellation<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
    cancellation: &dyn CancellationProbe,
) -> Result<LocalMergeOutcome, PeerSyncError> {
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let operation = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
    })?;
    let (
        context,
        conflicts,
        local_generation,
        local_manifest_hash,
        remote_manifest_hash,
        mut backups,
    ) = match operation {
        PeerBidirectionalDurableOperation::AwaitingConflict {
            context,
            conflicts,
            local_generation,
            local_manifest_hash,
            remote_manifest_hash,
            backups,
            ..
        } if context.operation_id == operation_id => (
            context,
            conflicts,
            local_generation,
            local_manifest_hash,
            remote_manifest_hash,
            backups,
        ),
        other if other.operation_id() != operation_id => {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        _ => {
            return Err(PeerSyncError::Validation(
                "bidirectional operation is not awaiting conflict resolution".to_owned(),
            ));
        }
    };
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision != expected_revision
        || (actual_revision != context.expected_local_revision
            && actual_revision != context.expected_local_revision.saturating_add(1))
    {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_revision.to_string()),
            actual: Some(actual_revision.to_string()),
        });
    }
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let received_remote_hash = hash_logical_manifest(&remote_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != context.library_id
        || remote_manifest.generation != context.expected_remote_generation.generation_id
        || remote_manifest.generation_sequence
            != context.expected_remote_generation.generation_sequence
        || received_remote_hash != context.expected_remote_generation.manifest_hash
    {
        return Err(PeerSyncError::StaleManifest {
            expected: context.expected_remote_generation.manifest_hash,
            received: received_remote_hash,
        });
    }
    let verified_local_backup = if winner == PeerBidirectionalConflictWinner::Remote {
        let verified = if let Some(receipt) = backups
            .iter()
            .find(|backup| backup.side == PeerBidirectionalBackupSide::Local)
        {
            let expected_path = bidirectional_backup_path(
                app_root,
                operation_id,
                PeerBidirectionalBackupSide::Local,
            );
            if Path::new(&receipt.path) != expected_path {
                return Err(PeerSyncError::Storage(
                    "bidirectional backup receipt has an unexpected path".to_owned(),
                ));
            }
            VerifiedTargetPreparedLocalBackup {
                receipt: verify_bidirectional_backup_receipt(
                    &expected_path,
                    operation_id,
                    context.expected_local_revision,
                    PeerBidirectionalBackupSide::Local,
                    &local_generation,
                    Some(&receipt.package_id),
                    cancellation,
                )?,
            }
        } else {
            let verified = ensure_bidirectional_backup_receipt(
                store,
                cas,
                app_root,
                operation_id,
                context.expected_local_revision,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
                None,
                cancellation,
            )?;
            backups.push(verified.receipt().clone());
            journal.store(&PeerBidirectionalDurableOperation::AwaitingConflict {
                schema: OPERATION_SCHEMA.to_owned(),
                context: context.clone(),
                conflicts: conflicts.clone(),
                local_generation: local_generation.clone(),
                local_manifest_hash: local_manifest_hash.clone(),
                remote_manifest_hash: remote_manifest_hash.clone(),
                backups: backups.clone(),
            })?;
            VerifiedTargetPreparedLocalBackup {
                receipt: verified.into_receipt(),
            }
        };
        Some(verified)
    } else {
        None
    };
    let job = RefCell::new(open_or_begin_target_job(app_root, &context.durable_job_id)?);
    let recovered_after_activation = job.borrow().is_sealed();
    let conflict_policy = match winner {
        PeerBidirectionalConflictWinner::Local => LogicalDeltaConflictPolicy::PreferLocal,
        PeerBidirectionalConflictWinner::Remote => LogicalDeltaConflictPolicy::PreferRemote,
    };
    let mut target = if recovered_after_activation {
        PersistentLogicalDeltaTarget::new_p5_deferred(
            store,
            cas,
            &context.credential.source_device_id,
            &context.library_id,
            &local_generation.generation_id,
            remote_manifest_bytes,
            &app_root.join("peer-bidirectional").join("staging"),
            conflict_policy,
        )?
    } else {
        PersistentLogicalDeltaTarget::new_p5_deferred_with_durable_job(
            store,
            cas,
            &context.credential.source_device_id,
            &context.library_id,
            &local_generation.generation_id,
            remote_manifest_bytes,
            &app_root.join("peer-bidirectional").join("staging"),
            &job,
            conflict_policy,
        )?
    };
    let plan = match target.resolve_authoritative_plan(context.expected_local_revision) {
        Ok(LogicalDeltaPlanResolution::Ready { plan, .. }) => plan,
        Ok(LogicalDeltaPlanResolution::Conflict { .. }) => {
            return Err(PeerSyncError::Protocol(
                "chosen bidirectional winner did not resolve its conflicts".to_owned(),
            ));
        }
        Err(error) => return Err(error),
    };
    let changed = !plan.apply.is_empty();
    let remote_sizes = remote_manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
    let prepared = RefCell::new(None);
    let prepared_changed = changed || recovered_after_activation;
    let activation = execute_logical_delta_pull_with_pre_activation(
        &plan,
        &BTreeSet::new(),
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
        |selection| {
            let (transferred_objects, transferred_bytes) = predicted_transfer_totals(selection)?;
            let operation = PeerBidirectionalDurableOperation::TargetPrepared {
                schema: OPERATION_SCHEMA.to_owned(),
                context: context.clone(),
                local_generation: local_generation.clone(),
                conflict_policy: match winner {
                    PeerBidirectionalConflictWinner::Local => {
                        TargetPreparedConflictPolicy::PreferLocal
                    }
                    PeerBidirectionalConflictWinner::Remote => {
                        TargetPreparedConflictPolicy::PreferRemote
                    }
                },
                changed: prepared_changed,
                remote_backup_required: winner == PeerBidirectionalConflictWinner::Local,
                transferred_objects,
                transferred_bytes,
                backups: backups.clone(),
            };
            #[cfg(test)]
            if TARGET_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated target-prepared journal store failure".to_owned(),
                ));
            }
            journal.store(&operation)?;
            prepared.replace(Some(operation));
            Ok(())
        },
        cancellation,
    );
    let measured_totals = measured_source.totals();
    drop(target);
    drop(job);
    let Some(prepared) = prepared.into_inner() else {
        return activation.and_then(|_| {
            Err(PeerSyncError::Storage(
                "target conflict activation lacks its durable preparation".to_owned(),
            ))
        });
    };
    if let PeerBidirectionalDurableOperation::TargetPrepared {
        transferred_objects,
        transferred_bytes,
        ..
    } = &prepared
    {
        if activation.is_ok()
            && !recovered_after_activation
            && measured_totals != (*transferred_objects, *transferred_bytes)
        {
            return Err(PeerSyncError::Storage(
                "target conflict transfer totals differ from durable selection".to_owned(),
            ));
        }
    }
    retain_target_prepared_activation(
        store,
        cas,
        app_root,
        &prepared,
        verified_local_backup.as_ref(),
        activation,
    )
}

#[allow(clippy::too_many_arguments)]
// Test-facing wrapper around the cancellation-aware entry point.
#[cfg_attr(not(test), allow(dead_code))]
fn apply_bidirectional_remote_shared<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    peer_id: &str,
    expected_revision: i64,
    expected_common_base_manifest_hash: &str,
    expected_losing_generation: &SyncGenerationIdentity,
    expected_shared_generation: LanBidirectionalGeneration,
    shared_manifest_bytes: &[u8],
    shared_source: &mut S,
    backup_losing_side: bool,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    apply_bidirectional_remote_shared_inner(
        store,
        cas,
        app_root,
        operation_id,
        peer_id,
        expected_revision,
        expected_common_base_manifest_hash,
        expected_losing_generation,
        expected_shared_generation,
        shared_manifest_bytes,
        shared_source,
        backup_losing_side,
        None,
        None,
        None,
        false,
        &NeverCancelled,
    )
    .map(VerifiedRemoteApplyReceipt::into_receipt)
}

#[allow(clippy::too_many_arguments)]
fn apply_bidirectional_remote_shared_inner<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    peer_id: &str,
    expected_revision: i64,
    expected_common_base_manifest_hash: &str,
    expected_losing_generation: &SyncGenerationIdentity,
    expected_shared_generation: LanBidirectionalGeneration,
    shared_manifest_bytes: &[u8],
    shared_source: &mut S,
    backup_losing_side: bool,
    source_device_id: Option<&str>,
    retained_source: Option<&SourcePreparedEvidence>,
    retained_source_job_id: Option<&str>,
    completion_deferred_v1: bool,
    cancellation: &dyn CancellationProbe,
) -> Result<VerifiedRemoteApplyReceipt, PeerSyncError> {
    if uuid::Uuid::parse_str(operation_id)
        .map(|value| value.to_string() != operation_id)
        .unwrap_or(true)
    {
        return Err(PeerSyncError::Validation(
            "bidirectional remote operation ID is invalid".to_owned(),
        ));
    }
    if expected_revision < 0 {
        return Err(PeerSyncError::Validation(
            "bidirectional remote revision must be nonnegative".to_owned(),
        ));
    }
    let shared_manifest = decode_logical_manifest(shared_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let shared_manifest_hash = hash_logical_manifest(&shared_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if shared_manifest.library_id != PRODUCT_LOGICAL_LIBRARY_ID
        || shared_manifest.generation != expected_shared_generation.generation_id
        || shared_manifest_hash != expected_shared_generation.manifest_hash
        || shared_manifest.generation_sequence != expected_shared_generation.generation_sequence
    {
        return Err(PeerSyncError::Validation(
            "bidirectional shared generation differs from its verified manifest".to_owned(),
        ));
    }
    let expected_shared = SyncGenerationIdentity {
        generation_id: expected_shared_generation.generation_id.clone(),
        manifest_hash: expected_shared_generation.manifest_hash.clone(),
        generation_sequence: expected_shared_generation.generation_sequence.clone(),
    };
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision != expected_revision {
        let recovered_revision = expected_revision.checked_add(1);
        if recovered_revision == Some(actual_revision) {
            let active = store
                .seal_or_initialize_active_logical_generation(cas)
                .map_err(store_error)?;
            let active_identity = SyncGenerationIdentity {
                generation_id: active.manifest.generation,
                manifest_hash: active.manifest_hash,
                generation_sequence: active.manifest.generation_sequence,
            };
            let common_base = store
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .map_err(store_error)?;
            let acknowledgement =
                match store.sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id) {
                    Ok(acknowledgement) => Some(acknowledgement),
                    Err(StoreError::Validation { .. }) => None,
                    Err(error) => return Err(store_error(error)),
                };
            if common_base.as_ref() == Some(&expected_shared)
                && acknowledgement.as_ref().is_some_and(|state| {
                    state.shared_identity == expected_shared
                        && state.local_identity == active_identity
                })
            {
                let verified_backup = if backup_losing_side {
                    let receipt = verify_bidirectional_backup_receipt(
                        &bidirectional_backup_path(
                            app_root,
                            operation_id,
                            PeerBidirectionalBackupSide::Remote,
                        ),
                        operation_id,
                        expected_revision,
                        PeerBidirectionalBackupSide::Remote,
                        expected_losing_generation,
                        None,
                        cancellation,
                    )?;
                    Some(VerifiedBidirectionalBackupReceipt {
                        receipt,
                        published: false,
                    })
                } else {
                    None
                };
                let backup = verified_backup.as_ref().map(|verified| {
                    let receipt = verified.receipt();
                    LanBidirectionalBackupReceipt {
                        package_id: receipt.package_id.clone(),
                        path: receipt.path.clone(),
                    }
                });
                return Ok(VerifiedRemoteApplyReceipt {
                    receipt: LanBidirectionalRemoteApplyReceipt {
                        committed_revision: actual_revision,
                        committed_generation: expected_shared_generation,
                        transferred_objects: 0,
                        transferred_bytes: 0,
                        backup,
                    },
                    _backup: verified_backup,
                });
            }
        }
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_revision.to_string()),
            actual: Some(actual_revision.to_string()),
        });
    }
    let previous = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .map_err(store_error)?;
    if previous.shared_identity.manifest_hash != expected_common_base_manifest_hash {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(expected_common_base_manifest_hash.to_owned()),
            actual: Some(previous.shared_identity.manifest_hash),
        });
    }
    let local = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let local_identity = SyncGenerationIdentity {
        generation_id: local.manifest.generation.clone(),
        manifest_hash: local.manifest_hash.clone(),
        generation_sequence: local.manifest.generation_sequence.clone(),
    };
    if local_identity != *expected_losing_generation {
        return Err(PeerSyncError::StaleManifest {
            expected: expected_losing_generation.manifest_hash.clone(),
            received: local_identity.manifest_hash,
        });
    }
    let staging_root = app_root.join("peer-bidirectional").join("staging");
    let planning_target = PersistentLogicalDeltaTarget::new_p5_remote_shared_ack(
        store,
        cas,
        peer_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &local.manifest.generation,
        shared_manifest_bytes,
        &staging_root,
        LogicalDeltaConflictPolicy::PreferRemote,
    )?;
    let plan = match planning_target.resolve_authoritative_plan(expected_revision) {
        Ok(LogicalDeltaPlanResolution::Ready { plan, .. }) => plan,
        Ok(LogicalDeltaPlanResolution::Conflict { .. }) => {
            return Err(PeerSyncError::Protocol(
                "bidirectional remote winner did not resolve its conflicts".to_owned(),
            ));
        }
        Err(error) => return Err(error),
    };
    drop(planning_target);
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let shared_sizes = shared_manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let selection = select_missing_logical_delta_objects(&plan, &local_hashes, cas, &shared_sizes)?;
    let (selected_objects, selected_bytes) = predicted_transfer_totals(&selection)?;
    let durable_job_id = retained_source_job_id.unwrap_or(operation_id).to_owned();
    let mut prepared_evidence = if let Some(source_device_id) = source_device_id {
        let incoming_revision = i64::try_from(shared_manifest.source_revision).map_err(|_| {
            PeerSyncError::Validation(
                "bidirectional shared revision exceeds SQLite range".to_owned(),
            )
        })?;
        let evidence = retained_source.cloned().unwrap_or(SourcePreparedEvidence {
            operation_id: operation_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: peer_id.to_owned(),
            expected_source_revision: expected_revision,
            previous_shared: previous.shared_identity.clone(),
            expected_source_generation: expected_losing_generation.clone(),
            shared_generation: expected_shared_generation.clone(),
            incoming_revision,
            transferred_objects: selected_objects,
            transferred_bytes: selected_bytes,
            backup_required: backup_losing_side,
            backup: None,
            completion_deferred_v1,
        });
        if evidence.operation_id != operation_id
            || evidence.source_device_id != source_device_id
            || evidence.target_device_id != peer_id
            || evidence.expected_source_revision != expected_revision
            || evidence.previous_shared != previous.shared_identity
            || evidence.expected_source_generation != *expected_losing_generation
            || evidence.shared_generation != expected_shared_generation
            || evidence.incoming_revision != incoming_revision
            || evidence.requires_backup() != backup_losing_side
            || evidence.completion_deferred_v1 != completion_deferred_v1
            || selected_objects > evidence.transferred_objects
            || selected_bytes > evidence.transferred_bytes
        {
            return Err(PeerSyncError::Validation(
                "bidirectional source retry differs from retained evidence".to_owned(),
            ));
        }
        if retained_source.is_none() {
            #[cfg(test)]
            if SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated source-prepared journal failure".to_owned(),
                ));
            }
            PeerBidirectionalOperationJournal::new(app_root)
                .store(&evidence.durable_operation(durable_job_id.clone()))?;
            #[cfg(test)]
            if SOURCE_AFTER_INITIAL_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false))
            {
                return Err(PeerSyncError::Storage(
                    "simulated process loss after initial source-prepared journal store".to_owned(),
                ));
            }
        }
        Some(evidence)
    } else {
        None
    };
    let verified_backup = if backup_losing_side {
        let bound_package_id = prepared_evidence
            .as_ref()
            .and_then(|evidence| evidence.backup.as_ref())
            .map(|receipt| receipt.package_id.as_str());
        let verified = ensure_bidirectional_backup_receipt(
            store,
            cas,
            app_root,
            operation_id,
            expected_revision,
            PeerBidirectionalBackupSide::Remote,
            expected_losing_generation,
            bound_package_id,
            cancellation,
        )?;
        debug_assert!(bound_package_id.is_none() || !verified.published);
        #[cfg(test)]
        if SOURCE_AFTER_BACKUP_BEFORE_RECEIPT_STORE_FAILPOINT.with(|enabled| enabled.replace(false))
        {
            return Err(PeerSyncError::Storage(
                "simulated process loss after source backup publication".to_owned(),
            ));
        }
        if let Some(evidence) = prepared_evidence.as_mut() {
            let receipt = verified.receipt();
            let backup = LanBidirectionalBackupReceipt {
                package_id: receipt.package_id.clone(),
                path: receipt.path.clone(),
            };
            if evidence.backup.as_ref() != Some(&backup) {
                evidence.backup = Some(backup);
                PeerBidirectionalOperationJournal::new(app_root)
                    .store(&evidence.durable_operation(durable_job_id.clone()))?;
            }
        }
        Some(verified)
    } else {
        None
    };
    let backup = verified_backup.as_ref().map(|verified| {
        let receipt = verified.receipt();
        LanBidirectionalBackupReceipt {
            package_id: receipt.package_id.clone(),
            path: receipt.path.clone(),
        }
    });
    #[cfg(test)]
    if prepared_evidence.is_some()
        && SOURCE_AFTER_PREPARED_STORE_PANIC.with(|enabled| enabled.replace(false))
    {
        panic!("simulated process loss after source-prepared receipt store");
    }
    let newly_published_backup = verified_backup.as_ref().and_then(|verified| {
        verified.published.then(|| {
            bidirectional_backup_path(app_root, operation_id, PeerBidirectionalBackupSide::Remote)
        })
    });
    let cancel_after_backup = cancellation.is_cancelled();
    #[cfg(test)]
    let cancel_after_backup = cancel_after_backup
        || SOURCE_AFTER_BACKUP_PUBLISH_CANCEL_FAILPOINT.with(|enabled| enabled.replace(false));
    if cancel_after_backup {
        if prepared_evidence.is_none() {
            remove_newly_published_backup_after_cancellation(newly_published_backup.as_deref())?;
        }
        return Err(PeerSyncError::Cancelled);
    }
    let (stage_with_durable_job, durable_job) =
        if let Some(retained_job_id) = retained_source_job_id {
            match DurableCasJob::open(app_root, retained_job_id) {
                Ok(mut job) if job.is_released() => {
                    job.release(CasReleaseOutcome::Aborted)?;
                    drop(job);
                    (
                        true,
                        DurableCasJob::begin(
                            app_root,
                            retained_job_id,
                            CasJobKind::LogicalDeltaTarget,
                            now_millis()?,
                        )?,
                    )
                }
                Ok(job) => (!job.is_sealed(), job),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    true,
                    DurableCasJob::begin(
                        app_root,
                        retained_job_id,
                        CasJobKind::LogicalDeltaTarget,
                        now_millis()?,
                    )?,
                ),
                Err(error) => return Err(error.into()),
            }
        } else {
            (
                true,
                DurableCasJob::begin(
                    app_root,
                    &durable_job_id,
                    CasJobKind::LogicalDeltaTarget,
                    now_millis()?,
                )?,
            )
        };
    if durable_job.kind() != CasJobKind::LogicalDeltaTarget {
        return Err(PeerSyncError::Storage(
            "retained source job has an unexpected kind".to_owned(),
        ));
    }
    if durable_job.is_sealed()
        && !durable_job
            .root_set()?
            .object_hashes
            .contains(&shared_manifest_hash)
    {
        return Err(PeerSyncError::Storage(
            "retained source job does not own the shared manifest".to_owned(),
        ));
    }
    let job = RefCell::new(durable_job);
    let target = if stage_with_durable_job {
        PersistentLogicalDeltaTarget::new_p5_remote_shared_ack_with_durable_job(
            store,
            cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &local.manifest.generation,
            shared_manifest_bytes,
            &staging_root,
            &job,
            LogicalDeltaConflictPolicy::PreferRemote,
        )
    } else {
        PersistentLogicalDeltaTarget::new_p5_remote_shared_ack(
            store,
            cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &local.manifest.generation,
            shared_manifest_bytes,
            &staging_root,
            LogicalDeltaConflictPolicy::PreferRemote,
        )
    };
    let mut target = match target {
        Ok(target) => target,
        Err(error) => {
            if retained_source.is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            return Err(error);
        }
    };
    let mut measured_source = MeasuredLogicalDeltaSource::new(shared_source);
    let activation = execute_logical_delta_pull_with_pre_activation(
        &plan,
        &local_hashes,
        cas,
        &shared_sizes,
        &mut measured_source,
        &mut target,
        |selection| {
            if let Some(evidence) = prepared_evidence.as_ref() {
                let (remaining_objects, remaining_bytes) = predicted_transfer_totals(selection)?;
                if remaining_objects > evidence.transferred_objects
                    || remaining_bytes > evidence.transferred_bytes
                {
                    return Err(PeerSyncError::Storage(
                        "source transfer exceeds its durable selection".to_owned(),
                    ));
                }
            }
            #[cfg(test)]
            if SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated source pre-activation failure".to_owned(),
                ));
            }
            Ok(())
        },
        cancellation,
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    drop(target);
    let committed_revision = match activation {
        Ok(LogicalDeltaActivation::Activated { revision })
        | Ok(LogicalDeltaActivation::AlreadyActive { revision }) => revision,
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision, ..
        }) => {
            if prepared_evidence.is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        Err(error) => {
            if prepared_evidence.is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
                if error == PeerSyncError::Cancelled {
                    remove_newly_published_backup_after_cancellation(
                        newly_published_backup.as_deref(),
                    )?;
                }
            }
            return Err(error);
        }
    };
    let acknowledged = store
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
        .map_err(store_error)?;
    if acknowledged.shared_identity != expected_shared {
        return Err(PeerSyncError::Storage(
            "bidirectional remote acknowledgement differs from shared generation".to_owned(),
        ));
    }
    job.borrow_mut().release(CasReleaseOutcome::Committed)?;
    let measured_receipt = LanBidirectionalRemoteApplyReceipt {
        committed_revision,
        committed_generation: expected_shared_generation,
        transferred_objects,
        transferred_bytes,
        backup,
    };
    let Some(evidence) = prepared_evidence else {
        return Ok(VerifiedRemoteApplyReceipt {
            receipt: measured_receipt,
            _backup: verified_backup,
        });
    };
    let receipt = evidence.receipt_at(measured_receipt.committed_revision)?;
    if retained_source.is_none()
        && (receipt.transferred_objects != measured_receipt.transferred_objects
            || receipt.transferred_bytes != measured_receipt.transferred_bytes)
    {
        return Err(PeerSyncError::Storage(
            "bidirectional source activation differs from prepared evidence".to_owned(),
        ));
    }
    Ok(VerifiedRemoteApplyReceipt {
        receipt,
        _backup: verified_backup,
    })
}

fn complete_bidirectional_local_after_remote_apply(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    remote: LanBidirectionalRemoteApplyReceipt,
) -> Result<PeerBidirectionalCompletedResult, PeerSyncError> {
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let operation = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
    })?;
    let (
        mut context,
        committed_revision,
        shared_generation,
        changed,
        remote_backup_required,
        retained_remote_apply_receipt,
        local_transferred_objects,
        local_transferred_bytes,
        mut backups,
    ) = match operation {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            committed_revision,
            shared_generation,
            changed,
            remote_backup_required,
            remote_apply_receipt,
            transferred_objects,
            transferred_bytes,
            backups,
            ..
        } if context.operation_id == operation_id => (
            context,
            committed_revision,
            shared_generation,
            changed,
            remote_backup_required,
            remote_apply_receipt,
            transferred_objects,
            transferred_bytes,
            backups,
        ),
        other if other.operation_id() != operation_id => {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        _ => {
            return Err(PeerSyncError::Validation(
                "bidirectional operation is not ready for remote completion".to_owned(),
            ));
        }
    };
    let remote_shared = SyncGenerationIdentity {
        generation_id: remote.committed_generation.generation_id.clone(),
        manifest_hash: remote.committed_generation.manifest_hash.clone(),
        generation_sequence: remote.committed_generation.generation_sequence.clone(),
    };
    if remote_shared != shared_generation {
        return Err(PeerSyncError::Validation(
            "bidirectional remote receipt differs from shared generation".to_owned(),
        ));
    }
    if remote_backup_required && remote.backup.is_none() {
        return Err(PeerSyncError::Validation(
            "bidirectional remote overwrite is missing its required backup".to_owned(),
        ));
    }
    let remote_changed = if remote.committed_revision == context.expected_remote_revision {
        false
    } else if context
        .expected_remote_revision
        .checked_add(1)
        .is_some_and(|revision| remote.committed_revision == revision)
    {
        true
    } else {
        return Err(PeerSyncError::Validation(
            "bidirectional remote receipt has an unexpected revision".to_owned(),
        ));
    };
    match retained_remote_apply_receipt {
        Some(retained) if retained != remote => {
            return Err(PeerSyncError::Validation(
                "bidirectional remote receipt differs from retained completion".to_owned(),
            ));
        }
        Some(_) => {}
        None => journal.store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context.clone(),
            committed_revision,
            shared_generation: shared_generation.clone(),
            changed,
            remote_backup_required,
            remote_apply_receipt: Some(remote.clone()),
            transferred_objects: local_transferred_objects,
            transferred_bytes: local_transferred_bytes,
            backups: backups.clone(),
        })?,
    }
    let active = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let active_identity = SyncGenerationIdentity {
        generation_id: active.manifest.generation.clone(),
        manifest_hash: active.manifest_hash.clone(),
        generation_sequence: active.manifest.generation_sequence.clone(),
    };
    if !store
        .logical_generation_descends_from(
            &context.library_id,
            &active_identity.generation_id,
            &shared_generation.generation_id,
        )
        .map_err(store_error)?
    {
        return Err(PeerSyncError::ActivationConflict {
            expected: Some(shared_generation.manifest_hash),
            actual: Some(active_identity.manifest_hash),
        });
    }
    #[cfg(test)]
    if COMPLETE_BEFORE_ACK_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated crash before bidirectional acknowledgement".to_owned(),
        ));
    }
    let acknowledgement = store
        .sync_device_ack_state(&context.library_id, &context.credential.source_device_id)
        .map_err(store_error)?;
    if acknowledgement.shared_identity != shared_generation {
        let shared = store
            .build_indexed_logical_manifest(&context.library_id, &shared_generation.generation_id)
            .map_err(store_error)?;
        if shared.manifest_hash != shared_generation.manifest_hash
            || shared.manifest.generation_sequence != shared_generation.generation_sequence
        {
            return Err(PeerSyncError::Storage(
                "retained shared generation differs from its manifest".to_owned(),
            ));
        }
        let active_revision = store.revision().map_err(store_error)?;
        store
            .advance_active_sync_device_historical_shared_ack(
                &context.library_id,
                &context.credential.source_device_id,
                active_revision,
                &context.previous_shared,
                &context.previous_local,
                &shared_generation,
                &shared.manifest,
                &active_identity,
            )
            .map_err(store_error)?;
    }
    #[cfg(test)]
    if COMPLETE_AFTER_ACK_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated crash after bidirectional acknowledgement".to_owned(),
        ));
    }
    let completed_revision = store.revision().map_err(store_error)?;
    let retained_backups = backups.clone();
    if let Some(backup) = remote.backup.clone() {
        backups.push(PeerBidirectionalBackupReceipt {
            package_id: backup.package_id,
            side: PeerBidirectionalBackupSide::Remote,
            path: backup.path,
        });
    }
    let transferred_objects = local_transferred_objects
        .checked_add(remote.transferred_objects)
        .ok_or_else(|| {
            PeerSyncError::Validation("bidirectional transfer object count overflow".to_owned())
        })?;
    let transferred_bytes = local_transferred_bytes
        .checked_add(remote.transferred_bytes)
        .ok_or_else(|| {
            PeerSyncError::Validation("bidirectional transfer byte count overflow".to_owned())
        })?;
    let result = PeerBidirectionalCompletedResult {
        kind: if changed || remote_changed {
            "updated".to_owned()
        } else {
            "noChanges".to_owned()
        },
        operation_id: operation_id.to_owned(),
        revision: completed_revision,
        remote_revision: remote.committed_revision,
        transferred_objects,
        transferred_bytes,
        backups: backups.clone(),
    };
    match context.completion_mode {
        PeerBidirectionalCompletionMode::V1 => complete_v1_bidirectional_accounting(
            app_root,
            &journal,
            &mut context,
            committed_revision,
            &shared_generation,
            changed,
            remote_backup_required,
            &remote,
            local_transferred_objects,
            local_transferred_bytes,
            &retained_backups,
        )?,
        PeerBidirectionalCompletionMode::Unsupported => {
            record_bidirectional_completion(app_root, &context, &result)?
        }
    }
    release_retained_target_job(
        app_root,
        &context.durable_job_id,
        CasReleaseOutcome::Committed,
    )?;
    journal.store(&PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: result.clone(),
    })?;
    Ok(result)
}

fn record_bidirectional_completion(
    app_root: &Path,
    context: &PeerBidirectionalOperationContext,
    result: &PeerBidirectionalCompletedResult,
) -> Result<(), PeerSyncError> {
    let source = registered_bidirectional_completion_source(app_root, context)?;
    let receipt_id = super::device_registry::completion_receipt_id(
        "bidirectional",
        &result.operation_id,
        &context.credential.manifest_id,
    );
    super::device_registry::record_incoming_completed_operation_once_for_lane(
        app_root,
        &source.device_id,
        super::device_registry::CompletionLane::Bidirectional,
        &receipt_id,
        result.transferred_bytes,
    )
}

fn store_initial_registered_target_operation(
    app_root: &Path,
    journal: &PeerBidirectionalOperationJournal,
    operation: &PeerBidirectionalDurableOperation,
) -> Result<(), PeerSyncError> {
    let context = match operation {
        PeerBidirectionalDurableOperation::TargetPrepared { context, .. }
        | PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } => context,
        _ => {
            return Err(PeerSyncError::Validation(
                "initial bidirectional target operation has an invalid phase".to_owned(),
            ));
        }
    };
    let _source_lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
    registered_bidirectional_completion_source(app_root, context)?;
    journal.store(operation)
}

fn registered_bidirectional_completion_source(
    app_root: &Path,
    context: &PeerBidirectionalOperationContext,
) -> Result<super::device_registry::IncomingSource, PeerSyncError> {
    let Some(source) = super::device_registry::incoming_source_by_id(
        app_root,
        &context.credential.source_device_id,
    )?
    else {
        return Err(PeerSyncError::Validation(
            "registered bidirectional source is missing".to_owned(),
        ));
    };
    if (!context.credential.bearer.is_empty() && source.bearer != context.credential.bearer)
        || !source.permissions.allows_bidirectional()
    {
        return Err(PeerSyncError::Validation(
            "registered bidirectional source credential or permission changed".to_owned(),
        ));
    }
    Ok(source)
}

#[allow(clippy::too_many_arguments)]
fn complete_v1_bidirectional_accounting(
    app_root: &Path,
    journal: &PeerBidirectionalOperationJournal,
    context: &mut PeerBidirectionalOperationContext,
    committed_revision: i64,
    shared_generation: &SyncGenerationIdentity,
    changed: bool,
    remote_backup_required: bool,
    remote: &LanBidirectionalRemoteApplyReceipt,
    local_transferred_objects: u64,
    local_transferred_bytes: u64,
    backups: &[PeerBidirectionalBackupReceipt],
) -> Result<(), PeerSyncError> {
    let source = registered_bidirectional_completion_source(app_root, context)?;
    let delivery = if let Some(delivery) = context.completion_delivery.clone() {
        delivery
    } else {
        let hello = super::lan::authenticated_peer_hello_with_capabilities(
            &source.endpoint,
            &source.bearer,
        )?;
        if hello.completion != super::lan::PeerCompletionCapability::V1
            || hello.hello.device_id != source.device_id
            || !hello.hello.permissions.allows_bidirectional()
        {
            return Err(PeerSyncError::Validation(
                "registered bidirectional source no longer advertises completion V1".to_owned(),
            ));
        }
        let useful_bytes = prepare_peer_logical_completion(
            &source.endpoint,
            &source.bearer,
            super::device_registry::CompletionLane::Bidirectional,
            &context.operation_id,
            &context.credential.manifest_id,
        )?;
        let delivery = super::device_registry::PendingCompletionDelivery {
            source_device_id: source.device_id.clone(),
            lane: super::device_registry::CompletionLane::Bidirectional
                .as_str()
                .to_owned(),
            completion_lease_id: context.operation_id.clone(),
            manifest_id: context.credential.manifest_id.clone(),
            useful_bytes,
            receipt_id: super::device_registry::completion_receipt_id(
                "bidirectional",
                &context.operation_id,
                &context.credential.manifest_id,
            ),
        };
        context.completion_delivery = Some(delivery.clone());
        journal.store(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context.clone(),
            committed_revision,
            shared_generation: shared_generation.clone(),
            changed,
            remote_backup_required,
            remote_apply_receipt: Some(remote.clone()),
            transferred_objects: local_transferred_objects,
            transferred_bytes: local_transferred_bytes,
            backups: backups.to_vec(),
        })?;
        delivery
    };
    let prepare =
        super::device_registry::prepare_incoming_completion_delivery(app_root, delivery.clone())?;
    if prepare == super::device_registry::CompletionDeliveryPrepareStatus::Pending {
        let snapshot = super::device_registry::snapshot_incoming_completion_delivery(
            app_root,
            &source.device_id,
            super::device_registry::CompletionLane::Bidirectional,
        )?
        .ok_or_else(|| {
            PeerSyncError::Storage("bidirectional completion outbox disappeared".to_owned())
        })?;
        if snapshot.delivery != delivery
            || (!context.credential.bearer.is_empty()
                && snapshot.source.bearer != context.credential.bearer)
            || !snapshot.source.permissions.allows_bidirectional()
        {
            return Err(PeerSyncError::Validation(
                "bidirectional completion outbox source changed".to_owned(),
            ));
        }
        let hello = super::lan::authenticated_peer_hello_with_capabilities(
            &snapshot.source.endpoint,
            &snapshot.source.bearer,
        )?;
        if hello.completion != super::lan::PeerCompletionCapability::V1
            || hello.hello.device_id != snapshot.source.device_id
            || !hello.hello.permissions.allows_bidirectional()
        {
            return Err(PeerSyncError::Validation(
                "registered bidirectional source no longer advertises completion V1".to_owned(),
            ));
        }
        deliver_peer_completion(
            &snapshot.source.endpoint,
            &snapshot.source.bearer,
            hello.completion,
            super::device_registry::CompletionLane::Bidirectional,
            &delivery.completion_lease_id,
            &delivery.manifest_id,
            delivery.useful_bytes,
        )?;
        super::device_registry::finalize_incoming_completion_delivery(app_root, &delivery)?;
    }
    if !super::device_registry::incoming_completion_is_durable(app_root, &delivery)? {
        return Err(PeerSyncError::Storage(
            "bidirectional completion receipt is not durable".to_owned(),
        ));
    }
    Ok(())
}

fn retain_remote_apply_receipt(
    app_root: &Path,
    operation_id: &str,
    receipt: &LanBidirectionalRemoteApplyReceipt,
) -> Result<(), PeerSyncError> {
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let operation = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
    })?;
    let PeerBidirectionalDurableOperation::LocalCommitted {
        schema,
        context,
        committed_revision,
        shared_generation,
        changed,
        remote_backup_required,
        remote_apply_receipt,
        transferred_objects,
        transferred_bytes,
        backups,
    } = operation
    else {
        return Err(PeerSyncError::Validation(
            "bidirectional operation is not ready for remote completion".to_owned(),
        ));
    };
    if context.operation_id != operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    if let Some(retained) = remote_apply_receipt {
        return if retained == *receipt {
            Ok(())
        } else {
            Err(PeerSyncError::Validation(
                "bidirectional remote receipt differs from retained completion".to_owned(),
            ))
        };
    }
    #[cfg(test)]
    if REMOTE_RECEIPT_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated remote receipt journal failure".to_owned(),
        ));
    }
    journal.store(&PeerBidirectionalDurableOperation::LocalCommitted {
        schema,
        context,
        committed_revision,
        shared_generation,
        changed,
        remote_backup_required,
        remote_apply_receipt: Some(receipt.clone()),
        transferred_objects,
        transferred_bytes,
        backups,
    })
}

fn resume_bidirectional_local_committed_with_remote<F>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    request_remote: F,
) -> Result<ResumeLocalCommittedOutcome, PeerSyncError>
where
    F: FnOnce(
        &PeerBidirectionalOperationContext,
        i64,
        &SyncGenerationIdentity,
        &[u8],
        bool,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError>,
{
    let journal = PeerBidirectionalOperationJournal::new(app_root);
    let operation = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
    })?;
    let (
        context,
        committed_revision,
        shared_generation,
        remote_backup_required,
        remote_apply_receipt,
    ) = match operation {
        PeerBidirectionalDurableOperation::LocalCommitted {
            context,
            committed_revision,
            shared_generation,
            remote_backup_required,
            remote_apply_receipt,
            ..
        } if context.operation_id == operation_id => (
            context,
            committed_revision,
            shared_generation,
            remote_backup_required,
            remote_apply_receipt,
        ),
        other if other.operation_id() != operation_id => {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        _ => {
            return Err(PeerSyncError::Validation(
                "bidirectional operation is not ready to resume".to_owned(),
            ));
        }
    };
    if let Some(remote) = remote_apply_receipt {
        return complete_bidirectional_local_after_remote_apply(
            store,
            cas,
            app_root,
            operation_id,
            remote,
        )
        .map(ResumeLocalCommittedOutcome::Completed);
    }
    let shared = store
        .build_indexed_logical_manifest(&context.library_id, &shared_generation.generation_id)
        .map_err(store_error)?;
    if shared.manifest_hash != shared_generation.manifest_hash
        || shared.manifest.generation_sequence != shared_generation.generation_sequence
    {
        return Err(PeerSyncError::Storage(
            "retained shared generation differs from its manifest".to_owned(),
        ));
    }
    let remote = match request_remote(
        &context,
        committed_revision,
        &shared_generation,
        &shared.manifest_bytes,
        remote_backup_required,
    ) {
        Ok(remote) => remote,
        Err(PeerSyncError::Transport(_)) => {
            return Ok(ResumeLocalCommittedOutcome::SourceUnavailable {
                operation_id: operation_id.to_owned(),
                committed_revision,
            });
        }
        Err(
            error
            @ (PeerSyncError::ActivationConflict { .. } | PeerSyncError::StaleManifest { .. }),
        ) => {
            if context.completion_mode == PeerBidirectionalCompletionMode::V1 {
                return Err(error);
            }
            journal.abandon(operation_id)?;
            return Ok(ResumeLocalCommittedOutcome::Stale {
                operation_id: operation_id.to_owned(),
                reason: PeerBidirectionalStaleReason::RemoteGeneration,
            });
        }
        Err(error) => return Err(error),
    };
    complete_bidirectional_local_after_remote_apply(store, cas, app_root, operation_id, remote)
        .map(ResumeLocalCommittedOutcome::Completed)
}

enum SourceOperationReconcile {
    New,
    Retry(SourcePreparedEvidence, String),
    Completed(
        SourcePreparedEvidence,
        LanBidirectionalRemoteApplyReceipt,
        i64,
        String,
    ),
    /// The retained journal already holds the completed source binding this
    /// request asks for, so the receipt is replayed instead of re-applied.
    CompletedReplay(LanBidirectionalRemoteApplyReceipt),
}

fn release_retained_source_job(
    app_root: &Path,
    durable_job_id: &str,
    outcome: CasReleaseOutcome,
) -> Result<(), PeerSyncError> {
    match DurableCasJob::open(app_root, durable_job_id) {
        Ok(mut job) if !job.is_released() => Ok(job.release(outcome)?),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn verified_source_acknowledgement(
    store: &PersistentStore,
    target_device_id: &str,
    shared: &SyncGenerationIdentity,
) -> Result<(i64, SyncGenerationIdentity), PeerSyncError> {
    let common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
        .map_err(store_error)?;
    let acknowledgement = store
        .sync_device_ack_state_for_recovery(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
        .map_err(store_error)?;
    if common.as_ref() != Some(shared) || acknowledgement.shared_identity != *shared {
        return Err(PeerSyncError::Validation(
            "source completion does not match its shared acknowledgement".to_owned(),
        ));
    }
    let committed = store
        .build_indexed_logical_manifest(
            PRODUCT_LOGICAL_LIBRARY_ID,
            &acknowledgement.local_identity.generation_id,
        )
        .map_err(store_error)?;
    if committed.manifest_hash != acknowledgement.local_identity.manifest_hash
        || committed.manifest.generation_sequence
            != acknowledgement.local_identity.generation_sequence
    {
        return Err(PeerSyncError::Storage(
            "source acknowledgement witness differs from its indexed manifest".to_owned(),
        ));
    }
    let committed_revision = i64::try_from(committed.manifest.source_revision).map_err(|_| {
        PeerSyncError::Storage("source acknowledgement revision exceeds SQLite range".to_owned())
    })?;
    Ok((committed_revision, acknowledgement.local_identity))
}

fn completed_source_request_matches(
    binding: &SourcePreparedEvidence,
    session: &LanBidirectionalSession,
    request: &LanBidirectionalRemoteApplyRequest,
) -> bool {
    binding.source_device_id == session.source_device_id
        && binding.target_device_id == session.target_device_id
        && binding.operation_id == request.operation_id
        && binding.expected_source_revision == request.expected_source_revision
        && binding.expected_source_generation
            == (SyncGenerationIdentity {
                generation_id: request.expected_source_generation.generation_id.clone(),
                manifest_hash: request.expected_source_generation.manifest_hash.clone(),
                generation_sequence: request
                    .expected_source_generation
                    .generation_sequence
                    .clone(),
            })
        && binding.previous_shared.manifest_hash == request.expected_common_base_manifest_hash
        && request.backup_losing_side == binding.requires_backup()
        && binding.completion_deferred_v1 == request.completion_deferred_v1.unwrap_or(false)
}

fn deferred_source_completion_allows_cleanup(
    app_root: &Path,
    evidence: &SourcePreparedEvidence,
) -> Result<bool, PeerSyncError> {
    if !evidence.completion_deferred_v1
        || !super::device_registry::outgoing_device_is_registered(
            app_root,
            &evidence.target_device_id,
        )?
    {
        return Ok(true);
    }
    let lane = super::device_registry::CompletionLane::Bidirectional;
    let manifest_id = &evidence.expected_source_generation.manifest_hash;
    Ok(super::device_registry::outgoing_completion_receipt_bytes(
        app_root,
        &evidence.target_device_id,
        lane,
        &evidence.operation_id,
        manifest_id,
    )?
    .is_some())
}

fn require_deferred_source_completion_cleanup(
    app_root: &Path,
    evidence: &SourcePreparedEvidence,
) -> Result<(), PeerSyncError> {
    if deferred_source_completion_allows_cleanup(app_root, evidence)? {
        Ok(())
    } else {
        Err(PeerSyncError::Protocol(
            "bidirectional V1 source completion must finish or be explicitly revoked".to_owned(),
        ))
    }
}

fn abandon_target_completion_delivery(
    app_root: &Path,
    context: &PeerBidirectionalOperationContext,
) -> Result<Option<super::device_registry::CompletionDeliveryAbandonStatus>, PeerSyncError> {
    if let Some(delivery) = &context.completion_delivery {
        return super::device_registry::abandon_incoming_completion_delivery(app_root, delivery)
            .map(Some);
    }
    Ok(None)
}

enum SourcePreparedState {
    Precommit,
    PrecommitDescendant {
        active_manifest_hash: String,
    },
    Postcommit {
        receipt: LanBidirectionalRemoteApplyReceipt,
        completed_revision: i64,
    },
    Mixed {
        actual_revision: i64,
    },
}

fn classify_source_prepared_state(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    evidence: &SourcePreparedEvidence,
) -> Result<SourcePreparedState, PeerSyncError> {
    let actual_revision = store.revision().map_err(store_error)?;
    let active = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let active_identity = SyncGenerationIdentity {
        generation_id: active.manifest.generation,
        manifest_hash: active.manifest_hash,
        generation_sequence: active.manifest.generation_sequence,
    };
    let common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, &evidence.target_device_id)
        .map_err(store_error)?;
    let acknowledgement = store
        .sync_device_ack_state_for_recovery(PRODUCT_LOGICAL_LIBRARY_ID, &evidence.target_device_id)
        .map_err(store_error)?;
    let remains_at_previous = common.as_ref() == Some(&evidence.previous_shared)
        && acknowledgement.shared_identity == evidence.previous_shared
        && acknowledgement.local_identity == evidence.expected_source_generation;
    if remains_at_previous {
        if actual_revision == evidence.expected_source_revision
            && active_identity == evidence.expected_source_generation
        {
            return Ok(SourcePreparedState::Precommit);
        }
        let active_descends_from_expected = actual_revision > evidence.expected_source_revision
            && store
                .logical_generation_descends_from(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    &active_identity.generation_id,
                    &evidence.expected_source_generation.generation_id,
                )
                .map_err(store_error)?;
        if active_descends_from_expected {
            return Ok(SourcePreparedState::PrecommitDescendant {
                active_manifest_hash: active_identity.manifest_hash,
            });
        }
        return Ok(SourcePreparedState::Mixed { actual_revision });
    }

    let shared = SyncGenerationIdentity {
        generation_id: evidence.shared_generation.generation_id.clone(),
        manifest_hash: evidence.shared_generation.manifest_hash.clone(),
        generation_sequence: evidence.shared_generation.generation_sequence.clone(),
    };
    if common.as_ref() != Some(&shared) || acknowledgement.shared_identity != shared {
        return Ok(SourcePreparedState::Mixed { actual_revision });
    }
    let (committed_revision, witnessed_local) =
        verified_source_acknowledgement(store, &evidence.target_device_id, &shared)?;
    let active_descends_from_ack = active_identity == witnessed_local
        || store
            .logical_generation_descends_from(
                PRODUCT_LOGICAL_LIBRARY_ID,
                &active_identity.generation_id,
                &witnessed_local.generation_id,
            )
            .map_err(store_error)?;
    let receipt = evidence.receipt_at(committed_revision)?;
    if active_descends_from_ack && actual_revision >= receipt.committed_revision {
        return Ok(SourcePreparedState::Postcommit {
            receipt,
            completed_revision: actual_revision,
        });
    }
    Ok(SourcePreparedState::Mixed { actual_revision })
}

fn verify_source_remote_backup(
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    expected_source: &SyncGenerationIdentity,
    backup: Option<&LanBidirectionalBackupReceipt>,
) -> Result<(), PeerSyncError> {
    let Some(backup) = backup else {
        return Ok(());
    };
    let expected_path =
        bidirectional_backup_path(app_root, operation_id, PeerBidirectionalBackupSide::Remote);
    if Path::new(&backup.path) != expected_path {
        return Err(PeerSyncError::Storage(
            "bidirectional backup receipt has an unexpected path".to_owned(),
        ));
    }
    verify_bidirectional_backup_receipt(
        &expected_path,
        operation_id,
        expected_revision,
        PeerBidirectionalBackupSide::Remote,
        expected_source,
        Some(&backup.package_id),
        &NeverCancelled,
    )?;
    Ok(())
}

fn verify_source_evidence_backup(
    app_root: &Path,
    evidence: &SourcePreparedEvidence,
) -> Result<(), PeerSyncError> {
    verify_source_remote_backup(
        app_root,
        &evidence.operation_id,
        evidence.expected_source_revision,
        &evidence.expected_source_generation,
        evidence.backup.as_ref(),
    )
}

fn reconcile_source_operation(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    session: &LanBidirectionalSession,
    request: &LanBidirectionalRemoteApplyRequest,
) -> Result<SourceOperationReconcile, PeerSyncError> {
    let Some(operation) = PeerBidirectionalOperationJournal::new(app_root).load()? else {
        return Ok(SourceOperationReconcile::New);
    };
    if operation.operation_id() != request.operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    match operation {
        PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: Some(binding),
            result,
            ..
        } => {
            if !completed_source_request_matches(&binding, session, request) {
                return Err(PeerSyncError::Validation(
                    "bidirectional completed source retry differs from its binding".to_owned(),
                ));
            }
            verify_source_evidence_backup(app_root, &binding)?;
            let shared = SyncGenerationIdentity {
                generation_id: receipt.committed_generation.generation_id.clone(),
                manifest_hash: receipt.committed_generation.manifest_hash.clone(),
                generation_sequence: receipt.committed_generation.generation_sequence.clone(),
            };
            let (witnessed_revision, witnessed_local) =
                verified_source_acknowledgement(store, &session.target_device_id, &shared)?;
            if binding.receipt_at(witnessed_revision)? != receipt {
                return Err(PeerSyncError::Validation(
                    "bidirectional completed source receipt lacks its acknowledgement witness"
                        .to_owned(),
                ));
            }
            let actual_revision = store.revision().map_err(store_error)?;
            let active = store
                .seal_or_initialize_active_logical_generation(cas)
                .map_err(store_error)?;
            let active_identity = SyncGenerationIdentity {
                generation_id: active.manifest.generation,
                manifest_hash: active.manifest_hash,
                generation_sequence: active.manifest.generation_sequence,
            };
            let active_descends_from_witness = active_identity == witnessed_local
                || store
                    .logical_generation_descends_from(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        &active_identity.generation_id,
                        &witnessed_local.generation_id,
                    )
                    .map_err(store_error)?;
            if !active_descends_from_witness || actual_revision < result.revision {
                return Err(PeerSyncError::Validation(
                    "bidirectional completed source retry differs from durable source state"
                        .to_owned(),
                ));
            }
            Ok(SourceOperationReconcile::CompletedReplay(receipt))
        }
        PeerBidirectionalDurableOperation::Completed { .. } => Err(PeerSyncError::Validation(
            "bidirectional target completion cannot be replayed by a source".to_owned(),
        )),
        operation @ PeerBidirectionalDurableOperation::SourcePrepared { .. } => {
            let evidence = SourcePreparedEvidence::from_operation(&operation).ok_or_else(|| {
                PeerSyncError::Storage("source-prepared operation is invalid".to_owned())
            })?;
            let durable_job_id = match &operation {
                PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                    durable_job_id.clone()
                }
                _ => unreachable!(),
            };
            if !completed_source_request_matches(&evidence, session, request) {
                return Err(PeerSyncError::Validation(
                    "bidirectional source retry differs from retained evidence".to_owned(),
                ));
            }
            match classify_source_prepared_state(store, cas, &evidence)? {
                SourcePreparedState::Precommit => {
                    return Ok(SourceOperationReconcile::Retry(evidence, durable_job_id));
                }
                SourcePreparedState::PrecommitDescendant {
                    active_manifest_hash,
                } => {
                    require_deferred_source_completion_cleanup(app_root, &evidence)?;
                    PeerBidirectionalOperationJournal::new(app_root)
                        .abandon(&evidence.operation_id)?;
                    return Err(PeerSyncError::StaleManifest {
                        expected: evidence.expected_source_generation.manifest_hash,
                        received: active_manifest_hash,
                    });
                }
                SourcePreparedState::Postcommit {
                    receipt,
                    completed_revision,
                } => {
                    return Ok(SourceOperationReconcile::Completed(
                        evidence,
                        receipt,
                        completed_revision,
                        durable_job_id,
                    ));
                }
                SourcePreparedState::Mixed { actual_revision } => {
                    return Err(PeerSyncError::ActivationConflict {
                        expected: Some(format!(
                            "source revision {} at previous base or revision {} at shared base",
                            evidence.expected_source_revision,
                            evidence.expected_source_revision.saturating_add(1)
                        )),
                        actual: Some(actual_revision.to_string()),
                    });
                }
            }
        }
        _ => Err(PeerSyncError::Validation(
            "bidirectional target operation is retained by the source".to_owned(),
        )),
    }
}

fn store_source_completed(
    app_root: &Path,
    evidence: SourcePreparedEvidence,
    receipt: &LanBidirectionalRemoteApplyReceipt,
    completed_revision: i64,
) -> Result<(), PeerSyncError> {
    let backups = receipt
        .backup
        .iter()
        .map(|backup| PeerBidirectionalBackupReceipt {
            package_id: backup.package_id.clone(),
            side: PeerBidirectionalBackupSide::Remote,
            path: backup.path.clone(),
        })
        .collect();
    let result = PeerBidirectionalCompletedResult {
        kind: if receipt.committed_revision != evidence.expected_source_revision
            || receipt.transferred_objects != 0
        {
            "updated".to_owned()
        } else {
            "noChanges".to_owned()
        },
        operation_id: evidence.operation_id.clone(),
        revision: completed_revision,
        remote_revision: evidence.incoming_revision,
        transferred_objects: receipt.transferred_objects,
        transferred_bytes: receipt.transferred_bytes,
        backups,
    };
    PeerBidirectionalOperationJournal::new(app_root).store(
        &PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: Some(receipt.clone()),
            source_binding: Some(evidence),
            result,
        },
    )
}

struct ProductionLanBidirectionalControl {
    app_root: PathBuf,
    store: Mutex<PersistentStore>,
    source_session_id: String,
    source_device_id: String,
}

impl ProductionLanBidirectionalControl {
    fn new(
        app_root: PathBuf,
        store: PersistentStore,
        source_session_id: &str,
        source_device_id: &str,
    ) -> Self {
        Self {
            app_root,
            store: Mutex::new(store),
            source_session_id: source_session_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
        }
    }

    fn register(
        &self,
        session: super::lan::LanBidirectionalSession,
        request: super::lan::LanBidirectionalRegistrationRequest,
    ) -> Result<(), PeerSyncError> {
        if session.session_id != self.source_session_id
            || session.source_device_id != self.source_device_id
            || request.library_id != PRODUCT_LOGICAL_LIBRARY_ID
        {
            return Err(PeerSyncError::Validation(
                "bidirectional registration does not match its source session".to_owned(),
            ));
        }
        let receipt =
            crate::persistent_store::VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                &request.library_id,
                &session.target_device_id,
                SyncGenerationIdentity {
                    generation_id: request.generation.generation_id,
                    manifest_hash: request.generation.manifest_hash,
                    generation_sequence: request.generation.generation_sequence,
                },
                now_millis()?,
            )
            .map_err(store_error)?;
        self.store
            .lock()
            .map_err(|error| {
                PeerSyncError::Storage(format!(
                    "bidirectional production store mutex poisoned: {error}"
                ))
            })?
            .attach_verified_sync_device_at_common_base(receipt, request.expected_revision)
            .map_err(store_error)?;
        Ok(())
    }

    fn validate_session(&self, session: &LanBidirectionalSession) -> Result<(), PeerSyncError> {
        if session.session_id != self.source_session_id
            || session.source_device_id != self.source_device_id
        {
            return Err(PeerSyncError::Validation(
                "bidirectional request does not match its source session".to_owned(),
            ));
        }
        Ok(())
    }
}

impl LanBidirectionalControl for ProductionLanBidirectionalControl {
    fn register(
        &self,
        session: LanBidirectionalSession,
        request: LanBidirectionalRegistrationRequest,
    ) -> Result<(), PeerSyncError> {
        ProductionLanBidirectionalControl::register(self, session, request)
    }

    fn remote_apply(
        &self,
        session: LanBidirectionalSession,
        request: LanBidirectionalRemoteApplyRequest,
        cancellation: &dyn CancellationProbe,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
        self.validate_session(&session)?;
        let cas = PayloadCas::new(&self.app_root)?;
        let reconciled = {
            let mut store = self.store.lock().map_err(|error| {
                PeerSyncError::Storage(format!(
                    "bidirectional production store mutex poisoned: {error}"
                ))
            })?;
            reconcile_source_operation(&mut store, &cas, &self.app_root, &session, &request)?
        };
        let (retained_source, retained_source_job_id) = match reconciled {
            SourceOperationReconcile::Completed(
                evidence,
                receipt,
                completed_revision,
                durable_job_id,
            ) => {
                verify_source_prepared_backup(&self.app_root, &evidence)?;
                release_retained_source_job(
                    &self.app_root,
                    &durable_job_id,
                    CasReleaseOutcome::Committed,
                )?;
                store_source_completed(&self.app_root, evidence, &receipt, completed_revision)?;
                return Ok(receipt);
            }
            SourceOperationReconcile::Retry(evidence, durable_job_id) => {
                (Some(evidence), Some(durable_job_id))
            }
            SourceOperationReconcile::CompletedReplay(receipt) => return Ok(receipt),
            SourceOperationReconcile::New => (None, None),
        };
        let mut client = LanLogicalDeltaClient::claim(
            &request.source_endpoint,
            &request.source_session_id,
            &request.source_manifest_id,
            &request.source_claim,
            &self.source_device_id,
        )?;
        if client.source_device_id() != session.target_device_id {
            return Err(PeerSyncError::Validation(
                "bidirectional shared source belongs to another device".to_owned(),
            ));
        }
        let shared_manifest_bytes = client.fetch_manifest()?;
        let shared_manifest = decode_logical_manifest(&shared_manifest_bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let incoming_revision = i64::try_from(shared_manifest.source_revision).map_err(|_| {
            PeerSyncError::Validation(
                "bidirectional shared revision exceeds SQLite range".to_owned(),
            )
        })?;
        let shared_manifest_hash = hash_logical_manifest(&shared_manifest)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let shared_generation = LanBidirectionalGeneration {
            generation_id: shared_manifest.generation,
            manifest_hash: shared_manifest_hash,
            generation_sequence: shared_manifest.generation_sequence,
        };
        if retained_source.as_ref().is_some_and(|evidence| {
            evidence.shared_generation != shared_generation
                || evidence.incoming_revision != incoming_revision
        }) {
            return Err(PeerSyncError::Validation(
                "bidirectional source retry shared identity differs from retained evidence"
                    .to_owned(),
            ));
        }
        let losing_generation = SyncGenerationIdentity {
            generation_id: request.expected_source_generation.generation_id,
            manifest_hash: request.expected_source_generation.manifest_hash,
            generation_sequence: request.expected_source_generation.generation_sequence,
        };
        let mut store = self.store.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "bidirectional production store mutex poisoned: {error}"
            ))
        })?;
        let verified_receipt = apply_bidirectional_remote_shared_inner(
            &mut store,
            &cas,
            &self.app_root,
            &request.operation_id,
            &session.target_device_id,
            request.expected_source_revision,
            &request.expected_common_base_manifest_hash,
            &losing_generation,
            shared_generation,
            &shared_manifest_bytes,
            &mut client,
            request.backup_losing_side,
            Some(&self.source_device_id),
            retained_source.as_ref(),
            retained_source_job_id.as_deref(),
            request.completion_deferred_v1.unwrap_or(false),
            cancellation,
        )?;
        let evidence = PeerBidirectionalOperationJournal::new(&self.app_root)
            .load()?
            .as_ref()
            .and_then(SourcePreparedEvidence::from_operation)
            .ok_or_else(|| {
                PeerSyncError::Storage(
                    "bidirectional source activation is missing prepared evidence".to_owned(),
                )
            })?;
        #[cfg(test)]
        if SOURCE_COMPLETE_STORE_FAILPOINT.with(|enabled| enabled.replace(false)) {
            return Err(PeerSyncError::Storage(
                "simulated source completion journal failure".to_owned(),
            ));
        }
        let receipt = verified_receipt.receipt();
        store_source_completed(
            &self.app_root,
            evidence,
            receipt,
            receipt.committed_revision,
        )?;
        Ok(verified_receipt.into_receipt())
    }
}

// Desktop-only reverse tunnel wrapper: the target-side remote apply forwards
// its temporary shared source through it. Android targets pair over the
// trusted LAN instead.
#[cfg(desktop)]
struct BidirectionalTunnel(tunnel::RunningTunnel);

#[cfg(desktop)]
impl BidirectionalTunnel {
    fn transport_url(&self) -> &url::Url {
        self.0.transport_url()
    }
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.0.stop(Duration::from_secs(2)).map_err(|_| {
            PeerSyncError::Transport("peer bidirectional tunnel failed to stop".to_owned())
        })
    }
}

#[cfg(desktop)]
type FailedBidirectionalTunnel = TunnelStartFailure<SystemTunnelProcess, LanCloneHost>;

#[cfg(desktop)]
trait ReverseTunnelProcess: Send {
    fn stop(&mut self) -> Result<(), PeerSyncError>;
}

#[cfg(desktop)]
impl ReverseTunnelProcess for BidirectionalTunnel {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        BidirectionalTunnel::stop(self)
    }
}

#[cfg(desktop)]
enum ReverseTunnelCleanupOwner {
    Running(Box<dyn ReverseTunnelProcess>),
    Failed(FailedBidirectionalTunnel),
}

#[cfg(target_os = "android")]
enum ReverseTunnelCleanupOwner {}

#[cfg(desktop)]
impl ReverseTunnelCleanupOwner {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        match self {
            Self::Running(tunnel) => tunnel.stop(),
            Self::Failed(failure) => failure.retry_cleanup().map_err(|_| {
                PeerSyncError::Transport("reverse tunnel cleanup is pending".to_owned())
            }),
        }
    }
}

#[cfg(desktop)]
static REVERSE_TUNNEL_CLEANUP: LazyLock<Mutex<BTreeMap<String, ReverseTunnelCleanupOwner>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

#[cfg(desktop)]
fn retry_reverse_tunnel_cleanup(operation_id: &str) -> Result<(), PeerSyncError> {
    let mut tunnel = REVERSE_TUNNEL_CLEANUP
        .lock()
        .map_err(|error| {
            PeerSyncError::Storage(format!("reverse tunnel cleanup mutex poisoned: {error}"))
        })?
        .remove(operation_id);
    let Some(mut tunnel) = tunnel.take() else {
        return Ok(());
    };
    let result = tunnel.stop();
    if let Err(error) = result {
        REVERSE_TUNNEL_CLEANUP
            .lock()
            .map_err(|poison| {
                PeerSyncError::Storage(format!("reverse tunnel cleanup mutex poisoned: {poison}"))
            })?
            .insert(operation_id.to_owned(), tunnel);
        return Err(error);
    }
    Ok(())
}

#[cfg(desktop)]
fn retain_reverse_cleanup_owner(
    operation_id: &str,
    mut owner: ReverseTunnelCleanupOwner,
) -> Result<(), PeerSyncError> {
    match owner.stop() {
        Ok(()) => Ok(()),
        Err(error) => {
            REVERSE_TUNNEL_CLEANUP
                .lock()
                .map_err(|poison| {
                    PeerSyncError::Storage(format!(
                        "reverse tunnel cleanup mutex poisoned: {poison}"
                    ))
                })?
                .insert(operation_id.to_owned(), owner);
            Err(error)
        }
    }
}

/// Stops every reverse tunnel this process still owns. The desktop exit hook
/// calls it so a cloudflared child never outlives the app.
#[cfg(desktop)]
pub(crate) fn cleanup_reverse_tunnels_for_exit() {
    let operation_ids = REVERSE_TUNNEL_CLEANUP
        .lock()
        .map(|owners| owners.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for operation_id in operation_ids {
        let _ = retry_reverse_tunnel_cleanup(&operation_id);
    }
}

#[cfg(target_os = "android")]
fn retry_reverse_tunnel_cleanup(_operation_id: &str) -> Result<(), PeerSyncError> {
    Ok(())
}

#[cfg(target_os = "android")]
fn retain_reverse_cleanup_owner(
    _operation_id: &str,
    owner: ReverseTunnelCleanupOwner,
) -> Result<(), PeerSyncError> {
    match owner {}
}

fn resolve_remote_apply_result(
    result: Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError>,
    cleanup: Result<(), PeerSyncError>,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    match (result, cleanup) {
        (Ok(receipt), Ok(())) => Ok(receipt),
        (Err(primary), _) => Err(primary),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn finish_remote_apply_request(
    app_root: &Path,
    operation_id: &str,
    result: Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError>,
    reverse_owner: Option<ReverseTunnelCleanupOwner>,
    lan_host: Option<&mut LanCloneHost>,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    let receipt_store = match &result {
        Ok(receipt) => retain_remote_apply_receipt(app_root, operation_id, receipt),
        Err(_) => Ok(()),
    };
    let cleanup = if let Some(owner) = reverse_owner {
        retain_reverse_cleanup_owner(operation_id, owner)
    } else if let Some(host) = lan_host {
        host.stop()
    } else {
        Ok(())
    };
    if let Err(error) = receipt_store {
        return Err(error);
    }
    resolve_remote_apply_result(result, cleanup)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<PeerBidirectionalStatusOperation>,
}

#[cfg(any(target_os = "android", test))]
type AndroidBidirectionalTargetStatus =
    AndroidTargetForegroundTransition<PeerBidirectionalSyncResult>;

#[derive(Default)]
struct PeerBidirectionalRuntime {
    target_active: bool,
    #[cfg(any(target_os = "android", test))]
    target_foreground: Option<AndroidBidirectionalTargetStatus>,
}

#[derive(Clone)]
pub(crate) struct PeerBidirectionalCommandState {
    runtime: Arc<Mutex<PeerBidirectionalRuntime>>,
    lifecycle_operation: Arc<Mutex<()>>,
}

impl Default for PeerBidirectionalCommandState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerBidirectionalRuntime::default())),
            lifecycle_operation: Arc::new(Mutex::new(())),
        }
    }
}

impl PeerBidirectionalCommandState {
    fn lock_lifecycle_operation(&self) -> Result<MutexGuard<'_, ()>, PeerSyncError> {
        self.lifecycle_operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "peer bidirectional lifecycle mutex poisoned: {error}"
            ))
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, PeerBidirectionalRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "peer bidirectional command state mutex poisoned: {error}"
            ))
        })
    }

    #[cfg(any(target_os = "android", test))]
    fn reserve_target_foreground(&self) -> Result<AndroidForegroundKey, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        if runtime.target_foreground.is_some() {
            return Err(PeerSyncError::Protocol(
                "Android bidirectional target foreground cleanup is pending".to_owned(),
            ));
        }
        let foreground = registry()
            .reserve(AndroidForegroundLane::P5Target)
            .map_err(PeerSyncError::Protocol)?;
        runtime.target_foreground = Some(AndroidBidirectionalTargetStatus::reserved(
            foreground.clone(),
        ));
        Ok(foreground)
    }

    #[cfg(any(target_os = "android", test))]
    fn mark_target_running_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<AndroidCancellationProbe, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android bidirectional target foreground is absent".to_owned())
        })?;
        target.require_exact_reserved(foreground).map_err(|_| {
            PeerSyncError::Protocol(
                "Android bidirectional target foreground identity is stale".to_owned(),
            )
        })?;
        if !registry().retain_target_exact(foreground) {
            return Err(PeerSyncError::Protocol(
                "Android bidirectional target foreground could not be retained".to_owned(),
            ));
        }
        let cancellation = registry().acquire_exact(foreground).ok_or_else(|| {
            PeerSyncError::Protocol(
                "Android bidirectional target foreground is not attached".to_owned(),
            )
        })?;
        target.mark_running();
        Ok(cancellation)
    }

    #[cfg(any(target_os = "android", test))]
    fn publish_target_terminal_exact(
        &self,
        foreground: &AndroidForegroundKey,
        outcome: Result<PeerBidirectionalSyncResult, String>,
    ) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android bidirectional target foreground is absent".to_owned())
        })?;
        target.require_exact_running(foreground).map_err(|_| {
            PeerSyncError::Protocol(
                "Android bidirectional target foreground identity is stale".to_owned(),
            )
        })?;
        target.publish_terminal(outcome);
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn target_foreground_status(
        &self,
    ) -> Result<Option<AndroidBidirectionalTargetStatus>, PeerSyncError> {
        Ok(self.lock()?.target_foreground.clone())
    }

    #[cfg(any(target_os = "android", test))]
    fn cancel_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        let matches_target = self
            .lock()?
            .target_foreground
            .as_ref()
            .is_some_and(|target| target.matches(foreground));
        // The runtime guard is dropped before cancel_exact: a registered
        // source_stop callback runs synchronously on this thread and re-locks
        // this non-reentrant mutex. cancel_exact keeps the exact-generation
        // semantics through its own key equality check.
        Ok(matches_target && registry().cancel_exact(foreground))
    }

    #[cfg(any(target_os = "android", test))]
    fn release_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        let operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let Some(target) = runtime.target_foreground.as_ref() else {
            return Ok(registry().release_target_exact(foreground));
        };
        if !target.matches(foreground) {
            return Ok(false);
        }
        if target.is_running() {
            // Both guards are dropped before cancel_exact: a registered
            // source_stop callback runs synchronously on this thread and
            // re-locks these non-reentrant mutexes. cancel_exact keeps the
            // exact-generation semantics through its own key equality check.
            drop(runtime);
            drop(operation);
            let _ = registry().cancel_exact(foreground);
            return Ok(false);
        }
        if !registry().release_target_exact(foreground) {
            return Ok(false);
        }
        runtime.target_foreground = None;
        Ok(true)
    }

    fn begin_target(&self) -> Result<PeerBidirectionalStateGuard, PeerSyncError> {
        let mut runtime = self.lock()?;
        if runtime.target_active {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional target operation is active".to_owned(),
            ));
        }
        runtime.target_active = true;
        Ok(PeerBidirectionalStateGuard {
            state: self.clone(),
        })
    }

    fn begin_target_if_idle(&self) -> Result<Option<PeerBidirectionalStateGuard>, PeerSyncError> {
        match self.begin_target() {
            Ok(guard) => Ok(Some(guard)),
            Err(PeerSyncError::Protocol(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn status(
        &self,
        app_root: &Path,
        store: &mut PersistentStore,
    ) -> Result<PeerBidirectionalStatus, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let journal = PeerBidirectionalOperationJournal::new(app_root);
        let mut retained = journal.load()?;
        let prepared = matches!(
            retained.as_ref(),
            Some(
                PeerBidirectionalDurableOperation::SourcePrepared { .. }
                    | PeerBidirectionalDurableOperation::TargetPrepared { .. }
            )
        );
        let promotion_guard = if prepared {
            self.begin_target_if_idle()?
        } else {
            None
        };
        if promotion_guard.is_some() {
            match retained.as_ref() {
                Some(PeerBidirectionalDurableOperation::TargetPrepared { .. }) => {
                    promote_target_prepared_for_status(store, app_root)?;
                    retained = journal.load()?;
                }
                Some(operation @ PeerBidirectionalDurableOperation::SourcePrepared { .. }) => {
                    let evidence =
                        SourcePreparedEvidence::from_operation(operation).ok_or_else(|| {
                            PeerSyncError::Storage(
                                "source-prepared operation is invalid".to_owned(),
                            )
                        })?;
                    let classification = match PayloadCas::new(app_root) {
                        Ok(cas) => classify_source_prepared_state(store, &cas, &evidence),
                        Err(error) => Err(error.into()),
                    };
                    if let Ok(SourcePreparedState::Postcommit {
                        receipt,
                        completed_revision,
                    }) = classification
                    {
                        let durable_job_id = match operation {
                            PeerBidirectionalDurableOperation::SourcePrepared {
                                durable_job_id,
                                ..
                            } => durable_job_id,
                            _ => unreachable!(),
                        };
                        verify_source_prepared_backup(app_root, &evidence)?;
                        release_retained_source_job(
                            app_root,
                            durable_job_id,
                            CasReleaseOutcome::Committed,
                        )?;
                        store_source_completed(app_root, evidence, &receipt, completed_revision)?;
                        retained = journal.load()?;
                    }
                }
                _ => {}
            }
        }
        let operation = retained.and_then(|operation| operation.status_projection());
        Ok(PeerBidirectionalStatus { operation })
    }

    fn acknowledge(
        &self,
        app_root: &Path,
        store: &mut PersistentStore,
        operation_id: &str,
    ) -> Result<(), PeerSyncError> {
        let journal = PeerBidirectionalOperationJournal::new(app_root);
        let _guard = self.begin_target()?;
        let operation = match journal.load_for_acknowledge()? {
            AcknowledgeOperationLoad::Valid(operation) => operation,
            AcknowledgeOperationLoad::Absent => return Ok(()),
            AcknowledgeOperationLoad::SemanticallyInvalid(snapshot) => {
                #[cfg(test)]
                if let Some(bytes) = ABANDON_BEFORE_CLAIM_REPLACEMENT
                    .with(|replacement| replacement.borrow_mut().take())
                {
                    fs::write(journal.root.join(OPERATION_FILE), bytes)?;
                }
                return journal.abandon_invalid_record(operation_id, snapshot);
            }
        };
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        let source_evidence = SourcePreparedEvidence::from_operation(&operation);
        match operation {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                let evidence = source_evidence.ok_or_else(|| {
                    PeerSyncError::Storage("source-prepared operation is invalid".to_owned())
                })?;
                require_deferred_source_completion_cleanup(app_root, &evidence)?;
                let cas = PayloadCas::new(app_root)?;
                match classify_source_prepared_state(store, &cas, &evidence)? {
                    SourcePreparedState::Precommit
                    | SourcePreparedState::PrecommitDescendant { .. } => {
                        journal.abandon(operation_id)
                    }
                    SourcePreparedState::Postcommit {
                        receipt,
                        completed_revision,
                    } => {
                        verify_source_prepared_backup(app_root, &evidence)?;
                        release_retained_source_job(
                            app_root,
                            &durable_job_id,
                            CasReleaseOutcome::Committed,
                        )?;
                        store_source_completed(app_root, evidence, &receipt, completed_revision)
                    }
                    SourcePreparedState::Mixed { .. } => Err(PeerSyncError::Validation(
                        "source-prepared operation cannot be abandoned from mixed durable state"
                            .to_owned(),
                    )),
                }
            }
            PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
                abandon_target_completion_delivery(app_root, &context)?;
                journal.abandon(operation_id)
            }
            PeerBidirectionalDurableOperation::LocalCommitted {
                context,
                remote_apply_receipt: Some(receipt),
                ..
            } => {
                if context.completion_mode == PeerBidirectionalCompletionMode::V1 {
                    match abandon_target_completion_delivery(app_root, &context)? {
                        Some(
                            super::device_registry::CompletionDeliveryAbandonStatus::Removed
                            | super::device_registry::CompletionDeliveryAbandonStatus::Missing,
                        ) => return journal.abandon(operation_id),
                        Some(
                            super::device_registry::CompletionDeliveryAbandonStatus::AlreadyDurable,
                        )
                        | None => {}
                    }
                }
                complete_bidirectional_local_after_remote_apply(
                    store,
                    &PayloadCas::new(app_root)?,
                    app_root,
                    operation_id,
                    receipt,
                )?;
                journal.acknowledge_completed(operation_id)
            }
            PeerBidirectionalDurableOperation::LocalCommitted { context, .. } => {
                abandon_target_completion_delivery(app_root, &context)?;
                journal.abandon(operation_id)
            }
            PeerBidirectionalDurableOperation::Completed {
                source_binding: Some(binding),
                ..
            } => {
                require_deferred_source_completion_cleanup(app_root, &binding)?;
                journal.acknowledge_completed(operation_id)
            }
            PeerBidirectionalDurableOperation::Completed { .. } => {
                journal.acknowledge_completed(operation_id)
            }
            PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } => {
                abandon_target_completion_delivery(app_root, &context)?;
                journal.abandon_awaiting_conflict(operation_id)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalCapabilities {
    desktop: bool,
    atomic_activation_ready: bool,
    authenticated_transport_ready: bool,
    lossless_backup_ready: bool,
    durable_state_ready: bool,
    production_enabled: bool,
}

#[tauri::command]
pub fn peer_bidirectional_capabilities() -> PeerBidirectionalCapabilities {
    PeerBidirectionalCapabilities {
        desktop: cfg!(desktop),
        atomic_activation_ready: true,
        authenticated_transport_ready: true,
        lossless_backup_ready: true,
        durable_state_ready: true,
        production_enabled: true,
    }
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_bidirectional_target_reserve(
    state: State<'_, PeerBidirectionalCommandState>,
) -> Result<AndroidForegroundKey, String> {
    finish_peer_command(
        "bidirectional target foreground reservation",
        state.reserve_target_foreground(),
    )
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_bidirectional_target_foreground_status(
    state: State<'_, PeerBidirectionalCommandState>,
) -> Result<Option<AndroidBidirectionalTargetStatus>, String> {
    finish_peer_command(
        "bidirectional target foreground status",
        state.target_foreground_status(),
    )
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_bidirectional_target_foreground_cancel(
    state: State<'_, PeerBidirectionalCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    finish_peer_command(
        "bidirectional target foreground cancellation",
        state.cancel_target_foreground_exact(&foreground),
    )
}

#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_bidirectional_target_foreground_release(
    state: State<'_, PeerBidirectionalCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    finish_peer_command(
        "bidirectional target foreground release",
        state.release_target_foreground_exact(&foreground),
    )
}

/// The application data directory every bidirectional command projects. Only
/// the device sync page reaches these commands, so the failure detail stays in
/// the native log and the caller sees the bounded code.
fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    finish_peer_command(
        "bidirectional application data directory",
        app.path()
            .app_data_dir()
            .map_err(|error| PeerSyncError::Storage(error.to_string())),
    )
}

pub(super) fn open_command_store(app: &AppHandle) -> Result<PersistentStore, PeerSyncError> {
    persistent_store::commands::with_store_mut(app.state(), |store| store.open_native_job_store())
        .map_err(store_error)
}

pub(crate) struct PreparedSharedBidirectionalSource {
    session: Option<PreparedBidirectionalLogicalLanSession>,
}

impl PreparedSharedBidirectionalSource {
    pub(crate) fn take_session(
        &mut self,
    ) -> Result<PreparedBidirectionalLogicalLanSession, PeerSyncError> {
        self.session.take().ok_or_else(|| {
            PeerSyncError::Protocol("shared bidirectional source session is unavailable".to_owned())
        })
    }

    pub(crate) fn cleanup(&mut self) {
        self.session.take();
    }
}

fn prepare_product_source_session(
    app_root: &Path,
    store: PersistentStore,
    source: LogicalDeltaSourceSession,
    source_device_id: &str,
    manifest_bytes: Vec<u8>,
) -> Result<(PreparedBidirectionalLogicalLanSession, String, String), PeerSyncError> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let manifest_id = source.manifest_hash().to_owned();
    let objects = source.objects().to_vec();
    let control = Arc::new(ProductionLanBidirectionalControl::new(
        app_root.to_path_buf(),
        store,
        &session_id,
        source_device_id,
    ));
    let prepared = PreparedBidirectionalLogicalLanSession::new(
        &session_id,
        source_device_id,
        manifest_id.clone(),
        manifest_bytes,
        objects,
        Box::new(source),
        control,
    )?;
    Ok((prepared, session_id, manifest_id))
}

pub(crate) fn prepare_shared_bidirectional_source(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    expected_revision: i64,
) -> Result<PreparedSharedBidirectionalSource, PeerSyncError> {
    let retained = PeerBidirectionalOperationJournal::new(app_root).load()?;
    let source_device_id = super::delta_commands::canonical_source_device_id(app_root)?;
    if retained
        .as_ref()
        .is_some_and(|operation| !retained_allows_source_prepare(operation, &source_device_id))
    {
        return Err(PeerSyncError::Protocol(
            "a target-owned bidirectional operation is retained".to_owned(),
        ));
    }
    store
        .reclaim_logical_generation_pins(P5_SOURCE_PIN_PREFIX)
        .map_err(store_error)?;
    let actual = store.revision().map_err(store_error)?;
    if actual != expected_revision {
        return Err(store_error(StoreError::RevisionConflict {
            expected: expected_revision,
            actual,
        }));
    }
    let built = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let control_store = store.open_native_job_store().map_err(store_error)?;
    let session_store = store.open_native_job_store().map_err(store_error)?;
    let source = LogicalDeltaSourceSession::open_owned(
        session_store,
        app_root,
        &built.manifest.library_id,
        &built.manifest.generation,
        P5_SOURCE_PIN_PREFIX,
    )?;
    let (session, _session_id, _manifest_id) = prepare_product_source_session(
        app_root,
        control_store,
        source,
        &source_device_id,
        built.manifest_bytes,
    )?;
    Ok(PreparedSharedBidirectionalSource {
        session: Some(session),
    })
}

#[cfg(desktop)]
#[allow(clippy::too_many_arguments)]
fn request_remote_apply_from_shared(
    session_store: PersistentStore,
    app_root: &Path,
    local_device_id: &str,
    client: &LanBidirectionalLogicalClient,
    context: &PeerBidirectionalOperationContext,
    shared_generation: &SyncGenerationIdentity,
    shared_manifest_bytes: &[u8],
    backup_losing_side: bool,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    if context.completion_mode == PeerBidirectionalCompletionMode::V1 {
        require_retained_v1_source(context, client)?;
    }
    retry_reverse_tunnel_cleanup(&context.operation_id)?;
    let source = LogicalDeltaSourceSession::open(
        session_store,
        app_root,
        &context.library_id,
        &shared_generation.generation_id,
    )?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let prepared = super::lan::PreparedLogicalLanSession::new(
        &session_id,
        local_device_id,
        shared_generation.manifest_hash.clone(),
        shared_manifest_bytes.to_vec(),
        source.objects().to_vec(),
        Box::new(source),
    )?;
    let mut host = Some(LanCloneHost::prepare_logical(prepared));
    let public_forward = context.credential.endpoint.is_empty()
        || context.credential.endpoint.starts_with("https://");
    let pairing = if public_forward {
        host.as_mut().unwrap().start_quick_tunnel_origin()?
    } else {
        host.as_mut().unwrap().start()?
    };
    let mut reverse_tunnel = if public_forward {
        match tunnel::start_quick_desktop_tunnel(host.take().unwrap()) {
            Ok(tunnel) => Some(BidirectionalTunnel(tunnel)),
            Err(mut failure) => {
                if failure.retry_cleanup().is_err() {
                    REVERSE_TUNNEL_CLEANUP
                        .lock()
                        .map_err(|error| {
                            PeerSyncError::Storage(format!(
                                "reverse tunnel cleanup mutex poisoned: {error}"
                            ))
                        })?
                        .insert(
                            context.operation_id.clone(),
                            ReverseTunnelCleanupOwner::Failed(failure),
                        );
                }
                return Err(PeerSyncError::Transport(
                    "reverse Quick Tunnel failed to start".to_owned(),
                ));
            }
        }
    } else {
        None
    };
    let endpoint = if let Some(tunnel) = reverse_tunnel.as_ref() {
        validate_p5_desktop_endpoint(tunnel.transport_url().as_str())?
    } else {
        let address = host
            .as_ref()
            .and_then(LanCloneHost::address)
            .ok_or_else(|| {
                PeerSyncError::Transport(
                    "temporary shared source address is unavailable".to_owned(),
                )
            })?;
        format!("http://{}:{}", discover_lan_ipv4()?, address.port())
    };
    let result = client.request_remote_apply(LanBidirectionalRemoteApplyRequest {
        operation_id: context.operation_id.clone(),
        source_endpoint: endpoint,
        source_session_id: pairing.session_id,
        source_manifest_id: pairing.manifest_id,
        source_claim: pairing.claim,
        expected_source_revision: context.expected_remote_revision,
        expected_source_generation: context.expected_remote_generation.clone(),
        expected_common_base_manifest_hash: context.previous_shared.manifest_hash.clone(),
        backup_losing_side,
        completion_deferred_v1: (context.completion_mode == PeerBidirectionalCompletionMode::V1)
            .then_some(true),
    });
    let reverse_owner = reverse_tunnel.take().map(|tunnel| {
        ReverseTunnelCleanupOwner::Running(Box::new(tunnel) as Box<dyn ReverseTunnelProcess>)
    });
    let lan_host = if reverse_owner.is_none() {
        host.as_mut()
    } else {
        None
    };
    finish_remote_apply_request(
        app_root,
        &context.operation_id,
        result,
        reverse_owner,
        lan_host,
    )
}

#[cfg(target_os = "android")]
#[allow(clippy::too_many_arguments)]
fn request_remote_apply_from_shared(
    session_store: PersistentStore,
    app_root: &Path,
    local_device_id: &str,
    client: &LanBidirectionalLogicalClient,
    context: &PeerBidirectionalOperationContext,
    shared_generation: &SyncGenerationIdentity,
    shared_manifest_bytes: &[u8],
    backup_losing_side: bool,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    if context.completion_mode == PeerBidirectionalCompletionMode::V1 {
        require_retained_v1_source(context, client)?;
    }
    let source = LogicalDeltaSourceSession::open(
        session_store,
        app_root,
        &context.library_id,
        &shared_generation.generation_id,
    )?;
    let session_id = uuid::Uuid::new_v4().to_string();
    let prepared = super::lan::PreparedLogicalLanSession::new(
        &session_id,
        local_device_id,
        shared_generation.manifest_hash.clone(),
        shared_manifest_bytes.to_vec(),
        source.objects().to_vec(),
        Box::new(source),
    )?;
    let mut host = LanCloneHost::prepare_logical(prepared);
    let address = super::lan::discover_private_lan_address().map_err(PeerSyncError::Transport)?;
    let pairing = host.start_private_lan(address)?;
    let port = host
        .address()
        .ok_or_else(|| {
            PeerSyncError::Transport("temporary shared source address is unavailable".to_owned())
        })?
        .port();
    let result = client.request_remote_apply(LanBidirectionalRemoteApplyRequest {
        operation_id: context.operation_id.clone(),
        source_endpoint: format!("http://{address}:{port}"),
        source_session_id: pairing.session_id,
        source_manifest_id: pairing.manifest_id,
        source_claim: pairing.claim,
        expected_source_revision: context.expected_remote_revision,
        expected_source_generation: context.expected_remote_generation.clone(),
        expected_common_base_manifest_hash: context.previous_shared.manifest_hash.clone(),
        backup_losing_side,
        completion_deferred_v1: (context.completion_mode == PeerBidirectionalCompletionMode::V1)
            .then_some(true),
    });
    finish_remote_apply_request(
        app_root,
        &context.operation_id,
        result,
        None,
        Some(&mut host),
    )
}

fn attach_local_device_at_existing_base(
    store: &mut PersistentStore,
    device_id: &str,
    expected_revision: i64,
) -> Result<SyncGenerationIdentity, PeerSyncError> {
    let common = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, device_id)
        .map_err(store_error)?
        .ok_or_else(|| {
            PeerSyncError::Validation(
                "bidirectional sync requires an exact existing P4 common base".to_owned(),
            )
        })?;
    let receipt = VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
        PRODUCT_LOGICAL_LIBRARY_ID,
        device_id,
        common.clone(),
        now_millis()?,
    )
    .map_err(store_error)?;
    store
        .attach_verified_sync_device_at_common_base(receipt, expected_revision)
        .map_err(store_error)?;
    Ok(common)
}

fn run_retained_remote_completion(
    store: &mut PersistentStore,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    client: Option<&LanBidirectionalLogicalClient>,
) -> Result<PeerBidirectionalSyncResult, PeerSyncError> {
    retry_reverse_tunnel_cleanup(operation_id)?;
    let retained = PeerBidirectionalOperationJournal::new(app_root)
        .load()?
        .ok_or_else(|| {
            PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
        })?;
    if retained.operation_id() != operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    let (committed_revision, has_remote_receipt) = match &retained {
        PeerBidirectionalDurableOperation::SourcePrepared { .. }
        | PeerBidirectionalDurableOperation::TargetPrepared { .. } => {
            return Err(PeerSyncError::Validation(
                "prepared operation requires its authenticated recovery path".to_owned(),
            ));
        }
        PeerBidirectionalDurableOperation::Completed { .. } => {
            return retained_result(&retained);
        }
        PeerBidirectionalDurableOperation::AwaitingConflict { .. } => {
            return Err(PeerSyncError::Validation(
                "bidirectional operation is awaiting conflict resolution".to_owned(),
            ));
        }
        PeerBidirectionalDurableOperation::LocalCommitted {
            committed_revision,
            remote_apply_receipt,
            ..
        } => (*committed_revision, remote_apply_receipt.is_some()),
    };
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision == committed_revision
        && actual_revision != expected_revision
        && !has_remote_receipt
    {
        return retained_result(&retained);
    }
    let cas = PayloadCas::new(app_root)?;
    let local_device_id = super::delta_commands::canonical_source_device_id(app_root)?;
    let session_store = store.open_native_job_store().map_err(store_error)?;
    let outcome = resume_bidirectional_local_committed_with_remote(
        store,
        &cas,
        app_root,
        operation_id,
        |context, _, shared, manifest, backup_losing_side| {
            if let Some(client) = client {
                request_remote_apply_from_shared(
                    session_store,
                    app_root,
                    &local_device_id,
                    client,
                    context,
                    shared,
                    manifest,
                    backup_losing_side,
                )
            } else {
                if context.credential.endpoint.is_empty() {
                    return Err(PeerSyncError::Transport(
                        "public tunnel recovery requires a fresh pairing link".to_owned(),
                    ));
                }
                let client = LanBidirectionalLogicalClient::resume(context.credential.clone())?;
                request_remote_apply_from_shared(
                    session_store,
                    app_root,
                    &local_device_id,
                    &client,
                    context,
                    shared,
                    manifest,
                    backup_losing_side,
                )
            }
        },
    )?;
    Ok(resumed_result(outcome))
}

fn recover_target_prepared_with_client(
    store: &mut PersistentStore,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    rebind_credential: bool,
    client: &mut LanBidirectionalLogicalClient,
    cancellation: &dyn CancellationProbe,
) -> Result<PeerBidirectionalSyncResult, PeerSyncError> {
    let operation = PeerBidirectionalOperationJournal::new(app_root)
        .load()?
        .ok_or_else(|| {
            PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
        })?;
    let PeerBidirectionalDurableOperation::TargetPrepared { context, .. } = &operation else {
        return retained_result(&operation);
    };
    if context.operation_id != operation_id {
        return Err(PeerSyncError::Validation(
            "another bidirectional operation is retained".to_owned(),
        ));
    }
    let manifest = fetch_retained_bidirectional_manifest(context, client)?;
    let replacement_credential = rebind_credential.then(|| client.credential());
    resume_bidirectional_target_prepared_with_cancellation(
        store,
        &PayloadCas::new(app_root)?,
        app_root,
        operation_id,
        expected_revision,
        replacement_credential,
        &manifest,
        client,
        cancellation,
    )?;
    if cancellation.is_cancelled() {
        return retained_result(
            &PeerBidirectionalOperationJournal::new(app_root)
                .load()?
                .ok_or_else(|| {
                    PeerSyncError::Storage("bidirectional operation was not retained".to_owned())
                })?,
        );
    }
    let committed_revision = store.revision().map_err(store_error)?;
    run_retained_remote_completion(
        store,
        app_root,
        operation_id,
        committed_revision,
        Some(client),
    )
}

fn fetch_retained_bidirectional_manifest(
    context: &PeerBidirectionalOperationContext,
    client: &LanBidirectionalLogicalClient,
) -> Result<Vec<u8>, PeerSyncError> {
    if client.source_device_id() != context.credential.source_device_id
        || client.device_id() != context.credential.device_id
    {
        return Err(PeerSyncError::Validation(
            "fresh bidirectional source differs from the retained operation".to_owned(),
        ));
    }
    if context.completion_mode != PeerBidirectionalCompletionMode::V1 {
        return client.fetch_manifest();
    }
    require_retained_v1_source(context, client)?;
    let lease = super::device_registry::CompletionLeaseId::parse(&context.operation_id)?;
    let response = client.fetch_manifest_with_completion_lease(Some(&lease))?;
    if response.completion_lease_id.as_ref() != Some(&lease) {
        return Err(PeerSyncError::Protocol(
            "registered bidirectional source changed the retained completion lease".to_owned(),
        ));
    }
    Ok(response.bytes)
}

fn require_retained_v1_source(
    context: &PeerBidirectionalOperationContext,
    client: &LanBidirectionalLogicalClient,
) -> Result<(), PeerSyncError> {
    let observed = client.hello_with_capabilities()?;
    if observed.completion != super::lan::PeerCompletionCapability::V1
        || observed.hello.device_id != context.credential.source_device_id
        || !observed.hello.permissions.allows_bidirectional()
    {
        return Err(PeerSyncError::Validation(
            "registered bidirectional source no longer advertises completion V1".to_owned(),
        ));
    }
    Ok(())
}

fn retained_allows_source_prepare(
    operation: &PeerBidirectionalDurableOperation,
    source_device_id: &str,
) -> bool {
    match operation {
        PeerBidirectionalDurableOperation::SourcePrepared {
            source_device_id: retained_source,
            ..
        } => retained_source == source_device_id,
        PeerBidirectionalDurableOperation::Completed {
            source_binding: Some(binding),
            ..
        } => binding.source_device_id == source_device_id,
        _ => false,
    }
}

#[tauri::command]
pub fn peer_bidirectional_status(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
) -> Result<PeerBidirectionalStatus, String> {
    let mut store = finish_peer_command("bidirectional status store", open_command_store(&app))?;
    finish_peer_command(
        "bidirectional status",
        state.status(&app_root(&app)?, &mut store),
    )
}

pub(crate) async fn peer_bidirectional_sync_registered_client(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    source_device_id: String,
    bearer: String,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
) -> Result<PeerBidirectionalSyncResult, String> {
    peer_bidirectional_sync_with_factory(
        app,
        state,
        expected_revision,
        foreground,
        move |_, local_device_id| {
            LanBidirectionalLogicalClient::from_registered(
                &endpoint,
                &session_id,
                &manifest_id,
                local_device_id,
                &source_device_id,
                &bearer,
            )
        },
    )
    .await
}

fn bidirectional_peer_operation_failure(context: &str, error: PeerSyncError) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    code_for(&error).code().to_owned()
}

fn bound_registered_bidirectional_outcome<T>(outcome: Result<T, String>) -> Result<T, String> {
    outcome.map_err(|error| {
        if is_bounded_code(&error) {
            error
        } else {
            crate::nlog!("warn", "registered bidirectional operation failed: {error}");
            PeerCommandCode::OperationFailed.code().to_owned()
        }
    })
}

async fn peer_bidirectional_sync_with_factory<
    F: Fn(&Path, &str) -> Result<LanBidirectionalLogicalClient, PeerSyncError> + Send + Sync + 'static,
>(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
    client_factory: F,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    #[cfg(target_os = "android")]
    let foreground = foreground.ok_or_else(|| {
        crate::nlog!("warn", "registered bidirectional foreground is missing");
        PeerCommandCode::OperationFailed.code().to_owned()
    })?;
    #[cfg(target_os = "android")]
    let cancellation = state
        .mark_target_running_exact(&foreground)
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional target foreground", error)
        })?;
    #[cfg(desktop)]
    let cancellation = {
        let _ = foreground;
        NeverCancelled
    };
    let worker_state = state.clone();
    let root = app_root(&app)?;
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let _guard = worker_state.begin_target().map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional target state", error)
        })?;
        #[cfg(target_os = "android")]
        if cancellation.is_cancelled() {
            return Err("Android foreground service was cancelled".to_owned());
        }
        if let Some(retained) = PeerBidirectionalOperationJournal::new(&root)
            .load()
            .map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional operation journal", error)
            })?
        {
            let local_device_id = super::delta_commands::canonical_source_device_id(&root)
                .map_err(|error| {
                    bidirectional_peer_operation_failure("bidirectional local identity", error)
                })?;
            match &retained {
                PeerBidirectionalDurableOperation::TargetPrepared { context, .. } => {
                    if context.credential.device_id != local_device_id {
                        return Err(
                            "retained bidirectional operation belongs to another target device"
                                .to_owned(),
                        );
                    }
                    let mut client = client_factory(&root, &local_device_id).map_err(|error| {
                        bidirectional_peer_operation_failure("bidirectional client", error)
                    })?;
                    if client.source_device_id() != context.credential.source_device_id {
                        return Err(
                            "fresh bidirectional source belongs to another device".to_owned()
                        );
                    }
                    let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
                    return recover_target_prepared_with_client(
                        &mut store,
                        &root,
                        &context.operation_id,
                        expected_revision,
                        true,
                        &mut client,
                        &cancellation,
                    )
                    .map_err(|error| {
                        bidirectional_peer_operation_failure("bidirectional target recovery", error)
                    });
                }
                PeerBidirectionalDurableOperation::LocalCommitted {
                    context,
                    remote_apply_receipt,
                    ..
                } => {
                    if remote_apply_receipt.is_some() {
                        let mut store =
                            open_command_store(&app).map_err(|error| error.to_string())?;
                        return run_retained_remote_completion(
                            &mut store,
                            &root,
                            &context.operation_id,
                            expected_revision,
                            None,
                        )
                        .map_err(|error| {
                            bidirectional_peer_operation_failure(
                                "bidirectional remote completion",
                                error,
                            )
                        });
                    }
                    if context.credential.device_id != local_device_id {
                        return Err(
                            "retained bidirectional operation belongs to another target device"
                                .to_owned(),
                        );
                    }
                    let client = client_factory(&root, &local_device_id).map_err(|error| {
                        bidirectional_peer_operation_failure("bidirectional client", error)
                    })?;
                    if client.source_device_id() != context.credential.source_device_id {
                        return Err(
                            "fresh bidirectional source belongs to another device".to_owned()
                        );
                    }
                    if context.completion_mode != PeerBidirectionalCompletionMode::V1 {
                        let manifest = client.fetch_manifest().map_err(|error| {
                            bidirectional_peer_operation_failure("bidirectional manifest", error)
                        })?;
                        let decoded = decode_logical_manifest(&manifest)
                            .map_err(|error| error.to_string())?;
                        if decoded.library_id != context.library_id {
                            return Err(
                                "fresh bidirectional source belongs to another library".to_owned()
                            );
                        }
                    }
                    let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
                    return run_retained_remote_completion(
                        &mut store,
                        &root,
                        &context.operation_id,
                        expected_revision,
                        Some(&client),
                    )
                    .map_err(|error| {
                        bidirectional_peer_operation_failure(
                            "bidirectional remote completion",
                            error,
                        )
                    });
                }
                PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } => {
                    if context.credential.device_id != local_device_id {
                        return Err(
                            "retained bidirectional operation belongs to another target device"
                                .to_owned(),
                        );
                    }
                    let client = client_factory(&root, &local_device_id).map_err(|error| {
                        bidirectional_peer_operation_failure("bidirectional client", error)
                    })?;
                    let manifest_bytes = fetch_retained_bidirectional_manifest(context, &client)
                        .map_err(|error| {
                            bidirectional_peer_operation_failure("bidirectional manifest", error)
                        })?;
                    let manifest = decode_logical_manifest(&manifest_bytes)
                        .map_err(|error| error.to_string())?;
                    let remote_revision =
                        i64::try_from(manifest.source_revision).map_err(|_| {
                            "bidirectional peer revision exceeds SQLite range".to_owned()
                        })?;
                    let generation = LanBidirectionalGeneration {
                        generation_id: manifest.generation.clone(),
                        manifest_hash: hash_logical_manifest(&manifest)
                            .map_err(|error| error.to_string())?,
                        generation_sequence: manifest.generation_sequence.clone(),
                    };
                    return rebind_awaiting_conflict(
                        &root,
                        retained,
                        client.credential(),
                        &manifest.library_id,
                        remote_revision,
                        &generation,
                    )
                    .map_err(|error| error.to_string());
                }
                _ => return retained_result(&retained).map_err(|error| error.to_string()),
            }
        }
        let local_device_id =
            super::delta_commands::canonical_source_device_id(&root).map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional local identity", error)
            })?;
        let mut client = client_factory(&root, &local_device_id)
            .map_err(|error| bidirectional_peer_operation_failure("bidirectional client", error))?;
        let completion_capability = {
            let observed = client.hello_with_capabilities().map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional capabilities", error)
            })?;
            if observed.hello.device_id != client.source_device_id()
                || !observed.hello.permissions.allows_bidirectional()
            {
                return Err(
                    "registered bidirectional source identity or permission changed".to_owned(),
                );
            }
            observed.completion
        };
        let (remote_manifest_bytes, completion_operation_id, completion_mode) =
            if completion_capability == super::lan::PeerCompletionCapability::V1 {
                let completion =
                    client
                        .fetch_manifest_with_completion_lease(None)
                        .map_err(|error| {
                            bidirectional_peer_operation_failure(
                                "bidirectional completion manifest",
                                error,
                            )
                        })?;
                let lease = completion.completion_lease_id.ok_or_else(|| {
                    "registered bidirectional V1 source omitted completion lease".to_owned()
                })?;
                (
                    completion.bytes,
                    Some(lease.as_str().to_owned()),
                    PeerBidirectionalCompletionMode::V1,
                )
            } else {
                (
                    client.fetch_manifest().map_err(|error| {
                        bidirectional_peer_operation_failure("bidirectional manifest", error)
                    })?,
                    None,
                    PeerBidirectionalCompletionMode::Unsupported,
                )
            };
        let remote_manifest =
            decode_logical_manifest(&remote_manifest_bytes).map_err(|error| error.to_string())?;
        let remote_revision = i64::try_from(remote_manifest.source_revision)
            .map_err(|_| "bidirectional peer revision exceeds SQLite range".to_owned())?;
        let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
        let common = attach_local_device_at_existing_base(
            &mut store,
            client.source_device_id(),
            expected_revision,
        )
        .map_err(|error| error.to_string())?;
        client
            .register(LanBidirectionalRegistrationRequest {
                library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                generation: LanBidirectionalGeneration {
                    generation_id: common.generation_id,
                    manifest_hash: common.manifest_hash,
                    generation_sequence: common.generation_sequence,
                },
                expected_revision: remote_revision,
            })
            .map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional registration", error)
            })?;
        let outcome = begin_bidirectional_local_merge_with_cancellation(
            &mut store,
            &PayloadCas::new(&root).map_err(|error| error.to_string())?,
            &root,
            client.credential(),
            expected_revision,
            &remote_manifest_bytes,
            &mut client,
            &cancellation,
            completion_operation_id,
            completion_mode,
        )
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional local merge", error)
        })?;
        match outcome {
            LocalMergeOutcome::Conflict(result) => Ok(conflict_result(result)),
            LocalMergeOutcome::LocalCommitted => {
                if cancellation.is_cancelled() {
                    return retained_result(
                        &PeerBidirectionalOperationJournal::new(&root)
                            .load()
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "bidirectional operation was not retained".to_owned())?,
                    )
                    .map_err(|error| error.to_string());
                }
                let committed_revision = store.revision().map_err(|error| error.to_string())?;
                run_retained_remote_completion(
                    &mut store,
                    &root,
                    PeerBidirectionalOperationJournal::new(&root)
                        .load()
                        .map_err(|error| error.to_string())?
                        .as_ref()
                        .ok_or_else(|| "bidirectional operation was not retained".to_owned())?
                        .operation_id(),
                    committed_revision,
                    Some(&client),
                )
                .map_err(|error| {
                    bidirectional_peer_operation_failure("bidirectional remote completion", error)
                })
            }
        }
    })
    .await
    // A worker that never returned has no outcome of its own, so its join
    // failure becomes the outcome and the Android terminal publication below
    // still runs.
    .unwrap_or_else(|error| {
        crate::nlog!("warn", "registered bidirectional worker failed: {error}");
        Err(PeerCommandCode::OperationFailed.code().to_owned())
    });
    let outcome = bound_registered_bidirectional_outcome(outcome);
    #[cfg(target_os = "android")]
    state
        .publish_target_terminal_exact(&foreground, outcome.clone())
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional terminal state", error)
        })?;
    outcome
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn peer_bidirectional_resolve_registered_client(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
    winner: PeerBidirectionalConflictWinner,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    source_device_id: String,
    bearer: String,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
) -> Result<PeerBidirectionalSyncResult, String> {
    peer_bidirectional_resolve_with_factory(
        app,
        state,
        operation_id,
        winner,
        expected_revision,
        foreground,
        move |_, local_device_id| {
            LanBidirectionalLogicalClient::from_registered(
                &endpoint,
                &session_id,
                &manifest_id,
                local_device_id,
                &source_device_id,
                &bearer,
            )
        },
    )
    .await
}

async fn peer_bidirectional_resolve_with_factory<
    F: Fn(&Path, &str) -> Result<LanBidirectionalLogicalClient, PeerSyncError> + Send + Sync + 'static,
>(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
    client_factory: F,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    #[cfg(target_os = "android")]
    let foreground = foreground.ok_or_else(|| {
        crate::nlog!("warn", "registered bidirectional foreground is missing");
        PeerCommandCode::OperationFailed.code().to_owned()
    })?;
    #[cfg(target_os = "android")]
    let cancellation = state
        .mark_target_running_exact(&foreground)
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional target foreground", error)
        })?;
    #[cfg(desktop)]
    let cancellation = {
        let _ = foreground;
        NeverCancelled
    };
    let worker_state = state.clone();
    let root = app_root(&app)?;
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let _guard = worker_state.begin_target().map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional target state", error)
        })?;
        #[cfg(target_os = "android")]
        if cancellation.is_cancelled() {
            return Err("Android foreground service was cancelled".to_owned());
        }
        let retained = PeerBidirectionalOperationJournal::new(&root)
            .load()
            .map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional operation journal", error)
            })?
            .ok_or_else(|| "bidirectional operation is not retained".to_owned())?;
        if retained.operation_id() != operation_id {
            return Err("another bidirectional operation is retained".to_owned());
        }
        let local_device_id =
            super::delta_commands::canonical_source_device_id(&root).map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional local identity", error)
            })?;
        let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &retained else {
            return retained_result(&retained).map_err(|error| error.to_string());
        };
        if context.credential.device_id != local_device_id {
            return Err(
                "retained bidirectional operation belongs to another target device".to_owned(),
            );
        }
        let mut client = client_factory(&root, &local_device_id)
            .map_err(|error| bidirectional_peer_operation_failure("bidirectional client", error))?;
        let remote_manifest =
            fetch_retained_bidirectional_manifest(context, &client).map_err(|error| {
                bidirectional_peer_operation_failure("bidirectional manifest", error)
            })?;
        let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
        let outcome = resolve_awaiting_conflict_with_fresh_source_and_cancellation(
            &mut store,
            &PayloadCas::new(&root).map_err(|error| error.to_string())?,
            &root,
            &retained,
            &operation_id,
            winner,
            expected_revision,
            &client.credential(),
            &remote_manifest,
            &mut client,
            &cancellation,
        )
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional conflict resolution", error)
        })?;
        match outcome {
            LocalMergeOutcome::Conflict(result) => Ok(conflict_result(result)),
            LocalMergeOutcome::LocalCommitted => {
                if cancellation.is_cancelled() {
                    return retained_result(
                        &PeerBidirectionalOperationJournal::new(&root)
                            .load()
                            .map_err(|error| error.to_string())?
                            .ok_or_else(|| "bidirectional operation was not retained".to_owned())?,
                    )
                    .map_err(|error| error.to_string());
                }
                let committed_revision = store.revision().map_err(|error| error.to_string())?;
                run_retained_remote_completion(
                    &mut store,
                    &root,
                    &operation_id,
                    committed_revision,
                    Some(&client),
                )
                .map_err(|error| {
                    bidirectional_peer_operation_failure("bidirectional remote completion", error)
                })
            }
        }
    })
    .await
    // Same as the sync worker above: the join failure is the outcome.
    .unwrap_or_else(|error| {
        crate::nlog!(
            "warn",
            "registered bidirectional resolution worker failed: {error}"
        );
        Err(PeerCommandCode::OperationFailed.code().to_owned())
    });
    let outcome = bound_registered_bidirectional_outcome(outcome);
    #[cfg(target_os = "android")]
    state
        .publish_target_terminal_exact(&foreground, outcome.clone())
        .map_err(|error| {
            bidirectional_peer_operation_failure("bidirectional terminal state", error)
        })?;
    outcome
}

#[tauri::command]
pub async fn peer_bidirectional_resume(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
    expected_revision: i64,
    foreground: Option<AndroidForegroundKey>,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    #[cfg(target_os = "android")]
    let foreground = finish_peer_command(
        "bidirectional resume foreground",
        foreground.ok_or_else(|| {
            PeerSyncError::Protocol(
                "Android bidirectional target foreground is required".to_owned(),
            )
        }),
    )?;
    #[cfg(target_os = "android")]
    let cancellation = finish_peer_command(
        "bidirectional resume target foreground",
        state.mark_target_running_exact(&foreground),
    )?;
    #[cfg(desktop)]
    let cancellation = {
        let _ = foreground;
        NeverCancelled
    };
    let worker_state = state.clone();
    let root = app_root(&app)?;
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let _guard = worker_state.begin_target()?;
        #[cfg(target_os = "android")]
        if cancellation.is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        let retained = PeerBidirectionalOperationJournal::new(&root).load()?;
        if let Some(PeerBidirectionalDurableOperation::TargetPrepared { context, .. }) = retained {
            if context.operation_id != operation_id {
                return Err(PeerSyncError::Validation(
                    "another bidirectional operation is retained".to_owned(),
                ));
            }
            let mut client = LanBidirectionalLogicalClient::resume(context.credential)?;
            let mut store = open_command_store(&app)?;
            return recover_target_prepared_with_client(
                &mut store,
                &root,
                &operation_id,
                expected_revision,
                false,
                &mut client,
                &cancellation,
            );
        }
        if cancellation.is_cancelled() {
            return retained
                .as_ref()
                .ok_or_else(|| {
                    PeerSyncError::Validation("bidirectional operation is not retained".to_owned())
                })
                .and_then(retained_result);
        }
        let mut store = open_command_store(&app)?;
        run_retained_remote_completion(&mut store, &root, &operation_id, expected_revision, None)
    })
    .await;
    // A worker that never returned has no outcome of its own, so its join code
    // becomes the outcome: the Android foreground notification has to leave the
    // running state even when the worker panicked.
    let outcome = finish_peer_worker("bidirectional resume worker", joined.map(Ok))
        .and_then(|resumed| finish_peer_command("bidirectional resume", resumed));
    #[cfg(target_os = "android")]
    finish_peer_command(
        "bidirectional resume terminal state",
        state.publish_target_terminal_exact(&foreground, outcome.clone()),
    )?;
    outcome
}

#[tauri::command]
pub fn peer_bidirectional_acknowledge(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
) -> Result<(), String> {
    let root = app_root(&app)?;
    let mut store = finish_peer_command(
        "bidirectional acknowledgement store",
        open_command_store(&app),
    )?;
    finish_peer_command(
        "bidirectional acknowledgement",
        state.acknowledge(&root, &mut store, &operation_id),
    )
}

struct PeerBidirectionalStateGuard {
    state: PeerBidirectionalCommandState,
}

impl Drop for PeerBidirectionalStateGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.state.runtime.lock() {
            runtime.target_active = false;
        }
    }
}

fn now_millis() -> Result<i64, PeerSyncError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| PeerSyncError::Storage("system time exceeds SQLite range".to_owned()))
}

fn store_error(error: crate::persistent_store::StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

pub(crate) struct PeerBidirectionalOperationJournal {
    root: PathBuf,
}

/// `source_device_id` of `None` asks the same question about any registered source.
pub(crate) fn registered_bidirectional_source_is_active(
    app_root: &Path,
    source_device_id: Option<&str>,
) -> Result<bool, PeerSyncError> {
    Ok(matches!(
        PeerBidirectionalOperationJournal::new(app_root).load()?,
        Some(
            PeerBidirectionalDurableOperation::TargetPrepared { context, .. }
            | PeerBidirectionalDurableOperation::AwaitingConflict { context, .. }
            | PeerBidirectionalDurableOperation::LocalCommitted { context, .. }
        ) if source_device_id.is_none_or(|wanted| context.credential.source_device_id == wanted)
    ))
}

enum AcknowledgeOperationLoad {
    Absent,
    Valid(PeerBidirectionalDurableOperation),
    SemanticallyInvalid(InvalidOperationSnapshot),
}

struct InvalidOperationSnapshot {
    bytes: Vec<u8>,
    operation: PeerBidirectionalDurableOperation,
}

impl PeerBidirectionalOperationJournal {
    pub(crate) fn new(app_root: &Path) -> Self {
        Self {
            root: app_root.join("peer-bidirectional"),
        }
    }

    pub(crate) fn load(&self) -> Result<Option<PeerBidirectionalDurableOperation>, PeerSyncError> {
        super::maintenance::with_backup_reference_lifecycle(|| self.load_with_backup_lifecycle())
    }

    pub(crate) fn load_with_backup_lifecycle(
        &self,
    ) -> Result<Option<PeerBidirectionalDurableOperation>, PeerSyncError> {
        let Some(bytes) = self.read_bounded()? else {
            return Ok(None);
        };
        let operation = serde_json::from_slice::<PeerBidirectionalDurableOperation>(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        operation.validate()?;
        Ok(Some(operation))
    }

    fn load_for_acknowledge(&self) -> Result<AcknowledgeOperationLoad, PeerSyncError> {
        super::maintenance::with_backup_reference_lifecycle(|| {
            self.load_for_acknowledge_with_backup_lifecycle()
        })
    }

    fn load_for_acknowledge_with_backup_lifecycle(
        &self,
    ) -> Result<AcknowledgeOperationLoad, PeerSyncError> {
        let Some(bytes) = self.read_bounded()? else {
            return Ok(AcknowledgeOperationLoad::Absent);
        };
        let operation = serde_json::from_slice::<PeerBidirectionalDurableOperation>(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        match operation.validate() {
            Ok(()) => Ok(AcknowledgeOperationLoad::Valid(operation)),
            Err(_) => Ok(AcknowledgeOperationLoad::SemanticallyInvalid(
                InvalidOperationSnapshot { bytes, operation },
            )),
        }
    }

    fn read_bounded(&self) -> Result<Option<Vec<u8>>, PeerSyncError> {
        #[cfg(test)]
        if OPERATION_READ_FAILPOINT.with(|enabled| enabled.replace(false)) {
            return Err(PeerSyncError::Storage(
                "simulated bidirectional operation read failure".to_owned(),
            ));
        }
        self.recover_abandon_claim()?;
        let path = self.root.join(OPERATION_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_OPERATION_BYTES
        {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record is invalid".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(&path)?
            .take(MAX_OPERATION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OPERATION_BYTES {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record exceeds its bound".to_owned(),
            ));
        }
        Ok(Some(bytes))
    }

    fn validate_abandon_identity(
        operation: &PeerBidirectionalDurableOperation,
        operation_id: &str,
    ) -> Result<(), PeerSyncError> {
        let schema = match &operation {
            PeerBidirectionalDurableOperation::SourcePrepared { schema, .. }
            | PeerBidirectionalDurableOperation::TargetPrepared { schema, .. }
            | PeerBidirectionalDurableOperation::AwaitingConflict { schema, .. }
            | PeerBidirectionalDurableOperation::LocalCommitted { schema, .. }
            | PeerBidirectionalDurableOperation::Completed { schema, .. } => schema,
        };
        if schema != OPERATION_SCHEMA || !is_canonical_uuid(operation.operation_id()) {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record is invalid".to_owned(),
            ));
        }
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        Ok(())
    }

    fn abandon_invalid_record(
        &self,
        operation_id: &str,
        snapshot: InvalidOperationSnapshot,
    ) -> Result<(), PeerSyncError> {
        super::maintenance::with_backup_reference_lifecycle(|| {
            self.abandon_invalid_record_with_backup_lifecycle(operation_id, snapshot)
        })
    }

    fn abandon_invalid_record_with_backup_lifecycle(
        &self,
        operation_id: &str,
        snapshot: InvalidOperationSnapshot,
    ) -> Result<(), PeerSyncError> {
        Self::validate_abandon_identity(&snapshot.operation, operation_id)?;
        let operation_path = self.root.join(OPERATION_FILE);
        let claim_path = self.root.join(ABANDON_CLAIM_FILE);
        fs::rename(&operation_path, &claim_path)?;

        #[cfg(test)]
        if ABANDON_AFTER_CLAIM_FAILPOINT.with(|enabled| enabled.replace(false)) {
            return Err(PeerSyncError::Storage(
                "simulated crash after bidirectional operation claim".to_owned(),
            ));
        }

        let validation = (|| {
            let claimed = self.read_bounded_path(&claim_path)?;
            if claimed != snapshot.bytes {
                return Err(PeerSyncError::Storage(
                    "bidirectional operation changed before abandon".to_owned(),
                ));
            }
            let operation = serde_json::from_slice::<PeerBidirectionalDurableOperation>(&claimed)
                .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
            if operation.validate().is_ok() {
                return Err(PeerSyncError::Storage(
                    "valid bidirectional operation cannot use tolerant abandon".to_owned(),
                ));
            }
            Self::validate_abandon_identity(&operation, operation_id)
        })();
        if let Err(error) = validation {
            self.restore_claim(&claim_path)?;
            return Err(error);
        }

        #[cfg(test)]
        if ABANDON_CLAIM_REMOVE_FAILPOINT.with(|enabled| enabled.replace(false)) {
            let error = PeerSyncError::Storage(
                "simulated bidirectional operation claim removal failure".to_owned(),
            );
            self.restore_claim(&claim_path)?;
            return Err(error);
        }
        if let Err(error) = fs::remove_file(&claim_path) {
            let original = PeerSyncError::from(error);
            self.restore_claim(&claim_path)?;
            return Err(original);
        }
        Ok(())
    }

    fn read_bounded_path(&self, path: &Path) -> Result<Vec<u8>, PeerSyncError> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_OPERATION_BYTES
        {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record is invalid".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(path)?
            .take(MAX_OPERATION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_OPERATION_BYTES {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record exceeds its bound".to_owned(),
            ));
        }
        Ok(bytes)
    }

    fn restore_claim(&self, claim_path: &Path) -> Result<(), PeerSyncError> {
        let operation_path = self.root.join(OPERATION_FILE);
        fs::hard_link(claim_path, &operation_path)?;
        fs::remove_file(claim_path)?;
        Ok(())
    }

    fn recover_abandon_claim(&self) -> Result<(), PeerSyncError> {
        let claim_path = self.root.join(ABANDON_CLAIM_FILE);
        match fs::symlink_metadata(&claim_path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let claimed = self.read_bounded_path(&claim_path)?;
        let claimed_operation =
            serde_json::from_slice::<PeerBidirectionalDurableOperation>(&claimed)
                .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        let claimed_operation_id = claimed_operation.operation_id().to_owned();
        Self::validate_abandon_identity(&claimed_operation, &claimed_operation_id)?;

        let operation_path = self.root.join(OPERATION_FILE);
        match fs::symlink_metadata(&operation_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.restore_claim(&claim_path)
            }
            Err(error) => Err(error.into()),
            Ok(_) => {
                let canonical = self.read_bounded_path(&operation_path)?;
                let canonical_operation =
                    serde_json::from_slice::<PeerBidirectionalDurableOperation>(&canonical)
                        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
                let canonical_operation_id = canonical_operation.operation_id().to_owned();
                Self::validate_abandon_identity(&canonical_operation, &canonical_operation_id)?;
                if canonical != claimed || canonical_operation_id != claimed_operation_id {
                    return Err(PeerSyncError::Storage(
                        "bidirectional operation and abandon claim differ".to_owned(),
                    ));
                }
                fs::remove_file(claim_path)?;
                Ok(())
            }
        }
    }

    pub(crate) fn store(
        &self,
        operation: &PeerBidirectionalDurableOperation,
    ) -> Result<(), PeerSyncError> {
        if operation.backup_paths().is_empty() {
            return self.store_unlocked(operation);
        }
        super::maintenance::with_backup_reference_lifecycle(|| {
            self.validate_local_backup_publications(operation)?;
            self.store_unlocked(operation)
        })
    }

    fn validate_local_backup_publications(
        &self,
        operation: &PeerBidirectionalDurableOperation,
    ) -> Result<(), PeerSyncError> {
        let app_root = self.root.parent().ok_or_else(|| {
            PeerSyncError::Storage("bidirectional operation root has no app root".to_owned())
        })?;
        for backup_path in operation.backup_paths() {
            let path = Path::new(backup_path);
            let candidate = if path.is_absolute() {
                path.to_owned()
            } else {
                app_root.join(path)
            };
            let expected_local = bidirectional_backup_path(
                app_root,
                operation.operation_id(),
                PeerBidirectionalBackupSide::Local,
            );
            let expected_remote = bidirectional_backup_path(
                app_root,
                operation.operation_id(),
                PeerBidirectionalBackupSide::Remote,
            );
            // Older synthetic and migrated records can contain non-canonical paths. New
            // production publications always use the operation-owned deterministic path.
            if candidate != expected_local && candidate != expected_remote {
                continue;
            }
            let metadata = fs::symlink_metadata(&candidate).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    PeerSyncError::Storage(
                        "bidirectional backup disappeared before receipt publication".to_owned(),
                    )
                } else {
                    error.into()
                }
            })?;
            if !metadata.is_file() || backup_path_is_link_like(&metadata) {
                return Err(PeerSyncError::Storage(
                    "bidirectional backup disappeared before receipt publication".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn store_unlocked(
        &self,
        operation: &PeerBidirectionalDurableOperation,
    ) -> Result<(), PeerSyncError> {
        operation.validate()?;
        let mut durable = operation.clone();
        match &mut durable {
            PeerBidirectionalDurableOperation::TargetPrepared { context, .. }
            | PeerBidirectionalDurableOperation::AwaitingConflict { context, .. }
            | PeerBidirectionalDurableOperation::LocalCommitted { context, .. }
                if context.credential.endpoint.starts_with("https://") =>
            {
                context.credential.endpoint.clear();
                context.credential.session_id.clear();
                context.credential.bearer.clear();
            }
            _ => {}
        }
        let bytes = serde_json::to_vec(&durable)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        if bytes.len() as u64 > MAX_OPERATION_BYTES {
            return Err(PeerSyncError::Storage(
                "bidirectional operation record exceeds its bound".to_owned(),
            ));
        }
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(OPERATION_FILE);
        let temporary = self
            .root
            .join(format!(".operation-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            replace_file_atomic(&temporary, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub(crate) fn acknowledge_completed(&self, operation_id: &str) -> Result<(), PeerSyncError> {
        let Some(operation) = self.load()? else {
            return Ok(());
        };
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        if !matches!(
            operation,
            PeerBidirectionalDurableOperation::Completed { .. }
        ) {
            return Err(PeerSyncError::Validation(
                "only a completed bidirectional operation can be acknowledged".to_owned(),
            ));
        }
        match fs::remove_file(self.root.join(OPERATION_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn abandon(&self, operation_id: &str) -> Result<(), PeerSyncError> {
        let Some(operation) = self.load()? else {
            return Ok(());
        };
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        let repository_root = self.root.parent().ok_or_else(|| {
            PeerSyncError::Storage(
                "bidirectional operation directory has no repository root".to_owned(),
            )
        })?;
        let durable_job_id = match &operation {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                Some(durable_job_id.as_str())
            }
            PeerBidirectionalDurableOperation::TargetPrepared { context, .. }
            | PeerBidirectionalDurableOperation::AwaitingConflict { context, .. }
            | PeerBidirectionalDurableOperation::LocalCommitted { context, .. } => {
                Some(context.durable_job_id.as_str())
            }
            PeerBidirectionalDurableOperation::Completed { .. } => None,
        };
        if let Some(durable_job_id) = durable_job_id {
            match DurableCasJob::open(repository_root, durable_job_id) {
                Ok(mut job) if job.kind() == CasJobKind::LogicalDeltaTarget => {
                    job.release(CasReleaseOutcome::Aborted)?
                }
                Ok(_) => {
                    return Err(PeerSyncError::Storage(
                        "bidirectional operation job has an unexpected identity".to_owned(),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(evidence) = SourcePreparedEvidence::from_operation(&operation) {
            cleanup_source_prepared_backup_artifacts(repository_root, &evidence)?;
        }
        match fs::remove_file(self.root.join(OPERATION_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn abandon_awaiting_conflict(&self, operation_id: &str) -> Result<(), PeerSyncError> {
        let Some(operation) = self.load()? else {
            return Ok(());
        };
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        let PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } = &operation else {
            return Err(PeerSyncError::Validation(
                "only an awaiting-conflict operation can use conflict abandon".to_owned(),
            ));
        };
        let repository_root = self.root.parent().ok_or_else(|| {
            PeerSyncError::Storage(
                "bidirectional operation directory has no repository root".to_owned(),
            )
        })?;
        let mut job = match DurableCasJob::open(repository_root, &context.durable_job_id) {
            Ok(job) => job,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return match fs::remove_file(self.root.join(OPERATION_FILE)) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(error.into()),
                };
            }
            Err(error) => return Err(error.into()),
        };
        if job.kind() != CasJobKind::LogicalDeltaTarget || job.is_sealed() {
            return Err(PeerSyncError::Validation(
                "awaiting-conflict operation CAS job cannot be abandoned".to_owned(),
            ));
        }
        job.release(CasReleaseOutcome::Aborted)?;
        match fs::remove_file(self.root.join(OPERATION_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
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
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    sync_destination_parent(destination)
}

#[cfg(not(windows))]
fn sync_destination_parent(destination: &Path) -> Result<(), PeerSyncError> {
    #[cfg(test)]
    if ATOMIC_PARENT_SYNC_FAILPOINT.with(|enabled| enabled.replace(false)) {
        return Err(PeerSyncError::Storage(
            "simulated parent directory sync failure".to_owned(),
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        PeerSyncError::Storage("atomic destination has no parent directory".to_owned())
    })?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
#[path = "bidirectional_commands_tests.rs"]
mod tests;
