use super::*;
use crate::persistent_store::message_pages;

const LANGUAGE: [&str; 2] = ["root", "language"];
const PLUGIN_KEY: [&str; 4] = ["plugin-local", "synthetic-owner", "string", "large"];
const STRUCTURED_KEY: [&str; 4] = ["plugin-local", "synthetic-owner", "json", "structured"];

fn store() -> (tempfile::TempDir, PersistentStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(dir.path()).unwrap();
    (dir, store)
}
// A JSON string whose canonical form is exactly `len` bytes.
fn text(len: usize) -> String {
    "a".repeat(len - 2)
}
fn canonical(value: &Value) -> Vec<u8> {
    risunest_sync_wire::payload_value::encode(value).unwrap()
}
fn save(store: &mut PersistentStore, parts: &[&str], value: Value) {
    store
        .commit(&WorkingSetCommit {
            expected_revision: store.revision().unwrap(),
            unit_mutations: Some(vec![UnitMutation::Set { key: unit_key(parts).unwrap(), value }]),
            ..Default::default()
        })
        .unwrap();
}
fn outbox_value(store: &PersistentStore, parts: &[&str]) -> OutboxEntry {
    let key = unit_key(parts).unwrap();
    store.lww_read_outbox(store.lww_binding_authority().unwrap(), 1000).unwrap()
        .entries.into_iter().find(|entry| entry.key == key).unwrap()
}
fn ack_all(store: &mut PersistentStore) {
    let entries = store.lww_read_outbox(store.lww_binding_authority().unwrap(), 1000).unwrap().entries;
    store.lww_ack_outbox(
        &Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() },
        &entries.into_iter().map(|entry| AckEntry {
            key: entry.key, version: entry.version, stamp: entry.stamp, value_identity: entry.value.identity().unwrap(),
        }).collect::<Vec<_>>(),
    ).unwrap();
}
fn stamp(time: u64) -> Stamp {
    Stamp { physical_ms: time.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000002".into() }
}
fn stage(store: &mut PersistentStore, change: Change) -> StoreResult<Header> {
    let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() };
    store.lww_stage_receive(&StageReceive {
        header: header.clone(),
        changes: vec![change],
        progress: Progress { kind: "server".into(), cursor: 1.into(), writer_id: None },
        admitted_time_upper_ms: u64::MAX.into(),
    })?;
    Ok(header)
}
fn receive(store: &mut PersistentStore, change: Change) -> ApplyResult {
    let header = stage(store, change).unwrap();
    let result = store.lww_apply_receive(&ApplyReceive { header: header.clone(), generating: vec![] }).unwrap();
    store.lww_finish_receive(&header).unwrap();
    result
}
fn stage_error(store: &mut PersistentStore, change: Change) -> String {
    stage(store, change).unwrap_err().to_string()
}
// Places a body in the library store the way transport receive does, then
// returns the large unit that names it.
fn published(store: &PersistentStore, body: &[u8]) -> UnitValue {
    let hash = risunest_sync_wire::hash(body);
    store.lww_put_object(&hash, body).unwrap();
    UnitValue::object(RecordDescriptor::content(hash)).unwrap()
}
fn object_hash(value: &UnitValue) -> &str {
    match value {
        UnitValue::Object { descriptor, .. } => &descriptor.object_hash,
        other => panic!("expected a large unit, found {}", serde_json::to_string(other).unwrap().len()),
    }
}
fn plugin_value(store: &PersistentStore) -> Option<String> {
    plugin_value_at(store, &PLUGIN_KEY)
}
fn plugin_value_at(store: &PersistentStore, key: &[&str; 4]) -> Option<String> {
    store.device_store().unwrap().connection().query_row(
        "SELECT value FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3 AND tombstone=0",
        [key[1], key[2], key[3]], |row| row.get(0),
    ).optional().unwrap()
}
// Structure text naming `parts` for a stream of `bytes`, padded by `pad` bytes.
fn structured_text(parts: &[String], bytes: u64, pad: usize) -> String {
    format!(
        "#risunest-structured:1\n{{\"parts\":[{}],\"bytes\":{bytes},\"nodes\":[{{\"type\":\"ArrayBuffer\",\"offset\":0,\"byteLength\":{bytes}}}],\"root\":{{\"ref\":0}},\"pad\":\"{}\"}}",
        parts.iter().map(|part| format!("\"p:{part}\"")).collect::<Vec<_>>().join(","), "p".repeat(pad),
    )
}

#[test]
fn shared_values_above_the_inline_bound_capture_as_content_objects() {
    let (_dir, mut store) = store();
    let at_limit = Value::String(text(MAX_INLINE_UNIT_BYTES));
    save(&mut store, &LANGUAGE, at_limit.clone());
    assert_eq!(outbox_value(&store, &LANGUAGE).value, inline(&at_limit).unwrap());

    let large = Value::String(text(MAX_INLINE_UNIT_BYTES + 1));
    save(&mut store, &LANGUAGE, large.clone());
    let entry = outbox_value(&store, &LANGUAGE);
    let body = canonical(&large);
    assert_eq!(entry.value, UnitValue::object(RecordDescriptor::content(risunest_sync_wire::hash(&body))).unwrap());
    assert_eq!(store.lww_object_body(object_hash(&entry.value)).unwrap(), Some(body));
    assert_eq!(read_unit(&store.connection, &entry.key).unwrap().unwrap().1, entry.value);
    assert_eq!(json_value_resolved(&store.connection, &entry.value).unwrap(), Some(large.clone()));
    assert_eq!(json_value(&entry.value).unwrap_err().to_string(), "unit-object-body-required");
    assert_eq!(store.read_root(None).unwrap().value["language"], large);

    ack_all(&mut store);
    save(&mut store, &LANGUAGE, large);
    assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 1000).unwrap().entries.is_empty());
}

