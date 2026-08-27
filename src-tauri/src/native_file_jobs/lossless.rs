use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError, OpenedJobSource};
use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};
use crate::asset_repository::PayloadCas;
use crate::local_backup::CancellationProbe;
use crate::lossless_backup::{
    create_and_verify_lossless_backup_v1_durable_report,
    restore_verified_lossless_package_v1_durable_controlled,
    verify_lossless_package_v1_for_production, LosslessError, LosslessErrorCode,
};
use crate::persistent_store::export::destination::{
    self, DestinationProgress, DestinationWriteError,
};
use crate::persistent_store::PersistentStore;
use std::cell::{Cell, RefCell};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const INPUT_FILE: &str = "input.risulossless";
const ARCHIVE_FILE: &str = "archive.risulossless.part";
const RECOVERY_FILE: &str = "recovery.risulossless.part";

struct JobCancellation<'a>(&'a JobControl);

impl CancellationProbe for JobCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancel_requested()
    }
}

pub(crate) fn restore_lossless_backup(
    source: OpenedJobSource,
    expected_revision: i64,
    owned_directory: &Path,
    mut store: PersistentStore,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::ReadingSource)
        .map_err(job_state_error)?;
    let immutable_source = copy_source_to_owned(source, owned_directory, job)?;
    let cancellation = JobCancellation(job);
    let mut source_file = File::open(&immutable_source).map_err(|error| {
        NativeJobError::new(
            "invalid-source",
            format!("lossless input spool cannot be opened: {error}"),
        )
    })?;
    let verified = verify_lossless_package_v1_for_production(&mut source_file, &cancellation)
        .map_err(lossless_input_error)?;
    source_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(job_state_error)?;
    let repository_root = store.repository_root().to_path_buf();
    let cas = PayloadCas::new(&repository_root).map_err(io_store_error)?;
    let recovery_source = owned_directory.join(RECOVERY_FILE);
    let recovery_directory = repository_root.join("persistent").join("recovery");
    fs::create_dir_all(&recovery_directory).map_err(io_store_error)?;
    let recovery_destination = recovery_directory.join(format!(
        "risulossless-recovery-{}.risulossless",
        Uuid::new_v4()
    ));
    let mut durable = DurableCasJob::begin(
        &repository_root,
        &job.id(),
        CasJobKind::LosslessImport,
        now_millis(),
    )
    .map_err(io_store_error)?;
    let activation_attempted = Cell::new(false);
    let phase_failure = RefCell::new(None);
    let before_activation = || {
        publish_owned_package(
            owned_directory,
            &recovery_source,
            &recovery_directory,
            &recovery_destination,
            job,
            &phase_failure,
            None,
            || Ok(()),
        )
        .map_err(|error| error.message)?;
        job.wait_for_restore_finalization()
    };
    let outcome = restore_verified_lossless_package_v1_durable_controlled(
        &mut source_file,
        &verified,
        owned_directory,
        &cas,
        &mut store,
        expected_revision,
        &recovery_source,
        None,
        &mut durable,
        now_millis(),
        &before_activation,
        &|| activation_attempted.set(true),
        &cancellation,
    )
    .map_err(lossless_operation_error)
    .map(|report| JobResultSummary {
        revision: report.revision,
        source_bytes: report.source_bytes,
        source_sha256: report.source_sha256,
        character_count: report.character_count,
        preset_count: report.preset_count,
        warning_codes: bounded_warning_codes(
            report.warnings.into_iter().map(|warning| warning.code),
        ),
        handoff_path: None,
        recovery_path: Some(recovery_destination.to_string_lossy().into_owned()),
    });
    let outcome =
        remove_uncommitted_recovery(outcome, &recovery_destination, activation_attempted.get());
    finish_durable_job(outcome, &mut durable)
}

