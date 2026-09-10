use super::*;

#[test]
fn ordinary_parent_saves_preserve_owner_heads_and_remap_modules() {
    let (_directory, mut store, database) = open_fixture();
    let mut value = root(&database);
    value["modules"] = json!([
        {"id":"one", "assets":[["a","assets/a","bin"]]},
        {"id":"two", "assets":[]}
    ]);
    let heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "11".repeat(32),
            1,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 1 },
            "22".repeat(32),
            0,
        ),
    ];
    store
        .commit(&WorkingSetCommit {
            root: Some(value.clone()),
            asset_owner_heads: Some(heads.clone()),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    value["theme"] = json!("changed");
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![super::super::RootMutation::Set {
                key: "theme".to_owned(),
                value: json!("changed"),
            }]),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    assert_eq!(store.list_asset_owner_heads(None).unwrap().value, heads);
    value["modules"].as_array_mut().unwrap().reverse();
    store
        .commit(&WorkingSetCommit {
            root: Some(value),
            ..empty_working_set_commit(3)
        })
        .unwrap();
    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .unwrap()
            .unwrap()
            .value
            .manifest_hash,
        heads[1].manifest_hash
    );
}

#[test]
fn ordinary_character_save_preserves_head_but_changed_assets_drop_it() {
    let (_directory, mut store, _database) = open_fixture();
    let mut detail = store.read_character("char-a", None).unwrap().unwrap().value;
    detail["additionalAssets"] = json!([["a", "assets/a", "bin"]]);
    let owner = AssetOwnerLocator::CharacterAdditionalAssets {
        character_id: "char-a".to_owned(),
    };
    let head = AssetOwnerHead::present(owner.clone(), "33".repeat(32), 1);
    store
        .commit(&WorkingSetCommit {
            character: Some(detail.clone()),
            asset_owner_heads: Some(vec![head.clone()]),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let lease = store.acquire_revision(2).unwrap();
    detail["name"] = json!("Changed name");
    store
        .commit(&WorkingSetCommit {
            character_details: Some(vec![detail.clone()]),
            ..empty_working_set_commit(2)
        })
        .unwrap();
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .unwrap()
            .unwrap()
            .value,
        head
    );
    detail["additionalAssets"] = json!([]);
    store
        .commit(&WorkingSetCommit {
            character: Some(detail),
            ..empty_working_set_commit(3)
        })
        .unwrap();
    assert!(store.read_asset_owner_head(&owner, None).unwrap().is_none());
    assert_eq!(
        store
            .read_asset_owner_head(&owner, Some(&lease.lease))
            .unwrap()
            .unwrap()
            .value,
        head
    );
}

#[test]
fn asset_owner_occurrences_are_isolated_by_revision_lease() {
    let (_directory, mut store, database) = open_fixture();
    let mut first_root = root(&database);
    first_root["modules"] = json!([
        {
            "id": "duplicate-module",
            "name": "First duplicate",
            "description": "",
            "assets": [
                ["first", "assets/first.bin", "BIN"],
                ["first", "assets/first.bin", "BIN"]
            ]
        },
        {
            "id": "duplicate-module",
            "name": "Second duplicate",
            "description": "",
            "assets": []
        }
    ]);
    first_root["personas"] = json!([
        {
            "name": "Missing ID and absent assets",
            "personaPrompt": "",
            "icon": "",
            "embeddedModule": { "id": "", "name": "Absent assets", "description": "" }
        },
        {
            "name": "Missing ID and present assets",
            "personaPrompt": "",
            "icon": "",
            "embeddedModule": {
                "id": "",
                "name": "Present assets",
                "description": "",
                "assets": [["persona", "assets/persona.bin", "OddExt"]]
            }
        }
    ]);
    let original_heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "11".repeat(32),
            2,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 1 },
            "22".repeat(32),
            0,
        ),
        AssetOwnerHead::absent(AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 }),
        AssetOwnerHead::present(
            AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 1 },
            "33".repeat(32),
            1,
        ),
    ];
    let shadowed = store
        .commit(&WorkingSetCommit {
            root: Some(first_root.clone()),
            asset_owner_heads: Some(original_heads.clone()),
            ..empty_working_set_commit(1)
        })
        .expect("commit original owner heads");
    let lease = store
        .acquire_revision(shadowed.revision)
        .expect("acquire owner-head revision");
    let mut reordered_root = first_root;
    reordered_root["modules"]
        .as_array_mut()
        .expect("module array")
        .reverse();
    let reordered_heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "22".repeat(32),
            0,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 1 },
            "11".repeat(32),
            2,
        ),
    ];
    let reordered = store
        .commit(&WorkingSetCommit {
            root: Some(reordered_root),
            asset_owner_heads: Some(reordered_heads.clone()),
            ..empty_working_set_commit(shadowed.revision)
        })
        .expect("commit reordered owner heads");

    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .expect("read current owner head"),
        Some(super::Versioned {
            revision: reordered.revision,
            value: reordered_heads[0].clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(
                &AssetOwnerLocator::RootModuleAssets { index: 0 },
                Some(&lease.lease),
            )
            .expect("read leased owner head"),
        Some(super::Versioned {
            revision: shadowed.revision,
            value: original_heads[0].clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(
                &AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 },
                Some(&lease.lease),
            )
            .expect("read leased absent owner head"),
        Some(super::Versioned {
            revision: shadowed.revision,
            value: original_heads[2].clone(),
        })
    );
}

#[test]
fn invalid_or_stale_owner_head_commit_preserves_parent_and_revision() {
    let (_directory, mut store, database) = open_fixture();
    let mut original_root = root(&database);
    original_root["modules"] = json!([{
        "id": "module",
        "name": "Module",
        "description": "",
        "assets": [["kept", "assets/kept.bin", "BIN"]]
    }]);
    let valid_head = AssetOwnerHead::present(
        AssetOwnerLocator::RootModuleAssets { index: 0 },
        "44".repeat(32),
        1,
    );
    let committed = store
        .commit(&WorkingSetCommit {
            root: Some(original_root.clone()),
            asset_owner_heads: Some(vec![valid_head.clone()]),
            ..empty_working_set_commit(1)
        })
        .expect("commit valid owner head");
    let mut rejected_root = original_root.clone();
    rejected_root["username"] = json!("must not commit");
    let invalid_head = AssetOwnerHead {
        owner: valid_head.owner.clone(),
        present: true,
        manifest_hash: Some("INVALID".to_owned()),
        entry_count: 1,
    };

    assert!(matches!(
        store.commit(&WorkingSetCommit {
            root: Some(rejected_root.clone()),
            asset_owner_heads: Some(vec![invalid_head]),
            ..empty_working_set_commit(committed.revision)
        }),
        Err(StoreError::Validation { .. })
    ));
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            root: Some(rejected_root),
            asset_owner_heads: Some(vec![valid_head.clone()]),
            ..empty_working_set_commit(1)
        }),
        Err(StoreError::RevisionConflict { .. })
    ));
    assert!(matches!(
        store.read_asset_owner_head(
            &AssetOwnerLocator::RootModuleAssets {
                index: super::JAVASCRIPT_MAX_SAFE_INTEGER + 1,
            },
            None,
        ),
        Err(StoreError::Validation { .. })
    ));

    assert_eq!(store.revision().expect("read revision"), committed.revision);
    assert_eq!(
        store.read_root(None).expect("read root").value,
        original_root
    );
    assert_eq!(
        store
            .read_asset_owner_head(&valid_head.owner, None)
            .expect("read owner head")
            .expect("owner head exists")
            .value,
        valid_head
    );
}

