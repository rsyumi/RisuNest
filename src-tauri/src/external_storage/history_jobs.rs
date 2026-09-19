//! Durable manual history publication over an already immutable snapshot.
use super::{
    connection_commands::ConnectedRepository,
    connection_store::ConnectionStore,
    contract::{Cancellation, ErrorKind, ObjectRole, ProviderError, Result},
    control::{BackupPointDocument, BackupPointKind},
    job_store::DurableJob,
    journal::{JobIdentity, TransferJournal},
    packaging::RemoteObject,
    runtime::{job_directory, local_error, native_store, root},
};
use crate::persistent_store::{
    external_storage_state::{PinHistoryIntent, PinHistoryRecord},
    PersistentStore,
};
use serde_json::{json, Value};
use std::path::Path;
use tauri::AppHandle;

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn result_value(record: &PinHistoryRecord) -> Result<Value> {
    let observation = record.point_observation.as_deref().ok_or_else(corrupt)?;
    let point: RemoteObject = serde_json::from_str(observation).map_err(|_| corrupt())?;
    if point.repository_id != record.repository_id
        || point.object_id != format!("backup-point-{}", record.point_id)
        || point.role != ObjectRole::BackupPoint
    {
        return Err(corrupt());
    }
    Ok(json!({"snapshotId":record.snapshot_id,"pointId":record.point_id}))
}

fn validate_record(record: &PinHistoryRecord, job: &DurableJob) -> Result<()> {
    if record.job_id != job.id
        || record.connection_id != job.request.connection_id
        || record.point_id != job.id
        || record.identity != job.admission_identity
        || job.request.snapshot_id.as_deref() != Some(record.snapshot_id.as_str())
    {
        return Err(corrupt());
    }
    Ok(())
}

/// Recovers a success whose authoritative PDS commit outlived the auxiliary job cache.
pub(crate) fn completed_pin_history(
    store: &PersistentStore,
    job: &DurableJob,
) -> Result<Option<Value>> {
    let Some(record) = store
        .external_pin_history_record(&job.id)
        .map_err(local_error)?
    else {
        return Ok(None);
    };
    validate_record(&record, job)?;
    record
        .point_observation
        .as_ref()
        .map(|_| result_value(&record))
        .transpose()
}

