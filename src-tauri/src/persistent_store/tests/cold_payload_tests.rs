use super::*;

#[test]
fn cold_authority_marker_rejects_preparing_and_activates_v2_with_the_generation() {
    let directory = tempfile::tempdir().expect("create cold authority directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    assert_eq!(
        store
            .read_cold_payload_authority(None)
            .expect("read initial cold authority")
            .value,
        ColdPayloadAuthorityState::Legacy
    );

    let staging = store.replace_begin().expect("begin preparing replacement");
    store
        .replace_put_cold_payload_authority(
            &staging.staging_id,
            &ColdPayloadAuthorityState::Preparing {
                migration_id: "cold-migration".to_owned(),
                source_revision: 0,
            },
        )
        .expect("stage preparing cold authority");
    assert!(store.replace_commit(&staging.staging_id, Some(0)).is_err());
    assert_eq!(store.revision().expect("read unchanged revision"), 0);
    assert_eq!(
        store
            .read_cold_payload_authority(None)
            .expect("read unchanged cold authority")
            .value,
        ColdPayloadAuthorityState::Legacy
    );

    let authority = ColdPayloadAuthorityState::V2 {
        migration_id: "cold-migration".to_owned(),
        compatibility_hash: "9b".repeat(32),
    };
    store
        .replace_put_cold_payload_authority(&staging.staging_id, &authority)
        .expect("stage v2 cold authority");
    let activated = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate v2 cold authority");
    assert_eq!(
        store
            .read_cold_payload_authority(None)
            .expect("read v2 cold authority"),
        super::Versioned {
            revision: activated.revision,
            value: authority,
        }
    );
}

#[test]
fn staged_v2_cold_authority_rejects_missing_and_wrong_sized_cas_objects() {
    let directory = tempfile::tempdir().expect("create staged cold validation directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let authority = ColdPayloadAuthorityState::V2 {
        migration_id: "cold-restore".to_owned(),
        compatibility_hash: "7a".repeat(32),
    };
    let missing = ColdAlias {
        key: "cold/missing".to_owned(),
        object_hash: Some("7b".repeat(32)),
        size: 4,
        metadata: json!({}),
    };
    let missing_staging = store.replace_begin().expect("begin missing CAS staging");
    store
        .replace_put_cold_aliases(&missing_staging.staging_id, &[missing])
        .expect("stage missing cold alias");
    store
        .replace_put_cold_payload_authority(&missing_staging.staging_id, &authority)
        .expect("stage missing cold authority");
    assert!(matches!(
        store.replace_commit(&missing_staging.staging_id, Some(0)),
        Err(StoreError::Validation { .. })
    ));

    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"four")
        .expect("prepare wrong-sized object");
    let wrong_size = ColdAlias {
        key: "cold/wrong-size".to_owned(),
        object_hash: Some(prepared.content_hash),
        size: 5,
        metadata: json!({}),
    };
    let wrong_staging = store.replace_begin().expect("begin wrong-size staging");
    store
        .replace_put_cold_aliases(&wrong_staging.staging_id, &[wrong_size])
        .expect("stage wrong-sized cold alias");
    store
        .replace_put_cold_payload_authority(&wrong_staging.staging_id, &authority)
        .expect("stage wrong-sized cold authority");
    assert!(matches!(
        store.replace_commit(&wrong_staging.staging_id, Some(0)),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("read unchanged revision"), 0);
}

