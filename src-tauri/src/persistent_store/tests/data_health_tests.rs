use super::*;
use crate::data_health::{codes, Findings, Severity};
use crate::local_backup::NeverCancelled;

fn healthy_fixture() -> (tempfile::TempDir, PersistentStore) {
    stored(fixture())
}

#[test]
fn repairing_a_missing_or_stale_chat_page_selects_the_first_conversation() {
    for missing in [true, false] {
        let (_directory, mut store) = healthy_fixture();
        store.connection.execute(if missing {
            "UPDATE characters SET detail=json_remove(detail,'$.chatPage') WHERE character_id='char-a'"
        } else {
            "UPDATE characters SET detail=json_set(detail,'$.chatPage',999) WHERE character_id='char-a'"
        }, []).unwrap();
        let findings = scan(&mut store);
        assert!(findings.items.iter().any(|finding| finding.code == codes::REFERENCE_MISSING
            && finding.locator.as_ref().is_some_and(|locator| locator.source_path == "$.chatPage")));
        let diagnosis = crate::data_health::ScanResult::new(store.revision().unwrap(), 1, crate::data_health::ScanDepth::Quick, findings);
        let candidates = crate::data_health::repair::plan(&diagnosis).into_iter().filter(|candidate|
            matches!(&candidate.action, crate::data_health::repair::RepairAction::SelectFirstConversation { character_id } if character_id == "char-a")
        ).collect::<Vec<_>>();
        store.apply_repair(store.revision().unwrap(), &candidates, 10).unwrap();
        assert_eq!(store.read_character("char-a", None).unwrap().unwrap().value["chatPage"], 0);
        assert!(scan(&mut store).items.iter().all(|finding|
            finding.locator.as_ref().is_none_or(|locator| locator.source_path != "$.chatPage")));
    }
}

/// The root keeps the selected preset and persona the way the app stores them, and the personas
/// it selects from.
fn selection_fixture(preset: Value, persona: Value) -> (tempfile::TempDir, PersistentStore) {
    let mut database = fixture();
    database["botPresetsId"] = preset;
    database["selectedPersona"] = persona;
    database["personas"] = json!([
        {"id": "persona-a", "name": "Persona A", "personaPrompt": "", "icon": ""},
        {"id": "persona-b", "name": "Persona B", "personaPrompt": "", "icon": ""}
    ]);
    stored(database)
}

fn stored(database: Value) -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let staging = store.replace_begin().expect("begin staged replacement");
    store
        .replace_put_root(&staging.staging_id, &staged_root(&database))
        .expect("stage fixture root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"].as_array().expect("fixture presets"),
        )
        .expect("stage fixture presets");
    store
        .replace_add_characters(
            &staging.staging_id,
            database["characters"].as_array().expect("fixture characters"),
        )
        .expect("stage fixture characters");
    store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit fixture");
    (directory, store)
}

pub(super) fn scan(store: &mut PersistentStore) -> Findings {
    let revision = store.revision().expect("read revision");
    let lease = store
        .acquire_revision(revision)
        .expect("acquire revision lease")
        .lease;
    let findings = store
        .data_health_reader(&lease)
        .expect("open the diagnosis reader")
        .scan(256, &NeverCancelled)
        .expect("scan the live library");
    store.release_revision(&lease).expect("release lease");
    findings
}

fn insert_alias(store: &PersistentStore, key: &str, object_hash: &str, size: i64) {
    let generation = active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "INSERT INTO asset_aliases (generation,logical_key,object_hash,kind,size,mime,name,ext,inlay_type,width,height,metadata)
             VALUES (?1,?2,?3,'asset',?4,'application/octet-stream','payload','bin',NULL,NULL,NULL,'{}')",
            params![generation, key, object_hash, size],
        )
        .expect("insert synthetic alias");
}

