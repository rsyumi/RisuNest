use super::{
    AssetAlias, AssetAliasListQuery, AssetOwnerHead, AssetOwnerLocator,
    AssetRepositoryAuthorityState, CharacterQuery, CheckpointMode, ColdAlias,
    ColdPayloadAuthorityState, ColdPayloadMigrationInput, ConversationMutation, ConversationPage,
    ConversationQuery, ConversationWindowQuery, PersistentStore, PluginStorageMutation, QueryOrder,
    StoreError, WorkingSetCommit,
};
use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
    thread,
    time::Duration,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json"))
        .expect("parse persistent store fixture")
}

fn staged_root(database: &Value) -> Value {
    let mut root = database.clone();
    let root = root.as_object_mut().expect("fixture database object");
    root.remove("characters");
    root.remove("botPresets");
    Value::Object(root.clone())
}

fn root(database: &Value) -> Value {
    let mut root = staged_root(database);
    let root = root.as_object_mut().expect("fixture staged root object");
    root.remove("pluginCustomStorage");
    Value::Object(root.clone())
}

fn open_fixture() -> (tempfile::TempDir, PersistentStore, Value) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let database = fixture();
    let staging = store.replace_begin().expect("begin staged replacement");
    let root = staged_root(&database);
    let characters = database["characters"]
        .as_array()
        .expect("fixture characters");
    store
        .replace_put_root(&staging.staging_id, &root)
        .expect("stage fixture root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage fixture presets");
    store
        .replace_add_characters(&staging.staging_id, characters)
        .expect("stage fixture characters");
    assert_eq!(
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("commit fixture")
            .revision,
        1
    );
    (directory, store, database)
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
            expected_revision: 1,
            root: Some(first_root.clone()),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(original_heads.clone()),
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
            expected_revision: shadowed.revision,
            root: Some(reordered_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(reordered_heads.clone()),
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
            expected_revision: 1,
            root: Some(original_root.clone()),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![valid_head.clone()]),
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
            expected_revision: committed.revision,
            root: Some(rejected_root.clone()),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![invalid_head]),
        }),
        Err(StoreError::Validation { .. })
    ));
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(rejected_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![valid_head.clone()]),
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
fn character_parent_change_invalidates_omitted_owner_head() {
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
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: Some(detail.clone()),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![head]),
        })
        .expect("commit character owner head");
    assert!(store
        .read_asset_owner_head(&owner, None)
        .expect("read owner head")
        .is_some());
    detail["name"] = json!("Changed through legacy path");
    let changed = store
        .commit(&WorkingSetCommit {
            expected_revision: shadowed.revision,
            root: None,
            replace_presets: None,
            character: Some(detail),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        })
        .expect("commit legacy character change");

    assert_eq!(store.revision().expect("read revision"), changed.revision);
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read invalidated owner head"),
        None
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
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: Some(vec![earlier_detail.clone()]),
            replace_character: Some(final_character.clone()),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![final_head.clone()]),
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
            expected_revision: committed.revision,
            root: Some(rejected_root),
            replace_presets: None,
            character: None,
            character_details: Some(vec![earlier_detail]),
            replace_character: Some(final_character),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![earlier_head]),
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
            expected_revision: 1,
            root: Some(database_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: Some(vec![head.clone(), absent_head.clone()]),
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
            expected_revision: first.revision,
            root: Some(json!({ "username": "copy-on-write" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
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
                expected_revision: 1,
                root: Some(imported_root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: Some(character),
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: Some(heads.clone()),
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
                expected_revision: committed.revision,
                root: Some(json!({ "username": "must not commit" })),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: Some(json!({
                    "chaId": "rejected-native-character",
                    "name": "Rejected native character",
                    "chats": []
                })),
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
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

#[test]
fn preset_catalog_reads_and_materializes_in_configured_order() {
    let (_directory, mut store, database) = open_fixture();

    assert!(store
        .read_root(None)
        .expect("read root")
        .value
        .get("botPresets")
        .is_none());
    assert_eq!(
        serde_json::to_value(store.query_presets(None).expect("query presets"))
            .expect("serialize preset catalog"),
        json!({
            "revision": 1,
            "items": [
                { "id": "0", "name": "Preset Beta", "image": "preset-beta.png", "configuredIndex": 0 },
                { "id": "1", "name": "Preset Alpha", "configuredIndex": 1 }
            ]
        })
    );
    assert_eq!(
        store
            .read_preset("1", None)
            .expect("read preset")
            .expect("preset exists")
            .value,
        database["botPresets"][1]
    );

    let lease = store.acquire_revision(1).expect("acquire preset lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Preset commit", "botPresets": ["strip"] })),
            replace_presets: Some(vec![json!({ "name": "Replacement" })]),
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("replace presets");
    assert_eq!(
        store.materialize(None).expect("materialize replacement")["botPresets"],
        json!([{ "name": "Replacement" }])
    );
    assert_eq!(
        store
            .query_presets(Some(&lease.lease))
            .expect("query leased presets")
            .items[0]
            .name,
        "Preset Beta"
    );
    store
        .release_revision(&lease.lease)
        .expect("release preset lease");
    assert!(matches!(
        store.query_presets(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn plugin_storage_is_revisioned_per_key_and_lease_isolated() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut database = fixture();
    database["pluginCustomStorage"] = json!({
        "alpha": "old",
        "beta": { "enabled": true }
    });
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&database))
        .expect("stage plugin storage");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage characters");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit plugin storage");

    assert!(store
        .read_root(None)
        .expect("read stripped root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        serde_json::to_value(
            store
                .query_plugin_storage(None)
                .expect("query plugin storage")
        )
        .expect("serialize plugin catalog"),
        json!({
            "revision": 1,
            "items": [
                { "key": "alpha", "byteSize": 5 },
                { "key": "beta", "byteSize": 16 }
            ]
        })
    );
    assert_eq!(
        store
            .read_plugin_storage("beta", None)
            .expect("read plugin key")
            .expect("plugin key exists")
            .value,
        json!({ "enabled": true })
    );
    let lease = store
        .acquire_revision(imported.revision)
        .expect("acquire plugin lease");

    store
        .commit(&WorkingSetCommit {
            expected_revision: imported.revision,
            root: Some(json!({ "username": "Plugin commit" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "alpha".to_owned(),
                    value: json!("new"),
                },
                PluginStorageMutation::Delete {
                    key: "beta".to_owned(),
                },
            ]),
        })
        .expect("mutate plugin storage");

    assert_eq!(
        store
            .read_plugin_storage("alpha", Some(&lease.lease))
            .expect("read leased plugin key")
            .expect("leased plugin key exists")
            .value,
        json!("old")
    );
    assert_eq!(
        store.materialize(None).expect("materialize plugin storage")["pluginCustomStorage"],
        json!({ "alpha": "new" })
    );
    store
        .release_revision(&lease.lease)
        .expect("release plugin lease");
    assert!(matches!(
        store.read_plugin_storage("alpha", Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn plugin_storage_preserves_legacy_object_key_order_across_reopen() {
    let directory = tempfile::tempdir().expect("create plugin order directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin ordered replacement");
    let mut storage = serde_json::Map::new();
    storage.insert("zeta".to_owned(), json!("first string"));
    storage.insert("10".to_owned(), json!("ten"));
    storage.insert("2".to_owned(), json!(0));
    storage.insert("01".to_owned(), json!("non-index"));
    storage.insert("4294967294".to_owned(), json!(true));
    storage.insert("4294967295".to_owned(), json!(false));
    storage.insert("\u{ffff}x".to_owned(), json!("unicode"));
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": Value::Object(storage) }),
        )
        .expect("stage ordered plugin storage");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit ordered plugin storage");
    let original_order = vec![
        "2",
        "10",
        "4294967294",
        "zeta",
        "01",
        "4294967295",
        "\u{ffff}x",
    ];
    assert_eq!(
        store
            .query_plugin_storage(None)
            .expect("query ordered storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        original_order
    );
    assert_eq!(
        store
            .read_plugin_storage("\u{ffff}x", None)
            .expect("read unicode key")
            .expect("unicode key exists")
            .value,
        json!("unicode")
    );

    let updated = store
        .commit(&WorkingSetCommit {
            expected_revision: imported.revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![
                PluginStorageMutation::Set {
                    key: "zeta".to_owned(),
                    value: json!("updated"),
                },
                PluginStorageMutation::Delete {
                    key: "zeta".to_owned(),
                },
                PluginStorageMutation::Set {
                    key: "zeta".to_owned(),
                    value: json!("reinserted"),
                },
            ]),
        })
        .expect("reinsert string key");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen ordered storage");
    let expected = vec![
        "2",
        "10",
        "4294967294",
        "01",
        "4294967295",
        "\u{ffff}x",
        "zeta",
    ];
    assert_eq!(
        reopened
            .query_plugin_storage(None)
            .expect("query reopened ordered storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        reopened
            .materialize(Some(updated.revision))
            .expect("materialize ordered storage")["pluginCustomStorage"]
            .as_object()
            .expect("plugin storage object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn ordinary_root_commits_do_not_replace_plugin_records_and_empty_materializes() {
    let directory = tempfile::tempdir().expect("create root semantics directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    assert_eq!(
        store.materialize(None).expect("materialize empty store")["pluginCustomStorage"],
        json!({})
    );
    let staging = store.replace_begin().expect("begin plugin replacement");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "pluginCustomStorage": { "retained": 0 } }),
        )
        .expect("stage plugin replacement");
    let imported = store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit plugin replacement");

    store
        .commit(&WorkingSetCommit {
            expected_revision: imported.revision,
            root: Some(json!({
                "username": "ordinary root",
                "pluginCustomStorage": { "incidental": "ignored" }
            })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit ordinary root");

    assert_eq!(
        store
            .materialize(None)
            .expect("materialize retained storage")["pluginCustomStorage"],
        json!({ "retained": 0 })
    );
}

fn message(id: &str) -> Value {
    json!({ "role": "user", "data": id, "chatId": id, "time": 1_800_000_000_000i64 })
}

fn commit(store: &mut PersistentStore, revision: i64, mutation: ConversationMutation) -> i64 {
    store
        .commit(&WorkingSetCommit {
            expected_revision: revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: Some(vec![mutation]),
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit conversation mutation")
        .revision
}

#[test]
fn opens_new_store_at_revision_zero() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");

    assert_eq!(store.revision().expect("read revision"), 0);
    assert!(directory.path().join("persistent/persistent.db").is_file());
}

#[test]
fn staged_fixture_round_trips_through_materialize() {
    let (_directory, store, database) = open_fixture();

    assert_eq!(
        store.read_root(None).expect("read root").value,
        root(&database)
    );
    assert_eq!(
        store.materialize(None).expect("materialize fixture"),
        database
    );
}

#[test]
fn pinned_materialization_is_not_affected_by_active_changes() {
    let (_directory, mut store, database) = open_fixture();
    let lease = store
        .acquire_revision(1)
        .expect("acquire materialization lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Changed after lease" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("change active generation after lease");

    assert_ne!(
        store.materialize(None).expect("materialize active data"),
        database
    );
    assert_eq!(
        store
            .materialize_lease(&lease.lease)
            .expect("materialize pinned generation"),
        database
    );
    store
        .release_revision(&lease.lease)
        .expect("release materialization lease");
    assert!(matches!(
        store.materialize_lease(&lease.lease),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn staged_materialization_is_exact_and_inaccessible_after_abort_or_reopen() {
    let (directory, mut store, active_database) = open_fixture();
    let aborted = store.replace_begin().expect("begin staged materialization");
    store
        .replace_put_root(
            &aborted.staging_id,
            &json!({
                "username": "Staged database",
                "pluginCustomStorage": { "staged": 0 }
            }),
        )
        .expect("write staged materialization root");

    assert_eq!(
        store
            .materialize_staging(&aborted.staging_id)
            .expect("materialize exact staging generation"),
        json!({
            "username": "Staged database",
            "characters": [],
            "botPresets": [],
            "pluginCustomStorage": { "staged": 0 }
        })
    );
    assert_eq!(
        store
            .materialize(None)
            .expect("materialize active generation"),
        active_database
    );

    store
        .replace_abort(&aborted.staging_id)
        .expect("abort staged materialization");
    assert!(matches!(
        store.materialize_staging(&aborted.staging_id),
        Err(StoreError::Validation { .. })
    ));

    let abandoned = store
        .replace_begin()
        .expect("begin abandoned staged materialization");
    store
        .replace_put_root(
            &abandoned.staging_id,
            &json!({ "username": "Abandoned database" }),
        )
        .expect("write abandoned staged root");
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert!(matches!(
        reopened.materialize_staging(&abandoned.staging_id),
        Err(StoreError::Validation { .. })
    ));
}

#[test]
fn character_catalog_honors_order_search_trash_and_cursor() {
    let (_directory, store, _) = open_fixture();
    let configured = |search: Option<&str>, trash, cursor: Option<&str>| CharacterQuery {
        search: search.map(str::to_owned),
        order: QueryOrder::Configured,
        trash,
        limit: 1,
        cursor: cursor.map(str::to_owned),
    };

    let first = store
        .query_characters(&configured(None, false, None), None)
        .expect("first configured page");
    assert_eq!(first.items[0].id, "char-b");
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    assert_eq!(
        store
            .query_characters(&configured(None, false, first.next_cursor.as_deref()), None)
            .expect("second configured page")
            .items[0]
            .id,
        "char-a"
    );
    assert_eq!(
        store
            .query_characters(&configured(Some("LPH"), false, None), None)
            .expect("search catalog")
            .items[0]
            .id,
        "char-a"
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Recent,
                    trash: true,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("trashed catalog")
            .items[0]
            .id,
        "char-c"
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Recent,
                    trash: false,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("recent catalog")
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["char-a", "char-b"]
    );
    let detail = store
        .read_character("char-a", None)
        .expect("read character detail")
        .expect("character exists");
    assert_eq!(detail.value["chaId"], "char-a");
    assert!(detail.value.get("chats").is_none());
}

#[test]
fn character_search_uses_rust_unicode_lowercase_matching() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: Some(json!({
                "type": "character",
                "chaId": "unicode-name",
                "name": "Éclair",
                "chats": []
            })),
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("add character with Unicode name");

    let page = store
        .query_characters(
            &CharacterQuery {
                search: Some("éCL".to_owned()),
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("search Unicode character name");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, "unicode-name");
}

#[test]
fn conversation_catalog_honors_configured_recent_and_cursor() {
    let (_directory, store, _) = open_fixture();
    let query = |order, cursor: Option<&str>| ConversationQuery {
        character_id: "char-a".to_owned(),
        order,
        limit: 1,
        cursor: cursor.map(str::to_owned),
    };

    let first = store
        .query_conversations(&query(QueryOrder::Configured, None), None)
        .expect("first configured conversation page");
    assert_eq!(first.items[0].id, "conv-long");
    assert_eq!(first.next_cursor.as_deref(), Some("1"));
    assert_eq!(
        store
            .query_conversations(
                &query(QueryOrder::Configured, first.next_cursor.as_deref()),
                None,
            )
            .expect("second configured conversation page")
            .items[0]
            .id,
        "conv-short"
    );
    assert_eq!(
        store
            .query_conversations(&query(QueryOrder::Recent, None), None)
            .expect("recent conversation page")
            .items[0]
            .id,
        "conv-long"
    );
    assert_eq!(
        serde_json::to_value(
            store
                .query_conversations(
                    &ConversationQuery {
                        character_id: "missing".to_owned(),
                        order: QueryOrder::Configured,
                        limit: 10,
                        cursor: None,
                    },
                    None,
                )
                .expect("empty conversation page")
        )
        .expect("serialize empty conversation page"),
        json!({ "revision": 1, "items": [] })
    );
}

#[test]
fn conversation_catalog_includes_chat_list_metadata() {
    let (_directory, mut store, _) = open_fixture();
    let mut detail = store
        .read_conversation("char-a", "conv-short", None)
        .expect("read conversation")
        .expect("conversation exists")
        .value;
    let detail = detail.as_object_mut().expect("conversation object");
    detail.remove("message");
    detail.insert("folderId".to_owned(), json!("folder-a"));
    detail.insert("bindedPersona".to_owned(), json!("persona-a"));
    detail.insert("fmIndex".to_owned(), json!(-1));
    commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 0,
            delete_count: 0,
            messages: Vec::new(),
            conversation: Some(Value::Object(detail.clone())),
            configured_index: None,
        },
    );

    let page = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query conversations");
    let summary = page
        .items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("conversation summary");

    assert_eq!(
        serde_json::to_value(summary).expect("serialize summary"),
        json!({
            "id": "conv-short",
            "characterId": "char-a",
            "name": "Short chat",
            "folderId": "folder-a",
            "bindedPersona": "persona-a",
            "configuredIndex": 1,
            "recentAt": 250,
            "messageCount": 2,
            "fmIndex": -1
        })
    );
}

#[test]
fn conversation_windows_cover_latest_and_anchor_boundaries() {
    let (_directory, store, _) = open_fixture();
    let window = |anchor: Option<&str>, before, after| ConversationWindowQuery {
        character_id: "char-a".to_owned(),
        conversation_id: "conv-long".to_owned(),
        start_index: None,
        limit: Some(4),
        anchor_message_id: anchor.map(str::to_owned),
        before,
        after,
    };

    let latest = store
        .read_conversation_window(&window(None, None, None), None)
        .expect("read latest window")
        .expect("long conversation exists");
    assert_eq!(
        (latest.value.start_index, latest.value.end_index),
        (126, 130)
    );
    assert!(latest.value.has_more_before);
    assert!(!latest.value.has_more_after);

    let anchored = store
        .read_conversation_window(&window(Some("msg-127"), Some(2), Some(1)), None)
        .expect("read anchored window")
        .expect("long conversation exists");
    assert_eq!(
        (anchored.value.start_index, anchored.value.end_index),
        (125, 129)
    );
    assert_eq!(anchored.value.messages[2]["chatId"], "msg-127");
    assert!(anchored.value.has_more_before);
    assert!(anchored.value.has_more_after);
}

#[test]
fn conversation_windows_support_strict_absolute_ranges() {
    let (_directory, store, _) = open_fixture();
    let range = |start_index, limit| ConversationWindowQuery {
        character_id: "char-a".to_owned(),
        conversation_id: "conv-long".to_owned(),
        start_index,
        limit,
        anchor_message_id: None,
        before: None,
        after: None,
    };

    for (start_index, limit, expected_start, expected_end, expected_ids) in [
        (0, 3, 0, 3, vec!["msg-000", "msg-001", "msg-002"]),
        (126, 3, 126, 129, vec!["msg-126", "msg-127", "msg-128"]),
        (127, 1, 127, 128, vec!["msg-127"]),
        (128, 10, 128, 130, vec!["msg-128", "msg-129"]),
        (130, 4, 130, 130, vec![]),
        (200, 4, 130, 130, vec![]),
    ] {
        let result = store
            .read_conversation_window(&range(Some(start_index), Some(limit)), None)
            .expect("read absolute range")
            .expect("conversation exists");
        assert_eq!(
            (result.value.start_index, result.value.end_index),
            (expected_start, expected_end)
        );
        assert_eq!(
            result
                .value
                .messages
                .iter()
                .map(|message| message["chatId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_ids
        );
    }

    for invalid in [
        range(Some(-1), Some(1)),
        range(Some(0), None),
        range(Some(0), Some(0)),
        range(Some(0), Some(4_097)),
        ConversationWindowQuery {
            anchor_message_id: Some("msg-000".to_owned()),
            ..range(Some(0), Some(1))
        },
    ] {
        assert!(matches!(
            store.read_conversation_window(&invalid, None),
            Err(StoreError::Validation { .. })
        ));
    }
}

#[test]
fn replace_range_supports_append_insert_delete_and_conversation_lifecycle() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("append")],
            conversation: None,
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 1,
            delete_count: 0,
            messages: vec![message("insert")],
            conversation: None,
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 1,
            messages: vec![],
            conversation: None,
            configured_index: None,
        },
    );

    let short = store
        .read_conversation("char-a", "conv-short", None)
        .expect("read edited conversation")
        .expect("short conversation exists");
    assert_eq!(
        short.value["message"].as_array().expect("messages").len(),
        3
    );
    assert_eq!(short.value["message"][1]["chatId"], "insert");
    assert_eq!(short.value["message"][2]["chatId"], "append");

    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-new".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![message("new-message")],
            conversation: Some(json!({
                "id": "conv-new", "name": "New chat", "note": "", "localLore": []
            })),
            configured_index: None,
        },
    );
    assert!(store
        .read_conversation("char-a", "conv-new", None)
        .expect("read new conversation")
        .is_some());
    commit(
        &mut store,
        revision,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-new".to_owned(),
        },
    );
    assert!(store
        .read_conversation("char-a", "conv-new", None)
        .expect("read deleted conversation")
        .is_none());
}

#[test]
fn replace_range_creates_conversation_at_explicit_configured_position() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-branch".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![
                message("duplicate"),
                json!({ "role": "char", "data": "missing ID" }),
                message("duplicate"),
            ],
            conversation: Some(json!({
                "id": "conv-branch",
                "name": "Long chat (Branch)",
                "note": "branch detail",
                "localLore": [],
                "fmIndex": -1,
                "unknownDetail": { "keep": false }
            })),
            configured_index: Some(0),
        },
    );

    let conversations = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured conversations");
    assert_eq!(
        conversations
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.configured_index))
            .collect::<Vec<_>>(),
        vec![("conv-branch", 0), ("conv-long", 1), ("conv-short", 2)]
    );
    let branch = store
        .read_conversation("char-a", "conv-branch", None)
        .expect("read branch")
        .expect("branch exists");
    assert_eq!(branch.value["note"], "branch detail");
    assert_eq!(branch.value["unknownDetail"]["keep"], false);
    assert_eq!(branch.value["message"].as_array().unwrap().len(), 3);
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            expected_revision: revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-branch".to_owned(),
                start: 0,
                delete_count: 0,
                messages: vec![message("must-not-insert")],
                conversation: Some(json!({
                    "id": "conv-branch", "name": "Collision", "note": "", "localLore": []
                })),
                configured_index: Some(0),
            }]),
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        }),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("unchanged revision"), revision);
}

