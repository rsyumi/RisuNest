use super::{
    execute_logical_delta_pull,
    lan::{
        LanBidirectionalBackupReceipt, LanBidirectionalControl, LanBidirectionalGeneration,
        LanBidirectionalLogicalClient, LanBidirectionalLogicalCredential,
        LanBidirectionalRegistrationRequest, LanBidirectionalRemoteApplyReceipt,
        LanBidirectionalRemoteApplyRequest, LanBidirectionalSession, LanCloneHostControl,
        LanLogicalDeltaClient, PreparedBidirectionalLogicalLanSession,
    },
    logical_delta::{decode_logical_manifest, hash_logical_manifest},
    logical_delta_transfer::execute_logical_delta_pull_with_pre_activation,
    LanCloneHost, LogicalDeltaActivation, LogicalDeltaObject, LogicalDeltaObjectSource,
    PeerSyncError,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    local_backup::NeverCancelled,
    lossless_backup::{
        create_and_verify_lossless_backup_v1_report,
        create_and_verify_peer_bidirectional_backup_v1_report,
        verify_lossless_package_v1_for_production, LosslessPeerSourceBinding,
    },
    persistent_store::{
        self, logical_delta_source::LogicalDeltaSourceSession, LogicalDeltaConflictKind,
        LogicalDeltaConflictPolicy, LogicalDeltaPlanResolution, PersistentLogicalDeltaTarget,
        PersistentStore, RegisteredSyncDevice, RegisteredSyncDeviceStatus, StoreError,
        SyncGenerationIdentity, VerifiedSyncDeviceRegistration, PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, UdpSocket},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};

const OPERATION_SCHEMA: &str = "risunest.peer-bidirectional-operation/v1";
const OPERATION_FILE: &str = "operation.json";
const MAX_OPERATION_BYTES: u64 = 1_048_576;

#[cfg(test)]
thread_local! {
    static COMPLETE_BEFORE_ACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static COMPLETE_AFTER_ACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_COMPLETE_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_PREPARED_STORE_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_PREPARED_STORE_PANIC: Cell<bool> = const { Cell::new(false) };
    static SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT: Cell<bool> = const { Cell::new(false) };
    static DISCOVER_LAN_IPV4_OVERRIDE: Cell<Option<Ipv4Addr>> = const { Cell::new(None) };
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
}

fn conservative_remote_backup_required() -> bool {
    true
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        backup: Option<LanBidirectionalBackupReceipt>,
        durable_job_id: String,
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
        #[serde(default = "conservative_remote_backup_required")]
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
    fn operation_id(&self) -> &str {
        match self {
            Self::SourcePrepared { operation_id, .. } => operation_id,
            Self::AwaitingConflict { context, .. } | Self::LocalCommitted { context, .. } => {
                &context.operation_id
            }
            Self::Completed { result, .. } => &result.operation_id,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        let schema = match self {
            Self::SourcePrepared { schema, .. }
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
            Self::SourcePrepared { .. } => None,
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
            Self::Completed { result, .. } => Some(PeerBidirectionalStatusOperation::Completed {
                result: result.clone(),
            }),
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
        && generation.manifest_hash.len() == 64
        && generation
            .manifest_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
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
    backup: Option<LanBidirectionalBackupReceipt>,
}

impl SourcePreparedEvidence {
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
            backup,
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
            backup: backup.clone(),
        })
    }

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
            backup: self.backup.clone(),
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
) -> Result<PeerBidirectionalBackupReceipt, PeerSyncError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(PeerSyncError::Storage(
            "bidirectional backup path is not a regular file".to_owned(),
        ));
    }
    let verified =
        verify_lossless_package_v1_for_production(&mut File::open(path)?, &NeverCancelled)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
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

fn ensure_bidirectional_backup_receipt(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    operation_id: &str,
    expected_revision: i64,
    side: PeerBidirectionalBackupSide,
    expected_source: &SyncGenerationIdentity,
) -> Result<PeerBidirectionalBackupReceipt, PeerSyncError> {
    let side_name = match side {
        PeerBidirectionalBackupSide::Local => "local",
        PeerBidirectionalBackupSide::Remote => "remote",
    };
    let backup_path = bidirectional_backup_path(app_root, operation_id, side.clone());
    let backup_root = backup_path.parent().ok_or_else(|| {
        PeerSyncError::Storage("bidirectional backup path has no parent".to_owned())
    })?;
    fs::create_dir_all(&backup_root)?;
    match fs::symlink_metadata(&backup_path) {
        Ok(_) => {
            return verify_bidirectional_backup_receipt(
                &backup_path,
                operation_id,
                expected_revision,
                side,
                expected_source,
                None,
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let backup_staging = app_root
        .join("peer-bidirectional")
        .join("backup-staging")
        .join(format!("{operation_id}-{side_name}"));
    fs::create_dir_all(&backup_staging)?;
    let source_binding = lossless_source_binding(operation_id, side, expected_source);
    match create_and_verify_peer_bidirectional_backup_v1_report(
        &backup_path,
        &backup_staging,
        cas,
        store,
        expected_revision,
        &source_binding,
        &NeverCancelled,
    ) {
        Ok(report) => Ok(PeerBidirectionalBackupReceipt {
            package_id: report.archive_sha256,
            side,
            path: backup_path.to_string_lossy().into_owned(),
        }),
        Err(error) => Err(PeerSyncError::Storage(error.to_string())),
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
    LocalRevision,
    RemoteGeneration,
    CommonBase,
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

#[allow(clippy::too_many_arguments)]
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

fn begin_bidirectional_local_merge<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    credential: LanBidirectionalLogicalCredential,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<LocalMergeOutcome, PeerSyncError> {
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
    let operation_id = uuid::Uuid::new_v4().to_string();
    let durable_job_id = uuid::Uuid::new_v4().to_string();
    let job = RefCell::new(DurableCasJob::begin(
        app_root,
        &durable_job_id,
        CasJobKind::LogicalDeltaTarget,
        now_millis()?,
    )?);
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
    };
    let mut target = match PersistentLogicalDeltaTarget::new_p5_deferred_with_durable_job(
        store,
        cas,
        &context.credential.source_device_id,
        PRODUCT_LOGICAL_LIBRARY_ID,
        &local.manifest.generation,
        remote_manifest_bytes,
        &app_root.join("peer-bidirectional").join("staging"),
        &job,
        LogicalDeltaConflictPolicy::Reject,
    ) {
        Ok(target) => target,
        Err(error) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            return Err(error);
        }
    };
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
            if let Err(error) = journal.store(&durable) {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
                return Err(error);
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
            let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
            let activation = execute_logical_delta_pull(
                &plan,
                &local_hashes,
                cas,
                &remote_sizes,
                &mut measured_source,
                &mut target,
            );
            let (transferred_objects, transferred_bytes) = measured_source.totals();
            drop(target);
            retain_bidirectional_local_activation(
                store,
                cas,
                app_root,
                context,
                &job,
                expected_revision,
                activation,
                changed,
                false,
                transferred_objects,
                transferred_bytes,
                vec![],
            )
        }
        Err(error) => {
            drop(target);
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
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
    if winner == PeerBidirectionalConflictWinner::Remote {
        if let Some(receipt) = backups
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
            verify_bidirectional_backup_receipt(
                &expected_path,
                operation_id,
                context.expected_local_revision,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
                Some(&receipt.package_id),
            )?;
        } else {
            backups.push(ensure_bidirectional_backup_receipt(
                store,
                cas,
                app_root,
                operation_id,
                context.expected_local_revision,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
            )?);
            journal.store(&PeerBidirectionalDurableOperation::AwaitingConflict {
                schema: OPERATION_SCHEMA.to_owned(),
                context: context.clone(),
                conflicts: conflicts.clone(),
                local_generation: local_generation.clone(),
                local_manifest_hash: local_manifest_hash.clone(),
                remote_manifest_hash: remote_manifest_hash.clone(),
                backups: backups.clone(),
            })?;
        }
    }
    let job = RefCell::new(DurableCasJob::open(app_root, &context.durable_job_id)?);
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
    let activation = execute_logical_delta_pull(
        &plan,
        &BTreeSet::new(),
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    drop(target);
    let original_local_revision = context.expected_local_revision;
    retain_bidirectional_local_activation(
        store,
        cas,
        app_root,
        context,
        &job,
        original_local_revision,
        activation,
        changed || recovered_after_activation,
        winner == PeerBidirectionalConflictWinner::Local,
        transferred_objects,
        transferred_bytes,
        backups,
    )
}

#[allow(clippy::too_many_arguments)]
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
    )
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
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
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
                let backup = if backup_losing_side {
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
                    )?;
                    Some(LanBidirectionalBackupReceipt {
                        package_id: receipt.package_id,
                        path: receipt.path,
                    })
                } else {
                    None
                };
                return Ok(LanBidirectionalRemoteApplyReceipt {
                    committed_revision: actual_revision,
                    committed_generation: expected_shared_generation,
                    transferred_objects: 0,
                    transferred_bytes: 0,
                    backup,
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
    let backup = if backup_losing_side {
        let receipt = ensure_bidirectional_backup_receipt(
            store,
            cas,
            app_root,
            operation_id,
            expected_revision,
            PeerBidirectionalBackupSide::Remote,
            expected_losing_generation,
        )?;
        Some(LanBidirectionalBackupReceipt {
            package_id: receipt.package_id,
            path: receipt.path,
        })
    } else {
        None
    };
    let (durable_job_id, stage_with_durable_job, durable_job) =
        if let Some(retained_job_id) = retained_source_job_id {
            match DurableCasJob::open(app_root, retained_job_id) {
                Ok(job) => (retained_job_id.to_owned(), !job.is_sealed(), job),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    retained_job_id.to_owned(),
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
            let durable_job_id = uuid::Uuid::new_v4().to_string();
            let job = DurableCasJob::begin(
                app_root,
                &durable_job_id,
                CasJobKind::LogicalDeltaTarget,
                now_millis()?,
            )?;
            (durable_job_id, true, job)
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
    let staging_root = app_root.join("peer-bidirectional").join("staging");
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
    let plan = match target.resolve_authoritative_plan(expected_revision) {
        Ok(LogicalDeltaPlanResolution::Ready { plan, .. }) => plan,
        Ok(LogicalDeltaPlanResolution::Conflict { .. }) => {
            drop(target);
            if retained_source.is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            return Err(PeerSyncError::Protocol(
                "bidirectional remote winner did not resolve its conflicts".to_owned(),
            ));
        }
        Err(error) => {
            drop(target);
            if retained_source.is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            return Err(error);
        }
    };
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
    let mut measured_source = MeasuredLogicalDeltaSource::new(shared_source);
    let prepared_evidence = RefCell::new(None);
    let activation = execute_logical_delta_pull_with_pre_activation(
        &plan,
        &local_hashes,
        cas,
        &shared_sizes,
        &mut measured_source,
        &mut target,
        |selection| {
            let Some(source_device_id) = source_device_id else {
                return Ok(());
            };
            let transferred_objects =
                u64::try_from(selection.missing_objects().len()).map_err(|_| {
                    PeerSyncError::Validation(
                        "bidirectional transfer object count overflow".to_owned(),
                    )
                })?;
            let transferred_bytes =
                selection
                    .missing_objects()
                    .iter()
                    .try_fold(0_u64, |total, object| {
                        total.checked_add(object.size).ok_or_else(|| {
                            PeerSyncError::Validation(
                                "bidirectional transfer byte count overflow".to_owned(),
                            )
                        })
                    })?;
            let evidence = retained_source.cloned().unwrap_or(SourcePreparedEvidence {
                operation_id: operation_id.to_owned(),
                source_device_id: source_device_id.to_owned(),
                target_device_id: peer_id.to_owned(),
                expected_source_revision: expected_revision,
                previous_shared: previous.shared_identity.clone(),
                expected_source_generation: expected_losing_generation.clone(),
                shared_generation: expected_shared_generation.clone(),
                incoming_revision: i64::try_from(shared_manifest.source_revision).map_err(
                    |_| {
                        PeerSyncError::Validation(
                            "bidirectional shared revision exceeds SQLite range".to_owned(),
                        )
                    },
                )?,
                transferred_objects,
                transferred_bytes,
                backup: backup.clone(),
            });
            if evidence.operation_id != operation_id
                || evidence.source_device_id != source_device_id
                || evidence.target_device_id != peer_id
                || evidence.expected_source_revision != expected_revision
                || evidence.previous_shared != previous.shared_identity
                || evidence.expected_source_generation != *expected_losing_generation
                || evidence.shared_generation != expected_shared_generation
                || evidence.incoming_revision
                    != i64::try_from(shared_manifest.source_revision).map_err(|_| {
                        PeerSyncError::Validation(
                            "bidirectional shared revision exceeds SQLite range".to_owned(),
                        )
                    })?
                || evidence.backup != backup
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
                if SOURCE_AFTER_PREPARED_STORE_PANIC.with(|enabled| enabled.replace(false)) {
                    panic!("simulated process loss after source-prepared journal store");
                }
            }
            prepared_evidence.replace(Some(evidence));
            #[cfg(test)]
            if SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.replace(false)) {
                return Err(PeerSyncError::Storage(
                    "simulated source pre-activation failure".to_owned(),
                ));
            }
            Ok(())
        },
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    drop(target);
    let committed_revision = match activation {
        Ok(LogicalDeltaActivation::Activated { revision })
        | Ok(LogicalDeltaActivation::AlreadyActive { revision }) => revision,
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision, ..
        }) => {
            if prepared_evidence.borrow().is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            return Err(PeerSyncError::ActivationConflict {
                expected: Some(expected_revision.to_string()),
                actual: Some(actual_revision.to_string()),
            });
        }
        Err(error) => {
            if prepared_evidence.borrow().is_none() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
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
    let Some(evidence) = prepared_evidence.into_inner() else {
        return Ok(measured_receipt);
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
    Ok(receipt)
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
        context,
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
    if let Some(backup) = remote.backup {
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
        backups,
    };
    journal.store(&PeerBidirectionalDurableOperation::Completed {
        schema: OPERATION_SCHEMA.to_owned(),
        remote_apply_receipt: None,
        source_binding: None,
        result: result.clone(),
    })?;
    Ok(result)
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
        Err(PeerSyncError::ActivationConflict { .. } | PeerSyncError::StaleManifest { .. }) => {
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
    LegacyCompleted(LanBidirectionalRemoteApplyReceipt),
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
        .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
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
            if binding.source_device_id != session.source_device_id
                || binding.target_device_id != session.target_device_id
                || binding.operation_id != request.operation_id
                || binding.expected_source_revision != request.expected_source_revision
                || binding.expected_source_generation
                    != (SyncGenerationIdentity {
                        generation_id: request.expected_source_generation.generation_id.clone(),
                        manifest_hash: request.expected_source_generation.manifest_hash.clone(),
                        generation_sequence: request
                            .expected_source_generation
                            .generation_sequence
                            .clone(),
                    })
                || binding.previous_shared.manifest_hash
                    != request.expected_common_base_manifest_hash
            {
                return Err(PeerSyncError::Validation(
                    "bidirectional completed source retry differs from its binding".to_owned(),
                ));
            }
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
            Ok(SourceOperationReconcile::LegacyCompleted(receipt))
        }
        PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            source_binding: None,
            ..
        } => {
            let shared = SyncGenerationIdentity {
                generation_id: receipt.committed_generation.generation_id.clone(),
                manifest_hash: receipt.committed_generation.manifest_hash.clone(),
                generation_sequence: receipt.committed_generation.generation_sequence.clone(),
            };
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
                .sync_device_common_base_identity(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    &session.target_device_id,
                )
                .map_err(store_error)?;
            let acknowledgement = store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, &session.target_device_id)
                .map_err(store_error)?;
            if actual_revision != receipt.committed_revision
                || common.as_ref() != Some(&shared)
                || acknowledgement.shared_identity != shared
                || acknowledgement.local_identity != active_identity
            {
                return Err(PeerSyncError::Validation(
                    "legacy bidirectional source completion does not match durable source state"
                        .to_owned(),
                ));
            }
            Ok(SourceOperationReconcile::LegacyCompleted(receipt))
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
            if evidence.source_device_id != session.source_device_id
                || evidence.target_device_id != session.target_device_id
                || evidence.operation_id != request.operation_id
                || evidence.expected_source_revision != request.expected_source_revision
                || evidence.expected_source_generation
                    != (SyncGenerationIdentity {
                        generation_id: request.expected_source_generation.generation_id.clone(),
                        manifest_hash: request.expected_source_generation.manifest_hash.clone(),
                        generation_sequence: request
                            .expected_source_generation
                            .generation_sequence
                            .clone(),
                    })
                || evidence.previous_shared.manifest_hash
                    != request.expected_common_base_manifest_hash
            {
                return Err(PeerSyncError::Validation(
                    "bidirectional source retry differs from retained evidence".to_owned(),
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
            let common = store
                .sync_device_common_base_identity(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    &session.target_device_id,
                )
                .map_err(store_error)?;
            let acknowledgement = store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, &session.target_device_id)
                .map_err(store_error)?;
            if actual_revision == evidence.expected_source_revision
                && active_identity == evidence.expected_source_generation
                && common.as_ref() == Some(&evidence.previous_shared)
                && acknowledgement.shared_identity == evidence.previous_shared
                && acknowledgement.local_identity == evidence.expected_source_generation
            {
                return Ok(SourceOperationReconcile::Retry(evidence, durable_job_id));
            }
            let precommit_descendant = actual_revision > evidence.expected_source_revision
                && common.as_ref() == Some(&evidence.previous_shared)
                && acknowledgement.shared_identity == evidence.previous_shared
                && acknowledgement.local_identity == evidence.expected_source_generation
                && store
                    .logical_generation_descends_from(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        &active_identity.generation_id,
                        &evidence.expected_source_generation.generation_id,
                    )
                    .map_err(store_error)?;
            if precommit_descendant {
                PeerBidirectionalOperationJournal::new(app_root).abandon(&evidence.operation_id)?;
                return Err(PeerSyncError::StaleManifest {
                    expected: evidence.expected_source_generation.manifest_hash,
                    received: active_identity.manifest_hash,
                });
            }
            let shared = SyncGenerationIdentity {
                generation_id: evidence.shared_generation.generation_id.clone(),
                manifest_hash: evidence.shared_generation.manifest_hash.clone(),
                generation_sequence: evidence.shared_generation.generation_sequence.clone(),
            };
            let (committed_revision, witnessed_local) =
                verified_source_acknowledgement(store, &session.target_device_id, &shared)?;
            let active_descends_from_ack = active_identity == witnessed_local
                || store
                    .logical_generation_descends_from(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        &active_identity.generation_id,
                        &witnessed_local.generation_id,
                    )
                    .map_err(store_error)?;
            if active_descends_from_ack {
                let receipt = evidence.receipt_at(committed_revision)?;
                if actual_revision < receipt.committed_revision {
                    return Err(PeerSyncError::ActivationConflict {
                        expected: Some(receipt.committed_revision.to_string()),
                        actual: Some(actual_revision.to_string()),
                    });
                }
                return Ok(SourceOperationReconcile::Completed(
                    evidence,
                    receipt,
                    actual_revision,
                    durable_job_id,
                ));
            }
            Err(PeerSyncError::ActivationConflict {
                expected: Some(format!(
                    "source revision {} at previous base or revision {} at shared base",
                    evidence.expected_source_revision,
                    evidence.expected_source_revision.saturating_add(1)
                )),
                actual: Some(actual_revision.to_string()),
            })
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
            SourceOperationReconcile::LegacyCompleted(receipt) => return Ok(receipt),
            SourceOperationReconcile::New => (None, None),
        };
        let mut client = LanLogicalDeltaClient::claim(
            &request.source_endpoint,
            &request.source_session_id,
            &request.source_manifest_id,
            &request.source_claim,
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
        let receipt = apply_bidirectional_remote_shared_inner(
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
        store_source_completed(
            &self.app_root,
            evidence,
            &receipt,
            receipt.committed_revision,
        )?;
        Ok(receipt)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerBidirectionalSourcePhase {
    Idle,
    Prepared,
    Running,
    Stopped,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalSourceDevice {
    device_id: String,
    transferred_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_object: Option<String>,
    last_seen_at: u64,
    revoked: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalSourceStatus {
    phase: PeerBidirectionalSourcePhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pairing_uri: Option<String>,
    devices: Vec<PeerBidirectionalSourceDevice>,
}

impl PeerBidirectionalSourceStatus {
    fn idle(phase: PeerBidirectionalSourcePhase) -> Self {
        Self {
            phase,
            session_id: None,
            manifest_id: None,
            pairing_uri: None,
            devices: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalStatus {
    source: PeerBidirectionalSourceStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<PeerBidirectionalStatusOperation>,
}

struct BidirectionalSourceRuntime {
    session_id: String,
    manifest_id: String,
    pairing_uri: Option<String>,
    host: LanCloneHost,
    control: LanCloneHostControl,
    phase: PeerBidirectionalSourcePhase,
}

#[derive(Default)]
struct PeerBidirectionalRuntime {
    source_preparing: bool,
    source: Option<BidirectionalSourceRuntime>,
    stopped: bool,
    target_active: bool,
}

#[derive(Clone, Default)]
pub(crate) struct PeerBidirectionalCommandState {
    runtime: Arc<Mutex<PeerBidirectionalRuntime>>,
}

impl PeerBidirectionalCommandState {
    fn lock(&self) -> Result<MutexGuard<'_, PeerBidirectionalRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!(
                "peer bidirectional command state mutex poisoned: {error}"
            ))
        })
    }

    fn begin_source_prepare(&self) -> Result<PeerBidirectionalStateGuard, PeerSyncError> {
        let mut runtime = self.lock()?;
        if runtime.target_active {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional target operation is active".to_owned(),
            ));
        }
        if runtime.source_preparing || runtime.source.is_some() {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional source preparation is active".to_owned(),
            ));
        }
        runtime.source_preparing = true;
        Ok(PeerBidirectionalStateGuard {
            state: self.clone(),
            kind: PeerBidirectionalGuardKind::Source,
        })
    }

    fn begin_target(&self) -> Result<PeerBidirectionalStateGuard, PeerSyncError> {
        let mut runtime = self.lock()?;
        if runtime.source_preparing || runtime.source.is_some() {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional source operation is active".to_owned(),
            ));
        }
        if runtime.target_active {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional target operation is active".to_owned(),
            ));
        }
        runtime.target_active = true;
        Ok(PeerBidirectionalStateGuard {
            state: self.clone(),
            kind: PeerBidirectionalGuardKind::Target,
        })
    }

    fn install_source(
        &self,
        host: LanCloneHost,
        session_id: &str,
        manifest_id: &str,
    ) -> Result<PeerBidirectionalSourceStatus, PeerSyncError> {
        let control = host.control();
        let mut runtime = self.lock()?;
        if runtime.source.is_some() {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional source is already prepared".to_owned(),
            ));
        }
        runtime.source_preparing = false;
        runtime.stopped = false;
        runtime.source = Some(BidirectionalSourceRuntime {
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            pairing_uri: None,
            host,
            control,
            phase: PeerBidirectionalSourcePhase::Prepared,
        });
        Ok(source_status(&runtime, &[]))
    }

    fn start_source(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerBidirectionalSourceStatus, PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        if source.phase != PeerBidirectionalSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer bidirectional source is not prepared".to_owned(),
            ));
        }
        let pairing = source.host.start()?;
        let address = source.host.address().ok_or_else(|| {
            PeerSyncError::Transport("peer bidirectional source address is unavailable".to_owned())
        })?;
        source.pairing_uri = Some(build_pairing_uri(
            &format!("http://{advertised_ip}:{}", address.port()),
            &pairing,
        )?);
        source.phase = PeerBidirectionalSourcePhase::Running;
        Ok(source_status(&runtime, &[]))
    }

    fn stop_source(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.host.stop()?;
        runtime.source = None;
        runtime.stopped = true;
        Ok(())
    }

    fn revoke_source_device(&self, session_id: &str, device_id: &str) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let Some(source) = runtime.source.as_mut() else {
            return Ok(());
        };
        if source.session_id != session_id {
            return Err(PeerSyncError::Validation(
                "peer bidirectional source session is absent".to_owned(),
            ));
        }
        source.control.revoke(device_id);
        Ok(())
    }

    fn status(
        &self,
        app_root: &Path,
        store: &PersistentStore,
    ) -> Result<PeerBidirectionalStatus, PeerSyncError> {
        let durable_devices = store
            .list_sync_devices(PRODUCT_LOGICAL_LIBRARY_ID)
            .map_err(store_error)?;
        let source = {
            let runtime = self.lock()?;
            source_status(&runtime, &durable_devices)
        };
        let operation = PeerBidirectionalOperationJournal::new(app_root)
            .load()?
            .and_then(|operation| operation.status_projection());
        Ok(PeerBidirectionalStatus { source, operation })
    }

    fn source_is_active(&self) -> Result<bool, PeerSyncError> {
        Ok(self.lock()?.source.is_some())
    }

    fn acknowledge(
        &self,
        app_root: &Path,
        store: &mut PersistentStore,
        operation_id: &str,
    ) -> Result<(), PeerSyncError> {
        let journal = PeerBidirectionalOperationJournal::new(app_root);
        if self.source_is_active()? {
            let retained = journal.load()?;
            if retained
                .as_ref()
                .is_some_and(|operation| operation.operation_id() == operation_id)
            {
                return Err(PeerSyncError::Protocol(
                    "peer bidirectional source must be stopped before acknowledgement".to_owned(),
                ));
            }
        }
        let Some(operation) = journal.load()? else {
            return Ok(());
        };
        if operation.operation_id() != operation_id {
            return Err(PeerSyncError::Validation(
                "another bidirectional operation is retained".to_owned(),
            ));
        }
        match operation {
            PeerBidirectionalDurableOperation::SourcePrepared {
                target_device_id,
                expected_source_revision,
                previous_shared,
                expected_source_generation,
                ..
            } => {
                let cas = PayloadCas::new(app_root)?;
                let active = store
                    .seal_or_initialize_active_logical_generation(&cas)
                    .map_err(store_error)?;
                let active_identity = SyncGenerationIdentity {
                    generation_id: active.manifest.generation,
                    manifest_hash: active.manifest_hash,
                    generation_sequence: active.manifest.generation_sequence,
                };
                let common = store
                    .sync_device_common_base_identity(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        &target_device_id,
                    )
                    .map_err(store_error)?;
                let acknowledgement = store
                    .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, &target_device_id)
                    .map_err(store_error)?;
                if store.revision().map_err(store_error)? != expected_source_revision
                    || active_identity != expected_source_generation
                    || common.as_ref() != Some(&previous_shared)
                    || acknowledgement.shared_identity != previous_shared
                    || acknowledgement.local_identity != expected_source_generation
                {
                    return Err(PeerSyncError::Validation(
                        "source-prepared operation cannot be abandoned after source commit"
                            .to_owned(),
                    ));
                }
                journal.abandon(operation_id)
            }
            PeerBidirectionalDurableOperation::LocalCommitted {
                remote_apply_receipt: Some(receipt),
                ..
            } => {
                complete_bidirectional_local_after_remote_apply(
                    store,
                    &PayloadCas::new(app_root)?,
                    app_root,
                    operation_id,
                    receipt,
                )?;
                journal.acknowledge_completed(operation_id)
            }
            PeerBidirectionalDurableOperation::LocalCommitted { .. } => journal.abandon(operation_id),
            PeerBidirectionalDurableOperation::Completed { .. } => {
                journal.acknowledge_completed(operation_id)
            }
            PeerBidirectionalDurableOperation::AwaitingConflict { .. } => {
                Err(PeerSyncError::Validation(
                    "only a completed or source-unavailable bidirectional operation can be acknowledged"
                        .to_owned(),
                ))
            }
        }
    }
}