#[test]
fn live_scan_reports_dangling_references_without_refusing_the_library() {
    let (_directory, mut store) = healthy_fixture();
    let findings = scan(&mut store);
    assert_eq!(findings.omitted, 0);
    assert!(
        findings
            .items
            .iter()
            .all(|finding| finding.severity != Severity::Blocking),
        "a healthy library has no blocking finding: {:?}",
        findings.items
    );
    let dangling = findings
        .items
        .iter()
        .find(|finding| finding.code == codes::REFERENCE_MISSING)
        .expect("the fixture references assets it does not register");
    assert_eq!(dangling.severity, Severity::Degraded);
    assert!(
        dangling.locator.is_some() && dangling.target.is_some(),
        "a reference finding locates itself and names its target"
    );
}

#[test]
fn live_scan_reports_an_alias_whose_object_is_absent_or_differs() {
    let (_directory, mut store) = healthy_fixture();
    insert_alias(&store, "assets/absent", &"3c".repeat(32), 4);
    let absent = scan(&mut store);
    let finding = absent
        .items
        .iter()
        .find(|finding| finding.code == codes::ALIAS_OBJECT_ABSENT)
        .expect("an alias without a stored object is reported");
    assert_eq!(finding.severity, Severity::Blocking);
    assert_eq!(finding.owner.kind, "asset");
    assert_eq!(finding.owner.id, "assets/absent");

    let (_stored_directory, mut stored) = healthy_fixture();
    let cas =
        crate::asset_repository::PayloadCas::new(stored.repository_root()).expect("open the CAS");
    let stored_payload = cas.prepare_bytes(b"synthetic").expect("store a payload");
    insert_alias(&stored, "assets/mismatch", &stored_payload.content_hash, 4);
    let mismatch = scan(&mut stored);
    let finding = mismatch
        .items
        .iter()
        .find(|finding| finding.code == codes::ALIAS_OBJECT_MISMATCH)
        .expect("an alias whose stored object has another size is reported");
    assert_eq!(finding.owner.id, "assets/mismatch");
}

#[test]
fn live_scan_keeps_going_past_a_damaged_record() {
    let (_directory, mut store) = healthy_fixture();
    let generation = active_generation(&store.connection).expect("read active generation");
    let damaged = store
        .connection
        .execute(
            "UPDATE characters SET name='wrong' WHERE generation=?1",
            [&generation],
        )
        .expect("damage the derived character names");
    assert!(damaged > 1, "the fixture needs several characters");
    let findings = scan(&mut store);
    assert_eq!(
        findings
            .items
            .iter()
            .filter(|finding| finding.code == codes::RECORD_INVALID)
            .count(),
        damaged
    );
    assert!(
        findings
            .items
            .iter()
            .any(|finding| finding.code == codes::REFERENCE_MISSING),
        "the reference scan still runs after damaged records"
    );
}

#[test]
fn a_conversation_whose_character_is_gone_is_reported_as_an_orphan() {
    let (_directory, mut store) = healthy_fixture();
    let generation = active_generation(&store.connection).expect("read active generation");
    store
        .connection
        .execute(
            "UPDATE conversations SET character_id='char-gone' WHERE generation=?1",
            [&generation],
        )
        .expect("orphan the conversations");

    let findings = scan(&mut store);
    let finding = findings
        .items
        .iter()
        .find(|finding| finding.code == codes::RECORD_ORPHAN)
        .expect("a conversation with no character is reported");
    assert_eq!(finding.severity, Severity::Blocking);
    assert_eq!(finding.owner.kind, "conversations");
}

fn dangling_selections(findings: &Findings) -> Vec<String> {
    findings
        .items
        .iter()
        .filter(|finding| finding.code == codes::REFERENCE_MISSING)
        .filter_map(|finding| finding.locator.as_ref())
        .map(|locator| locator.source_path.clone())
        .filter(|path| path == "$.botPresetsId" || path == "$.selectedPersona")
        .collect()
}

#[test]
fn a_preset_and_persona_selected_by_id_are_not_reported() {
    let (_directory, mut store) = selection_fixture(json!("preset-alpha"), json!("persona-b"));
    assert_eq!(dangling_selections(&scan(&mut store)), Vec::<String>::new());
}

