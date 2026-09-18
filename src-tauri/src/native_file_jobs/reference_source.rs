use super::{JobControl, JobPhase, JobResultSummary, NativeFileJobState, NativeJobError};
use crate::persistent_store::external_conflicts::{self, ConflictSide, ConflictSourceDescriptor};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

struct ExternalReferenceEntry {
    conflict_id: String,
    descriptor: ConflictSourceDescriptor,
    renderer_active: bool,
    workers: usize,
}

#[derive(Default)]
pub(super) struct ExternalReferenceSources {
    entries: Mutex<BTreeMap<String, ExternalReferenceEntry>>,
}

pub(crate) struct ExternalReferenceSourceGuard {
    sources: Arc<ExternalReferenceSources>,
    token: String,
    descriptor: ConflictSourceDescriptor,
}

#[derive(Clone)]
pub(crate) enum ClaimedReferenceSource {
    External(Arc<ExternalReferenceSourceGuard>),
    Server(Arc<crate::server_sync::commands::ReferenceSourceGuard>),
}

pub(crate) fn claim_reference_source(
    app: &AppHandle,
    state: &NativeFileJobState,
    token: &str,
) -> Result<ClaimedReferenceSource, NativeJobError> {
    if token.starts_with("external:") {
        state
            .claim_external_reference_source(token)
            .map(Arc::new)
            .map(ClaimedReferenceSource::External)
    } else if token.starts_with("server:") {
        crate::server_sync::commands::claim_reference_source(app, token)
            .map(Arc::new)
            .map(ClaimedReferenceSource::Server)
            .map_err(|error| NativeJobError::new(&error.code, "Conflict source is unavailable"))
    } else {
        Err(NativeJobError::new(
            "invalid-source",
            "Unknown conflict source namespace",
        ))
    }
}

fn provider_error(error: crate::external_storage::contract::ProviderError) -> NativeJobError {
    use crate::external_storage::contract::ErrorKind;
    let code = match error.kind {
        ErrorKind::Cancelled => "cancelled",
        ErrorKind::NotFound => "source-unavailable",
        ErrorKind::Corrupt | ErrorKind::PreconditionFailed => "invalid-source",
        ErrorKind::Unauthorized | ErrorKind::ReauthRequired => "reauth-required",
        ErrorKind::RateLimited => "rate-limited",
        ErrorKind::DailyQuotaExhausted => "daily-quota-exhausted",
        ErrorKind::StorageFull => "storage-full",
        ErrorKind::FileTooLarge => "file-too-large",
        ErrorKind::Unsupported => "capability-unavailable",
        ErrorKind::Transient => "transport-failed",
    };
    NativeJobError::new(code, "Conflict source operation failed")
}