#[test]
fn cas_conflict_preserves_current_revision() {
    let (_directory, mut store, _) = open_fixture();
    let result = store.commit(&WorkingSetCommit {
        expected_revision: 0,
        root: Some(json!({ "username": "stale" })),
        replace_presets: None,
        character: None,
        character_details: None,
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_id: None,
        asset_owner_heads: None,
        plugin_storage: None,
    });

    assert!(matches!(
        result,
        Err(StoreError::RevisionConflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(store.revision().expect("read revision"), 1);
}

#[test]
fn selected_character_replacement_is_atomic_and_preserves_catalog_order() {
    let (_directory, mut store, database) = open_fixture();
    let mut replacement = database["characters"][1].clone();
    let mut chats = replacement["chats"]
        .as_array()
        .expect("replacement chats")
        .clone();
    chats[1]["name"] = json!("Renamed inactive chat");
    chats[0]["message"] = json!([message("only-message")]);
    let short = chats.remove(1);
    chats.insert(0, short);
    chats.push(json!({
        "id": "conv-added",
        "name": "Added chat",
        "note": "supplied ID",
        "localLore": [],
        "message": [message("added-message")],
        "lastDate": 500
    }));
    replacement["chats"] = Value::Array(chats);
    let mut changed_root = root(&database);
    changed_root["username"] = json!("Committed with character");

    let revision = store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(changed_root),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(replacement),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("replace selected character")
        .revision;
    assert_eq!(revision, 2);
    assert_eq!(
        store.read_root(None).expect("read changed root").value["username"],
        "Committed with character"
    );
    let characters = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query characters after replacement");
    assert_eq!(
        characters
            .items
            .iter()
            .map(|item| (item.id.as_str(), item.configured_index))
            .collect::<Vec<_>>(),
        vec![("char-b", 0), ("char-a", 1)]
    );
    assert_eq!(characters.items[1].conversation_count, 3);
    assert_eq!(
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Configured,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("query replaced conversations")
            .items
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["conv-short", "conv-long", "conv-added"]
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-long", None)
            .expect("read replaced conversation")
            .expect("conversation exists")
            .value["message"]
            .as_array()
            .expect("messages")
            .len(),
        1
    );
}

#[test]
fn replacement_uses_the_greatest_configured_index_after_a_gap() {
    let (_directory, mut store, database) = open_fixture();
    let deleted = store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: Some("char-a".to_owned()),
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("delete middle configured character");
    assert!(store
        .read_character("char-a", None)
        .expect("read deleted character")
        .is_none());
    assert!(store
        .read_conversation("char-a", "conv-long", None)
        .expect("read deleted conversation")
        .is_none());
    let mut replacement = database["characters"][1].clone();
    replacement["chaId"] = json!("char-new");
    replacement["name"] = json!("New character");
    store
        .commit(&WorkingSetCommit {
            expected_revision: deleted.revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(replacement),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("add replacement after configured gap");

    let items = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured characters")
        .items;
    assert_eq!(items[0].id, "char-b");
    assert_eq!(
        (items[1].id.as_str(), items[1].configured_index),
        ("char-new", 3)
    );
}

#[test]
fn invalid_character_replacements_leave_revision_and_data_unchanged() {
    let (_directory, mut store, database) = open_fixture();
    let original = store
        .read_conversation("char-a", "conv-long", None)
        .expect("read original conversation");
    let mut invalid = database["characters"][1].clone();
    invalid["chats"][1]["id"] = invalid["chats"][0]["id"].clone();

    assert!(matches!(
        store.commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "must roll back" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(invalid.clone()),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        }),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(store.revision().expect("read unchanged revision"), 1);
    assert_eq!(
        store.read_root(None).expect("read unchanged root").value["username"],
        "Fixture User"
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-long", None)
            .expect("read unchanged conversation"),
        original
    );
    assert!(matches!(
        store.commit(&WorkingSetCommit {
            expected_revision: 0,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: Some(invalid),
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        }),
        Err(StoreError::RevisionConflict { .. })
    ));
}

#[test]
fn reopen_recovers_committed_data_and_sweeps_abandoned_staging() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let database = fixture();
    let staging_id = {
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let committed = store.replace_begin().expect("begin committed staging");
        store
            .replace_put_root(&committed.staging_id, &root(&database))
            .expect("stage committed root");
        store
            .replace_put_presets(
                &committed.staging_id,
                database["botPresets"].as_array().expect("fixture presets"),
            )
            .expect("stage committed presets");
        store
            .replace_add_characters(
                &committed.staging_id,
                database["characters"]
                    .as_array()
                    .expect("fixture characters"),
            )
            .expect("stage committed characters");
        store
            .replace_commit(&committed.staging_id, Some(0))
            .expect("commit fixture before reopen");
        let staging = store.replace_begin().expect("begin abandoned staging");
        store
            .replace_put_root(&staging.staging_id, &json!({ "username": "abandoned" }))
            .expect("stage abandoned root");
        staging.staging_id
    };

    let mut reopened = PersistentStore::open(directory.path()).expect("reopen persistent store");
    assert!(reopened.replace_commit(&staging_id, None).is_err());
    assert_eq!(reopened.revision().expect("read recovered revision"), 1);
    assert_eq!(
        reopened.read_root(None).expect("read recovered root").value["username"],
        "Fixture User"
    );
    assert_eq!(
        reopened
            .materialize(None)
            .expect("materialize reopened fixture"),
        database
    );
}

#[test]
fn invalid_staged_replacement_is_aborted_without_activation() {
    let (_directory, mut store, database) = open_fixture();
    let staging = store.replace_begin().expect("begin invalid staging");
    store
        .replace_put_root(&staging.staging_id, &json!({ "username": "invalid" }))
        .expect("stage replacement root");
    let first = database["characters"][0].clone();
    store
        .replace_add_characters(&staging.staging_id, std::slice::from_ref(&first))
        .expect("stage first character");
    let duplicate = first;

    assert!(matches!(
        store.replace_add_characters(&staging.staging_id, &[duplicate]),
        Err(StoreError::Validation { .. })
    ));
    store
        .replace_abort(&staging.staging_id)
        .expect("abort invalid staging");
    assert_eq!(store.revision().expect("read active revision"), 1);
    assert_eq!(
        store.materialize(None).expect("materialize active data"),
        database
    );
}

#[test]
fn revision_leases_are_isolated_then_released() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "apiType": "fixture-provider", "username": "Changed" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit changed root");

    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read leased root")
            .value["username"],
        "Fixture User"
    );
    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn ordinary_commit_during_a_lease_does_not_copy_any_generation_family() {
    let (_directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "counted-zero".to_owned(),
                value: json!(0),
            }]),
        })
        .expect("seed counted plugin record");
    let count_records = |store: &PersistentStore| {
        super::GENERATION_TABLES
            .iter()
            .map(|(table, _)| {
                store
                    .connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("count generation records")
            })
            .collect::<Vec<_>>()
    };
    let before = count_records(&store);
    let generation_before =
        super::active_generation(&store.connection).expect("read generation before leased commit");

    let lease = store.acquire_revision(2).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({ "username": "Changed without generation copy" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit root while lease is active");

    assert_eq!(count_records(&store), before);
    assert_eq!(
        super::active_generation(&store.connection).expect("read generation after leased commit"),
        generation_before
    );
    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read pinned root")
            .value["username"],
        "Fixture User"
    );
    store.release_revision(&lease.lease).expect("release lease");
}

fn leased_family_canonical(store: &PersistentStore, lease: &str) -> Vec<u8> {
    let owner = AssetOwnerLocator::RootModuleAssets { index: 0 };
    serde_json::to_vec(&json!({
        "root": store.read_root(Some(lease)).expect("read leased root"),
        "presetCatalog": store.query_presets(Some(lease)).expect("query leased presets"),
        "preset": store.read_preset("0", Some(lease)).expect("read leased preset"),
        "liveCharacters": store.query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased live characters"),
        "trashedCharacters": store.query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: true,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased trashed characters"),
        "character": store.read_character("char-a", Some(lease))
            .expect("read leased character"),
        "conversationCatalog": store.query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 100,
                cursor: None,
            },
            Some(lease),
        ).expect("query leased conversations"),
        "conversation": store.read_conversation("char-a", "conv-short", Some(lease))
            .expect("read leased conversation"),
        "messageWindow": store.read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                before: None,
                after: None,
            },
            Some(lease),
        ).expect("read leased message window"),
        "pluginCatalog": store.query_plugin_storage(Some(lease))
            .expect("query leased plugin storage"),
        "plugin": store.read_plugin_storage("lease-key", Some(lease))
            .expect("read leased plugin value"),
        "assetAliases": store.list_asset_aliases(Some(lease))
            .expect("list leased asset aliases"),
        "assetAlias": store.read_asset_alias("asset", "assets/lease.bin", Some(lease))
            .expect("read leased asset alias"),
        "assetOwnerHeads": store.list_asset_owner_heads(Some(lease))
            .expect("list leased asset owner heads"),
        "assetOwnerHead": store.read_asset_owner_head(&owner, Some(lease))
            .expect("read leased asset owner head"),
        "assetRepositoryAuthority": store.read_asset_repository_authority(Some(lease))
            .expect("read leased asset repository authority"),
        "coldPayloadAuthority": store.read_cold_payload_authority(Some(lease))
            .expect("read leased cold payload authority"),
        "coldAliases": store.list_cold_aliases(Some(lease))
            .expect("list leased cold aliases"),
        "coldAlias": store.read_cold_alias("cold/lease", Some(lease))
            .expect("read leased cold alias"),
        "materialized": store.materialize_lease(lease).expect("materialize leased revision"),
    }))
    .expect("serialize leased record families")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[test]
fn wal_lease_keeps_every_final_record_family_and_native_export_canonical() {
    let (directory, mut store, database) = open_fixture();
    let alias = AssetAlias {
        key: "assets/lease.bin".to_owned(),
        object_hash: Some("11".repeat(32)),
        kind: "asset".to_owned(),
        size: 7,
        mime: "application/octet-stream".to_owned(),
        name: "lease.bin".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({ "fixture": "lease" }),
    };
    let owner = AssetOwnerHead::present(
        AssetOwnerLocator::RootModuleAssets { index: 0 },
        "22".repeat(32),
        1,
    );
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open payload CAS");
    let prepared_cold = cas
        .prepare_bytes(b"cold-data")
        .expect("prepare leased cold payload");
    let cold = ColdAlias {
        key: "cold/lease".to_owned(),
        object_hash: Some(prepared_cold.content_hash),
        size: prepared_cold.byte_size as i64,
        metadata: json!({ "codec": "fixture" }),
    };
    let mut final_root = staged_root(&database);
    final_root["modules"] = json!([{
        "id": "lease-module",
        "assets": [["lease", "assets/lease.bin", "BIN"]]
    }]);
    let staging = store.replace_begin().expect("begin final-family staging");
    store
        .replace_put_root(&staging.staging_id, &final_root)
        .expect("stage final-family root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage final-family presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage final-family characters");
    store
        .replace_put_asset_aliases(&staging.staging_id, std::slice::from_ref(&alias))
        .expect("stage final-family asset alias");
    store
        .replace_put_asset_owner_heads(&staging.staging_id, std::slice::from_ref(&owner))
        .expect("stage final-family owner head");
    store
        .replace_put_cold_aliases(&staging.staging_id, std::slice::from_ref(&cold))
        .expect("stage final-family cold alias");
    store
        .replace_put_cold_payload_authority(
            &staging.staging_id,
            &ColdPayloadAuthorityState::V2 {
                migration_id: "lease-cold-migration".to_owned(),
                compatibility_hash: "44".repeat(32),
            },
        )
        .expect("stage final-family cold authority");
    let seeded = store
        .replace_commit(&staging.staging_id, Some(1))
        .expect("activate final-family staging");
    let seeded = store
        .commit(&WorkingSetCommit {
            expected_revision: seeded.revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "lease-key".to_owned(),
                value: json!({ "nested": [0, false, ""] }),
            }]),
        })
        .expect("seed plugin family before lease");
    let lease = store
        .acquire_revision(seeded.revision)
        .expect("acquire canonical lease");
    let canonical_before = leased_family_canonical(&store, &lease.lease);
    let export_before = store
        .export_risu_save(&lease.lease, false)
        .expect("export canonical lease before writes");
    let export_before_bytes = fs::read(&export_before.path).expect("read first native export");

    let mut changed_character = database["characters"][1].clone();
    changed_character["name"] = json!("Writer Alpha");
    let changed = store
        .commit(&WorkingSetCommit {
            expected_revision: seeded.revision,
            root: Some(json!({
                "username": "Writer root",
                "modules": [{ "id": "writer-module" }]
            })),
            replace_presets: Some(vec![json!({ "name": "Writer preset" })]),
            character: Some(changed_character),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start: 2,
                delete_count: 0,
                messages: vec![message("writer-message")],
                conversation: None,
                configured_index: None,
            }]),
            delete_character_id: Some("char-b".to_owned()),
            asset_owner_heads: Some(vec![AssetOwnerHead::absent(
                AssetOwnerLocator::RootModuleAssets { index: 0 },
            )]),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "lease-key".to_owned(),
                value: json!("writer plugin"),
            }]),
        })
        .expect("mutate writer record families");
    let replacement_alias = AssetAlias {
        object_hash: Some("44".repeat(32)),
        metadata: json!({ "fixture": "writer" }),
        ..alias.clone()
    };
    let changed = store
        .commit_asset_alias(&replacement_alias, changed.revision)
        .expect("mutate writer asset alias");
    let replacement = store
        .replace_begin()
        .expect("begin replacement during canonical lease");
    store
        .replace_put_root(
            &replacement.staging_id,
            &json!({ "username": "Replacement" }),
        )
        .expect("stage replacement during canonical lease");
    store
        .replace_commit(&replacement.staging_id, Some(changed.revision))
        .expect("activate replacement during canonical lease");

    let canonical_after = leased_family_canonical(&store, &lease.lease);
    let export_after = store
        .export_risu_save(&lease.lease, false)
        .expect("export canonical lease after writes");
    let export_after_bytes = fs::read(&export_after.path).expect("read second native export");
    assert_eq!(sha256(&canonical_after), sha256(&canonical_before));
    assert_eq!(sha256(&export_after_bytes), sha256(&export_before_bytes));

    store
        .cleanup_risu_save_export(Path::new(&export_before.path))
        .expect("clean first native export");
    store
        .cleanup_risu_save_export(Path::new(&export_after.path))
        .expect("clean second native export");
    store
        .release_revision(&lease.lease)
        .expect("release canonical lease");
}

