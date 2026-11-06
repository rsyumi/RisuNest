use super::*;
use crate::persistent_store::commit;
use super::super::{inline, read_unit, unit_key, ApplyReceive, Progress, StageReceive, UnitMutation};
use crate::persistent_store::{
    active_generation, query, sync_selection::{ReplaceBindingRequest, SwitchBindingRequest, SyncTarget},
    ReadTarget, WorkingSetCommit,
};
use risunest_sync_wire::{descriptor::RecordDescriptor, stamp::Stamp};
use serde_json::{json, Value};

fn change(key: &[&str], value: Value) -> Change {
    Change { key: unit_key(key).unwrap(), stamp: Stamp { physical_ms: 7.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000001".into() }, value: inline(&value).unwrap() }
}
fn context(store: &PersistentStore) -> (Header, String) {
    let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() };
    let inspection = store.register_lww_binding_inspection(header.binding_authority, &SyncTarget::Server("remote".into()), "target", "library").unwrap();
    (header, inspection)
}
fn catalog(store: &PersistentStore, staging: &str) -> Value {
    query::materialize_target(&store.connection, &ReadTarget { generation: staging.into(), revision: store.revision().unwrap() }).unwrap()
}
fn activate(store: &mut PersistentStore, header: &Header, inspection: &str, stage: &BindingUnitStage) {
    let state = store.lww_binding_state().unwrap();
    let state = store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header: Header { binding_authority: state.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: state.selection_epoch, target: SyncTarget::Server("remote".into()), inspection_id: Some(inspection.into()),
    }).unwrap();
    store.replace_lww_binding(&ReplaceBindingRequest {
        header: Header { binding_authority: state.target_authority, request_id: header.request_id.clone() },
        expected_selection_epoch: state.selection_epoch, staging_id: stage.staging_id.clone(), receive_id: header.request_id.clone(), target_id: "target".into(), library_id: "library".into(),
    }).unwrap();
}
fn save(store: &mut PersistentStore, mutations: Vec<UnitMutation>) {
    store.commit(&WorkingSetCommit { expected_revision: store.revision().unwrap(), unit_mutations: Some(mutations), ..Default::default() }).unwrap();
}
fn message_value(store: &PersistentStore, values: &[Value]) -> UnitValue {
    use risunest_external_storage_format::message_pages::{repage, MessageHash};
    let bodies = values.iter().map(|v| risunest_sync_wire::payload_value::encode(v).unwrap()).collect::<Vec<_>>();
    let repaged = repage::<crate::persistent_store::StoreError>(&Default::default(), bodies.iter().map(|b| MessageHash::from_bytes(b)).collect(), 0..0, bodies.len(), |i| Ok(bodies[i].clone())).unwrap();
    for object in repaged.objects { store.lww_put_object(&object.hash, &object.bytes).unwrap(); }
    let manifest = repaged.index.manifest().encode().unwrap();
    store.lww_put_object(&manifest.hash, &manifest.bytes).unwrap();
    let mut descriptor = RecordDescriptor::content(manifest.hash);
    descriptor.dependencies = repaged.index.pages.iter().map(|page| page.page.hash.clone()).collect();
    descriptor.dependencies.sort(); descriptor.dependencies.dedup();
    UnitValue::object(descriptor).unwrap()
}

#[test]
fn staging_is_incoming_only_and_does_not_remap_active_retired_ids_or_issue_stamps() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    save(&mut store, vec![
        UnitMutation::Set { key: unit_key(&["root","language"]).unwrap(), value: json!("local") },
        UnitMutation::Set { key: unit_key(&["exists","character","same-id"]).unwrap(), value: json!(true) },
    ]);
    save(&mut store, vec![UnitMutation::Delete { key: unit_key(&["exists","character","same-id"]).unwrap() }]);
    let clock = serde_json::to_value(store.lww_clock_state().unwrap()).unwrap();
    let revision = store.revision().unwrap(); let generation = active_generation(&store.connection).unwrap();
    let outbox = serde_json::to_value(store.lww_read_outbox(0.into(), 100).unwrap()).unwrap();
    let incoming = vec![change(&["root","language"], json!("remote")), change(&["exists","character","same-id"], json!({"type":"character"})), change(&["character","same-id","name"], json!("remote-name")), change(&["character","same-id","futureField"], json!({"opaque":true}))];
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    let value = catalog(&store, &stage.staging_id);
    assert_eq!(value["language"], "remote"); assert_eq!(value["characters"][0]["chaId"], "same-id");
    assert_eq!(value["characters"][0]["name"], "remote-name"); assert!(value["characters"][0].get("futureField").is_none());
    assert_eq!(store.read_root(None).unwrap().value["language"], "local");
    assert_eq!(store.revision().unwrap(), revision); assert_eq!(active_generation(&store.connection).unwrap(), generation);
    assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(), clock);
    assert_eq!(serde_json::to_value(store.lww_read_outbox(0.into(), 100).unwrap()).unwrap(), outbox);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_binding_source_units WHERE staging_id=?1", [&stage.staging_id], |r| r.get::<_,i64>(0)).unwrap(), incoming.len() as i64);
}

#[test]
fn held_children_survive_activation_restart_and_project_on_parent_arrival_without_restamping() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let mut messages = change(&["messages","missing","chat"], Value::Null);
    messages.value = message_value(&store, &[json!({"role":"user","data":"synthetic-message","chatId":"m"})]);
    let incoming = vec![change(&["exists","conversation","missing","chat"], json!(true)), change(&["conversation","missing","chat","name"], json!("held-name")), messages.clone(), change(&["future","opaque"], json!({"untouched":true}))];
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    assert!(catalog(&store, &stage.staging_id)["characters"].as_array().unwrap().is_empty());
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE status='held'", [], |r| r.get::<_,i64>(0)).unwrap(), 3);
    drop(store); let mut store = PersistentStore::open(dir.path()).unwrap();
    let receive = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() };
    store.lww_stage_receive(&StageReceive { header: receive.clone(), changes: vec![change(&["exists","character","missing"], json!({"type":"character"}))], progress: Progress {kind:"server".into(),cursor:1.into(),writer_id:None}, admitted_time_upper_ms:7.into() }).unwrap();
    let result = store.lww_apply_receive(&ApplyReceive { header: receive.clone(), generating:vec![] }).unwrap();
    store.lww_finish_receive(&receive).unwrap();
    assert!(result.held_keys.is_empty()); assert!(result.affected_keys.contains(&messages.key));
    let value = catalog(&store, &active_generation(&store.connection).unwrap());
    assert_eq!(value["characters"][0]["chats"][0]["name"], "held-name");
    assert_eq!(value["characters"][0]["chats"][0]["message"][0]["data"], "synthetic-message");
    assert_eq!(read_unit(&store.connection, &messages.key).unwrap(), Some((messages.stamp.clone(),messages.value.clone())));
    assert_eq!(store.connection.query_row("SELECT version FROM lww_units WHERE key=?1", [messages.key.as_str()], |r| r.get::<_,String>(0)).unwrap(), header.request_id);
    assert_eq!(read_unit(&store.connection, &incoming[3].key).unwrap(), Some((incoming[3].stamp.clone(),incoming[3].value.clone())));
    assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(),100).unwrap().entries.is_empty());
}

#[test]
fn held_message_deferred_during_generation_projects_after_restart_and_drain_without_restamping() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let mut messages=change(&["messages","missing","chat"],Value::Null);
    messages.value=message_value(&store,&[json!({"data":"deferred-held-message","chatId":"m"})]);
    let incoming=vec![change(&["exists","conversation","missing","chat"],json!(true)),messages.clone()];
    let (header,inspection)=context(&store); let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    activate(&mut store,&header,&inspection,&stage);
    let receive=Header{binding_authority:store.lww_binding_authority().unwrap(),request_id:uuid::Uuid::new_v4().to_string()};
    store.lww_stage_receive(&StageReceive{header:receive.clone(),changes:vec![change(&["exists","character","missing"],json!(true))],progress:Progress{kind:"server".into(),cursor:1.into(),writer_id:None},admitted_time_upper_ms:7.into()}).unwrap();
    let applied=store.lww_apply_receive(&ApplyReceive{header:receive.clone(),generating:vec![super::super::MessageLocator{character_id:"missing".into(),conversation_id:"chat".into(),start:None}]}).unwrap();
    assert_eq!(applied.deferred_keys,vec![messages.key.clone()]);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM messages",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    store.lww_finish_receive(&receive).unwrap();
    drop(store); let mut store=PersistentStore::open(dir.path()).unwrap();
    let drained=store.lww_drain_deferred(&ApplyReceive{header:Header{binding_authority:store.lww_binding_authority().unwrap(),request_id:uuid::Uuid::new_v4().to_string()},generating:vec![]}).unwrap();
    assert!(drained.deferred_keys.is_empty()); assert!(drained.affected_keys.contains(&messages.key));
    let value=catalog(&store,&active_generation(&store.connection).unwrap());
    assert_eq!(value["characters"][0]["chats"][0]["message"][0]["data"],"deferred-held-message");
    assert_eq!(read_unit(&store.connection,&messages.key).unwrap(),Some((messages.stamp.clone(),messages.value.clone())));
    assert_eq!(store.connection.query_row("SELECT version FROM lww_units WHERE key=?1",[messages.key.as_str()],|r|r.get::<_,String>(0)).unwrap(),header.request_id);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE request_id=?1",[&header.request_id],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(),100).unwrap().entries.is_empty());
}