#[test]
fn a_preset_and_persona_selected_by_index_are_not_reported() {
    let (_directory, mut store) = selection_fixture(json!(1), json!(1));
    assert_eq!(dangling_selections(&scan(&mut store)), Vec::<String>::new());
}

fn diagnosis_of(store: &mut PersistentStore) -> crate::data_health::ScanResult {
    let findings = scan(store);
    crate::data_health::ScanResult::new(store.revision().unwrap(), 1, crate::data_health::ScanDepth::Quick, findings)
}

#[test]
fn a_selected_preset_or_persona_that_is_gone_is_reported_until_the_first_is_selected() {
    use crate::data_health::repair::RepairAction;
    for (preset, persona) in [
        (json!("preset-gone"), json!("persona-gone")),
        (json!(5), json!(5)),
    ] {
        let (_directory, mut store) = selection_fixture(preset, persona);
        let findings = scan(&mut store);
        assert_eq!(dangling_selections(&findings), ["$.botPresetsId", "$.selectedPersona"]);
        let diagnosis =
            crate::data_health::ScanResult::new(store.revision().unwrap(), 1, crate::data_health::ScanDepth::Quick, findings);
        let candidates = crate::data_health::repair::plan(&diagnosis)
            .into_iter()
            .filter(|candidate| matches!(candidate.action, RepairAction::SelectFirstPreset | RepairAction::SelectFirstPersona))
            .collect::<Vec<_>>();
        assert_eq!(candidates.len(), 2);
        store.apply_repair(store.revision().unwrap(), &candidates, 10).unwrap();
        let root = store.read_root(None).unwrap().value;
        // The first preset is the first configured one, not the first by id.
        assert_eq!(root["botPresetsId"], "preset-beta");
        assert_eq!(root["selectedPersona"], "persona-a");
        assert_eq!(dangling_selections(&scan(&mut store)), Vec::<String>::new());
    }
}

fn save_root(store: &mut PersistentStore) -> Result<(), StoreError> {
    let mut root = store.read_root(None).unwrap().value;
    root["username"] = json!("Saved");
    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit { root: Some(root), ..empty_working_set_commit(revision) })
        .map(|_| ())
}

#[test]
fn an_embedded_module_without_an_id_is_given_one_so_the_root_saves_again() {
    use crate::data_health::repair::RepairAction;
    let (_directory, mut store) = selection_fixture(json!("preset-alpha"), json!("persona-a"));
    // A library activated before every embedded module was given an id.
    let generation = active_generation(&store.connection).unwrap();
    store
        .connection
        .execute(
            "UPDATE root SET value=json_set(value,'$.personas[1].embeddedModule',json('{\"name\":\"Embedded\"}')) WHERE generation=?1",
            [&generation],
        )
        .unwrap();
    assert!(save_root(&mut store).is_err(), "the root cannot be saved while the module has no id");

    let candidates = crate::data_health::repair::plan(&diagnosis_of(&mut store))
        .into_iter()
        .filter(|candidate| matches!(candidate.action, RepairAction::AssignEmbeddedModuleId { .. }))
        .collect::<Vec<_>>();
    assert_eq!(
        candidates.iter().map(|candidate| &candidate.action).collect::<Vec<_>>(),
        [&RepairAction::AssignEmbeddedModuleId { persona_id: "persona-b".into() }]
    );
    store.apply_repair(store.revision().unwrap(), &candidates, 10).unwrap();
    let root = store.read_root(None).unwrap().value;
    let module = &root["personas"][1]["embeddedModule"];
    uuid::Uuid::parse_str(module["id"].as_str().expect("the module has an id")).expect("a fresh uuid");
    assert_eq!(module["name"], "Embedded");
    assert!(scan(&mut store).items.iter().all(|finding| {
        finding.locator.as_ref().is_none_or(|locator| locator.source_path != "$.embeddedModule.id")
    }));
    save_root(&mut store).expect("the root saves again");
}

