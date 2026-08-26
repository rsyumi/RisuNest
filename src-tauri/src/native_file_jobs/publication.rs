#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::{
        JobKind, JobPhase, JobRegistry, OfficialPublicationAttemptResult,
        OfficialPublicationCredential, OfficialPublicationJobRequest,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn request() -> OfficialPublicationJobRequest {
        OfficialPublicationJobRequest {
            lease: "snapshot-publication".to_owned(),
            expected_revision: 7,
            account_id: "account-1".to_owned(),
            base_url: "http://127.0.0.1:3000".to_owned(),
            replacements: HashMap::from([("asset".to_owned(), "remote".to_owned())]),
            session: Some("session-1".to_owned()),
            save_date: "save-date".to_owned(),
            credential: OfficialPublicationCredential::RisuAuth {
                token: "secret".to_owned(),
            },
        }
    }

    #[test]
    fn start_validation_bounds_every_private_input_and_aggregate_replacements() {
        validate_start_request(&request()).unwrap();

        let mut invalid = request();
        invalid.account_id = "a".repeat(MAX_ACCOUNT_BYTES + 1);
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );

        let mut invalid = request();
        invalid.replacements = HashMap::from([(
            "key".to_owned(),
            "v".repeat(MAX_REPLACEMENT_STRING_BYTES + 1),
        )]);
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );

        let mut invalid = request();
        invalid.base_url = "file:///private/database".to_owned();
        assert_eq!(
            validate_start_request(&invalid).unwrap_err().code,
            "invalid-input"
        );
    }

    #[test]
    fn response_classification_keeps_exact_written_text_and_never_requires_403_bodies() {
        let written = classify_response(
            200,
            false,
            true,
            Some(br#"{"warning":"quota","reloadSession":true}"#.to_vec()),
            "account-1",
            Some("session".to_owned()),
            "date",
        )
        .unwrap();
        assert_eq!(
            written,
            OfficialPublicationAttemptResult::Written {
                account_id: "account-1".to_owned(),
                session: Some("session".to_owned()),
                save_date: "date".to_owned(),
                status: 200,
                replacement_key: r#"{"warning":"quota","reloadSession":true}"#.to_owned(),
                warning: Some("quota".to_owned()),
                reload_session: true,
            }
        );
        assert!(matches!(
            classify_response(403, false, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::ReauthenticationNeeded { status: 403, .. }
        ));
        assert!(matches!(
            classify_response(403, true, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::AuthWarning { status: 403, .. }
        ));
        assert!(matches!(
            classify_response(304, false, false, None, "account-1", None, "date").unwrap(),
            OfficialPublicationAttemptResult::NotModified { status: 304, .. }
        ));
    }

    #[test]
    fn written_response_and_errors_are_bounded_without_echoing_credentials() {
        let error = classify_response(
            200,
            false,
            false,
            Some(vec![b'x'; MAX_REPLACEMENT_STRING_BYTES + 1]),
            "account-1",
            None,
            "date",
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid-response");
        assert!(error.message.len() <= 512);
        assert!(!error.message.contains("secret"));
    }

    #[test]
    fn publication_progress_is_monotonic_across_repeated_attempts() {
        let registry = JobRegistry::default();
        let job = registry
            .create(JobKind::OfficialPublicationUpload)
            .unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        record_uploaded_bytes(&job, 10).unwrap();
        record_uploaded_bytes(&job, 5).unwrap();
        assert_eq!(job.status().progress.completed_bytes, 15);
        assert_eq!(job.status().progress.total_bytes, None);
        assert_eq!(job.status().progress.completed_items, 1);
        assert_eq!(job.status().progress.total_items, Some(2));
    }

    #[test]
    fn per_attempt_activity_is_independent_from_monotonic_public_progress() {
        let activity = Arc::new(AtomicU64::new(0));
        activity.fetch_add(8, Ordering::Release);
        let first = activity.load(Ordering::Acquire);
        activity.store(0, Ordering::Release);
        activity.fetch_add(3, Ordering::Release);
        assert_eq!(first, 8);
        assert_eq!(activity.load(Ordering::Acquire), 3);
        assert_eq!(CONTROL_POLL_INTERVAL, Duration::from_millis(50));
    }
}