#[test]
fn source_retirement_suppresses_descendants_and_order_uses_creation_stamps() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let mut retired = change(&["exists","character","retired"], json!(true)); retired.value = UnitValue::Deleted;
    let mut first = change(&["exists","character","a"], json!(true)); first.stamp.physical_ms = 3.into();
    let mut incoming = vec![retired, change(&["character","retired","name"], json!("hidden")), change(&["exists","character","b"], json!(true)), first, change(&["order","characters"], json!(["retired","b"]))];
    for (character, own) in [("a", "a-only"), ("b", "b-only")] {
        incoming.push(change(&["exists","conversation",character,"shared"], json!(true)));
        incoming.push(change(&["exists","conversation",character,own], json!(true)));
    }
    incoming.extend([
        deleted_at(&["exists","conversation","a","retired-chat"], 7),
        change(&["conversation","a","retired-chat","name"], json!("hidden")),
        change(&["order","conversations","a"], json!({"ids":["b-only","shared","retired-chat","a-only"],"folders":[]})),
        change(&["order","conversations","b"], json!({"ids":["a-only","b-only","shared"],"folders":[]})),
    ]);
    let assert_orders = |value: &Value| {
        for (index, ids) in [(0, ["b-only", "shared"]), (1, ["shared", "a-only"])] {
            assert_eq!(value["characters"][index]["chats"].as_array().unwrap().iter()
                .map(|chat| chat["id"].as_str().unwrap()).collect::<Vec<_>>(), ids);
        }
    };
    let (header, inspection) = context(&store); let stage = store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let value = catalog(&store,&stage.staging_id);
    assert_eq!(value["characters"].as_array().unwrap().iter().map(|c| c["chaId"].as_str().unwrap()).collect::<Vec<_>>(), ["b","a"]);
    assert_orders(&value);
    activate(&mut store,&header,&inspection,&stage);
    assert_orders(&catalog(&store,&active_generation(&store.connection).unwrap()));
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_retired", [], |r| r.get::<_,i64>(0)).unwrap(), 2);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE status='held'", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(read_unit(&store.connection,&incoming[1].key).unwrap(), None);
    assert_eq!(read_unit(&store.connection,&unit_key(&["conversation","a","retired-chat","name"]).unwrap()).unwrap(), None);
}

#[test]
fn archive_alias_and_reference_controls_stage_without_reading_any_large_dependency_body() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let body_hash = "ab".repeat(32); let asset_hash = "cd".repeat(32);
    for hash in [&body_hash,&asset_hash] { store.connection.execute("INSERT INTO message_page_objects VALUES(?1,?2)", params![hash,b"poison-body".as_slice()]).unwrap(); }
    let archived = crate::persistent_store::archive::ArchivedObject { object_hash:body_hash.clone(),shared_object_hash:body_hash, archived_at:1,conversation_count:1,message_count:1,asset_hashes:vec![asset_hash.clone()],shared_asset_hashes:vec![asset_hash.clone()],identity_remap:Vec::new() };
    let mut archive = change(&["archive","archived"],Value::Null); archive.value = projection::archive_value(&store.connection,&archived).unwrap();
    let hashes = (0..100).map(|i| format!("{i:064x}")).collect::<Vec<_>>();
    let (root,objects) = risunest_sync_wire::descriptor::build_reference_tree(&hashes,false).unwrap();
    for (hash,bytes) in objects { store.lww_put_object(&hash,&bytes).unwrap(); }
    let mut descriptor = RecordDescriptor::content("ef".repeat(32)); descriptor.dependency_root = root;
    let mut opaque = change(&["future","object"],Value::Null); opaque.value = UnitValue::object(descriptor).unwrap();
    let incoming = vec![change(&["exists","character","archived"],json!(true)),change(&["character","archived","name"],json!("archive-name")),change(&["exists","conversation","archived","hidden"],json!(true)),archive,change(&["asset","assets/synthetic.bin"],json!({"key":"assets/synthetic.bin","objectHash":asset_hash,"kind":"asset","size":1024,"mime":"application/octet-stream","name":"synthetic","ext":"bin","metadata":{}})),opaque];
    let (header,inspection)=context(&store); let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let value=catalog(&store,&stage.staging_id); assert!(value["characters"].as_array().unwrap().is_empty());
    let marker:String=store.connection.query_row("SELECT detail FROM characters WHERE generation=?1 AND character_id='archived'",[&stage.staging_id],|r|r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&marker).unwrap()["chaId"],"archived");
    assert_eq!(store.connection.query_row("SELECT count(*) FROM characters WHERE generation=?1 AND archived_object IS NOT NULL",[&stage.staging_id],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM asset_aliases WHERE generation=?1",[&stage.staging_id],|r|r.get::<_,i64>(0)).unwrap(),1);
    let original:String=store.connection.query_row("SELECT archived_object FROM characters WHERE generation=?1 AND character_id='archived'",[&stage.staging_id],|r|r.get(0)).unwrap();
    let mut changed:Value=serde_json::from_str(&original).unwrap(); changed["archivedAt"]=json!(2);
    store.connection.execute("UPDATE characters SET archived_object=?2 WHERE generation=?1 AND character_id='archived'",params![stage.staging_id,changed.to_string()]).unwrap();
    assert!(crate::persistent_store::sync_selection::validate_binding_stage_content(&store.connection,&stage.staging_id).is_err());
    let state=store.lww_binding_state().unwrap();
    assert!(store.switch_lww_binding(&SwitchBindingRequest {
        header: Header { binding_authority: state.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: state.selection_epoch.clone(), target: SyncTarget::Server("remote".into()), inspection_id: Some(inspection.clone()), initial_publication: false,
    }).is_err());
    assert_eq!(store.lww_binding_state().unwrap().selection_epoch,state.selection_epoch);
    store.connection.execute("UPDATE characters SET archived_object=?2 WHERE generation=?1 AND character_id='archived'",params![stage.staging_id,original]).unwrap();
    activate(&mut store,&header,&inspection,&stage);
    assert!(store.read_character("archived",None).is_err());
}

/// A bound archive restores from the payload its unit shares, which this device recorded when it
/// wrote that payload, so the unused-file cleanup decides from the record while the payload is
/// only on the server.
#[test]
fn a_bound_archive_finds_the_record_of_the_payload_its_unit_shares() {
    use crate::persistent_store::{archive::read_archived_object, asset_alias_orphans::OrphanAliasScope};
    use crate::server_sync::{lww_tests::put_asset, residency::test_remote};
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let html = put_asset(&mut store, "assets/html.png", b"synthetic html body").object_hash.unwrap();
    put_asset(&mut store, "assets/orphan.png", b"synthetic orphan body");
    store.commit(&WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        add_character: Some(json!({"chaId":"archived","type":"character","name":"Synthetic","image":"","desc":"<img src=\"assets/html.png\">","chats":[],"lastInteraction":500})),
        ..Default::default()
    }).unwrap();
    store.archive_character("archived", store.revision().unwrap(), 10).unwrap();
    let mut archived = read_archived_object(&store.connection, &active_generation(&store.connection).unwrap(), "archived").unwrap().unwrap();
    assert_ne!(archived.shared_object_hash, archived.object_hash);
    // The unit an older release sent lists no key inside the text.
    archived.shared_asset_hashes.retain(|hash| *hash != html);
    let mut archive = change(&["archive","archived"], Value::Null); archive.value = projection::archive_value(&store.connection, &archived).unwrap();
    let mut incoming = vec![change(&["exists","character","archived"], json!(true)), change(&["character","archived","name"], json!("archive-name")), archive];
    for key in ["assets/html.png", "assets/orphan.png"] {
        let mut alias = change(&["asset", key], Value::Null);
        alias.value = read_unit(&store.connection, &alias.key).unwrap().unwrap().1;
        incoming.push(alias);
    }
    let (header, inspection) = context(&store); let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    activate(&mut store, &header, &inspection, &stage);
    let bound = read_archived_object(&store.connection, &active_generation(&store.connection).unwrap(), "archived").unwrap().unwrap();
    assert_eq!(bound.object_hash, archived.shared_object_hash);

    // The library the binding replaced holds its objects until it is purged in the background.
    while store.purge_retired_batch(256).unwrap() {}
    test_remote::keep_only_on_server(dir.path(), &bound.object_hash);
    let now = crate::persistent_store::device_store::now_ms().unwrap();
    let deleted = store.delete_orphaned_asset_aliases(&OrphanAliasScope::Cleanup { now_ms: now }).unwrap();
    assert_eq!(deleted.into_iter().map(|alias| alias.key).collect::<Vec<_>>(), ["assets/orphan.png"]);
    assert_eq!(test_remote::fetched(dir.path()), 0);
}

#[test]
fn admission_failure_leaves_active_state_and_staging_tables_untouched() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["root","language"],json!("remote"))];
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,6.into()).is_err());
    let mut invalid=change(&["messages","c","chat"],Value::Null); invalid.value=UnitValue::object(RecordDescriptor::content("12".repeat(32))).unwrap();
    assert!(store.lww_stage_binding_units(&header,&inspection,&[invalid],7.into()).is_err());
    assert!(store.lww_stage_binding_units(&header,&inspection,&[incoming[0].clone(),incoming[0].clone()],7.into()).is_err());
    assert!(store.lww_stage_binding_units(&Header{binding_authority:1.into(),..header.clone()},&inspection,&incoming,7.into()).is_err());
    assert_eq!(store.revision().unwrap(),0); assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_binding_sources",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM root WHERE generation LIKE 'staging-%'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
}

