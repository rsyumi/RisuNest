use super::*;
use crate::persistent_store::lww::{inline, unit_key, Change, Header};
use crate::persistent_store::sync_selection::{ReplaceBindingRequest, SwitchBindingRequest, SyncTarget};
use crate::persistent_store::{hash_work, message_pages};
use risunest_sync_wire::{stamp::Stamp, unit::UnitValue};
use std::collections::BTreeMap;

// Library rows of the active generation (without the generation column), the
// shared unit state and the device unit state, in a fixed order. Stamps,
// versions and request identities differ per run and are left out.
fn golden_dump(store: &PersistentStore) -> String {
    let db = &store.connection;
    let generation = active_generation(db).unwrap();
    let mut out = String::new();
    for &(table, _) in GENERATION_TABLES {
        let columns = db
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|name| name != "generation")
            .collect::<Vec<_>>();
        let list = columns.join(",");
        dump_rows(
            db,
            &mut out,
            table,
            &format!("SELECT {list} FROM {table} WHERE generation=?1 ORDER BY {list}"),
            &[&generation],
        );
    }
    for (label, sql) in [
        ("lww_units", "SELECT key,value,identity FROM lww_units ORDER BY key"),
        ("lww_outbox", "SELECT key,value,identity,authority FROM lww_outbox ORDER BY key"),
        ("lww_retired", "SELECT key FROM lww_retired ORDER BY key"),
        ("lww_receive_rows", "SELECT key,value,status FROM lww_receive_rows ORDER BY key,value,status"),
    ] {
        dump_rows(db, &mut out, label, sql, &[]);
    }
    let device = store.device_store().unwrap().connection();
    for (label, sql) in [
        ("device_lww_units", "SELECT key,value,identity FROM lww_units ORDER BY key"),
        ("device_lww_outbox", "SELECT key,value,identity,authority FROM lww_outbox ORDER BY key"),
    ] {
        dump_rows(device, &mut out, label, sql, &[]);
    }
    out
}

fn dump_rows(db: &Connection, out: &mut String, label: &str, sql: &str, params: &[&dyn rusqlite::ToSql]) {
    use rusqlite::types::ValueRef;
    out.push_str(label);
    out.push('\n');
    let mut statement = db.prepare(sql).unwrap();
    let count = statement.column_count();
    let mut rows = statement.query(params).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let mut fields = Vec::with_capacity(count);
        for index in 0..count {
            fields.push(match row.get_ref(index).unwrap() {
                ValueRef::Null => "null".to_owned(),
                ValueRef::Integer(value) => format!("i{value}"),
                ValueRef::Real(value) => format!("r{value}"),
                ValueRef::Text(value) => serde_json::to_string(&String::from_utf8_lossy(value)).unwrap(),
                ValueRef::Blob(value) => format!("b{}", hex::encode(value)),
            });
        }
        out.push_str(&fields.join("|"));
        out.push('\n');
    }
}

// Readers find the active library through the meta value while the purge goes
// by the generation states, so every activation has to move both.
fn assert_one_active_generation(store: &PersistentStore) {
    let marked = store
        .connection
        .prepare("SELECT id FROM generations WHERE state='active'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(marked, [active_generation(&store.connection).unwrap()]);
}

fn golden(store: &PersistentStore, name: &str) -> String {
    assert_one_active_generation(store);
    let dump = golden_dump(store);
    if let Some(directory) = std::env::var_os("RISUNEST_GOLDEN_DUMP") {
        fs::write(Path::new(&directory).join(format!("{name}.txt")), &dump).unwrap();
    }
    hex::encode(Sha256::digest(dump.as_bytes()))
}

fn seed_library(store: &mut PersistentStore) {
    let database = fixture();
    let staging = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&staging, &staged_root(&database)).unwrap();
    store
        .replace_put_presets(&staging, database["botPresets"].as_array().unwrap())
        .unwrap();
    let mut characters = database["characters"].as_array().unwrap().clone();
    characters.push(json!({
        "type": "character",
        "chaId": "archived-char",
        "name": "Archived",
        "chats": [{
            "id": "archived-chat",
            "name": "Archived Chat",
            "message": [
                { "role": "user", "data": "kept", "chatId": "archived-m1" },
                { "role": "char", "data": "away", "chatId": "archived-m2" }
            ]
        }]
    }));
    store.replace_add_characters(&staging, &characters).unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
    store
        .commit(&WorkingSetCommit {
            root: Some(json!({ "username": "Local edit" })),
            ..empty_working_set_commit(1)
        })
        .unwrap();
    let revision = store.revision().unwrap();
    store.archive_character("archived-char", revision, 10).unwrap();
}

fn build_stage(store: &mut PersistentStore) -> String {
    let database = fixture();
    let staging = store.replace_begin().unwrap().staging_id;
    let mut root = staged_root(&database);
    root["username"] = json!("Staged");
    root["language"] = json!("staged");
    store.replace_put_root(&staging, &root).unwrap();
    let mut presets = database["botPresets"].as_array().unwrap().clone();
    presets[0]["name"] = json!("Staged preset");
    store.replace_put_presets(&staging, &presets).unwrap();
    let mut characters = database["characters"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|character| character["chaId"] != "char-b")
        .cloned()
        .collect::<Vec<_>>();
    for character in &mut characters {
        if character["chaId"] == "char-a" {
            let messages = character["chats"][0]["message"].as_array_mut().unwrap();
            messages[5]["data"] = json!("staged edit");
            messages.push(json!({ "role": "user", "data": "staged tail", "chatId": "staged-tail" }));
            messages.push(json!({ "role": "char", "data": "staged end", "chatId": "staged-end" }));
        }
        if character["chaId"] == "char-c" {
            character["name"] = json!("Staged C");
        }
    }
    characters.push(json!({
        "type": "character",
        "chaId": "char-d",
        "name": "Staged D",
        "chats": [{
            "id": "char-d-chat",
            "name": "D Chat",
            "message": [
                { "role": "user", "data": "first", "chatId": "d-m1" },
                { "role": "char", "data": "second", "chatId": "d-m2" }
            ]
        }]
    }));
    store.replace_add_characters(&staging, &characters).unwrap();
    store.replace_preserve_repositories(&staging, None).unwrap();
    staging
}