fn require_source<'a>(
    runtime: &'a mut PeerBidirectionalRuntime,
    session_id: &str,
) -> Result<&'a mut BidirectionalSourceRuntime, PeerSyncError> {
    runtime
        .source
        .as_mut()
        .filter(|source| source.session_id == session_id)
        .ok_or_else(|| {
            PeerSyncError::Validation("peer bidirectional source session is absent".to_owned())
        })
}

fn source_status(
    runtime: &PeerBidirectionalRuntime,
    durable_devices: &[RegisteredSyncDevice],
) -> PeerBidirectionalSourceStatus {
    let mut devices = durable_devices
        .iter()
        .filter(|device| device.status != RegisteredSyncDeviceStatus::Forgotten)
        .map(|device| {
            let last_seen_at = device
                .revoked_at
                .unwrap_or(device.acknowledged_at)
                .max(device.acknowledged_at)
                .max(device.registered_at);
            (
                device.device_id.clone(),
                PeerBidirectionalSourceDevice {
                    device_id: device.device_id.clone(),
                    transferred_bytes: 0,
                    current_object: None,
                    last_seen_at: u64::try_from(last_seen_at).unwrap_or(u64::MAX),
                    revoked: device.status != RegisteredSyncDeviceStatus::Active,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let Some(source) = &runtime.source else {
        let mut status = PeerBidirectionalSourceStatus::idle(if runtime.stopped {
            PeerBidirectionalSourcePhase::Stopped
        } else {
            PeerBidirectionalSourcePhase::Idle
        });
        status.devices = devices.into_values().collect();
        return status;
    };
    for device in source.control.devices() {
        let durable = devices.get(&device.device_id);
        let last_seen_at = u64::try_from(device.last_seen_unix_ms).unwrap_or(u64::MAX);
        devices.insert(
            device.device_id.clone(),
            PeerBidirectionalSourceDevice {
                device_id: device.device_id,
                transferred_bytes: device.verified_bytes,
                current_object: device.current_object,
                last_seen_at: durable
                    .map(|durable| durable.last_seen_at.max(last_seen_at))
                    .unwrap_or(last_seen_at),
                revoked: device.revoked || durable.is_some_and(|durable| durable.revoked),
            },
        );
    }
    PeerBidirectionalSourceStatus {
        phase: source.phase.clone(),
        session_id: Some(source.session_id.clone()),
        manifest_id: Some(source.manifest_id.clone()),
        pairing_uri: source.pairing_uri.clone(),
        devices: devices.into_values().collect(),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerBidirectionalCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    authenticated_transport_ready: bool,
    lossless_backup_ready: bool,
    durable_state_ready: bool,
    production_enabled: bool,
}

#[tauri::command]
pub fn peer_bidirectional_capabilities() -> PeerBidirectionalCapabilities {
    PeerBidirectionalCapabilities {
        desktop: true,
        source_ready: true,
        atomic_activation_ready: true,
        authenticated_transport_ready: true,
        lossless_backup_ready: true,
        durable_state_ready: true,
        production_enabled: true,
    }
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, PeerSyncError> {
    let mut uri = url::Url::parse("risuailocal://peer-sync/v1")
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    uri.query_pairs_mut()
        .append_pair("endpoint", endpoint)
        .append_pair("session", &pairing.session_id)
        .append_pair("manifest", &pairing.manifest_id);
    uri.set_fragment(Some(&format!("claim={}", pairing.claim)));
    Ok(uri.to_string())
}

fn discover_lan_ipv4() -> Result<Ipv4Addr, PeerSyncError> {
    #[cfg(test)]
    if let Some(address) = DISCOVER_LAN_IPV4_OVERRIDE.with(Cell::take) {
        return Ok(address);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    match socket.local_addr()?.ip() {
        std::net::IpAddr::V4(address) if address.is_private() || address.is_link_local() => {
            Ok(address)
        }
        _ => Err(PeerSyncError::Validation(
            "no private IPv4 LAN address is available".to_owned(),
        )),
    }
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

fn open_command_store(app: &AppHandle) -> Result<PersistentStore, PeerSyncError> {
    persistent_store::commands::with_store_mut(app.state(), |store| store.open_native_job_store())
        .map_err(store_error)
}

fn prepare_product_source_host(
    app_root: &Path,
    store: PersistentStore,
    source: LogicalDeltaSourceSession,
    source_device_id: &str,
    manifest_bytes: Vec<u8>,
) -> Result<(LanCloneHost, String, String), PeerSyncError> {
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
    Ok((
        LanCloneHost::prepare_bidirectional_logical(prepared),
        session_id,
        manifest_id,
    ))
}

fn request_remote_apply_from_shared(
    app_root: &Path,
    local_device_id: &str,
    client: &LanBidirectionalLogicalClient,
    context: &PeerBidirectionalOperationContext,
    shared_generation: &SyncGenerationIdentity,
    shared_manifest_bytes: &[u8],
    backup_losing_side: bool,
) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
    let source = LogicalDeltaSourceSession::open(
        app_root,
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
    let pairing = host.start()?;
    let address = host.address().ok_or_else(|| {
        PeerSyncError::Transport("temporary shared source address is unavailable".to_owned())
    })?;
    let endpoint = format!("http://{}:{}", discover_lan_ipv4()?, address.port());
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
    });
    let stop = host.stop();
    match (result, stop) {
        (Ok(receipt), Ok(())) => Ok(receipt),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
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
        PeerBidirectionalDurableOperation::SourcePrepared { .. } => {
            return Err(PeerSyncError::Validation(
                "source-prepared operation cannot resume as a target".to_owned(),
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
    let local_device_id = super::delta_commands::load_or_create_source_device_id(
        &app_root.join("peer-delta").join("source-device-id"),
    )?;
    let outcome = resume_bidirectional_local_committed_with_remote(
        store,
        &cas,
        app_root,
        operation_id,
        |context, _, shared, manifest, backup_losing_side| {
            if let Some(client) = client {
                request_remote_apply_from_shared(
                    app_root,
                    &local_device_id,
                    client,
                    context,
                    shared,
                    manifest,
                    backup_losing_side,
                )
            } else {
                let client = LanBidirectionalLogicalClient::resume(context.credential.clone())?;
                request_remote_apply_from_shared(
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
pub async fn peer_bidirectional_prepare(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    expected_revision: i64,
) -> Result<PeerBidirectionalSourceStatus, String> {
    let state = state.inner().clone();
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state
            .begin_source_prepare()
            .map_err(|error| error.to_string())?;
        let retained = PeerBidirectionalOperationJournal::new(&root)
            .load()
            .map_err(|error| error.to_string())?;
        let source_device_id = super::delta_commands::load_or_create_source_device_id(
            &root.join("peer-delta").join("source-device-id"),
        )
        .map_err(|error| error.to_string())?;
        if retained
            .as_ref()
            .is_some_and(|operation| !retained_allows_source_prepare(operation, &source_device_id))
        {
            return Err("a target-owned bidirectional operation is retained".to_owned());
        }
        let cas = PayloadCas::new(&root).map_err(|error| error.to_string())?;
        let (built, store) = persistent_store::commands::with_store_mut(app.state(), |store| {
            let actual = store.revision()?;
            if actual != expected_revision {
                return Err(StoreError::RevisionConflict {
                    expected: expected_revision,
                    actual,
                });
            }
            let built = store.seal_or_initialize_active_logical_generation(&cas)?;
            let job_store = store.open_native_job_store()?;
            Ok((built, job_store))
        })
        .map_err(|error| error.to_string())?;
        let source = LogicalDeltaSourceSession::open(
            &root,
            &root,
            &built.manifest.library_id,
            &built.manifest.generation,
        )
        .map_err(|error| error.to_string())?;
        let (host, session_id, manifest_id) = prepare_product_source_host(
            &root,
            store,
            source,
            &source_device_id,
            built.manifest_bytes,
        )
        .map_err(|error| error.to_string())?;
        state
            .install_source(host, &session_id, &manifest_id)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer bidirectional source preparation worker failed: {error}"))?
}

#[tauri::command]
pub fn peer_bidirectional_start(
    state: State<'_, PeerBidirectionalCommandState>,
    session_id: String,
) -> Result<PeerBidirectionalSourceStatus, String> {
    state
        .start_source(
            &session_id,
            discover_lan_ipv4().map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_bidirectional_status(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
) -> Result<PeerBidirectionalStatus, String> {
    let store = open_command_store(&app).map_err(|error| error.to_string())?;
    state
        .status(&app_root(&app)?, &store)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_bidirectional_stop(
    state: State<'_, PeerBidirectionalCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|error| format!("peer bidirectional source stop worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_bidirectional_revoke(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    state
        .revoke_source_device(&session_id, &device_id)
        .map_err(|error| error.to_string())?;
    let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
    revoke_durable_bidirectional_device(&mut store, &device_id).map_err(|error| error.to_string())
}

fn revoke_durable_bidirectional_device(
    store: &mut PersistentStore,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    let device = store
        .list_sync_devices(PRODUCT_LOGICAL_LIBRARY_ID)
        .map_err(store_error)?
        .into_iter()
        .find(|device| device.device_id == device_id);
    match device.map(|device| device.status) {
        Some(RegisteredSyncDeviceStatus::Active) => {
            let acknowledgement = store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, &device_id)
                .map_err(store_error)?;
            store
                .revoke_sync_device(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    device_id,
                    &acknowledgement.shared_identity,
                )
                .map(|_| ())
                .map_err(store_error)
        }
        Some(RegisteredSyncDeviceStatus::Revoked | RegisteredSyncDeviceStatus::Forgotten)
        | None => Ok(()),
    }
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn peer_bidirectional_sync(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
    expected_revision: i64,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_target().map_err(|error| error.to_string())?;
        if let Some(retained) = PeerBidirectionalOperationJournal::new(&root)
            .load()
            .map_err(|error| error.to_string())?
        {
            let PeerBidirectionalDurableOperation::LocalCommitted {
                context,
                remote_apply_receipt,
                ..
            } = &retained
            else {
                return retained_result(&retained).map_err(|error| error.to_string());
            };
            if remote_apply_receipt.is_some() {
                let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
                return run_retained_remote_completion(
                    &mut store,
                    &root,
                    &context.operation_id,
                    expected_revision,
                    None,
                )
                .map_err(|error| error.to_string());
            }
            let local_device_id = super::delta_commands::load_or_create_source_device_id(
                &root.join("peer-delta").join("source-device-id"),
            )
            .map_err(|error| error.to_string())?;
            if context.credential.device_id != local_device_id {
                return Err(
                    "retained bidirectional operation belongs to another target device".to_owned(),
                );
            }
            let client = LanBidirectionalLogicalClient::claim(
                &endpoint,
                &session_id,
                &manifest_id,
                &claim,
                &local_device_id,
            )
            .map_err(|error| error.to_string())?;
            if client.source_device_id() != context.credential.source_device_id {
                return Err("fresh bidirectional source belongs to another device".to_owned());
            }
            let manifest = client.fetch_manifest().map_err(|error| error.to_string())?;
            let decoded = decode_logical_manifest(&manifest).map_err(|error| error.to_string())?;
            if decoded.library_id != context.library_id {
                return Err("fresh bidirectional source belongs to another library".to_owned());
            }
            let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
            return run_retained_remote_completion(
                &mut store,
                &root,
                &context.operation_id,
                expected_revision,
                Some(&client),
            )
            .map_err(|error| error.to_string());
        }
        let local_device_id = super::delta_commands::load_or_create_source_device_id(
            &root.join("peer-delta").join("source-device-id"),
        )
        .map_err(|error| error.to_string())?;
        let mut client = LanBidirectionalLogicalClient::claim(
            &endpoint,
            &session_id,
            &manifest_id,
            &claim,
            &local_device_id,
        )
        .map_err(|error| error.to_string())?;
        let remote_manifest_bytes = client.fetch_manifest().map_err(|error| error.to_string())?;
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
            .map_err(|error| error.to_string())?;
        let outcome = begin_bidirectional_local_merge(
            &mut store,
            &PayloadCas::new(&root).map_err(|error| error.to_string())?,
            &root,
            client.credential(),
            expected_revision,
            &remote_manifest_bytes,
            &mut client,
        )
        .map_err(|error| error.to_string())?;
        match outcome {
            LocalMergeOutcome::Conflict(result) => Ok(conflict_result(result)),
            LocalMergeOutcome::LocalCommitted => {
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
                .map_err(|error| error.to_string())
            }
        }
    })
    .await
    .map_err(|error| format!("peer bidirectional sync worker failed: {error}"))?
}

#[tauri::command]
pub async fn peer_bidirectional_resolve(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
    winner: PeerBidirectionalConflictWinner,
    expected_revision: i64,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_target().map_err(|error| error.to_string())?;
        let operation = PeerBidirectionalOperationJournal::new(&root)
            .load()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "bidirectional operation is not retained".to_owned())?;
        if operation.operation_id() != operation_id {
            return Err("another bidirectional operation is retained".to_owned());
        }
        let credential = match operation {
            PeerBidirectionalDurableOperation::AwaitingConflict { context, .. } => {
                context.credential
            }
            operation => return retained_result(&operation).map_err(|error| error.to_string()),
        };
        let mut client =
            LanBidirectionalLogicalClient::resume(credential).map_err(|error| error.to_string())?;
        let remote_manifest = client.fetch_manifest().map_err(|error| error.to_string())?;
        let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
        let outcome = resolve_bidirectional_conflict(
            &mut store,
            &PayloadCas::new(&root).map_err(|error| error.to_string())?,
            &root,
            &operation_id,
            winner,
            expected_revision,
            &remote_manifest,
            &mut client,
        )
        .map_err(|error| error.to_string())?;
        match outcome {
            LocalMergeOutcome::Conflict(result) => Ok(conflict_result(result)),
            LocalMergeOutcome::LocalCommitted => {
                let committed_revision = store.revision().map_err(|error| error.to_string())?;
                run_retained_remote_completion(
                    &mut store,
                    &root,
                    &operation_id,
                    committed_revision,
                    Some(&client),
                )
                .map_err(|error| error.to_string())
            }
        }
    })
    .await
    .map_err(|error| format!("peer bidirectional resolution worker failed: {error}"))?
}

#[tauri::command]
pub async fn peer_bidirectional_resume(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
    expected_revision: i64,
) -> Result<PeerBidirectionalSyncResult, String> {
    let state = state.inner().clone();
    let root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_target().map_err(|error| error.to_string())?;
        let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
        run_retained_remote_completion(&mut store, &root, &operation_id, expected_revision, None)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer bidirectional resume worker failed: {error}"))?
}

#[tauri::command]
pub fn peer_bidirectional_acknowledge(
    app: AppHandle,
    state: State<'_, PeerBidirectionalCommandState>,
    operation_id: String,
) -> Result<(), String> {
    let root = app_root(&app)?;
    let mut store = open_command_store(&app).map_err(|error| error.to_string())?;
    state
        .acknowledge(&root, &mut store, &operation_id)
        .map_err(|error| error.to_string())
}

enum PeerBidirectionalGuardKind {
    Source,
    Target,
}

struct PeerBidirectionalStateGuard {
    state: PeerBidirectionalCommandState,
    kind: PeerBidirectionalGuardKind,
}

impl Drop for PeerBidirectionalStateGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.state.runtime.lock() {
            match self.kind {
                PeerBidirectionalGuardKind::Source => runtime.source_preparing = false,
                PeerBidirectionalGuardKind::Target => runtime.target_active = false,
            }
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

impl PeerBidirectionalOperationJournal {
    pub(crate) fn new(app_root: &Path) -> Self {
        Self {
            root: app_root.join("peer-bidirectional"),
        }
    }

    pub(crate) fn load(&self) -> Result<Option<PeerBidirectionalDurableOperation>, PeerSyncError> {
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
        let operation = serde_json::from_slice::<PeerBidirectionalDurableOperation>(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        operation.validate()?;
        Ok(Some(operation))
    }

    pub(crate) fn store(
        &self,
        operation: &PeerBidirectionalDurableOperation,
    ) -> Result<(), PeerSyncError> {
        operation.validate()?;
        let bytes = serde_json::to_vec(operation)
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
        let durable_job_id = match &operation {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                Some(durable_job_id.as_str())
            }
            PeerBidirectionalDurableOperation::AwaitingConflict { context, .. }
            | PeerBidirectionalDurableOperation::LocalCommitted { context, .. } => {
                Some(context.durable_job_id.as_str())
            }
            PeerBidirectionalDurableOperation::Completed { .. } => None,
        };
        if let Some(durable_job_id) = durable_job_id {
            let repository_root = self.root.parent().ok_or_else(|| {
                PeerSyncError::Storage(
                    "bidirectional operation directory has no repository root".to_owned(),
                )
            })?;
            match DurableCasJob::open(repository_root, durable_job_id) {
                Ok(mut job) => job.release(CasReleaseOutcome::Aborted)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        match fs::remove_file(self.root.join(OPERATION_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asset_repository::{
            job_pins::{collect_durable_cas_job_roots, CasObjectRole},
            PayloadCas,
        },
        peer_sync::logical_delta::{
            build_logical_manifest, encode_logical_manifest, LogicalManifestBuilderInput,
            LogicalRecordEnvelope, LogicalRecordLocator, ProjectedLogicalRecord,
        },
        persistent_store::{
            establish_logical_common_base, logical_delta_source::LogicalDeltaSourceSession,
            AssetAlias, AssetRepositoryAuthorityState, ColdPayloadAuthorityState, PersistentStore,
            VerifiedSyncDeviceRegistration, WorkingSetCommit, PRODUCT_LOGICAL_LIBRARY_ID,
        },
    };
    use serde_json::json;
    use std::{
        collections::BTreeMap,
        io::Cursor,
        sync::{mpsc, Arc, Mutex},
        thread,
    };

    struct FixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        reads: usize,
    }

    struct BlockingFixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        opened: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl super::super::LogicalDeltaObjectSource for BlockingFixtureSource {
        fn open_object(
            &mut self,
            object: &super::super::LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.opened.send(()).unwrap();
            self.release.recv().unwrap();
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    impl super::super::LogicalDeltaObjectSource for FixtureSource {
        fn open_object(
            &mut self,
            object: &super::super::LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.reads += 1;
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    fn generation(id: &str, sequence: &str, hash: char) -> SyncGenerationIdentity {
        SyncGenerationIdentity {
            generation_id: id.to_owned(),
            manifest_hash: hash.to_string().repeat(64),
            generation_sequence: sequence.to_owned(),
        }
    }

    fn context(operation_id: &str) -> PeerBidirectionalOperationContext {
        let previous = generation("shared-c", "1", 'a');
        PeerBidirectionalOperationContext {
            operation_id: operation_id.to_owned(),
            credential: LanBidirectionalLogicalCredential {
                endpoint: "http://192.168.1.2:32146".to_owned(),
                session_id: "123e4567-e89b-42d3-a456-426614174000".to_owned(),
                manifest_id: "b".repeat(64),
                device_id: "123e4567-e89b-42d3-a456-426614174001".to_owned(),
                source_device_id: "123e4567-e89b-42d3-a456-426614174002".to_owned(),
                bearer: "c".repeat(64),
            },
            library_id: "risunest-product".to_owned(),
            expected_local_revision: 7,
            expected_remote_revision: 7,
            expected_remote_generation: LanBidirectionalGeneration {
                generation_id: "remote-r".to_owned(),
                manifest_hash: "d".repeat(64),
                generation_sequence: "2".to_owned(),
            },
            previous_shared: previous.clone(),
            previous_local: previous,
            durable_job_id: "123e4567-e89b-42d3-a456-426614174003".to_owned(),
        }
    }

    fn backup(side: PeerBidirectionalBackupSide) -> PeerBidirectionalBackupReceipt {
        PeerBidirectionalBackupReceipt {
            package_id: "f".repeat(64),
            side,
            path: "peer-bidirectional/backups/conflict.risulossless".to_owned(),
        }
    }

    fn lossless_root(username: &str) -> serde_json::Value {
        json!({
            "username": username,
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0,
            "enabledModules": [],
            "characterOrder": [],
            "modules": [],
            "loadouts": [],
            "plugins": [],
        })
    }

    fn seed_lossless_backup_fixture(store: &mut PersistentStore, cas: &PayloadCas) {
        let icon = cas.prepare_bytes(b"fixture-icon").unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&staging, &lossless_root("Base"))
            .unwrap();
        store
            .replace_put_presets(&staging, &[json!({"name": "preset"})])
            .unwrap();
        store
            .replace_put_asset_aliases(
                &staging,
                &[AssetAlias {
                    key: "icon".to_owned(),
                    object_hash: Some(icon.content_hash),
                    kind: "asset".to_owned(),
                    size: icon.byte_size as i64,
                    mime: "application/octet-stream".to_owned(),
                    name: "icon.bin".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                }],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "bidirectional-backup-fixture".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        store
            .replace_put_cold_payload_authority(
                &staging,
                &ColdPayloadAuthorityState::V2 {
                    migration_id: "bidirectional-backup-fixture".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
    }

    #[test]
    fn conflict_backup_requires_v2_authority_before_replacement() {
        let authorities = [
            (r#"{"format":"legacy"}"#, r#"{"format":"legacy"}"#),
            (
                r#"{"format":"preparing","migrationId":"asset-preparing","sourceRevision":0}"#,
                r#"{"format":"v2","migrationId":"cold-v2","compatibilityHash":"cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"}"#,
            ),
            (
                r#"{"format":"v2","migrationId":"asset-v2","compatibilityHash":"abababababababababababababababababababababababababababababababab"}"#,
                r#"{"format":"preparing","migrationId":"cold-preparing","sourceRevision":0}"#,
            ),
        ];

        for (asset_authority, cold_authority) in authorities {
            let directory = tempfile::tempdir().unwrap();
            let cas = PayloadCas::new(directory.path()).unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            seed_lossless_backup_fixture(&mut store, &cas);
            let active = store
                .seal_or_initialize_active_logical_generation(&cas)
                .unwrap();
            let source = SyncGenerationIdentity {
                generation_id: active.manifest.generation,
                manifest_hash: active.manifest_hash,
                generation_sequence: active.manifest.generation_sequence,
            };
            drop(store);
            let connection = rusqlite::Connection::open(
                directory.path().join("persistent").join("persistent.db"),
            )
            .unwrap();
            connection
                .execute(
                    "UPDATE asset_repository_authority SET value = ?1 WHERE generation = 'revision-1'",
                    [asset_authority],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE cold_payload_authority SET value = ?1 WHERE generation = 'revision-1'",
                    [cold_authority],
                )
                .unwrap();
            drop(connection);
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let operation_id = "123e4567-e89b-42d3-a456-426614174099";

            let error = ensure_bidirectional_backup_receipt(
                &mut store,
                &cas,
                directory.path(),
                operation_id,
                1,
                PeerBidirectionalBackupSide::Local,
                &source,
            )
            .unwrap_err();

            assert!(matches!(error, PeerSyncError::Storage(_)));
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(store.read_root(None).unwrap().value, lossless_root("Base"));
            assert!(!bidirectional_backup_path(
                directory.path(),
                operation_id,
                PeerBidirectionalBackupSide::Local,
            )
            .exists());
        }
    }

    fn remote_disjoint_manifest(
        base: &crate::peer_sync::logical_delta::LogicalManifest,
    ) -> crate::peer_sync::logical_delta::BuiltLogicalManifest {
        build_logical_manifest(LogicalManifestBuilderInput {
            library_id: base.library_id.clone(),
            generation: "remote-disjoint".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some(base.generation.clone()),
            source_revision: base.source_revision + 1,
            records: vec![
                ProjectedLogicalRecord::live(
                    LogicalRecordLocator::Root,
                    LogicalRecordEnvelope::Root {
                        value: json!({}),
                        owner_heads: vec![],
                    },
                    vec![],
                ),
                ProjectedLogicalRecord::live(
                    LogicalRecordLocator::Plugin {
                        storage_key: "remote-key".to_owned(),
                    },
                    LogicalRecordEnvelope::Plugin {
                        ordinal: 0,
                        value: json!({"side": "remote"}),
                    },
                    vec![],
                ),
            ],
        })
        .unwrap()
    }

    fn fixture_source(
        built: &crate::peer_sync::logical_delta::BuiltLogicalManifest,
    ) -> FixtureSource {
        FixtureSource {
            objects: built
                .record_objects
                .iter()
                .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
                .collect(),
            reads: 0,
        }
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn durable_job_journal_ids(repository_root: &Path) -> Vec<String> {
        let directory = repository_root.join("assets-v2").join("job-pins");
        let mut ids = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter_map(|name| {
                name.strip_prefix("job-")
                    .and_then(|name| name.strip_suffix(".journal"))
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[test]
    fn local_committed_operation_survives_reopen_and_atomic_completion() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let operation_id = "123e4567-e89b-42d3-a456-426614174004";
        let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            committed_revision: 8,
            shared_generation: generation("local-a", "3", 'e'),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![backup(PeerBidirectionalBackupSide::Local)],
        };
        journal.store(&local_committed).unwrap();
        assert_eq!(journal.load().unwrap(), Some(local_committed));

        let completed = PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "updated".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 8,
                remote_revision: 4,
                transferred_objects: 3,
                transferred_bytes: 27,
                backups: vec![backup(PeerBidirectionalBackupSide::Local)],
            },
        };
        journal.store(&completed).unwrap();
        assert_eq!(journal.load().unwrap(), Some(completed));
    }

    #[test]
    fn source_prepared_operation_survives_reopen_without_changing_older_journals() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let operation_id = "123e4567-e89b-42d3-a456-426614174104";
        let prepared = PeerBidirectionalDurableOperation::SourcePrepared {
            schema: OPERATION_SCHEMA.to_owned(),
            operation_id: operation_id.to_owned(),
            source_device_id: "123e4567-e89b-42d3-a456-426614174105".to_owned(),
            target_device_id: "123e4567-e89b-42d3-a456-426614174106".to_owned(),
            expected_source_revision: 7,
            previous_shared: generation("shared-b", "2", 'a'),
            expected_source_generation: generation("source-e", "3", 'b'),
            shared_generation: LanBidirectionalGeneration {
                generation_id: "shared-a".to_owned(),
                manifest_hash: "c".repeat(64),
                generation_sequence: "4".to_owned(),
            },
            incoming_revision: 8,
            transferred_objects: 2,
            transferred_bytes: 19,
            backup: Some(LanBidirectionalBackupReceipt {
                package_id: "d".repeat(64),
                path: "peer-bidirectional/backups/source.risulossless".to_owned(),
            }),
            durable_job_id: "123e4567-e89b-42d3-a456-426614174107".to_owned(),
        };

        journal.store(&prepared).unwrap();
        assert_eq!(journal.load().unwrap(), Some(prepared.clone()));
        assert!(retained_allows_source_prepare(
            &prepared,
            "123e4567-e89b-42d3-a456-426614174105"
        ));
        assert!(!retained_allows_source_prepare(
            &prepared,
            "123e4567-e89b-42d3-a456-426614174111"
        ));
        let store = PersistentStore::open(directory.path()).unwrap();
        assert!(PeerBidirectionalCommandState::default()
            .status(directory.path(), &store)
            .unwrap()
            .operation
            .is_none());

        let old_fixture = serde_json::to_vec(&PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            committed_revision: 8,
            shared_generation: generation("local-a", "3", 'e'),
            changed: true,
            remote_backup_required: true,
            remote_apply_receipt: None,
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![],
        })
        .unwrap();
        fs::write(journal.root.join(OPERATION_FILE), old_fixture).unwrap();
        assert!(matches!(
            journal.load().unwrap(),
            Some(PeerBidirectionalDurableOperation::LocalCommitted {
                remote_apply_receipt: None,
                ..
            })
        ));
    }

    #[test]
    fn source_completed_journal_rejects_binding_result_mismatches() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let binding = SourcePreparedEvidence {
            operation_id: "123e4567-e89b-42d3-a456-426614174112".to_owned(),
            source_device_id: "123e4567-e89b-42d3-a456-426614174113".to_owned(),
            target_device_id: "123e4567-e89b-42d3-a456-426614174114".to_owned(),
            expected_source_revision: 7,
            previous_shared: generation("shared-b", "2", 'a'),
            expected_source_generation: generation("source-e", "3", 'b'),
            shared_generation: LanBidirectionalGeneration {
                generation_id: "shared-a".to_owned(),
                manifest_hash: "c".repeat(64),
                generation_sequence: "4".to_owned(),
            },
            incoming_revision: 9,
            transferred_objects: 2,
            transferred_bytes: 19,
            backup: Some(LanBidirectionalBackupReceipt {
                package_id: "d".repeat(64),
                path: "peer-bidirectional/backups/source.risulossless".to_owned(),
            }),
        };
        let receipt = binding.receipt_at(8).unwrap();
        let valid = PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: Some(receipt),
            source_binding: Some(binding.clone()),
            result: PeerBidirectionalCompletedResult {
                kind: "updated".to_owned(),
                operation_id: binding.operation_id.clone(),
                revision: 8,
                remote_revision: binding.incoming_revision,
                transferred_objects: binding.transferred_objects,
                transferred_bytes: binding.transferred_bytes,
                backups: vec![PeerBidirectionalBackupReceipt {
                    package_id: binding.backup.as_ref().unwrap().package_id.clone(),
                    side: PeerBidirectionalBackupSide::Remote,
                    path: binding.backup.as_ref().unwrap().path.clone(),
                }],
            },
        };
        journal.store(&valid).unwrap();
        let invalid = [
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.operation_id = "123e4567-e89b-42d3-a456-426614174115".to_owned();
                }
                ("operation", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.revision = 7;
                }
                ("revision", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.remote_revision += 1;
                }
                ("remote revision", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.transferred_objects += 1;
                }
                ("object total", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.transferred_bytes += 1;
                }
                ("byte total", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.backups[0].side = PeerBidirectionalBackupSide::Local;
                }
                ("backup", operation)
            },
            {
                let mut operation = valid.clone();
                if let PeerBidirectionalDurableOperation::Completed { result, .. } = &mut operation
                {
                    result.kind = "noChanges".to_owned();
                }
                ("kind", operation)
            },
        ];

        for (field, operation) in invalid {
            fs::write(
                journal.root.join(OPERATION_FILE),
                serde_json::to_vec(&operation).unwrap(),
            )
            .unwrap();
            assert!(journal.load().is_err(), "accepted mismatched {field}");
        }
    }

    #[test]
    fn receipt_before_ack_crash_preserves_a_descendant_edit_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174097";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let previous = SyncGenerationIdentity {
            generation_id: base.manifest.generation,
            manifest_hash: base.manifest_hash,
            generation_sequence: base.manifest.generation_sequence,
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    previous.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side": "shared"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let active = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: active.manifest.generation,
            manifest_hash: active.manifest_hash,
            generation_sequence: active.manifest.generation_sequence,
        };
        let operation_id = "123e4567-e89b-42d3-a456-426614174098";
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.expected_remote_revision = 4;
        retained_context.previous_shared = previous.clone();
        retained_context.previous_local = previous.clone();
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context.clone(),
                committed_revision: 1,
                shared_generation: shared.clone(),
                changed: true,
                remote_backup_required: false,
                remote_apply_receipt: None,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        let receipt = LanBidirectionalRemoteApplyReceipt {
            committed_revision: 5,
            committed_generation: LanBidirectionalGeneration {
                generation_id: shared.generation_id.clone(),
                manifest_hash: shared.manifest_hash.clone(),
                generation_sequence: shared.generation_sequence.clone(),
            },
            transferred_objects: 3,
            transferred_bytes: 27,
            backup: None,
        };
        COMPLETE_BEFORE_ACK_FAILPOINT.with(|enabled| enabled.set(true));

        assert!(complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            receipt.clone(),
        )
        .is_err());
        assert_eq!(
            store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap()
                .shared_identity,
            previous
        );
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(json!({"side": "local-after-receipt"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        drop(store);
        let mut reopened = PersistentStore::open(directory.path()).unwrap();

        let resumed = resume_bidirectional_local_committed_with_remote(
            &mut reopened,
            &cas,
            directory.path(),
            operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                panic!("durable remote receipt must complete without a second network request")
            },
        )
        .unwrap();
        assert!(matches!(
            resumed,
            ResumeLocalCommittedOutcome::Completed(PeerBidirectionalCompletedResult {
                revision: 2,
                remote_revision: 5,
                transferred_objects: 3,
                transferred_bytes: 27,
                ..
            })
        ));
        assert_eq!(reopened.revision().unwrap(), 2);
        assert_eq!(
            reopened
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            Some(shared.clone())
        );

        reopened
            .commit(&WorkingSetCommit {
                expected_revision: 2,
                root: Some(json!({"side": "another-local-edit"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        reopened
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context,
                committed_revision: 1,
                shared_generation: shared.clone(),
                changed: true,
                remote_backup_required: false,
                remote_apply_receipt: Some(receipt),
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        PeerBidirectionalCommandState::default()
            .acknowledge(directory.path(), &mut reopened, operation_id)
            .unwrap();
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(reopened.revision().unwrap(), 3);
        assert_eq!(
            reopened
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            Some(shared)
        );
    }

    #[test]
    fn legacy_local_committed_journal_requires_remote_backup_conservatively() {
        let directory = tempfile::tempdir().unwrap();
        let operation_root = directory.path().join("peer-bidirectional");
        fs::create_dir_all(&operation_root).unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174004";
        let legacy_operation = json!({
            "phase": "localCommitted",
            "schema": "risunest.peer-bidirectional-operation/v1",
            "context": {
                "operationId": operation_id,
                "credential": {
                    "endpoint": "http://192.168.1.2:32146",
                    "sessionId": "123e4567-e89b-42d3-a456-426614174000",
                    "manifestId": "b".repeat(64),
                    "deviceId": "123e4567-e89b-42d3-a456-426614174001",
                    "sourceDeviceId": "123e4567-e89b-42d3-a456-426614174002",
                    "bearer": "c".repeat(64),
                },
                "libraryId": "risunest-product",
                "expectedLocalRevision": 7,
                "expectedRemoteRevision": 7,
                "expectedRemoteGeneration": {
                    "generationId": "remote-r",
                    "manifestHash": "d".repeat(64),
                    "generationSequence": "2",
                },
                "previousShared": {
                    "generationId": "shared-c",
                    "manifestHash": "a".repeat(64),
                    "generationSequence": "1",
                },
                "previousLocal": {
                    "generationId": "shared-c",
                    "manifestHash": "a".repeat(64),
                    "generationSequence": "1",
                },
                "durableJobId": "123e4567-e89b-42d3-a456-426614174003",
            },
            "committed_revision": 8,
            "shared_generation": {
                "generationId": "local-a",
                "manifestHash": "e".repeat(64),
                "generationSequence": "3",
            },
            "changed": true,
            "transferred_objects": 2,
            "transferred_bytes": 19,
            "backups": [],
        });
        fs::write(
            operation_root.join(OPERATION_FILE),
            serde_json::to_vec(&legacy_operation).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::LocalCommitted {
                remote_backup_required: true,
                ..
            }
        ));
    }

    #[test]
    fn zero_transfer_remote_revision_advance_completes_as_updated() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        let peer_id = "123e4567-e89b-42d3-a456-426614174025";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174026";
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.expected_remote_revision = 4;
        retained_context.previous_shared = shared.clone();
        retained_context.previous_local = shared.clone();
        let retained = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 0,
            shared_generation: shared.clone(),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());

        journal.store(&retained).unwrap();
        let unchanged = complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            LanBidirectionalRemoteApplyReceipt {
                committed_revision: 4,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: shared.generation_id.clone(),
                    manifest_hash: shared.manifest_hash.clone(),
                    generation_sequence: shared.generation_sequence.clone(),
                },
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            },
        )
        .unwrap();
        assert_eq!(unchanged.kind, "noChanges");

        journal.store(&retained).unwrap();
        let record_only = complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            LanBidirectionalRemoteApplyReceipt {
                committed_revision: 5,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: shared.generation_id.clone(),
                    manifest_hash: shared.manifest_hash.clone(),
                    generation_sequence: shared.generation_sequence.clone(),
                },
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            },
        )
        .unwrap();
        assert_eq!(record_only.kind, "updated");
        assert_eq!(record_only.transferred_objects, 0);
        assert_eq!(record_only.transferred_bytes, 0);

        journal.store(&retained).unwrap();
        assert!(matches!(
            complete_bidirectional_local_after_remote_apply(
                &mut store,
                &cas,
                directory.path(),
                operation_id,
                LanBidirectionalRemoteApplyReceipt {
                    committed_revision: 6,
                    committed_generation: LanBidirectionalGeneration {
                        generation_id: shared.generation_id,
                        manifest_hash: shared.manifest_hash,
                        generation_sequence: shared.generation_sequence,
                    },
                    transferred_objects: 0,
                    transferred_bytes: 0,
                    backup: None,
                },
            ),
            Err(PeerSyncError::Validation(_))
        ));
        assert_eq!(journal.load().unwrap(), Some(retained));
    }

    #[test]
    fn source_exact_common_base_no_op_reopens_with_the_same_revision() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let target_device_id = "123e4567-e89b-42d3-a456-426614174027";
        let source_device_id = "123e4567-e89b-42d3-a456-426614174028";
        let operation_id = "123e4567-e89b-42d3-a456-426614174029";
        let previous = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    target_device_id,
                    previous.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let shared_generation = LanBidirectionalGeneration {
            generation_id: previous.generation_id.clone(),
            manifest_hash: previous.manifest_hash.clone(),
            generation_sequence: previous.generation_sequence.clone(),
        };
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };

        let receipt = apply_bidirectional_remote_shared_inner(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            target_device_id,
            0,
            &previous.manifest_hash,
            &previous,
            shared_generation.clone(),
            &base.manifest_bytes,
            &mut source,
            false,
            Some(source_device_id),
            None,
            None,
        )
        .unwrap();

        assert_eq!(receipt.committed_revision, 0);
        assert_eq!(receipt.transferred_objects, 0);
        assert_eq!(receipt.transferred_bytes, 0);
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 0);
        drop(store);
        let start_retry_host = || {
            let source = LogicalDeltaSourceSession::open(
                directory.path(),
                directory.path(),
                PRODUCT_LOGICAL_LIBRARY_ID,
                &base.manifest.generation,
            )
            .unwrap();
            let prepared = super::super::lan::PreparedLogicalLanSession::new(
                &uuid::Uuid::new_v4().to_string(),
                target_device_id,
                base.manifest_hash.clone(),
                base.manifest_bytes.clone(),
                source.objects().to_vec(),
                Box::new(source),
            )
            .unwrap();
            let mut host = LanCloneHost::prepare_logical(prepared);
            let pairing = host.start().unwrap();
            (host, pairing)
        };
        let session = LanBidirectionalSession {
            session_id: "123e4567-e89b-42d3-a456-426614174031".to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };
        let mut request = LanBidirectionalRemoteApplyRequest {
            operation_id: operation_id.to_owned(),
            source_endpoint: String::new(),
            source_session_id: String::new(),
            source_manifest_id: String::new(),
            source_claim: String::new(),
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: previous.generation_id.clone(),
                manifest_hash: previous.manifest_hash.clone(),
                generation_sequence: previous.generation_sequence.clone(),
            },
            expected_common_base_manifest_hash: previous.manifest_hash.clone(),
            backup_losing_side: false,
        };
        let control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            PersistentStore::open(directory.path()).unwrap(),
            &session.session_id,
            source_device_id,
        );
        let prepared = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        let retained_job_id = match prepared {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                durable_job_id
            }
            other => panic!("expected retained source preparation, got {other:?}"),
        };
        assert_eq!(
            DurableCasJob::open(directory.path(), &retained_job_id)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let (mut host, pairing) = start_retry_host();
        request.source_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
        request.source_session_id = pairing.session_id;
        request.source_manifest_id = pairing.manifest_id;
        request.source_claim = pairing.claim;
        SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
        let retry_error = control
            .remote_apply(session.clone(), request.clone())
            .unwrap_err();
        assert!(
            matches!(&retry_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
            "{retry_error:?}"
        );
        assert!(!DurableCasJob::open(directory.path(), &retained_job_id)
            .unwrap()
            .is_sealed());
        host.stop().unwrap();

        let (mut host, pairing) = start_retry_host();
        request.source_endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
        request.source_session_id = pairing.session_id;
        request.source_manifest_id = pairing.manifest_id;
        request.source_claim = pairing.claim;

        assert_eq!(
            control
                .remote_apply(session.clone(), request.clone())
                .unwrap(),
            receipt.clone()
        );
        host.stop().unwrap();
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(replayed),
                result: PeerBidirectionalCompletedResult {
                    revision: 0,
                    ref kind,
                    ..
                },
                ..
            }) if replayed == receipt && kind == "noChanges"
        ));
        assert_eq!(
            control
                .remote_apply(session.clone(), request.clone())
                .unwrap(),
            receipt
        );

        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let mut mismatched = journal.load().unwrap().unwrap();
        if let PeerBidirectionalDurableOperation::Completed {
            remote_apply_receipt: Some(receipt),
            result,
            ..
        } = &mut mismatched
        {
            receipt.committed_revision = 1;
            result.revision = 1;
            result.kind = "updated".to_owned();
        } else {
            panic!("expected source completion");
        }
        journal.store(&mismatched).unwrap();
        assert!(matches!(
            control.remote_apply(session, request),
            Err(PeerSyncError::Storage(_) | PeerSyncError::Validation(_))
        ));
    }

    #[test]
    fn source_content_identical_no_op_recovers_original_receipt_after_descendant_edit() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let target_device_id = "123e4567-e89b-42d3-a456-426614174033";
        let source_device_id = "123e4567-e89b-42d3-a456-426614174034";
        let operation_id = "123e4567-e89b-42d3-a456-426614174035";
        let previous = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    target_device_id,
                    previous.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let mut shared_manifest = base.manifest.clone();
        shared_manifest.generation = "123e4567-e89b-42d3-a456-426614174036".to_owned();
        shared_manifest.generation_sequence = "1".to_owned();
        shared_manifest.parent_generation = Some(base.manifest.generation.clone());
        let shared_manifest_bytes = encode_logical_manifest(&shared_manifest).unwrap();
        let shared_generation = LanBidirectionalGeneration {
            generation_id: shared_manifest.generation.clone(),
            manifest_hash: hash_logical_manifest(&shared_manifest).unwrap(),
            generation_sequence: shared_manifest.generation_sequence.clone(),
        };
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        let receipt = apply_bidirectional_remote_shared_inner(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            target_device_id,
            0,
            &previous.manifest_hash,
            &previous,
            shared_generation,
            &shared_manifest_bytes,
            &mut source,
            false,
            Some(source_device_id),
            None,
            None,
        )
        .unwrap();
        assert_eq!(receipt.committed_revision, 0);
        assert_eq!(source.reads, 0);
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"preserved": "source-no-op-descendant"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        drop(store);
        let session = LanBidirectionalSession {
            session_id: "123e4567-e89b-42d3-a456-426614174037".to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: operation_id.to_owned(),
            source_endpoint: "http://127.0.0.1:1".to_owned(),
            source_session_id: "123e4567-e89b-42d3-a456-426614174038".to_owned(),
            source_manifest_id: "manifest".to_owned(),
            source_claim: "claim".to_owned(),
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: previous.generation_id,
                manifest_hash: previous.manifest_hash.clone(),
                generation_sequence: previous.generation_sequence,
            },
            expected_common_base_manifest_hash: previous.manifest_hash,
            backup_losing_side: false,
        };
        let control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            PersistentStore::open(directory.path()).unwrap(),
            &session.session_id,
            source_device_id,
        );

        assert_eq!(
            control.remote_apply(session, request).unwrap(),
            receipt.clone()
        );
        drop(control);
        let inspector = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(inspector.revision().unwrap(), 1);
        assert_eq!(
            inspector.read_root(None).unwrap().value["preserved"],
            "source-no-op-descendant"
        );
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(replayed),
                result: PeerBidirectionalCompletedResult { revision: 1, .. },
                ..
            }) if replayed == receipt && replayed.committed_revision == 0
        ));
    }

    fn assert_invalid_retained_sealed_source_job(
        kind: CasJobKind,
        pin_shared_manifest: bool,
        expected_message: &str,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let target_device_id = "123e4567-e89b-42d3-a456-426614174039";
        let source_device_id = "123e4567-e89b-42d3-a456-426614174040";
        let operation_id = "123e4567-e89b-42d3-a456-426614174041";
        let durable_job_id = "123e4567-e89b-42d3-a456-426614174042";
        let previous = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    target_device_id,
                    previous.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let shared_generation = LanBidirectionalGeneration {
            generation_id: previous.generation_id.clone(),
            manifest_hash: previous.manifest_hash.clone(),
            generation_sequence: previous.generation_sequence.clone(),
        };
        let evidence = SourcePreparedEvidence {
            operation_id: operation_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
            expected_source_revision: 0,
            previous_shared: previous.clone(),
            expected_source_generation: previous.clone(),
            shared_generation: shared_generation.clone(),
            incoming_revision: 0,
            transferred_objects: 0,
            transferred_bytes: 0,
            backup: None,
        };
        let mut job = DurableCasJob::begin(directory.path(), durable_job_id, kind, 0).unwrap();
        if pin_shared_manifest {
            let size = cas
                .stat_object(&base.manifest_hash)
                .unwrap()
                .expect("sealed active manifest must exist in the CAS");
            job.pin_existing(&cas, &base.manifest_hash, size, CasObjectRole::DirectObject)
                .unwrap();
        }
        job.seal(&mut store, 0).unwrap();
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&evidence.durable_operation(durable_job_id.to_owned()))
            .unwrap();
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };

        let error = apply_bidirectional_remote_shared_inner(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            target_device_id,
            0,
            &previous.manifest_hash,
            &previous,
            shared_generation,
            &base.manifest_bytes,
            &mut source,
            false,
            Some(source_device_id),
            Some(&evidence),
            Some(durable_job_id),
        )
        .unwrap_err();

        assert!(
            matches!(&error, PeerSyncError::Storage(message) if message == expected_message),
            "{error:?}"
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert!(DurableCasJob::open(directory.path(), durable_job_id)
            .unwrap()
            .is_sealed());
    }

    #[test]
    fn retained_sealed_source_job_rejects_an_unexpected_kind() {
        assert_invalid_retained_sealed_source_job(
            CasJobKind::DirectAssetOrInlayWrite,
            true,
            "retained source job has an unexpected kind",
        );
    }

    #[test]
    fn retained_sealed_source_job_requires_the_shared_manifest_root() {
        assert_invalid_retained_sealed_source_job(
            CasJobKind::LogicalDeltaTarget,
            false,
            "retained source job does not own the shared manifest",
        );
    }

    #[test]
    fn source_unavailable_preserves_local_committed_operation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        let peer_id = "123e4567-e89b-42d3-a456-426614174012";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174013";
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.previous_shared = shared.clone();
        retained_context.previous_local = shared.clone();
        let retained = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 0,
            shared_generation: shared.clone(),
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal.store(&retained).unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"preserved": "offline-descendant"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        drop(store);
        let mut reopened = PersistentStore::open(directory.path()).unwrap();

        let outcome = resume_bidirectional_local_committed_with_remote(
            &mut reopened,
            &cas,
            directory.path(),
            operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                Err(PeerSyncError::Transport(
                    "peer source is offline".to_owned(),
                ))
            },
        )
        .unwrap();

        assert_eq!(
            outcome,
            ResumeLocalCommittedOutcome::SourceUnavailable {
                operation_id: operation_id.to_owned(),
                committed_revision: 0,
            }
        );
        assert_eq!(journal.load().unwrap(), Some(retained));
        assert_eq!(
            reopened
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            crate::persistent_store::SyncDeviceAckState {
                shared_identity: shared.clone(),
                local_identity: shared,
            }
        );

        let outcome = resume_bidirectional_local_committed_with_remote(
            &mut reopened,
            &cas,
            directory.path(),
            operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("before".to_owned()),
                    actual: Some("after".to_owned()),
                })
            },
        )
        .unwrap();
        assert_eq!(
            outcome,
            ResumeLocalCommittedOutcome::Stale {
                operation_id: operation_id.to_owned(),
                reason: PeerBidirectionalStaleReason::RemoteGeneration,
            }
        );
        assert!(journal.load().unwrap().is_none());
    }

    #[test]
    fn resume_rejects_required_missing_remote_backup_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        let peer_id = "123e4567-e89b-42d3-a456-426614174032";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174031";
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.expected_local_revision = 0;
        retained_context.expected_remote_revision = 4;
        retained_context.previous_shared = shared.clone();
        retained_context.previous_local = shared.clone();
        let retained = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: retained_context,
            committed_revision: 0,
            shared_generation: shared.clone(),
            changed: true,
            remote_backup_required: true,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal.store(&retained).unwrap();
        let before_root = store.read_root(None).unwrap();
        let before_ack = store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
            .unwrap();
        drop(store);
        let mut reopened = PersistentStore::open(directory.path()).unwrap();

        let error = resume_bidirectional_local_committed_with_remote(
            &mut reopened,
            &cas,
            directory.path(),
            operation_id,
            |_context, _revision, remote_shared, _manifest, backup_required| {
                assert!(backup_required);
                Ok(LanBidirectionalRemoteApplyReceipt {
                    committed_revision: 4,
                    committed_generation: LanBidirectionalGeneration {
                        generation_id: remote_shared.generation_id.clone(),
                        manifest_hash: remote_shared.manifest_hash.clone(),
                        generation_sequence: remote_shared.generation_sequence.clone(),
                    },
                    transferred_objects: 0,
                    transferred_bytes: 0,
                    backup: None,
                })
            },
        )
        .unwrap_err();

        assert!(matches!(error, PeerSyncError::Validation(_)));
        assert_eq!(journal.load().unwrap(), Some(retained));
        assert_eq!(reopened.revision().unwrap(), 0);
        assert_eq!(reopened.read_root(None).unwrap(), before_root);
        assert_eq!(
            reopened
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            before_ack
        );
    }

    #[test]
    fn resume_contacts_the_source_before_abandoning_stale_local_peer_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        let peer_id = "123e4567-e89b-42d3-a456-426614174022";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());

        let common_base_operation_id = "123e4567-e89b-42d3-a456-426614174023";
        let mut common_base_context = context(common_base_operation_id);
        common_base_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        common_base_context.credential.source_device_id = peer_id.to_owned();
        common_base_context.previous_shared = generation("stale-base", "0", 'a');
        common_base_context.previous_local = shared.clone();
        journal
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: common_base_context,
                committed_revision: 0,
                shared_generation: shared.clone(),
                changed: false,
                remote_backup_required: false,
                remote_apply_receipt: None,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        assert_eq!(
            resume_bidirectional_local_committed_with_remote(
                &mut store,
                &cas,
                directory.path(),
                common_base_operation_id,
                |_context, _revision, _shared, _manifest, _backup_required| {
                    Err(PeerSyncError::ActivationConflict {
                        expected: Some("retained-source".to_owned()),
                        actual: Some("different-source".to_owned()),
                    })
                },
            )
            .unwrap(),
            ResumeLocalCommittedOutcome::Stale {
                operation_id: common_base_operation_id.to_owned(),
                reason: PeerBidirectionalStaleReason::RemoteGeneration,
            }
        );

        let device_ack_operation_id = "123e4567-e89b-42d3-a456-426614174024";
        assert_eq!(
            store
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            Some(shared.clone())
        );
        store
            .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, peer_id, &shared)
            .unwrap();
        let mut device_ack_context = context(device_ack_operation_id);
        device_ack_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        device_ack_context.credential.source_device_id = peer_id.to_owned();
        device_ack_context.previous_shared = shared.clone();
        device_ack_context.previous_local = shared.clone();
        journal
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: device_ack_context,
                committed_revision: 0,
                shared_generation: shared.clone(),
                changed: false,
                remote_backup_required: false,
                remote_apply_receipt: None,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        assert_eq!(
            resume_bidirectional_local_committed_with_remote(
                &mut store,
                &cas,
                directory.path(),
                device_ack_operation_id,
                |_context, _revision, _shared, _manifest, _backup_required| {
                    Err(PeerSyncError::ActivationConflict {
                        expected: Some("retained-source".to_owned()),
                        actual: Some("different-source".to_owned()),
                    })
                },
            )
            .unwrap(),
            ResumeLocalCommittedOutcome::Stale {
                operation_id: device_ack_operation_id.to_owned(),
                reason: PeerBidirectionalStaleReason::RemoteGeneration,
            }
        );
        assert!(journal.load().unwrap().is_none());
    }

    #[test]
    fn remote_generation_rejection_after_local_drift_abandons_the_operation_and_job() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174014";
        let peer_id = "123e4567-e89b-42d3-a456-426614174002";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"shared": "local-commit"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let committed = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let committed_shared = SyncGenerationIdentity {
            generation_id: committed.manifest.generation,
            manifest_hash: committed.manifest_hash,
            generation_sequence: committed.manifest.generation_sequence,
        };
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.previous_shared = shared.clone();
        retained_context.previous_local = shared.clone();
        let durable_job_id = retained_context.durable_job_id.clone();
        let job = DurableCasJob::begin(
            directory.path(),
            &durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .unwrap();
        drop(job);
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context,
                committed_revision: 1,
                shared_generation: committed_shared.clone(),
                changed: false,
                remote_backup_required: false,
                remote_apply_receipt: None,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(json!({"advanced": true})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();

        let outcome = resume_bidirectional_local_committed_with_remote(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            |_context, _revision, _shared, _manifest, _backup_required| {
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("retained-source".to_owned()),
                    actual: Some("different-source".to_owned()),
                })
            },
        )
        .unwrap();

        assert_eq!(
            outcome,
            ResumeLocalCommittedOutcome::Stale {
                operation_id: operation_id.to_owned(),
                reason: PeerBidirectionalStaleReason::RemoteGeneration,
            }
        );
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(
            DurableCasJob::open(directory.path(), &durable_job_id)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn stale_caller_revision_returns_resume_required_when_local_commit_is_intact() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let active = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174021";
        let retained = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            committed_revision: 0,
            shared_generation: SyncGenerationIdentity {
                generation_id: active.manifest.generation,
                manifest_hash: active.manifest_hash,
                generation_sequence: active.manifest.generation_sequence,
            },
            changed: false,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: vec![],
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal.store(&retained).unwrap();

        assert_eq!(
            run_retained_remote_completion(&mut store, directory.path(), operation_id, 1, None,)
                .unwrap(),
            PeerBidirectionalSyncResult::ResumeRequired {
                operation_id: operation_id.to_owned(),
                phase: "localCommitted",
                committed_revision: 0,
            }
        );
        assert_eq!(journal.load().unwrap(), Some(retained));
    }

    #[test]
    fn acknowledge_abandons_local_commit_and_allows_a_new_authenticated_target() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174024";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation,
            manifest_hash: base.manifest_hash,
            generation_sequence: base.manifest.generation_sequence,
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"committed": true})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174025";
        let mut retained_context = context(operation_id);
        retained_context.credential.source_device_id = peer_id.to_owned();
        retained_context.durable_job_id = "123e4567-e89b-42d3-a456-426614174026".to_owned();
        let durable_job_id = retained_context.durable_job_id.clone();
        drop(
            DurableCasJob::begin(
                directory.path(),
                &durable_job_id,
                CasJobKind::LogicalDeltaTarget,
                1,
            )
            .unwrap(),
        );
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: retained_context,
                committed_revision: 1,
                shared_generation: common.clone(),
                changed: true,
                remote_backup_required: true,
                remote_apply_receipt: None,
                transferred_objects: 1,
                transferred_bytes: 1,
                backups: vec![],
            })
            .unwrap();
        let committed_root = store.read_root(None).unwrap();

        let state = PeerBidirectionalCommandState::default();
        state
            .acknowledge(directory.path(), &mut store, operation_id)
            .unwrap();

        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.read_root(None).unwrap(), committed_root);
        assert_eq!(
            store
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap(),
            Some(common)
        );
        assert_eq!(
            DurableCasJob::open(directory.path(), &durable_job_id)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        attach_local_device_at_existing_base(&mut store, peer_id, 1).unwrap();
        assert!(state.begin_target().is_ok());
    }

    #[test]
    fn deferred_already_active_is_retained_as_a_changed_local_commit() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        let peer_id = "123e4567-e89b-42d3-a456-426614174002";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    shared.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174015";
        let mut retained_context = context(operation_id);
        retained_context.library_id = PRODUCT_LOGICAL_LIBRARY_ID.to_owned();
        retained_context.expected_remote_revision = 4;
        retained_context.previous_shared = shared.clone();
        retained_context.previous_local = shared.clone();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                &retained_context.durable_job_id,
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut().seal(&mut store, 0).unwrap();

        assert_eq!(
            retain_bidirectional_local_activation(
                &mut store,
                &cas,
                directory.path(),
                retained_context,
                &job,
                0,
                Ok(LogicalDeltaActivation::AlreadyActive { revision: 0 }),
                true,
                false,
                0,
                0,
                vec![],
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );

        let retained = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&retained).unwrap()["changed"],
            serde_json::Value::Bool(true)
        );
        assert!(matches!(
            retained,
            PeerBidirectionalDurableOperation::LocalCommitted {
                committed_revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
                ..
            }
        ));
        assert_eq!(
            DurableCasJob::open(directory.path(), "123e4567-e89b-42d3-a456-426614174003")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );

        let result = complete_bidirectional_local_after_remote_apply(
            &mut store,
            &cas,
            directory.path(),
            operation_id,
            LanBidirectionalRemoteApplyReceipt {
                committed_revision: 4,
                committed_generation: LanBidirectionalGeneration {
                    generation_id: shared.generation_id,
                    manifest_hash: shared.manifest_hash,
                    generation_sequence: shared.generation_sequence,
                },
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            },
        )
        .unwrap();
        assert_eq!(result.kind, "updated");
        assert_eq!(result.transferred_objects, 0);
        assert_eq!(result.transferred_bytes, 0);
        assert_eq!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(PeerBidirectionalDurableOperation::Completed {
                schema: OPERATION_SCHEMA.to_owned(),
                remote_apply_receipt: None,
                source_binding: None,
                result,
            })
        );
    }

    #[test]
    fn completed_acknowledgement_preserves_lossless_backups() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let operation_id = "123e4567-e89b-42d3-a456-426614174004";
        let backup_root = directory.path().join("peer-bidirectional/backups");
        fs::create_dir_all(&backup_root).unwrap();
        let local_path = backup_root.join(format!("{operation_id}-local.risulossless"));
        let remote_path = backup_root.join(format!("{operation_id}-remote.risulossless"));
        fs::write(&local_path, b"retained local backup").unwrap();
        fs::write(&remote_path, b"retained remote backup").unwrap();
        let backups = vec![
            PeerBidirectionalBackupReceipt {
                package_id: "a".repeat(64),
                side: PeerBidirectionalBackupSide::Local,
                path: local_path.to_string_lossy().into_owned(),
            },
            PeerBidirectionalBackupReceipt {
                package_id: "b".repeat(64),
                side: PeerBidirectionalBackupSide::Remote,
                path: remote_path.to_string_lossy().into_owned(),
            },
        ];
        let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            committed_revision: 7,
            shared_generation: generation("shared-a", "2", 'e'),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 0,
            transferred_bytes: 0,
            backups: backups.clone(),
        };
        journal.store(&local_committed).unwrap();

        assert!(journal
            .acknowledge_completed("123e4567-e89b-42d3-a456-426614174099")
            .is_err());
        assert!(journal.acknowledge_completed(operation_id).is_err());
        assert_eq!(journal.load().unwrap(), Some(local_committed));

        let completed = PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "updated".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 7,
                remote_revision: 4,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups,
            },
        };
        journal.store(&completed).unwrap();
        let reopened = PeerBidirectionalOperationJournal::new(directory.path());
        assert_eq!(reopened.load().unwrap(), Some(completed.clone()));
        assert!(reopened
            .acknowledge_completed("123e4567-e89b-42d3-a456-426614174099")
            .is_err());
        assert_eq!(reopened.load().unwrap(), Some(completed));
        assert!(local_path.is_file());
        assert!(remote_path.is_file());

        reopened.acknowledge_completed(operation_id).unwrap();
        assert_eq!(reopened.load().unwrap(), None);
        assert_eq!(fs::read(local_path).unwrap(), b"retained local backup");
        assert_eq!(fs::read(remote_path).unwrap(), b"retained remote backup");
    }

    #[test]
    fn durable_operations_project_the_exact_frontend_contract() {
        let operation_id = "123e4567-e89b-42d3-a456-426614174004";
        let conflict = PeerBidirectionalDurableOperation::AwaitingConflict {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            conflicts: vec![PeerBidirectionalConflict {
                key: "r1:root".to_owned(),
                conflict_type: "sameRecord".to_owned(),
            }],
            local_generation: generation("local-l", "2", 'c'),
            local_manifest_hash: "a".repeat(64),
            remote_manifest_hash: "b".repeat(64),
            backups: vec![],
        };
        assert_eq!(
            serde_json::to_value(conflict.status_projection()).unwrap(),
            serde_json::json!({
                "phase": "awaitingConflict",
                "result": {
                    "kind": "conflict",
                    "operationId": operation_id,
                    "conflicts": [{"key": "r1:root", "type": "sameRecord"}],
                    "localManifestHash": "a".repeat(64),
                    "remoteManifestHash": "b".repeat(64),
                }
            })
        );

        let local_committed = PeerBidirectionalDurableOperation::LocalCommitted {
            schema: OPERATION_SCHEMA.to_owned(),
            context: context(operation_id),
            committed_revision: 8,
            shared_generation: generation("local-a", "3", 'e'),
            changed: true,
            remote_backup_required: false,
            remote_apply_receipt: None,
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![backup(PeerBidirectionalBackupSide::Local)],
        };
        assert_eq!(
            serde_json::to_value(local_committed.status_projection()).unwrap(),
            serde_json::json!({
                "phase": "localCommitted",
                "operationId": operation_id,
                "committedRevision": 8,
            })
        );

        let completed = PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
            remote_apply_receipt: None,
            source_binding: None,
            result: PeerBidirectionalCompletedResult {
                kind: "updated".to_owned(),
                operation_id: operation_id.to_owned(),
                revision: 8,
                remote_revision: 9,
                transferred_objects: 3,
                transferred_bytes: 27,
                backups: vec![backup(PeerBidirectionalBackupSide::Remote)],
            },
        };
        assert_eq!(
            serde_json::to_value(completed.status_projection()).unwrap(),
            serde_json::json!({
                "phase": "completed",
                "result": {
                    "kind": "updated",
                    "operationId": operation_id,
                    "revision": 8,
                    "remoteRevision": 9,
                    "transferredObjects": 3,
                    "transferredBytes": 27,
                    "backups": [{
                        "packageId": "f".repeat(64),
                        "side": "remote",
                        "path": "peer-bidirectional/backups/conflict.risulossless",
                    }],
                }
            })
        );
    }

    #[test]
    fn reject_conflict_is_durable_and_mutates_neither_side() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        seed_lossless_backup_fixture(&mut store, &cas);
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        drop(store);
        let remote_directory = tempfile::tempdir().unwrap();
        copy_tree(directory.path(), remote_directory.path());
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174002";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            1,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common.clone(),
                    1,
                )
                .unwrap(),
                1,
            )
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(lossless_root("Local")),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote_cas = PayloadCas::new(remote_directory.path()).unwrap();
        let mut remote_store = PersistentStore::open(remote_directory.path()).unwrap();
        remote_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(lossless_root("Remote")),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote = remote_store
            .seal_or_initialize_active_logical_generation(&remote_cas)
            .unwrap();
        let credential = LanBidirectionalLogicalCredential {
            endpoint: "http://192.168.1.2:32146".to_owned(),
            session_id: "123e4567-e89b-42d3-a456-426614174000".to_owned(),
            manifest_id: remote.manifest_hash.clone(),
            device_id: "123e4567-e89b-42d3-a456-426614174001".to_owned(),
            source_device_id: peer_id.to_owned(),
            bearer: "c".repeat(64),
        };

        let mut source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        let conflict = match begin_bidirectional_local_merge(
            &mut store,
            &cas,
            directory.path(),
            credential,
            2,
            &remote.manifest_bytes,
            &mut source,
        )
        .unwrap()
        {
            LocalMergeOutcome::Conflict(conflict) => conflict,
            LocalMergeOutcome::LocalCommitted => panic!("conflict unexpectedly committed"),
        };

        assert_eq!(conflict.operation_id.len(), 36);
        assert_eq!(
            conflict.conflicts,
            vec![PeerBidirectionalConflictStatus {
                key: "r1:root".to_owned(),
                conflict_type: "sameRecord".to_owned(),
            }]
        );
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
        assert_eq!(
            store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap()
                .shared_identity,
            common
        );
        let retained = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        let (durable_job_id, local_generation) = match retained {
            PeerBidirectionalDurableOperation::AwaitingConflict {
                context,
                conflicts,
                local_generation,
                ..
            } => {
                assert_eq!(context.operation_id, conflict.operation_id);
                assert_eq!(conflicts.len(), 1);
                (context.durable_job_id, local_generation)
            }
            other => panic!("unexpected retained operation: {other:?}"),
        };
        let job = DurableCasJob::open(directory.path(), &durable_job_id).unwrap();
        assert!(!job.is_sealed());
        assert!(!job.is_released());
        drop(job);

        let backup_path = directory
            .path()
            .join("peer-bidirectional")
            .join("backups")
            .join(format!("{}-local.risulossless", conflict.operation_id));
        fs::create_dir_all(backup_path.parent().unwrap()).unwrap();
        let wrong_source_directory = tempfile::tempdir().unwrap();
        let wrong_source_cas = PayloadCas::new(wrong_source_directory.path()).unwrap();
        let mut wrong_source_store = PersistentStore::open(wrong_source_directory.path()).unwrap();
        seed_lossless_backup_fixture(&mut wrong_source_store, &wrong_source_cas);
        wrong_source_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(lossless_root("Wrong source at the same revision")),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let wrong_generation = wrong_source_store
            .seal_or_initialize_active_logical_generation(&wrong_source_cas)
            .unwrap();
        let wrong_source_identity = SyncGenerationIdentity {
            generation_id: wrong_generation.manifest.generation,
            manifest_hash: wrong_generation.manifest_hash,
            generation_sequence: wrong_generation.manifest.generation_sequence,
        };
        let wrong_source_staging = wrong_source_directory.path().join("backup-staging");
        fs::create_dir_all(&wrong_source_staging).unwrap();
        create_and_verify_peer_bidirectional_backup_v1_report(
            &backup_path,
            &wrong_source_staging,
            &wrong_source_cas,
            &mut wrong_source_store,
            2,
            &lossless_source_binding(
                &conflict.operation_id,
                PeerBidirectionalBackupSide::Local,
                &wrong_source_identity,
            ),
            &NeverCancelled,
        )
        .unwrap();
        let wrong_source_bytes = fs::read(&backup_path).unwrap();
        let mut wrong_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert!(resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut wrong_source,
        )
        .is_err());
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
        assert_eq!(fs::read(&backup_path).unwrap(), wrong_source_bytes);
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. }
                if backups.is_empty()
        ));

        fs::remove_file(&backup_path).unwrap();
        fs::write(&backup_path, b"corrupt backup").unwrap();
        let mut corrupt_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert!(resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut corrupt_source,
        )
        .is_err());
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
        assert_eq!(fs::read(&backup_path).unwrap(), b"corrupt backup");
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. }
                if backups.is_empty()
        ));

        fs::remove_file(&backup_path).unwrap();
        let complete_staging = directory.path().join("complete-backup-staging");
        fs::create_dir_all(&complete_staging).unwrap();
        let completed_backup = create_and_verify_peer_bidirectional_backup_v1_report(
            &backup_path,
            &complete_staging,
            &cas,
            &mut store,
            2,
            &lossless_source_binding(
                &conflict.operation_id,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
            ),
            &NeverCancelled,
        )
        .unwrap();
        let completed_receipt = PeerBidirectionalBackupReceipt {
            package_id: completed_backup.archive_sha256.clone(),
            side: PeerBidirectionalBackupSide::Local,
            path: backup_path.to_string_lossy().into_owned(),
        };
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let mut retained_with_receipt = journal.load().unwrap().unwrap();
        match &mut retained_with_receipt {
            PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. } => {
                backups.push(completed_receipt.clone());
            }
            other => panic!("unexpected retained operation: {other:?}"),
        }
        journal.store(&retained_with_receipt).unwrap();
        drop(store);
        let mut store = PersistentStore::open(directory.path()).unwrap();

        fs::remove_file(&backup_path).unwrap();
        let mut missing_backup_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert!(resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut missing_backup_source,
        )
        .is_err());
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.read_root(None).unwrap().value, lossless_root("Local"));
        assert_eq!(journal.load().unwrap(), Some(retained_with_receipt.clone()));

        create_and_verify_peer_bidirectional_backup_v1_report(
            &backup_path,
            &complete_staging,
            &cas,
            &mut store,
            2,
            &lossless_source_binding(
                &conflict.operation_id,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
            ),
            &NeverCancelled,
        )
        .unwrap();
        fs::write(&backup_path, b"corrupt persisted backup").unwrap();
        let mut corrupt_persisted_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert!(resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut corrupt_persisted_source,
        )
        .is_err());
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(fs::read(&backup_path).unwrap(), b"corrupt persisted backup");
        assert_eq!(journal.load().unwrap(), Some(retained_with_receipt.clone()));

        fs::remove_file(&backup_path).unwrap();
        create_and_verify_peer_bidirectional_backup_v1_report(
            &backup_path,
            &complete_staging,
            &cas,
            &mut store,
            2,
            &lossless_source_binding(
                &conflict.operation_id,
                PeerBidirectionalBackupSide::Local,
                &local_generation,
            ),
            &NeverCancelled,
        )
        .unwrap();
        let mut wrong_hash_receipt = retained_with_receipt.clone();
        match &mut wrong_hash_receipt {
            PeerBidirectionalDurableOperation::AwaitingConflict { backups, .. } => {
                backups[0].package_id = "0".repeat(64);
            }
            other => panic!("unexpected retained operation: {other:?}"),
        }
        journal.store(&wrong_hash_receipt).unwrap();
        let mut wrong_hash_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert!(resolve_bidirectional_conflict(
            &mut store,
            &cas,
            directory.path(),
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Remote,
            2,
            &remote.manifest_bytes,
            &mut wrong_hash_source,
        )
        .is_err());
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(journal.load().unwrap(), Some(wrong_hash_receipt));
        journal.store(&retained_with_receipt).unwrap();

        let mut resolution_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert_eq!(
            resolve_bidirectional_conflict(
                &mut store,
                &cas,
                directory.path(),
                &conflict.operation_id,
                PeerBidirectionalConflictWinner::Remote,
                2,
                &remote.manifest_bytes,
                &mut resolution_source,
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );

        assert_eq!(store.revision().unwrap(), 3);
        assert_eq!(
            store.read_root(None).unwrap().value,
            lossless_root("Remote")
        );
        let retained = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        match retained {
            PeerBidirectionalDurableOperation::LocalCommitted { backups, .. } => {
                assert_eq!(backups.len(), 1);
                assert_eq!(backups[0].side, PeerBidirectionalBackupSide::Local);
                assert_eq!(backups[0].package_id, completed_backup.archive_sha256);
                assert!(Path::new(&backups[0].path).is_file());
            }
            other => panic!("unexpected retained operation: {other:?}"),
        }
    }

    #[test]
    fn local_winner_backs_up_remote_before_replacement_and_retains_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let local_root = directory.path().join("local");
        fs::create_dir(&local_root).unwrap();
        let local_cas = PayloadCas::new(&local_root).unwrap();
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        seed_lossless_backup_fixture(&mut local_store, &local_cas);
        let base = local_store
            .seal_or_initialize_active_logical_generation(&local_cas)
            .unwrap();
        drop(local_store);
        let remote_root = directory.path().join("remote");
        copy_tree(&local_root, &remote_root);

        let remote_cas = PayloadCas::new(&remote_root).unwrap();
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        let mut remote_store = PersistentStore::open(&remote_root).unwrap();
        let local_device = "123e4567-e89b-42d3-a456-426614174027";
        let remote_device = "123e4567-e89b-42d3-a456-426614174028";
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        for (store, cas, peer_id) in [
            (&mut local_store, &local_cas, remote_device),
            (&mut remote_store, &remote_cas, local_device),
        ] {
            establish_logical_common_base(
                store,
                cas,
                peer_id,
                PRODUCT_LOGICAL_LIBRARY_ID,
                &base.manifest.generation,
                1,
                &base.manifest_bytes,
            )
            .unwrap();
            store
                .attach_verified_sync_device_at_common_base(
                    VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        peer_id,
                        common.clone(),
                        1,
                    )
                    .unwrap(),
                    1,
                )
                .unwrap();
        }
        local_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(lossless_root("Local")),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        remote_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(lossless_root("Remote")),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote = remote_store
            .seal_or_initialize_active_logical_generation(&remote_cas)
            .unwrap();
        let credential = LanBidirectionalLogicalCredential {
            endpoint: "http://192.168.1.2:32146".to_owned(),
            session_id: "123e4567-e89b-42d3-a456-426614174029".to_owned(),
            manifest_id: remote.manifest_hash.clone(),
            device_id: local_device.to_owned(),
            source_device_id: remote_device.to_owned(),
            bearer: "f".repeat(64),
        };
        let mut remote_source = LogicalDeltaSourceSession::open(
            &remote_root,
            &remote_root,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        let conflict = match begin_bidirectional_local_merge(
            &mut local_store,
            &local_cas,
            &local_root,
            credential,
            2,
            &remote.manifest_bytes,
            &mut remote_source,
        )
        .unwrap()
        {
            LocalMergeOutcome::Conflict(conflict) => conflict,
            other => panic!("unexpected merge outcome: {other:?}"),
        };
        assert_eq!(local_store.revision().unwrap(), 2);
        assert_eq!(
            local_store.read_root(None).unwrap().value,
            lossless_root("Local")
        );

        let mut wrong_source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        assert!(resolve_bidirectional_conflict(
            &mut local_store,
            &local_cas,
            &local_root,
            "123e4567-e89b-42d3-a456-426614174030",
            PeerBidirectionalConflictWinner::Local,
            2,
            &remote.manifest_bytes,
            &mut wrong_source,
        )
        .is_err());
        assert_eq!(wrong_source.reads, 0);
        assert_eq!(local_store.revision().unwrap(), 2);
        assert_eq!(
            local_store.read_root(None).unwrap().value,
            lossless_root("Local")
        );

        drop(remote_source);
        drop(local_store);
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        let mut resolution_source = LogicalDeltaSourceSession::open(
            &remote_root,
            &remote_root,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        assert_eq!(
            resolve_bidirectional_conflict(
                &mut local_store,
                &local_cas,
                &local_root,
                &conflict.operation_id,
                PeerBidirectionalConflictWinner::Local,
                2,
                &remote.manifest_bytes,
                &mut resolution_source,
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );
        assert_eq!(local_store.revision().unwrap(), 2);
        assert_eq!(
            local_store.read_root(None).unwrap().value,
            lossless_root("Local")
        );
        drop(local_store);
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(&local_root)
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::LocalCommitted {
                remote_backup_required: true,
                ..
            }
        ));
        assert_eq!(
            resume_bidirectional_local_committed_with_remote(
                &mut local_store,
                &local_cas,
                &local_root,
                &conflict.operation_id,
                |_context, _revision, _shared, _manifest, remote_backup_required| {
                    assert!(remote_backup_required);
                    Err(PeerSyncError::Transport(
                        "response lost before remote backup receipt".to_owned(),
                    ))
                },
            )
            .unwrap(),
            ResumeLocalCommittedOutcome::SourceUnavailable {
                operation_id: conflict.operation_id.clone(),
                committed_revision: 2,
            }
        );
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(&local_root)
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::LocalCommitted {
                remote_backup_required: true,
                ..
            }
        ));
        let mut stale_source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        assert!(resolve_bidirectional_conflict(
            &mut local_store,
            &local_cas,
            &local_root,
            &conflict.operation_id,
            PeerBidirectionalConflictWinner::Local,
            2,
            &remote.manifest_bytes,
            &mut stale_source,
        )
        .is_err());
        assert_eq!(stale_source.reads, 0);
        assert_eq!(local_store.revision().unwrap(), 2);

        let shared = local_store
            .seal_or_initialize_active_logical_generation(&local_cas)
            .unwrap();
        let shared_identity = SyncGenerationIdentity {
            generation_id: shared.manifest.generation.clone(),
            manifest_hash: shared.manifest_hash.clone(),
            generation_sequence: shared.manifest.generation_sequence.clone(),
        };
        let remote_identity = SyncGenerationIdentity {
            generation_id: remote.manifest.generation.clone(),
            manifest_hash: remote.manifest_hash.clone(),
            generation_sequence: remote.manifest.generation_sequence.clone(),
        };
        let mut shared_source = LogicalDeltaSourceSession::open(
            &local_root,
            &local_root,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &shared.manifest.generation,
        )
        .unwrap();
        let first_receipt = apply_bidirectional_remote_shared(
            &mut remote_store,
            &remote_cas,
            &remote_root,
            &conflict.operation_id,
            local_device,
            2,
            &common.manifest_hash,
            &remote_identity,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            },
            &shared.manifest_bytes,
            &mut shared_source,
            true,
        )
        .unwrap();
        let first_backup = first_receipt.backup.unwrap();
        assert_eq!(first_backup.package_id.len(), 64);
        assert!(Path::new(&first_backup.path).is_file());
        assert_eq!(remote_store.revision().unwrap(), 3);
        assert_eq!(
            remote_store.read_root(None).unwrap().value,
            lossless_root("Local")
        );

        drop(shared_source);
        drop(resolution_source);
        drop(remote_store);
        let mut remote_store = PersistentStore::open(&remote_root).unwrap();
        let mut retry_source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        let recovered_receipt = apply_bidirectional_remote_shared(
            &mut remote_store,
            &remote_cas,
            &remote_root,
            &conflict.operation_id,
            local_device,
            2,
            &common.manifest_hash,
            &remote_identity,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            },
            &shared.manifest_bytes,
            &mut retry_source,
            true,
        )
        .unwrap();
        assert_eq!(retry_source.reads, 0);
        assert_eq!(recovered_receipt.transferred_objects, 0);
        assert_eq!(recovered_receipt.transferred_bytes, 0);
        assert_eq!(recovered_receipt.backup, Some(first_backup.clone()));

        let result = complete_bidirectional_local_after_remote_apply(
            &mut local_store,
            &local_cas,
            &local_root,
            &conflict.operation_id,
            recovered_receipt,
        )
        .unwrap();
        assert_eq!(result.kind, "updated");
        assert_eq!(result.backups.len(), 1);
        assert_eq!(result.backups[0].side, PeerBidirectionalBackupSide::Remote);
        assert_eq!(result.backups[0].package_id, first_backup.package_id);

        drop(local_store);
        let retained = PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap()
            .unwrap();
        match retained {
            PeerBidirectionalDurableOperation::Completed {
                result: retained, ..
            } => {
                assert_eq!(retained.backups, result.backups);
            }
            other => panic!("unexpected retained operation: {other:?}"),
        }
    }

    #[test]
    fn disjoint_merge_commits_shared_a_and_retains_peer_state_at_c() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174012";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side": "local"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote = remote_disjoint_manifest(&base.manifest);
        let remote_object_bytes = remote
            .record_objects
            .iter()
            .find(|record| record.key != "r1:root")
            .unwrap()
            .object
            .size;
        let credential = LanBidirectionalLogicalCredential {
            endpoint: "http://192.168.1.2:32146".to_owned(),
            session_id: "123e4567-e89b-42d3-a456-426614174010".to_owned(),
            manifest_id: remote.manifest_hash.clone(),
            device_id: "123e4567-e89b-42d3-a456-426614174011".to_owned(),
            source_device_id: peer_id.to_owned(),
            bearer: "d".repeat(64),
        };
        let mut source = fixture_source(&remote);

        assert_eq!(
            begin_bidirectional_local_merge(
                &mut store,
                &cas,
                directory.path(),
                credential,
                1,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );

        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(
            store.read_root(None).unwrap().value,
            json!({"side": "local"})
        );
        assert_eq!(
            store
                .read_plugin_storage("remote-key", None)
                .unwrap()
                .unwrap()
                .value,
            json!({"side": "remote"})
        );
        assert_eq!(source.reads, 1);
        assert_eq!(
            store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, peer_id)
                .unwrap()
                .shared_identity,
            common
        );
        let retained = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        match retained {
            PeerBidirectionalDurableOperation::LocalCommitted {
                committed_revision,
                shared_generation,
                transferred_objects,
                transferred_bytes,
                backups,
                ..
            } => {
                assert_eq!(committed_revision, 2);
                assert_eq!(shared_generation.generation_sequence, "2");
                assert_eq!(transferred_objects, 1);
                assert_eq!(transferred_bytes, remote_object_bytes);
                assert!(backups.is_empty());
            }
            other => panic!("unexpected retained operation: {other:?}"),
        }
    }

    #[test]
    fn remote_apply_response_loss_reissues_shared_a_without_redownload() {
        let directory = tempfile::tempdir().unwrap();
        let bootstrap_root = directory.path().join("bootstrap");
        fs::create_dir(&bootstrap_root).unwrap();
        let bootstrap_cas = PayloadCas::new(&bootstrap_root).unwrap();
        let mut bootstrap_store = PersistentStore::open(&bootstrap_root).unwrap();
        let base = bootstrap_store
            .seal_or_initialize_active_logical_generation(&bootstrap_cas)
            .unwrap();
        drop(bootstrap_store);
        let local_root = directory.path().join("local");
        let remote_root = directory.path().join("remote");
        copy_tree(&bootstrap_root, &local_root);
        copy_tree(&bootstrap_root, &remote_root);

        let local_cas = PayloadCas::new(&local_root).unwrap();
        let remote_cas = PayloadCas::new(&remote_root).unwrap();
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        let mut remote_store = PersistentStore::open(&remote_root).unwrap();
        let local_device = "123e4567-e89b-42d3-a456-426614174021";
        let remote_device = "123e4567-e89b-42d3-a456-426614174022";
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        for (store, cas, peer_id) in [
            (&mut local_store, &local_cas, remote_device),
            (&mut remote_store, &remote_cas, local_device),
        ] {
            establish_logical_common_base(
                store,
                cas,
                peer_id,
                PRODUCT_LOGICAL_LIBRARY_ID,
                &base.manifest.generation,
                0,
                &base.manifest_bytes,
            )
            .unwrap();
            store
                .attach_verified_sync_device_at_common_base(
                    VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                        PRODUCT_LOGICAL_LIBRARY_ID,
                        peer_id,
                        common.clone(),
                        0,
                    )
                    .unwrap(),
                    0,
                )
                .unwrap();
        }
        local_store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side": "local"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        remote_store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                    key: "remote-key".to_owned(),
                    value: json!({"side": "remote"}),
                }]),
                asset_owner_heads: None,
            })
            .unwrap();
        let remote_generation = remote_store
            .seal_or_initialize_active_logical_generation(&remote_cas)
            .unwrap();
        let mut remote_source = LogicalDeltaSourceSession::open(
            &remote_root,
            &remote_root,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote_generation.manifest.generation,
        )
        .unwrap();
        let credential = LanBidirectionalLogicalCredential {
            endpoint: "http://192.168.1.2:32146".to_owned(),
            session_id: "123e4567-e89b-42d3-a456-426614174020".to_owned(),
            manifest_id: remote_generation.manifest_hash.clone(),
            device_id: local_device.to_owned(),
            source_device_id: remote_device.to_owned(),
            bearer: "e".repeat(64),
        };
        assert_eq!(
            begin_bidirectional_local_merge(
                &mut local_store,
                &local_cas,
                &local_root,
                credential,
                1,
                &remote_generation.manifest_bytes,
                &mut remote_source,
            )
            .unwrap(),
            LocalMergeOutcome::LocalCommitted
        );
        let operation_id = match PeerBidirectionalOperationJournal::new(&local_root)
            .load()
            .unwrap()
            .unwrap()
        {
            PeerBidirectionalDurableOperation::LocalCommitted { context, .. } => {
                context.operation_id
            }
            other => panic!("unexpected retained operation: {other:?}"),
        };
        let shared = local_store
            .seal_or_initialize_active_logical_generation(&local_cas)
            .unwrap();
        let shared_identity = SyncGenerationIdentity {
            generation_id: shared.manifest.generation.clone(),
            manifest_hash: shared.manifest_hash.clone(),
            generation_sequence: shared.manifest.generation_sequence.clone(),
        };
        let remote_identity = SyncGenerationIdentity {
            generation_id: remote_generation.manifest.generation.clone(),
            manifest_hash: remote_generation.manifest_hash.clone(),
            generation_sequence: remote_generation.manifest.generation_sequence.clone(),
        };
        let mut shared_source = LogicalDeltaSourceSession::open(
            &local_root,
            &local_root,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &shared.manifest.generation,
        )
        .unwrap();

        let receipt = apply_bidirectional_remote_shared(
            &mut remote_store,
            &remote_cas,
            &remote_root,
            &operation_id,
            local_device,
            1,
            &common.manifest_hash,
            &remote_identity,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            },
            &shared.manifest_bytes,
            &mut shared_source,
            false,
        )
        .unwrap();

        assert_eq!(receipt.committed_revision, 2);
        assert_eq!(
            receipt.committed_generation,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            }
        );
        assert_eq!(receipt.transferred_objects, 1);
        assert!(receipt.transferred_bytes > 0);
        assert!(receipt.backup.is_none());
        assert_eq!(
            remote_store.read_root(None).unwrap().value,
            json!({"side": "local"})
        );
        assert_eq!(
            remote_store
                .read_plugin_storage("remote-key", None)
                .unwrap()
                .unwrap()
                .value,
            json!({"side": "remote"})
        );
        let remote_ack = remote_store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, local_device)
            .unwrap();
        assert_eq!(remote_ack.shared_identity, shared_identity);
        assert_ne!(remote_ack.local_identity, remote_ack.shared_identity);

        drop(shared_source);
        drop(remote_source);
        drop(remote_store);
        drop(local_store);
        let mut remote_store = PersistentStore::open(&remote_root).unwrap();
        let mut local_store = PersistentStore::open(&local_root).unwrap();
        let mut retry_source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        let recovered_receipt = apply_bidirectional_remote_shared(
            &mut remote_store,
            &remote_cas,
            &remote_root,
            &operation_id,
            local_device,
            1,
            &common.manifest_hash,
            &remote_identity,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            },
            &shared.manifest_bytes,
            &mut retry_source,
            false,
        )
        .unwrap();
        assert_eq!(remote_store.revision().unwrap(), 2);
        assert_eq!(recovered_receipt.committed_revision, 2);
        assert_eq!(
            recovered_receipt.committed_generation,
            LanBidirectionalGeneration {
                generation_id: shared_identity.generation_id.clone(),
                manifest_hash: shared_identity.manifest_hash.clone(),
                generation_sequence: shared_identity.generation_sequence.clone(),
            }
        );
        assert_eq!(recovered_receipt.transferred_objects, 0);
        assert_eq!(recovered_receipt.transferred_bytes, 0);
        assert!(recovered_receipt.backup.is_none());
        assert_eq!(retry_source.reads, 0);

        let result = complete_bidirectional_local_after_remote_apply(
            &mut local_store,
            &local_cas,
            &local_root,
            &operation_id,
            recovered_receipt,
        )
        .unwrap();

        assert_eq!(result.kind, "updated");
        assert_eq!(result.operation_id, operation_id);
        assert_eq!(result.revision, 2);
        assert_eq!(result.remote_revision, 2);
        assert_eq!(result.transferred_objects, 1);
        assert!(result.transferred_bytes > 0);
        assert!(result.backups.is_empty());
        let local_ack = local_store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, remote_device)
            .unwrap();
        assert_eq!(local_ack.shared_identity, shared_identity);
        assert_eq!(local_ack.local_identity, shared_identity);
        assert_eq!(
            PeerBidirectionalOperationJournal::new(&local_root)
                .load()
                .unwrap()
                .unwrap(),
            PeerBidirectionalDurableOperation::Completed {
                schema: OPERATION_SCHEMA.to_owned(),
                remote_apply_receipt: None,
                source_binding: None,
                result: result.clone(),
            }
        );

        remote_store
            .revoke_sync_device(PRODUCT_LOGICAL_LIBRARY_ID, local_device, &shared_identity)
            .unwrap();
        let mut invalid_retry_source = FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        };
        assert!(matches!(
            apply_bidirectional_remote_shared(
                &mut remote_store,
                &remote_cas,
                &remote_root,
                &operation_id,
                local_device,
                1,
                &common.manifest_hash,
                &remote_identity,
                LanBidirectionalGeneration {
                    generation_id: shared_identity.generation_id.clone(),
                    manifest_hash: shared_identity.manifest_hash.clone(),
                    generation_sequence: shared_identity.generation_sequence.clone(),
                },
                &shared.manifest_bytes,
                &mut invalid_retry_source,
                false,
            ),
            Err(PeerSyncError::ActivationConflict { .. })
        ));
        assert_eq!(invalid_retry_source.reads, 0);
    }

    #[test]
    fn production_control_attaches_only_an_exact_existing_p4_common_base() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let target_device_id = "123e4567-e89b-42d3-a456-426614174080";
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let inspector = store.open_native_job_store().unwrap();
        let source_session_id = "123e4567-e89b-42d3-a456-426614174081";
        let source_device_id = "123e4567-e89b-42d3-a456-426614174082";
        let control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            store,
            source_session_id,
            source_device_id,
        );
        let common = LanBidirectionalGeneration {
            generation_id: base.manifest.generation,
            manifest_hash: base.manifest_hash,
            generation_sequence: base.manifest.generation_sequence,
        };
        let session = super::super::lan::LanBidirectionalSession {
            session_id: source_session_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };

        control
            .register(
                session.clone(),
                super::super::lan::LanBidirectionalRegistrationRequest {
                    library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                    generation: common.clone(),
                    expected_revision: 0,
                },
            )
            .unwrap();
        assert_eq!(
            inspector
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap()
                .shared_identity
                .manifest_hash,
            common.manifest_hash
        );

        let mismatched = LanBidirectionalGeneration {
            generation_id: "mismatched-base".to_owned(),
            manifest_hash: "a".repeat(64),
            generation_sequence: "0".to_owned(),
        };
        let error = control
            .register(
                session.clone(),
                super::super::lan::LanBidirectionalRegistrationRequest {
                    library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                    generation: mismatched,
                    expected_revision: 0,
                },
            )
            .unwrap_err();
        assert!(matches!(error, PeerSyncError::Storage(_)));
        assert_eq!(
            inspector
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap()
                .shared_identity
                .manifest_hash,
            common.manifest_hash
        );

        let unknown_target = "123e4567-e89b-42d3-a456-426614174083";
        let error = control
            .register(
                super::super::lan::LanBidirectionalSession {
                    target_device_id: unknown_target.to_owned(),
                    ..session
                },
                super::super::lan::LanBidirectionalRegistrationRequest {
                    library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                    generation: common,
                    expected_revision: 0,
                },
            )
            .unwrap_err();
        assert!(matches!(error, PeerSyncError::Storage(_)));
        assert!(inspector
            .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, unknown_target)
            .unwrap()
            .is_none());
    }

    #[test]
    fn source_prepared_precommit_descendant_is_abandoned_without_losing_the_edit() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let source_device_id = "123e4567-e89b-42d3-a456-426614174086";
        let target_device_id = "123e4567-e89b-42d3-a456-426614174087";
        let operation_id = "123e4567-e89b-42d3-a456-426614174088";
        let previous = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    target_device_id,
                    previous.clone(),
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let evidence = SourcePreparedEvidence {
            operation_id: operation_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
            expected_source_revision: 0,
            previous_shared: previous.clone(),
            expected_source_generation: previous.clone(),
            shared_generation: LanBidirectionalGeneration {
                generation_id: "123e4567-e89b-42d3-a456-426614174089".to_owned(),
                manifest_hash: "a".repeat(64),
                generation_sequence: "1".to_owned(),
            },
            incoming_revision: 1,
            transferred_objects: 2,
            transferred_bytes: 19,
            backup: None,
        };
        let durable_job_id = "123e4567-e89b-42d3-a456-426614174090";
        let mut job = DurableCasJob::begin(
            directory.path(),
            durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .unwrap();
        job.seal(&mut store, 0).unwrap();
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&evidence.durable_operation(durable_job_id.to_owned()))
            .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"preserved": "source-local-edit"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let session = LanBidirectionalSession {
            session_id: "123e4567-e89b-42d3-a456-426614174091".to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: operation_id.to_owned(),
            source_endpoint: "http://127.0.0.1:1".to_owned(),
            source_session_id: "123e4567-e89b-42d3-a456-426614174092".to_owned(),
            source_manifest_id: "manifest".to_owned(),
            source_claim: "claim".to_owned(),
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: previous.generation_id.clone(),
                manifest_hash: previous.manifest_hash.clone(),
                generation_sequence: previous.generation_sequence.clone(),
            },
            expected_common_base_manifest_hash: previous.manifest_hash.clone(),
            backup_losing_side: false,
        };

        drop(store);
        let control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            PersistentStore::open(directory.path()).unwrap(),
            &session.session_id,
            source_device_id,
        );
        let error = control.remote_apply(session, request).unwrap_err();
        drop(control);
        let store = PersistentStore::open(directory.path()).unwrap();

        assert!(matches!(
            error,
            PeerSyncError::StaleManifest { .. } | PeerSyncError::ActivationConflict { .. }
        ));
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value["preserved"],
            "source-local-edit"
        );
        assert_eq!(
            store
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap(),
            Some(previous.clone())
        );
        assert_eq!(
            store
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap()
                .shared_identity,
            previous
        );
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(
            DurableCasJob::open(directory.path(), durable_job_id)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn source_activation_crash_reopens_with_the_exact_prepared_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        seed_lossless_backup_fixture(&mut store, &cas);
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let target_device_id = "123e4567-e89b-42d3-a456-426614174092";
        let source_device_id = "123e4567-e89b-42d3-a456-426614174094";
        establish_logical_common_base(
            &mut store,
            &cas,
            target_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            1,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    target_device_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                1,
            )
            .unwrap();
        establish_logical_common_base(
            &mut store,
            &cas,
            source_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            1,
            &base.manifest_bytes,
        )
        .unwrap();
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    source_device_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                1,
            )
            .unwrap();
        drop(store);
        let remote_directory = tempfile::tempdir().unwrap();
        copy_tree(directory.path(), remote_directory.path());
        let remote_cas = PayloadCas::new(remote_directory.path()).unwrap();
        let mut remote_store = PersistentStore::open(remote_directory.path()).unwrap();
        remote_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                    key: "remote-key".to_owned(),
                    value: json!({"side": "remote"}),
                }]),
                asset_owner_heads: None,
            })
            .unwrap();
        let remote = remote_store
            .seal_or_initialize_active_logical_generation(&remote_cas)
            .unwrap();
        drop(remote_store);
        let inspector = PersistentStore::open(directory.path()).unwrap();
        let source_session_id = "123e4567-e89b-42d3-a456-426614174093";
        let control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            PersistentStore::open(directory.path()).unwrap(),
            source_session_id,
            source_device_id,
        );
        let temporary_session_id = "123e4567-e89b-42d3-a456-426614174095";
        let remote_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &remote.manifest.generation,
        )
        .unwrap();
        let prepared = super::super::lan::PreparedLogicalLanSession::new(
            temporary_session_id,
            target_device_id,
            remote.manifest_hash.clone(),
            remote.manifest_bytes.clone(),
            remote
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(remote_source),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared);
        let pairing = host.start().unwrap();
        let mut request = LanBidirectionalRemoteApplyRequest {
            operation_id: "123e4567-e89b-42d3-a456-426614174096".to_owned(),
            source_endpoint: format!("http://127.0.0.1:{}", host.address().unwrap().port()),
            source_session_id: pairing.session_id,
            source_manifest_id: pairing.manifest_id,
            source_claim: pairing.claim,
            expected_source_revision: 1,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: common.generation_id.clone(),
                manifest_hash: common.manifest_hash.clone(),
                generation_sequence: common.generation_sequence.clone(),
            },
            expected_common_base_manifest_hash: common.manifest_hash.clone(),
            backup_losing_side: true,
        };
        let session = LanBidirectionalSession {
            session_id: source_session_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        };

        SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
        let error = control
            .remote_apply(session.clone(), request.clone())
            .unwrap_err();
        assert!(
            matches!(error, PeerSyncError::Storage(message) if message.contains("source-prepared"))
        );
        assert_eq!(inspector.revision().unwrap(), 1);
        assert_eq!(
            inspector
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap(),
            Some(common.clone())
        );
        assert_eq!(
            inspector
                .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, target_device_id)
                .unwrap()
                .shared_identity,
            common
        );
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        host.stop().unwrap();

        let mut retry_remote_store = PersistentStore::open(remote_directory.path()).unwrap();
        retry_remote_store
            .commit(&WorkingSetCommit {
                expected_revision: 2,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                    key: "remote-key-2".to_owned(),
                    value: json!({"side": "remote-2"}),
                }]),
                asset_owner_heads: None,
            })
            .unwrap();
        let retry_remote = retry_remote_store
            .seal_or_initialize_active_logical_generation(&remote_cas)
            .unwrap();
        drop(retry_remote_store);
        let retry_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &retry_remote.manifest.generation,
        )
        .unwrap();
        let retry_prepared = super::super::lan::PreparedLogicalLanSession::new(
            "123e4567-e89b-42d3-a456-426614174109",
            target_device_id,
            retry_remote.manifest_hash.clone(),
            retry_remote.manifest_bytes.clone(),
            retry_source.objects().to_vec(),
            Box::new(retry_source),
        )
        .unwrap();
        let mut retry_host = LanCloneHost::prepare_logical(retry_prepared);
        let retry_pairing = retry_host.start().unwrap();
        request.source_endpoint =
            format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
        request.source_session_id = retry_pairing.session_id;
        request.source_manifest_id = retry_pairing.manifest_id;
        request.source_claim = retry_pairing.claim;

        SOURCE_AFTER_PREPARED_STORE_PANIC.with(|enabled| enabled.set(true));
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control.remote_apply(session.clone(), request.clone())
        }));
        assert!(crashed.is_err());
        drop(control);
        let crashed_prepared = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        let crashed_job_id = match &crashed_prepared {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                durable_job_id.clone()
            }
            other => panic!("expected source-prepared crash journal, got {other:?}"),
        };
        let crashed_job = DurableCasJob::open(directory.path(), &crashed_job_id).unwrap();
        assert!(!crashed_job.is_sealed());
        assert!(crashed_job.pin_count() > 0);
        let crashed_roots = collect_durable_cas_job_roots(directory.path());
        assert!(!crashed_roots.object_hashes.is_empty());
        let retained_object_hash = crashed_roots.object_hashes.iter().next().unwrap().clone();
        let retained_object_size = cas.stat_object(&retained_object_hash).unwrap().unwrap();
        assert!(crashed_roots
            .blockers
            .contains(&format!("job-pin-unsealed:{crashed_job_id}")));
        assert_eq!(
            crashed_roots
                .blockers
                .iter()
                .filter(|blocker| blocker.starts_with("job-pin-unsealed:"))
                .count(),
            1
        );
        retry_host.stop().unwrap();
        let handoff_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &retry_remote.manifest.generation,
        )
        .unwrap();
        let handoff_prepared = super::super::lan::PreparedLogicalLanSession::new(
            &uuid::Uuid::new_v4().to_string(),
            target_device_id,
            retry_remote.manifest_hash.clone(),
            retry_remote.manifest_bytes.clone(),
            handoff_source.objects().to_vec(),
            Box::new(handoff_source),
        )
        .unwrap();
        retry_host = LanCloneHost::prepare_logical(handoff_prepared);
        let handoff_pairing = retry_host.start().unwrap();
        request.source_endpoint =
            format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
        request.source_session_id = handoff_pairing.session_id;
        request.source_manifest_id = handoff_pairing.manifest_id;
        request.source_claim = handoff_pairing.claim;

        let recovery_control = ProductionLanBidirectionalControl::new(
            directory.path().to_path_buf(),
            PersistentStore::open(directory.path()).unwrap(),
            source_session_id,
            source_device_id,
        );
        SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
        let handoff_error = recovery_control
            .remote_apply(session.clone(), request.clone())
            .unwrap_err();
        assert!(
            matches!(&handoff_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
            "{handoff_error:?}"
        );
        let retained_old_job = DurableCasJob::open(directory.path(), &crashed_job_id).unwrap();
        assert!(!retained_old_job.is_sealed());
        assert_eq!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(crashed_prepared)
        );
        let retained_roots = collect_durable_cas_job_roots(directory.path());
        assert!(retained_roots.object_hashes.contains(&retained_object_hash));
        assert!(retained_roots
            .blockers
            .contains(&format!("job-pin-unsealed:{crashed_job_id}")));
        drop(retained_old_job);
        let mut sealed_retry_job = DurableCasJob::open(directory.path(), &crashed_job_id).unwrap();
        let mut sealed_retry_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &retry_remote.manifest.generation,
        )
        .unwrap();
        for object in &retry_remote.manifest.objects {
            match cas.stat_object(&object.hash).unwrap() {
                Some(size) => sealed_retry_job
                    .pin_existing(&cas, &object.hash, size, CasObjectRole::DirectObject)
                    .unwrap(),
                None => {
                    let mut reader = sealed_retry_source
                        .open_object(&LogicalDeltaObject {
                            hash: object.hash.clone(),
                            size: object.size,
                        })
                        .unwrap();
                    let prepared = sealed_retry_job
                        .prepare_reader(&cas, &mut reader, CasObjectRole::DirectObject)
                        .unwrap();
                    assert_eq!(prepared.content_hash, object.hash);
                    assert_eq!(prepared.byte_size, object.size);
                }
            }
        }
        let mut seal_store = PersistentStore::open(directory.path()).unwrap();
        sealed_retry_job.seal(&mut seal_store, 0).unwrap();
        drop(seal_store);
        let sealed_roots = sealed_retry_job.root_set().unwrap();
        assert!(sealed_roots
            .object_hashes
            .contains(&retry_remote.manifest_hash));
        assert!(!collect_durable_cas_job_roots(directory.path())
            .blockers
            .contains(&format!("job-pin-unsealed:{crashed_job_id}")));
        retry_host.stop().unwrap();
        let observer_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &retry_remote.manifest.generation,
        )
        .unwrap();
        let observer_prepared = super::super::lan::PreparedLogicalLanSession::new(
            &uuid::Uuid::new_v4().to_string(),
            target_device_id,
            retry_remote.manifest_hash.clone(),
            retry_remote.manifest_bytes.clone(),
            observer_source.objects().to_vec(),
            Box::new(observer_source),
        )
        .unwrap();
        retry_host = LanCloneHost::prepare_logical(observer_prepared);
        let observer_pairing = retry_host.start().unwrap();
        request.source_endpoint =
            format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
        request.source_session_id = observer_pairing.session_id;
        request.source_manifest_id = observer_pairing.manifest_id;
        request.source_claim = observer_pairing.claim;

        SOURCE_AFTER_PREPARED_CALLBACK_FAILPOINT.with(|enabled| enabled.set(true));
        let observer_error = recovery_control
            .remote_apply(session.clone(), request.clone())
            .unwrap_err();
        assert!(
            matches!(&observer_error, PeerSyncError::Storage(message) if message.contains("pre-activation")),
            "{observer_error:?}"
        );
        assert_eq!(
            durable_job_journal_ids(directory.path()),
            vec![crashed_job_id.clone()]
        );
        assert!(DurableCasJob::open(directory.path(), &crashed_job_id)
            .unwrap()
            .is_sealed());
        retry_host.stop().unwrap();
        let completion_source = LogicalDeltaSourceSession::open(
            remote_directory.path(),
            remote_directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &retry_remote.manifest.generation,
        )
        .unwrap();
        let completion_prepared = super::super::lan::PreparedLogicalLanSession::new(
            &uuid::Uuid::new_v4().to_string(),
            target_device_id,
            retry_remote.manifest_hash.clone(),
            retry_remote.manifest_bytes.clone(),
            completion_source.objects().to_vec(),
            Box::new(completion_source),
        )
        .unwrap();
        retry_host = LanCloneHost::prepare_logical(completion_prepared);
        let completion_pairing = retry_host.start().unwrap();
        request.source_endpoint =
            format!("http://127.0.0.1:{}", retry_host.address().unwrap().port());
        request.source_session_id = completion_pairing.session_id;
        request.source_manifest_id = completion_pairing.manifest_id;
        request.source_claim = completion_pairing.claim;

        SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.set(true));
        SOURCE_COMPLETE_STORE_FAILPOINT.with(|enabled| enabled.set(true));
        let error = recovery_control
            .remote_apply(session.clone(), request.clone())
            .unwrap_err();
        assert!(SOURCE_PREPARED_STORE_FAILPOINT.with(|enabled| enabled.replace(false)));
        assert!(
            matches!(&error, PeerSyncError::Storage(message) if message.contains("completion")),
            "{error:?}"
        );
        let prepared = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        let evidence = SourcePreparedEvidence::from_operation(&prepared).unwrap();
        let committed_job_id = match &prepared {
            PeerBidirectionalDurableOperation::SourcePrepared { durable_job_id, .. } => {
                durable_job_id.clone()
            }
            other => panic!("expected source-prepared completion journal, got {other:?}"),
        };
        assert_eq!(committed_job_id, crashed_job_id);
        assert!(evidence.transferred_objects > 0);
        assert!(evidence.transferred_bytes > 0);
        assert!(evidence.backup.is_some());
        let expected = evidence.receipt().unwrap();
        assert!(matches!(
            DurableCasJob::open(directory.path(), &crashed_job_id),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        let mut committed_source = PersistentStore::open(directory.path()).unwrap();
        let mut retained_committed_job = DurableCasJob::begin(
            directory.path(),
            &committed_job_id,
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .unwrap();
        retained_committed_job
            .pin_existing(
                &cas,
                &retained_object_hash,
                retained_object_size,
                CasObjectRole::DirectObject,
            )
            .unwrap();
        retained_committed_job
            .seal(&mut committed_source, 0)
            .unwrap();
        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&retained_object_hash));
        assert!(matches!(
            PeerBidirectionalCommandState::default().acknowledge(
                directory.path(),
                &mut committed_source,
                &request.operation_id,
            ),
            Err(PeerSyncError::Validation(message)) if message.contains("cannot be abandoned")
        ));
        assert!(PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .is_some());
        drop(committed_source);
        retry_host.stop().unwrap();
        drop(recovery_control);

        let mut source_store = PersistentStore::open(directory.path()).unwrap();
        source_store
            .commit(&WorkingSetCommit {
                expected_revision: expected.committed_revision,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                    key: "source-after-ack".to_owned(),
                    value: json!({"preserved": true}),
                }]),
                asset_owner_heads: None,
            })
            .unwrap();
        let source_active = source_store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let refreshed_source_revision = source_store.revision().unwrap();
        let source = LogicalDeltaSourceSession::open(
            directory.path(),
            directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &source_active.manifest.generation,
        )
        .unwrap();
        let (mut source_host, fresh_session_id, fresh_manifest_id) = prepare_product_source_host(
            directory.path(),
            source_store,
            source,
            source_device_id,
            source_active.manifest_bytes,
        )
        .unwrap();
        let pairing = source_host.start().unwrap();
        assert_eq!(pairing.session_id, fresh_session_id);
        assert_eq!(pairing.manifest_id, fresh_manifest_id);
        let source_endpoint = format!("http://127.0.0.1:{}", source_host.address().unwrap().port());
        let client = LanBidirectionalLogicalClient::claim(
            &source_endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            target_device_id,
        )
        .unwrap();
        assert_eq!(client.source_device_id(), source_device_id);

        let target_cas = PayloadCas::new(remote_directory.path()).unwrap();
        let mut target_store = PersistentStore::open(remote_directory.path()).unwrap();
        let target_active = target_store
            .seal_or_initialize_active_logical_generation(&target_cas)
            .unwrap();
        let target_revision = target_store.revision().unwrap();
        target_store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    source_device_id,
                    common.clone(),
                    0,
                )
                .unwrap(),
                target_revision,
            )
            .unwrap();
        let target_previous = target_store
            .sync_device_ack_state(PRODUCT_LOGICAL_LIBRARY_ID, source_device_id)
            .unwrap();
        let shared = SyncGenerationIdentity {
            generation_id: target_active.manifest.generation,
            manifest_hash: target_active.manifest_hash,
            generation_sequence: target_active.manifest.generation_sequence,
        };
        let operation_id = request.operation_id.clone();
        let durable_job_id = "123e4567-e89b-42d3-a456-426614174110";
        let mut completed_job = DurableCasJob::begin(
            remote_directory.path(),
            durable_job_id,
            CasJobKind::LogicalDeltaTarget,
            0,
        )
        .unwrap();
        completed_job.release(CasReleaseOutcome::Aborted).unwrap();
        PeerBidirectionalOperationJournal::new(remote_directory.path())
            .store(&PeerBidirectionalDurableOperation::LocalCommitted {
                schema: OPERATION_SCHEMA.to_owned(),
                context: PeerBidirectionalOperationContext {
                    operation_id: operation_id.clone(),
                    credential: client.credential(),
                    library_id: PRODUCT_LOGICAL_LIBRARY_ID.to_owned(),
                    expected_local_revision: target_revision,
                    expected_remote_revision: 1,
                    expected_remote_generation: request.expected_source_generation.clone(),
                    previous_shared: common,
                    previous_local: target_previous.local_identity,
                    durable_job_id: durable_job_id.to_owned(),
                },
                committed_revision: target_revision,
                shared_generation: shared.clone(),
                changed: true,
                remote_backup_required: true,
                remote_apply_receipt: None,
                transferred_objects: 0,
                transferred_bytes: 0,
                backups: vec![],
            })
            .unwrap();
        target_store
            .commit(&WorkingSetCommit {
                expected_revision: target_revision,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![crate::persistent_store::PluginStorageMutation::Set {
                    key: "target-after-response-loss".to_owned(),
                    value: json!({"preserved": true}),
                }]),
                asset_owner_heads: None,
            })
            .unwrap();
        target_store
            .seal_or_initialize_active_logical_generation(&target_cas)
            .unwrap();
        let refreshed_target_revision = target_store.revision().unwrap();

        DISCOVER_LAN_IPV4_OVERRIDE.with(|address| address.set(Some(Ipv4Addr::LOCALHOST)));
        let result = run_retained_remote_completion(
            &mut target_store,
            remote_directory.path(),
            &operation_id,
            refreshed_target_revision,
            Some(&client),
        )
        .unwrap();
        let PeerBidirectionalSyncResult::Updated {
            revision,
            transferred_objects,
            transferred_bytes,
            backups,
            ..
        } = result
        else {
            panic!("expected updated retained completion");
        };
        assert_eq!(revision, refreshed_target_revision);
        assert_eq!(transferred_objects, expected.transferred_objects);
        assert_eq!(transferred_bytes, expected.transferred_bytes);
        assert_eq!(backups.len(), 1);
        assert_eq!(inspector.revision().unwrap(), refreshed_source_revision);
        assert!(matches!(
            PeerBidirectionalOperationJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(PeerBidirectionalDurableOperation::Completed {
                remote_apply_receipt: Some(receipt),
                source_binding: Some(_),
                result,
                ..
            }) if receipt == expected && result.revision == refreshed_source_revision
        ));
        assert!(matches!(
            DurableCasJob::open(directory.path(), &committed_job_id),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        assert_eq!(
            target_store
                .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, source_device_id)
                .unwrap(),
            Some(shared)
        );
        source_host.stop().unwrap();
        let source_completed = PeerBidirectionalOperationJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        assert!(retained_allows_source_prepare(
            &source_completed,
            source_device_id
        ));
        PeerBidirectionalOperationJournal::new(directory.path())
            .acknowledge_completed(&operation_id)
            .unwrap();
        PeerBidirectionalOperationJournal::new(remote_directory.path())
            .acknowledge_completed(&operation_id)
            .unwrap();
    }

    #[test]
    fn command_state_excludes_source_preparation_and_target_work() {
        let state = PeerBidirectionalCommandState::default();

        let source = state.begin_source_prepare().unwrap();
        assert!(matches!(
            state.begin_target(),
            Err(PeerSyncError::Protocol(message)) if message.contains("source")
        ));
        drop(source);

        let target = state.begin_target().unwrap();
        assert!(matches!(
            state.begin_source_prepare(),
            Err(PeerSyncError::Protocol(message)) if message.contains("target")
        ));
        drop(target);

        assert!(state.begin_source_prepare().is_ok());
    }

    #[test]
    fn lane_three_capabilities_and_status_dtos_are_exact() {
        assert_eq!(
            serde_json::to_value(peer_bidirectional_capabilities()).unwrap(),
            json!({
                "desktop": true,
                "sourceReady": true,
                "atomicActivationReady": true,
                "authenticatedTransportReady": true,
                "losslessBackupReady": true,
                "durableStateReady": true,
                "productionEnabled": true,
            })
        );
        assert_eq!(
            serde_json::to_value(PeerBidirectionalSyncResult::ResumeRequired {
                operation_id: "123e4567-e89b-42d3-a456-426614174088".to_owned(),
                phase: "localCommitted",
                committed_revision: 9,
            })
            .unwrap(),
            json!({
                "kind": "resumeRequired",
                "operationId": "123e4567-e89b-42d3-a456-426614174088",
                "phase": "localCommitted",
                "committedRevision": 9,
            })
        );
        let directory = tempfile::tempdir().unwrap();
        let operation_id = "123e4567-e89b-42d3-a456-426614174084";
        PeerBidirectionalOperationJournal::new(directory.path())
            .store(&PeerBidirectionalDurableOperation::Completed {
                schema: OPERATION_SCHEMA.to_owned(),
                remote_apply_receipt: None,
                source_binding: None,
                result: PeerBidirectionalCompletedResult {
                    kind: "updated".to_owned(),
                    operation_id: operation_id.to_owned(),
                    revision: 8,
                    remote_revision: 11,
                    transferred_objects: 2,
                    transferred_bytes: 19,
                    backups: vec![backup(PeerBidirectionalBackupSide::Remote)],
                },
            })
            .unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();

        assert_eq!(
            serde_json::to_value(
                PeerBidirectionalCommandState::default()
                    .status(directory.path(), &store)
                    .unwrap()
            )
            .unwrap(),
            json!({
                "source": { "phase": "idle", "devices": [] },
                "operation": {
                    "phase": "completed",
                    "result": {
                        "kind": "updated",
                        "operationId": operation_id,
                        "revision": 8,
                        "remoteRevision": 11,
                        "transferredObjects": 2,
                        "transferredBytes": 19,
                        "backups": [{
                            "packageId": "f".repeat(64),
                            "side": "remote",
                            "path": "peer-bidirectional/backups/conflict.risulossless",
                        }],
                    },
                },
            })
        );
    }

    #[test]
    fn completed_acknowledgement_keeps_source_owned_until_explicit_stop() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let built = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let source = LogicalDeltaSourceSession::open(
            directory.path(),
            directory.path(),
            PRODUCT_LOGICAL_LIBRARY_ID,
            &built.manifest.generation,
        )
        .unwrap();
        let session_id = "123e4567-e89b-42d3-a456-426614174085";
        let prepared = super::super::lan::PreparedLogicalLanSession::new(
            session_id,
            "123e4567-e89b-42d3-a456-426614174086",
            built.manifest_hash.clone(),
            built.manifest_bytes,
            source.objects().to_vec(),
            Box::new(source),
        )
        .unwrap();
        let host = LanCloneHost::prepare_logical(prepared);
        let state = PeerBidirectionalCommandState::default();
        state
            .install_source(host, session_id, &built.manifest_hash)
            .unwrap();
        let started = state.start_source(session_id, Ipv4Addr::LOCALHOST).unwrap();
        assert_eq!(started.phase, PeerBidirectionalSourcePhase::Running);
        assert!(matches!(
            state.begin_target(),
            Err(PeerSyncError::Protocol(message)) if message.contains("source")
        ));
        let pairing_uri = url::Url::parse(started.pairing_uri.as_deref().unwrap()).unwrap();
        assert_eq!(pairing_uri.host_str(), Some("peer-sync"));
        let endpoint = pairing_uri
            .query_pairs()
            .find(|(key, _)| key == "endpoint")
            .unwrap()
            .1
            .into_owned();
        let manifest_id = pairing_uri
            .query_pairs()
            .find(|(key, _)| key == "manifest")
            .unwrap()
            .1
            .into_owned();
        let claim = pairing_uri
            .fragment()
            .unwrap()
            .strip_prefix("claim=")
            .unwrap();
        let _client = super::super::lan::LanLogicalDeltaClient::claim(
            &endpoint,
            session_id,
            &manifest_id,
            claim,
        )
        .unwrap();
        let claimed_device_id = state
            .status(directory.path(), &store)
            .unwrap()
            .source
            .devices[0]
            .device_id
            .clone();
        state
            .revoke_source_device(session_id, &claimed_device_id)
            .unwrap();
        assert!(
            state
                .status(directory.path(), &store)
                .unwrap()
                .source
                .devices[0]
                .revoked
        );
        let operation_id = "123e4567-e89b-42d3-a456-426614174087";
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        journal
            .store(&PeerBidirectionalDurableOperation::Completed {
                schema: OPERATION_SCHEMA.to_owned(),
                remote_apply_receipt: None,
                source_binding: None,
                result: PeerBidirectionalCompletedResult {
                    kind: "noChanges".to_owned(),
                    operation_id: operation_id.to_owned(),
                    revision: 0,
                    remote_revision: 0,
                    transferred_objects: 0,
                    transferred_bytes: 0,
                    backups: vec![],
                },
            })
            .unwrap();

        assert!(matches!(
            state.acknowledge(directory.path(), &mut store, operation_id),
            Err(PeerSyncError::Protocol(message)) if message.contains("source")
        ));
        assert!(journal.load().unwrap().is_some());
        assert_eq!(
            state.status(directory.path(), &store).unwrap().source.phase,
            PeerBidirectionalSourcePhase::Running
        );
        state.stop_source(session_id).unwrap();
        assert_eq!(
            state.status(directory.path(), &store).unwrap().source.phase,
            PeerBidirectionalSourcePhase::Stopped
        );
        assert!(state.begin_target().is_ok());
        state
            .acknowledge(directory.path(), &mut store, operation_id)
            .unwrap();
        assert!(journal.load().unwrap().is_none());
    }

    #[test]
    fn offline_status_projects_and_idempotently_revokes_a_durable_registered_device() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174099";
        establish_logical_common_base(
            &mut store,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation,
            manifest_hash: base.manifest_hash,
            generation_sequence: base.manifest.generation_sequence,
        };
        store
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common,
                    17,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        let state = PeerBidirectionalCommandState::default();

        let status = state.status(directory.path(), &store).unwrap();
        assert_eq!(status.source.phase, PeerBidirectionalSourcePhase::Idle);
        assert_eq!(
            status.source.devices,
            vec![PeerBidirectionalSourceDevice {
                device_id: peer_id.to_owned(),
                transferred_bytes: 0,
                current_object: None,
                last_seen_at: 17,
                revoked: false,
            }]
        );

        state
            .revoke_source_device("stopped-session", peer_id)
            .unwrap();
        revoke_durable_bidirectional_device(&mut store, peer_id).unwrap();
        revoke_durable_bidirectional_device(&mut store, peer_id).unwrap();

        let status = state.status(directory.path(), &store).unwrap();
        assert_eq!(status.source.devices.len(), 1);
        assert!(status.source.devices[0].revoked);
    }

    #[test]
    fn bidirectional_network_transfer_does_not_hold_the_managed_store_mutex() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut setup = PersistentStore::open(directory.path()).unwrap();
        let base = setup
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer_id = "123e4567-e89b-42d3-a456-426614174089";
        establish_logical_common_base(
            &mut setup,
            &cas,
            peer_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &base.manifest.generation,
            0,
            &base.manifest_bytes,
        )
        .unwrap();
        let common = SyncGenerationIdentity {
            generation_id: base.manifest.generation.clone(),
            manifest_hash: base.manifest_hash.clone(),
            generation_sequence: base.manifest.generation_sequence.clone(),
        };
        setup
            .attach_verified_sync_device_at_common_base(
                VerifiedSyncDeviceRegistration::from_authenticated_p5_receipt(
                    PRODUCT_LOGICAL_LIBRARY_ID,
                    peer_id,
                    common,
                    0,
                )
                .unwrap(),
                0,
            )
            .unwrap();
        setup
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({ "side": "local" })),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote = remote_disjoint_manifest(&base.manifest);
        let managed = Arc::new(Mutex::new(setup));
        let independent = managed.lock().unwrap().open_native_job_store().unwrap();
        let (opened_tx, opened_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let root = directory.path().to_path_buf();
        let remote_manifest = remote.manifest_bytes.clone();
        let remote_objects = remote
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect();
        let transfer = thread::spawn(move || {
            let mut store = independent;
            let cas = PayloadCas::new(&root).unwrap();
            let mut source = BlockingFixtureSource {
                objects: remote_objects,
                opened: opened_tx,
                release: release_rx,
            };
            begin_bidirectional_local_merge(
                &mut store,
                &cas,
                &root,
                LanBidirectionalLogicalCredential {
                    endpoint: "http://192.168.1.2:32146".to_owned(),
                    session_id: "123e4567-e89b-42d3-a456-426614174090".to_owned(),
                    manifest_id: remote.manifest_hash,
                    device_id: "123e4567-e89b-42d3-a456-426614174091".to_owned(),
                    source_device_id: peer_id.to_owned(),
                    bearer: "e".repeat(64),
                },
                1,
                &remote_manifest,
                &mut source,
            )
        });
        opened_rx.recv().unwrap();

        assert_eq!(
            managed.try_lock().unwrap().revision().unwrap(),
            1,
            "managed PDS must remain available while the independent coordinator reads the network",
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            transfer.join().unwrap().unwrap(),
            LocalMergeOutcome::LocalCommitted
        );
    }
}