#[test]
fn frozen_stage_reopens_retries_exactly_and_rejects_changed_source_or_projection() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["future","opaque"],json!({"x":1})),change(&["root","language"],json!("remote"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let before=store.connection.query_row("SELECT changes FROM lww_binding_stages WHERE staging_id=?1",[&stage.staging_id],|r|r.get::<_,String>(0)).unwrap();
    drop(store); let mut store=PersistentStore::open(dir.path()).unwrap();
    let mut reordered=incoming.clone(); reordered.reverse();
    assert_eq!(store.lww_stage_binding_units(&header,&inspection,&reordered,7.into()).unwrap(),stage);
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,8.into()).is_err());
    let mut altered=incoming.clone(); altered[0].value=inline(&json!({"x":2})).unwrap();
    assert!(store.lww_stage_binding_units(&header,&inspection,&altered,7.into()).is_err());
    assert_eq!(store.connection.query_row("SELECT changes FROM lww_binding_stages WHERE staging_id=?1",[&stage.staging_id],|r|r.get::<_,String>(0)).unwrap(),before);
    store.replace_put_root(&stage.staging_id,&json!({"language":"mutated"})).unwrap();
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).is_err());
    assert_eq!(store.lww_binding_authority().unwrap(),0.into()); assert_eq!(store.revision().unwrap(),0);
}

#[test]
fn staging_database_lives_in_app_scratch_beside_a_crash_leftover_and_is_removed() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let scratch_root=dir.path().join("external-storage").join("scratch");
    let leftover=scratch_root.join("binding-stage-leftover");
    std::fs::create_dir_all(&leftover).unwrap(); std::fs::write(leftover.join("incoming.sqlite"),b"synthetic crash leftover").unwrap();
    let (header,inspection)=context(&store);
    STAGE_SCRATCH.with(|scratch| scratch.replace(None));
    store.lww_stage_binding_units(&header,&inspection,&[change(&["root","language"],json!("remote"))],7.into()).unwrap();
    let staged=STAGE_SCRATCH.with(|scratch| scratch.take()).expect("binding stage scratch");
    assert!(staged.starts_with(&scratch_root),"{staged:?}");
    assert!(!staged.exists());
    assert_eq!(std::fs::read_dir(&scratch_root).unwrap().count(),1);
}