fn source_units() -> BTreeMap<risunest_sync_wire::unit::UnitKey, UnitValue> {
    BTreeMap::from([
        (unit_key(&["root", "username"]).unwrap(), inline(&json!("Source username")).unwrap()),
        (unit_key(&["character", "char-c", "name"]).unwrap(), inline(&json!("Source C")).unwrap()),
        (unit_key(&["future-unit", "backup"]).unwrap(), inline(&json!({ "opaque": 3 })).unwrap()),
        // A backup's format level leaves the golden state unchanged.
        (risunest_sync_wire::library_format::key(), risunest_sync_wire::library_format::value(2).unwrap()),
    ])
}

fn seeded() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    seed_library(&mut store);
    (directory, store)
}

fn header(store: &PersistentStore, request_id: &str) -> Header {
    Header {
        binding_authority: store.lww_binding_authority().unwrap(),
        request_id: request_id.into(),
    }
}

fn device_sections() -> Vec<crate::persistent_store::device_store::sections::PreparedSectionRows> {
    use crate::persistent_store::device_store::hypa::HypaEmbeddingWrite;
    use risunest_external_storage_format::section::SectionKind;
    let directory = tempfile::tempdir().unwrap();
    let mut source = PersistentStore::open(directory.path()).unwrap();
    let device = source.device_store_mut().unwrap();
    device
        .write_hypa_embeddings(&[HypaEmbeddingWrite {
            cache_key: "a".repeat(64),
            producer: "synthetic".into(),
            model: "golden".into(),
            endpoint: None,
            preprocess_version: 1,
            dimensions: 1,
            vector: vec![0, 0, 128, 63],
            metadata: None,
        }])
        .unwrap();
    device.write_setting("accountst", &json!("golden")).unwrap();
    device
        .capture_backup_sections(&[SectionKind::Hypa, SectionKind::LocalPlugins, SectionKind::LocalSettings], &std::env::temp_dir())
        .unwrap()
}

fn binding_stamp() -> Stamp {
    Stamp {
        physical_ms: 7.into(),
        logical: 0,
        writer_id: "00000000-0000-4000-8000-000000000001".into(),
    }
}

fn binding_change(key: &[&str], value: Value) -> Change {
    Change {
        key: unit_key(key).unwrap(),
        stamp: binding_stamp(),
        value: inline(&value).unwrap(),
    }
}

fn binding_messages(store: &PersistentStore, values: &[Value]) -> UnitValue {
    use risunest_external_storage_format::message_pages::{repage, MessageHash};
    use risunest_sync_wire::descriptor::RecordDescriptor;
    let bodies = values
        .iter()
        .map(|value| risunest_sync_wire::payload_value::encode(value).unwrap())
        .collect::<Vec<_>>();
    let repaged = repage::<StoreError>(
        &Default::default(),
        bodies.iter().map(|body| MessageHash::from_bytes(body)).collect(),
        0..0,
        bodies.len(),
        |index| Ok(bodies[index].clone()),
    )
    .unwrap();
    for object in repaged.objects {
        store.lww_put_object(&object.hash, &object.bytes).unwrap();
    }
    let manifest = repaged.index.manifest().encode().unwrap();
    store.lww_put_object(&manifest.hash, &manifest.bytes).unwrap();
    let mut descriptor = RecordDescriptor::content(manifest.hash);
    descriptor.dependencies = repaged.index.pages.iter().map(|page| page.page.hash.clone()).collect();
    descriptor.dependencies.sort();
    descriptor.dependencies.dedup();
    UnitValue::object(descriptor).unwrap()
}

fn binding_incoming(store: &PersistentStore) -> Vec<Change> {
    let mut messages = binding_change(&["messages", "remote-char", "remote-chat"], Value::Null);
    messages.value = binding_messages(
        store,
        &(0..5)
            .map(|index| json!({ "role": "user", "data": format!("remote {index}"), "chatId": format!("r{index}") }))
            .collect::<Vec<_>>(),
    );
    let mut held = binding_change(&["messages", "missing-char", "held-chat"], Value::Null);
    held.value = binding_messages(store, &[json!({ "role": "user", "data": "held", "chatId": "h1" })]);
    vec![
        binding_change(&["root", "language"], json!("remote")),
        binding_change(&["exists", "character", "remote-char"], json!({ "type": "character" })),
        binding_change(&["character", "remote-char", "name"], json!("Remote")),
        binding_change(&["character", "remote-char", "desc"], json!("Remote description")),
        binding_change(&["exists", "conversation", "remote-char", "remote-chat"], json!(true)),
        binding_change(&["conversation", "remote-char", "remote-chat", "name"], json!("Remote Chat")),
        messages,
        binding_change(&["exists", "conversation", "missing-char", "held-chat"], json!(true)),
        binding_change(&["conversation", "missing-char", "held-chat", "name"], json!("Held")),
        held,
        binding_change(&["future", "opaque"], json!({ "untouched": true })),
    ]
}