#[test]
fn received_large_units_resolve_from_bodies_and_reject_other_object_forms() {
    let (_dir, mut store) = store();
    let large = Value::String(text(MAX_INLINE_UNIT_BYTES + 1));
    let value = published(&store, &canonical(&large));
    receive(&mut store, Change { key: unit_key(&LANGUAGE).unwrap(), stamp: stamp(1), value: value.clone() });
    assert_eq!(store.read_root(None).unwrap().value["language"], large);
    assert_eq!(read_unit(&store.connection, &unit_key(&LANGUAGE).unwrap()).unwrap(), Some((stamp(1), value.clone())));
    assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 1000).unwrap().entries.is_empty());

    let other = |value: UnitValue| Change { key: unit_key(&LANGUAGE).unwrap(), stamp: stamp(2), value };
    let missing = UnitValue::object(RecordDescriptor::content(risunest_sync_wire::hash(b"\"absent\""))).unwrap();
    assert_eq!(stage_error(&mut store, other(missing)), "unit-object-body-required");
    let mut with_dependency = RecordDescriptor::content(object_hash(&value).to_owned());
    with_dependency.dependencies.push(risunest_sync_wire::hash(b"dependency"));
    assert_eq!(stage_error(&mut store, other(UnitValue::object(with_dependency).unwrap())), "invalid-large-unit");
    let small = published(&store, &canonical(&Value::String(text(MAX_INLINE_UNIT_BYTES))));
    assert_eq!(stage_error(&mut store, other(small)), "invalid-large-unit");
    let padded = published(&store, format!("[{}0]", " ".repeat(MAX_INLINE_UNIT_BYTES)).as_bytes());
    assert_eq!(stage_error(&mut store, other(padded)), "invalid-large-unit");
    let alias = Change { key: unit_key(&["asset", "synthetic-asset"]).unwrap(), stamp: stamp(2), value };
    assert_eq!(stage_error(&mut store, alias), "invalid-unit-payload");
}

#[test]
fn large_plugin_local_values_keep_bodies_in_the_device_store() {
    use device_store::plugin_values::PluginDeviceMutation;
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let large = text(MAX_INLINE_UNIT_BYTES + 3);
    store.device_store_mut().unwrap().write_plugin_device_values(PLUGIN_KEY[1], &[
        PluginDeviceMutation::Set { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), value: large.clone() },
    ]).unwrap();
    let entry = outbox_value(&store, &PLUGIN_KEY);
    let body = canonical(&Value::String(large.clone()));
    let hash = object_hash(&entry.value).to_owned();
    assert_eq!(hash, risunest_sync_wire::hash(&body));
    assert_eq!(message_pages::object_body(store.device_store().unwrap().connection(), &hash).unwrap(), Some(body.clone()));
    assert_eq!(message_pages::object_body(&store.connection, &hash).unwrap(), None);
    assert_eq!(store.lww_object_body(&hash).unwrap(), Some(body.clone()));
    assert!(store.external_lww_object_is_control(&hash).unwrap());

    let (_dir, mut target) = self::store();
    target.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let value = published(&target, &body);
    receive(&mut target, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(1), value });
    assert_eq!(plugin_value(&target), Some(large));

    use device_store::plugin_values::text_value;
    let large = format!("{}가😀\"\\", "x".repeat(text_value::PART_BYTES));
    let device = store.device_store_mut().unwrap();
    let opened = device.open_plugin_device_write(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3], text_value::encoded_length(&large)).unwrap();
    let id = serde_json::to_value(opened).unwrap()["sessionId"].as_str().unwrap().to_owned();
    for (offset, part) in [(0, &large[..text_value::PART_BYTES-8]), (text_value::PART_BYTES-8, &large[text_value::PART_BYTES-8..])] {
        device.append_plugin_device_write(PLUGIN_KEY[1], &id, offset as u64, part).unwrap();
    }
    device.write_plugin_device_values(PLUGIN_KEY[1], &[PluginDeviceMutation::Staged { session_id: id }]).unwrap();
    let entry = outbox_value(&store, &PLUGIN_KEY);
    let expected = canonical(&Value::String(large.clone()));
    let hash = object_hash(&entry.value);
    assert_eq!(hash, risunest_sync_wire::hash(&expected));
    let mut source = store.lww_native_object(hash).unwrap().unwrap();
    target.lww_put_object_reader(hash, &mut source, expected.len() as u64).unwrap();
    target.lww_put_object_reader(hash, expected.as_slice(), expected.len() as u64).unwrap();
    receive(&mut target, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(2), value: entry.value.clone() });
    assert_eq!(plugin_value(&target), Some(large.clone()));
    use device_store::plugin_values::{PluginDeviceRead, PluginDeviceRange};
    let Some(PluginDeviceRead::Ranges { version, .. }) = target.device_store().unwrap()
        .read_plugin_device_transfer(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap() else { panic!("expected ranged value"); };
    let mut replacement = large;
    replacement.replace_range(..1, "y");
    let value = published(&target, &canonical(&Value::String(replacement.clone())));
    receive(&mut target, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(3), value });
    assert!(matches!(target.device_store().unwrap()
        .read_plugin_device_range(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3], &version, 0).unwrap(), PluginDeviceRange::Changed));
    assert_eq!(plugin_value(&target), Some(replacement));
    let invalid_hash = "e".repeat(64);
    assert!(target.lww_put_object_reader(&invalid_hash, expected.as_slice(), expected.len() as u64).is_err());
    assert!(target.lww_native_object(&invalid_hash).unwrap().is_none());
}

