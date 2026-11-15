use super::{contract::*, lww_tests::CycleFixture};
use crate::persistent_store::{
    external_capture::{published_json_asset_roots, BackupBodyRole},
    lww::UnitMutation,
    PersistentStore, WorkingSetCommit,
};
use risunest_sync_wire::unit::{UnitKey, UnitValue};
use serde_json::{json, Value};

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}
fn set(store: &mut PersistentStore, key: &[&str], value: Value) {
    store.commit(&WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        unit_mutations: Some(vec![UnitMutation::Set { key: UnitKey::new(key).unwrap(), value }]),
        ..Default::default()
    }).unwrap();
}
// One body fits the segment and one exceeds the segment body bound, so both
// control carriages are exercised.
fn large_fixture(repository_id: &str) -> (CycleFixture, Value, Value) {
    let mut f = CycleFixture::with_repository_id(repository_id);
    let segment = json!("s".repeat(2 * 1024 * 1024));
    let catalog = json!("c".repeat(super::lww_segment::SMALL_BODY_BYTES + 1));
    set(&mut f.a, &["root", "additionalPrompt"], segment.clone());
    set(&mut f.a, &["root", "NAIImgUrl"], catalog.clone());
    assert!(f.a.lww_read_outbox(0.into(), 100).unwrap().entries.iter().all(|entry| matches!(entry.value, UnitValue::Object { .. })));
    (f, segment, catalog)
}
fn assert_received(store: &PersistentStore, segment: &Value, catalog: &Value) {
    let root = store.read_root(None).unwrap().value;
    assert!(root["additionalPrompt"] == *segment);
    assert!(root["NAIImgUrl"] == *catalog);
}

#[test]
fn large_units_publish_and_receive_through_segments_and_data_catalogs() {
    for repository_id in super::lww_tests::representative_repository_ids() {
        run(async {
            let (mut f, segment, catalog) = large_fixture(&repository_id);
            f.publish_a().await;
            assert!(f.receive_b().await > 0);
            assert_received(&f.b, &segment, &catalog);
        });
    }
}

#[test]
fn compaction_captures_large_unit_bodies_before_segments_retire() {
    for repository_id in super::lww_tests::representative_repository_ids() {
        run(async {
            let (mut f, segment, catalog) = large_fixture(&repository_id);
            f.publish_a().await;
            let job = tempfile::tempdir().unwrap();
            let writer = f.a.lww_clock_state().unwrap().writer_id;
            f.sender.compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-000000000099",
                &writer, &super::fake::capabilities(true), &Cancellation::default(), None)
                .await.unwrap();
            for object in f.sender.listing(&Cancellation::default()).await.unwrap() {
                f.provider.delete_object(&f.sender.repository, &object.locator, &Cancellation::default()).await.unwrap();
            }
            assert!(f.receive_b().await > 0);
            assert_received(&f.b, &segment, &catalog);
        });
    }
}