fn bind_target(store: &mut PersistentStore) {
    let incoming = binding_incoming(store);
    let source = header(store, &uuid::Uuid::new_v4().to_string());
    let inspection = store
        .register_lww_binding_inspection(
            source.binding_authority,
            &SyncTarget::Server("remote".into()),
            "target",
            "library",
        )
        .unwrap();
    let stage = store
        .lww_stage_binding_units(&source, &inspection, &incoming, 7.into())
        .unwrap();
    let state = store.lww_binding_state().unwrap();
    let state = store
        .switch_lww_binding(&SwitchBindingRequest {
            header: header(store, &uuid::Uuid::new_v4().to_string()),
            expected_selection_epoch: state.selection_epoch,
            target: SyncTarget::Server("remote".into()),
            inspection_id: Some(inspection),
            initial_publication: false,
        })
        .unwrap();
    store
        .replace_lww_binding(&ReplaceBindingRequest {
            header: Header {
                binding_authority: state.target_authority,
                request_id: source.request_id.clone(),
            },
            expected_selection_epoch: state.selection_epoch,
            staging_id: stage.staging_id,
            receive_id: source.request_id,
            target_id: "target".into(),
            library_id: "library".into(),
        })
        .unwrap();
}

fn bind_new_device(store: &mut PersistentStore) {
    let incoming = binding_incoming(store);
    let source = header(store, &uuid::Uuid::new_v4().to_string());
    let inspection = store
        .register_lww_binding_inspection(
            source.binding_authority,
            &SyncTarget::Server("fresh-connection".into()),
            "target",
            "library",
        )
        .unwrap();
    let staging = store
        .lww_stage_binding_units(&source, &inspection, &incoming, 7.into())
        .unwrap()
        .staging_id;
    let preparation = store.prepare_lww_new_device(&source, &staging).unwrap();
    store.authorize_lww_new_device(&preparation.authorization_id).unwrap();
    store
        .lww_replace_target_as_new_device(&source, &staging, &preparation.authorization_id)
        .unwrap();
}

// Captured at 9e3b21122, before activation stopped renaming the staged
// generation. Every path must keep producing the same library and unit state.
// TARGET and NEW_DEVICE were captured again once a binding stage counted the
// conversations it projects and gave each new character a chat page.
// PLAIN, SOURCE_UNITS and DEVICE_SECTIONS include removal of retired child units;
// permanent parent tombstones remain in the captured state.
const PLAIN: &str = "381f9a28efa91579d36d2f5bbefa30938b52c41c0c9b0dbbf0c6fc2c59e118ac";
const SOURCE_UNITS: &str = "e9a320c326ffe3c61e01f81112911613d582c1cde7098a9cf47cbab9f25ec89e";
const DEVICE_SECTIONS: &str = "746739c2babc5c7277bce5685d285da705836d6cad7d2194ef36165b493e252a";
const TARGET: &str = "908e9c6ddf880f41998c8a2f977a82c36e9a06dfc40d743bcd44b52a084e7de8";
const NEW_DEVICE: &str = "908e9c6ddf880f41998c8a2f977a82c36e9a06dfc40d743bcd44b52a084e7de8";

#[test]
fn plain_replacement_matches_the_golden_state() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let revision = store.revision().unwrap();
    store.replace_commit(&stage, Some(revision)).unwrap();
    assert_eq!(golden(&store, "plain"), PLAIN);
}

#[test]
fn source_unit_replacement_matches_the_golden_state() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let header = header(&store, "golden-source-units");
    store
        .lww_commit_replacement_units(&header, &stage, Some(&source_units()))
        .unwrap();
    assert_eq!(golden(&store, "source-units"), SOURCE_UNITS);
}

#[test]
fn device_section_replacement_matches_the_golden_state() {
    let (_directory, mut store) = seeded();
    let sections = device_sections();
    let stage = build_stage(&mut store);
    let header = header(&store, "golden-device-sections");
    store
        .lww_commit_replacement_with_device_sections(
            &header,
            &stage,
            Some(&source_units()),
            &sections.iter().collect::<Vec<_>>(),
        )
        .unwrap();
    assert_eq!(golden(&store, "device-sections"), DEVICE_SECTIONS);
}

#[test]
fn target_binding_replacement_matches_the_golden_state() {
    let (_directory, mut store) = seeded();
    bind_target(&mut store);
    assert_eq!(golden(&store, "target"), TARGET);
}

#[test]
fn new_device_replacement_matches_the_golden_state() {
    let (_directory, mut store) = seeded();
    bind_new_device(&mut store);
    assert_eq!(golden(&store, "new-device"), NEW_DEVICE);
}

/// The active persona's embedded module carries the id its persona derives, whichever path and
/// however many projections gave it one, so the root can be saved again.
fn assert_root_saves_with_a_named_module(store: &mut PersistentStore) {
    let mut root = store.read_root(None).unwrap().value;
    let persona = root["personas"].as_array().unwrap().iter().find(|persona| persona["id"] == "persona-a").unwrap();
    let id = persona["embeddedModule"]["id"].as_str().expect("the embedded module has an id");
    assert_eq!(id, crate::persistent_store::record_projection::embedded_module_id("persona-a"));
    uuid::Uuid::parse_str(id).expect("the embedded module id is a uuid");
    assert_eq!(persona["embeddedModule"]["name"], "Embedded");
    root["username"] = json!("Saved after activation");
    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit { root: Some(root), ..empty_working_set_commit(revision) })
        .expect("a root save after activation");
}