#[test]
fn character_name_change_preserves_omitted_owner_head() {
    let (_directory, mut store, _) = open_fixture();
    let mut detail = store
        .read_character("char-a", None)
        .expect("read character")
        .expect("character exists")
        .value;
    detail["additionalAssets"] = json!([
        ["duplicate", "assets/duplicate.bin", "BIN"],
        ["duplicate", "assets/duplicate.bin", "BIN"]
    ]);
    let owner = AssetOwnerLocator::CharacterAdditionalAssets {
        character_id: "char-a".to_owned(),
    };
    let head = AssetOwnerHead::present(owner.clone(), "55".repeat(32), 2);
    let shadowed = store
        .commit(&WorkingSetCommit {
            character: Some(detail.clone()),
            asset_owner_heads: Some(vec![head.clone()]),
            ..empty_working_set_commit(1)
        })
        .expect("commit character owner head");
    assert!(store
        .read_asset_owner_head(&owner, None)
        .expect("read owner head")
        .is_some());
    detail["name"] = json!("Changed through legacy path");
    let changed = store
        .commit(&WorkingSetCommit {
            character: Some(detail),
            ..empty_working_set_commit(shadowed.revision)
        })
        .expect("commit legacy character change");

    assert_eq!(store.revision().expect("read revision"), changed.revision);
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read retained owner head")
            .unwrap()
            .value,
        head
    );
}

#[test]
fn owner_head_validation_uses_the_final_character_parent_and_rejects_atomically() {
    let (_directory, mut store, database) = open_fixture();
    let mut earlier_detail = store
        .read_character("char-a", None)
        .expect("read character")
        .expect("character exists")
        .value;
    earlier_detail["additionalAssets"] = json!([["earlier", "assets/earlier.bin", "BIN"]]);
    let mut final_character = database["characters"]
        .as_array()
        .expect("fixture characters")
        .iter()
        .find(|character| character["chaId"] == "char-a")
        .expect("fixture character")
        .clone();
    final_character["additionalAssets"] = json!([
        ["final-a", "assets/final-a.bin", "BIN"],
        ["final-b", "assets/final-b.bin", "OddExt"]
    ]);
    let owner = AssetOwnerLocator::CharacterAdditionalAssets {
        character_id: "char-a".to_owned(),
    };
    let final_head = AssetOwnerHead::present(owner.clone(), "66".repeat(32), 2);

    let committed = store
        .commit(&WorkingSetCommit {
            character_details: Some(vec![earlier_detail.clone()]),
            replace_character: Some(final_character.clone()),
            asset_owner_heads: Some(vec![final_head.clone()]),
            ..empty_working_set_commit(1)
        })
        .expect("validate against final character replacement");

    let stored_character = store
        .read_character("char-a", None)
        .expect("read final character")
        .expect("final character exists")
        .value;
    assert_eq!(
        stored_character["additionalAssets"],
        final_character["additionalAssets"]
    );
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read final owner head")
            .expect("final owner head exists")
            .value,
        final_head
    );

    let root_before = store.read_root(None).expect("read root before rejection");
    let mut rejected_root = root_before.value.clone();
    rejected_root["username"] = json!("must not commit");
    let earlier_head = AssetOwnerHead::present(owner.clone(), "77".repeat(32), 1);

    assert!(matches!(
        store.commit(&WorkingSetCommit {
            root: Some(rejected_root),
            character_details: Some(vec![earlier_detail]),
            replace_character: Some(final_character),
            asset_owner_heads: Some(vec![earlier_head]),
            ..empty_working_set_commit(committed.revision)
        }),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("read revision"), committed.revision);
    assert_eq!(
        store.read_root(None).expect("read unchanged root"),
        root_before
    );
    assert_eq!(
        store
            .read_character("char-a", None)
            .expect("read unchanged character")
            .expect("unchanged character exists")
            .value,
        stored_character
    );
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read unchanged owner head")
            .expect("unchanged owner head exists")
            .value,
        final_head
    );
}