pub(crate) fn export_lossless_backup(
    destination: Option<&Path>,
    expected_revision: i64,
    owned_directory: &Path,
    handoff_directory: &Path,
    mut store: PersistentStore,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::WritingExport)
        .map_err(job_state_error)?;
    let cancellation = JobCancellation(job);
    let repository_root = store.repository_root().to_path_buf();
    let cas = PayloadCas::new(&repository_root).map_err(io_store_error)?;
    let mut durable = DurableCasJob::begin(
        &repository_root,
        &job.id(),
        CasJobKind::OfficialPublicationOrExportPreparation,
        now_millis(),
    )
    .map_err(io_store_error)?;
    let source = owned_directory.join(ARCHIVE_FILE);
    let created = create_and_verify_lossless_backup_v1_durable_report(
        &source,
        owned_directory,
        &cas,
        &mut store,
        expected_revision,
        &mut durable,
        now_millis(),
        &cancellation,
    )
    .map_err(lossless_operation_error);
    let outcome = created.and_then(|created| {
        job.set_phase(JobPhase::PublishingDestination)
            .map_err(job_state_error)?;
        let (destination_root, destination_path, handoff_path) = match destination {
            Some(destination) => (
                destination.parent().ok_or_else(|| {
                    NativeJobError::new(
                        "invalid-destination",
                        "desktop export destination has no parent directory",
                    )
                })?,
                destination.to_path_buf(),
                None,
            ),
            None => {
                fs::create_dir_all(handoff_directory).map_err(io_store_error)?;
                let path =
                    handoff_directory.join(format!("risulossless-{}.risulossless", Uuid::new_v4()));
                (
                    handoff_directory,
                    path.clone(),
                    Some(path.to_string_lossy().into_owned()),
                )
            }
        };
        let total_bytes = created.archive_bytes.saturating_mul(2);
        job.set_progress(JobProgress {
            completed_bytes: created.archive_bytes,
            total_bytes: Some(total_bytes),
            completed_items: 0,
            total_items: None,
        })
        .map_err(job_state_error)?;
        let phase_failure = RefCell::new(None);
        let published = publish_owned_package(
            owned_directory,
            &source,
            destination_root,
            &destination_path,
            job,
            &phase_failure,
            Some((created.archive_bytes, total_bytes)),
            || {
                if let Err(error) = job.set_phase(JobPhase::FinalizingExport) {
                    *phase_failure.borrow_mut() = Some(error);
                    return Err(DestinationWriteError::Cancelled);
                }
                Ok(())
            },
        )?;
        if published.bytes != created.archive_bytes || published.sha256 != created.archive_sha256 {
            return Err(NativeJobError::new(
                "hash-mismatch",
                "published lossless backup differs from its verified archive",
            ));
        }
        Ok(JobResultSummary {
            revision: expected_revision,
            source_bytes: published.bytes,
            source_sha256: published.sha256,
            character_count: created.character_count,
            preset_count: created.preset_count,
            warning_codes: Vec::new(),
            handoff_path,
            recovery_path: None,
        })
    });
    finish_durable_job(outcome, &mut durable)
}

fn copy_source_to_owned(
    mut source: OpenedJobSource,
    owned_directory: &Path,
    job: &JobControl,
) -> Result<PathBuf, NativeJobError> {
    let path = owned_directory.join(INPUT_FILE);
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(io_store_error)?;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    let mut copied = 0_u64;
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: Some(source.total_bytes),
        completed_items: 0,
        total_items: None,
    })
    .map_err(job_state_error)?;
    loop {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "lossless restore cancelled while copying its source",
            ));
        }
        let read = source.file.read(&mut buffer).map_err(|error| {
            NativeJobError::new(
                "invalid-source",
                format!("lossless source read failed: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        copied = copied.checked_add(read as u64).ok_or_else(|| {
            NativeJobError::new("invalid-source", "lossless source size overflow")
        })?;
        if copied > source.total_bytes {
            return Err(NativeJobError::new(
                "invalid-source",
                "lossless source changed while it was copied",
            ));
        }
        output.write_all(&buffer[..read]).map_err(io_store_error)?;
        job.set_progress(JobProgress {
            completed_bytes: copied,
            total_bytes: Some(source.total_bytes),
            completed_items: 0,
            total_items: None,
        })
        .map_err(job_state_error)?;
    }
    if copied != source.total_bytes {
        return Err(NativeJobError::new(
            "invalid-source",
            "lossless source changed while it was copied",
        ));
    }
    output.flush().map_err(io_store_error)?;
    output.sync_all().map_err(io_store_error)?;
    drop(output);
    Ok(path)
}

fn publish_owned_package(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination_path: &Path,
    job: &JobControl,
    phase_failure: &RefCell<Option<String>>,
    progress_range: Option<(u64, u64)>,
    before_replace: impl FnOnce() -> Result<(), DestinationWriteError>,
) -> Result<destination::DestinationWriteResult, NativeJobError> {
    destination::write_lossless_destination_controlled(
        source_root,
        source,
        destination_root,
        destination_path,
        || job.is_cancel_requested() || phase_failure.borrow().is_some(),
        |DestinationProgress {
             copied_bytes,
             total_bytes: _,
         }| {
            if let Some((base, total)) = progress_range {
                let current = job.status().progress;
                if let Err(error) = job.set_progress(JobProgress {
                    completed_bytes: base.saturating_add(copied_bytes),
                    total_bytes: Some(total),
                    completed_items: current.completed_items,
                    total_items: current.total_items,
                }) {
                    *phase_failure.borrow_mut() = Some(error);
                }
            }
        },
        before_replace,
    )
    .map_err(|error| {
        phase_failure
            .borrow_mut()
            .take()
            .map(job_state_error)
            .unwrap_or_else(|| destination_error(error))
    })
}

