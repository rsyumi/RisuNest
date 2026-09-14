use super::{capabilities::*, contract::*, publication::*, quota::*, registry::Registry};
use std::sync::Arc;

use super::fake::{capabilities, locator, repository, FakeProvider};
fn observation(commit: &str) -> HeadObservation {
    HeadObservation {
        commit_id: commit.into(),
        authenticated_body_hash: risunest_sync_wire::hash(commit.as_bytes()),
        version: None,
    }
}

#[test]
fn cas_two_writers_have_exactly_one_winner_for_create_and_update() {
    for initial in [false, true] {
        let provider = Arc::new(FakeProvider::new(true));
        let expected = if initial {
            provider
                .write(&locator(), Some(&ExpectedHead::Absent), b"first")
                .unwrap();
            ExpectedHead::Exact(VersionToken("1".into()))
        } else {
            ExpectedHead::Absent
        };
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|n| {
                let p = provider.clone();
                let b = barrier.clone();
                let e = expected.clone();
                std::thread::spawn(move || {
                    b.wait();
                    futures::executor::block_on(p.compare_exchange_head(
                        &repository(),
                        &locator(),
                        &e,
                        &HeadBytes::new(vec![n]).unwrap(),
                        &Cancellation::default(),
                    ))
                    .is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
}

#[test]
fn sequential_competitors_can_both_confirm_and_recovery_snapshots_survive() {
    let p = FakeProvider::new(false);
    p.write(
        &RemoteLocator {
            object: "snapshot-a".into(),
            ..locator()
        },
        None,
        b"a",
    )
    .unwrap();
    p.write(
        &RemoteLocator {
            object: "snapshot-b".into(),
            ..locator()
        },
        None,
        b"b",
    )
    .unwrap();
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    let mut b = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "b".into(),
        observation("b").authenticated_body_hash,
    )
    .unwrap();
    a.before_write(None, ExecutionSession::Foreground).unwrap();
    b.before_write(None, ExecutionSession::Foreground).unwrap();
    p.write(&locator(), None, b"a").unwrap();
    assert_eq!(
        a.observe_result(Some(&observation("a"))),
        Outcome::Confirmed
    );
    p.write(&locator(), None, b"b").unwrap();
    assert_eq!(
        b.observe_result(Some(&observation("b"))),
        Outcome::Confirmed
    );
    assert_eq!(p.state.lock().unwrap().objects.len(), 3);
    assert_eq!(
        p.write(&locator(), Some(&ExpectedHead::Absent), b"x")
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn response_loss_is_reconciled_by_observation_and_never_repeated_as_old_write() {
    let p = FakeProvider::new(false);
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    a.before_write(None, ExecutionSession::ExitDrain).unwrap();
    p.state.lock().unwrap().lose_response = true;
    let error = p.write(&locator(), None, b"a").unwrap_err();
    assert_eq!(a.write_failed(&error), Outcome::PublicationUnknown);
    assert!(a.before_write(None, ExecutionSession::Foreground).is_err());
    assert_eq!(
        a.observe_result(Some(&observation("b"))),
        Outcome::PublicationUnknown
    );
    assert_eq!(
        a.observe_result(Some(&observation("a"))),
        Outcome::Confirmed
    );
    assert_eq!(p.state.lock().unwrap().next_version, 1);
}

#[test]
fn sequential_hidden_and_changed_or_missing_head_cannot_publish() {
    let expected = observation("base");
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        Some(expected.clone()),
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    assert_eq!(
        a.before_write(Some(&expected), ExecutionSession::Hidden)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert!(a.before_write(None, ExecutionSession::Foreground).is_err());
    assert!(a
        .before_write(Some(&expected), ExecutionSession::Foreground)
        .is_err());
    assert!(Attempt::new(
        &capabilities(false),
        PublicationStrategy::Cas,
        None,
        "a".into(),
        observation("a").authenticated_body_hash
    )
    .is_err());
}

#[test]
fn quota_is_shared_persistent_atomic_and_does_not_reset_on_restore_or_retry() {
    let mut ledger = QuotaLedger::default();
    ledger.configure(
        "account",
        "download",
        Bucket {
            limit: 500,
            used: 499,
            reset: QuotaReset::At { unix_ms: 1000 },
            blocked_until_ms: None,
            last_reset_ms: None,
        },
    );
    let cost = RequestCost {
        bucket: "download".into(),
        shared_account: "account".into(),
        units: 1,
        reset: QuotaReset::At { unix_ms: 1000 },
    };
    assert!(ledger.reserve(&[cost.clone(), cost.clone()], 500).is_err());
    assert_eq!(ledger.used("account", "download"), Some(499));
    ledger.reserve(&[cost.clone()], 500).unwrap();
    let mut reopened: QuotaLedger =
        serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
    assert_eq!(
        reopened.reserve(&[cost.clone()], 999).unwrap_err().kind,
        ErrorKind::DailyQuotaExhausted
    );
    reopened.reserve(&[cost.clone()], 1000).unwrap();
    reopened.configure(
        "account",
        "download",
        Bucket {
            limit: 500,
            used: 0,
            reset: QuotaReset::At { unix_ms: 1000 },
            blocked_until_ms: None,
            last_reset_ms: None,
        },
    );
    reopened.reserve(&[cost], 1001).unwrap();
    assert_eq!(reopened.used("account", "download"), Some(2));
}

#[test]
fn unavailable_services_stay_hidden_and_head_size_and_sdk_overhead_are_bounded() {
    let registry = Registry::default();
    assert!(registry.available().is_empty());
    assert!(registry.get("s3").is_err());
    assert!(HeadBytes::new(vec![0; HeadBytes::MAX_BYTES + 1]).is_err());
    let c = Capabilities {
        max_stored_bytes: Some(250_000_000),
        sdk_overhead_bytes: 100,
        ..Default::default()
    };
    assert_eq!(c.payload_limit(64).unwrap(), Some(249_999_836));
    assert!(c.payload_limit(250_000_000).is_err());
}

#[test]
fn bounded_spool_round_trip_is_idempotent_and_rejects_corruption_and_overrun() {
    use super::transfer::{SpoolSink, SpoolSource};
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source");
        let bytes = vec![42; 100_005];
        std::fs::write(&source_path, &bytes).unwrap();
        let digest = risunest_sync_wire::hash(&bytes);
        let source = SpoolSource::verified(&source_path, bytes.len() as u64, &digest).unwrap();
        let p = FakeProvider::new(false);
        let repository = repository();
        let cancel = Cancellation::default();
        let intent = ObjectIntent {
            repository_id: repository.repository_id.clone(),
            job_id: "job".into(),
            object_id: "pack".into(),
            role: ObjectRole::Pack,
            byte_length: bytes.len() as u64,
            sha256: digest.clone(),
        };
        let first = p
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        assert_eq!(
            first,
            p.create_object(&repository, &intent, &source, None, &cancel)
                .await
                .unwrap()
        );
        let output = dir.path().join("received");
        let mut sink = SpoolSink::create(&output, bytes.len() as u64).unwrap();
        p.read_object(&repository, &first.locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        assert!(sink.is_verified());
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        assert!(source.open(bytes.len() as u64, 1, &cancel).await.is_err());
        let mut limited = SpoolSink::create(&dir.path().join("limited"), 4).unwrap();
        let mut writer = limited.open(0, 4, &cancel).await.unwrap();
        assert!(writer.write_all(b"12345").await.is_err());
        drop(writer);
        assert!(!limited.is_verified());
        assert!(limited.finish(4, &digest).await.is_err());
        cancel.cancel();
        assert!(source.open(0, 1, &cancel).await.is_err());
    });
}

#[test]
fn fake_snapshot_discovery_and_lost_upload_reconcile_use_complete_immutable_objects() {
    use super::transfer::SpoolSource;
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic-snapshot");
        std::fs::write(&path, b"synthetic-snapshot").unwrap();
        let digest = risunest_sync_wire::hash(b"synthetic-snapshot");
        let source = SpoolSource::verified(&path, 18, &digest).unwrap();
        let provider = FakeProvider::new(true);
        let repository = repository();
        let cancel = Cancellation::default();
        let mut intent = ObjectIntent {
            repository_id: repository.repository_id.clone(),
            job_id: "synthetic-job".into(),
            object_id: "snapshot-a".into(),
            role: ObjectRole::Snapshot,
            byte_length: 18,
            sha256: digest,
        };
        provider.state.lock().unwrap().lose_response = true;
        assert_eq!(
            provider
                .create_object(&repository, &intent, &source, None, &cancel)
                .await
                .unwrap_err()
                .kind,
            ErrorKind::Transient
        );
        let resume = ResumeState {
            sealed_state: SecretRef("synthetic-only".into()),
            confirmed_offset: 0,
            expires_at_ms: None,
        };
        assert!(matches!(
            provider
                .reconcile_upload(&repository, &intent, &resume, &cancel)
                .await
                .unwrap(),
            UploadResolution::Complete(_)
        ));
        provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        intent.object_id = "snapshot-b".into();
        provider
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        let first = provider
            .list_objects(&repository, Collection::Snapshots, None, 1, &cancel)
            .await
            .unwrap();
        assert_eq!(first.objects[0].locator.object, "snapshot-a");
        let second = provider
            .list_objects(
                &repository,
                Collection::Snapshots,
                first.next_cursor.as_deref(),
                1,
                &cancel,
            )
            .await
            .unwrap();
        assert_eq!(second.objects[0].locator.object, "snapshot-b");
        assert!(second.next_cursor.is_none());
        assert!(provider
            .list_objects(&repository, Collection::BackupPoints, None, 10, &cancel)
            .await
            .unwrap()
            .objects
            .is_empty());
        assert_eq!(provider.state.lock().unwrap().next_version, 2);
    });
}
