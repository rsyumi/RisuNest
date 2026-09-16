use super::*;
use crate::data_health::{codes, Findings, Severity};
use crate::local_backup::NeverCancelled;

/// The shared fixture leaves the storage authority at its legacy state, which is itself a
/// blocking finding. This one completes it so the scan starts from a healthy baseline.
fn v2_fixture() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().expect("create temporary directory");
    let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
    let database = fixture();
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
        .replace_put_asset_repository_authority(
            &staging.staging_id,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "data-health".to_owned(),
                compatibility_hash: "1a".repeat(32),
            },
        )
        .expect("stage v2 asset authority");
    store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit fixture");
    (directory, store)
}

fn scan(store: &mut PersistentStore) -> Findings {
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
    let (_directory, mut store) = v2_fixture();
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
    let (_directory, mut store) = v2_fixture();
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

    let (_stored_directory, mut stored) = v2_fixture();
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
    let (_directory, mut store) = v2_fixture();
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
    let (_directory, mut store) = v2_fixture();
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