#[test]
fn large_plugin_local_values_received_while_disabled_apply_on_enable() {
    let (_dir, mut store) = store();
    let large = text(MAX_INLINE_UNIT_BYTES + 5);
    let value = published(&store, &canonical(&Value::String(large.clone())));
    receive(&mut store, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(1), value: value.clone() });
    assert_eq!(plugin_value(&store), None);
    assert!(message_pages::verified_object_present(store.device_store().unwrap().connection(), object_hash(&value)).unwrap());
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    assert_eq!(plugin_value(&store), Some(large));
    assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 1000).unwrap().entries.is_empty());
}

#[test]
fn held_large_units_are_scanned_for_local_asset_roots() {
    let (_dir, mut store) = store();
    let asset = "b".repeat(64);
    let large = serde_json::json!([asset, "x".repeat(MAX_INLINE_UNIT_BYTES)]);
    let value = published(&store, &canonical(&large));
    receive(&mut store, Change { key: unit_key(&["character", "held-character", "desc"]).unwrap(), stamp: stamp(1), value });
    assert_eq!(store.connection.query_row("SELECT count(*) FROM lww_receive_rows WHERE status='held'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    let roots = super::super::snapshot::collect_asset_roots_with_plugin_cache(&store.connection, &Default::default()).unwrap();
    assert!(roots.object_hashes.contains(&asset));
    assert!(roots.blockers.is_empty(), "{:?}", roots.blockers);
}

#[test]
fn large_plugin_local_units_are_scanned_from_the_device_store() {
    let (_dir, mut store) = store();
    let large = format!("assets/{}", "p".repeat(MAX_INLINE_UNIT_BYTES));
    let value = published(&store, &canonical(&Value::String(large.clone())));
    receive(&mut store, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(1), value });
    assert_eq!(plugin_value(&store), None);
    let (_, roots) = super::super::snapshot::collect_device_plugin_asset_roots(&store).unwrap();
    assert!(roots.legacy_asset_keys.contains(&large));
    assert!(roots.blockers.is_empty(), "{:?}", roots.blockers);
}

#[test]
fn large_units_are_backup_controls_above_the_metadata_bound() {
    let (_dir, mut store) = store();
    let large = Value::String(text(risunest_sync_wire::MAX_METADATA_BYTES + 1));
    save(&mut store, &LANGUAGE, large.clone());
    let body = canonical(&large);
    let hash = risunest_sync_wire::hash(&body);
    let (lease, _) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    let units = store.lww_backup_unit_values(&lease.lease).unwrap();
    let closure = store.lww_backup_dependency_closure(&lease.lease, &units, &crate::local_backup::NeverCancelled).unwrap();
    assert_eq!(closure.controls.get(&hash), Some(&body));
    assert!(closure.payload_bodies.is_empty() && closure.managed_payloads.is_empty() && closure.record_payloads.is_empty());
    store.release_revision(&lease.lease).unwrap();
}

fn write_plugin(store: &mut PersistentStore, value: &str) {
    use device_store::plugin_values::PluginDeviceMutation;
    store.device_store_mut().unwrap().write_plugin_device_values(PLUGIN_KEY[1], &[
        PluginDeviceMutation::Set { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), value: value.into() },
    ]).unwrap();
}
fn device_object(store: &PersistentStore, table: &str, hash: &str) -> bool {
    store.device_store().unwrap().connection().query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE hash=?1)"), [hash], |row| row.get(0),
    ).unwrap()
}