async fn upload_prepared(
    connected: &ConnectedRepository,
    directory: &Path,
    record: &PinHistoryRecord,
    cancel: &Cancellation,
) -> Result<(RemoteObject, TransferJournal)> {
    let snapshot: RemoteObject =
        serde_json::from_str(&record.snapshot_reference).map_err(|_| corrupt())?;
    if record.repository_id != connected.stored.descriptor.repository_id
        || snapshot.repository_id != record.repository_id
        || snapshot.object_id != format!("snapshot-{}", record.snapshot_id)
        || !matches!(
            snapshot.role,
            ObjectRole::SyncState | ObjectRole::BackupBundle
        )
    {
        return Err(corrupt());
    }
    snapshot.stored(&connected.handle)?;
    let mut journal = TransferJournal::open(
        directory,
        JobIdentity {
            job_id: record.job_id.clone(),
            connection_id: record.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id: record.snapshot_id.clone(),
            capture: record.identity.clone(),
        },
    )?;
    // A point names a bundle. Pinning a published state wraps its library
    // reference in one so the retained material stays complete on its own.
    let bundle = if snapshot.role == ObjectRole::BackupBundle {
        snapshot
    } else {
        let view =
            super::control::read_snapshot_document(connected, &snapshot, cancel).await?;
        super::control::upload_backup_bundle(
            &connected.stored.descriptor,
            &connected.root_key,
            format!("{}-pinned", record.point_id),
            risunest_external_storage_format::control::BundleSource::SyncState {
                commit_id: record.snapshot_id.clone(),
            },
            record.created_at_ms,
            view.library,
            view.sections,
            &mut journal,
            connected.provider.as_ref(),
            &connected.handle,
            cancel,
        )
        .await?
    };
    let point = BackupPointDocument::single(
        &connected.stored.descriptor,
        record.point_id.clone(),
        BackupPointKind::Manual,
        record.created_at_ms,
        bundle,
    )?;
    let observation = super::control::upload_backup_point(
        &connected.stored.descriptor,
        &connected.root_key,
        point,
        &mut journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    Ok((observation, journal))
}

pub(crate) async fn run_pin_history(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    if let Some(result) = completed_pin_history(&native_store(app)?, job)? {
        return Ok(result);
    }
    let snapshot_id = job.request.snapshot_id.as_deref().ok_or_else(corrupt)?;
    let record = match native_store(app)?
        .external_pin_history_record(&job.id)
        .map_err(local_error)?
    {
        Some(record) => record,
        None => {
            let cache_root = root(app)?;
            let known = match ConnectionStore::open(&cache_root)?
                .discovery_snapshot(&job.request.connection_id, snapshot_id)
            {
                Ok(value) => Some(value),
                Err(error) if error.kind == ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            let invalidation_root = cache_root.clone();
            let invalidation_connection = job.request.connection_id.clone();
            let invalidation_id = snapshot_id.to_owned();
            let snapshot = super::control::find_snapshot_with_locator_invalidation(
                connected,
                snapshot_id,
                known.as_ref(),
                move || {
                    ConnectionStore::open(&invalidation_root)?
                        .forget_discovery(&invalidation_connection, &invalidation_id)
                },
                cancel,
            )
            .await?;
            ConnectionStore::open(&cache_root)?.remember_discovery(
                &job.request.connection_id,
                snapshot_id,
                &snapshot,
            )?;
            let document =
                super::control::read_snapshot_document(connected, &snapshot, cancel).await?;
            let snapshot_reference = serde_json::to_string(&snapshot).map_err(|_| corrupt())?;
            let created_at_ms = job.summary["startedAtMs"]
                .as_str()
                .and_then(|value| value.parse().ok())
                .ok_or_else(corrupt)?;
            native_store(app)?
                .external_prepare_pin_history(&PinHistoryIntent {
                    job_id: &job.id,
                    connection_id: &job.request.connection_id,
                    repository_id: &connected.stored.descriptor.repository_id,
                    snapshot_id,
                    snapshot_reference: &snapshot_reference,
                    point_id: &job.id,
                    logical_revision: document.revision.parse().unwrap_or_default(),
                    created_at_ms,
                    identity: &job.admission_identity,
                })
                .map_err(local_error)?;
            native_store(app)?
                .external_pin_history_record(&job.id)
                .map_err(local_error)?
                .ok_or_else(corrupt)?
        }
    };
    validate_record(&record, job)?;
    if record.point_observation.is_some() {
        return result_value(&record);
    }
    let directory = job_directory(&root(app)?, &job.request.connection_id, &job.id);
    let (point, mut journal) = upload_prepared(connected, &directory, &record, cancel).await?;
    let observation = serde_json::to_string(&point).map_err(|_| corrupt())?;
    native_store(app)?
        .external_finish_pin_history(&job.id, &observation)
        .map_err(local_error)?;
    if journal
        .release_completed_sessions(connected.dependencies.vault.as_ref())
        .await
        .is_err()
    {
        crate::nlog!(
            "warn",
            "External completed history upload secret cleanup is pending"
        );
    }
    result_value(
        &native_store(app)?
            .external_pin_history_record(&job.id)
            .map_err(local_error)?
            .ok_or_else(corrupt)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::{
            contract::{ObjectReceipt, RemoteLocator},
            fake::{self, FakeProvider},
            job_store::{JobKind, StartJobRequest},
        },
        persistent_store::{RootMutation, WorkingSetCommit},
    };
    use risunest_external_storage_format::{
        format::{Descriptor, Strategy},
        snapshot as wire,
    };
    use std::{collections::BTreeMap, sync::Arc};

    fn descriptor() -> Descriptor {
        Descriptor::new("descriptor-repository".into(), Some(Strategy::Cas),
        )
        .unwrap()
    }

    fn snapshot() -> RemoteObject {
        let header = wire::PublicObjectHeader::new(
            "descriptor-repository".into(),
            "snapshot-snapshot".into(),
            wire::ObjectRole::BackupBundle,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: "descriptor-repository".into(),
            object_id: "snapshot-snapshot".into(),
            role: ObjectRole::BackupBundle,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: fake::repository().connection_identity,
                    collection: None,
                    object: "opaque-snapshot".into(),
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

    fn job(identity: crate::persistent_store::sync_selection::CaptureIdentity) -> DurableJob {
        DurableJob::new(
            StartJobRequest {
                connection_id: "connection".into(),
                kind: JobKind::PinHistory,
                snapshot_id: Some("snapshot".into()),
                point_id: None,
                point_observation: None,
                confirm_other_device: None,
                confirm_last_retained: None,
                conflict_id: None,
                choice: None,
                restore_areas: None,
                target_revision: None,
                session: None,
                session_id: None,
                reason: Some("manual".into()),
            },
            false,
            1_000,
            identity,
        )
    }

    fn prepare(store: &mut PersistentStore, job: &DurableJob) -> PinHistoryRecord {
        let snapshot_reference = serde_json::to_string(&snapshot()).unwrap();
        store
            .external_prepare_pin_history(&PinHistoryIntent {
                job_id: &job.id,
                connection_id: &job.request.connection_id,
                repository_id: &descriptor().repository_id,
                snapshot_id: "snapshot",
                snapshot_reference: &snapshot_reference,
                point_id: &job.id,
                logical_revision: 7,
                created_at_ms: 1_000,
                identity: &job.admission_identity,
            })
            .unwrap();
        store.external_pin_history_record(&job.id).unwrap().unwrap()
    }

    fn connected(provider: Arc<FakeProvider>) -> ConnectedRepository {
        let test = fake::loopback_dependencies(fake::MemoryVault::default(), 1_000);
        ConnectedRepository {
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
                descriptor: descriptor(),
                descriptor_locator: RemoteLocator {
                    connection_identity: fake::repository().connection_identity,
                    collection: None,
                    object: "descriptor".into(),
                },
                provider_repository_id: fake::repository().repository_id,
                credential_ref: "credential".into(),
                root_key_ref: "key".into(),
                recovery_key_ref: "recovery-key".into(),
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

    fn edit(store: &mut PersistentStore) {
        let revision = store.revision().unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: revision,
                root: None,
                root_mutations: Some(vec![RootMutation::Set {
                    key: "synthetic".into(),
                    value: json!(revision + 1),
                }]),
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
    }

    #[test]
    fn failed_point_upload_preserves_previously_published_history() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let provider = Arc::new(FakeProvider::new(true));
            let connected = connected(provider.clone());
            let identity = crate::persistent_store::sync_selection::CaptureIdentity {
                store_id: "store".into(),
                library_epoch: "library".into(),
                generation: "generation".into(),
                selection_epoch: "selection".into(),
                revision: 3,
            };
            let mut old = PinHistoryRecord {
                job_id: "old".into(),
                connection_id: "connection".into(),
                repository_id: descriptor().repository_id,
                snapshot_id: "snapshot".into(),
                snapshot_reference: serde_json::to_string(&snapshot()).unwrap(),
                point_id: "old".into(),
                logical_revision: 7,
                created_at_ms: 1_000,
                identity: identity.clone(),
                point_observation: None,
            };
            let (published, _) = upload_prepared(
                &connected,
                &directory.path().join("old"),
                &old,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            old.point_observation = Some(serde_json::to_string(&published).unwrap());
            assert!(result_value(&old).is_ok());

            let mut replacement = old.clone();
            replacement.job_id = "replacement".into();
            replacement.point_id = "replacement".into();
            replacement.point_observation = None;
            let cancelled = Cancellation::default();
            cancelled.cancel();
            assert!(upload_prepared(
                &connected,
                &directory.path().join("replacement"),
                &replacement,
                &cancelled,
            )
            .await
            .is_err());
            let objects = &provider.state.lock().unwrap().objects;
            assert!(objects.contains_key("backup-point-old"));
            assert!(!objects.contains_key("backup-point-replacement"));
        });
    }

    #[test]
    fn retry_after_local_edit_reopens_admission_identity_and_exact_snapshot() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let job = job(store.external_identity().unwrap());
            let record = prepare(&mut store, &job);
            let provider = Arc::new(FakeProvider::new(true));
            let connected = connected(provider.clone());
            let cancelled = Cancellation::default();
            cancelled.cancel();
            assert!(upload_prepared(
                &connected,
                &directory.path().join("journal"),
                &record,
                &cancelled,
            )
            .await
            .is_err());

            edit(&mut store);
            assert!(store.external_identity().unwrap().revision > job.admission_identity.revision);
            let reopened = store.external_pin_history_record(&job.id).unwrap().unwrap();
            assert_eq!(reopened.identity, job.admission_identity);
            assert_eq!(reopened.snapshot_reference, record.snapshot_reference);
            let (point, _) = upload_prepared(
                &connected,
                &directory.path().join("journal"),
                &reopened,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let observation = serde_json::to_string(&point).unwrap();
            store
                .external_finish_pin_history(&job.id, &observation)
                .unwrap();
            assert_eq!(
                completed_pin_history(&store, &job).unwrap().unwrap()["snapshotId"],
                "snapshot"
            );
            let objects = &provider.state.lock().unwrap().objects;
            assert_eq!(objects.len(), 2);
            assert_eq!(objects.keys()
                .filter(|id| id.starts_with("inventory-page-")).count(), 1);
            assert!(objects.contains_key(&format!("backup-point-{}", job.id)));
        });
    }
}