#[test]
fn unchanged_asset_owner_head_survives_cow_generation_and_pinned_reads() {
    let (_directory, mut store, database) = open_fixture();
    let mut database_root = root(&database);
    database_root["modules"] = json!([{
        "id": "module",
        "name": "Module",
        "description": "",
        "assets": [["asset", "assets/owner.bin", "BIN"]]
    }]);
    database_root["personas"] = json!([{
        "name": "Absent assets",
        "embeddedModule": { "id": "embedded", "name": "Embedded" }
    }]);
    let owner = AssetOwnerLocator::RootModuleAssets { index: 0 };
    let head = AssetOwnerHead::present(owner.clone(), "81".repeat(32), 1);
    let absent_owner = AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 };
    let absent_head = AssetOwnerHead::absent(absent_owner.clone());
    let committed = store
        .commit(&WorkingSetCommit {
            root: Some(database_root),
            asset_owner_heads: Some(vec![head.clone(), absent_head.clone()]),
            ..empty_working_set_commit(1)
        })
        .expect("commit M5 owner head");
    let lease = store
        .acquire_revision(committed.revision)
        .expect("pin M5 owner head");
    let unrelated_alias = AssetAlias {
        key: "assets/unrelated.bin".to_owned(),
        object_hash: Some("82".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Unrelated".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let copied = store
        .commit_asset_alias(&unrelated_alias, committed.revision)
        .expect("commit unrelated generation mutation");

    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read current owner head"),
        Some(super::Versioned {
            revision: copied.revision,
            value: head.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(&owner, Some(&lease.lease))
            .expect("read pinned owner head"),
        Some(super::Versioned {
            revision: committed.revision,
            value: head,
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(&absent_owner, None)
            .expect("read current absent owner head"),
        Some(super::Versioned {
            revision: copied.revision,
            value: absent_head.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(&absent_owner, Some(&lease.lease))
            .expect("read pinned absent owner head"),
        Some(super::Versioned {
            revision: committed.revision,
            value: absent_head,
        })
    );
}

#[test]
fn staged_payload_namespaces_with_the_same_key_activate_atomically() {
    let directory = tempfile::tempdir().expect("create payload namespace directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    let key = "shared/payload-key";
    let asset = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("11".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Asset".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({ "name": "Asset", "ext": "bin", "mime": "application/octet-stream" }),
    };
    let inlay = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("22".repeat(32)),
        kind: "inlay".to_owned(),
        size: 2,
        mime: "image/webp".to_owned(),
        name: "Inlay".to_owned(),
        ext: "webp".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(10),
        height: Some(20),
        metadata: json!({
            "name": "Inlay",
            "ext": "webp",
            "mime": "image/webp",
            "inlayType": "image",
            "width": 10,
            "height": 20
        }),
    };
    let cold = ColdAlias {
        key: key.to_owned(),
        object_hash: Some("33".repeat(32)),
        size: 3,
        metadata: json!({ "source": "cold-storage", "ordinal": 7 }),
    };

    store
        .replace_put_asset_aliases(&staging.staging_id, &[inlay.clone(), asset.clone()])
        .expect("stage asset and Inlay aliases");
    store
        .replace_put_cold_aliases(&staging.staging_id, std::slice::from_ref(&cold))
        .expect("stage cold alias");
    assert_eq!(
        store
            .read_asset_alias("asset", key, None)
            .expect("read pre-activation ordinary asset"),
        None
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", key, None)
            .expect("read pre-activation Inlay"),
        None
    );
    assert_eq!(
        store
            .read_cold_alias(key, None)
            .expect("read pre-activation cold alias"),
        None
    );
    assert_eq!(store.revision().expect("read pre-activation revision"), 0);
    assert_eq!(
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("activate all payload namespaces")
            .revision,
        1
    );

    assert_eq!(
        store
            .read_asset_alias("asset", key, None)
            .expect("read ordinary asset")
            .expect("ordinary asset exists")
            .value,
        asset
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", key, None)
            .expect("read Inlay")
            .expect("Inlay exists")
            .value,
        inlay
    );
    assert_eq!(
        store
            .read_cold_alias(key, None)
            .expect("read cold alias")
            .expect("cold alias exists")
            .value,
        cold
    );
}

#[test]
fn alias_catalog_pages_and_deletes_only_the_typed_alias() {
    let directory = tempfile::tempdir().expect("create alias catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    let key = "shared/catalog-key";
    let asset = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("31".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Asset".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let inlay = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("32".repeat(32)),
        kind: "inlay".to_owned(),
        size: 2,
        mime: "image/webp".to_owned(),
        name: "Inlay".to_owned(),
        ext: "webp".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(1),
        height: Some(1),
        metadata: json!({}),
    };
    store
        .replace_put_asset_aliases(&staging.staging_id, &[inlay.clone(), asset.clone()])
        .expect("stage aliases");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate aliases");
    let lease = store
        .acquire_revision(imported.revision)
        .expect("pin aliases");

    let first = store
        .list_asset_alias_page(
            &AssetAliasListQuery {
                kind: None,
                limit: 1,
                cursor: None,
            },
            None,
        )
        .expect("list first alias page");
    assert_eq!(first.items, vec![asset.clone()]);
    let second = store
        .list_asset_alias_page(
            &AssetAliasListQuery {
                kind: None,
                limit: 1,
                cursor: first.next_cursor,
            },
            None,
        )
        .expect("list second alias page");
    assert_eq!(second.items, vec![inlay.clone()]);

    let deleted = store
        .delete_asset_alias("asset", key, imported.revision)
        .expect("delete ordinary asset alias");
    assert_eq!(
        store
            .read_asset_alias("asset", key, None)
            .expect("read deleted alias"),
        None
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", key, None)
            .expect("read sibling Inlay")
            .expect("sibling Inlay exists")
            .revision,
        deleted.revision
    );
    assert_eq!(
        store
            .read_asset_alias("asset", key, Some(&lease.lease))
            .expect("read pinned asset")
            .expect("pinned asset exists")
            .value,
        asset
    );
}

#[test]
fn authority_marker_rejects_preparing_and_activates_v2_with_the_generation() {
    let directory = tempfile::tempdir().expect("create authority directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read initial authority")
            .value,
        AssetRepositoryAuthorityState::Legacy
    );

    let staging = store.replace_begin().expect("begin preparing replacement");
    store
        .replace_put_asset_repository_authority(
            &staging.staging_id,
            &AssetRepositoryAuthorityState::Preparing {
                migration_id: "migration-atomic".to_owned(),
                source_revision: 0,
            },
        )
        .expect("stage preparing authority");
    assert!(store.replace_commit(&staging.staging_id, Some(0)).is_err());
    assert_eq!(store.revision().expect("read unchanged revision"), 0);
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read unchanged authority")
            .value,
        AssetRepositoryAuthorityState::Legacy
    );

    store
        .replace_put_asset_repository_authority(
            &staging.staging_id,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "migration-atomic".to_owned(),
                compatibility_hash: "9a".repeat(32),
            },
        )
        .expect("stage v2 authority");
    let activated = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate v2 authority");
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read v2 authority"),
        super::Versioned {
            revision: activated.revision,
            value: AssetRepositoryAuthorityState::V2 {
                migration_id: "migration-atomic".to_owned(),
                compatibility_hash: "9a".repeat(32),
            },
        }
    );
}

#[test]
fn v2_compatibility_materialization_and_export_fail_closed_on_corrupt_owner_manifest() {
    use crate::asset_repository::{owner_manifest_codec, PayloadCas};

    let directory = tempfile::tempdir().expect("create owner projection directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let tuples = json!([
        ["first", "assets/shared.bin", "BIN"],
        ["first", "assets/shared.bin", "BIN"]
    ]);
    let manifest_bytes = owner_manifest_codec::encode_owner_manifest(&[
        owner_manifest_codec::OwnerManifestEntry {
            tuple: [
                "first".to_owned(),
                "assets/shared.bin".to_owned(),
                "BIN".to_owned(),
            ],
            payload_hash: None,
        },
        owner_manifest_codec::OwnerManifestEntry {
            tuple: [
                "first".to_owned(),
                "assets/shared.bin".to_owned(),
                "BIN".to_owned(),
            ],
            payload_hash: None,
        },
    ])
    .expect("encode owner manifest");
    let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
    let manifest = cas
        .prepare_bytes(&manifest_bytes)
        .expect("prepare owner manifest");
    let staging = store.replace_begin().expect("begin v2 replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({
                "modules": [{
                    "id": "module",
                    "name": "Module",
                    "description": "",
                    "assets": tuples.clone()
                }]
            }),
        )
        .expect("stage v2 root");
    store
        .replace_put_asset_owner_heads(
            &staging.staging_id,
            &[AssetOwnerHead::present(
                AssetOwnerLocator::RootModuleAssets { index: 0 },
                manifest.content_hash.clone(),
                2,
            )],
        )
        .expect("stage owner head");
    store
        .replace_put_asset_repository_authority(
            &staging.staging_id,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "projection-test".to_owned(),
                compatibility_hash: "ab".repeat(32),
            },
        )
        .expect("stage v2 authority");
    let activated = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate v2 generation");
    let lease = store
        .acquire_revision(activated.revision)
        .expect("acquire v2 revision");

    assert_eq!(
        store.materialize(None).expect("materialize valid v2 owner")["modules"][0]["assets"],
        tuples
    );
    let exported = store
        .export_risu_save(&lease.lease, false)
        .expect("export valid v2 owner");
    store
        .cleanup_risu_save_export(Path::new(&exported.path))
        .expect("clean valid export");

    let generation = super::active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', '00', 1, ?2, 2)",
            params![generation, manifest.content_hash],
        )
        .expect("insert noncanonical duplicate owner locator");
    let locator_error = store
        .materialize(None)
        .expect_err("noncanonical owner locators must fail closed");
    assert!(locator_error.to_string().contains("locator"));
    store
        .connection
        .execute(
            "DELETE FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = 'root-module-assets' AND owner_locator = '00'",
            [generation],
        )
        .expect("remove noncanonical duplicate owner locator");

    fs::write(directory.path().join(&manifest.physical_key), b"corrupt")
        .expect("corrupt owner manifest object");

    let materialize_error = store
        .materialize(None)
        .expect_err("materialization must validate owner manifest identity");
    assert!(materialize_error.to_string().contains("owner manifest"));
    let lease_error = store
        .materialize_lease(&lease.lease)
        .expect_err("leased materialization must validate owner manifest identity");
    assert!(lease_error.to_string().contains("owner manifest"));
    let export_error = store
        .export_risu_save(&lease.lease, false)
        .expect_err("native export must validate owner manifest identity");
    assert!(export_error.to_string().contains("owner manifest"));
}

#[test]
fn staged_owner_heads_activate_and_remain_pinned_with_their_database_generation() {
    let directory = tempfile::tempdir().expect("create staged owner-head directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({
                "modules": [{
                    "id": "module",
                    "name": "Module",
                    "assets": [["asset", "shared", "BIN"]]
                }],
                "personas": [{
                    "id": "persona",
                    "embeddedModule": { "id": "embedded", "name": "Embedded" }
                }]
            }),
        )
        .expect("stage owner-head root");
    store
        .replace_put_presets(&staging.staging_id, &[])
        .expect("stage empty presets");
    store
        .replace_add_characters(&staging.staging_id, &[])
        .expect("stage empty characters");
    let present = AssetOwnerHead::present(
        AssetOwnerLocator::RootModuleAssets { index: 0 },
        "91".repeat(32),
        1,
    );
    let absent =
        AssetOwnerHead::absent(AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 });
    store
        .replace_put_asset_owner_heads(&staging.staging_id, &[present.clone(), absent.clone()])
        .expect("stage owner heads");
    let committed = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate owner-head generation");
    let lease = store
        .acquire_revision(committed.revision)
        .expect("pin owner-head generation");

    let expected = vec![absent, present];
    assert_eq!(
        store
            .list_asset_owner_heads(None)
            .expect("list current owner heads")
            .value,
        expected
    );
    assert_eq!(
        store
            .list_asset_owner_heads(Some(&lease.lease))
            .expect("list pinned owner heads")
            .value,
        expected
    );
}