#[test]
fn two_revision_leases_remain_independent_until_each_is_released() {
    let (_directory, mut store, _) = open_fixture();
    let first = store.acquire_revision(1).expect("acquire first lease");
    let second = store.acquire_revision(1).expect("acquire second lease");
    assert_eq!(store.lease_diagnostics().active_count, 2);
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Writer revision" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit writer revision");

    store
        .release_revision(&first.lease)
        .expect("release first lease");
    assert_eq!(store.lease_diagnostics().active_count, 1);
    assert!(matches!(
        store.read_root(Some(&first.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .read_root(Some(&second.lease))
            .expect("second lease remains pinned")
            .value["username"],
        "Fixture User"
    );
    assert!(matches!(
        store.checkpoint(CheckpointMode::Truncate),
        Err(StoreError::Store { .. })
    ));

    store
        .release_revision(&second.lease)
        .expect("release second lease");
    assert_eq!(store.lease_diagnostics().active_count, 0);
    store
        .release_revision(&second.lease)
        .expect("repeat released lease cleanup");
}

#[test]
fn native_job_store_uses_an_independent_connection_and_shared_reader_registry() {
    let (_directory, mut store, _) = open_fixture();
    let mut job_store = store
        .open_native_job_store()
        .expect("open native job store");
    let lease = job_store
        .acquire_revision(1)
        .expect("acquire native job revision lease");

    assert_eq!(store.lease_diagnostics().active_count, 1);
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Writer revision" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("advance live store while native job lease remains open");
    assert_eq!(
        job_store
            .read_root(Some(&lease.lease))
            .expect("read pinned root from native job connection")
            .value["username"],
        "Fixture User"
    );

    job_store
        .release_revision(&lease.lease)
        .expect("release native job revision lease");
    assert_eq!(store.lease_diagnostics().active_count, 0);
}

#[test]
fn revision_lease_survives_append_delete_root_change_and_staged_replace() {
    let (_directory, mut store, database) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "pinned-zero".to_owned(),
                value: json!(0),
            }]),
        })
        .expect("seed pinned plugin value");
    let lease = store.acquire_revision(2).expect("acquire revision lease");
    let revision = store
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({ "apiType": "fixture-provider", "username": "Changed" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start: 2,
                delete_count: 0,
                messages: vec![message("active-append")],
                conversation: None,
                configured_index: None,
            }]),
            delete_character_id: Some("char-b".to_owned()),
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "pinned-zero".to_owned(),
                value: json!(1),
            }]),
        })
        .expect("commit active changes")
        .revision;
    assert!(store
        .read_character("char-b", None)
        .expect("read active character")
        .is_none());

    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &root(&database))
        .expect("stage root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"]
                .as_array()
                .expect("fixture characters"),
        )
        .expect("stage characters");
    store
        .replace_commit(&staging.staging_id, Some(revision))
        .expect("activate staged replacement");

    assert_eq!(
        store
            .read_root(Some(&lease.lease))
            .expect("read leased root")
            .value["username"],
        "Fixture User"
    );
    assert!(store
        .read_character("char-b", Some(&lease.lease))
        .expect("read leased character")
        .is_some());
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", Some(&lease.lease))
            .expect("read leased conversation")
            .expect("leased conversation exists")
            .value["message"]
            .as_array()
            .expect("leased messages")
            .len(),
        2
    );
    assert_eq!(
        store
            .read_plugin_storage("pinned-zero", Some(&lease.lease))
            .expect("read leased plugin value")
            .expect("leased plugin value exists")
            .value,
        json!(0)
    );
    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn releasing_a_non_snapshot_generation_cannot_delete_active_data() {
    let (_directory, mut store, database) = open_fixture();

    assert!(matches!(
        store.release_revision("revision-1"),
        Err(StoreError::Validation { .. })
    ));
    assert_eq!(
        store.materialize(None).expect("active data remains"),
        database
    );
}

#[test]
fn conversation_mutations_recalculate_character_summary() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::Delete {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
        },
    );
    assert_eq!(revision, 2);
    let summary = store
        .query_characters(
            &CharacterQuery {
                search: Some("alpha".to_owned()),
                order: QueryOrder::Configured,
                trash: false,
                limit: 1,
                cursor: None,
            },
            None,
        )
        .expect("query alpha summary");
    assert_eq!(summary.items[0].conversation_count, 1);
}

#[test]
fn character_detail_update_preserves_index_and_conversations() {
    let (_directory, mut store, database) = open_fixture();
    let mut detail = database["characters"][1].clone();
    detail
        .as_object_mut()
        .expect("character object")
        .remove("chats");
    detail["name"] = json!("Alpha Renamed");
    detail["lastInteraction"] = json!(999);

    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: Some(detail.clone()),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit character detail update");

    let items = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query configured characters")
        .items;
    let alpha = items
        .iter()
        .find(|item| item.id == "char-a")
        .expect("alpha summary");
    assert_eq!(alpha.name, "Alpha Renamed");
    assert_eq!(alpha.configured_index, 1);
    assert_eq!(alpha.recent_at, 999);
    assert_eq!(alpha.conversation_count, 2);
    assert_eq!(
        store
            .read_character("char-a", None)
            .expect("read updated detail")
            .expect("character exists")
            .value,
        detail
    );
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", None)
            .expect("read untouched conversation")
            .expect("conversation exists")
            .value["message"]
            .as_array()
            .expect("messages")
            .len(),
        2
    );
}

#[test]
fn batch_character_details_delete_atomically_and_preserve_plugin_zero() {
    let (_directory, mut store, database) = open_fixture();
    let mut group = database["characters"][0].clone();
    group.as_object_mut().expect("group object").remove("chats");
    group["type"] = json!("group");
    group["characters"] = json!(["char-a", "char-c"]);
    group["characterTalks"] = json!([0.25, 0.75]);
    group["characterActive"] = json!([false, true]);

    let prepared = store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: Some(group.clone()),
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "zero".to_owned(),
                value: json!(0),
            }]),
        })
        .expect("prepare group and plugin value");
    let configured_index_before = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 100,
                cursor: None,
            },
            None,
        )
        .expect("query group before batch")
        .items
        .into_iter()
        .find(|summary| summary.id == "char-b")
        .expect("group summary before batch")
        .configured_index;
    let chats_before = store
        .materialize(None)
        .expect("materialize chats before batch")["characters"]
        .as_array()
        .expect("characters before batch")
        .iter()
        .find(|character| character["chaId"] == "char-b")
        .expect("group before batch")["chats"]
        .clone();
    let lease = store
        .acquire_revision(prepared.revision)
        .expect("acquire batch mutation lease");
    let mut updated_group = group.clone();
    updated_group["characters"] = json!(["char-c"]);
    updated_group["characterTalks"] = json!([0.75]);
    updated_group["characterActive"] = json!([true]);
    let mut invalid_detail = updated_group.clone();
    invalid_detail["chaId"] = json!("char-c");
    invalid_detail
        .as_object_mut()
        .expect("invalid detail object")
        .remove("name");

    let failed = store.commit(&WorkingSetCommit {
        expected_revision: prepared.revision,
        root: Some(json!({ "username": "Must roll back" })),
        replace_presets: None,
        character: None,
        character_details: Some(vec![updated_group.clone(), invalid_detail]),
        replace_character: None,
        add_character: None,
        conversations: None,
        delete_character_id: Some("char-a".to_owned()),
        asset_owner_heads: None,
        plugin_storage: None,
    });

    assert!(failed.is_err());
    assert_eq!(
        store.revision().expect("revision after rollback"),
        prepared.revision
    );
    assert!(store
        .read_character("char-a", None)
        .expect("read rolled back target")
        .is_some());
    assert_eq!(
        store
            .read_character("char-b", None)
            .expect("read rolled back group")
            .expect("group exists")
            .value["characters"],
        json!(["char-a", "char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage("zero", None)
            .expect("read plugin zero")
            .expect("plugin zero exists")
            .value,
        json!(0)
    );

    let committed = store
        .commit(&WorkingSetCommit {
            expected_revision: prepared.revision,
            root: Some(json!({ "username": "Committed" })),
            replace_presets: None,
            character: None,
            character_details: Some(vec![updated_group]),
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: Some("char-a".to_owned()),
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit batch delete");

    assert_eq!(committed.revision, prepared.revision + 1);
    assert!(store
        .read_character("char-a", None)
        .expect("read deleted target")
        .is_none());
    assert_eq!(
        store
            .read_character("char-b", None)
            .expect("read committed group")
            .expect("group exists")
            .value["characters"],
        json!(["char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage("zero", None)
            .expect("read preserved plugin zero")
            .expect("plugin zero exists")
            .value,
        json!(0)
    );
    assert_eq!(
        store
            .query_characters(
                &CharacterQuery {
                    search: None,
                    order: QueryOrder::Configured,
                    trash: false,
                    limit: 100,
                    cursor: None,
                },
                None,
            )
            .expect("query group after batch")
            .items
            .into_iter()
            .find(|summary| summary.id == "char-b")
            .expect("group summary after batch")
            .configured_index,
        configured_index_before
    );
    assert_eq!(
        store
            .materialize(None)
            .expect("materialize chats after batch")["characters"]
            .as_array()
            .expect("characters after batch")
            .iter()
            .find(|character| character["chaId"] == "char-b")
            .expect("group after batch")["chats"],
        chats_before
    );
    assert!(store
        .read_character("char-a", Some(&lease.lease))
        .expect("read leased target")
        .is_some());
    assert_eq!(
        store
            .read_character("char-b", Some(&lease.lease))
            .expect("read leased group")
            .expect("leased group exists")
            .value["characters"],
        json!(["char-a", "char-c"])
    );
    assert_eq!(
        store
            .read_plugin_storage("zero", Some(&lease.lease))
            .expect("read leased plugin zero")
            .expect("leased plugin zero exists")
            .value,
        json!(0)
    );
    store
        .release_revision(&lease.lease)
        .expect("release batch mutation lease");
}

#[test]
fn invalid_batch_character_detail_ids_leave_every_character_row_unchanged() {
    let (_directory, mut store, _) = open_fixture();
    let before = store
        .materialize(None)
        .expect("materialize before invalid batches");
    let detail = store
        .read_character("char-b", None)
        .expect("read batch detail")
        .expect("batch detail exists")
        .value;
    let mut empty = detail.clone();
    empty["chaId"] = json!("");
    let mut missing = detail.clone();
    missing["chaId"] = json!("missing-character");
    let cases = vec![
        ("empty", vec![empty], None),
        ("duplicate", vec![detail.clone(), detail.clone()], None),
        ("deleted", vec![detail.clone()], Some("char-b".to_owned())),
        ("missing", vec![missing], None),
    ];

    for (name, character_details, delete_character_id) in cases {
        let result = store.commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Must not persist" })),
            replace_presets: None,
            character: None,
            character_details: Some(character_details),
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id,
            asset_owner_heads: None,
            plugin_storage: None,
        });

        assert!(
            matches!(result, Err(StoreError::Validation { .. })),
            "{name} batch detail should be rejected"
        );
        assert_eq!(store.revision().expect("revision after invalid batch"), 1);
        assert_eq!(
            store
                .materialize(None)
                .expect("materialize after invalid batch"),
            before,
            "{name} batch detail changed stored character rows"
        );
    }
}

#[test]
fn replace_range_updates_conversation_detail_and_preserves_recent_at_without_it() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("appended")],
            conversation: Some(json!({
                "id": "conv-short", "name": "Renamed chat", "note": "updated",
                "localLore": [], "lastDate": 999
            })),
            configured_index: None,
        },
    );

    let query = |store: &PersistentStore| {
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Recent,
                    limit: 10,
                    cursor: None,
                },
                None,
            )
            .expect("query conversations")
            .items
    };
    let items = query(&store);
    let short = items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("short summary");
    assert_eq!(short.name, "Renamed chat");
    assert_eq!(short.recent_at, 999);
    assert_eq!(short.message_count, 3);
    assert_eq!(
        store
            .read_conversation("char-a", "conv-short", None)
            .expect("read updated conversation")
            .expect("conversation exists")
            .value["note"],
        "updated"
    );

    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 3,
            delete_count: 0,
            messages: vec![message("later")],
            conversation: None,
            configured_index: None,
        },
    );
    let items = query(&store);
    let short = items
        .iter()
        .find(|item| item.id == "conv-short")
        .expect("short summary after append");
    assert_eq!(short.recent_at, 999);
    assert_eq!(short.message_count, 4);
}

#[test]
fn summary_recent_at_falls_back_to_message_time_then_zero() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-from-message".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![message("timed")],
            conversation: Some(json!({ "name": "From message", "note": "", "localLore": [] })),
            configured_index: None,
        },
    );
    let revision = commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-empty".to_owned(),
            start: 0,
            delete_count: 0,
            messages: vec![],
            conversation: Some(json!({ "name": "Empty", "note": "", "localLore": [] })),
            configured_index: None,
        },
    );
    store
        .commit(&WorkingSetCommit {
            expected_revision: revision,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: Some(json!({ "chaId": "char-zero", "name": "Zero", "chats": [] })),
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("add character without lastInteraction");

    let conversations = store
        .query_conversations(
            &ConversationQuery {
                character_id: "char-a".to_owned(),
                order: QueryOrder::Configured,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query conversations")
        .items;
    let recent_at = |id: &str| {
        conversations
            .iter()
            .find(|item| item.id == id)
            .expect("conversation summary")
            .recent_at
    };
    assert_eq!(recent_at("conv-from-message"), 1_800_000_000_000);
    assert_eq!(recent_at("conv-empty"), 0);

    let characters = store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: false,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query characters")
        .items;
    assert_eq!(
        characters
            .iter()
            .find(|item| item.id == "char-zero")
            .expect("zero summary")
            .recent_at,
        0
    );
}

#[test]
fn replace_range_clamps_out_of_bounds_indices() {
    let (_directory, mut store, _) = open_fixture();
    let revision = commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 50,
            delete_count: 5,
            messages: vec![message("tail")],
            conversation: None,
            configured_index: None,
        },
    );
    commit(
        &mut store,
        revision,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: -3,
            delete_count: 100,
            messages: vec![message("only")],
            conversation: None,
            configured_index: None,
        },
    );

    let window = store
        .read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                before: None,
                after: None,
            },
            None,
        )
        .expect("read clamped window")
        .expect("conversation exists");
    assert_eq!(window.value.total_messages, 1);
    assert_eq!(window.value.messages[0]["chatId"], "only");
}

#[test]
fn revision_leases_isolate_conversation_reads() {
    let (_directory, mut store, _) = open_fixture();
    let lease = store.acquire_revision(1).expect("acquire revision lease");
    commit(
        &mut store,
        1,
        ConversationMutation::ReplaceRange {
            character_id: "char-a".to_owned(),
            conversation_id: "conv-short".to_owned(),
            start: 2,
            delete_count: 0,
            messages: vec![message("live-only")],
            conversation: None,
            configured_index: None,
        },
    );

    let window = |store: &PersistentStore, lease: Option<&str>| {
        store
            .read_conversation_window(
                &ConversationWindowQuery {
                    character_id: "char-a".to_owned(),
                    conversation_id: "conv-short".to_owned(),
                    start_index: Some(1),
                    limit: Some(2),
                    anchor_message_id: None,
                    before: None,
                    after: None,
                },
                lease,
            )
            .expect("read conversation window")
            .expect("conversation exists")
    };
    assert_eq!(window(&store, Some(&lease.lease)).value.total_messages, 2);
    assert_eq!(window(&store, None).value.total_messages, 3);
    assert_eq!(window(&store, Some(&lease.lease)).value.messages.len(), 1);
    assert_eq!(window(&store, None).value.messages.len(), 2);
    assert_eq!(
        store
            .query_conversations(
                &ConversationQuery {
                    character_id: "char-a".to_owned(),
                    order: QueryOrder::Configured,
                    limit: 10,
                    cursor: None,
                },
                Some(&lease.lease),
            )
            .expect("query leased conversations")
            .items
            .iter()
            .find(|item| item.id == "conv-short")
            .expect("leased short summary")
            .message_count,
        2
    );

    store.release_revision(&lease.lease).expect("release lease");
    assert!(matches!(
        store.read_conversation_window(
            &ConversationWindowQuery {
                character_id: "char-a".to_owned(),
                conversation_id: "conv-short".to_owned(),
                start_index: None,
                limit: None,
                anchor_message_id: None,
                before: None,
                after: None,
            },
            Some(&lease.lease),
        ),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn reopen_invalidates_runtime_and_legacy_leases_then_reclaims_inactive_generations() {
    let (directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: None,
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "ttl-zero".to_owned(),
                value: json!(0),
            }]),
        })
        .expect("seed leased plugin value");
    let lease = store.acquire_revision(2).expect("acquire revision lease");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 2,
            root: Some(json!({ "username": "Active after lease" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("fork active generation");
    store
        .connection
        .execute_batch(
            "
            INSERT INTO root (generation, value)
                VALUES ('revision-legacy', '{\"username\":\"Legacy pinned\"}');
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-legacy', 'revision-legacy', 2, 4102444800000);
            ",
        )
        .expect("seed rollback-compatible legacy lease");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("reopen after runtime lease drop");
    assert!(matches!(
        store.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert!(matches!(
        store.read_root(Some("snapshot-legacy")),
        Err(StoreError::SnapshotReleased)
    ));
    let persisted_leases: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM snapshot_leases", [], |row| row.get(0))
        .expect("count abandoned persisted leases");
    assert_eq!(persisted_leases, 0);
    let expired_generation_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root WHERE generation = 'revision-legacy'",
            [],
            |row| row.get(0),
        )
        .expect("count expired generation rows");
    assert_eq!(expired_generation_rows, 0);
    assert_eq!(
        store
            .read_plugin_storage("ttl-zero", None)
            .expect("read active plugin value after sweep")
            .expect("active plugin value survives sweep")
            .value,
        json!(0)
    );
    assert_eq!(
        store
            .read_root(None)
            .expect("read active root after sweep")
            .value["username"],
        "Active after lease"
    );
}

#[test]
fn pilot_mutated_database_supports_generation_cow_compatible_reopen_read_and_commit() {
    let (directory, mut store, _) = open_fixture();
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Pilot-mutated root" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                key: "rollback-compatible".to_owned(),
                value: json!({ "pilot": true }),
            }]),
        })
        .expect("mutate fixture through WAL pilot");
    let database_path = directory.path().join("persistent/persistent.db");
    drop(store);

    let mut compatibility = Connection::open(&database_path).expect("open database for COW path");
    super::schema::initialize(&mut compatibility).expect("initialize compatible schema");
    assert_eq!(
        compatibility
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read schema version"),
        14
    );
    assert_eq!(
        super::current_revision(&compatibility).expect("read pilot revision through COW path"),
        2
    );
    let source =
        super::active_generation(&compatibility).expect("read pilot generation through COW path");
    let source_root: String = compatibility
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&source],
            |row| row.get(0),
        )
        .expect("read pilot root through COW path");
    assert_eq!(
        serde_json::from_str::<Value>(&source_root).expect("parse pilot root")["username"],
        "Pilot-mutated root"
    );

    let legacy_lease = "snapshot-rollback-compatible";
    compatibility
        .execute(
            "INSERT INTO snapshot_leases (lease, generation, revision, created_at)
             VALUES (?1, ?2, 2, 4102444800000)",
            params![legacy_lease, source],
        )
        .expect("acquire generation COW lease");
    let target = "revision-3";
    let transaction = compatibility
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin generation COW commit");
    for (table, columns) in super::GENERATION_TABLES {
        transaction
            .execute(
                &format!(
                    "INSERT INTO {table} (generation, {columns})
                     SELECT ?1, {columns} FROM {table} WHERE generation = ?2"
                ),
                params![target, source],
            )
            .unwrap_or_else(|error| panic!("copy {table} through generation COW path: {error}"));
        let source_rows: i64 = transaction
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
                [&source],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("count source {table}: {error}"));
        let target_rows: i64 = transaction
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
                [target],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("count target {table}: {error}"));
        assert_eq!(target_rows, source_rows, "generation COW copied {table}");
    }
    transaction
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            params![
                target,
                serde_json::to_string(&json!({ "username": "Generation COW writer" }))
                    .expect("serialize COW root")
            ],
        )
        .expect("write generation COW root");
    transaction
        .execute(
            "UPDATE meta SET value = '3' WHERE key = 'currentRevision'",
            [],
        )
        .expect("advance generation COW revision");
    transaction
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'activeGeneration'",
            [serde_json::to_string(target).expect("serialize COW generation")],
        )
        .expect("activate generation COW target");
    transaction.commit().expect("commit generation COW write");

    let (leased_generation, leased_revision): (String, i64) = compatibility
        .query_row(
            "SELECT generation, revision FROM snapshot_leases WHERE lease = ?1",
            [legacy_lease],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolve generation COW lease");
    let leased_root: String = compatibility
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&leased_generation],
            |row| row.get(0),
        )
        .expect("read generation COW lease");
    assert_eq!(leased_revision, 2);
    assert_eq!(
        serde_json::from_str::<Value>(&leased_root).expect("parse leased COW root")["username"],
        "Pilot-mutated root"
    );
    assert_eq!(
        super::current_revision(&compatibility).expect("read generation COW revision"),
        3
    );
    assert_eq!(
        super::active_generation(&compatibility).expect("read generation COW target"),
        target
    );
    drop(compatibility);

    let reopened = PersistentStore::open(directory.path()).expect("reopen generation COW database");
    assert_eq!(reopened.revision().expect("read reopened COW revision"), 3);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read reopened COW root")
            .value["username"],
        "Generation COW writer"
    );
    assert_eq!(
        reopened
            .read_plugin_storage("rollback-compatible", None)
            .expect("read COW-copied plugin value")
            .expect("COW-copied plugin value exists")
            .value,
        json!({ "pilot": true })
    );
    assert_eq!(
        reopened
            .connection
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .expect("check COW database integrity"),
        "ok"
    );
}

