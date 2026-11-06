pub(crate) mod cache;
pub(crate) mod client;
pub(crate) mod commands;
pub(crate) mod asset_download_progress;
pub(crate) mod asset_cleanup_progress;
#[cfg(test)]
mod asset_cleanup_tests;
pub(crate) mod credentials;
pub(crate) mod events;
pub(crate) mod management;
pub(crate) mod media;
pub(crate) mod residency;
pub(crate) mod previous_storage;
pub(crate) mod progress;
#[cfg(test)]
mod tests;
pub(crate) mod transfer;
pub(crate) mod lww_client;
pub(crate) mod library_format;
#[cfg(test)]
pub(crate) mod hash_metrics;
mod binding;
#[cfg(test)]
pub(crate) use binding::{first_binding_cycle, hydrate_binding_bodies};
pub(crate) use binding::carry_operation_log;
pub(crate) mod notification;
#[cfg(test)]
pub(crate) mod lww_tests;
#[cfg(test)]
pub(crate) mod body_encoding_tests;
#[cfg(test)]
mod lww_large_unit_tests;
#[cfg(test)]
mod media_admission_tests;
#[cfg(test)]
mod residency_lww_tests;
#[cfg(test)]
mod representative_image_tests;
#[cfg(test)]
pub(crate) mod previous_storage_tests;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncError {
    pub code: String,
    pub status: u16,
    /// False for a rejection the same request would receive again. The scheduler
    /// stops automatic retries on it instead of backing off forever.
    pub retryable: bool,
    /// An activation failed before changing an already usable local library.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_local_ready: Option<bool>,
    /// Where the error was raised. Kept for the device log, never sent.
    #[serde(skip)]
    pub at: &'static std::panic::Location<'static>,
    /// The io, SQLite or store failure behind the code, for the device log.
    #[serde(skip)]
    pub cause: Option<String>,
}
impl SyncError {
    #[track_caller]
    pub fn new(code: impl Into<String>, status: u16) -> Self {
        let code = code.into();
        let retryable = retryable(&code, status);
        Self {
            code,
            status,
            retryable,
            startup_local_ready: None,
            at: std::panic::Location::caller(),
            cause: None,
        }
    }
    #[track_caller]
    fn caused(code: &str, status: u16, cause: String) -> Self {
        Self {
            cause: Some(cause),
            ..Self::new(code, status)
        }
    }
}
/// Client-side waits, another library operation that has not finished yet and
/// ordering outcomes, whatever their status.
pub(crate) const TRANSIENT_CODES: [&str; 15] = [
    "cancelled",
    "library-operation-busy",
    "server-sync-busy",
    "asset-jobs-busy",
    "stale-head",
    "operation-already-pending",
    "local-revision-changed",
    "remote-catch-up-required",
    "device-operation-active",
    "staging-expired",
    "upload-expired",
    "pin-expired",
    "operation-history-expired",
    "state-pin-expired",
    "journal-floor",
];
/// Transport failures, client-side waits, server load, another library
/// operation that has not finished yet and ordering outcomes are worth another
/// attempt. Every validation reply and local invariant failure is not,
/// including a reply this client rejected as invalid.
fn retryable(code: &str, status: u16) -> bool {
    match code {
        "local-storage-full" => false,
        code if TRANSIENT_CODES.contains(&code) => true,
        _ => match status {
            408 | 429 => true,
            // An unparsed body is what a gateway returns; a parsed 502 code is
            // this client refusing the reply, which repeating cannot change.
            502 => code == "server-response-error",
            500..=599 => true,
            _ => false,
        },
    }
}
// Each conversion tracks its caller, so an error raised by `?` records the line
// of that `?`.
impl From<risunest_sync_wire::WireError> for SyncError {
    #[track_caller]
    fn from(value: risunest_sync_wire::WireError) -> Self {
        Self::new(value.0, 400)
    }
}
impl From<std::io::Error> for SyncError {
    #[track_caller]
    fn from(error: std::io::Error) -> Self {
        let (code, status) = if error.kind() == std::io::ErrorKind::StorageFull {
            ("local-storage-full", 507)
        } else {
            ("local-storage", 503)
        };
        Self::caused(code, status, format!("{:?}: {}", error.kind(), crate::native_log::io_failure(&error)))
    }
}
impl From<rusqlite::Error> for SyncError {
    #[track_caller]
    fn from(error: rusqlite::Error) -> Self {
        let (code, status) = if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) {
            ("local-storage-full", 507)
        } else {
            ("local-metadata", 503)
        };
        Self::caused(code, status, crate::native_log::sqlite_failure(&error))
    }
}
impl From<crate::persistent_store::StoreError> for SyncError {
    #[track_caller]
    fn from(value: crate::persistent_store::StoreError) -> Self {
        match value {
            crate::persistent_store::StoreError::RevisionConflict { .. } => {
                Self::new("local-revision-changed", 409)
            }
            crate::persistent_store::StoreError::Validation { message } if matches!(message.as_str(), "accepted-clock-correction-required" | "incoming-clock-skew" | "clock-skew" | "equal-stamp-integrity" | "writer-collision" | "binding-authority-changed") => Self::new(message,409),
            crate::persistent_store::StoreError::Store { message } => Self::caused("local-storage", 503, message),
            crate::persistent_store::StoreError::CommitBusy => Self::new("library-operation-busy", 409),
            crate::persistent_store::StoreError::Validation { message }
                if message == crate::persistent_store::ASSET_INVENTORY_BUSY =>
                Self::new("library-operation-busy", 409),
            other => Self::caused("local-validation", 409, other.to_string()),
        }
    }
}
pub(crate) type Result<T> = std::result::Result<T, SyncError>;