fn scan_bounded(store: &mut PersistentStore, limit: usize) -> Findings {
    let revision = store.revision().expect("read revision");
    let lease = store
        .acquire_revision(revision)
        .expect("acquire revision lease")
        .lease;
    let findings = store
        .data_health_reader(&lease)
        .expect("open the diagnosis reader")
        .scan(limit, &NeverCancelled)
        .expect("scan the live library");
    store.release_revision(&lease).expect("release lease");
    findings
}

fn remove_local_body(store: &PersistentStore, hash: &str) {
    let cas =
        crate::asset_repository::PayloadCas::new(store.repository_root()).expect("open the CAS");
    std::fs::remove_file(
        cas.object_path(hash)
            .expect("object path")
            .expect("the body is stored here"),
    )
    .expect("remove the local body");
}

/// An alias whose body this device no longer holds, as an offload or a receive under the remote
/// policy leaves it.
fn alias_without_local_body(store: &PersistentStore, key: &str, body: &[u8]) -> (String, u64) {
    let cas =
        crate::asset_repository::PayloadCas::new(store.repository_root()).expect("open the CAS");
    let payload = cas.prepare_bytes(body).expect("store a payload");
    insert_alias(store, key, &payload.content_hash, payload.byte_size as i64);
    remove_local_body(store, &payload.content_hash);
    (payload.content_hash, payload.byte_size)
}

fn absent_aliases(findings: &Findings) -> Vec<String> {
    findings
        .items
        .iter()
        .filter(|finding| finding.code == codes::ALIAS_OBJECT_ABSENT)
        .map(|finding| finding.owner.id.clone())
        .collect()
}

