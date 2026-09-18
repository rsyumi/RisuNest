use super::{capabilities::*, contract::*, publication::*, registry::Registry};
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
    a.before_write(None, PublicationMode::Foreground).unwrap();
    b.before_write(None, PublicationMode::Foreground).unwrap();
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
    a.before_write(None, PublicationMode::ExitDrain).unwrap();
    p.state.lock().unwrap().lose_response = true;
    let error = p.write(&locator(), None, b"a").unwrap_err();
    assert_eq!(a.write_failed(&error), Outcome::PublicationUnknown);
    assert!(a.before_write(None, PublicationMode::Foreground).is_err());
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
fn sequential_changed_or_missing_head_cannot_publish() {
    let expected = observation("base");
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        Some(expected.clone()),
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    assert!(a.before_write(None, PublicationMode::Foreground).is_err());
    assert!(a
        .before_write(Some(&expected), PublicationMode::Foreground)
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
fn publication_classification_requires_the_exact_commit_and_state() {
    assert_eq!(
        classify_publication("commit", "snapshot-state", Some(("commit", "snapshot-state")), false),
        PublicationObservation::Confirmed
    );
    for observed in [None, Some(("other", "snapshot-state")), Some(("commit", "other"))] {
        assert_eq!(
            classify_publication("commit", "snapshot-state", observed, false),
            PublicationObservation::Unknown
        );
    }
    assert_eq!(
        classify_publication("commit", "snapshot-state", Some(("other", "other")), true),
        PublicationObservation::Rejected
    );
}

#[test]
fn every_listed_provider_has_a_factory_and_only_registration_exposes_it() {
    use super::{fake::MemoryVault, providers, registry::PROVIDER_IDS};
    let test = super::fake::loopback_dependencies(MemoryVault::default(), 0);
    let mut registry = Registry::default();
    assert!(providers::create("proton", test.dependencies.clone()).is_err());
    for id in PROVIDER_IDS {
        let provider = providers::create(id, test.dependencies.clone()).unwrap();
        // A handle from another provider never yields a head.
        assert!(provider.head_locator(&repository()).is_err());
        assert!(registry.get(id).is_err());
        registry.register(id, provider).unwrap();
        assert!(registry.get(id).is_ok());
    }
    assert_eq!(registry.available().len(), PROVIDER_IDS.len());
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        let mut cancelled_reader = source.open(0, 1, &cancel).await.unwrap();
        let cancelled_output = dir.path().join("cancelled-output");
        let mut cancelled_sink = SpoolSink::create(&cancelled_output, 1).unwrap();
        let mut cancelled_writer = cancelled_sink.open(0, 1, &cancel).await.unwrap();
        cancel.cancel();
        let mut byte = [0];
        assert_eq!(
            cancelled_reader
                .read_exact(&mut byte)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Other
        );
        assert_eq!(
            cancelled_reader
                .read_exact(&mut byte)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::Other
        );
        assert_eq!(
            cancelled_writer.write_all(&byte).await.unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
        assert_eq!(
            cancelled_writer.write_all(&byte).await.unwrap_err().kind(),
            std::io::ErrorKind::Other
        );
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
            role: ObjectRole::SyncState,
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
                .reconcile_upload(&repository, &intent, Some(&resume), &cancel)
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
