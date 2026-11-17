use super::*;

fn native_restore_stage(
    store: &mut crate::persistent_store::PersistentStore,
    request_id: &str,
) -> (crate::persistent_store::lww::Header, String) {
    let stage = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage, &serde_json::json!({"username":"restored"})).unwrap();
    let header = crate::persistent_store::lww::Header {
        binding_authority: store.lww_binding_authority().unwrap(),
        request_id: request_id.into(),
    };
    (header, stage)
}

fn state(root: &Path) -> DeviceBackupState {
    let state = DeviceBackupState::initialize(root.join("device-backup"));
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    state
}

#[test]
fn active_device_restore_owns_only_its_exact_native_library_job() {
    for includes_library in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        let coordinator = state(root.path());
        assert!(!coordinator.owns_native_restore_job("synthetic-owner").unwrap());
        let stage = includes_library.then(|| store.replace_begin().unwrap().staging_id);
        coordinator.create_native_portable_session("synthetic-owner", includes_library,
            &["hypa".into()], 0, stage).unwrap();
        assert_eq!(coordinator.owns_native_restore_job("synthetic-owner").unwrap(), includes_library);
        assert!(!coordinator.owns_native_restore_job("synthetic-unrelated").unwrap());
    }
}

#[test]
fn native_recovery_leaves_live_restore_pins_and_settles_unsealed_pins_as_aborted() {
    use crate::asset_repository::job_pins::{CasJobKind, CasJobOwner, CasObjectRole, CasReleaseOutcome, DurableCasJob};
    let root = tempfile::tempdir().unwrap();
    let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let job = "unsealed-restore-job";
    let mut pins = DurableCasJob::begin(root.path(), job, CasJobKind::LocalBackupRestore, CasJobOwner::for_test(), 0).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(root.path()).unwrap();
    let published = pins.prepare_bytes(&cas, b"synthetic body published after activation", CasObjectRole::DirectObject).unwrap().content_hash;
    let coordinator = state(root.path());
    let stage = store.replace_begin().unwrap().staging_id;
    let id = coordinator.create_native_portable_session(job, true, &["hypa".into()], 0, Some(stage)).unwrap();
    let session = coordinator.session(&id).unwrap();

    commands::release_native_restore_pins_for_test(&session, root.path(), CasReleaseOutcome::Committed).unwrap();
    assert!(!pins.is_released());
    let more = pins.prepare_bytes(&cas, b"synthetic body after device recovery", CasObjectRole::DirectObject).unwrap().content_hash;
    drop(pins);

    commands::release_native_restore_pins_for_test(&session, root.path(), CasReleaseOutcome::Committed).unwrap();
    assert_eq!(DurableCasJob::open(root.path(), job).err().map(|error| error.kind()), Some(std::io::ErrorKind::NotFound));
    let library = rusqlite::Connection::open(root.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
    for hash in [published, more] {
        let registered: i64 = library.query_row("SELECT count(*) FROM asset_objects WHERE object_hash=?1", [&hash], |row| row.get(0)).unwrap();
        assert_eq!(registered, 1);
    }
}

#[test]
fn portable_adoption_requires_completed_exact_native_session_before_release() {
    use crate::local_backup::NeverCancelled;
    let root=tempfile::tempdir().unwrap();
    let mut store=crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let stage=store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage,&serde_json::json!({"username":"synthetic restored"})).unwrap();
    let selected=vec!["hypa".to_owned(),"local-plugins".to_owned(),"local-settings".to_owned()];
    let source=capture_prepared_native_sections(&mut store,&selected,&NeverCancelled).unwrap();
    let rollback=capture_prepared_native_sections(&mut store,&selected,&NeverCancelled).unwrap();
    let coordinator=state(root.path());
    let job="coordinated-adoption";
    let id=coordinator.create_native_portable_session(job,true,&selected,0,Some(stage)).unwrap();
    let header=crate::persistent_store::lww::Header {binding_authority:store.lww_binding_authority().unwrap(),request_id:job.into()};
    coordinator.set_library_replacement(&id,&header,std::collections::BTreeMap::new().into_iter().map(Ok)).unwrap();
    journal_prepared_native_sections(&coordinator,&id,Spool::Source,&source).unwrap();
    coordinator.source_ready(&id).unwrap();
    journal_prepared_native_sections(&coordinator,&id,Spool::Rollback,&rollback).unwrap();
    coordinator.prepared(&id).unwrap();
    let revision=resume_journaled_native_restore(&coordinator,&id,&mut store).unwrap();
    let authority=header.binding_authority.0.to_string();
    assert!(coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),&authority).is_err());
    coordinator.recovery_complete(&id).unwrap();
    coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),&authority).unwrap();
    crate::server_sync::lww_tests::save(&mut store,&["root","username"],serde_json::json!("synthetic later ordinary edit"));
    assert!(store.revision().unwrap()>revision);
    coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),&authority).unwrap();
    assert!(coordinator.verify_portable_adoption_complete(&id,"other-job",&revision.to_string(),&authority).is_err());
    assert!(coordinator.verify_portable_adoption_complete(&id,job,&(revision+1).to_string(),&authority).is_err());
    assert!(coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),"999").is_err());
    coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),&authority).unwrap();
    let library=rusqlite::Connection::open(root.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
    library.execute("UPDATE meta SET value=?1 WHERE key='activeGeneration'",[serde_json::to_string("other-generation").unwrap()]).unwrap();
    assert!(coordinator.verify_portable_adoption_complete(&id,job,&revision.to_string(),&authority).is_err());
}

