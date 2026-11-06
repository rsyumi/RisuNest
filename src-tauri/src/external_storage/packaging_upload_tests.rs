mod upload_concurrency_tests {
    use super::*;
    use crate::external_storage::transfer_job::concurrency_tests::GateProvider;

    #[test]
    fn sealed_small_packs_release_families_and_upload_more_than_two_at_once() {
        runtime().block_on(async {
            const RECORDS: usize = 10;
            let root = tempfile::tempdir().unwrap();
            let repository = fake::repository();
            let (provider, mut entered) = GateProvider::new();
            let families = PackFamilies::new(ACTIVE_PACK_FAMILIES);
            let capture = captured_record_window(root.path(), "parallel", 1, RECORDS, 0, 0, 0);
            let meta = metadata("parallel", &capture);
            let directory = root.path().join("parallel-job");
            let job = format!("parallel-{}", uuid::Uuid::new_v4());
            let holder = (job.clone(), directory.clone());
            let mut transfer = journal(&directory, &job, &capture);
            transfer.set_spool_budget(
                super::super::super::journal::SpoolBudget::new(job, move || Ok(vec![holder.clone()]))
                    .with_limit(128 * 1024).with_families(families.clone()),
            );
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let cache = root.path().join("cache");
            let cancel = Cancellation::default();
            let progress = PhaseProgress::silent();
            let (result, ()) = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(capture, vec![], root.path(), &cache, meta, &[5; 32],
                        package_limits, None, &mut transfer, &provider, &repository, &progress, &cancel),
                    async {
                        let mut initial = vec![entered.recv().await.unwrap()];
                        // Keep the initial ready wave at its completion check
                        // while preparation runs ahead.
                        loop {
                            while let Ok(object) = entered.try_recv() { initial.push(object); }
                            let files = fs::read_dir(&directory).unwrap().filter_map(|entry| entry.ok())
                                .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "spool")).count();
                            assert!(families.counts().0 <= ACTIVE_PACK_FAMILIES);
                            assert!(held_spool(&directory) <= 128 * 1024);
                            if files == RECORDS && families.counts().0 == 0 { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                        for object in &initial { provider.release(object); }
                        let mut simultaneous = Vec::new();
                        for _ in 0..4 { simultaneous.push(entered.recv().await.unwrap()); }
                        assert_eq!(families.counts(), (0, 0));
                        assert!(entered.try_recv().is_err());
                        for object in simultaneous.iter().rev() { provider.release(object); }
                        for _ in initial.len() + 4..RECORDS { let object = entered.recv().await.unwrap(); provider.release(&object); }
                    }
                )
            }).await.expect("sealed-pack pipeline stopped making progress");
            assert_eq!(packs_of(&result.unwrap()), RECORDS);
            assert_eq!(held_spool(&directory), 0);
            assert_eq!(families.counts(), (0, 0));
            assert_eq!(build_files(&cache.join("build")), 0);
        });
    }

    #[test]
    fn packaging_remote_standalone_source_does_not_nest_the_cpu_permit() {
        use crate::external_storage::{lww_tests::CycleFixture, lww_residency, lww_segment,
            previous_storage_tests, connection_store::ConnectionStore, transfer_limit};
        runtime().block_on(async {
            let f = CycleFixture::new();
            let root = f.directory_b.path();
            let bytes = vec![73; 96 * 1024];
            let id = "remote-packaging-source";
            let digest = risunest_sync_wire::hash(&bytes);
            let mut sealed = Vec::new();
            lww_segment::seal_body_stream(&mut std::io::Cursor::new(&bytes), &mut sealed,
                &f.receiver.library, id, &[7; 32], bytes.len() as u64).unwrap();
            let source = lww_residency::Source { hash: digest, library_id: f.receiver.library.clone(),
                connection_id: "receiver".into(), connection_root: root.into(), protected_segment: "synthetic-segment".into(),
                body: lww_segment::LargeBody { object_id: id.into(), sha256: risunest_sync_wire::hash(&sealed),
                    byte_length: (sealed.len() as u64).into(), plaintext_byte_length: (bytes.len() as u64).into(),
                    locator: Some(RemoteLocator { connection_identity: f.receiver.repository.connection_identity.clone(),
                        collection: None, object: id.into() }) } };
            f.provider.seed(id, ObjectRole::Pack, sealed);
            lww_residency::register(root, &source).unwrap();
            let mut connection = previous_storage_tests::receiver_connection(&f);
            connection.stored.transfer_concurrency = Some(1);
            ConnectionStore::open(root).unwrap().insert(&connection.stored).unwrap();
            connection.provider = transfer_limit::wrap(root, "receiver", connection.provider).unwrap();
            let provider = connection.provider.clone();
            let _installed = lww_residency::install_test_source_connection(root, Arc::new(connection)).unwrap();
            let (capture, _) = captured_payloads(root, "remote-packaging", 1, b"synthetic record", &[&bytes]);
            let meta = metadata("remote-packaging", &capture);
            let mut transfer = journal(&root.join("remote-packaging-job"), "remote-packaging", &capture);
            let result = tokio::time::timeout(FAMILY_TEST_BOUND,
                package_and_upload(capture, vec![], root, &root.join("remote-packaging-cache"), meta,
                    &[5; 32], limits(256 * 1024), None, &mut transfer, provider.as_ref(), &fake::repository(),
                    &PhaseProgress::silent(), &Cancellation::default())).await
                .expect("source hydration nested the preparation CPU permit").unwrap();
            assert!(packs_of(&result) >= 2);
            assert!(f.provider.read_attempts(id) > 0);
            assert_eq!(held_spool(&root.join("remote-packaging-job")), 0);
        });
    }

    #[test]
    fn failed_upload_remains_the_error_when_a_full_sealed_channel_closes() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (provider, mut entered) = GateProvider::new();
            let capture = captured_record_window(root.path(), "closed-channel", 1, 32, 0, 0, 0);
            let meta = metadata("closed-channel", &capture);
            let directory = root.path().join("job");
            let mut transfer = journal(&directory, "closed-channel", &capture);
            let mut package_limits = limits(4096);
            package_limits.target_plaintext_bytes = 1;
            let cache = root.path().join("cache");
            let repository = fake::repository();
            let progress = PhaseProgress::silent();
            let cancel = Cancellation::default();
            let (result, ()) = tokio::time::timeout(FAMILY_TEST_BOUND, async {
                tokio::join!(
                    package_and_upload(capture, vec![], root.path(), &cache, meta, &[5; 32],
                        package_limits, None, &mut transfer, &provider, &repository,
                        &progress, &cancel),
                    async {
                        let mut initial = vec![entered.recv().await.unwrap()];
                        // Hold the initial active wave until sixteen packs
                        // are queued and another sealed pack cannot be sent.
                        loop {
                            while let Ok(object) = entered.try_recv() { initial.push(object); }
                            let files = fs::read_dir(&directory).unwrap().filter_map(|entry| entry.ok())
                                .filter(|entry| entry.path().extension().is_some_and(|extension| extension == "spool")).count();
                            if files >= initial.len() + 17 { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                        *provider.fail.lock().unwrap() = Some(initial[0].clone());
                        for object in &initial { provider.release(object); }
                    }
                )
            }).await.expect("failed pipeline did not join its blocked producer");
            assert_eq!(result.err().unwrap().kind, ErrorKind::RateLimited);
            assert_eq!(transfer.families().counts(), (0, 0));
            assert_eq!(build_files(&cache.join("build")), 0);
        });
    }
}