// Captures the source as a device backup bundle, uploads it, and activates it
// in an empty library through the database-first restore.
fn restore_original_backup(populate: impl FnOnce(&mut PersistentStore)) -> (tempfile::TempDir, PersistentStore) {
    use super::{
        connection_commands::ConnectedRepository, connection_store::StoredConnection, fake,
        job_store::{DurableJob, JobStore}, journal::{JobIdentity, TransferJournal}, packaging,
        phase_progress::PhaseProgress, runtime_restore,
    };
    use std::sync::Arc;
    let source_root = tempfile::tempdir().unwrap();
    let mut source = PersistentStore::open(source_root.path()).unwrap();
    populate(&mut source);
    let probe = super::runtime::CancelProbe(Cancellation::default());
    let hydration = source.hydrate_external_capture_dependencies("sender", &probe).unwrap();
    let (lease, prepared) = source.lww_acquire_backup_capture(source.revision().unwrap()).unwrap();
    let sections = super::sections::capture_prepared_backup_sections(&prepared, &source_root.path().join("backup-sections"), &probe.0).unwrap();
    let capture = source.capture_external_library_from_lease_with_sections("sender", &hydration, &lease.lease, sections, &probe).unwrap();
    let sections = capture.catalog.backup_sections(source_root.path()).unwrap();
    let original_units = capture.catalog.original_backup_units().unwrap();
    let provider = Arc::new(fake::FakeProvider::new(false));
    let repository = fake::repository();
    let connected = ConnectedRepository {
        stored: StoredConnection {
            id: "synthetic-connection".into(),
            config: ConnectionConfig { provider: "synthetic".into(), profile: None, endpoint: "https://synthetic.invalid".into(),
                account_id: "account".into(), location: Default::default(), oauth_profile: None },
            descriptor: risunest_external_storage_format::format::Descriptor::new("format-repository".into(),
                Some(risunest_external_storage_format::format::Strategy::Cas)).unwrap(),
            descriptor_locator: RemoteLocator { connection_identity: repository.connection_identity.clone(), collection: None, object: "descriptor".into() },
            provider_repository_id: repository.repository_id.clone(), credential_ref: "credential".into(), root_key_ref: "key".into(),
            recovery_key_ref: "recovery".into(), retention_policy: None, transfer_concurrency: None, labels: Default::default(), capabilities: fake::capabilities(true),
            created_at_ms: 1, verified_at_ms: 1, last_sync_at_ms: None, last_backup_at_ms: None,
        },
        provider: provider.clone(), handle: repository,
        dependencies: fake::loopback_dependencies(fake::MemoryVault::default(), 1).dependencies,
        root_key: zeroize::Zeroizing::new([21; 32]),
    };
    let backup_id = uuid::Uuid::new_v4().to_string();
    let work = tempfile::tempdir().unwrap();
    let mut journal = TransferJournal::open(&work.path().join("backup-journal"), JobIdentity {
        job_id: backup_id.clone(), connection_id: connected.stored.id.clone(), repository_id: connected.handle.repository_id.clone(),
        capture_id: capture.id.clone(), capture: capture.identity.clone(),
    }).unwrap();
    let metadata = packaging::SnapshotMetadata {
        snapshot_id: backup_id.clone(), repository_id: connected.stored.descriptor.repository_id.clone(),
        library_id: capture.identity.library_epoch.clone(), author_device_id: capture.identity.store_id.clone(),
        created_at_ms: super::runtime::now_ms(), logical_revision: capture.identity.revision as u64, parent_snapshot_id: None,
        content_fingerprint: capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
        purpose: packaging::SnapshotPurpose::BackupBundle {
            source: risunest_external_storage_format::control::BundleSource::Device { writer_id: source.lww_clock_state().unwrap().writer_id },
            remote_generation: None, original_units,
        },
    };
    let destination = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(destination.path()).unwrap();
    run(async {
        let backup = packaging::package_and_upload(capture, sections, source_root.path(), &work.path().join("cache"), metadata,
            &connected.root_key, packaging::PackageLimits::from_capabilities(&connected.stored.capabilities).unwrap(), None, &mut journal,
            connected.provider.as_ref(), &connected.handle, &PhaseProgress::silent(), &Cancellation::default()).await.unwrap();
        let request = serde_json::from_value(json!({"connectionId": "synthetic-connection", "kind": "restore", "snapshotId": backup_id, "targetRevision": "0"})).unwrap();
        let mut job = DurableJob::new(request, 1, store.external_identity().unwrap());
        job.summary["restoreSource"] = serde_json::to_value(backup.reference.stored(&connected.handle).unwrap()).unwrap();
        JobStore::open(destination.path()).unwrap().put(&job).unwrap();
        let (database, sections) = runtime_restore::prepare_database_first_backup(destination.path(), &connected, &job, &Cancellation::default()).await.unwrap();
        let admission = Arc::new(crate::native_file_jobs::admission::Admission::default());
        let state = crate::persistent_store::commands::PersistentStoreState::default();
        runtime_restore::activate_database_first_backup(&mut store, &state, &job, database, sections, Cancellation::default(), admission.staging().unwrap()).unwrap();
        let before=provider.uploaded_ids().into_iter().filter(|id|id.starts_with("pack-")).collect::<Vec<_>>();
        let hydration=store.hydrate_external_capture_dependencies("restored-backup",&probe).unwrap();
        let (lease,prepared)=store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
        let sections=super::sections::capture_prepared_backup_sections(&prepared,&destination.path().join("backup-sections"),&probe.0).unwrap();
        let capture=store.capture_external_library_from_lease_with_sections("restored-backup",&hydration,&lease.lease,sections,&probe).unwrap();
        let sections=capture.catalog.backup_sections(destination.path()).unwrap();
        let units=capture.catalog.original_backup_units().unwrap();
        let meta=packaging::SnapshotMetadata {snapshot_id:"restored-backup".into(),repository_id:connected.stored.descriptor.repository_id.clone(),library_id:capture.identity.library_epoch.clone(),author_device_id:capture.identity.store_id.clone(),
            created_at_ms:super::runtime::now_ms(),logical_revision:capture.identity.revision as u64,parent_snapshot_id:None,
            content_fingerprint:capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
            purpose:packaging::SnapshotPurpose::BackupBundle {source:risunest_external_storage_format::control::BundleSource::Device {writer_id:store.lww_clock_state().unwrap().writer_id},remote_generation:None,original_units:units}};
        let identity=JobIdentity {job_id:"restored-backup".into(),connection_id:"synthetic-connection".into(),repository_id:connected.handle.repository_id.clone(),capture_id:capture.id.clone(),capture:capture.identity.clone()};
        let mut journal=TransferJournal::open(&destination.path().join("second-journal"),identity).unwrap();
        let cache=super::runtime::connection_directory(destination.path(),"synthetic-connection").join("package-cache");
        let clock=fake::FakeLeaseClock::new(super::runtime::now_ms());let cancel=Cancellation::default();
        let context=super::leases::LeaseContext {root:destination.path(),connection_id:"synthetic-connection",writer_id:"backup-writer",descriptor:&connected.stored.descriptor,
            root_key:&connected.root_key,provider:connected.provider.as_ref(),repository:&connected.handle,clock:&clock,protection_supported:true,ledger:None};
        let owner=match super::leases::admit(&context,"restored-backup",LeaseKind::Work,&cancel).await.unwrap() {
            super::leases::Admission::Admitted(owner)=>owner,_=>panic!("backup lease must be admitted"),
        };
        let view=super::cleanup::ConnectedRepositoryView {connected:&connected,writer_id:"collector",policy:super::connection::RetentionPolicy::DEFAULT,
            now_ms:super::runtime::now_ms(),unfinished:vec![],cache_root:&cache,reads:Default::default()};
        let documents=super::cleanup::ConnectedDocuments {connected:&connected,cancel:&cancel,scratch:destination.path()};
        let request=super::cleanup::CleanupRequest {job_id:"cleanup-during-backup",cleanup_supported:true,limits:super::cleanup::CleanupLimits::default(),connection_time:&|_|Ok(true)};
        assert_eq!(super::cleanup::run(&context,&request,&view,&documents,&cancel).await.unwrap().stop_reason,super::cleanup::StopReason::Lease);
        assert!(before.iter().all(|id|provider.holds(id)),"the backup lease protects packs inherited from restore");
        let second=owner.run(&context,&cancel,packaging::package_and_upload_protected(capture,sections,destination.path(),&cache,meta,&connected.root_key,packaging::PackageLimits::from_capabilities(&connected.stored.capabilities).unwrap(),None,&mut journal,
            connected.provider.as_ref(),&connected.handle,&PhaseProgress::silent(),&cancel,Some((&owner,&context)))).await.unwrap();
        let after=provider.uploaded_ids().into_iter().filter(|id|id.starts_with("pack-")).collect::<Vec<_>>();
        assert_eq!(before,after,"the first backup after restore reuses unchanged packs");
        assert!(second.referenced_objects.iter().any(|object|object.role==ObjectRole::Pack));

    });
    (destination, store)
}