/// Large device values travel as objects through both journal spools. The
/// rollback spool captures what this device already holds before the restore
/// replaces it, and the restore installs the source text byte for byte.
#[test]
fn journaled_restore_carries_large_device_values_in_source_and_rollback_spools() {
    use crate::local_backup::NeverCancelled;
    use crate::persistent_store::device_store::plugin_values::PluginDeviceMutation;
    fn write_large(store: &mut crate::persistent_store::PersistentStore, label: &str) -> (String, String, String) {
        let string = format!("{label}\n{}{}", "가".repeat(25_000), "x".repeat(8 * 1024 * 1024));
        let json = format!("{{ \"label\": \"{label}\", \"n\": 1.0e1,\n \"pad\": \"{}\" }}", "p".repeat(70_000));
        let setting = serde_json::json!({ "label": label, "pad": "q".repeat(70_000) });
        let device = store.device_store_mut().unwrap();
        let opened = device.open_plugin_device_write("owner-large", "string", "value",
            crate::persistent_store::device_store::plugin_values::text_value::encoded_length(&string)).unwrap();
        let session = serde_json::to_value(opened).unwrap()["sessionId"].as_str().unwrap().to_owned();
        let mut offset = 0;
        while offset < string.len() {
            let mut end = (offset + 1024 * 1024).min(string.len());
            while !string.is_char_boundary(end) { end -= 1; }
            device.append_plugin_device_write("owner-large", &session, offset as u64, &string[offset..end]).unwrap();
            offset = end;
        }
        device.write_plugin_device_values("owner-large", &[
            PluginDeviceMutation::Staged { session_id: session },
            PluginDeviceMutation::Set { space: "json".into(), key: "value".into(), value: json.clone() },
        ]).unwrap();
        device.write_setting("risuNestDeviceSettings", &setting).unwrap();
        (string, json, serde_json::to_string(&setting).unwrap())
    }
    fn objects(sections: &[PreparedDeviceSection]) -> std::collections::BTreeSet<Vec<u8>> {
        let mut objects = std::collections::BTreeSet::new();
        for section in sections {
            section.rows().visit_entries(|entry, object| {
                for reference in entry.value.object_references() { objects.insert(object(reference)?); }
                Ok(())
            }).unwrap();
        }
        objects
    }
    let root = tempfile::tempdir().unwrap();
    let source_root = tempfile::tempdir().unwrap();
    let mut source_store = crate::persistent_store::PersistentStore::open(source_root.path()).unwrap();
    let restored = write_large(&mut source_store, "source");
    let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let held = write_large(&mut store, "held");
    let stage = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&stage, &serde_json::json!({"username": "synthetic restored"})).unwrap();
    let selected = vec!["hypa".to_owned(), "local-plugins".to_owned(), "local-settings".to_owned()];
    let source = capture_prepared_native_sections(&mut source_store, &selected, &NeverCancelled).unwrap();
    let rollback = capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let coordinator = state(root.path());
    let job = "large-device-values";
    let id = coordinator.create_native_portable_session(job, true, &selected, 0, Some(stage)).unwrap();
    let header = crate::persistent_store::lww::Header {
        binding_authority: store.lww_binding_authority().unwrap(), request_id: job.into(),
    };
    coordinator.set_library_replacement(&id, &header, std::collections::BTreeMap::new().into_iter().map(Ok)).unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Source, &source).unwrap();
    coordinator.source_ready(&id).unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback).unwrap();
    coordinator.prepared(&id).unwrap();
    let held_bodies = [&held.0, &held.1, &held.2].into_iter()
        .flat_map(|text| text.as_bytes().chunks(8 * 1024 * 1024).map(<[u8]>::to_vec)).collect::<std::collections::BTreeSet<_>>();
    let journaled = archive::prepare_journaled_native_sections(&coordinator, &id, Spool::Rollback, &selected).unwrap();
    assert_eq!(objects(&journaled), held_bodies);
    resume_journaled_native_restore(&coordinator, &id, &mut store).unwrap();
    let device = store.device_store().unwrap();
    assert_eq!(device.read_plugin_device_value("owner-large", "string", "value").unwrap(), Some(restored.0));
    assert_eq!(device.read_plugin_device_value("owner-large", "json", "value").unwrap(), Some(restored.1));
    let setting: String = device.connection().query_row(
        "SELECT value FROM device_settings WHERE key='risuNestDeviceSettings'", [], |row| row.get(0),
    ).unwrap();
    assert_eq!(setting, restored.2);
}