#[test]
fn a_superseded_device_large_unit_body_is_reclaimed_once_no_backup_lease_reads_it() {
    use device_store::plugin_values::PluginDeviceMutation;
    let (grace, short) = (super::super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, super::super::SUPERSEDED_OBJECT_GRACE_MS);
    let limit = super::super::MESSAGE_PAGE_SWEEP_LIMIT;
    let device = super::super::MessageObjectStore::Device;
    let clock = || super::super::commands::current_time_ms().unwrap();
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let [old, new, newer, newest] = [3, 4, 5, 6].map(|extra| text(MAX_INLINE_UNIT_BYTES + extra));
    let hash = |value: &str| risunest_sync_wire::hash(&canonical(&Value::String(value.to_owned())));
    write_plugin(&mut store, &old);
    ack_all(&mut store);
    let lease = store.lww_acquire_library_backup_capture(store.revision().unwrap()).unwrap();
    write_plugin(&mut store, &new);
    ack_all(&mut store);
    assert!(device_object(&store, "message_page_objects", &hash(&old)));

    let now = clock();
    store.sweep_message_page_objects(device, now + short, limit).unwrap();
    assert!(device_object(&store, "message_page_objects", &hash(&old)), "a backup lease still reads the old device unit");
    assert!(!device_object(&store, "message_page_object_marks", &hash(&old)), "a referenced body kept its mark");
    store.release_revision(&lease.lease).unwrap();
    store.sweep_message_page_objects(device, now + 2 * short, limit).unwrap();
    assert!(device_object(&store, "message_page_object_marks", &hash(&old)));
    store.sweep_message_page_objects(device, now + 2 * short + grace, limit).unwrap();
    assert!(!device_object(&store, "message_page_objects", &hash(&old)), "the superseded device body survived");

    // With no reader left, a replaced body waits only the short grace, unless
    // another key still names it.
    store.device_store_mut().unwrap().write_plugin_device_values(PLUGIN_KEY[1], &[
        PluginDeviceMutation::Set { space: PLUGIN_KEY[2].into(), key: "copy".into(), value: newer.clone() },
    ]).unwrap();
    let before = clock();
    write_plugin(&mut store, &newer);
    write_plugin(&mut store, &newest);
    let after = clock();
    ack_all(&mut store);
    store.sweep_message_page_objects(device, before + short - 60_000, limit).unwrap();
    assert!(device_object(&store, "message_page_objects", &hash(&new)), "a replaced body went before its grace");
    store.sweep_message_page_objects(device, after + short, limit).unwrap();
    assert!(!device_object(&store, "message_page_objects", &hash(&new)), "a replaced body outlived the short grace");
    assert!(device_object(&store, "message_page_objects", &hash(&newer)), "a body another key names was deleted");
    assert!(!device_object(&store, "message_page_object_marks", &hash(&newer)));
    assert!(!device_object(&store, "message_page_object_marks", &hash(&newest)));
    assert_eq!(plugin_value(&store), Some(newest.clone()));
    assert_eq!(store.lww_object_body(&hash(&newest)).unwrap(), Some(canonical(&Value::String(newest))));
}

#[test]
fn a_marked_body_is_stored_again_before_a_write_or_receive_relies_on_it() {
    let limit = super::super::MESSAGE_PAGE_SWEEP_LIMIT;
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let values = [MAX_INLINE_UNIT_BYTES + 3, MAX_INLINE_UNIT_BYTES + 4, MAX_INLINE_UNIT_BYTES + 5].map(text);
    let hashes = values.clone().map(|value| risunest_sync_wire::hash(&canonical(&Value::String(value))));
    let now = super::super::commands::current_time_ms().unwrap();
    write_plugin(&mut store, &values[0]);
    write_plugin(&mut store, &values[1]);
    ack_all(&mut store);
    store.sweep_message_page_objects(super::super::MessageObjectStore::Device, now, limit).unwrap();
    assert!(device_object(&store, "message_page_object_marks", &hashes[0]));
    write_plugin(&mut store, &values[0]);
    assert!(!device_object(&store, "message_page_object_marks", &hashes[0]), "a local write trusted a marked device body");

    write_plugin(&mut store, &values[2]);
    ack_all(&mut store);
    store.sweep_message_page_objects(super::super::MessageObjectStore::Device, now, limit).unwrap();
    assert!(device_object(&store, "message_page_object_marks", &hashes[1]));
    let value = published(&store, &canonical(&Value::String(values[1].clone())));
    stage(&mut store, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(1), value }).unwrap();
    assert!(!device_object(&store, "message_page_object_marks", &hashes[1]), "a staged receive trusted a marked device body");

    let control = b"synthetic-control-object".to_vec();
    let control_hash = risunest_sync_wire::hash(&control);
    store.lww_put_object(&control_hash, &control).unwrap();
    assert_eq!(store.external_lww_verified_control_size(&control_hash).unwrap(), Some(control.len() as u64));
    store.sweep_message_page_objects(super::super::MessageObjectStore::Library, now, limit).unwrap();
    assert_eq!(store.external_lww_verified_control_size(&control_hash).unwrap(), None, "a marked control was trusted");
    assert!(store.external_lww_object_is_control(&control_hash).unwrap());
    store.lww_put_object(&control_hash, &control).unwrap();
    assert_eq!(store.external_lww_verified_control_size(&control_hash).unwrap(), Some(control.len() as u64));
}


