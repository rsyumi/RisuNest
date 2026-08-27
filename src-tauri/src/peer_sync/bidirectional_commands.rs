use super::{
    lan::{LanBidirectionalGeneration, LanBidirectionalLogicalCredential},
    PeerSyncError,
};
use crate::persistent_store::SyncGenerationIdentity;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const OPERATION_SCHEMA: &str = "risunest.peer-bidirectional-operation/v1";
const OPERATION_FILE: &str = "operation.json";
const MAX_OPERATION_BYTES: u64 = 1_048_576;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBidirectionalConflict {
    pub(crate) key: String,
    pub(crate) conflict_type: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PeerBidirectionalBackupSide {
    Local,
    Remote,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBidirectionalBackupReceipt {
    pub(crate) package_id: String,
    pub(crate) side: PeerBidirectionalBackupSide,
    pub(crate) path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerBidirectionalCompletedResult {
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
    pub(crate) expected_remote_revision: i64,
    pub(crate) expected_remote_generation: LanBidirectionalGeneration,
    pub(crate) previous_shared: SyncGenerationIdentity,
    pub(crate) previous_local: SyncGenerationIdentity,
    pub(crate) durable_job_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub(crate) enum PeerBidirectionalDurableOperation {
    AwaitingConflict {
        schema: String,
        context: PeerBidirectionalOperationContext,
        conflicts: Vec<PeerBidirectionalConflict>,
        local_manifest_hash: String,
        remote_manifest_hash: String,
    },
    LocalCommitted {
        schema: String,
        context: PeerBidirectionalOperationContext,
        committed_revision: i64,
        shared_generation: SyncGenerationIdentity,
        transferred_objects: u64,
        transferred_bytes: u64,
        backups: Vec<PeerBidirectionalBackupReceipt>,
    },
    Completed {
        schema: String,
        result: PeerBidirectionalCompletedResult,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerBidirectionalConflictStatus {
    key: String,
    #[serde(rename = "type")]
    conflict_type: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerBidirectionalConflictResult {
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
pub(crate) enum PeerBidirectionalStatusOperation {
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
            Self::AwaitingConflict { context, .. } | Self::LocalCommitted { context, .. } => {
                &context.operation_id
            }
            Self::Completed { result, .. } => &result.operation_id,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        let schema = match self {
            Self::AwaitingConflict { schema, .. }
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
            Self::AwaitingConflict { conflicts, .. } if conflicts.is_empty() => Err(
                PeerSyncError::Storage("awaiting-conflict operation has no conflicts".to_owned()),
            ),
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
            _ => Ok(()),
        }
    }

    pub(crate) fn status_projection(&self) -> PeerBidirectionalStatusOperation {
        match self {
            Self::AwaitingConflict {
                context,
                conflicts,
                local_manifest_hash,
                remote_manifest_hash,
                ..
            } => PeerBidirectionalStatusOperation::AwaitingConflict {
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
            },
            Self::LocalCommitted {
                context,
                committed_revision,
                ..
            } => PeerBidirectionalStatusOperation::LocalCommitted {
                operation_id: context.operation_id.clone(),
                committed_revision: *committed_revision,
            },
            Self::Completed { result, .. } => PeerBidirectionalStatusOperation::Completed {
                result: result.clone(),
            },
        }
    }
}

fn valid_backups(backups: &[PeerBidirectionalBackupReceipt]) -> bool {
    backups
        .iter()
        .all(|backup| !backup.package_id.is_empty() && !backup.path.is_empty())
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
            transferred_objects: 2,
            transferred_bytes: 19,
            backups: vec![backup(PeerBidirectionalBackupSide::Local)],
        };
        journal.store(&local_committed).unwrap();
        assert_eq!(journal.load().unwrap(), Some(local_committed));

        let completed = PeerBidirectionalDurableOperation::Completed {
            schema: OPERATION_SCHEMA.to_owned(),
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
    fn only_the_matching_completed_operation_can_be_acknowledged() {
        let directory = tempfile::tempdir().unwrap();
        let journal = PeerBidirectionalOperationJournal::new(directory.path());
        let operation_id = "123e4567-e89b-42d3-a456-426614174004";
        journal
            .store(&PeerBidirectionalDurableOperation::Completed {
                schema: OPERATION_SCHEMA.to_owned(),
                result: PeerBidirectionalCompletedResult {
                    kind: "noChanges".to_owned(),
                    operation_id: operation_id.to_owned(),
                    revision: 7,
                    remote_revision: 4,
                    transferred_objects: 0,
                    transferred_bytes: 0,
                    backups: vec![],
                },
            })
            .unwrap();

        assert!(journal
            .acknowledge_completed("123e4567-e89b-42d3-a456-426614174099")
            .is_err());
        assert!(journal.load().unwrap().is_some());
        journal.acknowledge_completed(operation_id).unwrap();
        assert_eq!(journal.load().unwrap(), None);
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
            local_manifest_hash: "a".repeat(64),
            remote_manifest_hash: "b".repeat(64),
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
}