impl From<risunest_sync_connect::ConnectError> for SyncError {
    #[track_caller]
    fn from(value: risunest_sync_connect::ConnectError) -> Self {
        Self::new(value.0, 400)
    }
}

#[cfg(test)]
mod classification_tests {
    use super::SyncError;

    #[test]
    fn local_storage_exhaustion_requires_space_without_masking_server_failures() {
        let io = SyncError::from(std::io::Error::from(std::io::ErrorKind::StorageFull));
        let sqlite = SyncError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL), None));
        for error in [io, sqlite, SyncError::new("local-storage-full", 507)] {
            assert_eq!(error.code, "local-storage-full");
            assert!(!error.retryable);
        }
        assert!(SyncError::new("server-storage-full", 507).retryable);
        assert!(SyncError::new("local-storage", 503).retryable);
    }

    #[test]
    fn local_store_failures_are_retryable_and_validation_stays_final() {
        use crate::persistent_store::StoreError;
        let store = SyncError::from(StoreError::Store { message: "disk I/O error".to_owned() });
        assert_eq!((store.code.as_str(), store.status, store.retryable), ("local-storage", 503, true));
        assert_eq!(store.cause.as_deref(), Some("disk I/O error"));
        let busy = SyncError::from(StoreError::CommitBusy);
        assert_eq!((busy.code.as_str(), busy.status, busy.retryable), ("library-operation-busy", 409, true));
        let conflict = SyncError::from(StoreError::RevisionConflict { expected: 1, actual: 2 });
        assert_eq!((conflict.code.as_str(), conflict.retryable), ("local-revision-changed", true));
        let collision = SyncError::from(StoreError::Validation { message: "writer-collision".to_owned() });
        assert_eq!((collision.code.as_str(), collision.retryable), ("writer-collision", false));
        let invalid = SyncError::from(StoreError::Validation { message: "unit is malformed".to_owned() });
        assert_eq!((invalid.code.as_str(), invalid.status, invalid.retryable), ("local-validation", 409, false));
        assert_eq!(invalid.cause.as_deref(), Some("unit is malformed"));
    }

    #[test]
    fn transport_waits_and_server_load_stay_retryable() {
        for (code, status) in [
            ("server-unreachable", 503),
            ("server-timeout", 503),
            ("incomplete-response", 503),
            ("directory-unreachable", 503),
            ("sync-retry-budget-exhausted", 503),
            ("cancelled", 409),
            ("library-operation-busy", 409),
            ("server-sync-busy", 409),
            ("asset-jobs-busy", 409),
            ("device-busy", 429),
            ("server-busy", 429),
            ("server-updating", 503),
            ("storage-unavailable", 503),
            ("request-timeout", 408),
            ("server-response-error", 502),
            ("staging-expired", 410),
            ("upload-expired", 410),
            ("pin-expired", 410),
            ("operation-history-expired", 410),
            ("state-pin-expired", 410),
            ("journal-floor", 410),
            ("stale-head", 412),
            ("operation-already-pending", 409),
            ("local-revision-changed", 409),
            ("remote-catch-up-required", 409),
            ("device-operation-active", 409),
        ] {
            assert!(
                SyncError::new(code, status).retryable,
                "{code} ({status}) must stay retryable"
            );
        }
    }

    #[test]
    fn validation_replies_and_local_invariants_stop_automatic_retries() {
        for (code, status) in [
            ("invalid-control-schema", 400),
            ("unordered-keys", 400),
            ("too-many-objects", 400),
            ("unauthorized", 401),
            ("forbidden", 403),
            ("object-not-found", 404),
            ("request-too-large", 413),
            ("unsupported-media-type", 415),
            ("precondition-required", 428),
            ("page-intent-conflict", 409),
            ("asset-jobs-unresolved", 409),
            ("page-gap", 409),
            ("staging-sealed", 409),
            ("staging-not-sealed", 409),
            ("descriptor-object-mismatch", 409),
            ("missing-dependency", 409),
            ("missing-related-record", 409),
            ("reference-kind-mismatch", 409),
            ("changes-digest-mismatch", 409),
            ("operation-intent-conflict", 409),
            ("staged-intent-mismatch", 409),
            ("invalid-operation-status", 502),
            ("receipt-identity-mismatch", 409),
            ("server-descriptor-semantics-mismatch", 409),
            ("cached-object-missing", 409),
            ("missing-local-payload", 409),
            ("invalid-remote-record", 502),
            ("invalid-missing-response", 502),
            ("invalid-directory-envelope", 502),
            ("unexpected-content-encoding", 502),
            ("response-too-large", 502),
            ("epoch-reconciliation-required", 409),
            ("device-credential-unavailable", 409),
        ] {
            assert!(
                !SyncError::new(code, status).retryable,
                "{code} ({status}) must block automatic retries"
            );
        }
    }
}