#[test]
fn received_plugin_bodies_wait_the_short_grace_in_either_store() {
    let (short, limit) = (super::super::SUPERSEDED_OBJECT_GRACE_MS, super::super::MESSAGE_PAGE_SWEEP_LIMIT);
    let (library, device) = (super::super::MessageObjectStore::Library, super::super::MessageObjectStore::Device);
    let clock = || super::super::commands::current_time_ms().unwrap();
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let library_object = |store: &PersistentStore, table: &str, hash: &str| -> bool {
        store.connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE hash=?1)"), [hash], |row| row.get(0)).unwrap()
    };
    let bodies = [3, 4, 5, 6].map(|extra| canonical(&Value::String(text(MAX_INLINE_UNIT_BYTES + extra))));
    let hashes = bodies.clone().map(|body| risunest_sync_wire::hash(&body));
    let change = |key: &[&str], value, time| Change { key: unit_key(key).unwrap(), stamp: stamp(time), value };

    // A shared plugin value lives in the library store.
    const SHARED: [&str; 3] = ["plugin", "synthetic-owner", "settings"];
    let value = published(&store, &bodies[0]);
    receive(&mut store, change(&SHARED, value, 1));
    let value = published(&store, &bodies[1]);
    let before = clock();
    receive(&mut store, change(&SHARED, value, 2));
    let after = clock();
    store.sweep_message_page_objects(library, before + short - 60_000, limit).unwrap();
    assert!(library_object(&store, "message_page_objects", &hashes[0]), "a replaced shared body went before its grace");
    store.sweep_message_page_objects(library, after + short, limit).unwrap();
    assert!(!library_object(&store, "message_page_objects", &hashes[0]), "a replaced shared body outlived the short grace");
    assert!(library_object(&store, "message_page_objects", &hashes[1]));

    // A received device value leaves its body in the library store as well.
    let value = published(&store, &bodies[2]);
    receive(&mut store, change(&PLUGIN_KEY, value, 3));
    assert!(device_object(&store, "message_page_objects", &hashes[2]) && library_object(&store, "message_page_objects", &hashes[2]));
    let value = published(&store, &bodies[3]);
    receive(&mut store, change(&PLUGIN_KEY, value, 4));
    let after = clock();
    // A mark the library store refuses keeps the device copy for the next sweep.
    store.connection.execute_batch("CREATE TEMP TRIGGER synthetic_mark_refusal BEFORE INSERT ON main.message_page_object_marks
        BEGIN SELECT RAISE(ABORT,'synthetic mark refusal'); END;").unwrap();
    assert!(store.sweep_message_page_objects(device, after + short, limit).is_err());
    assert!(device_object(&store, "message_page_objects", &hashes[2]), "the device copy went without marking the library copy");
    store.connection.execute_batch("DROP TRIGGER temp.synthetic_mark_refusal").unwrap();
    store.sweep_message_page_objects(device, after + short, limit).unwrap();
    assert!(!device_object(&store, "message_page_objects", &hashes[2]));
    assert!(library_object(&store, "message_page_objects", &hashes[2]));
    store.sweep_message_page_objects(library, after + 2 * short - 60_000, limit).unwrap();
    assert!(library_object(&store, "message_page_objects", &hashes[2]), "the library copy went before its grace");
    store.sweep_message_page_objects(library, after + 2 * short, limit).unwrap();
    assert!(!library_object(&store, "message_page_objects", &hashes[2]), "the library copy of a reclaimed device body stayed");
    assert!(library_object(&store, "message_page_objects", &hashes[3]), "the library copy of the current device body was deleted");
    assert!(device_object(&store, "message_page_objects", &hashes[3]));
    assert_eq!(plugin_value(&store), Some(text(MAX_INLINE_UNIT_BYTES + 6)));
}

#[test]
fn a_body_only_a_pending_received_page_names_survives_the_sweep() {
    use crate::server_sync::lww_client::OperationLog;
    let (grace, limit) = (super::super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, super::super::MESSAGE_PAGE_SWEEP_LIMIT);
    let library = super::super::MessageObjectStore::Library;
    let (_dir, mut store) = store();
    let root = store.repository_root().to_owned();
    let present = |store: &PersistentStore, hash: &str| -> bool {
        store.connection.query_row("SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)", [hash], |row| row.get(0)).unwrap()
    };
    let now = 1_900_000_000_000i64;
    store.sweep_message_page_objects(library, now, limit).unwrap();
    assert!(!OperationLog::path(&root).exists(), "a sweep created the operation log");

    let body = canonical(&Value::String(text(MAX_INLINE_UNIT_BYTES + 7)));
    let hash = risunest_sync_wire::hash(&body);
    let page = StageReceive {
        header: Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: "synthetic-page".into() },
        changes: vec![Change { key: unit_key(&LANGUAGE).unwrap(), stamp: stamp(1), value: published(&store, &body) }],
        progress: Progress { kind: "server".into(), cursor: 1.into(), writer_id: None },
        admitted_time_upper_ms: 1.into(),
    };
    let log = OperationLog::open(&root).unwrap();
    log.0.execute("INSERT INTO receive_pages VALUES('synthetic-authority',?1,NULL,0,0)", [serde_json::to_string(&page).unwrap()]).unwrap();
    for step in 0..3 {
        store.sweep_message_page_objects(library, now + step * grace, limit).unwrap();
    }
    assert!(present(&store, &hash), "a body a pending received page names was deleted");
    log.0.execute("DELETE FROM receive_pages", []).unwrap();
    store.sweep_message_page_objects(library, now + 3 * grace, limit).unwrap();
    store.sweep_message_page_objects(library, now + 4 * grace, limit).unwrap();
    assert!(!present(&store, &hash));
}