fn prepare_external_source(
    guard: Arc<ExternalReferenceSourceGuard>,
    app: &AppHandle,
    staging: &Path,
    job: &Arc<JobControl>,
) -> Result<(
    crate::external_storage::snapshot_restore::PreparedRemoteSnapshot,
    Arc<ExternalReferenceSourceGuard>,
), NativeJobError> {
    let descriptor = guard.descriptor().clone();
    let source = crate::external_storage::snapshot_export_commands::validated_conflict_source(
        guard, descriptor,
    )
    .map_err(provider_error)?;
    let cancellation = crate::external_storage::contract::Cancellation::default();
    let cancellation_watch = cancellation.clone();
    let job_watch = Arc::clone(job);
    std::fs::create_dir_all(staging).map_err(|_| {
        NativeJobError::new("store-error", "Conflict source staging is unavailable")
    })?;
    let prepared = tauri::async_runtime::block_on(async {
        let preparation =
            crate::external_storage::snapshot_export_commands::prepare_validated_conflict_source(
                app,
                source,
                staging,
                &cancellation,
            );
        tokio::pin!(preparation);
        let watch = async move {
            loop {
                if job_watch.is_cancel_requested() {
                    cancellation_watch.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::pin!(watch);
        tokio::select! {
            result = &mut preparation => result,
            _ = &mut watch => preparation.await,
        }
    })
    .map_err(provider_error)?;
    Ok(prepared.into_parts())
}

fn finish_reference_restore(
    store: &mut crate::persistent_store::PersistentStore,
    prepared: crate::persistent_store::PreparedReplaceCommit,
    expected_revision: i64,
    source_bytes: u64,
    source_sha256: String,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    let staging_id = prepared.external_staging_id().to_owned();
    let outcome = (|| {
        let preview = store
            .staged_plugin_preview(&staging_id)
            .map_err(super::native_store_error)?;
        job.set_plugin_value_preview(&staging_id, preview)
            .map_err(|error| NativeJobError::new("store-error", error))?;
        let activation_revision = job
            .wait_for_restore_finalization()
            .map_err(|error| NativeJobError::new("cancelled", error))?
            .unwrap_or(expected_revision);
        if activation_revision != expected_revision {
            return Err(NativeJobError::new(
                "revision-conflict",
                "Library changed before conflict restore activation",
            ));
        }
        if job.is_cancel_requested() {
            return Err(NativeJobError::new(
                "cancelled",
                "Conflict restore cancelled before activation",
            ));
        }
        let (character_count, preset_count) = store
            .staged_library_counts(&staging_id)
            .map_err(super::native_store_error)?;
        let revision = store
            .finish_prepared_replace(prepared)
            .map_err(super::native_store_error)?
            .revision;
        Ok(JobResultSummary {
            revision,
            source_bytes,
            source_sha256,
            character_count,
            preset_count,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: None,
        })
    })();
    match outcome {
        Err(failure) => match store.replace_abort(&staging_id) {
            Ok(()) => Err(failure),
            Err(cleanup) => Err(NativeJobError::new(
                &failure.code,
                format!("{}; staging abort failed: {cleanup}", failure.message),
            )),
        },
        success => success,
    }
}

pub(crate) fn restore_reference_source(
    source: ClaimedReferenceSource,
    expected_revision: i64,
    owned: &Path,
    mut store: crate::persistent_store::PersistentStore,
    job: &Arc<JobControl>,
    app: &AppHandle,
) -> Result<JobResultSummary, NativeJobError> {
    super::job_transition(job, job.start(JobPhase::ReadingSource))?;
    super::job_transition(job, job.set_phase(JobPhase::StagingDatabase))?;
    match source {
        ClaimedReferenceSource::External(guard) => {
            let staging = owned.join("external-conflict-source");
            let (snapshot, claim) = prepare_external_source(guard, app, &staging, job)?;
            let source_bytes = snapshot
                .records
                .iter()
                .map(|record| record.byte_length)
                .chain(snapshot.objects.iter().map(|object| object.byte_length))
                .try_fold(0u64, u64::checked_add)
                .ok_or_else(|| NativeJobError::new("invalid-source", "Conflict source is too large"))?;
            let source_sha256 = snapshot.library_fingerprint.clone();
            let fingerprint: [u8; 32] = hex::decode(&snapshot.library_fingerprint)
                .ok()
                .and_then(|value| value.try_into().ok())
                .ok_or_else(|| NativeJobError::new("invalid-source", "Conflict source fingerprint is invalid"))?;
            let scope = risunest_external_storage_format::format::library_fingerprint_domain();
            let application = crate::persistent_store::external_apply::ExternalSnapshotApplication {
                expected_revision,
                staging_root: &snapshot.staging_root,
                scope_id: &scope,
                fingerprint: &fingerprint,
            };
            let records = snapshot.records.into_iter().map(|record| {
                Ok(crate::persistent_store::external_apply::ExternalSnapshotRecord {
                    key: record.key,
                    content_hash: record.content_hash,
                    byte_length: record.byte_length,
                    path: record.path,
                })
            });
            let objects = snapshot.objects.into_iter().map(|object| {
                Ok(crate::persistent_store::external_apply::ExternalSnapshotObject {
                    content_hash: object.content_hash,
                    byte_length: object.byte_length,
                    path: object.path,
                })
            });
            let prepared = store
                .prepare_external_snapshot_application(&application, records, objects)
                .map_err(super::native_store_error)?;
            let outcome = finish_reference_restore(
                &mut store,
                prepared,
                expected_revision,
                source_bytes,
                source_sha256,
                job,
            );
            drop(claim);
            outcome
        }
        ClaimedReferenceSource::Server(guard) => {
            let source_bytes = guard.source().index_bytes();
            let source_sha256 = guard.source().index_hash().to_owned();
            let check = || {
                if job.is_cancel_requested() {
                    Err(crate::server_sync::SyncError::new("cancelled", 409))
                } else {
                    Ok(())
                }
            };
            let prepared = store
                .prepare_server_conflict_replacement(guard.source(), expected_revision, &check)
                .map_err(|error| {
                    NativeJobError::new(&error.code, "Server conflict source restore failed")
                })?;
            finish_reference_restore(
                &mut store,
                prepared,
                expected_revision,
                source_bytes,
                source_sha256,
                job,
            )
        }
    }
}

pub(crate) fn export_reference_source(
    source: ClaimedReferenceSource,
    destination: Option<&Path>,
    owned: &Path,
    handoffs: &Path,
    job: &Arc<JobControl>,
    app: &AppHandle,
) -> Result<JobResultSummary, NativeJobError> {
    match source {
        ClaimedReferenceSource::External(guard) => {
            super::job_transition(job, job.start(JobPhase::WritingExport))?;
            let cancellation = crate::external_storage::contract::Cancellation::default();
            let staging = owned.join("external-conflict-export");
            let (snapshot, claim) = prepare_external_source(guard, app, &staging, job)?;
            let mut character_count = 0u64;
            let mut preset_count = 0u64;
            for record in &snapshot.records {
                match crate::logical_records::decode_logical_record_key(&record.key)
                    .map_err(|_| NativeJobError::new("invalid-source", "Conflict record key is invalid"))?
                {
                    crate::logical_records::LogicalRecordLocator::Character { .. } => {
                        character_count = character_count.checked_add(1).ok_or_else(|| {
                            NativeJobError::new("invalid-source", "Conflict record count overflowed")
                        })?;
                    }
                    crate::logical_records::LogicalRecordLocator::Preset { .. } => {
                        preset_count = preset_count.checked_add(1).ok_or_else(|| {
                            NativeJobError::new("invalid-source", "Conflict record count overflowed")
                        })?;
                    }
                    _ => {}
                }
            }
            let (destination, handoff) = match destination {
                Some(path) => (path.to_path_buf(), None),
                None => {
                    std::fs::create_dir_all(handoffs).map_err(|_| {
                        NativeJobError::new("store-error", "Portable handoff directory is unavailable")
                    })?;
                    let path = handoffs.join(format!(
                        "risunest-backup-{}.risunest",
                        uuid::Uuid::new_v4()
                    ));
                    (path.clone(), Some(path.to_string_lossy().into_owned()))
                }
            };
            super::job_transition(job, job.set_phase(JobPhase::PublishingDestination))?;
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done_watch = Arc::clone(&done);
            let cancellation_watch = cancellation.clone();
            let job_watch = Arc::clone(job);
            let watcher = std::thread::spawn(move || {
                while !done_watch.load(std::sync::atomic::Ordering::Acquire) {
                    if job_watch.is_cancel_requested() {
                        cancellation_watch.cancel();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
            let receipt =
                crate::external_storage::snapshot_export::export_verified_snapshot_controlled(
                    snapshot,
                    &destination,
                    &staging,
                    &cancellation,
                    || {
                        job.commit_export_publication().map_err(|message| {
                            if job.is_cancel_requested() {
                                crate::persistent_store::export::destination::DestinationWriteError::Cancelled
                            } else {
                                crate::persistent_store::export::destination::DestinationWriteError::Io {
                                    operation: "commit conflict export publication",
                                    source: std::io::Error::other(message),
                                }
                            }
                        })
                    },
                );
            done.store(true, std::sync::atomic::Ordering::Release);
            let _ = watcher.join();
            let receipt = receipt.map_err(provider_error)?;
            let source_bytes = std::fs::metadata(&receipt.destination)
                .map_err(|_| {
                    NativeJobError::new(
                        "store-error",
                        "Published conflict backup metadata is unavailable",
                    )
                })?
                .len();
            let result = JobResultSummary {
                revision: receipt.revision,
                source_bytes,
                source_sha256: receipt.sha256,
                character_count,
                preset_count,
                warning_codes: Vec::new(),
                handoff_path: handoff,
                publication: None,
            };
            drop(claim);
            Ok(result)
        }
        ClaimedReferenceSource::Server(guard) => {
            let source_root = crate::persistent_store::commands::with_store_mut(
                app.state(),
                |store| Ok(store.repository_root().to_path_buf()),
            )
            .map_err(super::native_store_error)?;
            let check = || {
                if job.is_cancel_requested() {
                    Err(crate::server_sync::SyncError::new("cancelled", 409))
                } else {
                    Ok(())
                }
            };
            let prepared = crate::server_sync::backups::prepare_reference_portable_store(
                &source_root,
                guard.source(),
                owned,
                &check,
            )
            .map_err(|error| {
                NativeJobError::new(&error.code, "Server conflict source export failed")
            })?;
            let (store, revision, scratch) = prepared.into_parts();
            let result = super::portable::export_portable(
                destination,
                revision,
                owned,
                handoffs,
                store,
                job,
                None,
            )?;
            drop(scratch);
            drop(guard);
            Ok(result)
        }
    }
}

impl ExternalReferenceSourceGuard {
    pub(crate) fn descriptor(&self) -> &ConflictSourceDescriptor {
        &self.descriptor
    }
}

impl Drop for ExternalReferenceSourceGuard {
    fn drop(&mut self) {
        let Ok(mut entries) = self.sources.entries.lock() else {
            return;
        };
        let remove = if let Some(entry) = entries.get_mut(&self.token) {
            entry.workers = entry.workers.saturating_sub(1);
            !entry.renderer_active && entry.workers == 0
        } else {
            false
        };
        if remove {
            entries.remove(&self.token);
        }
    }
}

fn conflict_id(descriptor: &ConflictSourceDescriptor) -> &str {
    match descriptor {
        ConflictSourceDescriptor::Local { conflict_id, .. }
        | ConflictSourceDescriptor::Remote { conflict_id, .. } => conflict_id,
    }
}

fn source_unavailable() -> NativeJobError {
    NativeJobError::new(
        "source-unavailable",
        "Conflict source is no longer available",
    )
}

fn validate_external_token(token: &str) -> Result<(), NativeJobError> {
    let id = token
        .strip_prefix("external:")
        .ok_or_else(|| NativeJobError::new("invalid-source", "Invalid conflict source token"))?;
    let uuid = Uuid::parse_str(id).map_err(|_| source_unavailable())?;
    if uuid.to_string() != id || uuid.get_version() != Some(uuid::Version::Random) {
        return Err(source_unavailable());
    }
    Ok(())
}

impl ExternalReferenceSources {
    fn register(
        self: &Arc<Self>,
        descriptor: ConflictSourceDescriptor,
    ) -> Result<String, NativeJobError> {
        let mut entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        loop {
            let token = format!("external:{}", Uuid::new_v4());
            if entries.contains_key(&token) {
                continue;
            }
            entries.insert(
                token.clone(),
                ExternalReferenceEntry {
                    conflict_id: conflict_id(&descriptor).to_owned(),
                    descriptor,
                    renderer_active: true,
                    workers: 0,
                },
            );
            return Ok(token);
        }
    }

    fn claim(
        self: &Arc<Self>,
        token: &str,
    ) -> Result<ExternalReferenceSourceGuard, NativeJobError> {
        validate_external_token(token)?;
        let descriptor = {
            let mut entries = self.entries.lock().map_err(|_| {
                NativeJobError::new("store-error", "Conflict source state is unavailable")
            })?;
            let entry = entries.get_mut(token).ok_or_else(source_unavailable)?;
            if !entry.renderer_active {
                return Err(source_unavailable());
            }
            entry.workers = entry.workers.checked_add(1).ok_or_else(|| {
                NativeJobError::new("store-error", "Conflict source claims overflowed")
            })?;
            entry.descriptor.clone()
        };
        Ok(ExternalReferenceSourceGuard {
            sources: Arc::clone(self),
            token: token.to_owned(),
            descriptor,
        })
    }

    fn release(&self, token: &str) -> Result<(), NativeJobError> {
        validate_external_token(token)?;
        let mut entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        let remove = if let Some(entry) = entries.get_mut(token) {
            entry.renderer_active = false;
            entry.workers == 0
        } else {
            false
        };
        if remove {
            entries.remove(token);
        }
        Ok(())
    }

    fn conflict_in_use(&self, id: &str) -> Result<bool, NativeJobError> {
        if id.is_empty() || id.len() > 1024 || id.contains('\0') {
            return Err(NativeJobError::new(
                "invalid-input",
                "Invalid external conflict ID",
            ));
        }
        let entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        Ok(entries
            .values()
            .any(|entry| entry.conflict_id == id && (entry.renderer_active || entry.workers != 0)))
    }
}

impl NativeFileJobState {
    fn external_conflict_source_open_admission(
        &self,
    ) -> Result<super::admission::Permit, NativeJobError> {
        self.admission
            .file(false)
            .map_err(|code| NativeJobError::new(code, "Another library operation is running"))
    }

    pub(crate) fn external_conflict_mutation_admission(
        &self,
    ) -> Result<super::admission::Permit, NativeJobError> {
        self.admission
            .file(true)
            .map_err(|code| NativeJobError::new(code, "Another library operation is running"))
    }

    fn register_external_reference_source(
        &self,
        descriptor: ConflictSourceDescriptor,
    ) -> Result<String, NativeJobError> {
        self.external_reference_sources.register(descriptor)
    }

    pub(crate) fn claim_external_reference_source(
        &self,
        token: &str,
    ) -> Result<ExternalReferenceSourceGuard, NativeJobError> {
        self.external_reference_sources.claim(token)
    }

    pub(crate) fn external_conflict_in_use(&self, id: &str) -> Result<bool, NativeJobError> {
        self.external_reference_sources.conflict_in_use(id)
    }

    fn release_external_reference_source(&self, token: &str) -> Result<(), NativeJobError> {
        self.external_reference_sources.release(token)
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ConflictReferenceSource {
    ConflictReference { token: String },
}

#[derive(Serialize)]
pub(crate) struct OpenConflictSourceResponse {
    source: ConflictReferenceSource,
}

#[tauri::command]
pub(crate) fn external_storage_open_conflict_source(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    id: String,
    side: ConflictSide,
) -> Result<OpenConflictSourceResponse, NativeJobError> {
    let _admission = state.external_conflict_source_open_admission()?;
    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
        let root = store.repository_root().to_path_buf();
        let descriptor = external_conflicts::conflict_source_descriptor(
            store.device_store()?.connection(),
            &root,
            &id,
            side,
        )?;
        state
            .register_external_reference_source(descriptor)
            .map(|token| OpenConflictSourceResponse {
                source: ConflictReferenceSource::ConflictReference { token },
            })
            .map_err(|error| crate::persistent_store::StoreError::Store {
                message: error.to_string(),
            })
    })
    .map_err(super::native_store_error)
}

#[tauri::command]
pub(crate) fn external_storage_release_conflict_source(
    state: State<'_, NativeFileJobState>,
    token: String,
) -> Result<(), NativeJobError> {
    state.release_external_reference_source(&token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::capture::DurableCaptureReference,
        persistent_store::sync_selection::CaptureIdentity,
    };

    fn descriptor(id: &str) -> ConflictSourceDescriptor {
        ConflictSourceDescriptor::Local {
            conflict_id: id.into(),
            repository_id: "repository".into(),
            capture: DurableCaptureReference {
                capture_id: "capture".into(),
                identity: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "library".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
                catalog_path: "captures/catalog.sqlite".into(),
                catalog_hash: "00".repeat(32),
            },
        }
    }

    #[test]
    fn released_renderer_lease_keeps_worker_claim_pinned_until_drop() {
        let sources = Arc::new(ExternalReferenceSources::default());
        let token = sources.register(descriptor("conflict")).unwrap();
        let worker = Arc::new(sources.claim(&token).unwrap());
        let terminal = Arc::clone(&worker);
        sources.release(&token).unwrap();
        assert!(sources.conflict_in_use("conflict").unwrap());
        let error = match sources.claim(&token) {
            Ok(_) => panic!("released source was claimed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "source-unavailable");
        drop(worker);
        assert!(sources.conflict_in_use("conflict").unwrap());
        drop(terminal);
        assert!(!sources.conflict_in_use("conflict").unwrap());
        sources.release(&token).unwrap();
    }

    #[test]
    fn conflict_delete_cannot_cross_the_descriptor_read_and_registration_boundary() {
        let state = NativeFileJobState::initialize_with_max_workers(
            tempfile::tempdir().unwrap().path().to_path_buf(),
            1,
        );
        let open = state.external_conflict_source_open_admission().unwrap();
        assert_eq!(
            state
                .external_conflict_mutation_admission()
                .unwrap_err()
                .code,
            "library-operation-busy"
        );
        let token = state
            .register_external_reference_source(descriptor("conflict"))
            .unwrap();
        drop(open);

        let _delete = state.external_conflict_mutation_admission().unwrap();
        assert!(state.external_conflict_in_use("conflict").unwrap());
        state.release_external_reference_source(&token).unwrap();
        assert!(!state.external_conflict_in_use("conflict").unwrap());
    }
}