#[cfg(test)]
mod log_detail_tests {
    use super::SyncError;

    #[test]
    fn a_failed_question_mark_keeps_its_line_and_cause_out_of_the_reply() {
        let fail = || -> super::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::InvalidData))?;
            Ok(())
        };
        let line = line!() - 3;
        let error = fail().unwrap_err();
        assert_eq!(error.code, "local-storage");
        assert_eq!(error.at.line(), line);
        assert!(error.at.file().ends_with("mod.rs"));
        assert!(error.cause.as_deref().unwrap().starts_with("InvalidData: "));
        let reply = serde_json::to_value(&error).unwrap();
        let keys = reply.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys, ["code", "status", "retryable"]);
    }

    #[test]
    fn a_wrapped_json_failure_keeps_its_payload_out_of_the_cause() {
        let shape = || serde_json::from_str::<u32>("\"private-payload-value\"").unwrap_err();
        let io = SyncError::from(std::io::Error::new(std::io::ErrorKind::InvalidData, shape()));
        let sqlite = SyncError::from(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(shape()),
        ));
        assert!(io.cause.as_deref().unwrap().starts_with("InvalidData: json-shape at line 1 column"));
        for error in [io, sqlite] {
            let cause = error.cause.unwrap();
            assert!(cause.contains("json-shape"), "{cause}");
            assert!(!cause.contains("private-payload-value"));
        }
    }

    #[test]
    fn a_created_error_records_where_it_was_made_without_a_cause() {
        let line = line!() + 1;
        let error = SyncError::new("server-sync-busy", 409);
        assert_eq!(error.at.line(), line);
        assert!(error.cause.is_none());
    }
}