#[test]
fn conditional_plugin_local_writes_reject_received_values_deletions_and_staged_payloads() {
    use device_store::plugin_values::{PluginDeviceMutation as Mutation, PluginDeviceObservation, text_value};
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let change = |value: &str, time| Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(time), value: inline(&serde_json::json!(value)).unwrap() };
    receive(&mut store, change("old", 1));
    let observed = PluginDeviceObservation { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(),
        sequence: store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap() };
    let staged = store.device_store_mut().unwrap().open_plugin_device_write(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3], 7).unwrap();
    let id = serde_json::to_value(staged).unwrap()["sessionId"].as_str().unwrap().to_owned();
    store.device_store_mut().unwrap().append_plugin_device_write(PLUGIN_KEY[1], &id, 0, "stale").unwrap();
    assert_eq!(receive(&mut store, change("remote", 2)).plugin_storage_changed_keys, vec![unit_key(&PLUGIN_KEY).unwrap()]);
    let current = store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap();
    assert_ne!(current, observed.sequence);
    assert!(receive(&mut store, change("remote", 2)).plugin_storage_changed_keys.is_empty());
    assert!(receive(&mut store, change("remote", 3)).plugin_storage_changed_keys.is_empty());
    assert_eq!(current, store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap());
    let hydrated = store.device_store().unwrap().hydrate_plugin_device_storage(PLUGIN_KEY[1]).unwrap();
    assert_eq!(hydrated.entries.len(), 1);
    assert_eq!(hydrated.entries[0].value, "remote");
    assert_eq!(hydrated.entries[0].sequence, current);
    for mutation in [Mutation::Set { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), value: "stale".into() },
        Mutation::Delete { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into() },
        Mutation::Clear { space: PLUGIN_KEY[2].into() }, Mutation::Staged { session_id: id }] {
        let failed = store.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[
            Mutation::Set { space: "string".into(), key: "batch-sibling".into(), value: "must roll back".into() }, mutation,
        ], &[observed.clone()], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).unwrap_err();
        assert!(failed.to_string().contains("PLUGIN_STORAGE_STALE"));
        assert_eq!(plugin_value(&store), Some("remote".into()));
        assert!(store.device_store().unwrap().read_plugin_device_value(PLUGIN_KEY[1], "string", "batch-sibling").unwrap().is_none());
        assert!(store.lww_read_outbox(store.lww_binding_authority().unwrap(), 100).unwrap().entries.is_empty());
    }
    assert_eq!(receive(&mut store, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(4), value: UnitValue::Deleted }).plugin_storage_changed_keys, vec![unit_key(&PLUGIN_KEY).unwrap()]);
    let mutation = Mutation::Set { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), value: "fresh".into() };
    assert!(store.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[mutation.clone()], &[observed], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).is_err());
    assert_eq!(plugin_value(&store), None);
    let fresh = PluginDeviceObservation { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(),
        sequence: store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap() };
    let committed = store.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[mutation], &[fresh.clone()], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).unwrap();
    assert_eq!(committed[0].sequence, fresh.sequence);
    assert_eq!(plugin_value(&store), Some("fresh".into()));
    let cleared = store.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1],
        &[Mutation::Clear { space: PLUGIN_KEY[2].into() }], &[], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).unwrap();
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].key, PLUGIN_KEY[3]);
    assert_eq!(cleared[0].sequence, fresh.sequence);
    assert_eq!(plugin_value(&store), None);
}

#[test]
fn plugin_local_observations_ignore_nonparticipating_receives_and_fence_a_reopened_connection() {
    use device_store::plugin_values::{PluginDeviceMutation as Mutation, PluginDeviceObservation, text_value};
    let (dir, mut store) = store();
    let sequence = store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap();
    assert!(receive(&mut store, Change { key: unit_key(&PLUGIN_KEY).unwrap(), stamp: stamp(1), value: inline(&serde_json::json!("remote")).unwrap() }).plugin_storage_changed_keys.is_empty());
    assert_eq!(sequence, store.device_store().unwrap().plugin_device_sequence(PLUGIN_KEY[1], PLUGIN_KEY[2], PLUGIN_KEY[3]).unwrap());
    let observed = PluginDeviceObservation { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), sequence };
    let mutation = Mutation::Set { space: PLUGIN_KEY[2].into(), key: PLUGIN_KEY[3].into(), value: "local".into() };
    store.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[mutation.clone()], &[observed.clone()], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).unwrap();
    assert_eq!(plugin_value(&store), Some("local".into()));
    drop(store);
    let mut reopened = PersistentStore::open(dir.path()).unwrap();
    assert!(reopened.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[mutation.clone()], &[observed], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).is_err());
    reopened.device_store_mut().unwrap().write_plugin_device_values_observed(PLUGIN_KEY[1], &[mutation], &[], text_value::PART_BYTES as u64, text_value::REQUEST_BYTES).unwrap();
}

#[test]
fn shared_plugin_change_keys_exclude_echoes_equal_values_and_losing_changes() {
    let (_dir, mut store) = store();
    let key = unit_key(&["plugin", "synthetic-owner", "settings"]).unwrap();
    let change = |value, time| Change { key: key.clone(), stamp: stamp(time), value };
    let value = inline(&serde_json::json!({ "count": 1 })).unwrap();
    assert_eq!(receive(&mut store, change(value.clone(), 2)).plugin_storage_changed_keys, vec![key.clone()]);
    assert!(receive(&mut store, change(value.clone(), 2)).plugin_storage_changed_keys.is_empty());
    assert!(receive(&mut store, change(value, 3)).plugin_storage_changed_keys.is_empty());
    assert!(receive(&mut store, change(inline(&serde_json::json!({ "count": 0 })).unwrap(), 1)).plugin_storage_changed_keys.is_empty());
    assert_eq!(receive(&mut store, change(UnitValue::Deleted, 4)).plugin_storage_changed_keys, vec![key.clone()]);
    assert!(receive(&mut store, change(UnitValue::Deleted, 5)).plugin_storage_changed_keys.is_empty());
}