#[test]
fn crash_after_atomic_copy_retains_exact_receipt_before_caller_registration() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["character","missing","name"],json!("held"))];
    FAIL_AFTER_STAGE_COPY.with(|fail| fail.set(true));
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).is_err());
    let frozen:(String,String)=store.connection.query_row("SELECT staging_id,source_digest FROM lww_binding_sources WHERE request_id=?1",[&header.request_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_binding_stages WHERE staging_id=?1 AND receive_id=?2",params![frozen.0,header.request_id],|r|r.get::<_,i64>(0)).unwrap(),1);
    drop(store); let mut store=PersistentStore::open(dir.path()).unwrap();
    let stage: super::super::BindingUnitStage =store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    assert_eq!((stage.staging_id,stage.source_digest),frozen); assert_eq!(store.revision().unwrap(),0);
}

#[test]
fn cancellation_after_the_stage_commits_returns_the_committed_stage() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let incoming = vec![change(&["root","language"], json!("remote"))];
    let path: String = store.connection.query_row("SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0)).unwrap();
    let reader = crate::sqlite_open::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let stage = store.lww_stage_binding_units_observed(&header, &inspection, &incoming, 7.into(), |_| {
        let committed: bool = reader.query_row("SELECT EXISTS(SELECT 1 FROM lww_binding_sources WHERE request_id=?1)", [&header.request_id], |row| row.get(0))?;
        if committed { return Err(error("synthetic-observer-after-commit")); }
        Ok(())
    }).unwrap();
    drop(reader);
    let frozen = store.connection.query_row("SELECT staging_id,source_digest FROM lww_binding_sources WHERE request_id=?1", [&header.request_id], |row| {
        Ok(BindingUnitStage { staging_id: row.get(0)?, source_digest: row.get(1)? })
    }).unwrap();
    assert_eq!(stage, frozen);
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(active_generation(&store.connection).unwrap(), stage.staging_id);
    assert_eq!(catalog(&store, &stage.staging_id)["language"], "remote");
    assert_eq!(store.revision().unwrap(), 1);
}

#[test]
fn startup_sweeps_unattested_and_abandoned_stages_but_retains_frozen_binding_stage() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let abandoned=store.replace_begin().unwrap().staging_id;
    let (header,inspection)=context(&store); let incoming=vec![change(&["root","language"],json!("remote"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let (other_header,other_inspection)=context(&store);
    let unattested=store.lww_stage_binding_units(&other_header,&other_inspection,&[],0.into()).unwrap();
    store.connection.execute("DELETE FROM lww_binding_stages WHERE staging_id=?1",[&unattested.staging_id]).unwrap();
    drop(store); let mut store=PersistentStore::open(dir.path()).unwrap();
    for id in [&abandoned,&unattested.staging_id] {
        assert_eq!(commit::generation_state(&store.connection,id).unwrap().as_deref(),Some("retired"));
    }
    while store.purge_retired_batch(256).unwrap() {}
    for id in [&abandoned,&unattested.staging_id] {
        assert_eq!(store.connection.query_row("SELECT count(*) FROM root WHERE generation=?1",[id],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    assert!(store.lww_stage_binding_units(&other_header,&other_inspection,&[],0.into()).is_err());
    assert_eq!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap(),stage);
}

#[test]
fn uncertain_target_activation_replays_device_intent_without_replacing_later_edits_or_losing_holds() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["character","missing","name"],json!("held")),change(&["root","language"],json!("remote"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let state=store.lww_binding_state().unwrap();
    let state=store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header:Header{binding_authority:state.target_authority,request_id:uuid::Uuid::new_v4().to_string()},expected_selection_epoch:state.selection_epoch,
        target:SyncTarget::Server("remote".into()),inspection_id:Some(inspection.clone()),
    }).unwrap();
    let activation=Header{binding_authority:state.target_authority,request_id:header.request_id.clone()};
    let source=sorted_source(&incoming).unwrap().into_iter().cloned().collect::<Vec<_>>();
    let rows=super::super::intent_rows::write(&mut store.device_store_mut().unwrap().connection,&activation.request_id,"target",source.iter().map(super::super::intent_rows::target_row)).unwrap();
    let intent=super::super::Intent::Target{device_revision: super::super::device_revision(store.device_store().unwrap().connection()).unwrap(),staging_id:stage.staging_id.clone(),changes:rows};
    let (stamp,digest)=store.reserve_intent(&activation,&intent).unwrap();
    let result=commit::replace_commit_lww(&mut store.connection,&store.repository_root,&stage.staging_id,&activation,&stamp,&digest,&[],true,&source,None,None,None).unwrap();
    drop(store); let mut store=PersistentStore::open(dir.path()).unwrap();
    assert_eq!(store.revision().unwrap(),result.revision);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE status='held'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    save(&mut store,vec![UnitMutation::Set{key:unit_key(&["root","language"]).unwrap(),value:json!("later")}]);
    assert_eq!(store.lww_replace_target(&activation,&stage.staging_id,&source).unwrap().revision,result.revision);
    assert_eq!(store.read_root(None).unwrap().value["language"],"later");
    assert_eq!(store.connection.query_row("SELECT source_digest FROM lww_binding_sources WHERE staging_id=?1",[&stage.staging_id],|r|r.get::<_,String>(0)).unwrap(),stage.source_digest);
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).is_err());
}

#[test]
fn target_proof_and_status_corruption_cannot_change_frozen_source_digest() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["character","missing","name"],json!("hidden"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let mut different=incoming.clone(); different[0].stamp.logical=1;
    assert!(validate_binding_source(&store.connection,&store.repository_root,&stage.staging_id,&header,&different).is_err());
    store.connection.execute("UPDATE lww_binding_source_units SET status='ready' WHERE staging_id=?1",[&stage.staging_id]).unwrap();
    assert!(validate_binding_source(&store.connection,&store.repository_root,&stage.staging_id,&header,&incoming).is_err());
    store.connection.execute("UPDATE lww_binding_source_units SET status='held',key='not-json' WHERE staging_id=?1",[&stage.staging_id]).unwrap();
    assert!(matches!(validate_binding_source(&store.connection,&store.repository_root,&stage.staging_id,&header,&incoming),Err(crate::persistent_store::StoreError::Validation { .. })));
    assert_eq!(store.connection.query_row("SELECT source_digest FROM lww_binding_sources WHERE staging_id=?1",[&stage.staging_id],|r|r.get::<_,String>(0)).unwrap(),stage.source_digest);
}

#[test]
fn missing_source_or_status_proof_fails_before_switch_activation_or_writer_reservation() {
    for delete_source in [true,false] {
        let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
        let (header,inspection)=context(&store); let incoming=vec![change(&["character","missing","name"],json!("held"))];
        let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
        let table=if delete_source { "lww_binding_sources" } else { "lww_binding_source_units" };
        store.connection.execute(&format!("DELETE FROM {table} WHERE staging_id=?1"),[&stage.staging_id]).unwrap();
        let clock=serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(); let state=store.lww_binding_state().unwrap();
        assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).is_err());
        assert!(store.switch_lww_binding(&SwitchBindingRequest {
            initial_publication: false,
            header:Header {binding_authority:state.target_authority,request_id:uuid::Uuid::new_v4().to_string()},expected_selection_epoch:state.selection_epoch.clone(),target:SyncTarget::Server("remote".into()),inspection_id:Some(inspection),
        }).is_err());
        assert!(store.prepare_lww_new_device(&header,&stage.staging_id).is_err());
        assert!(store.lww_replace_target(&header,&stage.staging_id,&incoming).is_err());
        assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(),clock);
        assert_eq!(store.lww_binding_state().unwrap().selection_epoch,state.selection_epoch);
        assert_eq!(store.device_store().unwrap().connection().query_row("SELECT count(*) FROM lww_new_device_authorizations",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(store.device_store().unwrap().connection().query_row("SELECT count(*) FROM lww_intents",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(store.revision().unwrap(),0);
    }
}

#[test]
fn missing_source_after_switch_rejects_replacement_without_creating_intent_or_changing_clock() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["root","language"],json!("remote"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let state=store.lww_binding_state().unwrap();
    let state=store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header:Header{binding_authority:state.target_authority,request_id:uuid::Uuid::new_v4().to_string()},expected_selection_epoch:state.selection_epoch,target:SyncTarget::Server("remote".into()),inspection_id:Some(inspection),
    }).unwrap();
    store.connection.execute("DELETE FROM lww_binding_sources WHERE staging_id=?1",[&stage.staging_id]).unwrap();
    let clock=serde_json::to_value(store.lww_clock_state().unwrap()).unwrap();
    assert!(store.replace_lww_binding(&ReplaceBindingRequest {
        header:Header{binding_authority:state.target_authority,request_id:header.request_id.clone()},expected_selection_epoch:state.selection_epoch,
        staging_id:stage.staging_id,receive_id:header.request_id,target_id:"target".into(),library_id:"library".into(),
    }).is_err());
    assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(),clock);
    assert_eq!(store.device_store().unwrap().connection().query_row("SELECT count(*) FROM lww_intents WHERE complete=0",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(store.revision().unwrap(),0);
}

#[test]
fn pending_switch_journal_rechecks_missing_source_before_authority_reservation() {
    use crate::persistent_store::sync_selection::BindingSelectionChange;
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    let (header,inspection)=context(&store); let incoming=vec![change(&["root","language"],json!("remote"))];
    let stage=store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    let state=store.lww_binding_state().unwrap();
    let request=SwitchBindingRequest {
        initial_publication: false,
        header:Header{binding_authority:state.target_authority,request_id:uuid::Uuid::new_v4().to_string()},expected_selection_epoch:state.selection_epoch.clone(),target:SyncTarget::Server("remote".into()),inspection_id:Some(inspection.clone()),
    };
    let selection=BindingSelectionChange { initial_publication: false,expected_epoch:state.selection_epoch.clone(),new_epoch:uuid::Uuid::new_v4().to_string(),target:request.target.clone(),library_id:Some("library".into()),target_id:Some("target".into()),inspection_id:Some(inspection)};
    store.connection.execute("INSERT INTO lww_binding_switch_requests VALUES(?1,?2,?3)",params![request.header.request_id,serde_json::to_string(&request).unwrap(),serde_json::to_string(&selection).unwrap()]).unwrap();
    store.connection.execute("DELETE FROM lww_binding_sources WHERE staging_id=?1",[&stage.staging_id]).unwrap();
    assert!(store.switch_lww_binding(&request).is_err());
    assert_eq!(store.lww_binding_authority().unwrap(),state.target_authority);
    assert_eq!(store.lww_binding_state().unwrap().selection_epoch,state.selection_epoch);
    assert_eq!(store.device_store().unwrap().connection().query_row("SELECT count(*) FROM lww_intents",[],|r|r.get::<_,i64>(0)).unwrap(),0);
}

#[test]
fn empty_incoming_stage_copies_no_active_records_and_preserves_exact_empty_receipt() {
    let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
    save(&mut store,vec![UnitMutation::Set{key:unit_key(&["exists","character","local"]).unwrap(),value:json!(true)}]);
    let (header,inspection)=context(&store); let stage=store.lww_stage_binding_units(&header,&inspection,&[],0.into()).unwrap();
    assert!(catalog(&store,&stage.staging_id)["characters"].as_array().unwrap().is_empty());
    assert_eq!(store.connection.query_row("SELECT changes FROM lww_binding_stages WHERE staging_id=?1",[&stage.staging_id],|r|r.get::<_,String>(0)).unwrap(),stage.source_digest);
    activate(&mut store,&header,&inspection,&stage);
    assert!(catalog(&store,&active_generation(&store.connection).unwrap())["characters"].as_array().unwrap().is_empty());
}

#[test]
fn binding_stage_digest_streams_large_message_catalog_and_detects_tail_changes() {
    use crate::persistent_store::{hash_work::{reset_hash_work, take_hash_work}, sync_selection::binding_stage_digest};
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    let message = json!({"role":"char","data":"x".repeat(4096),"chatId":"synthetic"}).to_string();
    {
        let tx = store.connection.transaction().unwrap();
        let mut insert = tx.prepare("INSERT INTO messages(generation,character_id,conversation_id,message_index,value) VALUES(?1,'character','conversation',?2,?3)").unwrap();
        for index in 0..8192 { insert.execute(params![staging,index,message]).unwrap(); }
        drop(insert);
        tx.commit().unwrap();
    }
    let expected = catalog_digest(&store.connection, &staging).unwrap();
    reset_hash_work();
    assert_eq!(binding_stage_digest(&store.connection, &staging).unwrap(), expected);
    let work = take_hash_work();
    assert!(work.incomplete.is_empty());
    assert_eq!(work.domains["binding_catalog_proof"].calls, 1);
    assert!(work.domains["binding_catalog_proof"].bytes > 32 * 1024 * 1024);
    assert!(!work.domains.contains_key("binding_stage_proof"));
    store.connection.execute("UPDATE messages SET value='{}' WHERE generation=?1 AND message_index=8191",[&staging]).unwrap();
    assert_ne!(binding_stage_digest(&store.connection, &staging).unwrap(), expected);
    store.connection.execute("UPDATE messages SET value=?2 WHERE generation=?1 AND message_index=8191",params![staging,message]).unwrap();
    assert_eq!(binding_stage_digest(&store.connection, &staging).unwrap(), expected);
}

#[test]
fn binding_stage_digest_covers_raw_root_plugin_owners_and_asset_metadata() {
    use crate::persistent_store::sync_selection::binding_stage_digest;
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let staging = store.replace_begin().unwrap().staging_id;
    let root = json!({"futureRoot":{"nested":true},"hypaV3":{"memos":[{"chatId":"synthetic"}]},"plugins":[{"name":"synthetic"}]}).to_string();
    store.connection.execute("UPDATE root SET value=?2 WHERE generation=?1",params![staging,root]).unwrap();
    store.connection.execute("INSERT INTO plugin_storage(generation,owner,storage_key,byte_size,ordinal,value) VALUES(?1,'orphan','key',2,0,'{}')",[&staging]).unwrap();
    store.connection.execute("INSERT INTO asset_aliases(generation,logical_key,kind,size,mime,name,ext,metadata) VALUES(?1,'assets/synthetic','asset',0,'application/octet-stream','synthetic','bin','{}')",[&staging]).unwrap();
    store.connection.execute("INSERT INTO asset_owner_heads(generation,owner_kind,owner_locator,present,manifest_hash,entry_count) VALUES(?1,'root-module-assets','synthetic',1,?2,0)",params![staging,"ab".repeat(32)]).unwrap();
    let expected = binding_stage_digest(&store.connection, &staging).unwrap();
    for sql in [
        "UPDATE root SET value='{}' WHERE generation=?1",
        "UPDATE plugin_storage SET owner='different' WHERE generation=?1",
        "UPDATE plugin_storage SET value='null' WHERE generation=?1",
        "UPDATE plugin_storage SET ordinal=1 WHERE generation=?1",
        "UPDATE asset_aliases SET metadata='{\"unknown\":true}' WHERE generation=?1",
        "UPDATE asset_aliases SET size=1 WHERE generation=?1",
        "UPDATE asset_owner_heads SET entry_count=1 WHERE generation=?1",
        "DELETE FROM asset_owner_heads WHERE generation=?1",
    ] {
        let tx = store.connection.transaction().unwrap();
        tx.execute(sql,[&staging]).unwrap();
        assert_ne!(binding_stage_digest(&tx, &staging).unwrap(), expected, "{sql}");
        tx.rollback().unwrap();
        assert_eq!(binding_stage_digest(&store.connection, &staging).unwrap(), expected);
    }
}

#[test]
fn triggered_binding_copy_does_not_spool_source_rows_into_an_ephemeral_btree() {
    let dir = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(dir.path()).unwrap();
    store.connection.execute("ATTACH DATABASE ?1 AS binding_incoming", [store.database_path.to_string_lossy().as_ref()]).unwrap();
    let columns = catalog_columns(&store.connection, "messages").unwrap();
    let opcodes = |sql: &str| {
        let expanded = store.connection.prepare(sql).unwrap().expanded_sql().unwrap();
        let mut stmt = store.connection.prepare(&format!("EXPLAIN {expanded}")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    let bulk = format!("INSERT INTO messages(generation,{0}) SELECT ?1,{0} FROM binding_incoming.messages WHERE generation='incoming'", columns.join(","));
    assert!(opcodes(&bulk).iter().any(|opcode| opcode == "OpenEphemeral"));
    assert!(!opcodes(&binding_copy_insert_sql("messages", &columns)).iter().any(|opcode| opcode == "OpenEphemeral"));
    store.connection.execute_batch("DETACH DATABASE binding_incoming").unwrap();
}

#[test]
fn binding_copy_streams_large_multicolumn_catalog_without_changing_source_types() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("incoming.sqlite");
    let mut source = Connection::open(&path).unwrap();
    schema::initialize(&mut source).unwrap();
    source.execute("INSERT INTO root VALUES('incoming','{}')", []).unwrap();
    let body = json!({"role":"char","data":"x".repeat(4096)}).to_string();
    {
        let tx = source.transaction().unwrap();
        let mut insert = tx.prepare("INSERT INTO messages(generation,character_id,conversation_id,message_index,message_id,value,canonical_hash,canonical_size) VALUES('incoming','character','conversation',?1,NULL,?2,'synthetic-hash',?3)").unwrap();
        for index in 0..8192 { insert.execute(params![index,body,body.len() as i64]).unwrap(); }
        drop(insert);
        tx.execute("INSERT INTO plugin_storage(generation,owner,storage_key,byte_size,ordinal,value,assigned_at) VALUES('incoming','orphan','raw-types',3,0,?1,?2)", params![vec![0u8,1,255],1.5f64]).unwrap();
        tx.commit().unwrap();
    }
    let expected = catalog_digest(&source, "incoming").unwrap();
    source.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let mut store = PersistentStore::open(&dir.path().join("target")).unwrap();
    store.connection.execute("ATTACH DATABASE ?1 AS binding_incoming", [path.to_string_lossy().as_ref()]).unwrap();
    {
        let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        for &(table, _) in GENERATION_TABLES { copy_binding_table_observed(&tx, table, "copied", &mut || Ok(())).unwrap(); }
        assert_eq!(catalog_digest(&tx, "copied").unwrap(), expected);
        tx.commit().unwrap();
    }
    store.connection.execute_batch("DETACH DATABASE binding_incoming").unwrap();
    assert_eq!(catalog_digest(&source, "incoming").unwrap(), expected);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM messages WHERE generation='copied'", [], |row| row.get::<_,i64>(0)).unwrap(),8192);
    let types: (String,String,String) = store.connection.query_row("SELECT typeof(value),typeof(assigned_at),typeof(claimed_from) FROM plugin_storage WHERE generation='copied'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(types,("blob".into(),"real".into(),"null".into()));
}

#[test]
fn late_binding_copy_trigger_failure_rolls_back_every_family_and_retries_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let values = (0..513).map(|index| json!({"role":"char","data":"x".repeat(4096),"chatId":format!("synthetic-{index}")})).collect::<Vec<_>>();
    let mut messages = change(&["messages","character","conversation"], Value::Null);
    messages.value = message_value(&store, &values);
    let incoming = vec![change(&["root","language"],json!("remote")),change(&["exists","character","character"],json!(true)),change(&["exists","conversation","character","conversation"],json!(true)),messages];
    let (header,inspection) = context(&store);
    let generation = active_generation(&store.connection).unwrap();
    let clock = serde_json::to_value(store.lww_clock_state().unwrap()).unwrap();
    let source_objects: i64 = store.connection.query_row("SELECT count(*) FROM message_page_objects", [], |row| row.get(0)).unwrap();
    store.connection.pragma_update(None, "wal_autocheckpoint", 73).unwrap();
    store.connection.execute_batch("CREATE TRIGGER reject_binding_tail BEFORE INSERT ON messages WHEN NEW.generation LIKE 'staging-%' AND NEW.message_index=512 BEGIN SELECT RAISE(ABORT,'synthetic tail failure'); END;").unwrap();
    assert!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).is_err());
    assert_eq!(store.connection.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_,i64>(0)).unwrap(), 73);
    assert!(!STAGE_SCRATCH.with(|scratch| scratch.borrow().clone()).unwrap().exists());
    let wal = std::fs::metadata(format!("{}-wal", store.database_path.display())).map_or(0, |metadata| metadata.len());
    assert!(wal < 64 << 10, "the main WAL must be checkpointed after the scratch is removed: {wal}");
    eprintln!("binding_copy main_wal_after_checkpoint={wal}");
    for &(table, _) in GENERATION_TABLES {
        assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table} WHERE generation LIKE 'staging-%'"), [], |row| row.get::<_,i64>(0)).unwrap(),0,"{table}");
    }
    for table in ["lww_binding_sources","lww_binding_source_units","lww_binding_stages"] {
        assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_,i64>(0)).unwrap(),0,"{table}");
    }
    assert_eq!(active_generation(&store.connection).unwrap(),generation);
    assert_eq!(store.revision().unwrap(),0);
    assert_eq!(store.lww_binding_authority().unwrap(),header.binding_authority);
    assert_eq!(serde_json::to_value(store.lww_clock_state().unwrap()).unwrap(),clock);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM message_page_objects", [], |row| row.get::<_,i64>(0)).unwrap(),source_objects);
    store.connection.execute_batch("DROP TRIGGER reject_binding_tail").unwrap();
    let stage = store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap();
    assert_eq!(store.connection.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_,i64>(0)).unwrap(), 73);
    assert!(!STAGE_SCRATCH.with(|scratch| scratch.borrow().clone()).unwrap().exists());
    let wal = std::fs::metadata(format!("{}-wal", store.database_path.display())).map_or(0, |metadata| metadata.len());
    assert!(wal < 64 << 10, "the main WAL must be checkpointed after the scratch is removed: {wal}");
    eprintln!("binding_copy main_wal_after_checkpoint={wal}");
    assert_eq!(store.connection.query_row("SELECT count(*) FROM messages WHERE generation=?1", [&stage.staging_id], |row| row.get::<_,i64>(0)).unwrap(),513);
    assert_eq!(store.lww_stage_binding_units(&header,&inspection,&incoming,7.into()).unwrap(),stage);
    activate(&mut store,&header,&inspection,&stage);
    let last: String = store.connection.query_row("SELECT value FROM messages WHERE generation=?1 AND message_index=512", [active_generation(&store.connection).unwrap()], |row| row.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&last).unwrap(),values[512]);
}