/// A persona whose embedded module was saved before activation gave every module an id.
fn idless_module() -> Value {
    json!({ "name": "Embedded", "lorebook": [] })
}

#[test]
fn a_replacement_names_an_embedded_module_without_an_id_after_its_persona() {
    let (_directory, mut store) = seeded();
    let stage = store.replace_begin().unwrap().staging_id;
    let mut root = staged_root(&fixture());
    root["personas"] = json!([{ "id": "persona-a", "name": "A", "embeddedModule": idless_module() }]);
    store.replace_put_root(&stage, &root).unwrap();
    let revision = store.revision().unwrap();
    store.replace_commit(&stage, Some(revision)).unwrap();
    assert_root_saves_with_a_named_module(&mut store);
}

#[test]
fn a_replacement_from_source_units_names_an_embedded_module_without_an_id_after_its_persona() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let mut units = source_units();
    units.insert(unit_key(&["exists", "persona", "persona-a"]).unwrap(), inline(&json!(true)).unwrap());
    units.insert(unit_key(&["persona", "persona-a", "name"]).unwrap(), inline(&json!("A")).unwrap());
    units.insert(unit_key(&["persona", "persona-a", "embeddedModule"]).unwrap(), inline(&idless_module()).unwrap());
    let request = header(&store, "embedded-module-source-units");
    store.lww_commit_replacement_units(&request, &stage, Some(&units)).unwrap();
    assert_root_saves_with_a_named_module(&mut store);
    // The same unit projected again names the module the same way.
    let stage = build_stage(&mut store);
    let request = header(&store, "embedded-module-source-units-again");
    store.lww_commit_replacement_units(&request, &stage, Some(&units)).unwrap();
    assert_root_saves_with_a_named_module(&mut store);
}

#[test]
fn a_target_binding_names_an_embedded_module_without_an_id_after_its_persona() {
    let (_directory, mut store) = seeded();
    let mut incoming = binding_incoming(&store);
    incoming.push(binding_change(&["exists", "persona", "persona-a"], json!(true)));
    incoming.push(binding_change(&["persona", "persona-a", "name"], json!("A")));
    incoming.push(binding_change(&["persona", "persona-a", "embeddedModule"], idless_module()));
    let source = header(&store, &uuid::Uuid::new_v4().to_string());
    let inspection = store
        .register_lww_binding_inspection(source.binding_authority, &SyncTarget::Server("remote".into()), "target", "library")
        .unwrap();
    let stage = store.lww_stage_binding_units(&source, &inspection, &incoming, 7.into()).unwrap();
    let state = store.lww_binding_state().unwrap();
    let state = store
        .switch_lww_binding(&SwitchBindingRequest {
            header: header(&store, &uuid::Uuid::new_v4().to_string()),
            expected_selection_epoch: state.selection_epoch,
            target: SyncTarget::Server("remote".into()),
            inspection_id: Some(inspection),
            initial_publication: false,
        })
        .unwrap();
    store
        .replace_lww_binding(&ReplaceBindingRequest {
            header: Header { binding_authority: state.target_authority, request_id: source.request_id.clone() },
            expected_selection_epoch: state.selection_epoch,
            staging_id: stage.staging_id,
            receive_id: source.request_id,
            target_id: "target".into(),
            library_id: "library".into(),
        })
        .unwrap();
    assert_root_saves_with_a_named_module(&mut store);
}

#[test]
fn a_portable_restore_names_an_embedded_module_without_an_id_after_its_persona() {
    // A portable backup or snapshot of such a library carries the module as it was saved.
    let (_source_directory, source, _) = open_fixture();
    let generation = active_generation(&source.connection).unwrap();
    let personas = json!([{ "id": "persona-a", "name": "A", "embeddedModule": idless_module() }]);
    source
        .connection
        .execute(
            "UPDATE root SET value=json_set(value,'$.personas',json(?2)) WHERE generation=?1",
            params![generation, personas.to_string()],
        )
        .unwrap();
    let reader = crate::persistent_store::snapshot::open_generation_reader(&source.database_path, &generation).unwrap();
    let (_directory, mut store) = seeded();
    let stage = store.stage_portable_records(&reader, &crate::local_backup::NeverCancelled).unwrap().staging_id;
    let revision = store.revision().unwrap();
    store.replace_commit(&stage, Some(revision)).unwrap();
    assert_root_saves_with_a_named_module(&mut store);

    // Staged rows and the source unit carrying the same module name it the same way.
    let (_directory, mut store) = seeded();
    let stage = store.stage_portable_records(&reader, &crate::local_backup::NeverCancelled).unwrap().staging_id;
    let units = BTreeMap::from([
        (unit_key(&["exists", "persona", "persona-a"]).unwrap(), inline(&json!(true)).unwrap()),
        (unit_key(&["persona", "persona-a", "embeddedModule"]).unwrap(), inline(&idless_module()).unwrap()),
    ]);
    let request = header(&store, "embedded-module-portable-units");
    store.lww_commit_replacement_units(&request, &stage, Some(&units)).unwrap();
    assert_root_saves_with_a_named_module(&mut store);
}

#[test]
fn replacement_activation_reads_no_messages_and_writes_no_pages() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let revision = store.revision().unwrap();
    message_pages::reset_capture_work();
    hash_work::reset_hash_work();
    store.replace_commit(&stage, Some(revision)).unwrap();
    let pages = message_pages::take_capture_work();
    let hashes = hash_work::take_hash_work();
    eprintln!("activation page work: {pages:?}");
    eprintln!("activation hash work: {hashes:?}");
    assert_eq!(pages.capture_calls, 0);
    assert_eq!(pages.work.messages_read, 0);
    assert_eq!(pages.work.pages_written, 0);
    for domain in ["native_message_verify", "native_page_identity"] {
        assert!(!hashes.domains.contains_key(domain), "{domain}");
    }
    // The intent rows are hashed once, as they are written, and the body that
    // is hashed again on completion carries only their digest.
    assert_eq!(hashes.domains["native_intent_rows"].calls, 1);
    assert!(hashes.domains["native_intent"].bytes < 1024, "{hashes:?}");
}