#[test]
fn app_kv_round_trips_json() {
    let (_directory, store, _) = open_fixture();
    let value = json!({ "sourceRevision": 1, "imported": true });
    store.set_app_kv("migration", &value).expect("write app kv");
    assert_eq!(
        store.get_app_kv("migration").expect("read app kv"),
        Some(value)
    );
}

#[test]
fn app_kv_remove_deletes_only_the_selected_key() {
    let (_directory, store, _) = open_fixture();
    store
        .set_app_kv("credential", &json!({ "token": "legacy" }))
        .expect("write credential");
    store
        .set_app_kv("ledger", &json!({ "version": 1 }))
        .expect("write ledger");

    store
        .remove_app_kv("credential")
        .expect("remove credential");

    assert_eq!(
        store.get_app_kv("credential").expect("read credential"),
        None
    );
    assert_eq!(
        store.get_app_kv("ledger").expect("read ledger"),
        Some(json!({ "version": 1 }))
    );
}

fn create_v1_database(path: &Path) {
    fs::create_dir_all(path.parent().expect("v1 database parent")).expect("create v1 parent");
    let connection = rusqlite::Connection::open(path).expect("create v1 database");
    connection
        .execute_batch(
            "
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE app_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE snapshot_leases (generation TEXT PRIMARY KEY, created_at INTEGER NOT NULL);
            CREATE TABLE root (generation TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE characters (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL, recent_at INTEGER NOT NULL,
                trashed INTEGER NOT NULL, name TEXT NOT NULL, image TEXT,
                conversation_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id)
            );
            CREATE INDEX characters_configured ON characters (generation, configured_index);
            CREATE INDEX characters_recent ON characters (generation, recent_at DESC, configured_index);
            CREATE TABLE conversations (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, configured_index INTEGER NOT NULL,
                recent_at INTEGER NOT NULL, name TEXT NOT NULL,
                message_count INTEGER NOT NULL, detail TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id)
            );
            CREATE INDEX conversations_configured
                ON conversations (generation, character_id, configured_index);
            CREATE INDEX conversations_recent
                ON conversations (generation, character_id, recent_at DESC, configured_index);
            CREATE TABLE messages (
                generation TEXT NOT NULL, character_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL, message_index INTEGER NOT NULL,
                message_id TEXT, value TEXT NOT NULL,
                PRIMARY KEY (generation, character_id, conversation_id, message_index)
            );
            CREATE INDEX messages_by_id
                ON messages (generation, character_id, conversation_id, message_id);
            INSERT INTO meta VALUES ('currentRevision', '7');
            INSERT INTO meta VALUES ('activeGeneration', '\"revision-7\"');
            PRAGMA user_version = 1;
            ",
        )
        .expect("create v1 schema");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V1 active",
                    "characters": ["strip"],
                    "botPresets": [
                        { "name": "V1 first", "image": "v1.png" },
                        { "name": "V1 second" }
                    ],
                    "pluginCustomStorage": {
                        "active-memory": { "turns": [1, 2, 3] }
                    }
                })
                .to_string()
            ],
        )
        .expect("insert v1 active root");
    connection
        .execute(
            "INSERT INTO root VALUES (?1, ?2)",
            rusqlite::params![
                "revision-old",
                json!({
                    "username": "V1 old",
                    "botPresets": [{ "name": "Old preset" }],
                    "pluginCustomStorage": { "old-memory": "preserved" }
                })
                .to_string()
            ],
        )
        .expect("insert v1 old root");
    let detail = json!({
        "type": "character",
        "chaId": "v1-character",
        "name": "V1 character",
        "creatorNotes": "migrated notes",
        "trashTime": 123
    });
    connection
        .execute(
            "INSERT INTO characters
             (generation, character_id, configured_index, recent_at, trashed, name, image,
              conversation_count, detail)
             VALUES (?1, ?2, 0, 0, 1, ?3, NULL, 0, ?4)",
            rusqlite::params![
                "revision-7",
                "v1-character",
                "V1 character",
                detail.to_string()
            ],
        )
        .expect("insert v1 character");
}

fn create_v2_database(path: &Path) {
    create_v1_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v1 database for v2 setup");
    connection
        .execute_batch(
            "
            CREATE TABLE bot_presets (
                generation TEXT NOT NULL,
                preset_id TEXT NOT NULL,
                configured_index INTEGER NOT NULL,
                name TEXT NOT NULL,
                image TEXT,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, preset_id)
            );
            CREATE INDEX bot_presets_configured ON bot_presets (generation, configured_index);
            ALTER TABLE characters ADD COLUMN type TEXT NOT NULL DEFAULT '';
            ALTER TABLE characters ADD COLUMN creator_notes TEXT;
            ALTER TABLE characters ADD COLUMN trash_time INTEGER;
            PRAGMA user_version = 2;
            ",
        )
        .expect("create v2 schema additions");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({
                    "username": "V2 active",
                    "pluginCustomStorage": { "v2-memory": { "lossless": true } }
                })
                .to_string()
            ],
        )
        .expect("write v2 plugin root");
}

fn create_v2_database_with_lease(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v2 fixture database");
    connection
        .execute_batch(
            "
            INSERT INTO root (generation, value)
                VALUES ('snapshot-7-v2fixture',
                        '{\"username\":\"V2 leased\",\"pluginCustomStorage\":{\"leased-zero\":0}}');
            INSERT INTO snapshot_leases (generation, created_at)
                VALUES ('snapshot-7-v2fixture', 4102444800000);
            ",
        )
        .expect("create v2 fixture lease");
}

fn table_columns(
    connection: &rusqlite::Connection,
    table: &str,
) -> Vec<(String, String, bool, Option<String>, i64)> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info('{table}')"))
        .expect("prepare table column query");
    statement
        .query_map([], |row| {
            Ok((
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .expect("query table columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table columns")
}

fn assert_j2_v8_payload_schema(connection: &rusqlite::Connection) {
    assert_eq!(
        table_columns(connection, "asset_aliases"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("logical_key".to_owned(), "TEXT".to_owned(), true, None, 3),
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("kind".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            ("mime".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("name".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("ext".to_owned(), "TEXT".to_owned(), true, None, 0),
            ("inlay_type".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("width".to_owned(), "INTEGER".to_owned(), false, None, 0),
            ("height".to_owned(), "INTEGER".to_owned(), false, None, 0),
            (
                "metadata".to_owned(),
                "TEXT".to_owned(),
                true,
                Some("'{}'".to_owned()),
                0,
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "asset_owner_heads"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("owner_kind".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("owner_locator".to_owned(), "TEXT".to_owned(), true, None, 3),
            ("present".to_owned(), "INTEGER".to_owned(), true, None, 0),
            (
                "manifest_hash".to_owned(),
                "TEXT".to_owned(),
                false,
                None,
                0
            ),
            (
                "entry_count".to_owned(),
                "INTEGER".to_owned(),
                true,
                None,
                0
            ),
        ]
    );
    assert_eq!(
        table_columns(connection, "cold_aliases"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), true, None, 1),
            ("key".to_owned(), "TEXT".to_owned(), true, None, 2),
            ("object_hash".to_owned(), "TEXT".to_owned(), false, None, 0),
            ("size".to_owned(), "INTEGER".to_owned(), true, None, 0),
            ("metadata".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
}

fn assert_m4_v9_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
    assert_eq!(
        table_columns(connection, "asset_repository_authority"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("value".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM asset_repository_authority",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count M4 authority rows"),
        expected_rows
    );
}

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
        .replace_preserve_cold_payloads(&staging.staging_id, migrated.revision)
        .expect("preserve active cold payloads");
    store
        .replace_put_root(
            &staging.staging_id,
            &json!({ "username": "Restored database" }),
        )
        .expect("stage restored root");
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

fn assert_cold_v11_authority_schema(connection: &rusqlite::Connection, expected_rows: i64) {
    assert_eq!(
        table_columns(connection, "cold_payload_authority"),
        vec![
            ("generation".to_owned(), "TEXT".to_owned(), false, None, 1),
            ("value".to_owned(), "TEXT".to_owned(), true, None, 0),
        ]
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM cold_payload_authority", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count cold authority rows"),
        expected_rows
    );
}

fn assert_empty_logical_schema(connection: &rusqlite::Connection) {
    const TABLES: &[&str] = &[
        "logical_generation_session_pins",
        "logical_library_head",
        "logical_message_page_sources",
        "logical_peer_common_bases",
        "logical_record_dependencies",
        "logical_record_heads",
        "logical_sync_device_ack_proofs",
        "logical_sync_devices",
        "logical_sync_generations",
    ];
    const INDEXES: &[&str] = &[
        "logical_generation_session_pins_generation",
        "logical_message_page_sources_object",
        "logical_peer_common_bases_manifest",
        "logical_record_dependencies_object",
        "logical_record_heads_kind",
        "logical_record_heads_object",
        "logical_sync_device_ack_proofs_local_generation",
        "logical_sync_devices_status",
        "logical_sync_generations_manifest",
        "logical_sync_generations_parent",
    ];

    let schema_names = |kind: &str| {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = ?1 AND name LIKE 'logical_%'
                 ORDER BY name",
            )
            .expect("prepare logical schema inventory query");
        statement
            .query_map([kind], |row| row.get::<_, String>(0))
            .expect("query logical schema inventory")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect logical schema inventory")
    };

    assert_eq!(
        schema_names("table"),
        TABLES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        schema_names("index"),
        INDEXES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    for table in TABLES {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count logical schema rows");
        assert_eq!(count, 0, "{table} must start empty");
    }

    let expected_columns: &[(&str, &[&str])] = &[
        (
            "logical_sync_generations",
            &[
                "library_id",
                "generation_id",
                "generation_sequence",
                "parent_generation_id",
                "pds_generation",
                "source_revision",
                "state",
                "manifest_hash",
                "created_at",
                "completed_at",
            ],
        ),
        (
            "logical_library_head",
            &["singleton", "library_id", "generation_id"],
        ),
        (
            "logical_generation_session_pins",
            &["session_id", "library_id", "generation_id", "created_at"],
        ),
        (
            "logical_record_heads",
            &[
                "library_id",
                "generation_id",
                "record_key",
                "record_kind",
                "state",
                "object_hash",
                "object_size",
                "deleted_generation_sequence",
            ],
        ),
        (
            "logical_record_dependencies",
            &[
                "library_id",
                "generation_id",
                "record_key",
                "object_hash",
                "object_size",
            ],
        ),
        (
            "logical_message_page_sources",
            &[
                "library_id",
                "generation_id",
                "record_key",
                "record_kind",
                "page_index",
                "first_message_index",
                "message_count",
                "object_hash",
                "object_size",
            ],
        ),
        (
            "logical_peer_common_bases",
            &[
                "peer_id",
                "library_id",
                "generation_id",
                "manifest_hash",
                "generation_sequence",
                "updated_at",
            ],
        ),
        (
            "logical_sync_device_ack_proofs",
            &[
                "library_id",
                "device_id",
                "shared_generation_id",
                "shared_manifest_hash",
                "shared_generation_sequence",
                "local_generation_id",
                "local_manifest_hash",
                "local_generation_sequence",
                "verified_at",
            ],
        ),
        (
            "logical_sync_devices",
            &[
                "library_id",
                "device_id",
                "status",
                "acknowledged_generation_id",
                "acknowledged_manifest_hash",
                "acknowledged_generation_sequence",
                "registered_at",
                "acknowledged_at",
                "revoked_at",
                "forgotten_at",
            ],
        ),
    ];
    for (table, expected) in expected_columns {
        let actual = table_columns(connection, table)
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected
                .iter()
                .map(|column| (*column).to_owned())
                .collect::<Vec<_>>(),
            "{table} must contain only bounded logical metadata columns"
        );
    }
}

fn insert_logical_schema_fixture(connection: &rusqlite::Connection) {
    connection
        .execute_batch(
            r#"
            INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
            ) VALUES (
                'library-fixture', 'logical-generation-1', '1', NULL,
                'revision-0', 0, 'complete',
                'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 10, 11
            );
            INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                'conversation', 'live',
                '1111111111111111111111111111111111111111111111111111111111111111',
                11, NULL
            );
            INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                '2222222222222222222222222222222222222222222222222222222222222222',
                22
            );
            INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, record_kind, page_index,
                first_message_index, message_count, object_hash, object_size
            ) VALUES (
                'library-fixture', 'logical-generation-1', 'r1:conversation:fixture',
                'conversation', 0, 0, 4,
                '2222222222222222222222222222222222222222222222222222222222222222',
                22
            );
            INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
            ) VALUES (
                'peer-fixture', 'library-fixture', 'logical-generation-1',
                'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
                '1', 12
            );
            "#,
        )
        .expect("insert logical schema fixture");
}

fn assert_logical_schema_fixture(connection: &rusqlite::Connection) {
    let generation: (String, String, i64) = connection
        .query_row(
            "SELECT generation_sequence, manifest_hash, source_revision
             FROM logical_sync_generations
             WHERE library_id = 'library-fixture' AND generation_id = 'logical-generation-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read retained logical generation");
    assert_eq!(generation, ("1".to_owned(), "aa".repeat(32), 0));

    for table in [
        "logical_record_heads",
        "logical_record_dependencies",
        "logical_message_page_sources",
        "logical_peer_common_bases",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count retained logical rows");
        assert_eq!(count, 1, "{table} fixture row must be retained");
    }
}

#[test]
fn fresh_schema_v14_contains_dual_authority_and_empty_p4_logical_tables() {
    let directory = tempfile::tempdir().expect("create fresh v14 directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh v14 store");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read fresh schema version"),
        14
    );
    assert_j2_v8_payload_schema(&store.connection);
    assert_m4_v9_authority_schema(&store.connection, 1);
    assert_cold_v11_authority_schema(&store.connection, 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read fresh legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM cold_payload_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read fresh legacy cold authority"),
        r#"{"format":"legacy"}"#
    );
    assert_empty_logical_schema(&store.connection);
}

#[test]
fn schema_v11_backfills_cold_authority_for_active_and_leased_v10_generations() {
    let directory = tempfile::tempdir().expect("create v10 cold migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            INSERT INTO root (generation, value)
                VALUES ('snapshot-v10-cold', '{"username":"leased"}');
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-v10-cold', 'snapshot-v10-cold', 0, 4102444800000);
            DROP TABLE cold_payload_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            PRAGMA user_version = 10;
            "#,
        )
        .expect("create v10 cold fixture");
    drop(store);

    let mut connection =
        rusqlite::Connection::open(directory.path().join("persistent").join("persistent.db"))
            .expect("reopen v10 cold fixture");
    super::schema::initialize(&mut connection).expect("migrate v10 cold authority");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read migrated cold schema version"),
        14
    );
    assert_cold_v11_authority_schema(&connection, 2);
    for generation in ["revision-0", "snapshot-v10-cold"] {
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM cold_payload_authority WHERE generation = ?1",
                    [generation],
                    |row| row.get::<_, String>(0),
                )
                .expect("read backfilled cold authority"),
            r#"{"format":"legacy"}"#
        );
    }
}

