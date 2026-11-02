use super::body_encoding_tests::random_text;
use super::lww_tests::{drain_publications, header, insert_messages, local, publish_cycle, receive_available, save, LocalServerFixture};
use crate::persistent_store::PersistentStore;
use risunest_sync_wire::{descriptor::RecordDescriptor, unit::{UnitValue, MAX_INLINE_UNIT_BYTES}, MAX_METADATA_BYTES};
use serde_json::{json, Value};

const FIELDS: [&str; 5] = ["additionalPrompt", "NAIImgUrl", "autofillRequestUrl", "language", "ImagenModel"];

fn pending(store: &PersistentStore) -> usize {
    store.lww_read_outbox(store.lww_binding_authority().unwrap(), 256).unwrap().entries.len()
}

#[test]
fn large_root_field_converges_through_a_chunked_body_without_asset_custody() {
    let server = LocalServerFixture::new();
    let (_a, mut a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let large = Value::String("x".repeat(2 * MAX_METADATA_BYTES));
    save(&mut a, &["root", "additionalPrompt"], large.clone());
    let hash = risunest_sync_wire::hash(&risunest_sync_wire::payload_value::encode(&large).unwrap());
    let entry = a.lww_read_outbox(0.into(), 256).unwrap().entries.remove(0);
    assert_eq!(entry.value, UnitValue::object(RecordDescriptor::content(hash.clone())).unwrap());
    drain_publications(&ca, &mut a, &[]).unwrap();
    assert_eq!(pending(&a), 0);
    receive_available(&cb, &mut b, &[]).unwrap();
    assert_eq!(b.read_root(None).unwrap().value["additionalPrompt"], large);
    assert!(b.lww_verified_object_present(&hash).unwrap());
    assert!(crate::asset_repository::PayloadCas::new(b.repository_root()).unwrap().stat_object(&hash).unwrap().is_none());
    assert!(super::residency::Residency::open(b.repository_root()).unwrap().object(&hash, None).unwrap().is_none());
    assert_eq!(pending(&b), 0);
}

#[test]
fn pushes_split_outbox_pages_by_canonical_request_bytes() {
    let server = LocalServerFixture::new();
    let (_a, mut a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let values = FIELDS.iter().enumerate()
        .map(|(i, field)| (*field, Value::String(format!("{i}{}", "v".repeat(MAX_INLINE_UNIT_BYTES - 3)))))
        .collect::<Vec<_>>();
    for (field, value) in &values {
        save(&mut a, &["root", field], value.clone());
    }
    assert!(a.lww_read_outbox(0.into(), 256).unwrap().entries.iter().all(|entry| matches!(entry.value, UnitValue::Inline { .. })));
    assert!(publish_cycle(&ca, &mut a, &[]).unwrap().is_some());
    let remaining = pending(&a);
    assert!(remaining > 0 && remaining < FIELDS.len(), "{remaining}");
    drain_publications(&ca, &mut a, &[]).unwrap();
    assert_eq!(pending(&a), 0);
    receive_available(&cb, &mut b, &[]).unwrap();
    let root = b.read_root(None).unwrap().value;
    for (field, value) in &values {
        assert!(root[field] == *value, "{field}");
    }
}

#[test]
fn a_rejected_single_entry_push_reports_unit_too_large_and_keeps_the_entry() {
    let server = LocalServerFixture::with_router(|router| {
        router.layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                if request.uri().path() == "/push" {
                    // Finish the upload so an early disconnect cannot hide the 413 response.
                    axum::body::to_bytes(request.into_body(), MAX_METADATA_BYTES)
                        .await
                        .expect("synthetic push request body");
                    return axum::response::Response::builder()
                        .status(413)
                        .body(axum::body::Body::from("{\"error\":\"body-too-large\"}"))
                        .unwrap();
                }
                next.run(request).await
            },
        ))
    });
    server.switch_format();
    for value in ["synthetic".to_owned(), random_text(32 * 1024, 7)] {
        let (_a, mut a) = local();
        let ca = server.client(&a);
        save(&mut a, &["root", "language"], json!(value));
        let request = header(&a);
        let entry = a.lww_read_outbox(request.binding_authority, 1).unwrap().entries.remove(0);
        ca.admit_synthetic_clock(entry.stamp.physical_ms.0);
        let error = ca.push(&mut a, &request, &[]).unwrap_err();
        assert_eq!((error.code.as_str(), error.retryable), ("unit-too-large", false), "{error:?}");
        assert_eq!(pending(&a), 1);
    }
}

type Routes = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;