#[test]
fn binding_replacement_applies_large_shared_and_device_units_from_their_bodies() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    store.device_store_mut().unwrap().set_section_participating(crate::persistent_store::device_store::Section::LocalPlugins, true).unwrap();
    let limit = risunest_sync_wire::unit::MAX_INLINE_UNIT_BYTES;
    let object = |store: &PersistentStore, value: &Value| {
        let body = risunest_sync_wire::payload_value::encode(value).unwrap();
        let hash = risunest_sync_wire::hash(&body);
        store.lww_put_object(&hash, &body).unwrap();
        UnitValue::object(RecordDescriptor::content(hash)).unwrap()
    };
    let language = json!("l".repeat(limit + 1)); let plugin = "p".repeat(limit + 7);
    let mut root = change(&["root","language"], Value::Null); root.value = object(&store, &language);
    let mut local = change(&["plugin-local","synthetic-owner","string","large"], Value::Null); local.value = object(&store, &json!(plugin));
    let description = json!("d".repeat(limit + 3));
    let mut detail = change(&["character","synthetic","desc"], Value::Null); detail.value = object(&store, &description);
    let incoming = vec![root.clone(), local.clone(), change(&["exists","character","synthetic"], json!(true)), detail,
        change(&["character","synthetic","name"], json!("Synthetic")), change(&["character","synthetic","notes"], json!("retained notes"))];
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    assert_eq!(catalog(&store, &stage.staging_id)["language"], language);
    let character = catalog(&store, &stage.staging_id)["characters"][0].clone();
    assert_eq!(character["desc"], description);
    assert_eq!(character["name"], "Synthetic");
    assert_eq!(character["notes"], "retained notes");
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(store.read_root(None).unwrap().value["language"], language);
    assert_eq!(catalog(&store, &active_generation(&store.connection).unwrap())["characters"][0], character);
    assert_eq!(read_unit(&store.connection, &root.key).unwrap(), Some((root.stamp.clone(), root.value.clone())));
    let device = store.device_store().unwrap().connection();
    assert_eq!(read_unit(device, &local.key).unwrap(), Some((local.stamp.clone(), local.value.clone())));
    assert_eq!(device.query_row("SELECT value FROM plugin_device_storage WHERE owner='synthetic-owner' AND space='string' AND key='large' AND tombstone=0", [], |r| r.get::<_,String>(0)).unwrap(), plugin);
}

