//! Archiving and restoring a character whose stored bodies another storage holds.
use super::*;
use crate::asset_repository::PayloadCas;
use crate::persistent_store::{RevisionResult, StoreResult};
use crate::persistent_store::archive::{
    ArchivedObject, ARCHIVE_CANCELLED_MESSAGE, ARCHIVE_DATA_MISSING_MESSAGE,
    ARCHIVE_DATA_UNAVAILABLE_MESSAGE,
};
use crate::server_sync::{
    lww_tests::{drain_publications, local, LocalServerFixture},
    residency::{test_remote, AssetPolicy},
};

const CHARACTER: &str = "char-owner";

/// A store with one character that has a conversation and additional assets, the hash of its
/// owner manifest, and the character as reading it returns it before it is archived.
fn owner_store() -> (tempfile::TempDir, PersistentStore, String, Value) {
    let (root, mut store) = local();
    let manifest =
        data_health_tests::add_character_with_additional_asset(&mut store, CHARACTER);
    let before = character(&store).expect("the character is readable");
    (root, store, manifest, before)
}

fn character(store: &PersistentStore) -> Option<Value> {
    store.materialize(None).expect("materialize the library")["characters"]
        .as_array()
        .expect("characters")
        .iter()
        .find(|character| character["chaId"] == CHARACTER)
        .cloned()
}

fn archived(store: &PersistentStore) -> Option<ArchivedObject> {
    let generation = active_generation(&store.connection).expect("read active generation");
    archive::read_archived_object(&store.connection, &generation, CHARACTER)
        .expect("read the archived character")
}

fn archive(store: &mut PersistentStore) -> ArchivedObject {
    store
        .archive_character(CHARACTER, store.revision().expect("read revision"), 10)
        .expect("archive the character");
    archived(store).expect("the character is archived")
}

/// Everything an archive or a restore changes, and the intents it leaves open.
fn state(store: &PersistentStore) -> (i64, Option<String>, i64, i64, i64) {
    let count = |sql: &str| -> i64 {
        store.connection.query_row(sql, [CHARACTER], |row| row.get(0)).expect("count rows")
    };
    (
        store.revision().expect("read revision"),
        store
            .connection
            .query_row(
                "SELECT archived_object FROM characters WHERE character_id = ?1",
                [CHARACTER],
                |row| row.get(0),
            )
            .expect("read the character row"),
        count("SELECT count(*) FROM conversations WHERE character_id = ?1"),
        count("SELECT count(*) FROM messages WHERE character_id = ?1"),
        store
            .device_store()
            .expect("open the device store")
            .connection()
            .query_row("SELECT count(*) FROM lww_intents WHERE complete = 0", [], |row| row.get(0))
            .expect("count open intents"),
    )
}

/// Removes a body from this device and returns its bytes.
fn take_local_body(store: &PersistentStore, hash: &str) -> Vec<u8> {
    let cas = PayloadCas::new(store.repository_root()).expect("open the CAS");
    let bytes = cas.read_object(hash).expect("read the body").expect("the body is here");
    fs::remove_file(cas.object_path(hash).expect("object path").expect("object path"))
        .expect("remove the body");
    bytes
}

fn is_local(store: &PersistentStore, hash: &str) -> bool {
    PayloadCas::new(store.repository_root())
        .expect("open the CAS")
        .stat_object(hash)
        .expect("stat the body")
        .is_some()
}