fn recording_server(fail_push: std::sync::Arc<std::sync::atomic::AtomicBool>) -> (LocalServerFixture, Routes) {
    let routes = Routes::default();
    let seen = routes.clone();
    let server = LocalServerFixture::with_router(move |router| {
        router.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let seen = seen.clone();
                let fail_push = fail_push.clone();
                async move {
                    let path = request.uri().path();
                    if path == "/push" && fail_push.load(std::sync::atomic::Ordering::Relaxed) {
                        // Finish the upload so an early disconnect cannot hide the 503 response.
                        axum::body::to_bytes(request.into_body(), usize::MAX)
                            .await
                            .expect("synthetic push request body");
                        return axum::response::Response::builder()
                            .status(503)
                            .body(axum::body::Body::from("{\"error\":\"unavailable\"}"))
                            .unwrap();
                    }
                    seen.lock().unwrap().push(super::body_encoding_tests::route(path));
                    next.run(request).await
                }
            },
        ))
    });
    (server, routes)
}

fn lane(store: &PersistentStore, name: &str) -> std::path::PathBuf {
    store.repository_root().join("server-sync/lww-cache").join(name)
}

fn raw_bytes(client: &super::lww_client::LwwClient) -> (u64, u64) {
    let counters = client.client.test_io.as_ref().unwrap();
    let load = |value: &std::sync::atomic::AtomicU64| value.load(std::sync::atomic::Ordering::Relaxed);
    (load(&counters.request_raw_bytes), load(&counters.response_raw_bytes))
}