#[test]
fn schema_v10_migrates_v8_through_m4_v9_and_preserves_j2_data() {
    let directory = tempfile::tempdir().expect("create v8 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create v8 store");
    let cas_sentinel = directory
        .path()
        .join("assets-v2/objects/aa/preserved-object");
    fs::create_dir_all(cas_sentinel.parent().expect("CAS sentinel parent"))
        .expect("create CAS sentinel parent");
    fs::write(&cas_sentinel, b"must remain untouched").expect("write CAS sentinel");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root SET value = '{"v8":"preserved"}' WHERE generation = 'revision-0';
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
            ) VALUES
                (
                    'revision-0', 'shared-key',
                    '1111111111111111111111111111111111111111111111111111111111111111',
                    'asset', 17, 'application/octet-stream', 'asset-name', 'BIN',
                    NULL, NULL, NULL, '{"source":"v8-asset"}'
                ),
                (
                    'revision-0', 'shared-key',
                    '2222222222222222222222222222222222222222222222222222222222222222',
                    'inlay', 23, 'image/webp', 'inlay-name', 'WebP',
                    'image', 640, 480, '{"source":"v8-inlay","width":640,"height":480}'
                );
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '3333333333333333333333333333333333333333333333333333333333333333', 2
            );
            INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
            VALUES (
                'revision-0', 'cold-preserved',
                '4444444444444444444444444444444444444444444444444444444444444444',
                29, '{"source":"v8-cold"}'
            );
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            PRAGMA user_version = 8;
            "#,
        )
        .expect("seed final J2 v8 data");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read v8 fixture version"),
        8
    );
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v8 store to v10");

    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read migrated schema version"),
        14
    );
    assert_j2_v8_payload_schema(&store.connection);
    assert_m4_v9_authority_schema(&store.connection, 1);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read migrated legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_empty_logical_schema(&store.connection);
    assert_eq!(
        store.read_root(None).expect("read preserved v8 root").value,
        json!({ "v8": "preserved" })
    );
    assert_eq!(
        store
            .read_asset_alias("asset", "shared-key", None)
            .expect("read preserved ordinary alias")
            .expect("ordinary alias exists")
            .value,
        AssetAlias {
            key: "shared-key".to_owned(),
            object_hash: Some("11".repeat(32)),
            kind: "asset".to_owned(),
            size: 17,
            mime: "application/octet-stream".to_owned(),
            name: "asset-name".to_owned(),
            ext: "BIN".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({ "source": "v8-asset" }),
        }
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", "shared-key", None)
            .expect("read preserved Inlay alias")
            .expect("Inlay alias exists")
            .value,
        AssetAlias {
            key: "shared-key".to_owned(),
            object_hash: Some("22".repeat(32)),
            kind: "inlay".to_owned(),
            size: 23,
            mime: "image/webp".to_owned(),
            name: "inlay-name".to_owned(),
            ext: "WebP".to_owned(),
            inlay_type: Some("image".to_owned()),
            width: Some(640),
            height: Some(480),
            metadata: json!({
                "source": "v8-inlay",
                "width": 640,
                "height": 480
            }),
        }
    );
    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .expect("read preserved owner head")
            .expect("owner head exists")
            .value,
        AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "33".repeat(32),
            2,
        )
    );
    assert_eq!(
        store
            .read_cold_alias("cold-preserved", None)
            .expect("read preserved cold alias")
            .expect("cold alias exists")
            .value,
        ColdAlias {
            key: "cold-preserved".to_owned(),
            object_hash: Some("44".repeat(32)),
            size: 29,
            metadata: json!({ "source": "v8-cold" }),
        }
    );
    assert_eq!(
        fs::read(&cas_sentinel).expect("read untouched CAS sentinel"),
        b"must remain untouched"
    );
}

#[test]
fn schema_v10_migration_collision_preserves_completed_m4_v9_and_v8_rows() {
    let directory = tempfile::tempdir().expect("create v8 rollback directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root SET value = '{"rollback":"preserved"}' WHERE generation = 'revision-0';
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
            ) VALUES (
                'revision-0', 'rollback-asset', NULL, 'asset', 0,
                'application/octet-stream', 'Rollback asset', 'bin',
                NULL, NULL, NULL, '{"source":"v8"}'
            );
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '5555555555555555555555555555555555555555555555555555555555555555', 1
            );
            INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
            VALUES ('revision-0', 'rollback-cold', NULL, 0, '{"source":"v8"}');
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            CREATE TABLE logical_record_heads (collision_marker TEXT NOT NULL);
            PRAGMA user_version = 8;
            "#,
        )
        .expect("create colliding v8 fixture");
    drop(store);

    let error = match PersistentStore::open(directory.path()) {
        Ok(_) => panic!("logical table collision must fail migration"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("logical_record_heads"));

    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(database_path).expect("reopen retained M4 v9 db");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back schema version"),
        9
    );
    assert_j2_v8_payload_schema(&connection);
    assert_m4_v9_authority_schema(&connection, 1);
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM asset_repository_authority WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read retained M4 legacy authority"),
        r#"{"format":"legacy"}"#
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM root WHERE generation = 'revision-0'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read preserved v8 root"),
        r#"{"rollback":"preserved"}"#
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT metadata FROM cold_aliases
                 WHERE generation = 'revision-0' AND key = 'rollback-cold'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read preserved v8 cold alias"),
        r#"{"source":"v8"}"#
    );
    for table in ["asset_aliases", "asset_owner_heads", "cold_aliases"] {
        let count: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE generation = 'revision-0'"),
                [],
                |row| row.get(0),
            )
            .expect("count preserved v8 J2 rows");
        assert_eq!(count, 1, "{table} row must survive migration rollback");
    }
    assert_eq!(
        table_columns(&connection, "logical_record_heads"),
        vec![(
            "collision_marker".to_owned(),
            "TEXT".to_owned(),
            true,
            None,
            0,
        )]
    );
    let partial_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('logical_sync_generations', 'logical_record_dependencies')",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back logical tables");
    assert_eq!(partial_table_count, 0);
}

#[test]
fn schema_v10_migration_collision_rolls_back_v9_logical_ddl_only() {
    let directory = tempfile::tempdir().expect("create M4 v9 rollback directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            DROP TABLE cold_payload_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            CREATE TABLE logical_record_heads (collision_marker TEXT NOT NULL);
            PRAGMA user_version = 9;
            "#,
        )
        .expect("create colliding M4 v9 fixture");
    drop(store);

    let error = match PersistentStore::open(directory.path()) {
        Ok(_) => panic!("logical table collision must fail v9 to v10 migration"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("logical_record_heads"));

    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(database_path).expect("reopen rolled-back v9 db");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back M4 schema version"),
        9
    );
    assert_m4_v9_authority_schema(&connection, 1);
    assert_eq!(
        table_columns(&connection, "logical_record_heads"),
        vec![(
            "collision_marker".to_owned(),
            "TEXT".to_owned(),
            true,
            None,
            0,
        )]
    );
    let partial_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN ('logical_sync_generations', 'logical_record_dependencies')",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back logical tables");
    assert_eq!(partial_table_count, 0);
}

#[test]
fn schema_v10_logical_rows_survive_snapshot_restore_and_reopen() {
    let directory = tempfile::tempdir().expect("create v10 snapshot directory");
    let store = PersistentStore::open(directory.path()).expect("open v10 store");
    insert_logical_schema_fixture(&store.connection);
    let snapshot = store
        .snapshot_create("logical-v10")
        .expect("create v10 logical snapshot");
    store
        .connection
        .execute_batch(
            "DELETE FROM logical_message_page_sources;
             DELETE FROM logical_record_dependencies;
             DELETE FROM logical_record_heads;
             DELETE FROM logical_peer_common_bases;
             DELETE FROM logical_sync_generations;",
        )
        .expect("change logical rows after snapshot");
    store
        .snapshot_restore_request(Path::new(&snapshot.path))
        .expect("request v10 logical snapshot restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore v10 snapshot on reopen");
    assert_eq!(
        restored
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read restored schema version"),
        14
    );
    assert_logical_schema_fixture(&restored.connection);
    drop(restored);

    let reopened = PersistentStore::open(directory.path()).expect("reopen restored v10 store");
    assert_logical_schema_fixture(&reopened.connection);
}

#[test]
fn schema_v8_adds_empty_payload_alias_tables_to_v5() {
    let directory = tempfile::tempdir().expect("create v5 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    drop(store);
    let database_path = directory.path().join("persistent/persistent.db");
    let connection = rusqlite::Connection::open(&database_path).expect("open migration fixture");
    connection
        .execute_batch(
            "DROP TABLE asset_aliases;
             DROP TABLE asset_owner_heads;
             DROP TABLE cold_aliases;
             DROP TABLE cold_payload_authority;
             DROP TABLE asset_repository_authority;
             DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_objects;
             DROP TABLE logical_generation_session_pins;
             DROP TABLE logical_library_head;
             DROP TABLE logical_message_page_sources;
             DROP TABLE logical_peer_common_bases;
             DROP TABLE logical_record_dependencies;
             DROP TABLE logical_record_heads;
             DROP TABLE logical_sync_generations;
             PRAGMA user_version = 5;",
        )
        .expect("downgrade fixture schema marker");
    drop(connection);

    let store = PersistentStore::open(directory.path()).expect("migrate v5 store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    let alias_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM asset_aliases", [], |row| row.get(0))
        .expect("count migrated aliases");
    let head_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM asset_owner_heads", [], |row| {
            row.get(0)
        })
        .expect("count migrated owner heads");

    assert_eq!(version, 14);
    assert_eq!(alias_count, 0);
    assert_eq!(head_count, 0);
    assert_eq!(
        store.read_root(None).expect("read migrated root").revision,
        0
    );
}

#[test]
fn schema_v10_chains_m4_authority_and_p4_logical_migrations_from_v8() {
    let directory = tempfile::tempdir().expect("create v8 authority migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            "DROP TABLE cold_payload_authority;
             DROP TABLE asset_repository_authority;
             DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_objects;
             DROP TABLE logical_generation_session_pins;
             DROP TABLE logical_library_head;
             DROP TABLE logical_message_page_sources;
             DROP TABLE logical_peer_common_bases;
             DROP TABLE logical_record_dependencies;
             DROP TABLE logical_record_heads;
             DROP TABLE logical_sync_generations;
             PRAGMA user_version = 8;",
        )
        .expect("downgrade authority schema fixture");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v8 authority store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 14);
    assert_eq!(
        store
            .read_asset_repository_authority(None)
            .expect("read migrated legacy authority")
            .value,
        AssetRepositoryAuthorityState::Legacy
    );
}

#[test]
fn schema_v8_migrates_v6_alias_without_changing_its_value() {
    let directory = tempfile::tempdir().expect("create v6 alias migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    let alias = AssetAlias {
        key: "legacy/shared-key".to_owned(),
        object_hash: Some("ab".repeat(32)),
        kind: "asset".to_owned(),
        size: 17,
        mime: "application/octet-stream".to_owned(),
        name: "Legacy alias".to_owned(),
        ext: "BIN".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({
            "name": "Legacy alias",
            "ext": "BIN",
            "mime": "application/octet-stream"
        }),
    };
    let inlay = AssetAlias {
        key: "legacy/inlay-key".to_owned(),
        object_hash: Some("cd".repeat(32)),
        kind: "inlay".to_owned(),
        size: 23,
        mime: "image/webp".to_owned(),
        name: "Legacy Inlay".to_owned(),
        ext: "WebP".to_owned(),
        inlay_type: Some("image".to_owned()),
        width: Some(640),
        height: None,
        metadata: json!({
            "name": "Legacy Inlay",
            "ext": "WebP",
            "mime": "image/webp",
            "inlayType": "image",
            "width": 640
        }),
    };
    for legacy in [&alias, &inlay] {
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES ('revision-0', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    legacy.key,
                    legacy.object_hash,
                    legacy.kind,
                    legacy.size,
                    legacy.mime,
                    legacy.name,
                    legacy.ext,
                    legacy.inlay_type,
                    legacy.width,
                    legacy.height,
                ],
            )
            .expect("insert legacy alias");
    }
    store
        .connection
        .execute_batch(
            "
            DROP INDEX asset_aliases_generation;
            ALTER TABLE asset_aliases RENAME TO asset_aliases_v8;
            CREATE TABLE asset_aliases (
                generation TEXT NOT NULL,
                logical_key TEXT NOT NULL,
                object_hash TEXT,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                mime TEXT NOT NULL,
                name TEXT NOT NULL,
                ext TEXT NOT NULL,
                inlay_type TEXT,
                width INTEGER,
                height INTEGER,
                PRIMARY KEY (generation, logical_key)
            );
            INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
            )
            SELECT generation, logical_key, object_hash, kind, size, mime, name, ext,
                   inlay_type, width, height
            FROM asset_aliases_v8;
            DROP TABLE asset_aliases_v8;
            CREATE INDEX asset_aliases_generation ON asset_aliases (generation);
            DROP TABLE asset_owner_heads;
            DROP TABLE cold_aliases;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            PRAGMA user_version = 6;
            ",
        )
        .expect("downgrade alias schema fixture to v6");
    drop(store);

    let store = PersistentStore::open(directory.path()).expect("migrate v6 alias store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    assert_eq!(version, 14);
    assert_eq!(
        store
            .read_asset_alias("asset", &alias.key, None)
            .expect("read migrated alias"),
        Some(super::Versioned {
            revision: 0,
            value: alias,
        })
    );
    assert_eq!(
        store
            .read_asset_alias("inlay", &inlay.key, None)
            .expect("read migrated Inlay alias"),
        Some(super::Versioned {
            revision: 0,
            value: inlay,
        })
    );
    assert_eq!(
        store
            .list_cold_aliases(None)
            .expect("read empty migrated cold inventory")
            .value,
        Vec::<ColdAlias>::new()
    );
    assert_eq!(
        store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .expect("read empty migrated owner-head table"),
        None
    );
    let primary_key_columns = {
        let mut statement = store
            .connection
            .prepare(
                "SELECT name FROM pragma_table_info('asset_aliases')
                 WHERE pk > 0 ORDER BY pk",
            )
            .expect("prepare alias primary key query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query alias primary key")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect alias primary key")
    };
    assert_eq!(
        primary_key_columns,
        vec!["generation", "kind", "logical_key"]
    );
}

#[test]
fn schema_v8_preserves_m5_v7_owner_heads_through_cow_and_pinned_reads() {
    let directory = tempfile::tempdir().expect("create v7 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            r#"
            UPDATE root
            SET value = '{"modules":[{"id":"v7-module","assets":[["v7","assets/v7.bin","BIN"]]}]}'
            WHERE generation = 'revision-0';
            INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
            ) VALUES (
                'revision-0', 'root-module-assets', '0', 1,
                '8383838383838383838383838383838383838383838383838383838383838383', 1
            );
            DROP TABLE cold_aliases;
            DROP TABLE cold_payload_authority;
            DROP TABLE asset_repository_authority;
            DROP INDEX logical_sync_device_ack_proofs_local_generation;
            DROP TABLE logical_sync_device_ack_proofs;
            DROP INDEX logical_sync_devices_status;
            DROP TABLE logical_sync_devices;
            DROP TABLE asset_objects;
            DROP TABLE logical_generation_session_pins;
            DROP TABLE logical_library_head;
            DROP TABLE logical_message_page_sources;
            DROP TABLE logical_peer_common_bases;
            DROP TABLE logical_record_dependencies;
            DROP TABLE logical_record_heads;
            DROP TABLE logical_sync_generations;
            PRAGMA user_version = 7;
            "#,
        )
        .expect("create M5 v7 owner-head fixture");
    drop(store);

    let mut store = PersistentStore::open(directory.path()).expect("migrate M5 v7 store");
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated schema version");
    let cold_table_exists: bool = store
        .connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'cold_aliases'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query migrated cold table");

    assert_eq!(version, 14);
    assert!(cold_table_exists);
    let owner = AssetOwnerLocator::RootModuleAssets { index: 0 };
    let head = AssetOwnerHead::present(owner.clone(), "83".repeat(32), 1);
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read migrated current owner head"),
        Some(super::Versioned {
            revision: 0,
            value: head.clone(),
        })
    );
    let lease = store.acquire_revision(0).expect("pin migrated v7 revision");
    let alias = AssetAlias {
        key: "assets/post-v7.bin".to_owned(),
        object_hash: Some("84".repeat(32)),
        kind: "asset".to_owned(),
        size: 1,
        mime: "application/octet-stream".to_owned(),
        name: "Post-v7".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let current = store
        .commit_asset_alias(&alias, 0)
        .expect("copy migrated owner head into new generation");
    assert_eq!(
        store
            .read_asset_owner_head(&owner, None)
            .expect("read copied current owner head"),
        Some(super::Versioned {
            revision: current.revision,
            value: head.clone(),
        })
    );
    assert_eq!(
        store
            .read_asset_owner_head(&owner, Some(&lease.lease))
            .expect("read migrated pinned owner head"),
        Some(super::Versioned {
            revision: 0,
            value: head,
        })
    );
}

#[test]
fn schema_v8_creates_the_m5_owner_head_table() {
    let directory = tempfile::tempdir().expect("create owner-head schema directory");
    let store = PersistentStore::open(directory.path()).expect("create v8 store");
    let table_exists: bool = store
        .connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_schema
                WHERE type = 'table' AND name = 'asset_owner_heads'
             )",
            [],
            |row| row.get(0),
        )
        .expect("query owner-head table");

    assert!(table_exists);
}

#[test]
fn schema_v8_migrates_v2_snapshot_lease_and_plugin_records() {
    let directory = tempfile::tempdir().expect("create v2 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database_with_lease(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v2 store");
    assert_eq!(
        store
            .read_asset_alias("asset", "assets/not-backfilled.bin", None)
            .expect("query empty migrated alias table"),
        None
    );
    assert!(matches!(
        store.read_root(Some("snapshot-7-v2fixture")),
        Err(StoreError::SnapshotReleased)
    ));
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 14);

    let snapshot_rows: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation = 'snapshot-7-v2fixture'",
            [],
            |row| row.get(0),
        )
        .expect("count released migrated plugin records");
    assert_eq!(snapshot_rows, 0);
}