#[test]
fn binding_character_fields_preserve_each_owner_and_flush_the_final_group() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let incoming = vec![
        change(&["exists","character","a"], json!(true)), change(&["exists","character","z"], json!(true)),
        change(&["character","a","desc"], json!("first description")),
        change(&["character","a","futureField"], json!("unprojected")),
        change(&["character","a","name"], json!("First")),
        change(&["character","missing","name"], json!("held")),
        change(&["character","z","desc"], json!("final description")),
        change(&["character","z","name"], json!("Last")),
        deleted_at(&["character","z","notes"], 7),
    ];
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    let characters = catalog(&store, &stage.staging_id)["characters"].clone();
    assert_eq!(characters.as_array().unwrap().len(), 2);
    for (index, id, name, description) in [(0, "a", "First", "first description"), (1, "z", "Last", "final description")] {
        assert_eq!(characters[index]["chaId"], id);
        assert_eq!(characters[index]["name"], name);
        assert_eq!(characters[index]["desc"], description);
        assert!(characters[index].get("futureField").is_none());
    }
    assert!(characters[1].get("notes").is_none());
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(catalog(&store, &active_generation(&store.connection).unwrap())["characters"], characters);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE status='held'", [], |row| row.get::<_,i64>(0)).unwrap(), 1);
}

#[test]
fn binding_character_group_rejects_invalid_or_deleted_name_before_copy() {
    for name in [change(&["character","synthetic","name"], json!(42)), deleted_at(&["character","synthetic","name"], 7)] {
        let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
        let generation = active_generation(&store.connection).unwrap();
        let incoming = vec![change(&["exists","character","synthetic"], json!(true)), name,
            change(&["character","synthetic","notes"], json!("later field cannot repair name"))];
        let (header, inspection) = context(&store);
        let failed = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into());
        assert!(failed.unwrap_err().to_string().contains("requires name"));
        assert_eq!(active_generation(&store.connection).unwrap(), generation);
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_binding_stages", [], |row| row.get::<_,i64>(0)).unwrap(), 0);
    }
}

#[test]
fn a_bootstrap_proves_the_stage_catalog_once_per_boundary() {
    use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work};
    let proofs = || take_hash_work().domains.get("binding_catalog_proof").map_or(0, |work| work.calls);
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let incoming = vec![change(&["root","language"],json!("remote")),change(&["exists","character","character"],json!(true))];
    reset_hash_work();
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    let staged = proofs();
    reset_hash_work();
    let state = store.lww_binding_state().unwrap();
    let state = store.switch_lww_binding(&SwitchBindingRequest {
        header: Header { binding_authority: state.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: state.selection_epoch, target: SyncTarget::Server("remote".into()), inspection_id: Some(inspection.clone()), initial_publication: false,
    }).unwrap();
    let switched = proofs();
    reset_hash_work();
    store.replace_lww_binding(&ReplaceBindingRequest {
        header: Header { binding_authority: state.target_authority, request_id: header.request_id.clone() },
        expected_selection_epoch: state.selection_epoch, staging_id: stage.staging_id.clone(), receive_id: header.request_id.clone(), target_id: "target".into(), library_id: "library".into(),
    }).unwrap();
    let replaced = proofs();
    assert_eq!((staged, switched, replaced), (1, 1, 1));
    assert_eq!(active_generation(&store.connection).unwrap(), stage.staging_id);
}

#[test]
fn a_binding_stage_cannot_be_activated_as_an_ordinary_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &[change(&["root","language"],json!("remote"))], 7.into()).unwrap();
    let revision = store.revision().unwrap();
    assert!(store.replace_commit(&stage.staging_id, Some(revision)).is_err());
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(commit::generation_state(&store.connection, &stage.staging_id).unwrap().as_deref(), Some("staging"));
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(active_generation(&store.connection).unwrap(), stage.staging_id);
}

#[test]
fn streaming_binding_rejects_late_producer_failure_and_retries_without_partial_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let generation = active_generation(&store.connection).unwrap();
    let mut observed = Vec::new();
    let failed = store.lww_stage_binding_units_stream_observed(&header, &inspection, 7.into(), |emit| {
        for index in 0..1025 { emit(change(&["root", &format!("synthetic-{index:04}")], json!(index)))?; }
        Err(error("synthetic-producer-tail"))
    }, |progress| { observed.push(progress); Ok(()) });
    assert!(failed.is_err());
    assert!(observed.iter().any(|progress| progress.phase == "read" && progress.done == 0 && progress.total.is_none()));
    for table in ["lww_binding_stages", "lww_binding_sources", "lww_binding_source_units"] {
        assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_,i64>(0)).unwrap(), 0);
    }
    assert_eq!(active_generation(&store.connection).unwrap(), generation);
    let stage = store.lww_stage_binding_units_stream(&header, &inspection, 7.into(), |emit| {
        for index in (0..1025).rev() { emit(change(&["root", &format!("synthetic-{index:04}")], json!(index)))?; }
        Ok(())
    }).unwrap();
    let retry = store.lww_stage_binding_units_stream(&header, &inspection, 7.into(), |emit| {
        for index in 0..1025 { emit(change(&["root", &format!("synthetic-{index:04}")], json!(index)))?; }
        Ok(())
    }).unwrap();
    assert_eq!(retry, stage);
    assert_eq!(store.connection.query_row("SELECT length(changes) FROM lww_binding_stages WHERE staging_id=?1", [&stage.staging_id], |row| row.get::<_,i64>(0)).unwrap(), 64);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_binding_source_units WHERE staging_id=?1", [&stage.staging_id], |row| row.get::<_,i64>(0)).unwrap(), 1025);
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_units", [], |row| row.get::<_,i64>(0)).unwrap(), 1025);
}

#[test]
fn observed_binding_reports_stored_work_and_cancellation_retries_without_partial_stage() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let generation = active_generation(&store.connection).unwrap();
    let original = catalog(&store, &generation);
    let incoming = (0..129).map(|index|
        change(&["variable", &format!("synthetic-{index:03}")], json!(index))
    ).collect::<Vec<_>>();
    let expected = (0..129).map(|index| (format!("synthetic-{index:03}"), json!(index)))
        .collect::<serde_json::Map<String, Value>>();
    struct RestoreInterval(std::time::Duration);
    impl Drop for RestoreInterval { fn drop(&mut self) { REPORT_INTERVAL.with(|interval| interval.set(self.0)); } }
    let _restore = RestoreInterval(REPORT_INTERVAL.with(|interval| interval.replace(std::time::Duration::ZERO)));
    for phase in ["validate", "project", "copy", "check"] {
        let mut cancelled_at = None;
        let failed = store.lww_stage_binding_units_observed(&header, &inspection, &incoming, 7.into(), |progress| {
            if progress.phase == phase && progress.done > 0 {
                cancelled_at = Some(progress.done);
                return Err(error("synthetic-observer-cancelled"));
            }
            Ok(())
        });
        assert!(failed.unwrap_err().to_string().contains("synthetic-observer-cancelled"));
        assert_eq!(cancelled_at, Some(1), "{phase}");
        for &(table, _) in GENERATION_TABLES {
            assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table} WHERE generation LIKE 'staging-%'"), [], |row| row.get::<_,i64>(0)).unwrap(), 0, "{phase}: {table}");
        }
        for table in ["lww_binding_stages", "lww_binding_sources", "lww_binding_source_units"] {
            assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_,i64>(0)).unwrap(), 0, "{phase}: {table}");
        }
        assert_eq!(active_generation(&store.connection).unwrap(), generation);
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(catalog(&store, &generation), original);
    }
    let mut observed = Vec::new();
    let stage = store.lww_stage_binding_units_observed(&header, &inspection, &incoming, 7.into(), |progress| {
        observed.push(progress);
        Ok(())
    }).unwrap();
    let count = incoming.len() as u64;
    let copied: u64 = GENERATION_TABLES.iter().map(|(table, _)| {
        store.connection.query_row(&format!("SELECT count(*) FROM {table} WHERE generation=?1"), [&stage.staging_id], |row| row.get::<_,i64>(0)).unwrap() as u64
    }).sum();
    assert!(copied > 0);
    // Each phase is one bar with a fixed total that fills without restarting.
    let mut phases = observed.iter().map(|progress| (progress.phase, progress.total)).collect::<Vec<_>>();
    phases.dedup();
    assert_eq!(phases, [("read", None), ("validate", Some(count * 2)), ("project", Some(count * 4)), ("copy", Some(copied)), ("check", Some(copied))]);
    assert!(observed.windows(2).all(|pair| pair[0].phase != pair[1].phase || pair[0].done <= pair[1].done));
    assert!(observed.iter().any(|progress| progress.phase == "read" && progress.done == count));
    for (phase, total) in &phases[1..] {
        assert_eq!(Some(observed.iter().rev().find(|progress| progress.phase == *phase).unwrap().done), *total, "{phase}");
    }
    assert_eq!(catalog(&store, &stage.staging_id)["explicitGlobalChatVariables"], json!(expected));
    assert_eq!(active_generation(&store.connection).unwrap(), generation);
    assert_eq!(store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap(), stage);
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(catalog(&store, &active_generation(&store.connection).unwrap())["explicitGlobalChatVariables"], json!(expected));
}