#[test]
fn structured_plugin_values_carry_their_parts_as_dependencies() {
    use device_store::plugin_values::{structure, text_value, PluginDeviceMutation};
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let part = vec![0u8, 255, 128];
    let hash = structure::put_part(store.device_store().unwrap().connection(), &part).unwrap();
    let text = structured_text(&[hash.clone()], 3, 0);
    store.device_store_mut().unwrap().write_plugin_device_values(STRUCTURED_KEY[1], &[
        PluginDeviceMutation::Set { space: STRUCTURED_KEY[2].into(), key: STRUCTURED_KEY[3].into(), value: text.clone() },
    ]).unwrap();
    // Shorter than the inline bound, the value is still an object naming its part.
    let body = canonical(&Value::String(text.clone()));
    assert!(body.len() <= MAX_INLINE_UNIT_BYTES);
    let descriptor = RecordDescriptor { dependencies: vec![hash.clone()], ..RecordDescriptor::content(risunest_sync_wire::hash(&body)) };
    let entry = outbox_value(&store, &STRUCTURED_KEY);
    assert_eq!(entry.value, UnitValue::object(descriptor.clone()).unwrap());
    assert_eq!(store.lww_object_body(&descriptor.object_hash).unwrap(), Some(body.clone()));
    assert_eq!(store.lww_object_body(&hash).unwrap(), Some(part.clone()));

    let (_dir, mut target) = self::store();
    target.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let change = |value: UnitValue, time: u64| Change { key: unit_key(&STRUCTURED_KEY).unwrap(), stamp: stamp(time), value };
    published(&target, &body);
    for dependencies in [vec![], vec![risunest_sync_wire::hash(b"other")]] {
        let other = RecordDescriptor { dependencies, ..RecordDescriptor::content(descriptor.object_hash.clone()) };
        assert_eq!(stage_error(&mut target, change(UnitValue::object(other).unwrap(), 1)), "invalid-large-unit");
    }
    // The stored size counts the parts as well as the text.
    let count = text_value::MAX_VALUE_BYTES.div_ceil(structure::PART_BYTES) as usize;
    let oversized = canonical(&Value::String(structured_text(&vec![hash.clone(); count], text_value::MAX_VALUE_BYTES, 0)));
    published(&target, &oversized);
    let oversized = RecordDescriptor { dependencies: vec![hash.clone()], ..RecordDescriptor::content(risunest_sync_wire::hash(&oversized)) };
    assert_eq!(stage_error(&mut target, change(UnitValue::object(oversized).unwrap(), 1)), "plugin device value is too large");
    // A value whose part the device cannot find is left out, and the rest of its page applies.
    const PLAIN_KEY: [&str; 4] = ["plugin-local", "synthetic-owner", "json", "plain"];
    let header = Header { binding_authority: target.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() };
    target.lww_stage_receive(&StageReceive {
        header: header.clone(),
        changes: vec![change(entry.value.clone(), 1),
            Change { key: unit_key(&PLAIN_KEY).unwrap(), stamp: stamp(1), value: inline(&serde_json::json!("remote")).unwrap() }],
        progress: Progress { kind: "server".into(), cursor: 1.into(), writer_id: None },
        admitted_time_upper_ms: u64::MAX.into(),
    }).unwrap();
    target.lww_apply_receive(&ApplyReceive { header: header.clone(), generating: vec![] }).unwrap();
    target.lww_finish_receive(&header).unwrap();
    assert_eq!(plugin_value_at(&target, &STRUCTURED_KEY), None);
    assert_eq!(plugin_value_at(&target, &PLAIN_KEY).as_deref(), Some("remote"));
    target.lww_put_object(&hash, &part).unwrap();
    receive(&mut target, change(entry.value.clone(), 1));
    assert_eq!(plugin_value_at(&target, &STRUCTURED_KEY), Some(text));
    assert!(message_pages::verified_object_present(target.device_store().unwrap().connection(), &hash).unwrap());

    // A row never names a part the device lacks, whatever form the unit takes.
    let (_dir, mut bare) = self::store();
    bare.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let missing = structured_text(&[risunest_sync_wire::hash(b"absent")], 6, MAX_INLINE_UNIT_BYTES);
    let value = published(&bare, &canonical(&Value::String(missing)));
    let header = stage(&mut bare, change(value, 1)).unwrap();
    let failure = bare.lww_apply_receive(&ApplyReceive { header, generating: vec![] }).unwrap_err().to_string();
    assert!(failure.contains("part is missing"), "{failure}");
    assert_eq!(plugin_value_at(&bare, &STRUCTURED_KEY), None);
}

