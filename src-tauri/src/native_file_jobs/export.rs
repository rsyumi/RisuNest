use super::error::{self, cancelled, destination_error_with, job_error};
use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use crate::persistent_store::export::{self, destination, EXPORT_CANCELLED_MESSAGE};
use crate::persistent_store::{PreparedRisuSaveExport, StoreError};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};

pub(crate) fn export_block_risu_save(
    mut prepared: PreparedRisuSaveExport,
    destination_path: &Path,
    omit_account: bool,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    let reader = prepared.take_reader().map_err(store_error)?;
    if job.is_cancel_requested() {
        return finish_with_cleanup_failure(
            cancelled("export cancelled before encoding"),
            prepared.release(reader),
        );
    }
    if let Err(error) = job.start(JobPhase::WritingExport) {
        let primary = if job.is_cancel_requested() {
            cancelled("export cancelled before encoding")
        } else {
            job_error(error)
        };
        return finish_with_cleanup_failure(primary, prepared.release(reader));
    }
    let progress_failure = RefCell::new(None);
    let completed_items = Cell::new(0u64);
    let total_items = Cell::new(None);
    let encoded = export::create_controlled(
        &reader.connection,
        &prepared.snapshots_dir,
        &reader.target,
        &prepared.lease,
        omit_account,
        || job.is_cancel_requested() || progress_failure.borrow().is_some(),
        |completed_bytes, completed, total| {
            completed_items.set(completed);
            total_items.set(Some(total));
            if job.is_cancel_requested() {
                return;
            }
            if let Err(error) = job.set_progress(JobProgress {
                completed_bytes,
                total_bytes: None,
                completed_items: completed,
                total_items: Some(total),
            }) {
                if !job.is_cancel_requested() {
                    *progress_failure.borrow_mut() = Some(error);
                }
            }
        },
    );

    let encoded = match encoded {
        Ok(encoded) => encoded,
        Err(error) => {
            let primary = progress_failure
                .into_inner()
                .map(job_error)
                .unwrap_or_else(|| store_error(error));
            return finish_with_cleanup_failure(primary, prepared.release(reader));
        }
    };
    if let Some(error) = progress_failure.into_inner() {
        return finish_export_cleanup(
            Err(job_error(error)),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    }
    if job.is_cancel_requested() {
        return finish_export_cleanup(
            Err(cancelled("export cancelled before destination publication")),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    }
    if let Err(error) = job.set_phase(JobPhase::PublishingDestination) {
        let error = if job.is_cancel_requested() {
            cancelled("export cancelled before destination publication")
        } else {
            job_error(error)
        };
        return finish_export_cleanup(
            Err(error),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    }

    let source = PathBuf::from(&encoded.path);
    let Some(source_root) = source.parent() else {
        return finish_export_cleanup(
            Err(invalid_destination(
                "native export source directory is unavailable",
            )),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    };
    let Some(destination_root) = destination_path.parent() else {
        return finish_export_cleanup(
            Err(invalid_destination("destination directory is unavailable")),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    };
    let total_bytes = encoded.bytes.saturating_mul(2);
    if let Err(error) = job.set_progress(JobProgress {
        completed_bytes: encoded.bytes,
        total_bytes: Some(total_bytes),
        completed_items: completed_items.get(),
        total_items: total_items.get(),
    }) {
        return finish_export_cleanup(
            Err(if job.is_cancel_requested() {
                cancelled("export cancelled before destination publication")
            } else {
                job_error(error)
            }),
            &prepared,
            reader,
            Some(Path::new(&encoded.path)),
        );
    }

    let phase_failure = RefCell::new(None);
    let published = destination::write_desktop_destination_controlled(
        source_root,
        &source,
        destination_root,
        destination_path,
        || job.is_cancel_requested() || phase_failure.borrow().is_some(),
        |progress| {
            if job.is_cancel_requested() {
                return;
            }
            if let Err(error) = job.set_progress(JobProgress {
                completed_bytes: encoded.bytes.saturating_add(progress.copied_bytes),
                total_bytes: Some(total_bytes),
                completed_items: completed_items.get(),
                total_items: total_items.get(),
            }) {
                if !job.is_cancel_requested() {
                    *phase_failure.borrow_mut() = Some(error);
                }
            }
        },
        || {
            if phase_failure.borrow().is_some() {
                return Err(destination::DestinationWriteError::Cancelled);
            }
            if job.is_cancel_requested() {
                return Err(destination::DestinationWriteError::Cancelled);
            }
            job.set_phase(JobPhase::FinalizingExport).map_err(|error| {
                if !job.is_cancel_requested() {
                    *phase_failure.borrow_mut() = Some(error);
                }
                destination::DestinationWriteError::Cancelled
            })
        },
    );
    let published = match published {
        Ok(result) => Ok(JobResultSummary {
            revision: prepared.revision,
            source_bytes: result.bytes,
            source_sha256: result.sha256,
            character_count: encoded.character_count,
            preset_count: encoded.preset_count,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: None,
        }),
        Err(error) => Err(phase_failure
            .into_inner()
            .map(job_error)
            .unwrap_or_else(|| destination_error(error))),
    };
    finish_export_cleanup(published, &prepared, reader, Some(Path::new(&encoded.path)))
}

fn finish_export_cleanup(
    outcome: Result<JobResultSummary, NativeJobError>,
    prepared: &PreparedRisuSaveExport,
    reader: crate::persistent_store::RevisionReadLease,
    export_path: Option<&Path>,
) -> Result<JobResultSummary, NativeJobError> {
    let cleanup_error = match prepared.release(reader) {
        Err(error) => Some(error),
        Ok(()) => export_path.and_then(|path| prepared.cleanup_file(path).err()),
    };
    match (outcome, cleanup_error) {
        (Ok(mut result), Some(_)) => {
            result.warning_codes.push("cleanup-failed".to_owned());
            Ok(result)
        }
        (Ok(result), None) => Ok(result),
        (Err(error), Some(cleanup)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{}; native export cleanup failed: {cleanup}", error.message),
        )),
        (Err(error), None) => Err(error),
    }
}

fn finish_with_cleanup_failure(
    outcome: NativeJobError,
    cleanup: Result<(), StoreError>,
) -> Result<JobResultSummary, NativeJobError> {
    match cleanup {
        Ok(()) => Err(outcome),
        Err(error) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{}; native export cleanup failed: {error}", outcome.message),
        )),
    }
}

fn store_error(error: StoreError) -> NativeJobError {
    // Local override: a store-level export cancellation surfaces as
    // "cancelled" here; everything else uses the shared mapping.
    match error {
        StoreError::Validation { ref message } if message == EXPORT_CANCELLED_MESSAGE => {
            cancelled(message)
        }
        error => error::store_error(error),
    }
}

fn destination_error(error: destination::DestinationWriteError) -> NativeJobError {
    destination_error_with(
        error,
        "native export source is unavailable",
        "desktop export destination is invalid",
        "export cancelled before destination publication",
    )
}

fn invalid_destination(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-destination", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use crate::persistent_store::PersistentStore;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, PersistentStore, i64) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": "Native Export",
                    "account": { "token": "secret" },
                    "modules": [{ "name": "Module" }],
                    "loadouts": [{ "name": "Loadout" }],
                    "plugins": [{ "name": "Plugin" }],
                    "pluginCustomStorage": { "plugin": { "enabled": true } }
                }),
            )
            .unwrap();
        store
            .replace_put_presets(
                &staging,
                &[json!({ "name": "Preset A" }), json!({ "name": "Preset B" })],
            )
            .unwrap();
        store
            .replace_add_characters(
                &staging,
                &[
                    json!({
                        "type": "character",
                        "chaId": "trash-first",
                        "name": "Trash First",
                        "trashTime": 10,
                        "chats": [{
                            "id": "trash-chat",
                            "name": "Trash Chat",
                            "message": [{ "role": "user", "data": "trash", "chatId": "t1" }]
                        }]
                    }),
                    json!({
                        "type": "character",
                        "chaId": "live-second",
                        "name": "Live Second",
                        "chats": [{
                            "id": "live-chat",
                            "name": "Live Chat",
                            "message": [
                                { "role": "user", "data": "hello", "chatId": "l1" },
                                { "role": "char", "data": "world", "chatId": "l2" }
                            ]
                        }]
                    }),
                ],
            )
            .unwrap();
        let revision = store.replace_commit(&staging, None).unwrap().revision;
        (directory, store, revision)
    }

    fn frozen_export(omit_account: bool) -> (&'static str, usize, &'static str) {
        if omit_account {
            (
                include_str!("../../fixtures/risusave/pre-native-job-export-omit-account-true.b64"),
                796,
                "29dfc33b0f964cd3d54791fb92a12565f393e7710f713622cd52b667e66d3921",
            )
        } else {
            (
                include_str!(
                    "../../fixtures/risusave/pre-native-job-export-omit-account-false.b64"
                ),
                813,
                "2b84dcc0abeafc7354354e0d1fb3360443011bc73bb364ffa24180f29f0e0baa",
            )
        }
    }

    #[test]
    fn desktop_job_preserves_current_block_bytes_and_omit_account_semantics() {
        for omit_account in [false, true] {
            let (directory, mut store, revision) = fixture();
            let (encoded_golden, expected_length, expected_sha256) = frozen_export(omit_account);
            let expected = STANDARD.decode(encoded_golden.trim()).unwrap();
            assert_eq!(expected.len(), expected_length);
            assert_eq!(hex::encode(Sha256::digest(&expected)), expected_sha256);

            let prepared = store.prepare_risu_save_export(revision).unwrap();
            let chosen = directory.path().join("chosen");
            fs::create_dir_all(&chosen).unwrap();
            let destination = chosen.join("backup.risudat");
            let job = JobRegistry::default()
                .create(JobKind::ExportBlockRisuSave)
                .unwrap();

            let result =
                export_block_risu_save(prepared, &destination, omit_account, &job).unwrap();

            assert_eq!(fs::read(&destination).unwrap(), expected);
            assert_eq!(result.revision, revision);
            assert_eq!(result.source_bytes, expected.len() as u64);
            assert_eq!(result.source_sha256, expected_sha256);
            assert_eq!(result.character_count, 2);
            assert_eq!(result.preset_count, 2);
            assert_eq!(job.status().phase, JobPhase::FinalizingExport);
            let exports = directory.path().join("persistent").join("exports");
            assert!(fs::read_dir(exports).unwrap().next().is_none());
        }
    }

    #[test]
    fn cancellation_before_encoding_preserves_existing_destination_and_releases_the_lease() {
        let (directory, mut store, revision) = fixture();
        let prepared = store.prepare_risu_save_export(revision).unwrap();
        let chosen = directory.path().join("chosen");
        fs::create_dir_all(&chosen).unwrap();
        let destination = chosen.join("backup.risudat");
        fs::write(&destination, b"previous export").unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportBlockRisuSave)
            .unwrap();
        job.request_cancel().unwrap();

        let error = export_block_risu_save(prepared, &destination, false, &job).unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(fs::read(destination).unwrap(), b"previous export");
        let exports = directory.path().join("persistent").join("exports");
        assert!(!exports.exists() || fs::read_dir(exports).unwrap().next().is_none());
    }

    #[test]
    fn job_owned_connection_exports_the_pinned_revision_after_the_live_store_advances() {
        let (directory, mut store, revision) = fixture();
        let baseline_lease = store.acquire_revision(revision).unwrap().lease;
        let baseline = store.export_risu_save(&baseline_lease, false).unwrap();
        let expected = fs::read(&baseline.path).unwrap();
        store
            .cleanup_risu_save_export(Path::new(&baseline.path))
            .unwrap();
        store.release_revision(&baseline_lease).unwrap();
        let prepared = store.prepare_risu_save_export(revision).unwrap();

        let next = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&next, &json!({ "username": "New live revision" }))
            .unwrap();
        store.replace_put_presets(&next, &[]).unwrap();
        let next_revision = store.replace_commit(&next, None).unwrap().revision;
        assert!(next_revision > revision);

        let chosen = directory.path().join("chosen");
        fs::create_dir_all(&chosen).unwrap();
        let destination = chosen.join("pinned.risudat");
        let job = JobRegistry::default()
            .create(JobKind::ExportBlockRisuSave)
            .unwrap();
        let result = export_block_risu_save(prepared, &destination, false, &job).unwrap();

        assert_eq!(result.revision, revision);
        assert_eq!(fs::read(destination).unwrap(), expected);
        assert_eq!(store.revision().unwrap(), next_revision);
    }
}