fn create_v3_database(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open v2 database for v3 setup");
    connection
        .execute_batch(
            "
            CREATE TABLE plugin_storage (
                generation TEXT NOT NULL,
                storage_key TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (generation, storage_key)
            );
            ",
        )
        .expect("create v3 plugin table");
    let roots = {
        let mut statement = connection
            .prepare("SELECT generation, value FROM root")
            .expect("prepare v3 roots");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query v3 roots")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect v3 roots")
    };
    for (generation, serialized) in roots {
        let mut value: Value = serde_json::from_str(&serialized).expect("parse v3 root");
        let object = value.as_object_mut().expect("v3 root object");
        if let Some(storage) = object.remove("pluginCustomStorage") {
            for (key, value) in storage.as_object().expect("v3 plugin object") {
                let serialized = serde_json::to_string(value).expect("serialize v3 plugin value");
                connection
                    .execute(
                        "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![generation, key, serialized.len() as i64, serialized],
                    )
                    .expect("insert v3 plugin value");
            }
        }
        connection
            .execute(
                "UPDATE root SET value = ?2 WHERE generation = ?1",
                rusqlite::params![generation, value.to_string()],
            )
            .expect("strip v3 root");
    }
    connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, value)
             VALUES ('revision-7', 'zeta', 1, '0'),
                    ('revision-7', '10', 1, '0'),
                    ('revision-7', '2', 1, '0')",
            [],
        )
        .expect("insert v3 ordering values");
    connection
        .pragma_update(None, "user_version", 3)
        .expect("set v3 schema version");
}

fn create_snapshot_v3_database(path: &Path) {
    create_v2_database(path);
    let connection = rusqlite::Connection::open(path).expect("open snapshot v3 fixture database");
    connection
        .execute_batch(
            "
            DROP TABLE snapshot_leases;
            CREATE TABLE snapshot_leases (
                lease TEXT PRIMARY KEY,
                generation TEXT NOT NULL,
                revision INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX snapshot_leases_generation ON snapshot_leases (generation);
            INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                VALUES ('snapshot-v3fixture', 'revision-7', 7, 4102444800000);
            PRAGMA user_version = 3;
            ",
        )
        .expect("create snapshot v3 fixture schema");
}

fn create_task4_v4_database(path: &Path) {
    create_v3_database(path);
    let connection = rusqlite::Connection::open(path).expect("open Task 4 v4 fixture database");
    connection
        .execute_batch(
            "
            ALTER TABLE plugin_storage ADD COLUMN ordinal INTEGER NOT NULL DEFAULT 0;
            UPDATE plugin_storage AS target
            SET ordinal = (
                SELECT COUNT(*) - 1
                FROM plugin_storage AS predecessor
                WHERE predecessor.generation = target.generation
                  AND predecessor.storage_key <= target.storage_key
            );
            INSERT INTO root (generation, value)
                VALUES ('snapshot-7-task4v4', '{\"username\":\"Task 4 v4 leased\"}');
            INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
                VALUES ('snapshot-7-task4v4', 'leased-zero', 1, 0, '0');
            INSERT INTO snapshot_leases (generation, created_at)
                VALUES ('snapshot-7-task4v4', 4102444800000);
            PRAGMA user_version = 4;
            ",
        )
        .expect("create Task 4 v4 fixture schema");
}

#[test]
fn schema_v8_migrates_snapshot_v3_without_plugin_table() {
    let directory = tempfile::tempdir().expect("create snapshot v3 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_snapshot_v3_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate snapshot v3 store");
    assert!(matches!(
        store.read_root(Some("snapshot-v3fixture")),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read snapshot v3 migrated version"),
        14
    );
    assert_eq!(
        store
            .read_plugin_storage("v2-memory", None)
            .expect("read active migrated plugin value")
            .expect("active migrated plugin value exists")
            .value,
        json!({ "lossless": true })
    );
}

#[test]
fn schema_v8_migrates_task4_v4_lease_with_plugin_ordinal() {
    let directory = tempfile::tempdir().expect("create Task 4 v4 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_task4_v4_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate Task 4 v4 store");
    assert!(matches!(
        store.read_root(Some("snapshot-7-task4v4")),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read Task 4 v4 migrated version"),
        14
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM plugin_storage WHERE generation = 'snapshot-7-task4v4'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count released Task 4 v4 plugin rows"),
        0
    );
}

#[test]
fn schema_v8_migrates_existing_v2_plugin_storage() {
    let directory = tempfile::tempdir().expect("create v2 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v2 store");

    assert!(store
        .read_root(None)
        .expect("read v2 migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("v2-memory", None)
            .expect("read v2 migrated plugin key")
            .expect("v2 migrated plugin key exists")
            .value,
        json!({ "lossless": true })
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read v2 migrated version");
    assert_eq!(version, 14);
}

#[test]
fn schema_v8_adds_durable_plugin_ordinals_to_task4_v3() {
    let directory = tempfile::tempdir().expect("create v3 migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v3_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v3 store");

    assert_eq!(
        store
            .query_plugin_storage(None)
            .expect("query migrated v3 storage")
            .items
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        vec!["2", "10", "v2-memory", "zeta"]
    );
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated v3 version");
    assert_eq!(version, 14);
}

#[test]
fn schema_v8_migrates_large_retained_roots_one_generation_at_a_time() {
    let directory = tempfile::tempdir().expect("create retained root migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v2_database(&database_path);
    let connection = rusqlite::Connection::open(&database_path).expect("open retained roots");
    let payload = "x".repeat(512 * 1024);
    for index in 0..12 {
        connection
            .execute(
                "INSERT INTO root (generation, value) VALUES (?1, ?2)",
                rusqlite::params![
                    format!("retained-{index}"),
                    json!({
                        "username": format!("Retained {index}"),
                        "pluginCustomStorage": { format!("memory-{index}"): payload }
                    })
                    .to_string()
                ],
            )
            .expect("insert retained root");
    }
    drop(connection);

    let store = PersistentStore::open(directory.path()).expect("migrate retained roots");
    let migrated_count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM plugin_storage WHERE generation LIKE 'retained-%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained plugin rows");
    let retained_root_fields: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM root
             WHERE generation LIKE 'retained-%' AND value LIKE '%pluginCustomStorage%'",
            [],
            |row| row.get(0),
        )
        .expect("count retained root plugin fields");
    assert_eq!(migrated_count, 12);
    assert_eq!(retained_root_fields, 0);
}

#[test]
fn schema_v8_migrates_records_for_every_v1_generation() {
    let directory = tempfile::tempdir().expect("create migration directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);

    let store = PersistentStore::open(directory.path()).expect("migrate v1 store");
    assert_eq!(
        store
            .query_presets(None)
            .expect("query migrated presets")
            .items
            .len(),
        2
    );
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("botPresets")
        .is_none());
    assert!(store
        .read_root(None)
        .expect("read migrated root")
        .value
        .get("pluginCustomStorage")
        .is_none());
    assert_eq!(
        store
            .read_plugin_storage("active-memory", None)
            .expect("read migrated plugin storage")
            .expect("migrated plugin key exists")
            .value,
        json!({ "turns": [1, 2, 3] })
    );
    let old_root: String = store
        .connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated old root");
    assert_eq!(
        serde_json::from_str::<Value>(&old_root).expect("parse old root"),
        json!({ "username": "V1 old" })
    );
    let old_presets: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM bot_presets WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("count old presets");
    assert_eq!(old_presets, 1);
    let old_plugin_storage: String = store
        .connection
        .query_row(
            "SELECT value FROM plugin_storage
             WHERE generation = 'revision-old' AND storage_key = 'old-memory'",
            [],
            |row| row.get(0),
        )
        .expect("read old plugin storage");
    assert_eq!(
        serde_json::from_str::<Value>(&old_plugin_storage).expect("parse old plugin storage"),
        json!("preserved")
    );
    let summary = &store
        .query_characters(
            &CharacterQuery {
                search: None,
                order: QueryOrder::Configured,
                trash: true,
                limit: 10,
                cursor: None,
            },
            None,
        )
        .expect("query migrated character")
        .items[0];
    assert_eq!(summary.r#type, "character");
    assert_eq!(summary.creator_notes.as_deref(), Some("migrated notes"));
    assert_eq!(summary.trash_time, Some(123));
    let version: i64 = store
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read migrated version");
    assert_eq!(version, 14);
}

#[test]
fn schema_v8_rolls_back_when_v1_bot_presets_is_not_an_array() {
    let directory = tempfile::tempdir().expect("create migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid v1 root",
        "botPresets": { "legacy": "unsupported" }
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    let original_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read original active v1 root");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid v1 root");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back v1 database");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back schema version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved v1 root");
    let preserved_active: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-7'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved active v1 root");
    let preset_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'bot_presets'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back preset table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved root"),
        invalid_root
    );
    assert_eq!(preserved_active, original_active);
    assert_eq!(preset_table_count, 0);
}

#[test]
fn schema_v8_rolls_back_when_v1_plugin_storage_is_not_an_object() {
    let directory = tempfile::tempdir().expect("create plugin migration rollback directory");
    let database_path = directory.path().join("persistent/persistent.db");
    create_v1_database(&database_path);
    let invalid_root = json!({
        "username": "Invalid plugin root",
        "botPresets": [{ "name": "Still valid" }],
        "pluginCustomStorage": ["unsupported"]
    });
    let connection = rusqlite::Connection::open(&database_path).expect("open v1 database");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params!["revision-old", invalid_root.to_string()],
        )
        .expect("write invalid plugin storage");
    drop(connection);

    assert!(PersistentStore::open(directory.path()).is_err());

    let connection =
        rusqlite::Connection::open(&database_path).expect("reopen rolled back plugin migration");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read rolled back version");
    let preserved: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-old'",
            [],
            |row| row.get(0),
        )
        .expect("read preserved plugin root");
    let plugin_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'plugin_storage'",
            [],
            |row| row.get(0),
        )
        .expect("check rolled back plugin table");
    assert_eq!(version, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&preserved).expect("parse preserved plugin root"),
        invalid_root
    );
    assert_eq!(plugin_table_count, 0);
}

#[test]
fn pending_v1_snapshot_restores_then_migrates_to_v8() {
    let directory = tempfile::tempdir().expect("create restore directory");
    let store = PersistentStore::open(directory.path()).expect("open current v5 store");
    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-v1.db");
    create_v1_database(&candidate);
    store
        .snapshot_restore_request(&candidate)
        .expect("request v1 snapshot restore");
    drop(store);

    let restored =
        PersistentStore::open(directory.path()).expect("restore and migrate v1 snapshot");
    assert_eq!(restored.revision().expect("read restored revision"), 7);
    assert_eq!(
        restored
            .query_presets(None)
            .expect("query restored presets")
            .items[0]
            .name,
        "V1 first"
    );
    let version: i64 = restored
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read restored version");
    assert_eq!(version, 14);
}

#[test]
fn invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create invalid restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v5 store");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "username": "Preserved live database" })),
            replace_presets: Some(vec![json!({ "name": "Live preset" })]),
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("seed live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open invalid v1 candidate");
    connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                "revision-7",
                json!({ "username": "Invalid candidate", "botPresets": { "legacy": true } })
                    .to_string()
            ],
        )
        .expect("corrupt candidate preset shape");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved live root")
            .value["username"],
        "Preserved live database"
    );
    assert_eq!(
        reopened
            .query_presets(None)
            .expect("query preserved live presets")
            .items[0]
            .name,
        "Live preset"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}

#[test]
fn semantically_invalid_pending_v1_snapshot_preserves_live_database_and_restore_marker() {
    let directory = tempfile::tempdir().expect("create semantic restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open current v5 store");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 0,
            root: Some(json!({ "username": "Preserved semantic live database" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("seed semantic live database");

    let candidate = directory
        .path()
        .join("persistent/snapshots/persistent-semantic-invalid-v1.db");
    create_v1_database(&candidate);
    let connection = rusqlite::Connection::open(&candidate).expect("open semantic v1 candidate");
    connection
        .execute(
            "UPDATE meta SET value = '\"missing-generation\"' WHERE key = 'activeGeneration'",
            [],
        )
        .expect("make active generation semantically invalid");
    drop(connection);
    store
        .snapshot_restore_request(&candidate)
        .expect("request semantic invalid v1 restore");
    drop(store);

    let reopened = PersistentStore::open(directory.path())
        .expect("semantic invalid candidate must not replace live database");
    assert_eq!(reopened.revision().expect("read preserved revision"), 1);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read preserved semantic live root")
            .value["username"],
        "Preserved semantic live database"
    );
    assert!(directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .is_file());
    assert!(candidate.is_file());
}

#[test]
fn schema_configures_the_documented_sqlite_profile() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");

    let integer_pragma = |name: &str| {
        store
            .connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
            .expect("read integer pragma")
    };
    let journal_mode: String = store
        .connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("read journal mode");

    assert_eq!(journal_mode, "wal");
    assert_eq!(integer_pragma("synchronous"), 1);
    assert_eq!(integer_pragma("busy_timeout"), 5_000);
    assert_eq!(integer_pragma("cache_size"), -16_000);
    assert_eq!(integer_pragma("temp_store"), 2);
    assert_eq!(integer_pragma("journal_size_limit"), 67_108_864);
    assert_eq!(integer_pragma("foreign_keys"), 0);
    assert_eq!(integer_pragma("user_version"), 14);
}

#[test]
fn snapshots_create_list_and_restore_on_reopen() {
    let (directory, mut store, database) = open_fixture();
    let snapshot = store
        .snapshot_create("contract-test")
        .expect("create snapshot");
    assert!(std::path::Path::new(&snapshot.path).is_file());
    assert!(snapshot.bytes > 0);
    assert_eq!(store.snapshot_list().expect("list snapshots").len(), 1);

    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Changed after snapshot" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("change database after snapshot");
    store
        .snapshot_restore_request(std::path::Path::new(&snapshot.path))
        .expect("request snapshot restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore snapshot on reopen");
    assert_eq!(restored.revision().expect("read restored revision"), 1);
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(std::path::Path::new(&snapshot.path).is_file());
    assert!(!directory
        .path()
        .join("persistent/snapshots/pending-restore.json")
        .exists());
}

#[test]
fn snapshot_creation_persists_asset_roots_before_returning() {
    let (directory, store, _) = open_fixture();
    let generation = super::active_generation(&store.connection).expect("read active generation");
    let manifest_hash = "a".repeat(64);
    let object_hash = "b".repeat(64);
    let cold_object_hash = "c".repeat(64);
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({
                    "asset": "assets/exact.bin",
                    "inlay": "{{inlay::kept-inlay}}",
                    "coldStoragedChats": ["cold-chat"]
                }))
                .unwrap()
            ],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES (?1, 'assets/missing.bin', NULL, 'asset', 0,
                'application/octet-stream', 'missing', 'bin', NULL, NULL, NULL)",
            [&generation],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height
             ) VALUES (?1, 'assets/exact.bin', ?2, 'asset', 1,
                'application/octet-stream', 'exact', 'bin', NULL, NULL, NULL)",
            rusqlite::params![generation, object_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-chat', ?2, 1, '{}')",
            rusqlite::params![generation, cold_object_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', 'module-1', 1, ?2, 1)",
            rusqlite::params![generation, manifest_hash],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'opaque', 2, 0, '{}')",
            [generation],
        )
        .unwrap();

    let snapshot = store.snapshot_create("asset-roots").unwrap();
    let sidecar = crate::asset_repository::migration_gc::read_snapshot_asset_root_sidecar(
        Path::new(&snapshot.path),
    )
    .expect("read snapshot asset-root sidecar");

    assert_eq!(sidecar.revision, 1);
    assert_eq!(sidecar.roots.manifest_hashes, [manifest_hash].into());
    assert_eq!(
        sidecar.roots.object_hashes,
        [object_hash, cold_object_hash].into()
    );
    assert_eq!(
        sidecar.roots.legacy_asset_keys,
        [
            "assets/exact.bin".to_owned(),
            "assets/missing.bin".to_owned()
        ]
        .into()
    );
    assert_eq!(sidecar.roots.inlay_ids, ["kept-inlay".to_owned()].into());
    assert_eq!(sidecar.roots.cold_keys, ["cold-chat".to_owned()].into());
    assert_eq!(
        sidecar.roots.blockers,
        ["cold-payload-unscanned".to_owned()].into()
    );
    assert!(sidecar.roots.retain_all_objects);
    assert!(
        crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(
            &snapshot.path
        ))
        .is_file()
    );
    drop(directory);
}

#[test]
fn asset_gc_dry_run_keeps_leased_generation_roots_until_release() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let original_payload = cas.prepare_bytes(b"original").unwrap();
    let replacement_payload = cas.prepare_bytes(b"replacement").unwrap();
    let collectable_payload = cas.prepare_bytes(b"collectable").unwrap();
    let original = AssetAlias {
        key: "assets/leased.bin".to_owned(),
        object_hash: Some(original_payload.content_hash.clone()),
        kind: "asset".to_owned(),
        size: original_payload.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Leased".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store.commit_asset_alias(&original, 0).unwrap();
    let lease = store.acquire_revision(first.revision).unwrap();
    let replacement = AssetAlias {
        object_hash: Some(replacement_payload.content_hash.clone()),
        size: replacement_payload.byte_size as i64,
        ..original
    };
    store
        .commit_asset_alias(&replacement, first.revision)
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: original_payload.content_hash.clone(),
                    byte_size: original_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: replacement_payload.content_hash.clone(),
                    byte_size: replacement_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable_payload.content_hash.clone(),
                    byte_size: collectable_payload.byte_size,
                },
            ],
            0,
        )
        .expect("register GC candidates");

    let leased = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(leased
        .marked_hashes
        .contains(&original_payload.content_hash));
    assert!(leased
        .marked_hashes
        .contains(&replacement_payload.content_hash));
    assert_eq!(
        leased.potential_delete_hashes,
        vec![collectable_payload.content_hash.clone()]
    );

    store.release_revision(&lease.lease).unwrap();
    let released = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert_eq!(
        released.marked_hashes,
        vec![replacement_payload.content_hash]
    );
    assert_eq!(
        released
            .potential_delete_hashes
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        [
            collectable_payload.content_hash,
            original_payload.content_hash
        ]
        .into_iter()
        .collect()
    );
    assert!(!released.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_retains_every_catalog_object_for_opaque_plugin_storage() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let first = cas.prepare_bytes(b"plugin-private-a").unwrap();
    let second = cas.prepare_bytes(b"plugin-private-b").unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES (?1, 'opaque-plugin', 22, 0, ?2)",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({
                    "privateEncoding": "cGx1Z2luLWRlZmluZWQtcmVmZXJlbmNl"
                }))
                .unwrap()
            ],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: first.content_hash.clone(),
                    byte_size: first.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: second.content_hash.clone(),
                    byte_size: second.byte_size,
                },
            ],
            0,
        )
        .expect("register plugin liveness candidates");

    let first_page = store.asset_gc_dry_run(1, None, 100, 10).unwrap();
    let second_page = store
        .asset_gc_dry_run(1, first_page.next_cursor.as_deref(), 100, 10)
        .unwrap();
    let mut marked = first_page.report.marked_hashes.clone();
    marked.extend(second_page.report.marked_hashes.clone());
    marked.sort();
    marked.dedup();

    assert_eq!(
        marked,
        [first.content_hash, second.content_hash]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    assert!(first_page.report.potential_delete_hashes.is_empty());
    assert!(second_page.report.potential_delete_hashes.is_empty());
    assert!(first_page.next_cursor.is_some());
    assert!(second_page.next_cursor.is_none());
    for report in [first_page.report, second_page.report] {
        assert!(!report
            .blockers
            .contains(&"plugin-storage-opaque".to_owned()));
        assert!(!report.deletion_enabled);
    }
}