#[test]
fn committing_one_alias_kind_preserves_the_sibling_kind_and_pinned_revision() {
    let directory = tempfile::tempdir().expect("create alias namespace directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    let key = "shared/mutation-key";
    let asset = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("41".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Asset".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let inlay = AssetAlias {
        key: key.to_owned(),
        object_hash: Some("42".repeat(32)),
        kind: "inlay".to_owned(),
        size: 2,
        mime: "image/png".to_owned(),
        name: "Inlay".to_owned(),
        ext: "png".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(2),
        height: Some(3),
        metadata: json!({}),
    };
    store
        .replace_put_asset_aliases(&staging.staging_id, &[asset.clone(), inlay.clone()])
        .expect("stage sibling alias kinds");
    let first = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate sibling alias kinds");
    let lease = store
        .acquire_revision(first.revision)
        .expect("pin sibling alias kinds");
    let replacement = AssetAlias {
        object_hash: Some("43".repeat(32)),
        name: "Replacement asset".to_owned(),
        ..asset.clone()
    };

    let second = store
        .commit_asset_alias(&replacement, first.revision)
        .expect("commit only ordinary asset kind");

    assert_eq!(
        store
            .read_asset_alias("asset", key, None)
            .expect("read current ordinary asset"),
        Some(super::Versioned {
            revision: second.revision,
            value: replacement,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", key, None)
            .expect("read current Inlay"),
        Some(super::Versioned {
            revision: second.revision,
            value: inlay.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_alias("asset", key, Some(&lease.lease))
            .expect("read pinned ordinary asset"),
        Some(super::Versioned {
            revision: first.revision,
            value: asset,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", key, Some(&lease.lease))
            .expect("read pinned Inlay"),
        Some(super::Versioned {
            revision: first.revision,
            value: inlay,
        })
    );
}

#[test]
fn asset_alias_unknown_metadata_survives_activation_lease_and_reopen() {
    let directory = tempfile::tempdir().expect("create alias metadata directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let original = AssetAlias {
        key: "asset/metadata".to_owned(),
        object_hash: Some("12".repeat(32)),
        kind: "asset".to_owned(),
        size: 12,
        mime: "application/octet-stream".to_owned(),
        name: "Metadata".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({
            "name": "Metadata",
            "ext": "bin",
            "mime": "application/octet-stream",
            "unknown": {
                "nested": [0, false, null, { "unicode": "메타데이터" }]
            }
        }),
    };
    let original_inlay = AssetAlias {
        key: original.key.clone(),
        object_hash: Some("56".repeat(32)),
        kind: "inlay".to_owned(),
        size: 56,
        mime: "image/webp".to_owned(),
        name: "Metadata Inlay".to_owned(),
        ext: "webp".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(4),
        height: Some(5),
        metadata: json!({
            "name": "Metadata Inlay",
            "ext": "webp",
            "mime": "image/webp",
            "inlayType": "image",
            "width": 4,
            "height": 5,
            "unknown": { "nested": [{ "retained": true }] }
        }),
    };
    let staging = store.replace_begin().expect("begin alias metadata staging");
    store
        .replace_put_asset_aliases(
            &staging.staging_id,
            &[original.clone(), original_inlay.clone()],
        )
        .expect("stage alias metadata");
    let first = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate alias metadata");
    let lease = store
        .acquire_revision(first.revision)
        .expect("lease original alias metadata");
    let replacement = AssetAlias {
        object_hash: Some("34".repeat(32)),
        metadata: json!({
            "name": "Metadata",
            "ext": "bin",
            "mime": "application/octet-stream",
            "unknown": { "nested": ["current"] }
        }),
        ..original.clone()
    };
    let second = store
        .commit_asset_alias(&replacement, first.revision)
        .expect("replace current alias metadata");
    assert_eq!(
        store
            .read_asset_alias("asset", &original.key, Some(&lease.lease))
            .expect("read leased alias metadata"),
        Some(super::Versioned {
            revision: first.revision,
            value: original.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", &original.key, Some(&lease.lease))
            .expect("read leased Inlay metadata"),
        Some(super::Versioned {
            revision: first.revision,
            value: original_inlay.clone(),
        })
    );
    assert_eq!(
        store
            .list_asset_aliases(Some(&lease.lease))
            .expect("list leased alias metadata")
            .value,
        vec![original.clone(), original_inlay.clone()]
    );
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen alias metadata store");
    assert_eq!(
        store
            .read_asset_alias("asset", &original.key, None)
            .expect("read current alias metadata"),
        Some(super::Versioned {
            revision: second.revision,
            value: replacement.clone(),
        })
    );
    assert_eq!(
        store
            .list_asset_aliases(None)
            .expect("list current alias metadata")
            .value,
        vec![replacement, original_inlay]
    );
    assert!(matches!(
        store.read_asset_alias("asset", &original.key, Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert!(matches!(
        store.list_asset_aliases(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn payload_inventories_and_typed_reads_are_deterministic_at_a_pinned_revision() {
    let directory = tempfile::tempdir().expect("create pinned inventory directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let shared_asset = AssetAlias {
        key: "shared".to_owned(),
        object_hash: Some("11".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Shared asset".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let z_asset = AssetAlias {
        key: "z-last".to_owned(),
        name: "Last asset".to_owned(),
        ..shared_asset.clone()
    };
    let shared_inlay = AssetAlias {
        key: "shared".to_owned(),
        object_hash: Some("22".repeat(32)),
        kind: "inlay".to_owned(),
        size: 2,
        mime: "image/webp".to_owned(),
        name: "Shared Inlay".to_owned(),
        ext: "webp".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(1),
        height: Some(2),
        metadata: json!({}),
    };
    let cold_a = ColdAlias {
        key: "a-first".to_owned(),
        object_hash: Some("33".repeat(32)),
        size: 3,
        metadata: json!({ "kind": "memory" }),
    };
    let cold_z = ColdAlias {
        key: "z-last".to_owned(),
        object_hash: None,
        size: 0,
        metadata: json!({ "kind": "embedding", "missing": true }),
    };
    let first_staging = store.replace_begin().expect("begin first inventory");
    store
        .replace_put_asset_aliases(
            &first_staging.staging_id,
            &[z_asset.clone(), shared_inlay.clone(), shared_asset.clone()],
        )
        .expect("stage first asset inventory");
    store
        .replace_put_cold_aliases(&first_staging.staging_id, &[cold_z.clone(), cold_a.clone()])
        .expect("stage first cold inventory");
    let first = store
        .replace_commit(&first_staging.staging_id, Some(0))
        .expect("activate first inventory");
    let lease = store
        .acquire_revision(first.revision)
        .expect("pin first inventory");

    let replacement_asset = AssetAlias {
        object_hash: Some("44".repeat(32)),
        name: "Current asset".to_owned(),
        ..shared_asset.clone()
    };
    let replacement_cold = ColdAlias {
        object_hash: Some("55".repeat(32)),
        metadata: json!({ "kind": "current" }),
        ..cold_a.clone()
    };
    let second_staging = store.replace_begin().expect("begin second inventory");
    store
        .replace_put_asset_aliases(
            &second_staging.staging_id,
            std::slice::from_ref(&replacement_asset),
        )
        .expect("stage replacement asset inventory");
    store
        .replace_put_cold_aliases(
            &second_staging.staging_id,
            std::slice::from_ref(&replacement_cold),
        )
        .expect("stage replacement cold inventory");
    let second = store
        .replace_commit(&second_staging.staging_id, Some(first.revision))
        .expect("activate second inventory");

    assert_eq!(
        store
            .list_asset_aliases(Some(&lease.lease))
            .expect("list pinned assets"),
        super::Versioned {
            revision: first.revision,
            value: vec![shared_asset.clone(), z_asset, shared_inlay.clone()],
        }
    );
    assert_eq!(
        store
            .list_cold_aliases(Some(&lease.lease))
            .expect("list pinned cold aliases"),
        super::Versioned {
            revision: first.revision,
            value: vec![cold_a.clone(), cold_z],
        }
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", "shared", Some(&lease.lease))
            .expect("read pinned Inlay")
            .expect("pinned Inlay exists")
            .value,
        shared_inlay
    );
    assert_eq!(
        store
            .read_asset_alias("asset", "shared", None)
            .expect("read current asset")
            .expect("current asset exists"),
        super::Versioned {
            revision: second.revision,
            value: replacement_asset.clone(),
        }
    );
    assert_eq!(
        store
            .read_cold_alias("a-first", Some(&lease.lease))
            .expect("read pinned cold alias")
            .expect("pinned cold alias exists")
            .value,
        cold_a
    );
    assert_eq!(
        store
            .read_cold_alias("a-first", None)
            .expect("read current cold alias")
            .expect("current cold alias exists"),
        super::Versioned {
            revision: second.revision,
            value: replacement_cold,
        }
    );
}

#[test]
fn cold_aliases_follow_copy_on_write_without_leaking_between_revisions() {
    let directory = tempfile::tempdir().expect("create cold COW directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cold = ColdAlias {
        key: "cold/cow".to_owned(),
        object_hash: Some("66".repeat(32)),
        size: 6,
        metadata: json!({ "scope": "both revisions" }),
    };
    let staging = store.replace_begin().expect("begin cold COW fixture");
    store
        .replace_put_cold_aliases(&staging.staging_id, std::slice::from_ref(&cold))
        .expect("stage cold COW fixture");
    let first = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate cold COW fixture");
    let lease = store
        .acquire_revision(first.revision)
        .expect("pin cold COW fixture");
    let second = store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "copy-on-write" })),
            ..empty_working_set_commit(first.revision)
        })
        .expect("commit copy-on-write revision");

    assert_eq!(
        store
            .read_cold_alias(&cold.key, None)
            .expect("read current copied cold alias"),
        Some(super::Versioned {
            revision: second.revision,
            value: cold.clone(),
        })
    );
    assert_eq!(
        store
            .read_cold_alias(&cold.key, Some(&lease.lease))
            .expect("read pinned cold alias"),
        Some(super::Versioned {
            revision: first.revision,
            value: cold.clone(),
        })
    );
    store
        .release_revision(&lease.lease)
        .expect("release pinned cold revision");
    assert_eq!(
        store
            .read_cold_alias(&cold.key, None)
            .expect("read current cold alias after lease release")
            .expect("current cold alias remains")
            .value,
        cold
    );
}

#[test]
fn asset_alias_overwrite_isolated_by_revision_lease() {
    let (_directory, mut store, _) = open_fixture();
    let original = AssetAlias {
        key: "assets/shared.bin".to_owned(),
        object_hash: Some("11".repeat(32)),
        kind: "asset".to_owned(),
        size: 3,
        mime: "application/octet-stream".to_owned(),
        name: "Shared Original".to_owned(),
        ext: "BIN".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store
        .commit_asset_alias(&original, 1)
        .expect("commit original alias");
    let lease = store
        .acquire_revision(first.revision)
        .expect("acquire alias revision");
    let replacement = AssetAlias {
        key: original.key.clone(),
        object_hash: Some("22".repeat(32)),
        kind: "asset".to_owned(),
        size: 7,
        mime: "application/octet-stream".to_owned(),
        name: "Shared Replacement".to_owned(),
        ext: "BIN".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let second = store
        .commit_asset_alias(&replacement, first.revision)
        .expect("commit replacement alias");

    assert_eq!(
        store
            .read_asset_alias("asset", &original.key, None)
            .expect("read current alias"),
        Some(super::Versioned {
            revision: second.revision,
            value: replacement,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", &original.key, None)
            .expect("read absent sibling Inlay alias"),
        None
    );
    assert_eq!(
        store
            .read_asset_alias("asset", &original.key, Some(&lease.lease))
            .expect("read leased alias"),
        Some(super::Versioned {
            revision: first.revision,
            value: original,
        })
    );
}

#[test]
fn asset_alias_batch_lookup_preserves_kind_and_exact_revision_lease() {
    let (_directory, mut store, _) = open_fixture();
    let asset = AssetAlias {
        key: "assets/batch-shared.bin".to_owned(),
        object_hash: Some("31".repeat(32)),
        kind: "asset".to_owned(),
        size: 3,
        mime: "application/octet-stream".to_owned(),
        name: "Batch asset".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let inlay = AssetAlias {
        key: asset.key.clone(),
        object_hash: Some("32".repeat(32)),
        kind: "inlay".to_owned(),
        size: 4,
        mime: "image/webp".to_owned(),
        name: "Batch inlay".to_owned(),
        ext: "webp".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store
        .commit_asset_alias(&asset, 1)
        .expect("commit batch asset");
    let second = store
        .commit_asset_alias(&inlay, first.revision)
        .expect("commit batch inlay");
    let lease = store
        .acquire_revision(second.revision)
        .expect("acquire batch revision");
    let replacement = AssetAlias {
        object_hash: Some("33".repeat(32)),
        size: 5,
        ..asset.clone()
    };
    let third = store
        .commit_asset_alias(&replacement, second.revision)
        .expect("commit batch replacement");
    let keys = vec![asset.key.clone(), "assets/missing.bin".to_owned()];

    assert_eq!(
        store
            .read_asset_aliases_by_keys("asset", &keys, None)
            .expect("read current batch"),
        super::Versioned {
            revision: third.revision,
            value: vec![replacement],
        }
    );
    assert_eq!(
        store
            .read_asset_aliases_by_keys("inlay", &[asset.key.clone()], None)
            .expect("read sibling kind batch"),
        super::Versioned {
            revision: third.revision,
            value: vec![inlay],
        }
    );
    assert_eq!(
        store
            .read_asset_aliases_by_keys("asset", &keys, Some(&lease.lease))
            .expect("read leased batch"),
        super::Versioned {
            revision: second.revision,
            value: vec![asset],
        }
    );
}

#[test]
fn asset_alias_batch_lookup_accepts_512_unique_keys_and_rejects_invalid_batches() {
    let (_directory, store, _) = open_fixture();
    let keys = (0..512)
        .map(|index| format!("assets/batch-{index}.bin"))
        .collect::<Vec<_>>();

    assert_eq!(
        store
            .read_asset_aliases_by_keys("asset", &keys, None)
            .expect("accept 512 batch keys")
            .value,
        Vec::<AssetAlias>::new()
    );
    assert!(matches!(
        store.read_asset_aliases_by_keys("asset", &[], None),
        Err(StoreError::Validation { .. })
    ));
    assert!(matches!(
        store.read_asset_aliases_by_keys(
            "asset",
            &["duplicate".to_owned(), "duplicate".to_owned()],
            None,
        ),
        Err(StoreError::Validation { .. })
    ));
    let mut too_many = keys;
    too_many.push("assets/batch-512.bin".to_owned());
    assert!(matches!(
        store.read_asset_aliases_by_keys("asset", &too_many, None),
        Err(StoreError::Validation { .. })
    ));
    assert!(matches!(
        store.read_asset_aliases_by_keys("invalid", &["assets/valid.bin".to_owned()], None),
        Err(StoreError::Validation { .. })
    ));
}

#[test]
fn staged_asset_aliases_activate_with_zero_and_missing_payloads() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    let zero_byte = AssetAlias {
        key: "assets/empty.bin".to_owned(),
        object_hash: Some(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
        ),
        kind: "asset".to_owned(),
        size: 0,
        mime: "application/octet-stream".to_owned(),
        name: String::new(),
        ext: String::new(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let missing_payload = AssetAlias {
        key: "inlay/missing".to_owned(),
        object_hash: None,
        kind: "inlay".to_owned(),
        size: 0,
        mime: "image/png".to_owned(),
        name: "Missing payload".to_owned(),
        ext: "PNG".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(0),
        height: Some(0),
        metadata: json!({}),
    };
    let duplicate_bytes_alias = AssetAlias {
        key: "assets/empty-copy.dat".to_owned(),
        name: "Empty Copy".to_owned(),
        ext: "DAT".to_owned(),
        ..zero_byte.clone()
    };

    store
        .replace_put_asset_aliases(
            &staging.staging_id,
            &[
                zero_byte.clone(),
                duplicate_bytes_alias.clone(),
                missing_payload.clone(),
            ],
        )
        .expect("stage asset aliases");
    let committed = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("activate staged aliases");
    drop(store);
    let store = PersistentStore::open(directory.path()).expect("reopen persistent store");

    assert_eq!(
        store
            .read_asset_alias("asset", &zero_byte.key, None)
            .expect("read zero-byte alias"),
        Some(super::Versioned {
            revision: committed.revision,
            value: zero_byte,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", &missing_payload.key, None)
            .expect("read missing-payload alias"),
        Some(super::Versioned {
            revision: committed.revision,
            value: missing_payload,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("asset", &duplicate_bytes_alias.key, None)
            .expect("read duplicate-byte alias"),
        Some(super::Versioned {
            revision: committed.revision,
            value: duplicate_bytes_alias,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("asset", "assets/not-present.bin", None)
            .expect("read absent alias"),
        None
    );
}

#[test]
fn working_set_asset_alias_batch_is_atomic_with_imported_owners() {
    let (_directory, mut store, database) = open_fixture();
    let mut imported_root = root(&database);
    imported_root["modules"] = json!([{
        "id": "native-module",
        "name": "Native module",
        "description": "",
        "assets": [["module asset", "assets/native-module.bin", "BIN"]]
    }]);
    let mut character = database["characters"][0].clone();
    character["chaId"] = json!("native-character");
    character["name"] = json!("Native character");
    character["additionalAssets"] =
        json!([["character asset", "assets/native-character.bin", "BIN"]]);
    let aliases = vec![
        AssetAlias {
            key: "assets/native-character.bin".to_owned(),
            object_hash: Some("91".repeat(32)),
            kind: "asset".to_owned(),
            size: 4,
            mime: "application/octet-stream".to_owned(),
            name: "native-character.bin".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
        AssetAlias {
            key: "assets/native-module.bin".to_owned(),
            object_hash: Some("92".repeat(32)),
            kind: "asset".to_owned(),
            size: 5,
            mime: "application/octet-stream".to_owned(),
            name: "native-module.bin".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        },
    ];
    let heads = vec![
        AssetOwnerHead::present(
            AssetOwnerLocator::CharacterAdditionalAssets {
                character_id: "native-character".to_owned(),
            },
            "93".repeat(32),
            1,
        ),
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "94".repeat(32),
            1,
        ),
    ];
    let committed = store
        .commit_with_asset_aliases(
            &WorkingSetCommit {
                root: Some(imported_root),
                add_character: Some(character),
                asset_owner_heads: Some(heads.clone()),
                ..empty_working_set_commit(1)
            },
            &aliases,
        )
        .expect("commit imported content atomically");

    assert_eq!(
        store
            .read_character("native-character", None)
            .expect("read imported character")
            .expect("imported character exists")
            .revision,
        committed.revision
    );
    for alias in &aliases {
        assert_eq!(
            store
                .read_asset_alias(&alias.kind, &alias.key, None)
                .expect("read imported alias"),
            Some(super::Versioned {
                revision: committed.revision,
                value: alias.clone(),
            })
        );
    }
    for head in &heads {
        assert_eq!(
            store
                .read_asset_owner_head(&head.owner, None)
                .expect("read imported owner head"),
            Some(super::Versioned {
                revision: committed.revision,
                value: head.clone(),
            })
        );
    }

    let before = store
        .materialize(None)
        .expect("materialize before rejection");
    let invalid = AssetAlias {
        object_hash: Some("INVALID".to_owned()),
        ..aliases[0].clone()
    };
    let error = store
        .commit_with_asset_aliases(
            &WorkingSetCommit {
                root: Some(json!({ "username": "must not commit" })),
                add_character: Some(json!({
                    "chaId": "rejected-native-character",
                    "name": "Rejected native character",
                    "chats": []
                })),
                ..empty_working_set_commit(committed.revision)
            },
            &[invalid],
        )
        .expect_err("reject invalid imported alias batch");
    assert!(matches!(error, StoreError::Validation { .. }));
    assert_eq!(
        store
            .materialize(None)
            .expect("materialize after rejection"),
        before
    );
    assert!(store
        .read_character("rejected-native-character", None)
        .expect("read rejected character")
        .is_none());
}

#[test]
fn payload_alias_abort_and_reopen_sweep_remove_all_staged_rows() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let alias = AssetAlias {
        key: "assets/staged.bin".to_owned(),
        object_hash: Some("66".repeat(32)),
        kind: "asset".to_owned(),
        size: 6,
        mime: "application/octet-stream".to_owned(),
        name: "Staged".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let cold = ColdAlias {
        key: alias.key.clone(),
        object_hash: Some("77".repeat(32)),
        size: 7,
        metadata: json!({ "retained": true }),
    };
    let aborted = store.replace_begin().expect("begin aborted replacement");
    store
        .replace_put_asset_aliases(&aborted.staging_id, std::slice::from_ref(&alias))
        .expect("stage aborted alias");
    store
        .replace_put_cold_aliases(&aborted.staging_id, std::slice::from_ref(&cold))
        .expect("stage aborted cold alias");
    store
        .replace_abort(&aborted.staging_id)
        .expect("abort staged aliases");
    let aborted_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM asset_aliases WHERE generation = ?1",
            [&aborted.staging_id],
            |row| row.get(0),
        )
        .expect("count aborted alias rows");
    assert_eq!(aborted_rows, 0);
    let aborted_cold_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM cold_aliases WHERE generation = ?1",
            [&aborted.staging_id],
            |row| row.get(0),
        )
        .expect("count aborted cold alias rows");
    assert_eq!(aborted_cold_rows, 0);

    let abandoned = store.replace_begin().expect("begin abandoned replacement");
    store
        .replace_put_asset_aliases(&abandoned.staging_id, &[alias])
        .expect("stage abandoned alias");
    store
        .replace_put_cold_aliases(&abandoned.staging_id, &[cold])
        .expect("stage abandoned cold alias");
    drop(store);
    let store = PersistentStore::open(directory.path()).expect("reopen persistent store");
    let abandoned_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM asset_aliases WHERE generation = ?1",
            [&abandoned.staging_id],
            |row| row.get(0),
        )
        .expect("count swept alias rows");
    assert_eq!(abandoned_rows, 0);
    let abandoned_cold_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM cold_aliases WHERE generation = ?1",
            [&abandoned.staging_id],
            |row| row.get(0),
        )
        .expect("count swept cold alias rows");
    assert_eq!(abandoned_cold_rows, 0);
}

#[test]
fn invalid_asset_aliases_leave_revision_and_staging_rows_unchanged() {
    let valid = AssetAlias {
        key: "assets/valid.bin".to_owned(),
        object_hash: Some("99".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Valid".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let invalid_hash = AssetAlias {
        key: "assets/invalid.bin".to_owned(),
        object_hash: Some("INVALID".to_owned()),
        ..valid.clone()
    };
    let inlay_without_type = AssetAlias {
        key: "inlay/invalid.bin".to_owned(),
        kind: "inlay".to_owned(),
        ..valid.clone()
    };
    let asset_with_inlay_metadata = AssetAlias {
        key: "assets/invalid-metadata.bin".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(1),
        height: Some(1),
        ..valid.clone()
    };
    let invalid_lossless_metadata = AssetAlias {
        key: "assets/invalid-lossless-metadata.bin".to_owned(),
        metadata: json!(["not", "an", "object"]),
        ..valid.clone()
    };

    for invalid in [
        &invalid_hash,
        &inlay_without_type,
        &asset_with_inlay_metadata,
        &invalid_lossless_metadata,
    ] {
        let directory = tempfile::tempdir().expect("create temporary directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        assert!(store.commit_asset_alias(invalid, 0).is_err());
        assert_eq!(store.revision().expect("read unchanged revision"), 0);
        assert_eq!(
            store
                .read_asset_alias("asset", &invalid.key, None)
                .expect("read rejected current alias"),
            None
        );
    }

    let directory = tempfile::tempdir().expect("create staging directory");
    let mut store = PersistentStore::open(directory.path()).expect("open staging store");
    let staging = store.replace_begin().expect("begin staged replacement");
    assert!(store
        .replace_put_asset_aliases(&staging.staging_id, &[valid, invalid_hash])
        .is_err());
    let staged_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM asset_aliases WHERE generation = ?1",
            [&staging.staging_id],
            |row| row.get(0),
        )
        .expect("count rejected staged aliases");
    assert_eq!(staged_rows, 0);
    assert_eq!(store.revision().expect("read staged failure revision"), 0);
}

#[test]
fn invalid_cold_alias_batch_leaves_staging_and_revision_unchanged() {
    let directory = tempfile::tempdir().expect("create cold validation directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let valid = ColdAlias {
        key: "cold/valid".to_owned(),
        object_hash: Some("aa".repeat(32)),
        size: 1,
        metadata: json!({ "type": "memory" }),
    };
    let invalid_cases = [
        ColdAlias {
            key: String::new(),
            ..valid.clone()
        },
        ColdAlias {
            key: "cold\0invalid".to_owned(),
            ..valid.clone()
        },
        ColdAlias {
            object_hash: Some("AA".repeat(32)),
            ..valid.clone()
        },
        ColdAlias {
            size: -1,
            ..valid.clone()
        },
        ColdAlias {
            metadata: json!(["not", "an", "object"]),
            ..valid.clone()
        },
    ];

    for invalid in invalid_cases {
        let staging = store.replace_begin().expect("begin invalid cold batch");
        assert!(store
            .replace_put_cold_aliases(&staging.staging_id, &[valid.clone(), invalid])
            .is_err());
        let rows: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM cold_aliases WHERE generation = ?1",
                [&staging.staging_id],
                |row| row.get(0),
            )
            .expect("count rejected cold batch");
        assert_eq!(rows, 0);
        assert_eq!(store.revision().expect("read unchanged revision"), 0);
        store
            .replace_abort(&staging.staging_id)
            .expect("abort rejected cold staging");
    }
}

#[test]
fn corrupt_persisted_asset_alias_fails_direct_lookup_and_integrity_check() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", true)
        .expect("disable alias check constraints for corruption fixture");
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext
             ) VALUES ('revision-0', 'assets/corrupt.bin', 'CORRUPT', 'asset', 1,
                       'application/octet-stream', 'Corrupt', 'bin')",
            [],
        )
        .expect("insert corrupt alias fixture");
    store
        .connection
        .pragma_update(None, "ignore_check_constraints", false)
        .expect("restore alias check constraints");

    assert!(matches!(
        store.read_asset_alias("asset", "assets/corrupt.bin", None),
        Err(StoreError::Validation { .. })
    ));
    assert!(matches!(
        store.read_asset_aliases_by_keys("asset", &["assets/corrupt.bin".to_owned()], None,),
        Err(StoreError::Validation { .. })
    ));
    let integrity: String = store
        .connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity check");
    assert_ne!(integrity, "ok");
}

#[test]
fn asset_alias_direct_lookup_is_scoped_to_generation_and_logical_key() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext
             ) VALUES
                ('revision-other', 'assets/requested.bin', NULL, 'asset', 0,
                 'application/octet-stream', 'Other generation', 'bin'),
                ('revision-0', 'assets/other.bin', NULL, 'asset', 0,
                 'application/octet-stream', 'Other key', 'bin');",
        )
        .expect("insert direct lookup scope fixtures");

    assert_eq!(
        store
            .read_asset_alias("asset", "assets/requested.bin", None)
            .expect("read requested alias"),
        None
    );
    assert_eq!(
        store
            .read_asset_alias("asset", "assets/other.bin", None)
            .expect("read active other alias")
            .expect("active other alias exists")
            .value
            .key,
        "assets/other.bin"
    );
}

#[test]
fn native_export_lease_retains_its_asset_alias_generation() {
    let (_directory, mut store, _) = open_fixture();
    let original = AssetAlias {
        key: "assets/export.bin".to_owned(),
        object_hash: Some("aa".repeat(32)),
        kind: "asset".to_owned(),
        size: 2,
        mime: "application/original".to_owned(),
        name: "Original".to_owned(),
        ext: "old".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store
        .commit_asset_alias(&original, 1)
        .expect("commit export alias");
    let lease = store
        .acquire_revision(first.revision)
        .expect("acquire export alias lease");
    let replacement = AssetAlias {
        object_hash: Some("bb".repeat(32)),
        mime: "application/replacement".to_owned(),
        name: "Replacement".to_owned(),
        ext: "new".to_owned(),
        ..original.clone()
    };
    store
        .commit_asset_alias(&replacement, first.revision)
        .expect("overwrite export alias");

    let exported = store
        .export_risu_save(&lease.lease, false)
        .expect("export leased revision");

    assert!(Path::new(&exported.path).is_file());
    assert_eq!(
        store
            .read_asset_alias("asset", &original.key, Some(&lease.lease))
            .expect("read export alias lease"),
        Some(super::Versioned {
            revision: first.revision,
            value: original,
        })
    );
    store
        .cleanup_risu_save_export(Path::new(&exported.path))
        .expect("cleanup native export");
}

#[cfg(feature = "native-official-publication")]
#[test]
fn official_publication_transfers_only_the_exact_expected_revision_lease() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store
        .acquire_revision(1)
        .expect("acquire publication lease")
        .lease;

    let mismatch = match store.prepare_official_publication(&lease, 2) {
        Err(error) => error,
        Ok(_) => panic!("accepted revision mismatch"),
    };
    assert!(matches!(mismatch, StoreError::RevisionConflict { .. }));
    assert_eq!(
        store
            .read_root(Some(&lease))
            .expect("mismatch keeps attached lease")
            .revision,
        1
    );

    let prepared = store
        .prepare_official_publication(&lease, 1)
        .expect("transfer exact lease");
    assert!(matches!(
        store.read_root(Some(&lease)),
        Err(StoreError::SnapshotReleased)
    ));
    drop(prepared);
}

#[cfg(feature = "native-official-publication")]
#[test]
fn official_publication_seals_exact_owner_and_direct_roots_before_reader_transfer() {
    use crate::asset_repository::job_pins::{CasReleaseOutcome, DurableCasJob};

    let directory = tempfile::tempdir().expect("create publication store");
    let mut store = PersistentStore::open(directory.path()).expect("open publication store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let owner = cas
        .prepare_bytes(b"owner-manifest")
        .expect("prepare owner manifest");
    let direct = cas
        .prepare_bytes(b"direct-object")
        .expect("prepare direct object");
    let historical_owner = cas
        .prepare_bytes(b"historical-owner-manifest")
        .expect("prepare historical owner manifest");
    let historical_direct = cas
        .prepare_bytes(b"historical-direct-object")
        .expect("prepare historical direct object");
    let generation = super::active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', '0', 1, ?2, 1)",
            rusqlite::params![&generation, &owner.content_hash],
        )
        .expect("insert owner root");
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES ('historical-generation', 'root-module-assets', '0', 1, ?1, 1)",
            [&historical_owner.content_hash],
        )
        .expect("insert historical owner root");
    for (key, hash, size) in [
        (
            "assets/owner-overlap.bin",
            &owner.content_hash,
            owner.byte_size,
        ),
        ("assets/direct.bin", &direct.content_hash, direct.byte_size),
    ] {
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                    generation, logical_key, object_hash, kind, size, mime, name, ext,
                    inlay_type, width, height, metadata
                 ) VALUES (?1, ?2, ?3, 'asset', ?4,
                    'application/octet-stream', ?2, 'bin', NULL, NULL, NULL, '{}')",
                rusqlite::params![&generation, key, hash, size as i64],
            )
            .expect("insert direct root");
    }
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES ('historical-generation', 'assets/historical.bin', ?1, 'asset', ?2,
                'application/octet-stream', 'historical.bin', 'bin', NULL, NULL, NULL, '{}')",
            rusqlite::params![
                &historical_direct.content_hash,
                historical_direct.byte_size as i64
            ],
        )
        .expect("insert historical direct root");
    let lease = store
        .acquire_revision(0)
        .expect("acquire exact revision")
        .lease;
    let exports = directory.path().join("persistent").join("exports");

    let prepared = store
        .prepare_official_publication_for_job(&lease, 0, "publication-root-job", 1)
        .expect("prepare durable publication");

    let durable = DurableCasJob::open(directory.path(), "publication-root-job")
        .expect("open sealed publication journal");
    assert!(durable.is_sealed());
    assert_eq!(
        durable.root_set().expect("read durable roots"),
        crate::asset_repository::migration_gc::AssetRootSet {
            manifest_hashes: [owner.content_hash.clone()].into(),
            object_hashes: [direct.content_hash.clone()].into(),
            ..Default::default()
        }
    );
    assert_eq!(store.active_readers.active_count(), 1);
    assert!(matches!(
        store.read_root(Some(&lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert!(!exports.exists());

    drop(prepared);
    assert_eq!(store.active_readers.active_count(), 0);
    assert!(DurableCasJob::open(directory.path(), "publication-root-job").is_ok());
    let mut durable = DurableCasJob::open(directory.path(), "publication-root-job")
        .expect("reopen publication journal");
    durable
        .release(CasReleaseOutcome::Aborted)
        .expect("release publication fixture");
}

#[cfg(feature = "native-official-publication")]
#[test]
fn official_publication_payload_closes_its_reader_and_reopens_exact_managed_bytes() {
    let directory = tempfile::tempdir().expect("create publication store");
    let mut store = PersistentStore::open(directory.path()).expect("open publication store");
    let staging = store.replace_begin().expect("begin publication fixture");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({
                "account": { "id": "account-1", "token": "not-pinned" },
                "customBackground": "local-asset"
            }),
        )
        .expect("stage publication root");
    store
        .replace_put_presets(&staging.staging_id, &[])
        .expect("stage publication presets");
    let revision = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit publication fixture")
        .revision;
    let lease = store
        .acquire_revision(revision)
        .expect("acquire publication lease")
        .lease;
    let prepared = store
        .prepare_official_publication(&lease, revision)
        .expect("transfer publication lease");
    let payload = prepared
        .create_payload(
            "account-1",
            &std::collections::HashMap::from([(
                "local-asset".to_owned(),
                "remote-asset".to_owned(),
            )]),
            || false,
            |_, _, _| {},
        )
        .expect("create publication payload");

    assert_eq!(store.active_readers.active_count(), 0);
    assert!(store
        .active_readers
        .detached_asset_roots()
        .expect("read detached publication roots")
        .is_empty());
    assert!(matches!(
        store.read_root(Some(&lease)),
        Err(StoreError::SnapshotReleased)
    ));
    let (mut reopened, bytes) = payload.open().expect("reopen managed payload");
    let mut body = Vec::new();
    reopened
        .read_to_end(&mut body)
        .expect("read managed payload");
    assert_eq!(bytes, payload.bytes);
    assert_eq!(body.len() as u64, payload.bytes);
    assert_eq!(hex::encode(Sha256::digest(&body)), payload.sha256);
    let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
    assert_eq!(fs::read_dir(&exports_dir).unwrap().count(), 2);
    payload.cleanup().expect("cleanup managed payload pair");
    assert_eq!(fs::read_dir(exports_dir).unwrap().count(), 0);
}

#[cfg(feature = "native-official-publication")]
#[test]
fn exact_payload_hash_stops_on_cancellation_without_consuming_the_source() {
    let directory = tempfile::tempdir().expect("create hash fixture");
    let path = directory.path().join("payload.risudat");
    fs::write(&path, vec![7u8; 128 * 1024]).expect("write hash fixture");
    let cancelled = AtomicBool::new(true);

    let error = super::hash_exact_file(&path, &|| cancelled.load(AtomicOrdering::Acquire))
        .expect_err("cancel payload hash");

    assert!(matches!(error, StoreError::Validation { .. }));
    assert!(path.is_file());
}

#[cfg(feature = "native-official-publication")]
#[test]
fn official_publication_hash_cancellation_closes_reader_and_cleans_managed_pair() {
    let directory = tempfile::tempdir().expect("create publication store");
    let mut store = PersistentStore::open(directory.path()).expect("open publication store");
    let staging = store.replace_begin().expect("begin publication fixture");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "account": { "id": "account-1" } }),
        )
        .expect("stage publication root");
    store
        .replace_put_presets(&staging.staging_id, &[])
        .expect("stage publication presets");
    let revision = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit publication fixture")
        .revision;
    let lease = store
        .acquire_revision(revision)
        .expect("acquire publication lease")
        .lease;
    let active_readers = Arc::clone(&store.active_readers);
    let prepared = store
        .prepare_official_publication(&lease, revision)
        .expect("transfer publication lease");

    let error = prepared
        .create_payload(
            "account-1",
            &std::collections::HashMap::new(),
            || active_readers.active_count() == 0,
            |_, _, _| {},
        )
        .expect_err("cancel after reader release before hashing");

    assert!(matches!(error, StoreError::Validation { .. }));
    assert_eq!(store.active_readers.active_count(), 0);
    assert!(store
        .active_readers
        .detached_asset_roots()
        .expect("read roots after cancelled hash")
        .is_empty());
    assert!(matches!(
        store.read_root(Some(&lease)),
        Err(StoreError::SnapshotReleased)
    ));
    let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
    assert_eq!(fs::read_dir(exports_dir).unwrap().count(), 0);
}

#[cfg(feature = "native-official-publication")]
#[test]
fn pinned_publication_account_mismatch_releases_reader_and_cleans_export_files() {
    let directory = tempfile::tempdir().expect("create publication store");
    let mut store = PersistentStore::open(directory.path()).expect("open publication store");
    let staging = store.replace_begin().expect("begin publication fixture");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "account": { "id": "account-1" } }),
        )
        .expect("stage publication root");
    store
        .replace_put_presets(&staging.staging_id, &[])
        .expect("stage publication presets");
    let revision = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit publication fixture")
        .revision;
    let lease = store
        .acquire_revision(revision)
        .expect("acquire publication lease")
        .lease;
    let prepared = store
        .prepare_official_publication(&lease, revision)
        .expect("transfer publication lease");

    let error = prepared
        .create_payload(
            "different-account",
            &std::collections::HashMap::new(),
            || false,
            |_, _, _| {},
        )
        .expect_err("reject mismatched pinned account");

    assert!(matches!(error, StoreError::Validation { .. }));
    assert_eq!(store.active_readers.active_count(), 0);
    assert!(store
        .active_readers
        .detached_asset_roots()
        .expect("read roots after account mismatch")
        .is_empty());
    let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
    assert_eq!(fs::read_dir(exports_dir).unwrap().count(), 0);
}