#[test]
fn binding_scratch_commits_bounded_batches_so_its_wal_never_holds_the_whole_stage() {
    struct Restore((i64, i64), std::time::Duration);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCRATCH_BATCH_LIMITS.with(|limits| limits.set(self.0));
            REPORT_INTERVAL.with(|interval| interval.set(self.1));
        }
    }
    let _restore = Restore(SCRATCH_BATCH_LIMITS.with(std::cell::Cell::get), REPORT_INTERVAL.with(|interval| interval.replace(std::time::Duration::ZERO)));
    fn fixture(store: &PersistentStore) -> Vec<Change> {
        let mut incoming = vec![change(&["exists","character","character"], json!(true)), change(&["character","character","name"], json!("Synthetic"))];
        for chat in 0..32 {
            let conversation = format!("conversation-{chat:02}");
            let values = (0..32).map(|index| json!({"role":"char","data":"x".repeat(4096),"chatId":format!("{conversation}-{index}")})).collect::<Vec<_>>();
            let mut messages = change(&["messages","character",&conversation], Value::Null);
            messages.value = message_value(store, &values);
            incoming.push(change(&["exists","conversation","character",&conversation], json!(true)));
            incoming.push(messages);
        }
        incoming
    }
    // Every observation while the scratch WAL exists, in order. Its file keeps the
    // largest size any commit reached, since a checkpoint lets later commits reuse it.
    fn staged(store: &mut PersistentStore, header: &Header, inspection: &str, incoming: &[Change], cancel_at: Option<u64>) -> (StoreResult<BindingUnitStage>, Vec<(&'static str, u64)>) {
        STAGE_SCRATCH.with(|scratch| scratch.replace(None));
        let mut sizes = Vec::new();
        let staged = store.lww_stage_binding_units_observed(header, inspection, incoming, 7.into(), |progress| {
            let wal = STAGE_SCRATCH.with(|scratch| scratch.borrow().as_ref().and_then(|path| std::fs::metadata(path.join("incoming.sqlite-wal")).ok()));
            if let Some(wal) = wal { sizes.push((progress.phase, wal.len())); }
            if progress.phase == "project" && Some(progress.done) == cancel_at { return Err(error("synthetic-observer-cancelled")); }
            Ok(())
        });
        (staged, sizes)
    }
    let growth = |sizes: &[(&str, u64)]| sizes.iter().map(|(_, size)| *size).max().unwrap() - sizes[0].1;
    let digests = |store: &PersistentStore, stage: &BindingUnitStage| store.connection.query_row(
        "SELECT status_digest,catalog_digest FROM lww_binding_sources WHERE staging_id=?1", [&stage.staging_id],
        |row| Ok((row.get::<_,String>(0)?, row.get::<_,String>(1)?)),
    ).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let incoming = fixture(&store);
    let controls: i64 = store.connection.query_row("SELECT sum(length(body)) FROM message_page_objects", [], |row| row.get(0)).unwrap();
    assert!(controls > 4 << 20);
    let (header, inspection) = context(&store);
    let generation = active_generation(&store.connection).unwrap();
    SCRATCH_BATCH_LIMITS.with(|limits| limits.set((i64::MAX, 64 << 10)));
    // A cancellation after committed scratch batches leaves nothing behind.
    let (failed, _) = staged(&mut store, &header, &inspection, &incoming, Some(incoming.len() as u64 * 3 / 2));
    assert!(failed.unwrap_err().to_string().contains("synthetic-observer-cancelled"));
    assert!(!STAGE_SCRATCH.with(|scratch| scratch.take()).unwrap().exists());
    for &(table, _) in GENERATION_TABLES {
        assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table} WHERE generation LIKE 'staging-%'"), [], |row| row.get::<_,i64>(0)).unwrap(), 0, "{table}");
    }
    for table in ["lww_binding_stages", "lww_binding_sources", "lww_binding_source_units"] {
        assert_eq!(store.connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get::<_,i64>(0)).unwrap(), 0, "{table}");
    }
    assert_eq!(active_generation(&store.connection).unwrap(), generation);
    let (stage, sizes) = staged(&mut store, &header, &inspection, &incoming, None);
    let stage = stage.unwrap();
    assert!(sizes.iter().any(|(phase, _)| *phase == "validate") && sizes.iter().any(|(phase, _)| *phase == "project"));
    // The WAL grows by about one batch past the schema, never by the copied controls or projected messages.
    assert!(growth(&sizes) < 1 << 20, "{}", growth(&sizes));
    assert_eq!(store.connection.query_row("SELECT count(*) FROM messages WHERE generation=?1", [&stage.staging_id], |row| row.get::<_,i64>(0)).unwrap(), 32 * 32);
    // One batch for the same input holds every page in the WAL and stages identical content.
    SCRATCH_BATCH_LIMITS.with(|limits| limits.set((SCRATCH_BATCH_ROWS, SCRATCH_BATCH_BYTES)));
    let reference_dir = tempfile::tempdir().unwrap();
    let mut reference = PersistentStore::open(reference_dir.path()).unwrap();
    let reference_incoming = fixture(&reference);
    let (header, inspection) = context(&reference);
    let (unbatched, sizes) = staged(&mut reference, &header, &inspection, &reference_incoming, None);
    let unbatched = unbatched.unwrap();
    assert!(growth(&sizes) > controls as u64, "{}", growth(&sizes));
    assert_eq!(unbatched.source_digest, stage.source_digest);
    assert_eq!(digests(&reference, &unbatched), digests(&store, &stage));
}

#[test]
fn binding_rewrites_large_character_rows_with_a_bounded_scratch_wal() {
    struct Restore((i64, i64), std::time::Duration);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCRATCH_BATCH_LIMITS.with(|limits| limits.set(self.0));
            REPORT_INTERVAL.with(|interval| interval.set(self.1));
        }
    }
    let _restore = Restore(SCRATCH_BATCH_LIMITS.with(|limits| limits.replace((i64::MAX, 64 << 10))), REPORT_INTERVAL.with(|interval| interval.replace(std::time::Duration::ZERO)));
    for count in [8, 32] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let ids = (0..count).map(|index| format!("synthetic-{index:03}")).collect::<Vec<_>>();
        let mut incoming = Vec::new();
        for id in &ids {
            incoming.push(change(&["exists", "character", id], json!(true)));
            incoming.push(change(&["character", id, "desc"], json!("x".repeat(64 << 10))));
            // The second conversation changes the summary integer's stored width.
            for conversation in ["first", "second"] {
                incoming.push(change(&["exists", "conversation", id, conversation], json!(true)));
            }
        }
        let expected = ids.iter().rev().cloned().collect::<Vec<_>>();
        incoming.push(change(&["order", "characters"], json!(expected)));
        let (header, inspection) = context(&store);
        let mut peak = 0;
        let mut main_after = 0;
        let stage = store.lww_stage_binding_units_observed(&header, &inspection, &incoming, 7.into(), |progress| {
            if progress.phase == "project" {
                STAGE_SCRATCH.with(|scratch| {
                    if let Some(path) = scratch.borrow().as_ref() {
                        peak = peak.max(std::fs::metadata(path.join("incoming.sqlite-wal")).map_or(0, |metadata| metadata.len()));
                    }
                });
            }
            if progress.phase == "check" { main_after += 1; }
            Ok(())
        }).unwrap();
        let stored = catalog(&store, &stage.staging_id);
        assert_eq!(stored["characters"].as_array().unwrap().iter().map(|character| character["chaId"].as_str().unwrap().to_owned()).collect::<Vec<_>>(), expected);
        assert!(stored["characters"].as_array().unwrap().iter().all(|character| character["desc"].as_str().unwrap().len() == 64 << 10));
        assert!(conversation_counts(&store, &stage.staging_id).iter().all(|(_, stored, actual)| *stored == 2 && *actual == 2));
        assert!(main_after > 0);
        assert!(peak < 768 << 10, "{count} rewritten character rows reached {peak} scratch WAL bytes");
        eprintln!("binding_rewrites characters={count} detail_bytes={} scratch_wal_peak={peak}", count * (64 << 10));
    }
}

#[test]
fn streamed_binding_source_digest_matches_the_complete_sorted_input() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(dir.path()).unwrap();
    let (header, inspection) = context(&store);
    let changes = vec![change(&["root","username"], json!("synthetic")), change(&["root","language"], json!("ko"))];
    let expected = source_digest(&sorted_source(&changes).unwrap()).unwrap();
    let stage = store.lww_stage_binding_units_stream(&header, &inspection, 7.into(), |emit| {
        for change in &changes { emit(change.clone())?; }
        Ok(())
    }).unwrap();
    assert_eq!(stage.source_digest, expected);
    let rows = binding_source_rows(&store.connection, &store.repository_root, &stage.staging_id).unwrap();
    assert_eq!(source_digest_rows(&rows).unwrap(), expected);
}