/// Conversations of a stage whose stored manifest is missing or differs from
/// paging its messages again.
fn staged_manifest_gaps(store: &mut PersistentStore, staging: &str) -> Vec<String> {
    let conversations = store
        .connection
        .prepare("SELECT character_id,conversation_id FROM conversations WHERE generation=?1 ORDER BY 1,2")
        .unwrap()
        .query_map([staging], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!conversations.is_empty());
    let transaction = store.connection.transaction().unwrap();
    let mut gaps = Vec::new();
    for (character, conversation) in conversations {
        let stored: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3)",
                params![staging, character, conversation],
                |row| row.get(0),
            )
            .unwrap();
        let current = message_pages::current_manifest(&transaction, staging, &character, &conversation).unwrap();
        let captured = message_pages::capture_manifest(&transaction, staging, &character, &conversation, None).unwrap();
        if !stored || current != captured {
            gaps.push(format!("{character}/{conversation}"));
        }
    }
    transaction.rollback().unwrap();
    gaps
}

#[test]
fn staging_pages_every_conversation_it_writes() {
    let (_directory, mut store) = seeded();
    let whole = build_stage(&mut store);
    assert_eq!(staged_manifest_gaps(&mut store, &whole), Vec::<String>::new());

    let paged = store.replace_begin().unwrap().staging_id;
    store.replace_put_root(&paged, &json!({ "username": "Paged" })).unwrap();
    store.replace_put_presets(&paged, &[]).unwrap();
    store
        .replace_put_character_detail(&paged, &json!({ "chaId": "paged", "name": "Paged", "type": "character" }), 2)
        .unwrap();
    store
        .replace_put_conversation_row(&paged, "paged", 0, &json!({ "id": "empty", "name": "Empty" }), 0, 0)
        .unwrap();
    store
        .replace_put_conversation_row(&paged, "paged", 1, &json!({ "id": "long", "name": "Long" }), 0, 3)
        .unwrap();
    let messages = (0..3).map(|index| chat_message(&format!("paged-{index}"), "paged")).collect::<Vec<_>>();
    store.replace_add_conversation_messages(&paged, "paged", "long", 0, &messages[..2]).unwrap();
    store.replace_add_conversation_messages(&paged, "paged", "long", 2, &messages[2..]).unwrap();
    assert_eq!(staged_manifest_gaps(&mut store, &paged), Vec::<String>::new());

    let (_source_directory, source, _) = open_fixture();
    let generation = active_generation(&source.connection).unwrap();
    let source = crate::persistent_store::snapshot::open_generation_reader(&source.database_path, &generation).unwrap();
    let portable = store.stage_portable_records(&source, &crate::local_backup::NeverCancelled).unwrap().staging_id;
    assert_eq!(staged_manifest_gaps(&mut store, &portable), Vec::<String>::new());
}

fn manifest_mismatches(store: &mut PersistentStore) -> Vec<String> {
    let generation = active_generation(&store.connection).unwrap();
    let conversations = store
        .connection
        .prepare("SELECT character_id,conversation_id FROM conversations WHERE generation=?1
            UNION SELECT character_id,conversation_id FROM message_page_manifests WHERE generation=?1
            UNION SELECT character_id,conversation_id FROM message_page_indexes WHERE generation=?1 ORDER BY 1,2")
        .unwrap()
        .query_map([&generation], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let transaction = store.connection.transaction().unwrap();
    let mut mismatches = Vec::new();
    for (character, conversation) in conversations {
        // Both manifest helpers can rebuild pages, so observe the stored index first.
        let had_index = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM message_page_indexes WHERE generation=?1 AND character_id=?2 AND conversation_id=?3)",
            params![generation, character, conversation], |row| row.get::<_, bool>(0),
        ).unwrap();
        let current = message_pages::current_manifest(&transaction, &generation, &character, &conversation).unwrap();
        let captured = message_pages::capture_manifest(&transaction, &generation, &character, &conversation, None).unwrap();
        let orphan_index = captured == risunest_sync_wire::unit::UnitValue::Deleted && had_index;
        if current != captured || orphan_index {
            mismatches.push(format!("{character}/{conversation}"));
        }
    }
    transaction.rollback().unwrap();
    mismatches
}

fn chat_message(id: &str, data: &str) -> Value {
    json!({ "role": "user", "data": data, "chatId": id })
}