fn finish_durable_job(
    outcome: Result<JobResultSummary, NativeJobError>,
    durable: &mut DurableCasJob,
) -> Result<JobResultSummary, NativeJobError> {
    let release_outcome = if outcome.is_ok() {
        CasReleaseOutcome::Committed
    } else {
        CasReleaseOutcome::Aborted
    };
    match (outcome, durable.release(release_outcome)) {
        (Ok(mut result), Err(_)) => {
            result.warning_codes = bounded_warning_codes(
                result
                    .warning_codes
                    .into_iter()
                    .chain(["cleanup-failed".to_owned()]),
            );
            Ok(result)
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(cleanup)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!(
                "{}; durable CAS job release failed: {cleanup}",
                error.message
            ),
        )),
        (Err(error), Ok(())) => Err(error),
    }
}

fn remove_uncommitted_recovery(
    outcome: Result<JobResultSummary, NativeJobError>,
    recovery_path: &Path,
    activation_attempted: bool,
) -> Result<JobResultSummary, NativeJobError> {
    let error = match outcome {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    if activation_attempted {
        return Err(error);
    }
    match fs::remove_file(recovery_path) {
        Ok(()) => Err(error),
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
        Err(cleanup) => Err(NativeJobError::new(
            "cleanup-failed",
            format!(
                "{}; uncommitted lossless recovery cleanup failed: {cleanup}",
                error.message
            ),
        )),
    }
}

fn bounded_warning_codes(codes: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut bounded = Vec::new();
    for code in codes {
        if !bounded.contains(&code) {
            bounded.push(code);
            if bounded.len() == 16 {
                break;
            }
        }
    }
    bounded
}

fn lossless_input_error(error: LosslessError) -> NativeJobError {
    let code = match error.code {
        LosslessErrorCode::Cancelled => "cancelled",
        LosslessErrorCode::RevisionConflict => "revision-conflict",
        LosslessErrorCode::Io => "invalid-source",
        LosslessErrorCode::Store => "store-error",
        _ => "invalid-input",
    };
    NativeJobError::new(code, error.message)
}

fn lossless_operation_error(error: LosslessError) -> NativeJobError {
    let code = match error.code {
        LosslessErrorCode::Cancelled => "cancelled",
        LosslessErrorCode::RevisionConflict => "revision-conflict",
        LosslessErrorCode::HashMismatch | LosslessErrorCode::LengthMismatch => "hash-mismatch",
        LosslessErrorCode::Io | LosslessErrorCode::Store => "store-error",
        _ => "invalid-input",
    };
    NativeJobError::new(code, error.message)
}

fn destination_error(error: DestinationWriteError) -> NativeJobError {
    match error {
        DestinationWriteError::InvalidSource => {
            NativeJobError::new("invalid-source", "verified lossless archive is unavailable")
        }
        DestinationWriteError::InvalidDestination => NativeJobError::new(
            "invalid-destination",
            "lossless backup destination is invalid",
        ),
        DestinationWriteError::Cancelled => cancelled("lossless backup publication was cancelled"),
        DestinationWriteError::Io { operation, source } => {
            NativeJobError::new("destination-write-failed", format!("{operation}: {source}"))
        }
    }
}

fn job_state_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("job-error", message)
}

fn io_store_error(error: std::io::Error) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