#[test]
fn database_only_replace_preserves_active_v2_cold_payloads_across_reopen() {
    let directory = tempfile::tempdir().expect("create cold preservation directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"preserved-cold")
        .expect("prepare preserved cold object");
    let alias = ColdAlias {
        key: "cold/preserved".to_owned(),
        object_hash: Some(prepared.content_hash.clone()),
        size: prepared.byte_size as i64,
        metadata: json!({ "source": "database-only-restore" }),
    };
    let expected_authority = ColdPayloadAuthorityState::V2 {
        migration_id: "cold-preservation".to_owned(),
        compatibility_hash: "7c".repeat(32),
    };
    let migrated = store
        .activate_cold_payload_migration(&ColdPayloadMigrationInput {
            source_revision: 0,
            migration_id: "cold-preservation".to_owned(),
            compatibility_hash: "7c".repeat(32),
            cold_aliases: vec![alias.clone()],
        })
        .expect("activate cold payload authority");

    let staging = store.replace_begin().expect("begin database replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "username": "Restored database" }),
        )
        .expect("stage restored root");
    store
        .replace_preserve_repositories(&staging.staging_id, Some(migrated.revision))
        .expect("preserve active repositories");
    let replaced = store
        .replace_commit(&staging.staging_id, Some(migrated.revision))
        .expect("activate restored database");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert_eq!(
        reopened
            .read_cold_payload_authority(None)
            .expect("read preserved authority"),
        super::Versioned {
            revision: replaced.revision,
            value: expected_authority,
        }
    );
    assert_eq!(
        reopened
            .list_cold_aliases(None)
            .expect("list preserved aliases"),
        super::Versioned {
            revision: replaced.revision,
            value: vec![alias],
        }
    );
    assert_eq!(
        cas.read_object(&prepared.content_hash)
            .expect("read preserved CAS object"),
        Some(b"preserved-cold".to_vec())
    );
}

#[test]
fn database_only_replace_preserves_repositories_and_only_matching_owner_heads() {
    let directory = tempfile::tempdir().expect("create repository preservation directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut database = fixture();
    database["modules"] = json!([
        {
            "id": "kept-module",
            "name": "Kept module",
            "description": "",
            "assets": [["kept", "assets/kept.bin", "BIN"]]
        },
        {
            "id": "changed-module",
            "name": "Changed module",
            "description": "",
            "assets": [["old", "assets/old.bin", "BIN"]]
        }
    ]);
    let aliases = vec![
        AssetAlias {
            key: "assets/kept.bin".to_owned(),
            object_hash: Some("61".repeat(32)),
            kind: "asset".to_owned(),
            size: 4,
            mime: "application/octet-stream".to_owned(),
            name: "Kept".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
        AssetAlias {
            key: "assets/old.bin".to_owned(),
            object_hash: Some("62".repeat(32)),
            kind: "asset".to_owned(),
            size: 3,
            mime: "application/octet-stream".to_owned(),
            name: "Old".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
    ];
    let heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "63".repeat(32),
            1,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 1 },
            "64".repeat(32),
            1,
        ),
    ];
    let asset_authority = AssetRepositoryAuthorityState::V2 {
        migration_id: "asset-database-replacement".to_owned(),
        compatibility_hash: "65".repeat(32),
    };
    let initial = store
        .replace_begin()
        .expect("begin v2 repository generation");
    store
        .replace_put_root(&initial.staging_id, &staged_root(&database))
        .expect("stage v2 repository root");
    store
        .replace_put_presets(
            &initial.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage v2 repository presets");
    store
        .replace_add_characters(
            &initial.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage v2 repository characters");
    store
        .replace_put_asset_aliases(&initial.staging_id, &aliases)
        .expect("stage v2 aliases");
    store
        .replace_put_asset_owner_heads(&initial.staging_id, &heads)
        .expect("stage v2 owner heads");
    store
        .replace_put_asset_repository_authority(&initial.staging_id, &asset_authority)
        .expect("stage v2 asset authority");
    let assets_activated = store
        .replace_commit(&initial.staging_id, Some(0))
        .expect("activate v2 asset repository");

    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"cold-database-replacement")
        .expect("prepare cold replacement object");
    let cold_alias = ColdAlias {
        key: "cold/database-replacement".to_owned(),
        object_hash: Some(prepared.content_hash),
        size: prepared.byte_size as i64,
        metadata: json!({ "source": "before-database-replacement" }),
    };
    let cold_authority = ColdPayloadAuthorityState::V2 {
        migration_id: "cold-database-replacement".to_owned(),
        compatibility_hash: "67".repeat(32),
    };
    let cold_activated = store
        .activate_cold_payload_migration(&ColdPayloadMigrationInput {
            source_revision: assets_activated.revision,
            migration_id: "cold-database-replacement".to_owned(),
            compatibility_hash: "67".repeat(32),
            cold_aliases: vec![cold_alias.clone()],
        })
        .expect("activate cold repository");

    let mut replacement = database;
    replacement["username"] = json!("Database-only replacement");
    replacement["modules"][1]["assets"] = json!([["new", "assets/new.bin", "BIN"]]);
    let staging = store
        .replace_begin()
        .expect("begin database-only replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&replacement))
        .expect("stage replacement root");
    store
        .replace_put_presets(
            &staging.staging_id,
            replacement["botPresets"]
                .as_array()
                .expect("replacement presets"),
        )
        .expect("stage replacement presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            replacement["characters"]
                .as_array()
                .expect("replacement characters"),
        )
        .expect("stage replacement characters");
    store
        .replace_preserve_repositories(&staging.staging_id, Some(cold_activated.revision))
        .expect("preserve active repositories");
    let replaced = store
        .replace_commit(&staging.staging_id, Some(cold_activated.revision))
        .expect("activate database-only replacement");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert_eq!(
        reopened
            .read_asset_repository_authority(None)
            .expect("read preserved asset authority"),
        super::Versioned {
            revision: replaced.revision,
            value: asset_authority,
        }
    );
    for alias in aliases {
        assert_eq!(
            reopened
                .read_asset_alias("asset", &alias.key, None)
                .expect("read preserved asset alias")
                .expect("preserved asset alias exists")
                .value,
            alias
        );
    }
    assert_eq!(
        reopened
            .read_asset_owner_head(&heads[0].owner, None)
            .expect("read unchanged owner head")
            .expect("unchanged owner head exists")
            .value,
        heads[0]
    );
    assert_eq!(
        reopened
            .read_asset_owner_head(&heads[1].owner, None)
            .expect("read changed owner head"),
        None
    );
    assert_eq!(
        reopened
            .read_cold_payload_authority(None)
            .expect("read preserved cold authority"),
        super::Versioned {
            revision: replaced.revision,
            value: cold_authority,
        }
    );
    assert_eq!(
        reopened
            .list_cold_aliases(None)
            .expect("read preserved cold aliases")
            .value,
        vec![cold_alias]
    );
}

fn assert_database_only_replace_prunes_exact_candidates(replacement_root: Value) {
    let directory = tempfile::tempdir().expect("create alias provenance directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let reachable = cas.prepare_bytes(b"reachable replacement alias").unwrap();
    let unreachable = cas.prepare_bytes(b"unreachable replacement alias").unwrap();
    let aliases = [
        AssetAlias {
            key: "assets/reachable.bin".to_owned(),
            object_hash: Some(reachable.content_hash.clone()),
            kind: "asset".to_owned(),
            size: reachable.byte_size as i64,
            mime: "application/octet-stream".to_owned(),
            name: "Reachable".to_owned(),
            ext: "bin".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
        AssetAlias {
            key: "assets/unreachable.bin".to_owned(),
            object_hash: Some(unreachable.content_hash.clone()),
            kind: "asset".to_owned(),
            size: unreachable.byte_size as i64,
            mime: "application/octet-stream".to_owned(),
            name: "Unreachable".to_owned(),
            ext: "bin".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
    ];
    let initial = store.replace_begin().unwrap();
    store
        .replace_put_root(
            &initial.staging_id,
            &json!({ "activeAsset": "assets/reachable.bin" }),
        )
        .unwrap();
    store
        .replace_put_asset_aliases(&initial.staging_id, &aliases)
        .unwrap();
    store
        .replace_put_asset_repository_authority(
            &initial.staging_id,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "replacement-provenance".to_owned(),
                compatibility_hash: "8a".repeat(32),
            },
        )
        .unwrap();
    let activated = store.replace_commit(&initial.staging_id, Some(0)).unwrap();

    let replacement = store.replace_begin().unwrap();
    store
        .replace_put_root(&replacement.staging_id, &replacement_root)
        .unwrap();
    store
        .replace_preserve_repositories(&replacement.staging_id, Some(activated.revision))
        .unwrap();
    let replaced = store
        .replace_commit(&replacement.staging_id, Some(activated.revision))
        .unwrap();

    assert!(store
        .read_asset_alias("asset", "assets/reachable.bin", None)
        .unwrap()
        .is_some());
    assert!(store
        .read_asset_alias("asset", "assets/unreachable.bin", None)
        .unwrap()
        .is_none());
    let roots = super::snapshot::collect_asset_roots(&store.connection, &cas).unwrap();
    assert!(roots.object_hashes.contains(&reachable.content_hash));
    assert!(!roots.object_hashes.contains(&unreachable.content_hash));
    let provenance = store
        .connection
        .query_row(
            "SELECT logical_key, object_hash, byte_size
             FROM asset_alias_replacement_candidates
             WHERE generation = 'revision-2' AND logical_key = 'assets/unreachable.bin'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .expect("read exact forwarded alias provenance");
    assert_eq!(
        provenance,
        (
            "assets/unreachable.bin".to_owned(),
            unreachable.content_hash,
            unreachable.byte_size as i64,
        )
    );
    assert_eq!(replaced.revision, 2);
}

#[test]
fn database_only_replace_prunes_only_exact_forwarded_alias_candidates_after_complete_scan() {
    assert_database_only_replace_prunes_exact_candidates(
        json!({ "activeAsset": "assets/reachable.bin", "changed": true }),
    );
    assert_database_only_replace_prunes_exact_candidates(json!({
        "activeAsset": "assets/reachable.bin",
        "changed": true,
        "plugins": []
    }));
}

#[test]
fn historical_alias_without_replacement_provenance_remains_a_gc_root() {
    let directory = tempfile::tempdir().expect("create historical alias directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"historical alias payload").unwrap();
    let alias = AssetAlias {
        key: "assets/historical.bin".to_owned(),
        object_hash: Some(prepared.content_hash.clone()),
        kind: "asset".to_owned(),
        size: prepared.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Historical".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    store.commit_asset_alias(&alias, 0).unwrap();

    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_alias_replacement_candidates",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    let roots = super::snapshot::collect_asset_roots(&store.connection, &cas).unwrap();
    assert!(roots.object_hashes.contains(&prepared.content_hash));
    assert!(store
        .read_asset_alias("asset", &alias.key, None)
        .unwrap()
        .is_some());
}

#[test]
fn database_only_replace_retains_forwarded_aliases_when_plugin_scan_is_opaque() {
    let directory = tempfile::tempdir().expect("create opaque alias directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let alias = AssetAlias {
        key: "assets/ambiguous.bin".to_owned(),
        object_hash: Some("8b".repeat(32)),
        kind: "asset".to_owned(),
        size: 9,
        mime: "application/octet-stream".to_owned(),
        name: "Ambiguous".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let initial = store.replace_begin().unwrap();
    store
        .replace_put_root(&initial.staging_id, &json!({}))
        .unwrap();
    store
        .replace_put_asset_aliases(&initial.staging_id, std::slice::from_ref(&alias))
        .unwrap();
    let activated = store.replace_commit(&initial.staging_id, Some(0)).unwrap();
    let replacement = store.replace_begin().unwrap();
    store
        .replace_put_root(
            &replacement.staging_id,
            &json!({ "pluginCustomStorage": { "opaque-plugin": { "state": true } } }),
        )
        .unwrap();
    store
        .replace_preserve_repositories(&replacement.staging_id, Some(activated.revision))
        .unwrap();
    store
        .replace_commit(&replacement.staging_id, Some(activated.revision))
        .unwrap();

    assert_eq!(
        store
            .read_asset_alias("asset", &alias.key, None)
            .unwrap()
            .unwrap()
            .value,
        alias
    );
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let roots = super::snapshot::collect_asset_roots(&store.connection, &cas).unwrap();
    assert!(roots
        .object_hashes
        .contains(alias.object_hash.as_deref().unwrap()));
    assert!(roots.retain_all_objects);
}

fn assert_database_only_replace_retains_alias_for_opaque_plugin_root(replacement_root: Value) {
    let directory = tempfile::tempdir().expect("create plugin script alias directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas.prepare_bytes(b"plugin script alias payload").unwrap();
    let alias = AssetAlias {
        key: "assets/plugin-only.bin".to_owned(),
        object_hash: Some(prepared.content_hash.clone()),
        kind: "asset".to_owned(),
        size: prepared.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Plugin only".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let initial = store.replace_begin().unwrap();
    store
        .replace_put_root(&initial.staging_id, &json!({}))
        .unwrap();
    store
        .replace_put_asset_aliases(&initial.staging_id, std::slice::from_ref(&alias))
        .unwrap();
    let activated = store.replace_commit(&initial.staging_id, Some(0)).unwrap();

    let replacement = store.replace_begin().unwrap();
    store
        .replace_put_root(&replacement.staging_id, &replacement_root)
        .unwrap();
    store
        .replace_preserve_repositories(&replacement.staging_id, Some(activated.revision))
        .unwrap();
    store
        .replace_commit(&replacement.staging_id, Some(activated.revision))
        .unwrap();

    assert_eq!(
        store
            .read_asset_alias("asset", &alias.key, None)
            .unwrap()
            .unwrap()
            .value,
        alias
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM asset_alias_replacement_candidates
                 WHERE generation = 'revision-2' AND kind = 'asset'
                   AND logical_key = 'assets/plugin-only.bin' AND object_hash = ?1",
                [&prepared.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .expect("count plugin script alias provenance"),
        1
    );
    let roots = super::snapshot::collect_asset_roots(&store.connection, &cas).unwrap();
    assert!(roots.object_hashes.contains(&prepared.content_hash));
}

#[test]
fn database_only_replace_retains_alias_referenced_only_by_installed_plugin_script() {
    assert_database_only_replace_retains_alias_for_opaque_plugin_root(json!({
        "plugins": [{
            "name": "Script asset reader",
            "script": "await readImage('assets/plugin-only.bin')"
        }],
        "pluginCustomStorage": {}
    }));
}

#[test]
fn database_only_replace_treats_malformed_installed_plugins_as_opaque() {
    assert_database_only_replace_retains_alias_for_opaque_plugin_root(json!({
        "plugins": { "malformed": true },
        "pluginCustomStorage": {}
    }));
}

#[test]
fn database_only_replace_preserves_owner_head_across_nested_object_key_order() {
    let (_directory, mut store, mut database) = open_fixture();
    let source_assets: Value = serde_json::from_str(r#"[[{"metadata":{"first":1,"second":2}}]]"#)
        .expect("parse source assets");
    database["modules"] = json!([{
        "id": "semantic-module",
        "name": "Semantic module",
        "description": "",
        "assets": source_assets
    }]);
    let head = AssetOwnerHead::present(
        AssetOwnerLocator::RootModuleAssets { index: 0 },
        "69".repeat(32),
        1,
    );
    let activated = store
        .commit(&WorkingSetCommit {
            root: Some(root(&database)),
            asset_owner_heads: Some(vec![head.clone()]),
            ..empty_working_set_commit(1)
        })
        .expect("activate semantic owner tuple");

    let replacement_assets: Value =
        serde_json::from_str(r#"[[{"metadata":{"second":2,"first":1}}]]"#)
            .expect("parse replacement assets");
    database["modules"][0]["assets"] = replacement_assets;
    let staging = store
        .replace_begin()
        .expect("begin semantic database replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&database))
        .expect("stage semantic replacement root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"]
                .as_array()
                .expect("replacement presets"),
        )
        .expect("stage semantic replacement presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("replacement characters"),
        )
        .expect("stage semantic replacement characters");
    store
        .replace_preserve_repositories(&staging.staging_id, Some(activated.revision))
        .expect("preserve semantically equal owner tuple");
    let replaced = store
        .replace_commit(&staging.staging_id, Some(activated.revision))
        .expect("activate semantic database replacement");

    assert_eq!(
        store
            .read_asset_owner_head(&head.owner, None)
            .expect("read retained semantic owner head"),
        Some(super::Versioned {
            revision: replaced.revision,
            value: head,
        })
    );
}

#[test]
fn database_only_replace_drops_heads_for_malformed_replacement_parents() {
    let directory = tempfile::tempdir().expect("create malformed parent directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut database = fixture();
    database["modules"] = json!([
        {
            "id": "kept-module",
            "name": "Kept module",
            "description": "",
            "assets": [["kept", "assets/kept.bin", "BIN"]]
        }
    ]);
    database["personas"] = json!([
        {
            "name": "Persona",
            "personaPrompt": "",
            "icon": "",
            "embeddedModule": {
                "id": "persona-module",
                "name": "Persona module",
                "description": "",
                "assets": [["persona", "assets/persona.bin", "BIN"]]
            }
        }
    ]);
    database["characters"][0]["additionalAssets"] = json!([["c", "assets/c.bin", "BIN"]]);
    let character_id = database["characters"][0]["chaId"]
        .as_str()
        .expect("fixture character id")
        .to_owned();
    let alias = AssetAlias {
        key: "assets/kept.bin".to_owned(),
        object_hash: Some("71".repeat(32)),
        kind: "asset".to_owned(),
        size: 4,
        mime: "application/octet-stream".to_owned(),
        name: "Kept".to_owned(),
        ext: "BIN".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "72".repeat(32),
            1,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 },
            "73".repeat(32),
            1,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::CharacterAdditionalAssets {
                character_id: character_id.clone(),
            },
            "74".repeat(32),
            1,
        ),
    ];
    let asset_authority = AssetRepositoryAuthorityState::V2 {
        migration_id: "asset-malformed-parent".to_owned(),
        compatibility_hash: "75".repeat(32),
    };
    let initial = store.replace_begin().expect("begin v2 generation");
    store
        .replace_put_root(&initial.staging_id, &staged_root(&database))
        .expect("stage v2 root");
    store
        .replace_put_presets(
            &initial.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage v2 presets");
    store
        .replace_add_characters(
            &initial.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage v2 characters");
    store
        .replace_put_asset_aliases(&initial.staging_id, std::slice::from_ref(&alias))
        .expect("stage v2 aliases");
    store
        .replace_put_asset_owner_heads(&initial.staging_id, &heads)
        .expect("stage v2 owner heads");
    store
        .replace_put_asset_repository_authority(&initial.staging_id, &asset_authority)
        .expect("stage v2 asset authority");
    let activated = store
        .replace_commit(&initial.staging_id, Some(0))
        .expect("activate v2 repository");

    let mut replacement = database;
    replacement["username"] = json!("Malformed replacement parents");
    replacement["modules"] = json!(null);
    replacement["personas"][0]["embeddedModule"] = json!(null);
    replacement["characters"][0]["additionalAssets"] = json!(null);
    let staging = store
        .replace_begin()
        .expect("begin database-only replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&replacement))
        .expect("stage replacement root");
    store
        .replace_put_presets(
            &staging.staging_id,
            replacement["botPresets"]
                .as_array()
                .expect("replacement presets"),
        )
        .expect("stage replacement presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            replacement["characters"]
                .as_array()
                .expect("replacement characters"),
        )
        .expect("stage replacement characters");
    store
        .replace_preserve_repositories(&staging.staging_id, Some(activated.revision))
        .expect("preserve repositories despite malformed replacement parents");
    let replaced = store
        .replace_commit(&staging.staging_id, Some(activated.revision))
        .expect("activate database-only replacement");

    for head in &heads {
        assert_eq!(
            store
                .read_asset_owner_head(&head.owner, None)
                .expect("read dropped owner head"),
            None
        );
    }
    assert_eq!(
        store
            .read_asset_alias("asset", &alias.key, None)
            .expect("read preserved asset alias")
            .expect("preserved asset alias exists")
            .value,
        alias
    );
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read preserved asset authority"),
        super::Versioned {
            revision: replaced.revision,
            value: asset_authority,
        }
    );
}

#[test]
fn missing_cold_authority_marker_fails_closed() {
    let directory = tempfile::tempdir().expect("create cold authority directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute(
            "DELETE FROM cold_payload_authority WHERE generation = 'revision-0'",
            [],
        )
        .expect("remove cold authority marker");

    assert!(matches!(
        store.read_cold_payload_authority(None),
        Err(StoreError::Validation { .. })
    ));
}

#[test]
fn cold_payload_migration_and_mutations_are_revisioned_with_exact_cas_aliases() {
    let directory = tempfile::tempdir().expect("create cold migration directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let first = cas
        .prepare_bytes(b"first-cold")
        .expect("prepare first cold object");
    let first_alias = ColdAlias {
        key: "cold/item".to_owned(),
        object_hash: Some(first.content_hash),
        size: first.byte_size as i64,
        metadata: json!({ "source": "legacy" }),
    };
    let migrated = store
        .activate_cold_payload_migration(&ColdPayloadMigrationInput {
            source_revision: 0,
            migration_id: "cold-migration".to_owned(),
            compatibility_hash: "8c".repeat(32),
            cold_aliases: vec![first_alias.clone()],
        })
        .expect("activate cold migration");
    assert_eq!(migrated.revision, 1);
    assert!(matches!(
        store
            .read_cold_payload_authority(None)
            .expect("read migrated cold authority")
            .value,
        ColdPayloadAuthorityState::V2 { .. }
    ));
    assert_eq!(
        store
            .read_cold_alias(&first_alias.key, None)
            .expect("read migrated alias")
            .expect("migrated alias exists")
            .value,
        first_alias
    );

    let lease = store
        .acquire_revision(1)
        .expect("pin migrated cold revision");
    let second = cas
        .prepare_bytes(b"second-cold")
        .expect("prepare second cold object");
    let second_alias = ColdAlias {
        key: "cold/item".to_owned(),
        object_hash: Some(second.content_hash),
        size: second.byte_size as i64,
        metadata: json!({ "source": "updated" }),
    };
    let updated = store
        .commit_cold_alias(&second_alias, 1)
        .expect("commit updated cold alias");
    assert_eq!(updated.revision, 2);
    assert_eq!(
        store
            .read_cold_alias(&second_alias.key, Some(&lease.lease))
            .expect("read pinned cold alias")
            .expect("pinned cold alias exists")
            .value,
        first_alias
    );
    assert_eq!(
        store
            .read_cold_alias(&second_alias.key, None)
            .expect("read current cold alias")
            .expect("current cold alias exists")
            .value,
        second_alias
    );

    let deleted = store
        .delete_cold_alias("cold/item", 2)
        .expect("delete cold alias");
    assert_eq!(deleted.revision, 3);
    assert!(store
        .read_cold_alias("cold/item", None)
        .expect("read deleted cold alias")
        .is_none());
    assert!(matches!(
        store.activate_cold_payload_migration(&ColdPayloadMigrationInput {
            source_revision: 3,
            migration_id: "second-migration".to_owned(),
            compatibility_hash: "8d".repeat(32),
            cold_aliases: vec![],
        }),
        Err(StoreError::Validation { .. })
    ));
}