#[test]
fn asset_gc_dry_run_marks_native_cas_references_nested_in_cold_payloads() {
    use super::asset_object_catalog::AssetObjectRegistration;
    use flate2::{
        write::{DeflateEncoder, GzEncoder, ZlibEncoder},
        Compression,
    };
    use std::io::Write;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let nested = cas.prepare_bytes(b"nested-cold-resource").unwrap();
    let nested_url = cas.prepare_bytes(b"nested-cold-url-resource").unwrap();
    let collectable = cas.prepare_bytes(b"not-referenced-by-cold").unwrap();
    let nested_physical_key = format!(
        "assets-v2/objects/{}/{}",
        &nested.content_hash[..2],
        &nested.content_hash[2..]
    );
    let nested_url_physical_key = format!(
        "assets-v2/objects/{}/{}",
        &nested_url.content_hash[..2],
        &nested_url.content_hash[2..]
    );
    let nested_render_url = format!(
        "HTTP://user@RISUASSET.LOCALHOST:8080/{}",
        hex::encode(nested_url_physical_key)
    );
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(
            serde_json::to_string(&json!({
                "character": {
                    "name": "Cold fixture",
                    "roadmap14Unknown": {
                        "nested": [nested_physical_key.clone(), nested_render_url]
                    },
                    "chats": [{
                        "message": [{
                            "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-zlib"
                        }]
                    }]
                }
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let mut cold_bytes = encoder.finish().unwrap();
    let cold = cas.prepare_bytes(&cold_bytes).unwrap();
    let oversized = super::snapshot::decode_cold_payload_with_limit(
        &cas,
        &cold.content_hash,
        cold.byte_size,
        32,
    )
    .expect_err("decoded output over the configured limit must fail closed");
    assert!(oversized.to_string().contains("exceeds the decoded limit"));
    let mut zlib_encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib_encoder
        .write_all(
            serde_json::to_string(&json!({
                "message": { "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-raw" }
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let cold_zlib = cas.prepare_bytes(&zlib_encoder.finish().unwrap()).unwrap();
    let mut deflate_encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    deflate_encoder
        .write_all(
            serde_json::to_string(&json!([nested_physical_key]))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let cold_raw = cas
        .prepare_bytes(&deflate_encoder.finish().unwrap())
        .unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({ "coldstorage": "cold-root" })).unwrap()
            ],
        )
        .unwrap();
    for (key, alias) in [("cold-zlib", &cold_zlib), ("cold-raw", &cold_raw)] {
        store
            .connection
            .execute(
                "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
                 VALUES (?1, ?2, ?3, ?4, '{}')",
                rusqlite::params![generation, key, alias.content_hash, alias.byte_size as i64],
            )
            .unwrap();
    }
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-root', ?2, ?3, '{}')",
            rusqlite::params![generation, cold.content_hash, cold.byte_size as i64],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: cold.content_hash.clone(),
                    byte_size: cold.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: cold_zlib.content_hash.clone(),
                    byte_size: cold_zlib.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: cold_raw.content_hash.clone(),
                    byte_size: cold_raw.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: nested.content_hash.clone(),
                    byte_size: nested.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: nested_url.content_hash.clone(),
                    byte_size: nested_url.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable.content_hash.clone(),
                    byte_size: collectable.byte_size,
                },
            ],
            0,
        )
        .expect("register cold liveness candidates");

    let report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;

    assert!(report.marked_hashes.contains(&cold.content_hash));
    assert!(report.marked_hashes.contains(&cold_zlib.content_hash));
    assert!(report.marked_hashes.contains(&cold_raw.content_hash));
    assert!(report.marked_hashes.contains(&nested.content_hash));
    assert!(report.marked_hashes.contains(&nested_url.content_hash));
    assert_eq!(
        report.potential_delete_hashes,
        vec![collectable.content_hash.clone()]
    );
    assert!(!report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!report
        .blockers
        .contains(&"native-asset-url-unresolved".to_owned()));
    assert!(!report.deletion_enabled);

    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-raw'",
            [&generation],
        )
        .unwrap();
    let missing_nested = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(missing_nested.potential_delete_hashes.is_empty());
    assert!(missing_nested
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-raw', ?2, ?3, '{}')",
            rusqlite::params![generation, cold_raw.content_hash, cold_raw.byte_size as i64],
        )
        .unwrap();

    let mut unknown_encoder = GzEncoder::new(Vec::new(), Compression::default());
    unknown_encoder
        .write_all(b"{\"roadmap14Unknown\":true}")
        .unwrap();
    let unknown = cas
        .prepare_bytes(&unknown_encoder.finish().unwrap())
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-unknown', ?2, ?3, '{}')",
            rusqlite::params![generation, unknown.content_hash, unknown.byte_size as i64],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: unknown.content_hash.clone(),
                byte_size: unknown.byte_size,
            }],
            0,
        )
        .unwrap();
    let unknown_report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(unknown_report
        .marked_hashes
        .contains(&collectable.content_hash));
    assert!(unknown_report.potential_delete_hashes.is_empty());
    assert!(unknown_report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-unknown'",
            [&generation],
        )
        .unwrap();

    let mut unknown_url_encoder = GzEncoder::new(Vec::new(), Compression::default());
    unknown_url_encoder
        .write_all(b"{\"message\":{\"data\":\"HTTP://RISUASSET.LOCALHOST/not-hex\"}}")
        .unwrap();
    let unknown_url = cas
        .prepare_bytes(&unknown_url_encoder.finish().unwrap())
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold-unknown-url', ?2, ?3, '{}')",
            rusqlite::params![
                generation,
                unknown_url.content_hash,
                unknown_url.byte_size as i64
            ],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: unknown_url.content_hash.clone(),
                byte_size: unknown_url.byte_size,
            }],
            0,
        )
        .unwrap();
    let unknown_url_report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(unknown_url_report.potential_delete_hashes.is_empty());
    assert!(unknown_url_report
        .blockers
        .contains(&"native-asset-url-unresolved".to_owned()));
    store
        .connection
        .execute(
            "DELETE FROM cold_aliases WHERE generation = ?1 AND key = 'cold-unknown-url'",
            [generation],
        )
        .unwrap();

    cold_bytes[0] ^= 0xff;
    fs::write(directory.path().join(&cold.physical_key), cold_bytes).unwrap();
    let blocked = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(blocked.marked_hashes.contains(&cold.content_hash));
    assert!(blocked.marked_hashes.contains(&nested.content_hash));
    assert!(blocked.marked_hashes.contains(&nested_url.content_hash));
    assert!(blocked.marked_hashes.contains(&collectable.content_hash));
    assert!(blocked.potential_delete_hashes.is_empty());
    assert!(blocked
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!blocked.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_retains_ambiguous_cross_generation_cold_keys() {
    use super::asset_object_catalog::AssetObjectRegistration;
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(b"{\"message\":[]}")
        .expect("encode cold fixture");
    let cold = cas.prepare_bytes(&encoder.finish().unwrap()).unwrap();
    let collectable = cas.prepare_bytes(b"cross-generation-candidate").unwrap();
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "UPDATE root SET value = ?2 WHERE generation = ?1",
            rusqlite::params![
                generation,
                serde_json::to_string(&json!({ "coldstorage": "shared-cold-key" })).unwrap()
            ],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO root (generation, value) VALUES ('retained-shadow', ?1)",
            [serde_json::to_string(&json!({ "coldstorage": "shared-cold-key" })).unwrap()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'shared-cold-key', ?2, ?3, '{}')",
            rusqlite::params![generation, cold.content_hash, cold.byte_size as i64],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES ('retained-shadow', 'shared-cold-key', NULL, 0, '{}')",
            [],
        )
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: cold.content_hash.clone(),
                    byte_size: cold.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: collectable.content_hash.clone(),
                    byte_size: collectable.byte_size,
                },
            ],
            0,
        )
        .unwrap();

    let report = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;

    assert!(report.marked_hashes.contains(&cold.content_hash));
    assert!(report.marked_hashes.contains(&collectable.content_hash));
    assert!(report.potential_delete_hashes.is_empty());
    assert!(report
        .blockers
        .contains(&"cold-payload-unscanned".to_owned()));
    assert!(!report.deletion_enabled);
}

#[test]
fn asset_gc_dry_run_keeps_detached_export_roots_until_reader_release() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
    let original_payload = cas.prepare_bytes(b"detached-original").unwrap();
    let replacement_payload = cas.prepare_bytes(b"detached-replacement").unwrap();
    let original = AssetAlias {
        key: "assets/detached.bin".to_owned(),
        object_hash: Some(original_payload.content_hash.clone()),
        kind: "asset".to_owned(),
        size: original_payload.byte_size as i64,
        mime: "application/octet-stream".to_owned(),
        name: "Detached".to_owned(),
        ext: "bin".to_owned(),
        inlay_type: None,
        width: None,
        height: None,
        metadata: json!({}),
    };
    let first = store.commit_asset_alias(&original, 0).unwrap();
    let mut prepared = store.prepare_risu_save_export(first.revision).unwrap();
    let replacement = AssetAlias {
        object_hash: Some(replacement_payload.content_hash.clone()),
        size: replacement_payload.byte_size as i64,
        ..original
    };
    store
        .commit_asset_alias(&replacement, first.revision)
        .unwrap();
    store
        .asset_object_catalog()
        .register(
            &[
                AssetObjectRegistration {
                    object_hash: original_payload.content_hash.clone(),
                    byte_size: original_payload.byte_size,
                },
                AssetObjectRegistration {
                    object_hash: replacement_payload.content_hash.clone(),
                    byte_size: replacement_payload.byte_size,
                },
            ],
            0,
        )
        .expect("register GC candidates");

    let detached = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert!(detached
        .marked_hashes
        .contains(&original_payload.content_hash));
    assert!(detached
        .marked_hashes
        .contains(&replacement_payload.content_hash));
    assert!(detached.potential_delete_hashes.is_empty());

    let reader = prepared.take_reader().expect("take detached reader");
    prepared.release(reader).expect("release detached reader");
    let released = store.asset_gc_dry_run(16, None, 100, 10).unwrap().report;
    assert_eq!(
        released.marked_hashes,
        vec![replacement_payload.content_hash]
    );
    assert_eq!(
        released.potential_delete_hashes,
        vec![original_payload.content_hash]
    );
}

#[test]
fn detached_export_registry_keeps_exact_v8_roots_until_reader_release() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let generation = super::active_generation(&store.connection).expect("read generation");
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES (?1, 'assets/detached.bin', ?2, 'asset', 1,
                'application/octet-stream', 'Detached', 'bin', NULL, NULL, NULL, '{}')",
            rusqlite::params![generation, "ab".repeat(32)],
        )
        .expect("insert detached asset alias root");
    store
        .connection
        .execute(
            "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES (?1, 'root-module-assets', '0', 1, ?2, 1)",
            rusqlite::params![generation, "cd".repeat(32)],
        )
        .expect("insert detached owner head root");
    store
        .connection
        .execute(
            "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES (?1, 'cold/detached', ?2, 1, '{}')",
            rusqlite::params![generation, "ef".repeat(32)],
        )
        .expect("insert detached cold alias root");

    let mut prepared = store
        .prepare_risu_save_export(0)
        .expect("prepare detached reader with v8 roots");
    let roots = store
        .active_readers
        .detached_asset_roots()
        .expect("read detached roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].manifest_hashes, ["cd".repeat(32)].into());
    assert_eq!(
        roots[0].object_hashes,
        ["ab".repeat(32), "ef".repeat(32)].into()
    );

    let reader = prepared.take_reader().expect("take detached reader");
    prepared.release(reader).expect("release detached reader");
    assert!(store
        .active_readers
        .detached_asset_roots()
        .expect("read roots after detached release")
        .is_empty());
}

#[test]
fn android_revision_reader_starts_writable_before_query_only_snapshot_pinning() {
    let flags = super::snapshot::revision_reader_open_flags_for_target(true);

    assert!(flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE));
    assert!(!flags.contains(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY));
}

#[test]
fn checkpoints_accept_both_documented_modes() {
    let (_directory, store, _) = open_fixture();

    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint");
    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("truncate checkpoint");
}

#[test]
fn active_lease_rejects_truncate_and_final_release_truncates_the_wal() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let lease = store.acquire_revision(1).expect("acquire WAL reader");
    store
        .set_app_kv("after-lease", &json!(true))
        .expect("append WAL frame after lease");
    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint with active lease");

    let started = std::time::Instant::now();
    let error = store
        .checkpoint(CheckpointMode::Truncate)
        .expect_err("truncate must reject an active lease");
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(matches!(
        error,
        StoreError::Store { message } if message.contains("active read lease")
    ));

    store
        .release_revision(&lease.lease)
        .expect("release final lease and truncate WAL");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn detached_export_reader_rejects_truncate_until_it_is_released() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let mut prepared = store
        .prepare_risu_save_export(1)
        .expect("prepare detached export reader");
    store
        .set_app_kv("after-detached-export", &json!(true))
        .expect("append WAL frame after detached reader");

    let started = std::time::Instant::now();
    let error = store
        .checkpoint(CheckpointMode::Truncate)
        .expect_err("truncate must reject a detached export reader");
    assert!(started.elapsed() < Duration::from_millis(50));
    assert!(matches!(
        error,
        StoreError::Store { message } if message.contains("active read lease")
    ));

    let reader = prepared.take_reader().expect("take detached export reader");
    prepared
        .release(reader)
        .expect("release detached export reader");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn detached_export_release_stays_prompt_while_an_attached_reader_remains() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let attached = store.acquire_revision(1).expect("acquire attached reader");
    let mut prepared = store
        .prepare_risu_save_export(1)
        .expect("prepare detached export reader");
    store
        .set_app_kv("after-two-readers", &json!(true))
        .expect("append WAL frame after both readers");

    let reader = prepared.take_reader().expect("take detached export reader");
    let started = std::time::Instant::now();
    prepared
        .release(reader)
        .expect("release detached reader with attached reader remaining");
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(
        store
            .read_root(Some(&attached.lease))
            .expect("attached reader remains pinned")
            .revision,
        1
    );
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert!(fs::metadata(&wal_path).expect("read pinned WAL").len() > 0);

    store
        .release_revision(&attached.lease)
        .expect("release final attached reader");
    assert_eq!(fs::metadata(wal_path).expect("read final WAL").len(), 0);
}

#[test]
fn dropping_store_with_active_lease_reopens_latest_state_and_truncates_recovered_wal() {
    let (directory, mut store, _) = open_fixture();
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable automatic checkpoints");
    let lease = store.acquire_revision(1).expect("acquire WAL reader");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Writer survives lease drop" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("append writer state while lease is active");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    assert!(fs::metadata(&wal_path).expect("read pinned WAL").len() > 0);
    drop(store);

    let reopened = PersistentStore::open(directory.path()).expect("reopen after active lease drop");
    assert_eq!(reopened.revision().expect("read reopened revision"), 2);
    assert_eq!(
        reopened
            .read_root(None)
            .expect("read reopened writer state")
            .value["username"],
        "Writer survives lease drop"
    );
    assert!(matches!(
        reopened.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
    assert_eq!(
        fs::metadata(&wal_path).expect("read recovered WAL").len(),
        0
    );
}

fn snapshots_dir(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().join("persistent/snapshots")
}

fn sparse_snapshot(path: &Path, bytes: u64) {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .expect("create sparse snapshot");
    file.set_len(bytes).expect("size sparse snapshot");
    thread::sleep(Duration::from_millis(10));
}

fn stage_root(store: &mut PersistentStore, username: &str) -> String {
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &json!({ "username": username }))
        .expect("stage replacement root");
    staging.staging_id
}

#[test]
fn ninth_snapshot_removes_the_oldest_and_leaves_eight() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    let mut created = Vec::new();

    for index in 0..9 {
        created.push(
            store
                .snapshot_create(&format!("rotation-{index}"))
                .expect("create rotating snapshot")
                .path,
        );
        thread::sleep(Duration::from_millis(10));
    }

    let listed = store.snapshot_list().expect("list rotated snapshots");
    assert_eq!(listed.len(), 8);
    assert!(!Path::new(&created[0]).exists());
    assert!(
        !crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(
            &created[0]
        ))
        .exists()
    );
    assert!(created[1..].iter().all(|path| Path::new(path).is_file()));
    assert!(created[1..].iter().all(|path| {
        crate::asset_repository::migration_gc::snapshot_asset_root_sidecar_path(Path::new(path))
            .is_file()
    }));
}

#[test]
fn byte_rotation_uses_the_512_mib_floor_and_removes_oldest_first() {
    const MIB: u64 = 1024 * 1024;
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    let snapshots = snapshots_dir(&directory);
    let mut sparse = Vec::new();
    for index in 0..6 {
        let path = snapshots.join(format!("persistent-sparse-{index}.db"));
        sparse_snapshot(&path, 100 * MIB);
        sparse.push(path);
    }

    let created = store
        .snapshot_create("floor-rotation")
        .expect("create snapshot and rotate");
    let listed = store.snapshot_list().expect("list floor-rotated snapshots");
    let total: u64 = listed.iter().map(|snapshot| snapshot.bytes).sum();

    assert!(total <= 512 * MIB);
    assert!(!sparse[0].exists());
    assert!(sparse[1..].iter().all(|path| path.is_file()));
    assert!(Path::new(&created.path).is_file());
}