fn validation_message(result: StoreResult<RevisionResult>) -> String {
    match result {
        Err(StoreError::Validation { message }) => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn restore(store: &mut PersistentStore) -> StoreResult<RevisionResult> {
    let revision = store.revision().expect("read revision");
    store.restore_character(CHARACTER, revision)
}

#[test]
fn restoring_reads_an_archive_kept_on_this_device() {
    let (_root, mut store, _manifest, before) = owner_store();
    archive(&mut store);
    restore(&mut store).expect("restore the character");
    assert_eq!(character(&store), Some(before));
}

#[test]
fn archiving_and_restoring_fetch_only_what_the_sync_server_holds() {
    let server = LocalServerFixture::new();
    let (_root, mut store, manifest, mut before) = owner_store();
    let main = crate::server_sync::lww_tests::put_asset(&mut store, "assets/char-owner-main.png", b"synthetic separate representative");
    before["image"] = json!(main.key);
    store.commit(&WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        replace_character: Some(before.clone()),
        ..Default::default()
    }).expect("keep the representative separate from the additional asset");
    let core = server.client(&store);
    drain_publications(&core, &mut store, &[]).expect("publish the character");
    store.asset_residency_set_policy(AssetPolicy::Remote, || Ok(())).expect("keep bodies on the server");
    store.asset_residency_evict(|| Ok(())).expect("remove the server-held copies");
    let asset = store
        .read_asset_alias("asset", "assets/char-owner-extra.png", None)
        .expect("read the additional asset")
        .expect("the additional asset exists")
        .value
        .object_hash
        .expect("the additional asset has a body");
    assert!(!is_local(&store, &asset), "the remote policy leaves the asset on the server");
    assert!(is_local(&store, main.object_hash.as_ref().unwrap()));

    // Archiving reads only the owner manifest, which stays on this device, and still lists the
    // asset it names.
    let archived = archive(&mut store);
    assert!(archived.asset_hashes.contains(&asset));
    assert!(archived.asset_hashes.contains(&manifest));
    assert!(!is_local(&store, &asset));

    drain_publications(&core, &mut store, &[]).expect("publish the archive");
    store.asset_residency_evict(|| Ok(())).expect("remove the server-held copies");
    assert!(!is_local(&store, &archived.object_hash), "the archive is kept on the server");
    restore(&mut store).expect("restore from the server");
    assert_eq!(character(&store), Some(before));
    assert_eq!(state(&store).4, 0);
}

#[test]
fn restoring_fails_before_any_change_when_no_storage_holds_the_archive() {
    let (_root, mut store, _manifest, _before) = owner_store();
    let archived = archive(&mut store);
    take_local_body(&store, &archived.object_hash);
    let unchanged = state(&store);
    assert_eq!(validation_message(restore(&mut store)), ARCHIVE_DATA_MISSING_MESSAGE);
    assert_eq!(state(&store), unchanged);
}

#[test]
fn restoring_fails_before_any_change_when_the_server_cannot_be_reached() {
    let (root, mut store, _manifest, _before) = owner_store();
    let archived = archive(&mut store);
    let body = take_local_body(&store, &archived.object_hash);
    // Custody is recorded, but nothing answers at the server's address.
    test_remote::hold(root.path(), &[(&archived.object_hash, body.len() as u64)]);
    let unchanged = state(&store);
    assert_eq!(validation_message(restore(&mut store)), ARCHIVE_DATA_UNAVAILABLE_MESSAGE);
    assert_eq!(state(&store), unchanged);
    assert!(!is_local(&store, &archived.object_hash));
}

#[test]
fn cancelling_while_the_archive_is_fetched_changes_nothing() {
    let (root, mut store, _manifest, before) = owner_store();
    let archived = archive(&mut store);
    let body = take_local_body(&store, &archived.object_hash);
    test_remote::hold(root.path(), &[(&archived.object_hash, body.len() as u64)]);
    test_remote::serve(root.path(), &archived.object_hash, body);
    let unchanged = state(&store);
    let revision = store.revision().expect("read revision");
    let fetching = || test_remote::fetched(root.path()) > 0;
    let result = store.restore_character_with_cancellation(CHARACTER, revision, &fetching, None);
    assert_eq!(test_remote::fetched(root.path()), 1);
    assert_eq!(validation_message(result), ARCHIVE_CANCELLED_MESSAGE);
    assert_eq!(state(&store), unchanged);
    assert!(!is_local(&store, &archived.object_hash));

    restore(&mut store).expect("restore once nothing cancels it");
    assert_eq!(character(&store), Some(before));
}

#[test]
fn archiving_fetches_an_owner_manifest_this_device_does_not_keep() {
    let (root, mut store, manifest, before) = owner_store();
    let expected = {
        let (_root, mut control, _manifest, _before) = owner_store();
        archive(&mut control).asset_hashes
    };
    let bytes = take_local_body(&store, &manifest);
    test_remote::hold(root.path(), &[(&manifest, bytes.len() as u64)]);
    test_remote::serve(root.path(), &manifest, bytes);
    let archived = archive(&mut store);
    assert_eq!(test_remote::fetched(root.path()), 1);
    assert_eq!(archived.asset_hashes, expected);
    restore(&mut store).expect("restore the character");
    assert_eq!(character(&store), Some(before));
}

#[test]
fn archiving_fails_before_any_change_when_no_storage_holds_the_owner_manifest() {
    let (_root, mut store, manifest, _before) = owner_store();
    take_local_body(&store, &manifest);
    let unchanged = state(&store);
    let revision = store.revision().expect("read revision");
    assert_eq!(
        validation_message(store.archive_character(CHARACTER, revision, 10)),
        ARCHIVE_DATA_MISSING_MESSAGE
    );
    assert_eq!(state(&store), unchanged);
}

#[test]
fn restoring_fetches_an_archive_only_the_external_storage_holds() {
    use crate::external_storage::{
        connection_store::ConnectionStore, lww_residency,
        lww_tests::CycleFixture, previous_storage_tests::receiver_connection,
    };
    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime")
            .block_on(future)
    }
    let mut f = CycleFixture::new();
    // Incompressible text keeps the archive above the size a receive brings in with its records.
    let mut seed = Sha256::digest(b"synthetic external archive").to_vec();
    let data = (0..8192)
        .map(|_| {
            seed = Sha256::digest(&seed).to_vec();
            hex::encode(&seed)
        })
        .collect::<String>();
    f.a.commit(&WorkingSetCommit {
        expected_revision: f.a.revision().expect("read revision"),
        add_character: Some(json!({
            "chaId": CHARACTER, "type": "character", "name": "Synthetic external",
            "chats": [{"id": "external-chat", "name": "Chat", "message": [
                {"role": "user", "data": data, "chatId": "synthetic-external-message"}
            ]}],
        })),
        ..empty_working_set_commit(f.a.revision().expect("read revision"))
    })
    .expect("add the character");
    let before = character(&f.a).expect("the character is readable");
    archive(&mut f.a);
    run(f.publish_a());
    run(f.receive_b());
    let received = archived(&f.b).expect("the receiving device has the character archived");
    if is_local(&f.b, &received.object_hash) {
        take_local_body(&f.b, &received.object_hash);
    }
    let connection = receiver_connection(&f);
    ConnectionStore::open(f.directory_b.path())
        .expect("open the connection store")
        .insert(&connection.stored)
        .expect("store the connection");
    assert_eq!(
        data_health_tests::blocking(&data_health_tests::scan(&mut f.b)),
        Vec::<&crate::data_health::Finding>::new(),
        "an archive the external storage holds scans clean"
    );
    // The stored connection cannot reach the synthetic provider until the test resolves it.
    let unchanged = state(&f.b);
    assert_eq!(validation_message(restore(&mut f.b)), ARCHIVE_DATA_UNAVAILABLE_MESSAGE);
    assert_eq!(state(&f.b), unchanged);

    let _resolver = lww_residency::install_test_source_connection(
        f.directory_b.path(),
        std::sync::Arc::new(connection),
    )
    .expect("install the connection");
    restore(&mut f.b).expect("restore from the external storage");
    let restored = character(&f.b).expect("the character is readable");
    assert_eq!(restored["chats"], before["chats"]);
}