#[test]
fn original_backup_restores_a_large_unit_above_the_metadata_bound() {
    let large = json!("x".repeat(risunest_sync_wire::MAX_METADATA_BYTES));
    let (_destination, store) = restore_original_backup(|source| {
        set(source, &["root", "additionalPrompt"], large.clone());
        let key = UnitKey::new(&["root", "additionalPrompt"]).unwrap();
        assert!(source.lww_read_outbox(0.into(), 100).unwrap().entries.iter()
            .any(|entry| entry.key == key && matches!(entry.value, UnitValue::Object { .. })));
    });
    assert!(store.read_root(None).unwrap().value["additionalPrompt"] == large);
}

#[test]
fn original_backup_restores_an_indivisible_message_above_the_metadata_bound() {
    let data = "x".repeat(risunest_sync_wire::MAX_METADATA_BYTES);
    let (_destination, store) = restore_original_backup(|source| {
        source.commit(&WorkingSetCommit {
            expected_revision: source.revision().unwrap(),
            add_character: Some(json!({
                "type": "character", "chaId": "synthetic-large", "name": "Large", "chatPage": 0,
                "chats": [{"id": "synthetic-large-chat", "name": "Large", "message": [{"role": "user", "data": data}]}],
            })),
            ..Default::default()
        }).unwrap();
    });
    let database = store.materialize(None).unwrap();
    assert!(database["characters"][0]["chats"][0]["message"][0]["data"] == data.as_str());
}