#[test]
fn active_manifests_equal_a_full_capture_after_every_mutation_kind() {
    let (_directory, mut store, database) = open_fixture();
    let mut results = Vec::new();

    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            conversations: Some(vec![
                ConversationMutation::ReplaceRange {
                    character_id: "char-a".into(),
                    conversation_id: "conv-long".into(),
                    start: 5,
                    delete_count: 1,
                    messages: vec![chat_message("edited-5", "edited")],
                    conversation: None,
                    configured_index: None,
                },
                ConversationMutation::ReplaceRange {
                    character_id: "char-a".into(),
                    conversation_id: "conv-short".into(),
                    start: 2,
                    delete_count: 0,
                    messages: vec![chat_message("appended", "appended")],
                    conversation: None,
                    configured_index: None,
                },
            ]),
            ..empty_working_set_commit(revision)
        })
        .unwrap();
    results.push(("conversation edits", manifest_mismatches(&mut store)));

    let revision = store.revision().unwrap();
    store.commit(&WorkingSetCommit {
        conversations: Some(vec![ConversationMutation::Delete {
            character_id: "char-a".into(), conversation_id: "conv-long".into(),
        }]),
        ..empty_working_set_commit(revision)
    }).unwrap();
    results.push(("delete_conversation", manifest_mismatches(&mut store)));

    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            delete_character_ids: Some(vec!["char-b".into()]),
            ..empty_working_set_commit(revision)
        })
        .unwrap();
    results.push(("delete_character", manifest_mismatches(&mut store)));

    let mut replacement = database["characters"][1].clone();
    replacement["chats"][0]["message"] = json!([chat_message("only", "only")]);
    let revision = store.revision().unwrap();
    store
        .commit(&WorkingSetCommit {
            replace_character: Some(replacement),
            ..empty_working_set_commit(revision)
        })
        .unwrap();
    results.push(("replace_character", manifest_mismatches(&mut store)));

    let revision = store.revision().unwrap();
    store.archive_character("char-a", revision, 10).unwrap();
    results.push(("archive", manifest_mismatches(&mut store)));
    let revision = store.revision().unwrap();
    store.restore_character("char-a", revision).unwrap();
    results.push(("restore", manifest_mismatches(&mut store)));

    // A repair and its undo rewrite message rows outside a range edit, so both have to leave
    // the stored pages what paging the messages again gives.
    let generation = active_generation(&store.connection).unwrap();
    let stored: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM message_page_manifests WHERE generation=?1 AND character_id='char-a' AND conversation_id='conv-short'",
            [&generation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 1, "the repaired conversation has stored pages to keep current");
    let revision = store.revision().unwrap();
    let (repaired, journal) = store
        .apply_repair(
            revision,
            &[crate::data_health::repair::RepairCandidate {
                id: "0:drop-reference".into(),
                action: crate::data_health::repair::RepairAction::DropReference {
                    owner: crate::data_health::Owner {
                        kind: "message".into(),
                        id: "char-a/conv-short/1".into(),
                    },
                    source_path: "$.time".into(),
                    occurrence: 0,
                    token: None,
                },
                finding: 0,
                preferred: true,
                discards: true,
            }],
            10,
        )
        .unwrap();
    assert!(journal.records.iter().any(|record| record.table == "messages"), "the repair rewrote a message");
    results.push(("repair", manifest_mismatches(&mut store)));
    store.undo_repair(&journal, repaired.revision).unwrap();
    results.push(("undo repair", manifest_mismatches(&mut store)));

    eprintln!("current manifest equivalence: {results:?}");
    assert!(results.iter().all(|(_, mismatches)| mismatches.is_empty()), "{results:?}");
}

/// Character detail writes per character during one activation, counted by a
/// connection-local trigger.
fn count_detail_writes(
    store: &mut PersistentStore,
    activate: impl FnOnce(&mut PersistentStore),
) -> BTreeMap<String, i64> {
    store
        .connection
        .execute_batch(
            "CREATE TEMP TABLE detail_writes(character_id TEXT NOT NULL);
             CREATE TEMP TRIGGER count_detail_writes AFTER UPDATE OF detail ON main.characters
             BEGIN INSERT INTO detail_writes VALUES(NEW.character_id); END;",
        )
        .unwrap();
    activate(store);
    let writes = store
        .connection
        .prepare("SELECT character_id,count(*) FROM detail_writes GROUP BY character_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<BTreeMap<String, i64>, _>>()
        .unwrap();
    store.connection.execute_batch("DROP TRIGGER count_detail_writes; DROP TABLE detail_writes;").unwrap();
    writes
}

#[test]
fn activation_writes_each_character_detail_at_most_once() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let revision = store.revision().unwrap();
    let plain = count_detail_writes(&mut store, |store| {
        store.replace_commit(&stage, Some(revision)).unwrap();
    });
    eprintln!("plain activation detail writes: {plain:?}");
    assert!(plain.values().all(|count| *count <= 1), "{plain:?}");

    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let header = header(&store, "detail-writes-source-units");
    let mut units = source_units();
    units.insert(unit_key(&["character", "char-d", "desc"]).unwrap(), inline(&json!("Source D description")).unwrap());
    units.insert(unit_key(&["character", "char-d", "firstMessage"]).unwrap(), inline(&json!("Source D greeting")).unwrap());
    let sourced = count_detail_writes(&mut store, |store| {
        store.lww_commit_replacement_units(&header, &stage, Some(&units)).unwrap();
    });
    eprintln!("sourced activation detail writes: {sourced:?}");
    assert!(sourced.values().all(|count| *count <= 1), "{sourced:?}");
    assert_eq!(sourced.get("char-d"), Some(&1));
    let detail = store.read_character("char-d", None).unwrap().unwrap().value;
    assert_eq!(detail["desc"], "Source D description");
    assert_eq!(detail["firstMessage"], "Source D greeting");
}

