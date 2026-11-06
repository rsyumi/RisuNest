use super::*;
use crate::external_storage::{lww_tests::{CycleFixture, small_asset}, transfer_job::concurrency_tests::GateProvider};

#[test]
fn standalone_siblings_settle_with_original_ids_and_uncertain_body_reconciles_after_restart() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut f = CycleFixture::new();
        for index in 0..4 {
            small_asset(&mut f.a, &format!("standalone-{index}"), &vec![40 + index; 5 * 1024 * 1024]);
        }
        let (mut gate, mut entered) = GateProvider::new();
        gate.inner = f.provider.clone();
        gate.minimum_bytes = 4 * 1024 * 1024;
        let gate = Arc::new(gate);
        f.sender.provider = gate.clone();
        let cancel = Cancellation::default();
        let (result, identities) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                f.sender.publish(&mut f.a, DecimalU64(0), &[], &cancel),
                async {
                    let mut identities = Vec::new();
                    for _ in 0..4 { identities.push(entered.recv().await.unwrap()); }
                    *gate.fail.lock().unwrap() = Some(identities[0].clone());
                    for id in identities.iter().rev() { gate.release(id); }
                    identities
                }
            )
        }).await.expect("standalone bodies did not overlap");
        assert_eq!(result.err().unwrap().kind, ErrorKind::RateLimited);
        f.restart_a();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let pending = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
        assert_eq!(pending.bodies.len(), 4);
        for id in &identities[1..] {
            let body = pending.bodies.iter().find(|body| &body.object_id == id).unwrap();
            assert!(body.complete);
            assert_eq!(body.locator.as_ref().unwrap().object, *id);
        }
        let uncertain = pending.bodies.iter().find(|body| body.object_id == identities[0]).unwrap();
        assert!(!uncertain.complete && uncertain.resume.is_none());
        assert!(!pending.dispatched);
        f.sender.provider = f.provider.clone();
        assert_eq!(f.publish_a().await.segments.0, 1);
        for id in identities { assert_eq!(f.provider.upload_attempts(&id), 1); }
        f.receive_b().await;
        let staging = tempfile::tempdir().unwrap();
        let received = f.receiver.published_state(&mut f.b, staging.path(), &cancel).await.unwrap();
        assert_eq!(received.standalone.len(), 4);
    });
}

#[test]
fn full_publication_spools_remote_small_and_standalone_bodies_with_one_shared_slot() {
    use crate::external_storage::{connection_store::ConnectionStore, lww_residency, previous_storage_tests, transfer_limit};
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
        let mut f = CycleFixture::new();
        let mut sources = Vec::new();
        for (index, size) in [96 * 1024, 5 * 1024 * 1024].into_iter().enumerate() {
            let bytes = vec![60 + index as u8; size];
            let id = format!("remote-source-{index}");
            let mut sealed = Vec::new();
            segment::seal_body_stream(&mut Cursor::new(&bytes), &mut sealed, &f.receiver.library, &id,
                &[7; 32], bytes.len() as u64).unwrap();
            let hash = risunest_sync_wire::hash(&bytes);
            let source = lww_residency::Source { hash: hash.clone(), library_id: f.receiver.library.clone(),
                connection_id: "receiver".into(), connection_root: f.directory_b.path().into(),
                protected_segment: "synthetic-segment".into(),
                body: LargeBody { object_id: id.clone(), sha256: risunest_sync_wire::hash(&sealed),
                    byte_length: (sealed.len() as u64).into(), plaintext_byte_length: (bytes.len() as u64).into(),
                    locator: Some(RemoteLocator { connection_identity: f.receiver.repository.connection_identity.clone(),
                        collection: None, object: id.clone() }) } };
            f.provider.seed(&id, ObjectRole::Pack, sealed);
            lww_residency::register(f.directory_b.path(), &source).unwrap();
            f.b.commit_asset_alias(&crate::persistent_store::AssetAlias {
                key: format!("remote-alias-{index}"), object_hash: Some(hash), kind: "asset".into(),
                size: size as i64, mime: "application/octet-stream".into(), name: "synthetic".into(),
                ext: "bin".into(), inlay_type: None, width: None, height: None, metadata: serde_json::json!({}),
            }, f.b.revision().unwrap()).unwrap();
            sources.push(source);
        }
        let mut connection = previous_storage_tests::receiver_connection(&f);
        connection.stored.transfer_concurrency = Some(1);
        ConnectionStore::open(f.directory_b.path()).unwrap().insert(&connection.stored).unwrap();
        connection.provider = transfer_limit::wrap(f.directory_b.path(), "receiver", connection.provider).unwrap();
        f.receiver.provider = transfer_limit::wrap(f.directory_b.path(), "receiver", f.provider.clone()).unwrap();
        let _installed = lww_residency::install_test_source_connection(f.directory_b.path(), Arc::new(connection)).unwrap();
        let result = tokio::time::timeout(crate::external_storage::fake::DEADLOCK_TEST_BOUND,
            f.receiver.publish(&mut f.b, 0.into(), &[], &Cancellation::default())).await
            .expect("full one-slot publication deadlocked").unwrap();
        assert_eq!(result.segments.0, 1);
        for source in sources {
            assert!(f.provider.read_attempts(&source.body.object_id) > 0);
            assert!(!f.b.external_lww_object_is_local(&source.hash).unwrap());
        }
    });
}