#[test]
fn large_device_row_reads_are_exact_bounded_and_validate_offsets() {
    let root = tempfile::tempdir().unwrap();
    let coordinator = state(root.path());
    let id = coordinator.create_native_portable_session(
        "large-row", false, &["local-plugins".to_owned()], 0, None).unwrap();
    coordinator.section_begin(&id, Spool::Source, "local-plugins", r#"{"present":true}"#).unwrap();
    let payload = serde_json::to_vec(&"x".repeat(8 * 1024 * 1024)).unwrap();
    coordinator.blob_begin(&id, Spool::Source, "large-row").unwrap();
    for (index, chunk) in payload.chunks(MAX_CHUNK_BYTES).enumerate() {
        coordinator.blob_append(&id, Spool::Source, "large-row",
            (index * MAX_CHUNK_BYTES) as u64, chunk).unwrap();
    }
    let blob = coordinator.blob_finish(&id, Spool::Source, "large-row").unwrap();
    coordinator.row_append_from_blob(&id, Spool::Source, "local-plugins", 0, &blob.sha256).unwrap();
    let mut recovered = Vec::new();
    while recovered.len() < payload.len() {
        let chunk = coordinator.row_read_bytes(&id, Spool::Source, "local-plugins", 0,
            recovered.len() as u64, MAX_CHUNK_BYTES - 3).unwrap();
        assert!(!chunk.is_empty());
        recovered.extend(chunk);
    }
    assert_eq!(recovered, payload);
    assert!(coordinator.row_read_bytes(&id, Spool::Source, "local-plugins", 0,
        payload.len() as u64, MAX_CHUNK_BYTES).unwrap().is_empty());
    assert!(coordinator.row_read_bytes(&id, Spool::Source, "local-plugins", 0,
        payload.len() as u64 + 1, 1).is_err());
    assert!(coordinator.row_read_bytes(&id, Spool::Source, "local-plugins", 0,
        0, MAX_CHUNK_BYTES + 1).is_err());
}

#[test]
fn native_catalog_validation_rejects_unknown_noncanonical_and_corrupt_entries() {
    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    struct Cancelled;
    impl crate::local_backup::CancellationProbe for Cancelled {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    let root = tempfile::tempdir().unwrap();
    let jobs = root.path().join("jobs");
    std::fs::create_dir(&jobs).unwrap();
    let mut store = crate::persistent_store::PersistentStore::open(&root.path().join("source"))
        .unwrap();
    store
        .device_store_mut()
        .unwrap()
        .write_setting("dosync", &serde_json::json!(true))
        .unwrap();
    let catalog = crate::portable_backup::Catalog::create(&jobs, "native-validation", 0).unwrap();
    let selected = vec!["local-settings".to_owned()];
    capture_native_sections(&mut store, &selected, &catalog, &Never).unwrap();
    validate_archive_catalog(&catalog.db, &Never).unwrap();
    assert_eq!(
        validate_archive_catalog(&catalog.db, &Cancelled)
            .unwrap_err()
            .code,
        "device-cancelled"
    );

    let original: String = catalog
        .db
        .query_row(
            "SELECT metadata FROM device_records WHERE section='local-settings' AND ordinal=0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    catalog
        .db
        .execute(
            "UPDATE device_records SET metadata=?1 WHERE section='local-settings' AND ordinal=0",
            [format!("{original} ")],
        )
        .unwrap();
    assert_eq!(
        validate_archive_catalog(&catalog.db, &Never)
            .unwrap_err()
            .code,
        "device-metadata-invalid"
    );
    catalog
        .db
        .execute(
            "UPDATE device_records SET metadata=?1 WHERE section='local-settings' AND ordinal=0",
            [&original],
        )
        .unwrap();
    catalog
        .db
        .execute(
            "UPDATE device_records SET metadata='not-json' WHERE section='local-settings' AND ordinal=0",
            [],
        )
        .unwrap();
    assert_eq!(
        validate_archive_catalog(&catalog.db, &Never)
            .unwrap_err()
            .code,
        "device-metadata-invalid"
    );
    catalog
        .db
        .execute(
            "UPDATE device_records SET metadata=?1 WHERE section='local-settings' AND ordinal=0",
            [&original],
        )
        .unwrap();
    catalog
        .db
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             UPDATE device_records SET section='unknown-native' WHERE section='local-settings';
             UPDATE device_sections SET section='unknown-native' WHERE section='local-settings';
             PRAGMA foreign_keys=ON;",
        )
        .unwrap();
    assert_eq!(
        validate_archive_catalog(&catalog.db, &Never)
            .unwrap_err()
            .code,
        "device-invalid-state"
    );
}

#[test]
fn native_session_constraints_and_rolled_back_ack_remain_strict() {
    let root = tempfile::tempdir().unwrap();
    let coordinator = state(root.path());
    let selected = vec!["local-settings".to_owned()];
    let stage = format!("staging-{}", uuid::Uuid::new_v4());

    assert!(coordinator
        .create_native_portable_session("missing-stage", true, &selected, 0, None)
        .is_err());
    assert!(coordinator
        .create_native_portable_session(
            "unexpected-stage",
            false,
            &selected,
            0,
            Some(stage.clone()),
        )
        .is_err());
    assert!(coordinator
        .create_native_portable_session(
            "unknown-section",
            false,
            &["unknown-native".to_owned()],
            0,
            None,
        )
        .is_err());
    assert!(coordinator
        .create_native_portable_session(
            "invalid-stage",
            true,
            &selected,
            0,
            Some("staging-not-a-uuid".to_owned()),
        )
        .is_err());
    assert!(coordinator
        .create_native_portable_session("negative-revision", false, &selected, -1, None)
        .is_err());

    let id = coordinator
        .create_native_portable_session("rolled-back-job", true, &selected, 0, Some(stage))
        .unwrap();
    coordinator.fail(&id, "synthetic-preparation-failure").unwrap();
    commands::complete_native_recovery_for_test(
        &coordinator,
        &id,
        |session, repository_root, outcome| {
            assert_eq!(session.phase, "rolled-back");
            assert_eq!(repository_root, root.path());
            assert_eq!(
                outcome,
                crate::asset_repository::job_pins::CasReleaseOutcome::Aborted
            );
            Ok(())
        },
    )
    .unwrap();
    assert!(!coordinator.is_blocking().unwrap());
}

#[test]
fn native_section_archive_roundtrip_keeps_structured_keys_objects_and_local_scope() {
    use crate::persistent_store::device_store::{
        hypa::HypaEmbeddingWrite, plugin_values::PluginDeviceMutation,
    };

    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    let root = tempfile::tempdir().unwrap();
    let source_root = root.path().join("source");
    let target_root = root.path().join("target");
    let jobs = root.path().join("jobs");
    std::fs::create_dir(&jobs).unwrap();
    let mut source = crate::persistent_store::PersistentStore::open(&source_root).unwrap();
    let large_vector = vec![7u8; 8 * 1024];
    let large_string = format!("{}\n{}", "가".repeat(25_000), "z".repeat(100));
    let large_json = format!("{{ \"items\": [1.50, 2e1],\n  \"text\": \"{}\" }}", "j".repeat(70_000));
    let large_setting = serde_json::json!({ "notes": "s".repeat(70_000) });
    let part = vec![0u8, 255, 128];
    let part_hash = risunest_sync_wire::hash(&part);
    let structured = format!(
        "#risunest-structured:1\n{{\"parts\":[\"p:{part_hash}\"],\"bytes\":3,\"nodes\":[{{\"type\":\"ArrayBuffer\",\"offset\":0,\"byteLength\":3}}],\"root\":{{\"ref\":0}}}}");
    {
        let device = source.device_store_mut().unwrap();
        crate::persistent_store::device_store::plugin_values::structure::put_part(device.connection(), &part).unwrap();
        device
            .write_hypa_embeddings(&[HypaEmbeddingWrite {
                cache_key: "ab".repeat(32),
                producer: "synthetic".into(),
                model: "portable-roundtrip".into(),
                endpoint: None,
                preprocess_version: 1,
                dimensions: (large_vector.len() / 4) as i64,
                vector: large_vector.clone(),
                metadata: None,
            }])
            .unwrap();
        for (owner, space, value) in [
            ("owner-a", "string", "first"),
            ("owner-a", "json", r#"{"value":2}"#),
            ("owner-b", "string", "third"),
        ] {
            device
                .write_plugin_device_values(
                    owner,
                    &[PluginDeviceMutation::Set {
                        space: space.into(),
                        key: "shared".into(),
                        value: value.into(),
                    }],
                )
                .unwrap();
        }
        device
            .write_plugin_device_values(
                "owner-b",
                &[
                    PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "large".into(),
                        value: large_string.clone(),
                    },
                    PluginDeviceMutation::Set {
                        space: "json".into(),
                        key: "large".into(),
                        value: large_json.clone(),
                    },
                    PluginDeviceMutation::Set {
                        space: "json".into(),
                        key: "structured".into(),
                        value: structured.clone(),
                    },
                ],
            )
            .unwrap();
        device
            .write_setting("risuNestDeviceSettings", &large_setting)
            .unwrap();
        device
            .write_plugin_device_values(
                "owner-a",
                &[
                    PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "removed".into(),
                        value: "not exported".into(),
                    },
                    PluginDeviceMutation::Delete {
                        space: "string".into(),
                        key: "removed".into(),
                    },
                ],
            )
            .unwrap();
        device
            .write_setting("dosync", &serde_json::json!(true))
            .unwrap();
        device
            .write_setting("risu_lastsaved", &serde_json::json!("source-control"))
            .unwrap();
        device
            .write_plugin_permission("plugin-hash", "network", true)
            .unwrap();
    }

    let catalog = crate::portable_backup::Catalog::create(&jobs, "synthetic-native", 0).unwrap();
    catalog
        .db
        .execute(
            "UPDATE backup_info SET value='false' WHERE key='libraryIncluded'",
            [],
        )
        .unwrap();
    let selected = vec![
        "hypa".to_owned(),
        "local-plugins".to_owned(),
        "local-settings".to_owned(),
    ];
    source.device_store().unwrap()
        .write_setting("risuNestDefaultPluginsAttempted", &serde_json::json!(r#"["source-plugin"]"#))
        .unwrap();
    capture_native_sections(&mut source, &selected, &catalog, &Never).unwrap();
    assert_eq!(
        catalog
            .db
            .query_row::<i64, _, _>("SELECT count(*) FROM objects", [], |row| row.get(0))
            .unwrap(),
        5
    );
    assert_eq!(
        catalog
            .db
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM device_records WHERE section='local-plugins' AND ordinal>=0",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        6
    );
    let path = root.path().join("native.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        &jobs,
        &Never,
    )
    .unwrap();
    let prepared = prepare_native_sections(&archive, &selected, root.path(), &Never).unwrap();

    let mut target = crate::persistent_store::PersistentStore::open(&target_root).unwrap();
    {
        let device = target.device_store_mut().unwrap();
        device
            .write_plugin_device_values(
                "stale-owner",
                &[PluginDeviceMutation::Set {
                    space: "string".into(),
                    key: "stale".into(),
                    value: "remove me".into(),
                }],
            )
            .unwrap();
        device
            .write_setting("dosync", &serde_json::json!(false))
            .unwrap();
        device
            .write_setting("risu_lastsaved", &serde_json::json!("target-control"))
            .unwrap();
        device
            .write_setting("risuNestDefaultPluginsAttempted", &serde_json::json!(r#"["provider-manager","yumi-translator"]"#))
            .unwrap();
    }
    let rows = prepared.iter().map(PreparedDeviceSection::rows).collect::<Vec<_>>();
    let (header, stage) = native_restore_stage(&mut target, "synthetic-native-roundtrip");
    target.lww_commit_replacement_with_device_sections(&header, &stage, None, &rows).unwrap();
    let revision = target.device_store().unwrap().revision().unwrap();
    target.lww_commit_replacement_with_device_sections(&header, &stage, None, &rows).unwrap();
    let device = target.device_store().unwrap();
    assert_eq!(device.revision().unwrap(), revision);
    assert_eq!(
        device.read_hypa_embeddings(&["ab".repeat(32)]).unwrap()[0]
            .vector
            .as_deref(),
        Some(large_vector.as_slice())
    );
    for (owner, space, expected) in [
        ("owner-a", "string", "first"),
        ("owner-a", "json", r#"{"value":2}"#),
        ("owner-b", "string", "third"),
    ] {
        assert_eq!(
            device
                .read_plugin_device_value(owner, space, "shared")
                .unwrap()
                .as_deref(),
            Some(expected)
        );
    }
    for (space, expected) in [("string", &large_string), ("json", &large_json)] {
        assert_eq!(
            device
                .read_plugin_device_value("owner-b", space, "large")
                .unwrap()
                .as_ref(),
            Some(expected)
        );
    }
    assert_eq!(
        device.read_plugin_device_value("owner-b", "json", "structured").unwrap(),
        Some(structured)
    );
    assert_eq!(
        device.connection().query_row::<Vec<u8>, _, _>(
            "SELECT body FROM message_page_objects WHERE hash=?1", [&part_hash], |row| row.get(0)).unwrap(),
        part
    );
    assert_eq!(
        device.read_setting("risuNestDeviceSettings").unwrap(),
        Some(large_setting.clone())
    );
    assert_eq!(
        device
            .read_plugin_device_value("owner-a", "string", "removed")
            .unwrap(),
        None
    );
    assert_eq!(
        device
            .read_plugin_device_value("stale-owner", "string", "stale")
            .unwrap(),
        None
    );
    assert_eq!(
        device.read_setting("dosync").unwrap(),
        Some(serde_json::json!(true))
    );
    assert_eq!(
        device.read_setting("risu_lastsaved").unwrap(),
        Some(serde_json::json!("target-control"))
    );
    let permissions = device.read_plugin_permissions().unwrap();
    assert_eq!(permissions.len(), 1);
    assert_eq!(permissions[0].code_hash, "plugin-hash");
    assert_eq!(permissions[0].permission, "network");
    assert!(permissions[0].granted);
    assert_eq!(
        device.read_setting("risuNestDefaultPluginsAttempted").unwrap(),
        Some(serde_json::json!(r#"["provider-manager","yumi-translator"]"#))
    );
}

#[test]
fn selected_empty_native_sections_clear_device_data_and_keep_coordination_settings() {
    use crate::persistent_store::device_store::plugin_values::PluginDeviceMutation;

    struct Never;
    impl crate::local_backup::CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    let root = tempfile::tempdir().unwrap();
    let jobs = root.path().join("jobs");
    std::fs::create_dir(&jobs).unwrap();
    let mut source =
        crate::persistent_store::PersistentStore::open(&root.path().join("source")).unwrap();
    let catalog = crate::portable_backup::Catalog::create(&jobs, "synthetic-empty", 0).unwrap();
    catalog
        .db
        .execute(
            "UPDATE backup_info SET value='false' WHERE key='libraryIncluded'",
            [],
        )
        .unwrap();
    let selected = vec![
        "hypa".to_owned(),
        "local-plugins".to_owned(),
        "local-settings".to_owned(),
    ];
    capture_native_sections(&mut source, &selected, &catalog, &Never).unwrap();
    assert_eq!(
        catalog
            .db
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM device_sections WHERE present=1 AND record_count=0",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        3
    );
    let path = root.path().join("empty.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        &jobs,
        &Never,
    )
    .unwrap();
    let prepared = prepare_native_sections(&archive, &selected, root.path(), &Never).unwrap();
    let mut target =
        crate::persistent_store::PersistentStore::open(&root.path().join("target")).unwrap();
    {
        let device = target.device_store_mut().unwrap();
        device
            .write_plugin_device_values(
                "owner",
                &[PluginDeviceMutation::Set {
                    space: "string".into(),
                    key: "remove".into(),
                    value: "value".into(),
                }],
            )
            .unwrap();
        device
            .write_setting("dosync", &serde_json::json!(true))
            .unwrap();
        device
            .write_setting("risu_lastsaved", &serde_json::json!("target-control"))
            .unwrap();
        device
            .write_plugin_permission("plugin-hash", "network", true)
            .unwrap();
        device
            .write_setting("risuNestDefaultPluginsAttempted", &serde_json::json!(r#"["provider-manager","yumi-translator"]"#))
            .unwrap();
    }
    let rows = prepared.iter().map(PreparedDeviceSection::rows).collect::<Vec<_>>();
    let (header, stage) = native_restore_stage(&mut target, "synthetic-empty-restore");
    target.lww_commit_replacement_with_device_sections(&header, &stage, None, &rows).unwrap();
    let device = target.device_store().unwrap();
    assert_eq!(
        device
            .read_plugin_device_value("owner", "string", "remove")
            .unwrap(),
        None
    );
    assert_eq!(device.read_setting("dosync").unwrap(), None);
    assert_eq!(
        device.read_setting("risu_lastsaved").unwrap(),
        Some(serde_json::json!("target-control"))
    );
    assert!(device.read_plugin_permissions().unwrap().is_empty());
    assert_eq!(
        device.read_setting("risuNestDefaultPluginsAttempted").unwrap(),
        Some(serde_json::json!(r#"["provider-manager","yumi-translator"]"#))
    );
}

#[test]
fn native_journal_resumes_before_intent_and_around_an_inflight_section_commit() {
    use crate::local_backup::NeverCancelled;
    use crate::persistent_store::device_store::plugin_values::PluginDeviceMutation;

    for interruption in ["before-intent", "after-intent", "after-commit"] {
        let root = tempfile::tempdir().unwrap();
        let selected = vec!["hypa".to_owned(), "local-plugins".to_owned(), "local-settings".to_owned()];
        let mut source = crate::persistent_store::PersistentStore::open(
            &root.path().join("synthetic-source"),
        )
        .unwrap();
        source
            .device_store_mut()
            .unwrap()
            .write_plugin_device_values(
                "owner-a",
                &[PluginDeviceMutation::Set {
                    space: "string".into(),
                    key: "key".into(),
                    value: "restored".into(),
                }],
            )
            .unwrap();
        source.device_store_mut().unwrap().write_setting("dosync", &serde_json::json!(true)).unwrap();
        let source_stage = source.replace_begin().unwrap().staging_id;
        source.replace_put_root(&source_stage, &serde_json::json!({"username":"restored"})).unwrap();
        source.replace_commit(&source_stage, Some(0)).unwrap();
        let lease = source.lww_acquire_library_backup_capture(source.revision().unwrap()).unwrap().lease;
        let units = source.lww_backup_unit_values(&lease).unwrap();
        source.release_revision(&lease).unwrap();
        let incoming = capture_prepared_native_sections(&mut source, &selected, &NeverCancelled)
            .unwrap();

        let mut target = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        {
            let device = target.device_store_mut().unwrap();
            device
                .write_plugin_device_values(
                    "owner-a",
                    &[PluginDeviceMutation::Set {
                        space: "string".into(),
                        key: "key".into(),
                        value: "old".into(),
                    }],
                )
                .unwrap();
            device.write_setting("dosync", &serde_json::json!(true)).unwrap();
        }
        let rollback =
            capture_prepared_native_sections(&mut target, &selected, &NeverCancelled).unwrap();
        let stage = target.replace_begin().unwrap().staging_id;
        target.replace_put_root(&stage, &serde_json::json!({"username":"restored"})).unwrap();
        let header = crate::persistent_store::lww::Header {
            binding_authority: target.lww_binding_authority().unwrap(),
            request_id: "synthetic-job".into(),
        };
        let coordinator = state(root.path());
        let id = coordinator
            .create_native_portable_session("synthetic-job", true, &selected, 0, Some(stage.clone()))
            .unwrap();
        coordinator.set_library_replacement(&id, &header, units.clone().into_iter().map(Ok)).unwrap();
        let source_manifests = journal_prepared_native_sections(
            &coordinator,
            &id,
            Spool::Source,
            &incoming,
        )
        .unwrap();
        coordinator.source_ready(&id).unwrap();
        journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback).unwrap();
        coordinator.prepared(&id).unwrap();
        if interruption != "before-intent" {
            coordinator
                .section_intent(&id, "local-plugins")
                .unwrap();
        }
        let revision_after_first_apply = if interruption == "after-commit" {
            let rows = incoming.iter().map(PreparedDeviceSection::rows).collect::<Vec<_>>();
            assert_eq!(target.lww_commit_replacement_with_device_sections(&header, &stage, Some(&units), &rows).unwrap().revision, 1);
            Some(target.device_store().unwrap().revision().unwrap())
        } else {
            None
        };
        drop(target);
        drop(coordinator);

        let recovered = state(root.path());
        assert_eq!(
            recovered
                .bootstrap_for_entry()
                .unwrap()
                .session
                .unwrap()
                .phase,
            if interruption == "after-commit" {"committed"} else {"applying-device"}
        );
        if interruption == "before-intent" {
            assert_eq!(
                recovered.pending_source_sections(&id).unwrap(),
                selected
            );
        }
        let mut target = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        resume_journaled_native_restore(&recovered, &id, &mut target).unwrap();
        let session = recovered.session(&id).unwrap();
        assert_eq!(session.phase, "committed");
        assert_eq!(session.action, "native-complete");
        assert!(recovered.pending_source_sections(&id).unwrap().is_empty());
        assert_eq!(source_manifests.len(), 3);
        assert_eq!(target.revision().unwrap(), 1);
        assert_eq!(target.read_root(None).unwrap().value["username"], "restored");
        let device = target.device_store().unwrap();
        assert_eq!(
            device
                .read_plugin_device_value("owner-a", "string", "key")
                .unwrap()
                .as_deref(),
            Some("restored")
        );
        assert_eq!(
            device.read_setting("dosync").unwrap(),
            Some(serde_json::json!(true))
        );
        if let Some(revision) = revision_after_first_apply {
            assert_eq!(device.revision().unwrap(), revision);
        }
    }
}

#[test]
fn native_library_stage_survives_reopen_and_marker_prevents_second_activation() {
    use crate::local_backup::NeverCancelled;

    let root = tempfile::tempdir().unwrap();
    let selected = vec!["hypa".to_owned(), "local-plugins".to_owned(), "local-settings".to_owned()];
    let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let unrelated = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&unrelated, &serde_json::json!({ "username": "unrelated" }))
        .unwrap();
    let stage = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&stage, &serde_json::json!({ "username": "restored" }))
        .unwrap();
    let mut original = crate::persistent_store::PersistentStore::open(&root.path().join("synthetic-source")).unwrap();
    let original_stage = original.replace_begin().unwrap().staging_id;
    original.replace_put_root(&original_stage, &serde_json::json!({"username":"restored"})).unwrap();
    original.replace_commit(&original_stage, Some(0)).unwrap();
    let lease = original.lww_acquire_library_backup_capture(original.revision().unwrap()).unwrap().lease;
    let mut units = original.lww_backup_unit_values(&lease).unwrap();
    original.release_revision(&lease).unwrap();
    units.insert(
        risunest_sync_wire::unit::UnitKey::new(&["future-unit", "synthetic"]).unwrap(),
        risunest_sync_wire::unit::UnitValue::inline(br#""synthetic opaque unit""#).unwrap(),
    );
    let source = capture_prepared_native_sections(&mut original, &selected, &NeverCancelled).unwrap();
    let rollback =
        capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let mut pins = crate::asset_repository::job_pins::DurableCasJob::begin(
        root.path(),
        "library-recovery-job",
        crate::asset_repository::job_pins::CasJobKind::LocalBackupRestore,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        0,
    )
    .unwrap();
    pins.seal(&mut store, 0).unwrap();
    drop(pins);
    let coordinator = state(root.path());
    let id = coordinator
        .create_native_portable_session(
            "library-recovery-job",
            true,
            &selected,
            0,
            Some(stage.clone()),
        )
        .unwrap();
    let header = crate::persistent_store::lww::Header {
        binding_authority: store.lww_binding_authority().unwrap(),
        request_id: "library-recovery-job".into(),
    };
    coordinator.set_library_replacement(&id, &header, units.clone().into_iter().map(Ok)).unwrap();
    journal_prepared_native_sections(
        &coordinator,
        &id,
        Spool::Source,
        &source,
    )
    .unwrap();
    coordinator.source_ready(&id).unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback).unwrap();
    coordinator.prepared(&id).unwrap();
    drop(store);

    let mut reopened = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    assert!(reopened
        .prepare_replace_commit(&unrelated, Some(0))
        .is_err());
    for section in &selected {coordinator.section_intent(&id, section).unwrap();}
    let rows = source.iter().map(PreparedDeviceSection::rows).collect::<Vec<_>>();
    let committed = reopened.lww_commit_replacement_with_device_sections(&header, &stage, Some(&units), &rows).unwrap();
    assert_eq!(committed.revision, 1);
    drop(reopened);
    drop(coordinator);

    let recovered = state(root.path());
    let decision = recovered.bootstrap_for_entry().unwrap();
    let session = decision.session.unwrap();
    assert_eq!(session.phase, "committed");
    assert_eq!(session.action, "native-complete");
    let mut reopened = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    assert_eq!(
        resume_journaled_native_restore(&recovered, &id, &mut reopened).unwrap(),
        1
    );
    assert_eq!(reopened.replacement_source_rows(&stage).unwrap(), 0);
    assert_eq!(reopened.revision().unwrap(), 1);
    assert_eq!(
        reopened.read_root(None).unwrap().value["username"],
        "restored"
    );
    assert!(recovered.is_blocking().unwrap());
    assert!(!recovered.section_list(&id, Spool::Source).unwrap().is_empty());
    let library_revision = reopened.revision().unwrap();
    let device_revision = reopened.device_store().unwrap().revision().unwrap();

    let failure = commands::complete_native_recovery_for_test(
        &recovered,
        &id,
        |session, repository_root, outcome| {
            assert_eq!(session.job_id, "library-recovery-job");
            assert_eq!(outcome, crate::asset_repository::job_pins::CasReleaseOutcome::Committed);
            let mut pins = crate::asset_repository::job_pins::DurableCasJob::open(
                repository_root,
                &session.job_id,
            )
            .unwrap();
            pins.leave_release_record_for_cleanup_retry(
                crate::asset_repository::job_pins::CasReleaseOutcome::Committed,
            )
            .unwrap();
            Err(error(
                "device-storage-failed",
                "synthetic durable pin cleanup failure",
            ))
        },
    )
    .unwrap_err();
    assert_eq!(failure.code, "device-storage-failed");
    assert!(recovered.is_blocking().unwrap());
    assert_eq!(recovered.session(&id).unwrap().phase, "committed");
    assert!(!recovered.section_list(&id, Spool::Source).unwrap().is_empty());
    let released = crate::asset_repository::job_pins::DurableCasJob::open(
        root.path(),
        "library-recovery-job",
    )
    .unwrap();
    assert!(released.is_released());
    drop(released);
    assert_eq!(reopened.revision().unwrap(), library_revision);
    assert_eq!(
        reopened.device_store().unwrap().revision().unwrap(),
        device_revision
    );

    commands::complete_native_recovery_for_test(
        &recovered,
        &id,
        |session, repository_root, outcome| {
            assert_eq!(outcome, crate::asset_repository::job_pins::CasReleaseOutcome::Committed);
            let mut pins = crate::asset_repository::job_pins::DurableCasJob::open(
                repository_root,
                &session.job_id,
            )
            .map_err(|_| {
                error(
                    "device-storage-failed",
                    "Native portable recovery could not reopen durable asset pins",
                )
            })?;
            pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Committed)
                .map_err(|_| {
                    error(
                        "device-storage-failed",
                        "Native portable recovery could not retry durable asset pin cleanup",
                    )
                })
        },
    )
    .unwrap();
    assert!(!recovered.is_blocking().unwrap());
    assert_eq!(reopened.revision().unwrap(), library_revision);
    assert_eq!(
        reopened.device_store().unwrap().revision().unwrap(),
        device_revision
    );
    let missing = match crate::asset_repository::job_pins::DurableCasJob::open(
        root.path(),
        "library-recovery-job",
    ) {
        Ok(_) => panic!("released durable pin journal still exists"),
        Err(error) => error,
    };
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn missing_journal_owned_stage_blocks_open_without_discarding_source_spool() {
    use crate::local_backup::NeverCancelled;

    let root = tempfile::tempdir().unwrap();
    let selected = vec!["local-settings".to_owned()];
    let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let stage = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&stage, &serde_json::json!({ "username": "restored" }))
        .unwrap();
    let source = capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let rollback =
        capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let coordinator = state(root.path());
    let id = coordinator
        .create_native_portable_session(
            "missing-stage-job",
            true,
            &selected,
            0,
            Some(stage.clone()),
        )
        .unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Source, &source).unwrap();
    coordinator.source_ready(&id).unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback).unwrap();
    coordinator.prepared(&id).unwrap();
    store.replace_abort(&stage).unwrap();
    drop(store);

    assert!(crate::persistent_store::PersistentStore::open(root.path()).is_err());
    assert!(coordinator.is_blocking().unwrap());
    assert!(!coordinator.section_list(&id, Spool::Source).unwrap().is_empty());
}

#[test]
fn cleanup_closes_the_device_database_and_reopens_only_a_fresh_store() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("device-backup");
    let state = DeviceBackupState::initialize(directory.clone());
    state.close_for_cleanup().unwrap();
    assert!(state.lock().is_err());
    std::fs::remove_dir_all(&directory).unwrap();
    state.reopen_after_cleanup().unwrap();
    let inner = state.lock().unwrap();
    assert!(inner.connection.is_some());
    assert!(inner.cold_session.is_none());
    assert!(!inner.reconciled);
}

#[test]
fn device_backup_commands_log_their_failures() {
    let root = tempfile::tempdir().unwrap();
    let app = crate::test_app::mock_app();
    tauri::Manager::manage(&*app, state(root.path()));
    let missing = native_device_backup_recovery_complete(app.handle().clone(), tauri::Manager::state(&*app), "missing-session".into());
    assert_eq!(missing.unwrap_err().code, "device-session-missing");
    let entry = crate::native_log::global_state()
        .tail(None)
        .into_iter()
        .rev()
        .find(|entry| entry.message.starts_with("native_device_backup_recovery_complete failed: code=device-session-missing cause="))
        .expect("the command logs its failure");
    assert_eq!((entry.level.as_str(), entry.target.as_str()), ("error", "native-command"));
    assert!(entry.message.contains("commands.rs:"), "{}", entry.message);
}

#[test]
fn a_kept_device_failure_cause_stays_out_of_the_reply() {
    use crate::native_log::CommandFailure;
    let failure = DeviceBackupError::from(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "synthetic refusal"));
    assert_eq!(
        serde_json::to_value(&failure).unwrap(),
        serde_json::json!({"code": "device-storage-failed", "message": "Device maintenance filesystem operation failed"}),
    );
    assert_eq!(
        failure.detail().as_deref(),
        Some("Device maintenance filesystem operation failed: PermissionDenied: synthetic refusal"),
    );
    let failure = DeviceBackupError::from(rusqlite::Error::QueryReturnedNoRows);
    assert_eq!(failure.code().as_ref(), "device-storage-failed");
    assert_eq!(
        failure.detail().as_deref(),
        Some("Device maintenance SQLite operation failed: Query returned no rows"),
    );
}