#[test]
fn merged_replacement_changes_equal_whole_library_maps() {
    let mut rich = source_units();
    for (key, value) in [
        (&["character", "char-d", "desc"][..], inline(&json!("Source D description")).unwrap()),
        (&["character", "archived-char", "name"][..], inline(&json!("Source archived")).unwrap()),
        (&["exists", "conversation", "char-gone", "chat-gone"][..], UnitValue::Deleted),
        (&["archive", "char-c"][..], UnitValue::Deleted),
        (&["conversation", "char-a", "conv-long", "name"][..], inline(&json!("Source long")).unwrap()),
    ] {
        rich.insert(unit_key(key).unwrap(), value);
    }
    // Staging keeps the archived character, so both archive branches are reached.
    for source in [None, Some(source_units()), Some(rich)] {
        let (_directory, mut store) = seeded();
        let stage = build_stage(&mut store);
        let archived: bool = store
            .connection
            .query_row("SELECT archived_object IS NOT NULL FROM characters WHERE generation=?1 AND character_id='archived-char'", [&stage], |row| row.get(0))
            .unwrap();
        assert!(archived);
        // A stored unit that neither library captures is deleted by the replacement.
        let stored = unit_key(&["character", "char-stored-only", "name"]).unwrap();
        crate::persistent_store::lww::put_unit(&store.connection, &stored, &binding_stamp(), &inline(&json!("Stored")).unwrap(), "synthetic", None).unwrap();
        let [merged, whole] = store.replacement_change_sets(&stage, source.as_ref()).unwrap();
        assert!(merged.0.iter().any(|(key, value)| key == &stored && matches!(value, UnitValue::Deleted)));
        assert!(!merged.0.is_empty());
        assert_eq!(merged, whole);
    }
}

#[test]
fn replacement_units_of_converted_binaries_equal_the_parsed_library() {
    // A converted Uint8Array is an object with one member per byte.
    let binary = |length: usize| Value::Object((0..length).map(|index| (index.to_string(), json!(index % 256))).collect());
    let (_directory, mut store) = seeded();
    let stage = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&stage, &json!({
            "username": "Binary",
            "modules": [
                {"id": "module-large", "name": "Large", "binary": binary(30_000)},
                {"id": "module-small", "binary": binary(100), "configured_index": 1},
            ],
            "plugins": [{"name": "plugin", "script": "\"quoted\"\n"}],
            "loadouts": [{"id": "loadout", "binary": binary(20)}],
            "customModels": [{"id": "model", "binary": binary(30_000)}],
            "personas": [{"id": "persona", "name": "Persona", "embeddedModule": {"id": "embedded", "binary": binary(30_000)}}],
            "explicitGlobalChatVariables": {"toggle_binary": binary(10), "plain": "\u{d55c}"},
            "protectedPresetValues": {"seperateModels": {"memory": "model"}, "fallbackModels": [binary(5)]},
            "characterOrder": ["char-binary", {"name": "folder", "data": []}],
            "pluginCustomStorage": {"binary": {"blob": binary(30_000)}, "small": binary(10)},
            "pluginStorageMeta": {"binary": {"plugin": "synthetic-plugin"}},
        }))
        .unwrap();
    store
        .replace_put_presets(&stage, &[json!({"id": "preset-binary", "name": "Preset", "NAISettings": {"blob": binary(30_000)}, "temperature": 0.5})])
        .unwrap();
    store
        .replace_add_characters(&stage, &[json!({
            "type": "character",
            "chaId": "char-binary",
            "name": "Binary",
            "extentions": {"blob": binary(30_000)},
            "chatFolders": [{"id": "folder", "name": "Folder"}],
            "chats": [{"id": "chat", "name": "Chat", "scriptstate": {"blob": binary(30_000)},
                "message": [{"role": "user", "data": "synthetic", "chatId": "m1"}]}],
        })])
        .unwrap();
    store.replace_preserve_repositories(&stage, None).unwrap();
    let [merged, whole] = store.replacement_change_sets(&stage, None).unwrap();
    let value = |parts: &[&str]| merged.0.iter().find(|(key, _)| *key == unit_key(parts).unwrap()).map(|(_, value)| value.clone());
    assert!(matches!(value(&["record", "modules", "module-large"]), Some(UnitValue::Object { .. })));
    assert!(matches!(value(&["record", "modules", "module-small"]), Some(UnitValue::Inline { .. })));
    assert!(matches!(value(&["persona", "persona", "embeddedModule"]), Some(UnitValue::Object { .. })));
    assert!(matches!(value(&["character", "char-binary", "extentions"]), Some(UnitValue::Object { .. })));
    assert!(matches!(value(&["conversation", "char-binary", "chat", "scriptstate"]), Some(UnitValue::Object { .. })));
    assert!(matches!(value(&["preset", "preset-binary", "NAISettings"]), Some(UnitValue::Object { .. })));
    assert!(matches!(value(&["plugin", "synthetic-plugin", "binary"]), Some(UnitValue::Object { .. })));
    assert_eq!(merged, whole);
}

fn intent_rows(store: &PersistentStore, request_id: &str) -> i64 {
    store
        .device_store()
        .unwrap()
        .connection()
        .query_row("SELECT count(*) FROM lww_intent_rows WHERE request_id=?1", [request_id], |row| row.get(0))
        .unwrap()
}

fn intent_complete(store: &PersistentStore, request_id: &str) -> bool {
    store
        .device_store()
        .unwrap()
        .connection()
        .query_row("SELECT complete FROM lww_intents WHERE request_id=?1", [request_id], |row| row.get(0))
        .unwrap()
}

fn reject_activation(store: &PersistentStore) {
    store
        .connection
        .execute_batch(
            "CREATE TEMP TRIGGER reject_activation BEFORE INSERT ON main.lww_requests
             BEGIN SELECT RAISE(ABORT,'synthetic-activation-failure'); END",
        )
        .unwrap();
}

