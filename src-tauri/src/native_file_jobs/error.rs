// Shared NativeJobError mappers for the native file job writers so the
// TS-visible job error codes cannot drift between job kinds. Modules with an
// intentionally different mapping (restore.rs cancel-aware/store-error
// validation mapping, kei.rs transport mapping, screenshot_output.rs and
// official_snapshot.rs availability mappings) keep explicit local overrides.
use super::NativeJobError;
use crate::persistent_store::export::destination::DestinationWriteError;
use crate::persistent_store::StoreError;

pub(super) fn store_error(error: StoreError) -> NativeJobError {
    match error {
        StoreError::RevisionConflict { .. } => {
            NativeJobError::new("revision-conflict", error.to_string())
        }
        StoreError::Validation { .. } => invalid_input(error.to_string()),
        StoreError::SnapshotReleased | StoreError::Store { .. } => {
            NativeJobError::new("store-error", error.to_string())
        }
    }
}

// Combines a writer's outcome with its revision/lease release, keeping the
// fail-closed rule: a failed release turns even a successful write into a
// cleanup-failed error. The label names what failed to release in the
// combined message (an intentional per-writer difference).
pub(super) fn finish_with_release(
    outcome: Result<super::JobResultSummary, NativeJobError>,
    release: crate::persistent_store::StoreResult<()>,
    release_failure: &str,
) -> Result<super::JobResultSummary, NativeJobError> {
    match (outcome, release) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(store_error(error)),
        (Err(error), Err(release_error)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{}; {release_failure}: {release_error}", error.message),
        )),
    }
}

pub(super) fn io_error(error: std::io::Error) -> NativeJobError {
    NativeJobError::new("io-error", error.to_string())
}

pub(super) fn invalid_input(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-input", message)
}

pub(super) fn cancelled(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("cancelled", message)
}

pub(super) fn job_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("job-error", message)
}

// Destination failures share one code set; only the human-readable messages
// vary per job kind.
pub(super) fn destination_error_with(
    error: DestinationWriteError,
    source_message: &str,
    destination_message: &str,
    cancelled_message: &str,
) -> NativeJobError {
    match error {
        DestinationWriteError::InvalidSource => {
            NativeJobError::new("invalid-source", source_message)
        }
        DestinationWriteError::InvalidDestination => {
            NativeJobError::new("invalid-destination", destination_message)
        }
        DestinationWriteError::Cancelled => cancelled(cancelled_message),
        DestinationWriteError::Io { operation, source } => {
            NativeJobError::new("destination-write-failed", format!("{operation}: {source}"))
        }
    }
}
