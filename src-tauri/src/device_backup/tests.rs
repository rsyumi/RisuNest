use super::*;

fn state(root: &Path) -> DeviceBackupState {
    let state = DeviceBackupState::initialize(root.join("device-backup"));
    let pds = crate::persistent_store::commands::PersistentStoreState::default();
    state
        .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
        .unwrap();
    state
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
    {
        let device = source.device_store_mut().unwrap();
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
    capture_native_sections(&mut source, &selected, &catalog, &Never).unwrap();
    assert_eq!(
        catalog
            .db
            .query_row::<i64, _, _>("SELECT count(*) FROM objects", [], |row| row.get(0))
            .unwrap(),
        1
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
        3
    );
    let path = root.path().join("native.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        &jobs,
        &Never,
    )
    .unwrap();
    let prepared = prepare_native_sections(&archive, &selected, &Never).unwrap();

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
    }
    apply_prepared_native_sections(&mut target, &prepared).unwrap();
    let revision = target.device_store().unwrap().revision().unwrap();
    apply_prepared_native_sections(&mut target, &prepared).unwrap();
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
}

#[test]
fn selected_empty_native_section_clears_only_that_section() {
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
    let selected = vec!["local-plugins".to_owned()];
    capture_native_sections(&mut source, &selected, &catalog, &Never).unwrap();
    assert_eq!(
        catalog
            .db
            .query_row::<(i64, i64), _, _>(
                "SELECT present,record_count FROM device_sections WHERE section='local-plugins'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap(),
        (1, 0)
    );
    let path = root.path().join("empty.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive = crate::portable_backup::VerifiedArchive::open(
        std::fs::File::open(path).unwrap(),
        &jobs,
        &Never,
    )
    .unwrap();
    let prepared = prepare_native_sections(&archive, &selected, &Never).unwrap();
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
    }
    apply_prepared_native_sections(&mut target, &prepared).unwrap();
    let device = target.device_store().unwrap();
    assert_eq!(
        device
            .read_plugin_device_value("owner", "string", "remove")
            .unwrap(),
        None
    );
    assert_eq!(
        device.read_setting("dosync").unwrap(),
        Some(serde_json::json!(true))
    );
}

#[test]
fn native_journal_resumes_before_intent_and_around_an_inflight_section_commit() {
    use crate::local_backup::NeverCancelled;
    use crate::persistent_store::device_store::plugin_values::PluginDeviceMutation;

    for interruption in ["before-intent", "after-intent", "after-commit"] {
        let root = tempfile::tempdir().unwrap();
        let selected = vec!["local-plugins".to_owned()];
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
        let coordinator = state(root.path());
        let id = coordinator
            .create_native_portable_session("synthetic-job", false, &selected, 0, None)
            .unwrap();
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
            apply_prepared_native_sections(&mut target, &incoming).unwrap();
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
            "applying-device"
        );
        if interruption == "before-intent" {
            assert_eq!(
                recovered.pending_source_sections(&id).unwrap(),
                vec!["local-plugins".to_owned()]
            );
        }
        let mut target = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        resume_journaled_native_restore(&recovered, &id, &mut target).unwrap();
        let session = recovered.session(&id).unwrap();
        assert_eq!(session.phase, "committed");
        assert_eq!(session.action, "native-complete");
        assert!(recovered.pending_source_sections(&id).unwrap().is_empty());
        assert_eq!(source_manifests.len(), 1);
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
    let selected = vec!["local-settings".to_owned()];
    let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    let unrelated = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&unrelated, &serde_json::json!({ "username": "unrelated" }))
        .unwrap();
    let stage = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&stage, &serde_json::json!({ "username": "restored" }))
        .unwrap();
    let source = capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let rollback =
        capture_prepared_native_sections(&mut store, &selected, &NeverCancelled).unwrap();
    let mut pins = crate::asset_repository::job_pins::DurableCasJob::begin(
        root.path(),
        "library-recovery-job",
        crate::asset_repository::job_pins::CasJobKind::LocalBackupRestore,
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
    let source_manifest = journal_prepared_native_sections(
        &coordinator,
        &id,
        Spool::Source,
        &source,
    )
    .unwrap()
    .remove(0);
    coordinator.source_ready(&id).unwrap();
    journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback).unwrap();
    coordinator.prepared(&id).unwrap();
    drop(store);

    let mut reopened = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
    assert!(reopened
        .prepare_replace_commit(&unrelated, Some(0))
        .is_err());
    let prepared = reopened
        .prepare_replace_commit(&stage, Some(0))
        .unwrap();
    coordinator
        .section_intent(&id, "local-settings")
        .unwrap();
    apply_prepared_native_sections(&mut reopened, &source).unwrap();
    coordinator
        .section_complete(
            &id,
            "local-settings",
            &source_manifest.sha256,
        )
        .unwrap();
    coordinator.finish_device(&id).unwrap();
    let (marker_key, marker) = coordinator.commit_marker(&id).unwrap();
    let committed = reopened
        .finish_prepared_replace_with_app_kv(
            prepared,
            &marker_key,
            &serde_json::to_value(marker).unwrap(),
        )
        .unwrap();
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