#[test]
fn oversized_large_unit_controls_are_spooled_and_scanned() {
    let asset = "a".repeat(64);
    // Up to one part a control body may be raw bytes, so the spool checks the form of larger ones.
    let part = crate::persistent_store::device_store::plugin_values::structure::PART_BYTES as usize;
    let body = risunest_sync_wire::payload_value::encode(&json!([asset, "x".repeat(part)])).unwrap();
    let hash = risunest_sync_wire::hash(&body);
    assert!(published_json_asset_roots(&body).unwrap().object_hashes.contains(&asset));
    let padded = [b"[".as_slice(), &body[1..body.len() - 1], b" ]"].concat();
    assert!(published_json_asset_roots(&padded).is_err());

    let directory = tempfile::tempdir().unwrap();
    let mut spool = super::capture::BackupDependencySpool::new(directory.path()).unwrap();
    spool.push(&hash, &body, BackupBodyRole::Control).unwrap();
    assert!(spool.push(&risunest_sync_wire::hash(&padded), &padded, BackupBodyRole::Control).is_err());
    spool.seal().unwrap();
    assert_eq!(spool.control(&hash).unwrap(), Some(body.clone()));
    let mut visited = Vec::new();
    spool.visit(&mut |hash, bytes, role| { visited.push((hash.to_owned(), bytes.len(), role)); Ok(()) }).unwrap();
    assert_eq!(visited, vec![(hash, body.len(), BackupBodyRole::Control)]);
    use crate::persistent_store::external_capture::published_string_asset_roots;
    let text = format!("{} assets/objects/{}/{}", "x".repeat(8 * 1024 * 1024), &asset[..2], &asset[2..]);
    let body = serde_json::to_vec(&text).unwrap();
    let hash = risunest_sync_wire::hash(&body);
    let mut spool = super::capture::BackupDependencySpool::new(directory.path()).unwrap();
    assert!(spool.push_ranged_control(&hash, &mut std::io::Cursor::new(&body), body.len() as u64).unwrap());
    assert!(spool.push_ranged_control(&hash, &mut std::io::Cursor::new(&body), body.len() as u64).unwrap());
    spool.seal().unwrap();
    let mut source = spool.control_reader(&hash).unwrap().unwrap();
    let roots = published_string_asset_roots(&mut source, &hash, body.len() as u64).unwrap().unwrap();
    assert!(roots.object_hashes.contains(&asset));
    let mut content = super::content_store::ContentStore::open(&directory.path().join("streamed")).unwrap();
    spool.visit_ranged(&mut |digest, input, length, role| {
        assert_eq!(role, BackupBodyRole::Control);
        content.put_reader(digest, input, length)
    }).unwrap();
    assert_eq!(content.read_all(&hash).unwrap(), body);
}