fn change_at(key: &[&str], value: Value, physical_ms: u64) -> Change {
    let mut change = change(key, value);
    change.stamp.physical_ms = physical_ms.into();
    change
}
fn deleted_at(key: &[&str], physical_ms: u64) -> Change {
    let mut change = change_at(key, Value::Null, physical_ms);
    change.value = UnitValue::Deleted;
    change
}
fn receive(store: &mut PersistentStore, cursor: u64, changes: Vec<Change>) {
    let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() };
    store.lww_stage_receive(&StageReceive { header: header.clone(), changes, progress: Progress { kind: "server".into(), cursor: cursor.into(), writer_id: None }, admitted_time_upper_ms: 100.into() }).unwrap();
    store.lww_apply_receive(&ApplyReceive { header: header.clone(), generating: vec![] }).unwrap();
    store.lww_finish_receive(&header).unwrap();
}
/// Each character's stored conversation count beside the conversation rows it counts.
fn conversation_counts(store: &PersistentStore, generation: &str) -> Vec<(String, i64, i64)> {
    let mut statement = store.connection.prepare("SELECT c.character_id,c.conversation_count,(SELECT count(*) FROM conversations x WHERE x.generation=c.generation AND x.character_id=c.character_id) FROM characters c WHERE c.generation=?1 ORDER BY c.character_id").unwrap();
    let rows = statement.query_map([generation], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
    rows
}
fn chat_page(store: &PersistentStore, character_id: &str) -> Value {
    let detail: String = store.connection.query_row("SELECT detail FROM characters WHERE generation=?1 AND character_id=?2", params![active_generation(&store.connection).unwrap(), character_id], |r| r.get(0)).unwrap();
    serde_json::from_str::<Value>(&detail).unwrap()["chatPage"].clone()
}
fn select_chat_page(store: &PersistentStore, character_id: &str, index: u64) {
    let generation = active_generation(&store.connection).unwrap();
    let detail: String = store.connection.query_row("SELECT detail FROM characters WHERE generation=?1 AND character_id=?2", params![generation, character_id], |r| r.get(0)).unwrap();
    let mut detail: Value = serde_json::from_str(&detail).unwrap();
    detail["chatPage"] = json!(index);
    store.connection.execute("UPDATE characters SET detail=?3 WHERE generation=?1 AND character_id=?2", params![generation, character_id, detail.to_string()]).unwrap();
}
/// The fail-fast library gate an export capture runs, and what a data health scan reports.
fn export_gate(store: &mut PersistentStore) -> (Result<(), String>, Vec<crate::data_health::Finding>) {
    let lease = store.acquire_revision(store.revision().unwrap()).unwrap().lease;
    let reader = store.data_health_reader(&lease).unwrap();
    let gate = crate::portable_backup::validate_live_library(&reader.connection, &reader.cas, &crate::local_backup::NeverCancelled).map(|_| ()).map_err(|error| error.to_string());
    let findings = reader.scan(256, &crate::local_backup::NeverCancelled).unwrap().items;
    drop(reader);
    store.release_revision(&lease).unwrap();
    (gate, findings)
}

/// Findings outside the root. The fixture root names no preset or persona, so the scan reports
/// those two root references whatever the records hold.
fn record_findings(findings: &[crate::data_health::Finding]) -> Vec<String> {
    findings.iter().filter(|finding| finding.owner.kind != "root").map(|finding| format!("{} {}/{} {:?} {}", finding.code, finding.owner.kind, finding.owner.id, finding.locator.as_ref().map(|locator| &locator.source_path), finding.detail)).collect()
}

#[test]
fn a_replaced_library_counts_the_conversations_projected_after_their_character() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let mut messages = change(&["messages","windowed","chat-1"], Value::Null);
    messages.value = message_value(&store, &[json!({"role":"user","data":"synthetic-first","chatId":"m1"}), json!({"role":"char","data":"synthetic-second","chatId":"m2"})]);
    let mut incoming = vec![
        change(&["exists","character","windowed"], json!({"type":"character"})),
        change(&["character","windowed","name"], json!("Synthetic Windowed")),
        change(&["order","conversations","windowed"], json!({"ids":["chat-1","chat-2","chat-4","chat-5"],"folders":[]})),
        deleted_at(&["exists","conversation","windowed","chat-3"], 7),
        messages,
        change(&["exists","character","single"], json!({"type":"character"})),
        change(&["character","single","name"], json!("Synthetic Single")),
        change(&["exists","conversation","single","only"], json!(true)),
        change(&["conversation","single","only","name"], json!("Only")),
    ];
    for id in ["chat-1","chat-2","chat-4","chat-5"] {
        incoming.push(change(&["exists","conversation","windowed",id], json!(true)));
        incoming.push(change(&["conversation","windowed",id,"name"], json!(id)));
    }
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    let expected = vec![("single".to_owned(), 1, 1), ("windowed".to_owned(), 4, 4)];
    assert_eq!(conversation_counts(&store, &stage.staging_id), expected);
    activate(&mut store, &header, &inspection, &stage);
    assert_eq!(conversation_counts(&store, &active_generation(&store.connection).unwrap()), expected);
    for id in ["single", "windowed"] { assert_eq!(chat_page(&store, id), json!(0)); }
    let (gate, findings) = export_gate(&mut store);
    assert_eq!(gate, Ok(()));
    assert_eq!(record_findings(&findings), Vec::<String>::new());
}

#[test]
fn received_conversations_count_toward_their_character_in_any_page_order() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let incoming = vec![
        change(&["exists","character","early"], json!({"type":"character"})),
        change(&["character","early","name"], json!("Synthetic Early")),
        change(&["order","conversations","early"], json!({"ids":["first"],"folders":[]})),
        change(&["exists","conversation","early","first"], json!(true)),
        change(&["conversation","early","first","name"], json!("First")),
        change(&["order","conversations","late"], json!({"ids":["held"],"folders":[]})),
        change(&["exists","conversation","late","held"], json!(true)),
        change(&["conversation","late","held","name"], json!("Held")),
    ];
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    activate(&mut store, &header, &inspection, &stage);
    // The conversation order reaches this device one page before the conversation it names.
    receive(&mut store, 1, vec![change_at(&["order","conversations","early"], json!({"ids":["first","second"],"folders":[]}), 8)]);
    receive(&mut store, 2, vec![
        change_at(&["exists","conversation","early","second"], json!(true), 8),
        change_at(&["conversation","early","second","name"], json!("Second"), 8),
    ]);
    // The character of a conversation held since the binding arrives in a later page.
    receive(&mut store, 3, vec![change_at(&["exists","character","late"], json!({"type":"character"}), 8)]);
    let expected = vec![("early".to_owned(), 2, 2), ("late".to_owned(), 1, 1)];
    assert_eq!(conversation_counts(&store, &active_generation(&store.connection).unwrap()), expected);
    for id in ["early", "late"] { assert_eq!(chat_page(&store, id), json!(0)); }
    let (gate, findings) = export_gate(&mut store);
    assert_eq!(gate, Ok(()));
    assert_eq!(record_findings(&findings), Vec::<String>::new());
}

#[test]
fn a_received_deletion_selects_the_first_conversation_when_the_selected_one_is_gone() {
    let dir = tempfile::tempdir().unwrap(); let mut store = PersistentStore::open(dir.path()).unwrap();
    let mut incoming = Vec::new();
    for character in ["last", "kept"] {
        incoming.push(change(&["exists","character",character], json!({"type":"character"})));
        incoming.push(change(&["order","conversations",character], json!({"ids":["chat-1","chat-2","chat-3"],"folders":[]})));
        for id in ["chat-1","chat-2","chat-3"] {
            incoming.push(change(&["exists","conversation",character,id], json!(true)));
            incoming.push(change(&["conversation",character,id,"name"], json!(id)));
        }
    }
    let (header, inspection) = context(&store);
    let stage = store.lww_stage_binding_units(&header, &inspection, &incoming, 7.into()).unwrap();
    activate(&mut store, &header, &inspection, &stage);
    select_chat_page(&store, "last", 2);
    select_chat_page(&store, "kept", 1);
    let mut deletions = Vec::new();
    for character in ["last", "kept"] {
        deletions.push(deleted_at(&["exists","conversation",character,"chat-3"], 8));
        deletions.push(change_at(&["order","conversations",character], json!({"ids":["chat-1","chat-2"],"folders":[]}), 8));
    }
    receive(&mut store, 1, deletions);
    let expected = vec![("kept".to_owned(), 2, 2), ("last".to_owned(), 2, 2)];
    assert_eq!(conversation_counts(&store, &active_generation(&store.connection).unwrap()), expected);
    assert_eq!(chat_page(&store, "last"), json!(0));
    assert_eq!(chat_page(&store, "kept"), json!(1));
    let (gate, findings) = export_gate(&mut store);
    assert_eq!(gate, Ok(()));
    assert_eq!(record_findings(&findings), Vec::<String>::new());
}