#[test]
fn byte_rotation_uses_four_times_current_logical_database_size() {
    const MIB: u64 = 1024 * 1024;
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch(
            "CREATE TABLE rotation_payload (value BLOB);\n             INSERT INTO rotation_payload VALUES (zeroblob(140 * 1024 * 1024));",
        )
        .expect("grow logical database above the rotation floor branch");
    let page_count: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("read page count");
    let page_size: i64 = store
        .connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("read page size");
    let logical_bytes = (page_count * page_size) as u64;
    assert!(logical_bytes * 4 > 512 * MIB);

    let snapshots = snapshots_dir(&directory);
    let sparse_bytes = logical_bytes * 2 / 3;
    let mut sparse = Vec::new();
    for index in 0..5 {
        let path = snapshots.join(format!("persistent-large-sparse-{index}.db"));
        sparse_snapshot(&path, sparse_bytes);
        sparse.push(path);
    }

    let created = store
        .snapshot_create("large-rotation")
        .expect("create snapshot and rotate using logical size");
    let listed = store.snapshot_list().expect("list size-rotated snapshots");
    let total: u64 = listed.iter().map(|snapshot| snapshot.bytes).sum();

    assert!(total <= logical_bytes * 4);
    assert!(total > 512 * MIB);
    assert!(!sparse[0].exists());
    assert!(sparse[1..].iter().all(|path| path.is_file()));
    assert!(Path::new(&created.path).is_file());
}

#[test]
fn pending_restore_reopens_cleanly_after_the_store_drops_an_active_lease() {
    let (directory, mut store, database) = open_fixture();
    let lease = store
        .acquire_revision(1)
        .expect("acquire pre-restore lease");
    let snapshot = store
        .snapshot_create("lease-restore")
        .expect("create snapshot while lease is active");
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Writer after restore snapshot" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("commit after restore snapshot");
    store
        .snapshot_restore_request(Path::new(&snapshot.path))
        .expect("prepare restore while lease is active");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("apply pending restore");
    assert_eq!(restored.revision().expect("read restored revision"), 1);
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(matches!(
        restored.read_root(Some(&lease.lease)),
        Err(StoreError::SnapshotReleased)
    ));
}

#[test]
fn pending_restore_target_and_pre_restore_snapshot_survive_rotation() {
    let (directory, mut store, database) = open_fixture();
    let target = store
        .snapshot_create("restore-target")
        .expect("create restore target");
    for index in 0..7 {
        store
            .snapshot_create(&format!("fill-{index}"))
            .expect("fill snapshot rotation");
        thread::sleep(Duration::from_millis(10));
    }
    store
        .commit(&WorkingSetCommit {
            expected_revision: 1,
            root: Some(json!({ "username": "Current before restore" })),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            asset_owner_heads: None,
            plugin_storage: None,
        })
        .expect("change current data");
    store
        .snapshot_restore_request(Path::new(&target.path))
        .expect("request restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("apply pending restore");
    assert_eq!(
        restored
            .materialize(None)
            .expect("materialize restored data"),
        database
    );
    assert!(Path::new(&target.path).is_file());
    assert!(!snapshots_dir(&directory)
        .join("pending-restore.json")
        .exists());
    assert!(
        restored
            .snapshot_list()
            .expect("list rotated restore snapshots")
            .len()
            <= 8
    );

    let pre_restore = restored
        .snapshot_list()
        .expect("list restore snapshots")
        .into_iter()
        .find(|snapshot| snapshot.path.contains("pre-restore"))
        .expect("pre-restore snapshot remains after rotation");
    let connection =
        rusqlite::Connection::open(pre_restore.path).expect("open pre-restore snapshot");
    let value: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = 'revision-1'",
            [],
            |row| row.get(0),
        )
        .expect("read pre-restore root");
    assert_eq!(
        serde_json::from_str::<Value>(&value).expect("parse root")["username"],
        "Current before restore"
    );
}

#[test]
fn invalid_restore_candidates_preserve_current_data_and_marker() {
    for wrong_version in [false, true] {
        let (directory, mut store, database) = open_fixture();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(json!({ "username": "Current protected data" })),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: None,
            })
            .expect("change current data");
        let expected = store.materialize(None).expect("materialize current data");
        assert_ne!(expected, database);
        let candidate = snapshots_dir(&directory).join(if wrong_version {
            "persistent-wrong-version.db"
        } else {
            "persistent-corrupt.db"
        });
        if wrong_version {
            let connection =
                rusqlite::Connection::open(&candidate).expect("create wrong-version database");
            connection
                .execute_batch("PRAGMA user_version = 15;")
                .expect("set wrong schema version");
        } else {
            fs::write(&candidate, b"not a sqlite database").expect("write corrupt database");
        }
        store
            .snapshot_restore_request(&candidate)
            .expect("write pending marker");
        drop(store);

        let reopened =
            PersistentStore::open(directory.path()).expect("reopen after rejected restore");
        assert_eq!(
            reopened
                .materialize(None)
                .expect("read preserved current data"),
            expected
        );
        assert!(snapshots_dir(&directory)
            .join("pending-restore.json")
            .exists());
    }
}

#[test]
fn schema_v12_adds_only_the_global_empty_asset_object_catalog_after_cold_v11() {
    let directory = tempfile::tempdir().expect("create v12 schema directory");
    let store = PersistentStore::open(directory.path()).expect("open fresh v12 store");
    assert_eq!(
        store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read current version"),
        14
    );
    assert_eq!(
        table_columns(&store.connection, "asset_objects")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        ["object_hash", "byte_size", "created_at_ms"]
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row
                .get::<_, i64>(0))
            .expect("count fresh catalog"),
        0
    );
    assert!(super::GENERATION_TABLES
        .iter()
        .all(|(table, _)| *table != "asset_objects"));
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM cold_payload_authority", [], |row| row
                .get::<_, i64>(0))
            .expect("count cold authority rows"),
        1
    );
}

#[test]
fn schema_v12_migrates_v11_without_scanning_or_backfilling_cas_objects() {
    let directory = tempfile::tempdir().expect("create v11 migration directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_objects;
             PRAGMA user_version = 11;",
        )
        .expect("create v11 fixture");
    drop(store);
    let sentinel = directory
        .path()
        .join("assets-v2/objects/aa/untracked-v11-object");
    fs::create_dir_all(sentinel.parent().expect("read sentinel parent"))
        .expect("create sentinel shard");
    fs::write(&sentinel, b"not a catalog candidate").expect("write sentinel object");

    let migrated = PersistentStore::open(directory.path()).expect("migrate v11 store");
    assert_eq!(
        migrated
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read migrated version"),
        14
    );
    assert_eq!(
        migrated
            .connection
            .query_row("SELECT COUNT(*) FROM asset_objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count empty migrated catalog"),
        0
    );
    assert_eq!(
        fs::read(sentinel).expect("read untracked sentinel"),
        b"not a catalog candidate"
    );
}

#[test]
fn schema_v12_catalog_collision_rolls_back_without_advancing_v11() {
    let directory = tempfile::tempdir().expect("create v12 collision directory");
    let store = PersistentStore::open(directory.path()).expect("create current store");
    store
        .connection
        .execute_batch(
            "DROP INDEX logical_sync_device_ack_proofs_local_generation;
             DROP TABLE logical_sync_device_ack_proofs;
             DROP INDEX logical_sync_devices_status;
             DROP TABLE logical_sync_devices;
             DROP TABLE asset_objects;
             CREATE TABLE asset_objects (collision_marker TEXT NOT NULL);
             PRAGMA user_version = 11;",
        )
        .expect("create colliding v11 fixture");
    drop(store);

    assert!(PersistentStore::open(directory.path()).is_err());
    let connection = Connection::open(directory.path().join("persistent/persistent.db"))
        .expect("reopen colliding v11 fixture");
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .expect("read rolled-back version"),
        11
    );
    assert_eq!(
        table_columns(&connection, "asset_objects")
            .into_iter()
            .map(|column| column.0)
            .collect::<Vec<_>>(),
        ["collision_marker"]
    );
}

#[test]
fn asset_object_catalog_is_idempotent_conflict_safe_and_stably_paged() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let first = AssetObjectRegistration {
        object_hash: "11".repeat(32),
        byte_size: 11,
    };
    let second = AssetObjectRegistration {
        object_hash: "22".repeat(32),
        byte_size: 22,
    };
    let third = AssetObjectRegistration {
        object_hash: "33".repeat(32),
        byte_size: 33,
    };
    store
        .asset_object_catalog()
        .register(&[first.clone()], 5)
        .expect("register first object");
    store
        .asset_object_catalog()
        .register(&[first.clone()], 99)
        .expect("re-register same object");
    store
        .asset_object_catalog()
        .register(&[second.clone(), third.clone()], 5)
        .expect("register remaining objects");
    assert!(store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: first.object_hash.clone(),
                byte_size: 12,
            }],
            10,
        )
        .is_err());
    assert!(store
        .asset_object_catalog()
        .register(&[first.clone()], -1)
        .is_err());
    assert!(store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: "not-a-hash".to_owned(),
                byte_size: 1,
            }],
            10,
        )
        .is_err());

    let first_page = store
        .query_asset_object_catalog(2, None)
        .expect("query first page");
    assert_eq!(
        first_page
            .items
            .iter()
            .map(|item| (&item.object_hash, item.created_at_ms))
            .collect::<Vec<_>>(),
        [(&first.object_hash, 5), (&second.object_hash, 5)]
    );
    let second_page = store
        .query_asset_object_catalog(2, first_page.next_cursor.as_deref())
        .expect("query second page");
    assert_eq!(second_page.items.len(), 1);
    assert_eq!(second_page.items[0].object_hash, third.object_hash);
    assert!(second_page.next_cursor.is_none());
    assert!(store.query_asset_object_catalog(0, None).is_err());
    assert!(store
        .query_asset_object_catalog(1, Some("not-an-opaque-cursor"))
        .is_err());
}

#[test]
fn snapshot_restore_without_newer_catalog_rows_leaves_objects_untracked() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create catalog restore directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let snapshot = store
        .snapshot_create("before-catalog-registration")
        .expect("create empty-catalog snapshot");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"survives without restored inventory")
        .expect("prepare untracked object");
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            1,
        )
        .expect("register post-snapshot object");
    store
        .snapshot_restore_request(Path::new(&snapshot.path))
        .expect("request empty-catalog restore");
    drop(store);

    let restored = PersistentStore::open(directory.path()).expect("restore catalog snapshot");
    assert!(restored
        .query_asset_object_catalog(16, None)
        .expect("query restored catalog")
        .items
        .is_empty());
    assert_eq!(
        cas.stat_object(&prepared.content_hash)
            .expect("stat surviving object"),
        Some(prepared.byte_size)
    );
}

#[test]
fn asset_gc_catalog_retains_future_rows_and_aborts_on_size_mismatch() {
    use super::asset_object_catalog::AssetObjectRegistration;

    let directory = tempfile::tempdir().expect("create conservative catalog directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
    let prepared = cas
        .prepare_bytes(b"future catalog object")
        .expect("prepare future object");
    store
        .asset_object_catalog()
        .register(
            &[AssetObjectRegistration {
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            }],
            200,
        )
        .expect("register future catalog object");

    let future = store
        .asset_gc_dry_run(16, None, 100, 10)
        .expect("classify future catalog row");
    assert_eq!(
        future.report.grace_retained_hashes,
        vec![prepared.content_hash.clone()]
    );
    assert!(!future.report.deletion_enabled);

    store
        .connection
        .execute(
            "UPDATE asset_objects SET byte_size = byte_size + 1 WHERE object_hash = ?1",
            [&prepared.content_hash],
        )
        .expect("corrupt catalog size fixture");
    assert!(store.asset_gc_dry_run(16, None, 300, 10).is_err());
}

#[test]
fn replacements_snapshot_only_nonzero_revisions_and_abort_on_snapshot_failure() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    assert!(store
        .snapshot_list()
        .expect("list seed snapshots")
        .is_empty());

    let replacement = stage_root(&mut store, "Revision two");
    store
        .replace_commit(&replacement, Some(1))
        .expect("activate protected replacement");
    let snapshots = store.snapshot_list().expect("list pre-replace snapshots");
    assert_eq!(snapshots.len(), 1);
    let snapshot =
        rusqlite::Connection::open(&snapshots[0].path).expect("open pre-replace snapshot");
    let revision: String = snapshot
        .query_row(
            "SELECT value FROM meta WHERE key = 'currentRevision'",
            [],
            |row| row.get(0),
        )
        .expect("read snapshotted revision");
    assert_eq!(revision, "1");
    drop(snapshot);

    let failed = stage_root(&mut store, "Must not activate");
    for entry in fs::read_dir(snapshots_dir(&directory)).expect("read snapshots directory") {
        let path = entry.expect("read snapshot entry").path();
        fs::remove_file(path).expect("remove snapshot file");
    }
    fs::remove_dir(snapshots_dir(&directory)).expect("remove snapshots directory");
    fs::write(snapshots_dir(&directory), b"blocks directory creation")
        .expect("block snapshot directory");
    assert!(store.replace_commit(&failed, Some(2)).is_err());
    assert_eq!(store.revision().expect("read preserved revision"), 2);
    assert_eq!(
        store.read_root(None).expect("read preserved root").value["username"],
        "Revision two"
    );
}

#[test]
fn prepared_replacement_rechecks_revision_after_snapshot_before_activation() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Prepared replacement");

    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared
        .create_snapshot()
        .expect("create snapshot on dedicated connection");

    let competing = stage_root(&mut store, "Competing revision");
    store
        .replace_commit(&competing, Some(1))
        .expect("activate competing revision");
    let error = store
        .finish_prepared_replace(authorized)
        .expect_err("prepared replacement must recheck revision");

    assert!(matches!(
        error,
        StoreError::RevisionConflict {
            expected: 1,
            actual: 2
        }
    ));
    assert_eq!(store.revision().expect("read current revision"), 2);
    assert_eq!(
        store.read_root(None).expect("read current root").value["username"],
        "Competing revision"
    );
    store
        .replace_abort(&replacement)
        .expect("prepared staging remains abortable");
}

#[test]
fn prepared_replacement_commits_activation_marker_with_new_revision() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Peer clone");
    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared
        .create_snapshot()
        .expect("create snapshot on dedicated connection");

    let committed = store
        .finish_prepared_replace_with_app_kv(
            authorized,
            "peerCloneActiveManifest",
            &json!({ "manifestId": "manifest-1", "revision": 2 }),
        )
        .expect("activate replacement and marker");

    assert_eq!(committed.revision, 2);
    assert_eq!(
        store
            .get_app_kv("peerCloneActiveManifest")
            .expect("read activation marker"),
        Some(json!({ "manifestId": "manifest-1", "revision": 2 }))
    );
    assert_eq!(
        store.read_root(None).expect("read current root").value["username"],
        "Peer clone"
    );
}

#[test]
fn prepared_replacement_rolls_back_when_activation_marker_write_fails() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let seed = stage_root(&mut store, "Seed");
    store
        .replace_commit(&seed, Some(0))
        .expect("activate revision one");
    let replacement = stage_root(&mut store, "Must not activate");
    let prepared = store
        .prepare_replace_commit(&replacement, Some(1))
        .expect("prepare replacement");
    let authorized = prepared
        .create_snapshot()
        .expect("create snapshot on dedicated connection");
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER reject_peer_clone_marker
             BEFORE INSERT ON app_kv
             WHEN NEW.key = 'peerCloneActiveManifest'
             BEGIN
                 SELECT RAISE(ABORT, 'marker rejected');
             END;",
        )
        .expect("install marker failure trigger");

    store
        .finish_prepared_replace_with_app_kv(
            authorized,
            "peerCloneActiveManifest",
            &json!({ "manifestId": "manifest-1", "revision": 2 }),
        )
        .expect_err("marker failure must abort the replacement transaction");

    assert_eq!(store.revision().expect("read preserved revision"), 1);
    assert_eq!(
        store.read_root(None).expect("read preserved root").value["username"],
        "Seed"
    );
    assert_eq!(
        store
            .get_app_kv("peerCloneActiveManifest")
            .expect("read absent activation marker"),
        None
    );
    store
        .replace_abort(&replacement)
        .expect("rolled back staging remains abortable");
}

#[test]
fn checkpoint_truncate_reports_busy_and_truncates_when_unblocked() {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let store = PersistentStore::open(directory.path()).expect("open persistent store");
    store
        .connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0; INSERT INTO app_kv VALUES ('first', '1');")
        .expect("create initial WAL frames");
    let database_path = directory.path().join("persistent/persistent.db");
    let wal_path = PathBuf::from(format!("{}-wal", database_path.display()));
    store
        .checkpoint(CheckpointMode::Passive)
        .expect("passive checkpoint");
    assert!(fs::metadata(&wal_path).expect("read passive WAL").len() > 0);

    let reader = rusqlite::Connection::open(&database_path).expect("open blocking reader");
    reader
        .execute_batch("BEGIN; SELECT value FROM app_kv WHERE key = 'first';")
        .expect("hold read snapshot");
    store
        .connection
        .execute("INSERT INTO app_kv VALUES ('second', '2')", [])
        .expect("write newer WAL frame");
    store
        .connection
        .busy_timeout(Duration::ZERO)
        .expect("disable checkpoint wait");
    assert!(store.checkpoint(CheckpointMode::Truncate).is_err());
    reader
        .execute_batch("ROLLBACK")
        .expect("release read snapshot");

    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("truncate checkpoint");
    assert_eq!(
        fs::metadata(&wal_path).expect("read truncated WAL").len(),
        0
    );
}

#[test]
fn store_errors_serialize_to_the_command_contract() {
    assert_eq!(
        serde_json::to_value(StoreError::RevisionConflict {
            expected: 12,
            actual: 13,
        })
        .expect("serialize revision conflict"),
        json!({ "code": "revision-conflict", "expected": 12, "actual": 13 })
    );
    assert_eq!(
        serde_json::to_value(StoreError::SnapshotReleased).expect("serialize released snapshot"),
        json!({ "code": "snapshot-released" })
    );
    assert_eq!(
        serde_json::to_value(ConversationPage {
            revision: 4,
            items: vec![],
            next_cursor: None,
        })
        .expect("serialize empty page"),
        json!({ "revision": 4, "items": [] })
    );
}