fn alias_drops(findings: Findings) -> Vec<(String, bool)> {
    let result = crate::data_health::ScanResult::new(0, 0, crate::data_health::ScanDepth::Quick, findings);
    crate::data_health::repair::plan(&result)
        .into_iter()
        .filter_map(|candidate| match candidate.action {
            crate::data_health::repair::RepairAction::DropAlias { key, .. } => {
                Some((key, candidate.preferred))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn an_absent_body_the_sync_server_holds_is_not_a_problem_to_repair() {
    let (_directory, mut store) = healthy_fixture();
    let root = store.repository_root().to_owned();
    let (hash, size) = alias_without_local_body(&store, "assets/offloaded", b"synthetic offloaded body");

    // A body nothing holds stays the problem it is today, answered by removing the alias.
    let unheld = scan(&mut store);
    assert_eq!(absent_aliases(&unheld), ["assets/offloaded"]);
    assert!(unheld.items.iter().any(|finding| finding.severity == Severity::Blocking));
    assert_eq!(alias_drops(unheld), [("assets/offloaded".to_owned(), true)]);

    crate::server_sync::residency::test_remote::hold(&root, &[(&hash, size)]);
    let held = scan(&mut store);
    assert_eq!(absent_aliases(&held), Vec::<String>::new());
    assert!(
        held.items.iter().all(|finding| finding.severity != Severity::Blocking),
        "a body the server holds blocks nothing: {:?}",
        held.items
    );
    assert_eq!(alias_drops(held), Vec::<(String, bool)>::new());
    assert_eq!(crate::server_sync::residency::test_remote::fetched(&root), 0, "the scan downloads nothing");
    assert!(crate::asset_repository::PayloadCas::new(&root).unwrap().stat_object(&hash).unwrap().is_none());
}

#[test]
fn the_scan_creates_no_custody_store_on_a_device_that_never_synced() {
    let (_directory, mut store) = healthy_fixture();
    alias_without_local_body(&store, "assets/gone", b"synthetic gone body");
    assert_eq!(absent_aliases(&scan(&mut store)), ["assets/gone"]);
    assert!(!crate::server_sync::residency::Residency::exists(store.repository_root()));
}

#[test]
fn an_archived_character_whose_bodies_the_server_holds_is_not_reported() {
    let (_directory, mut store) = healthy_fixture();
    let root = store.repository_root().to_owned();
    let revision = store.revision().expect("read revision");
    store.archive_character("char-a", revision, 10).expect("archive a character");
    let generation = active_generation(&store.connection).expect("read active generation");
    let archived = crate::persistent_store::archive::read_archived_object(&store.connection, &generation, "char-a")
        .expect("read the archived character")
        .expect("the character is archived");
    let cas = crate::asset_repository::PayloadCas::new(&root).expect("open the CAS");
    let mut bodies = Vec::new();
    for hash in archived.object_roots() {
        if let Some(size) = cas.stat_object(hash).expect("stat a body") {
            remove_local_body(&store, hash);
            bodies.push((hash.to_owned(), size));
        }
    }
    assert!(!bodies.is_empty(), "the archive stores its records as bodies");
    let archive_findings = |findings: &Findings| {
        findings
            .items
            .iter()
            .filter(|finding| {
                finding.code == codes::RECORD_INVALID
                    && finding.owner.id == "char-a"
                    && finding.detail == "archived character payload is invalid or missing"
            })
            .count()
    };
    assert_eq!(archive_findings(&scan(&mut store)), 1);

    let held = bodies.iter().map(|(hash, size)| (hash.as_str(), *size)).collect::<Vec<_>>();
    crate::server_sync::residency::test_remote::hold(&root, &held);
    assert_eq!(archive_findings(&scan(&mut store)), 0);
}

pub(super) fn blocking(findings: &Findings) -> Vec<&crate::data_health::Finding> {
    findings
        .items
        .iter()
        .filter(|finding| finding.severity == Severity::Blocking)
        .collect()
}

/// Adds a character whose additional asset is listed by an owner manifest, as the app stores one.
pub(crate) fn add_character_with_additional_asset(store: &mut PersistentStore, character_id: &str) -> String {
    let key = format!("assets/{character_id}-extra.png");
    add_character_with_owner_manifest(store, character_id, &key, &key)
}

/// Adds a character whose additional assets name only `key`, with `image` as its image, and
/// returns the hash of its owner manifest.
pub(crate) fn add_character_with_owner_manifest(
    store: &mut PersistentStore,
    character_id: &str,
    key: &str,
    image: &str,
) -> String {
    use crate::asset_repository::{
        job_pins::{CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob},
        owner_manifest_codec::{encode_owner_manifest, OwnerManifestEntry},
    };
    let key = key.to_owned();
    let asset = crate::server_sync::lww_tests::put_asset(store, &key, format!("synthetic body of {key}").as_bytes())
        .object_hash
        .expect("store the additional asset");
    let canonical = encode_owner_manifest(&[OwnerManifestEntry {
        tuple: ["extra".into(), key.clone(), "png".into()],
        payload_hash: Some(hex::decode(&asset).unwrap().try_into().unwrap()),
    }])
    .expect("encode the owner manifest");
    let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).expect("open the CAS");
    let mut job = DurableCasJob::begin(
        store.repository_root(),
        &format!("{character_id}-owner"),
        CasJobKind::CardOrModuleContentImport,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    )
    .expect("begin the owner job");
    let manifest = job
        .prepare_reader(&cas, &mut canonical.as_slice(), CasObjectRole::OwnerManifest)
        .expect("store the owner manifest");
    job.seal(store, 2).expect("seal the owner job");
    store
        .commit(&WorkingSetCommit {
            expected_revision: store.revision().expect("read revision"),
            add_character: Some(json!({
                "chaId": character_id, "type": "character", "name": "Synthetic owner",
                "image": image, "creatorNotes": "synthetic notes", "lastInteraction": 500,
                "additionalAssets": [["extra", key, "png"]],
                "chats": [{"id": format!("{character_id}-chat"), "name": "Chat", "message": [
                    {"role": "user", "data": "synthetic first", "chatId": "synthetic-message"}
                ]}],
            })),
            asset_owner_heads: Some(vec![AssetOwnerHead::present(
                AssetOwnerLocator::CharacterAdditionalAssets { character_id: character_id.into() },
                manifest.content_hash.clone(),
                1,
            )]),
            ..empty_working_set_commit(store.revision().expect("read revision"))
        })
        .expect("add the character");
    job.release(CasReleaseOutcome::Committed).expect("release the owner job");
    manifest.content_hash
}

#[test]
fn a_library_with_archived_characters_scans_clean() {
    let (_directory, mut store) = healthy_fixture();
    add_character_with_additional_asset(&mut store, "char-owner");
    let before = scan(&mut store);
    assert_eq!(blocking(&before), Vec::<&crate::data_health::Finding>::new());
    // Alpha has an image and a last interaction, Gamma is in the trash, and the owner character
    // has creator notes and additional assets, so every summary column differs from the marker.
    for character_id in ["char-a", "char-c", "char-owner"] {
        let revision = store.revision().expect("read revision");
        store.archive_character(character_id, revision, 10).expect("archive a character");
    }
    let archived = scan(&mut store);
    assert_eq!(blocking(&archived), Vec::<&crate::data_health::Finding>::new());
    let generation = active_generation(&store.connection).expect("read active generation");
    for character_id in ["char-a", "char-c", "char-owner"] {
        let roots = archive::read_archived_object(&store.connection, &generation, character_id)
            .expect("read the archived character")
            .expect("the character is archived");
        for hash in roots.object_roots() {
            assert!(
                archived.items.iter().all(|finding| finding.owner.id != hash),
                "an archived character's body is reported: {:?}",
                archived.items
            );
        }
    }
    let unreferenced = |findings: &Findings| {
        findings.items.iter().filter(|finding| finding.code == codes::OBJECT_UNREFERENCED).count()
    };
    assert_eq!(unreferenced(&before), 0);
    assert_eq!(unreferenced(&archived), 0);
}

#[test]
fn a_damaged_archived_character_marker_is_still_refused() {
    for sql in [
        "UPDATE characters SET detail=json_set(detail,'$.name','Other') WHERE character_id='char-a'",
        "UPDATE characters SET detail=json_set(detail,'$.chaId','char-b') WHERE character_id='char-a'",
        "UPDATE characters SET detail=json_set(detail,'$.chats',json('[]')) WHERE character_id='char-a'",
        "UPDATE characters SET detail=json_remove(detail,'$.risuNestArchived') WHERE character_id='char-a'",
        "UPDATE characters SET name='Other' WHERE character_id='char-a'",
    ] {
        let (_directory, mut store) = healthy_fixture();
        let revision = store.revision().expect("read revision");
        store.archive_character("char-a", revision, 10).expect("archive a character");
        assert!(store.connection.execute(sql, []).expect("damage the archived row") > 0, "{sql}");
        let findings = scan(&mut store);
        assert!(
            blocking(&findings)
                .iter()
                .any(|finding| finding.code == codes::RECORD_INVALID && finding.owner.id == "char-a"),
            "{sql}: {:?}",
            findings.items
        );
    }
}

/// The quick scan over a library whose every body the server holds, as after a join under the
/// remote policy. Run explicitly with `--ignored --nocapture` to read the time.
#[test]
#[ignore = "scale measurement"]
fn measures_the_quick_scan_over_ten_thousand_server_held_aliases() {
    const ALIASES: usize = 10_000;
    let (_directory, mut store) = healthy_fixture();
    let root = store.repository_root().to_owned();
    let hashes = (0..ALIASES).map(|index| format!("{index:064x}")).collect::<Vec<_>>();
    store.connection.execute_batch("BEGIN").unwrap();
    for (index, hash) in hashes.iter().enumerate() {
        insert_alias(&store, &format!("assets/held-{index:05}"), hash, 4);
    }
    store.connection.execute_batch("COMMIT").unwrap();
    let held = hashes.iter().map(|hash| (hash.as_str(), 4)).collect::<Vec<_>>();
    crate::server_sync::residency::test_remote::hold(&root, &held);
    let started = std::time::Instant::now();
    let findings = scan_bounded(&mut store, 2000);
    let elapsed = started.elapsed();
    eprintln!(
        "quick scan, {ALIASES} server-held aliases: {elapsed:?}, {} findings, {} omitted, {} absent",
        findings.items.len(),
        findings.omitted,
        absent_aliases(&findings).len()
    );
    assert_eq!(absent_aliases(&findings), Vec::<String>::new());
}