#[test]
fn structured_plugin_values_received_while_disabled_keep_their_parts_until_enabled() {
    let grace = super::super::ASSET_GC_PRODUCT_MINIMUM_GRACE_MS;
    let limit = super::super::MESSAGE_PAGE_SWEEP_LIMIT;
    let (_dir, mut store) = store();
    let part = vec![7u8; 5];
    let hash = risunest_sync_wire::hash(&part);
    store.lww_put_object(&hash, &part).unwrap();
    let text = structured_text(&[hash.clone()], 5, 0);
    let UnitValue::Object { descriptor, .. } = published(&store, &canonical(&Value::String(text.clone()))) else { unreachable!() };
    let value = UnitValue::object(RecordDescriptor { dependencies: vec![hash.clone()], ..descriptor }).unwrap();
    receive(&mut store, Change { key: unit_key(&STRUCTURED_KEY).unwrap(), stamp: stamp(1), value });
    assert_eq!(plugin_value_at(&store, &STRUCTURED_KEY), None);
    let now = 1_900_000_000_000i64;
    for step in 0..3 {
        store.sweep_message_page_objects(super::super::MessageObjectStore::Device, now + step * grace, limit).unwrap();
    }
    assert!(device_object(&store, "message_page_objects", &hash), "a part the device unit names was collected");
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    assert_eq!(plugin_value_at(&store, &STRUCTURED_KEY), Some(text));
}

#[test]
fn a_restored_structured_value_publishes_the_unit_its_writer_published() {
    use device_store::plugin_values::{structure, text_value, PluginDeviceMutation};
    const LARGE_KEY: [&str; 4] = ["plugin-local", "synthetic-owner", "json", "large-structured"];
    let (_dir, mut store) = store();
    store.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let part = vec![0u8, 255, 128];
    let hash = structure::put_part(store.device_store().unwrap().connection(), &part).unwrap();
    let device = store.device_store_mut().unwrap();
    device.write_plugin_device_values(STRUCTURED_KEY[1], &[PluginDeviceMutation::Set {
        space: STRUCTURED_KEY[2].into(), key: STRUCTURED_KEY[3].into(), value: structured_text(&[hash.clone()], 3, 0),
    }]).unwrap();
    // Past one section object, the backup keeps the text as objects.
    let large = structured_text(&[hash.clone()], 3, text_value::PART_BYTES);
    let opened = device.open_plugin_device_write(LARGE_KEY[1], LARGE_KEY[2], LARGE_KEY[3], text_value::encoded_length(&large)).unwrap();
    let id = serde_json::to_value(opened).unwrap()["sessionId"].as_str().unwrap().to_owned();
    let middle = large.len() / 2;
    for (offset, piece) in [(0, &large[..middle]), (middle, &large[middle..])] {
        device.append_plugin_device_write(LARGE_KEY[1], &id, offset as u64, piece).unwrap();
    }
    device.write_plugin_device_values(LARGE_KEY[1], &[PluginDeviceMutation::Staged { session_id: id }]).unwrap();
    let keys = [STRUCTURED_KEY, LARGE_KEY];
    let values = keys.map(|key| outbox_value(&store, &key).value);
    assert!(values.iter().all(|value| matches!(value, UnitValue::Object { descriptor, .. } if descriptor.dependencies == [hash.clone()])));

    let (lease, sections) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    let (_target_dir, mut target) = self::store();
    target.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    let staging = target.replace_begin().unwrap().staging_id;
    target.replace_put_root(&staging, &target.read_root(None).unwrap().value).unwrap();
    target.replace_put_presets(&staging, &[]).unwrap();
    let header = Header { binding_authority: target.lww_binding_authority().unwrap(), request_id: "structured-restore".into() };
    target.lww_commit_replacement_with_device_sections(&header, &staging, None, &sections.iter().collect::<Vec<_>>()).unwrap();
    for (key, value) in keys.iter().zip(&values) {
        assert_eq!(outbox_value(&target, key).value, *value);
    }
    assert_eq!(target.lww_object_body(&hash).unwrap(), Some(part.clone()));
    store.release_revision(&lease.lease).unwrap();

    // A peer applies the large value with its part, and never without it.
    let body = store.lww_object_body(object_hash(&values[1])).unwrap().unwrap();
    let change = |value: UnitValue| Change { key: unit_key(&LARGE_KEY).unwrap(), stamp: stamp(1), value };
    let (_dir, mut bare) = self::store();
    bare.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    bare.lww_put_object_reader(object_hash(&values[1]), body.as_slice(), body.len() as u64).unwrap();
    let content = UnitValue::object(RecordDescriptor::content(object_hash(&values[1]).to_owned())).unwrap();
    let header = stage(&mut bare, change(content)).unwrap();
    let failure = bare.lww_apply_receive(&ApplyReceive { header, generating: vec![] }).unwrap_err().to_string();
    assert!(failure.contains("part is missing"), "{failure}");
    let (_dir, mut peer) = self::store();
    peer.device_store_mut().unwrap().set_section_participating(device_store::Section::LocalPlugins, true).unwrap();
    peer.lww_put_object_reader(object_hash(&values[1]), body.as_slice(), body.len() as u64).unwrap();
    peer.lww_put_object(&hash, &part).unwrap();
    receive(&mut peer, change(values[1].clone()));
    assert_eq!(plugin_value_at(&peer, &LARGE_KEY), Some(large));
}