fn cancelled(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("cancelled", message)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::job_pins::collect_durable_cas_job_roots;
    use crate::local_backup::NeverCancelled;
    use crate::lossless_backup::{
        create_and_verify_lossless_backup_v1_report, verify_lossless_package_v1,
    };
    use crate::native_file_jobs::{JobKind, JobRegistry, JobState};
    use crate::persistent_store::{AssetRepositoryAuthorityState, ColdPayloadAuthorityState};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    fn seed_complete_store(root: &Path, username: &str, use_v2_authority: bool) -> PersistentStore {
        let mut store = PersistentStore::open(root).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": username,
                    "botPresetsId": 0,
                    "personas": [{ "id": "persona" }],
                    "selectedPersona": 0,
                    "enabledModules": [],
                    "characterOrder": [],
                    "modules": [],
                    "loadouts": [],
                    "plugins": [{ "name": "Plugin" }],
                    "pluginCustomStorage": {
                        "fixture": { "username": username }
                    }
                }),
            )
            .unwrap();
        store
            .replace_put_presets(&staging, &[json!({ "name": "preset" })])
            .unwrap();
        store.replace_add_characters(&staging, &[]).unwrap();
        if use_v2_authority {
            store
                .replace_put_asset_repository_authority(
                    &staging,
                    &AssetRepositoryAuthorityState::V2 {
                        migration_id: "native-lossless-test-assets".to_owned(),
                        compatibility_hash: "ab".repeat(32),
                    },
                )
                .unwrap();
            store
                .replace_put_cold_payload_authority(
                    &staging,
                    &ColdPayloadAuthorityState::V2 {
                        migration_id: "native-lossless-test-cold".to_owned(),
                        compatibility_hash: "cd".repeat(32),
                    },
                )
                .unwrap();
        }
        store.replace_commit(&staging, Some(0)).unwrap();
        store
    }

    fn create_production_package(root: &Path, username: &str) -> PathBuf {
        let source_root = root.join("package-source");
        fs::create_dir(&source_root).unwrap();
        let mut store = seed_complete_store(&source_root, username, true);
        let cas = PayloadCas::new(&source_root).unwrap();
        let staging = source_root.join("package-staging");
        fs::create_dir(&staging).unwrap();
        let package = root.join("source.risulossless");
        create_and_verify_lossless_backup_v1_report(
            &package,
            &staging,
            &cas,
            &mut store,
            1,
            &NeverCancelled,
        )
        .unwrap();
        package
    }

    fn create_legacy_package(root: &Path) -> PathBuf {
        let source_root = root.join("legacy-package-source");
        fs::create_dir(&source_root).unwrap();
        let mut store = seed_complete_store(&source_root, "Legacy", false);
        let cas = PayloadCas::new(&source_root).unwrap();
        let staging = source_root.join("package-staging");
        fs::create_dir(&staging).unwrap();
        let package = root.join("legacy-source.risulossless");
        create_and_verify_lossless_backup_v1_report(
            &package,
            &staging,
            &cas,
            &mut store,
            1,
            &NeverCancelled,
        )
        .unwrap();
        package
    }

    fn opened(path: &Path) -> OpenedJobSource {
        OpenedJobSource {
            file: File::open(path).unwrap(),
            total_bytes: fs::metadata(path).unwrap().len(),
        }
    }

    fn wait_for_activation(job: &JobControl) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = job.status();
            if status.state == JobState::WaitingForInput {
                assert_eq!(status.phase, JobPhase::AwaitingActivation);
                return;
            }
            assert!(
                !status.state.is_terminal(),
                "lossless restore terminated before activation wait: {status:?}"
            );
            assert!(
                Instant::now() < deadline,
                "lossless restore did not reach activation wait: {status:?}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn desktop_and_android_lossless_publication_are_verified_and_owned() {
        let directory = TempDir::new().unwrap();
        let repository = directory.path().join("repository");
        fs::create_dir(&repository).unwrap();
        let store = seed_complete_store(&repository, "Export", true);
        let desktop_owned = directory.path().join("desktop-owned");
        let destination_root = directory.path().join("destination");
        let handoff_root = directory.path().join("handoffs");
        fs::create_dir(&desktop_owned).unwrap();
        fs::create_dir(&destination_root).unwrap();
        fs::create_dir(&handoff_root).unwrap();
        let desktop_destination = destination_root.join("backup.risulossless");
        let registry = JobRegistry::default();
        let desktop_job = registry.create(JobKind::ExportLosslessBackup).unwrap();

        let desktop = export_lossless_backup(
            Some(&desktop_destination),
            1,
            &desktop_owned,
            &handoff_root,
            store,
            &desktop_job,
        )
        .unwrap();

        assert_eq!(desktop.handoff_path, None);
        assert_eq!(
            desktop.source_bytes,
            fs::metadata(&desktop_destination).unwrap().len()
        );
        let verified = verify_lossless_package_v1(
            &mut File::open(&desktop_destination).unwrap(),
            &NeverCancelled,
        )
        .unwrap();
        assert_eq!(desktop.source_sha256, verified.archive_sha256);
        assert_eq!(
            collect_durable_cas_job_roots(&repository),
            Default::default()
        );

        let android_owned = directory.path().join("android-owned");
        fs::create_dir(&android_owned).unwrap();
        let android_job = registry.create(JobKind::ExportLosslessBackup).unwrap();
        let android = export_lossless_backup(
            None,
            1,
            &android_owned,
            &handoff_root,
            PersistentStore::open(&repository).unwrap(),
            &android_job,
        )
        .unwrap();
        let handoff = PathBuf::from(android.handoff_path.unwrap());

        assert_eq!(handoff.parent(), Some(handoff_root.as_path()));
        assert!(handoff.is_file());
        assert_eq!(android.source_bytes, fs::metadata(&handoff).unwrap().len());
        assert_eq!(
            collect_durable_cas_job_roots(&repository),
            Default::default()
        );
    }

    #[test]
    fn restore_publishes_recovery_before_finalize_and_releases_durable_roots() {
        let directory = TempDir::new().unwrap();
        let package = create_production_package(directory.path(), "Restored");
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let store = seed_complete_store(&target, "Old", true);
        let owned = directory.path().join("restore-owned");
        fs::create_dir(&owned).unwrap();
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreLosslessBackup, Some(1), Vec::new())
            .unwrap();
        let worker_job = Arc::clone(&job);
        let worker_owned = owned.clone();
        let worker_package = package.clone();
        let worker = std::thread::spawn(move || {
            restore_lossless_backup(
                opened(&worker_package),
                1,
                &worker_owned,
                store,
                &worker_job,
            )
        });
        wait_for_activation(&job);
        job.request_finalize().unwrap();
        let restored = worker.join().unwrap().unwrap();
        let recovery = PathBuf::from(restored.recovery_path.unwrap());

        assert_eq!(restored.revision, 2);
        assert!(recovery.is_file());
        assert_eq!(
            recovery.parent(),
            Some(target.join("persistent").join("recovery").as_path())
        );
        assert_eq!(
            PersistentStore::open(&target)
                .unwrap()
                .materialize(None)
                .unwrap()["username"],
            "Restored"
        );
        assert_eq!(collect_durable_cas_job_roots(&target), Default::default());
    }

    #[test]
    fn production_restore_rejects_legacy_authority_before_global_cas_staging() {
        let directory = TempDir::new().unwrap();
        let package = create_legacy_package(directory.path());
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let store = seed_complete_store(&target, "Old", true);
        let owned = directory.path().join("restore-owned");
        fs::create_dir(&owned).unwrap();
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreLosslessBackup, Some(1), Vec::new())
            .unwrap();

        let error = restore_lossless_backup(opened(&package), 1, &owned, store, &job).unwrap_err();

        assert_eq!(error.code, "invalid-input");
        assert_eq!(job.status().phase, JobPhase::ReadingSource);
        assert_eq!(collect_durable_cas_job_roots(&target), Default::default());
        assert_eq!(
            PersistentStore::open(&target)
                .unwrap()
                .materialize(None)
                .unwrap()["username"],
            "Old"
        );
    }

    #[test]
    fn cancelled_restore_removes_the_published_uncommitted_recovery() {
        let directory = TempDir::new().unwrap();
        let package = create_production_package(directory.path(), "Restored");
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let store = seed_complete_store(&target, "Old", true);
        let owned = directory.path().join("restore-owned");
        fs::create_dir(&owned).unwrap();
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreLosslessBackup, Some(1), Vec::new())
            .unwrap();
        let worker_job = Arc::clone(&job);
        let worker_owned = owned.clone();
        let worker_package = package.clone();
        let worker = std::thread::spawn(move || {
            restore_lossless_backup(
                opened(&worker_package),
                1,
                &worker_owned,
                store,
                &worker_job,
            )
        });
        wait_for_activation(&job);
        let recovery_directory = target.join("persistent").join("recovery");
        assert_eq!(fs::read_dir(&recovery_directory).unwrap().count(), 1);

        job.request_cancel().unwrap();
        let error = worker.join().unwrap().unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(fs::read_dir(&recovery_directory).unwrap().count(), 0);
        assert_eq!(collect_durable_cas_job_roots(&target), Default::default());
    }

    #[test]
    fn failed_lossless_outcome_aborts_its_durable_roots() {
        let directory = TempDir::new().unwrap();
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "native-lossless-abort-test",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();
        let outcome = Err(NativeJobError::new("cancelled", "cancelled"));

        let error = finish_durable_job(outcome, &mut durable).unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn unknown_activation_outcome_retains_recovery_for_reconciliation() {
        let directory = TempDir::new().unwrap();
        let recovery = directory.path().join("recovery.risulossless");
        fs::write(&recovery, b"recovery").unwrap();

        let error = remove_uncommitted_recovery(
            Err(NativeJobError::new(
                "store-error",
                "activation result is unknown",
            )),
            &recovery,
            true,
        )
        .unwrap_err();

        assert_eq!(error.code, "store-error");
        assert!(recovery.is_file());
    }
}