/// A store whose archived character is kept only on the sync server.
fn server_held_archive(server: &LocalServerFixture) -> (tempfile::TempDir, PersistentStore, Value) {
    let (root, mut store, _manifest, before) = owner_store();
    let core = server.client(&store);
    let archived = archive(&mut store);
    drain_publications(&core, &mut store, &[]).expect("publish the archive");
    store.asset_residency_set_policy(AssetPolicy::Remote, || Ok(())).expect("keep bodies on the server");
    store.asset_residency_evict(|| Ok(())).expect("remove the server-held copies");
    assert!(!is_local(&store, &archived.object_hash));
    (root, store, before)
}

#[test]
fn the_restore_command_fetches_from_the_sync_server() {
    use crate::persistent_store::commands;
    use tauri::Manager;
    let server = LocalServerFixture::new();
    let (_root, store, before) = server_held_archive(&server);
    let revision = store.revision().expect("read revision");
    let app = crate::test_app::mock_app();
    app.manage(commands::PersistentStoreState::with_test_store(store));
    tauri::async_runtime::block_on(commands::restore_character(
        app.handle().clone(),
        CHARACTER.into(),
        revision,
        "synthetic-restore".into(),
    ))
    .expect("restore through the command");
    let restored = commands::pds_read_character(app.state(), CHARACTER.into(), None)
        .expect("read the character")
        .expect("the character exists");
    assert_eq!(restored.value["name"], before["name"]);
    assert_eq!(commands::with_store(app.state(), |store| store.revision()).expect("read revision"), revision + 1);
}