#[test]
fn a_replacement_intent_body_carries_row_digests_instead_of_units() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let header = header(&store, "intent-body-source-units");
    store.lww_commit_replacement_units(&header, &stage, Some(&source_units())).unwrap();
    let body: String = store
        .device_store()
        .unwrap()
        .connection()
        .query_row("SELECT body FROM lww_intents WHERE request_id=?1", [&header.request_id], |row| row.get(0))
        .unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["changes"]["rows"].as_u64().unwrap() > 0, "{body}");
    assert_eq!(body["changes"]["digest"].as_str().unwrap().len(), 64);
    // Every source unit is staged, the format level among them.
    assert_eq!(body["source_units"]["rows"], 4);
    assert_eq!(body["device_changes"], json!([]));
    assert_eq!(intent_rows(&store, &header.request_id), 0);
    assert_eq!(golden(&store, "source-units-rows"), SOURCE_UNITS);
}

#[test]
fn an_interrupted_replacement_replays_its_rows_to_the_golden_state() {
    let directory = {
        let (directory, mut store) = seeded();
        let stage = build_stage(&mut store);
        let revision = store.revision().unwrap();
        reject_activation(&store);
        assert!(store.replace_commit(&stage, Some(revision)).is_err());
        assert!(!intent_complete(&store, &stage));
        assert!(intent_rows(&store, &stage) > 0);
        assert_eq!(store.revision().unwrap(), revision);
        directory
    };
    let mut store = PersistentStore::open(directory.path()).unwrap();
    store.lww_recover_intents().unwrap();
    assert_eq!(golden(&store, "plain-replayed"), PLAIN);
    let stage: String = store
        .device_store()
        .unwrap()
        .connection()
        .query_row("SELECT request_id FROM lww_intents ORDER BY rowid DESC LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert!(intent_complete(&store, &stage));
    assert_eq!(intent_rows(&store, &stage), 0);
}

#[test]
fn a_changed_intent_row_stops_recovery() {
    let (_directory, mut store) = seeded();
    let stage = build_stage(&mut store);
    let revision = store.revision().unwrap();
    reject_activation(&store);
    assert!(store.replace_commit(&stage, Some(revision)).is_err());
    store.connection.execute_batch("DROP TRIGGER reject_activation").unwrap();
    let language = inline(&json!("tampered")).unwrap();
    let tampered = store
        .device_store()
        .unwrap()
        .connection()
        .execute(
            "UPDATE lww_intent_rows SET value=?2 WHERE request_id=?1 AND key=?3",
            params![stage, serde_json::to_string(&language).unwrap(), unit_key(&["root", "language"]).unwrap().as_str()],
        )
        .unwrap();
    assert_eq!(tampered, 1);
    let failure = store.lww_recover_intents().unwrap_err();
    assert!(matches!(&failure, StoreError::Validation { message } if message == "request-id-integrity"), "{failure:?}");
    assert_eq!(store.revision().unwrap(), revision);
    assert!(!intent_complete(&store, &stage));
}

#[test]
fn rows_left_before_an_intent_was_issued_are_dropped_and_the_request_retries() {
    let directory = {
        let (directory, store) = seeded();
        let device = store.device_store().unwrap().connection();
        for request in ["orphan-request", "another-orphan"] {
            device
                .execute(
                    "INSERT INTO lww_intent_rows VALUES(?1,1,?2,NULL,?3,0)",
                    params![request, unit_key(&["root", "username"]).unwrap().as_str(), "\"partial\""],
                )
                .unwrap();
        }
        directory
    };
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let stage = build_stage(&mut store);
    let header = header(&store, "orphan-request");
    store.lww_commit_replacement_units(&header, &stage, None).unwrap();
    let left: i64 = store
        .device_store()
        .unwrap()
        .connection()
        .query_row("SELECT count(*) FROM lww_intent_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(golden(&store, "orphan-retried"), PLAIN);
}

#[test]
fn rows_of_an_unfinished_intent_keep_their_objects() {
    use crate::persistent_store::{MessageObjectStore, MESSAGE_PAGE_SWEEP_LIMIT};
    use risunest_sync_wire::descriptor::RecordDescriptor;
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let body = b"synthetic intent row object".to_vec();
    let hash = risunest_sync_wire::hash(&body);
    store.lww_put_object(&hash, &body).unwrap();
    let value = UnitValue::object(RecordDescriptor::content(hash.clone())).unwrap();
    let device = store.device_store().unwrap().connection();
    device
        .execute("INSERT INTO lww_intents VALUES('rooted','0','{}','{}','unused',0)", [])
        .unwrap();
    device
        .execute(
            "INSERT INTO lww_intent_rows VALUES('rooted',1,?1,NULL,?2,0)",
            params![unit_key(&["messages", "char", "chat"]).unwrap().as_str(), serde_json::to_string(&value).unwrap()],
        )
        .unwrap();
    let present = |store: &PersistentStore| -> bool {
        store
            .connection
            .query_row("SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)", [&hash], |row| row.get(0))
            .unwrap()
    };
    let now = 1_900_000_000_000i64;
    for step in 0..4 {
        store
            .sweep_message_page_objects(MessageObjectStore::Library, now + step * ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, MESSAGE_PAGE_SWEEP_LIMIT)
            .unwrap();
    }
    assert!(present(&store), "an unfinished intent row lost its object");
    store
        .device_store()
        .unwrap()
        .connection()
        .execute("UPDATE lww_intents SET complete=1 WHERE request_id='rooted'", [])
        .unwrap();
    for step in 4..8 {
        store
            .sweep_message_page_objects(MessageObjectStore::Library, now + step * ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, MESSAGE_PAGE_SWEEP_LIMIT)
            .unwrap();
    }
    assert!(!present(&store), "a completed intent row still roots its object");
}