/// Pushes a large root field from one store, receives it in another, then
/// edits it and returns the raw bytes and routes of the edit's round trip.
fn edit_round_trip(size: usize, expire_base: bool) -> ((u64, u64), Vec<&'static str>) {
    let (server, routes) = recording_server(Default::default());
    let (_a, mut a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let original = random_text(size, 1);
    save(&mut a, &["root", "customCSS"], json!(original));
    drain_publications(&ca, &mut a, &[]).unwrap();
    receive_available(&cb, &mut b, &[]).unwrap();
    let mut edited = original;
    edited.insert_str(size / 2, &random_text(64 * 1024, 2));
    save(&mut a, &["root", "customCSS"], json!(edited));
    routes.lock().unwrap().clear();
    for client in [&ca, &cb] {
        client.client.test_io.as_ref().unwrap().reset();
    }
    drain_publications(&ca, &mut a, &[]).unwrap();
    if expire_base {
        // The receiver comes back after the leases and upload sessions that kept
        // the replaced body ran out and maintenance collected it.
        let db = rusqlite::Connection::open(server.root().join("metadata.sqlite")).unwrap();
        db.execute_batch("DELETE FROM object_leases; UPDATE uploads SET expires=0;").unwrap();
        drop(db);
        assert!(server.server.maintain().unwrap().objects_removed > 0);
    }
    receive_available(&cb, &mut b, &[]).unwrap();
    let digest = |value: &Value| risunest_sync_wire::hash(value.to_string().as_bytes());
    assert!(digest(&b.read_root(None).unwrap().value["customCSS"]) == digest(&json!(edited)));
    for (store, name) in [(&a, "send"), (&b, "receive")] {
        assert!(!lane(store, name).exists(), "{name}");
    }
    let sent = raw_bytes(&ca).0;
    let received = raw_bytes(&cb).1;
    let routes = routes.lock().unwrap().clone();
    ((sent, received), routes)
}

#[test]
fn an_edited_large_unit_moves_as_a_delta_in_both_directions() {
    const MIB: u64 = 1024 * 1024;
    // Below the inline delta limit, as one frame each way.
    let ((sent, received), routes) = edit_round_trip(6 * MIB as usize, false);
    eprintln!("6 MiB edit: sent {sent} received {received} routes {routes:?}");
    assert!(sent < MIB && received < MIB, "{sent} {received}");
    assert!(routes.contains(&"frames") && routes.contains(&"transfer"), "{routes:?}");
    assert!(!routes.contains(&"chunk") && !routes.contains(&"part"), "{routes:?}");
    // Above it, through the streamed delta jobs.
    let ((sent, received), routes) = edit_round_trip(17 * MIB as usize, false);
    eprintln!("17 MiB edit: sent {sent} received {received} routes {routes:?}");
    assert!(sent < MIB && received < MIB, "{sent} {received}");
    assert!(routes.contains(&"upload-delta") && routes.contains(&"object-delta"), "{routes:?}");
    assert!(!routes.contains(&"chunk") && !routes.contains(&"part"), "{routes:?}");
}

#[test]
fn a_receiver_whose_base_the_server_collected_downloads_the_whole_body() {
    const MIB: u64 = 1024 * 1024;
    let ((sent, received), routes) = edit_round_trip(6 * MIB as usize, true);
    assert!(sent < MIB, "{sent}");
    assert!(received > 6 * MIB, "{received}");
    assert!(routes.contains(&"part"), "{routes:?}");
}

#[test]
fn a_failed_push_keeps_its_copies_for_the_retry_and_a_finished_one_removes_them() {
    let fail_push = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (server, _) = recording_server(fail_push.clone());
    server.switch_format();
    let (_a, mut a) = local();
    let ca = server.client(&a);
    let value = Value::String(random_text(2 * MAX_METADATA_BYTES, 3));
    save(&mut a, &["root", "customCSS"], value.clone());
    let hash = risunest_sync_wire::hash(&risunest_sync_wire::payload_value::encode(&value).unwrap());
    let request = header(&a);
    let error = ca.push(&mut a, &request, &[]).unwrap_err();
    // The server never recorded the push, so it is cancelled and fails as its reply did.
    assert_eq!((error.code.as_str(), error.status, error.retryable), ("unavailable", 503, true));
    let copies = crate::asset_repository::PayloadCas::new(&lane(&a, "send")).unwrap();
    assert!(copies.stat_object(&hash).unwrap().is_some());
    fail_push.store(false, std::sync::atomic::Ordering::Relaxed);
    drain_publications(&ca, &mut a, &[]).unwrap();
    assert_eq!(pending(&a), 0);
    assert!(!lane(&a, "send").exists());
}

/// One message whose stored body is larger than a metadata body, so the
/// message page that holds it alone is too.
fn oversized_message(seed: usize) -> Value {
    json!({"role":"user","data":random_text(MAX_METADATA_BYTES + 64 * 1024, seed)})
}

fn received_data(store: &PersistentStore) -> Vec<Value> {
    let chat = store.read_conversation("char", "chat", None).unwrap().unwrap().value;
    chat["message"].as_array().unwrap().iter().map(|message| message["data"].clone()).collect()
}

#[test]
fn a_bound_store_receives_message_pages_larger_than_a_metadata_body_on_every_receive() {
    let server = LocalServerFixture::new();
    let (_a, mut a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let first = oversized_message(5);
    insert_messages(&mut a, 0, vec![first.clone()]);
    let key = risunest_sync_wire::unit::UnitKey::new(&["messages", "char", "chat"]).unwrap();
    let entry = a.lww_read_outbox(0.into(), 256).unwrap().entries.into_iter().find(|entry| entry.key == key).unwrap();
    let UnitValue::Object { descriptor, .. } = &entry.value else { panic!("messages are an object unit") };
    assert_eq!(descriptor.dependencies.len(), 1);
    assert!(a.lww_object_body(&descriptor.dependencies[0]).unwrap().unwrap().len() > MAX_METADATA_BYTES);
    drain_publications(&ca, &mut a, &[]).unwrap();
    receive_available(&cb, &mut b, &[]).unwrap();
    assert!(received_data(&b) == [first["data"].clone()]);
    let second = oversized_message(6);
    insert_messages(&mut a, 1, vec![second.clone()]);
    drain_publications(&ca, &mut a, &[]).unwrap();
    receive_available(&cb, &mut b, &[]).unwrap();
    assert!(received_data(&b) == [first["data"].clone(), second["data"].clone()]);
    assert!(!lane(&b, "receive").exists());
}

#[test]
fn messages_and_archive_bodies_larger_than_a_metadata_body_are_prepared_whole() {
    let server = LocalServerFixture::new();
    let (_a, a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let staging = tempfile::tempdir().unwrap();
    let cache = super::cache::Cache::open(staging.path()).unwrap();
    let writer_id = b.lww_clock_state().unwrap().writer_id;
    let mut changes = Vec::new();
    let mut hashes = Vec::new();
    for (seed, parts) in [(7, vec!["archive", "char"]), (8, vec!["messages", "char", "chat"])] {
        let body = random_text(MAX_METADATA_BYTES + 1, seed).into_bytes();
        let hash = cache.put(&body).unwrap();
        changes.push(crate::persistent_store::lww::Change {
            key: risunest_sync_wire::unit::UnitKey::new(&parts).unwrap(),
            stamp: risunest_sync_wire::stamp::Stamp { physical_ms: 1.into(), logical: 0, writer_id: writer_id.clone() },
            value: UnitValue::object(RecordDescriptor::content(hash.clone())).unwrap(),
        });
        hashes.push(hash);
    }
    super::transfer::Transfer::new(&ca.client, &cache).unwrap().upload(&hashes, &[]).unwrap();
    cb.prepare_bodies(&mut b, &changes, 1.into()).unwrap();
    for hash in &hashes {
        assert!(b.lww_verified_object_present(hash).unwrap());
        assert!(b.lww_object_body(hash).unwrap().unwrap().len() > MAX_METADATA_BYTES);
    }
}

#[test]
fn plugin_value_parts_are_prepared_into_the_object_store() {
    let server = LocalServerFixture::new();
    let (_a, a) = local();
    let (_b, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let staging = tempfile::tempdir().unwrap();
    let cache = super::cache::Cache::open(staging.path()).unwrap();
    let part_hash = cache.put(&[0u8, 255, 128]).unwrap();
    let text = format!(
        "#risunest-structured:1\n{{\"parts\":[\"p:{part_hash}\"],\"bytes\":3,\"nodes\":[{{\"type\":\"ArrayBuffer\",\"offset\":0,\"byteLength\":3}}],\"root\":{{\"ref\":0}}}}");
    let body_hash = cache.put(&serde_json::to_vec(&text).unwrap()).unwrap();
    let hashes = [body_hash.clone(), part_hash.clone()];
    super::transfer::Transfer::new(&ca.client, &cache).unwrap().upload(&hashes, &[]).unwrap();
    let change = crate::persistent_store::lww::Change {
        key: risunest_sync_wire::unit::UnitKey::new(&["plugin-local", "synthetic-owner", "json", "structured"]).unwrap(),
        stamp: risunest_sync_wire::stamp::Stamp { physical_ms: 1.into(), logical: 0, writer_id: b.lww_clock_state().unwrap().writer_id },
        value: UnitValue::object(RecordDescriptor { dependencies: vec![part_hash], ..RecordDescriptor::content(body_hash) }).unwrap(),
    };
    cb.prepare_bodies(&mut b, &[change], 1.into()).unwrap();
    for hash in &hashes {
        assert!(b.lww_verified_object_present(hash).unwrap());
        assert!(super::residency::Residency::open(b.repository_root()).unwrap().object(hash, None).unwrap().is_none());
    }
}

#[test]
fn a_structured_plugin_value_moves_with_its_parts_through_the_server() {
    use crate::persistent_store::device_store::{plugin_values::{structure, PluginDeviceMutation}, Section};
    let server = LocalServerFixture::new();
    let (_a, mut a) = local();
    let (_b, mut b) = local();
    for store in [&mut a, &mut b] {
        store.device_store_mut().unwrap().set_section_participating(Section::LocalPlugins, true).unwrap();
    }
    let ca = server.client(&a);
    let cb = server.client(&b);
    let full = structure::PART_BYTES as usize;
    // A small value, a value of one full part, and a value whose last part is a remainder.
    let values = [
        ("small", vec![vec![0u8, 255, 128]]),
        ("full", vec![vec![1u8; full]]),
        ("remainder", vec![vec![2u8; full], vec![3u8; 5]]),
    ];
    let mut texts = Vec::new();
    for (key, parts) in &values {
        let hashes = parts.iter()
            .map(|part| structure::put_part(a.device_store().unwrap().connection(), part).unwrap())
            .collect::<Vec<_>>();
        let length = parts.iter().map(Vec::len).sum::<usize>();
        let text = format!(
            "#risunest-structured:1
{{\"parts\":[{}],\"bytes\":{length},\"nodes\":[{{\"type\":\"ArrayBuffer\",\"offset\":0,\"byteLength\":{length}}}],\"root\":{{\"ref\":0}}}}",
            hashes.iter().map(|hash| format!("\"p:{hash}\"")).collect::<Vec<_>>().join(","));
        a.device_store_mut().unwrap().write_plugin_device_values("synthetic-owner", &[
            PluginDeviceMutation::Set { space: "json".into(), key: (*key).into(), value: text.clone() },
        ]).unwrap();
        texts.push(text);
    }
    drain_publications(&ca, &mut a, &[]).unwrap();
    assert_eq!((pending(&a), server.server.library_format()), (0, 2));
    receive_available(&cb, &mut b, &[]).unwrap();
    for ((key, parts), text) in values.iter().zip(texts) {
        assert_eq!(b.device_store().unwrap().read_plugin_device_value("synthetic-owner", "json", key).unwrap(), Some(text));
        for part in parts {
            assert_eq!(b.lww_object_body(&risunest_sync_wire::hash(part)).unwrap().as_ref(), Some(part));
        }
    }
}
