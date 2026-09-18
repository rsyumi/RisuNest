use super::*;
use crate::persistent_store::device_store::plugin_values::PluginDeviceMutation;
use crate::persistent_store::server_sync_sections as server;
use risunest_sync_wire::Domain;

fn plugin(clock: &str, writer: &str) -> SectionRow {
    SectionRow {
        key1: "plugin".into(), key2: "string".into(), key3: "key".into(),
        value: SectionValueRow::Plugin { space: "string".into(), value: "value".into() },
        write_clock: Sequence::try_from(clock.to_owned()).unwrap(), writer_id: writer.into(),
    }
}

fn local_entry(row: &SectionRow) -> LocalSectionEntry {
    LocalSectionEntry {
        version: row.version(), entry: row.to_entry(SectionKind::LocalPlugins, true).unwrap(),
        object: None, published: true,
    }
}

fn assert_both(local: &SectionRow, incoming: &SectionRow, decision: SectionMergeDecision, outcome: server::Outcome) {
    assert_eq!(resolve_section_row(Some(local), incoming).unwrap(), decision);
    assert_eq!(server::resolve(Some(&local_entry(local)), &local_entry(incoming).entry).unwrap(), outcome);
}

#[test]
fn e2_both_adapters_compare_large_clocks_and_writer_ties_exactly() {
    let local = plugin("9007199254740992", "writer-z");
    let newer = plugin("9007199254740993", "writer-a");
    assert_both(&local, &newer, SectionMergeDecision::ApplyIncoming, server::Outcome::Apply);
    assert_both(&newer, &local, SectionMergeDecision::PublishLocal, server::Outcome::Publish);
    let same_clock = plugin("9007199254740993", "writer-b");
    assert_both(&newer, &same_clock, SectionMergeDecision::ApplyIncoming, server::Outcome::Apply);
    assert_both(&same_clock, &same_clock, SectionMergeDecision::Settled, server::Outcome::Settled);
}

#[test]
fn e2_a_newer_version_never_excuses_a_different_key_or_kind() {
    let local = plugin("1", "writer");
    let mut incoming = plugin("2", "writer");
    incoming.key3 = "other".into();
    assert!(resolve_section_row(Some(&local), &incoming).is_err());
    assert!(server::resolve(Some(&local_entry(&local)), &local_entry(&incoming).entry).is_err());
    let mut incoming = local_entry(&local).entry;
    incoming.kind = SectionKind::Hypa;
    assert!(server::resolve(None, &incoming).is_err());
    incoming = local_entry(&local).entry;
    incoming.version = None;
    assert!(server::resolve(None, &incoming).is_err());
}

#[test]
fn e2_same_version_different_values_are_rejected_by_both_adapters() {
    let local = plugin("7", "writer");
    let mut incoming = local.clone();
    incoming.value = SectionValueRow::Plugin { space: "string".into(), value: "other".into() };
    assert!(resolve_section_row(Some(&local), &incoming).is_err());
    assert!(server::resolve(Some(&local_entry(&local)), &local_entry(&incoming).entry).is_err());
}

#[test]
fn e2_removal_markers_converge_for_both_adapters() {
    for (generation, at_ms) in [(3, 500), (7, 99), (7, 100), (9, 1)] {
        let mut local = plugin("42", "writer");
        local.value = SectionValueRow::Tombstone { first_published: Some(TombstonePublication {
            generation: Sequence::from(7u64), at_ms: 100,
        }) };
        let mut incoming = local.clone();
        incoming.value = SectionValueRow::Tombstone { first_published: Some(TombstonePublication {
            generation: Sequence::from(generation as u64), at_ms,
        }) };
        let expected = match (generation, at_ms).cmp(&(7, 100)) {
            std::cmp::Ordering::Less => server::Outcome::Apply,
            std::cmp::Ordering::Equal => server::Outcome::Settled,
            std::cmp::Ordering::Greater => server::Outcome::Publish,
        };
        assert_eq!(server::resolve(Some(&local_entry(&local)), &local_entry(&incoming).entry).unwrap(), expected);
        let directory = tempfile::tempdir().unwrap();
        let mut device = DeviceStore::open(directory.path()).unwrap();
        device.apply_section_rows(Section::LocalPlugins, &[local.clone()]).unwrap();
        device.apply_section_rows(Section::LocalPlugins, &[incoming.clone()]).unwrap();
        let held = device.read_section_rows(Section::LocalPlugins).unwrap().remove(0);
        let expected_marker = if (generation, at_ms) < (7, 100) { incoming.value.first_published() } else { local.value.first_published() };
        assert_eq!(held.value.first_published(), expected_marker);
    }
    let mut bare = plugin("42", "writer");
    bare.value = SectionValueRow::Tombstone { first_published: None };
    let marker = TombstonePublication { generation: Sequence::from(7u64), at_ms: 100 };
    let mut marked = bare.clone();
    marked.value = SectionValueRow::Tombstone { first_published: Some(marker.clone()) };
    assert_eq!(resolve_section_row(Some(&bare), &marked).unwrap(), SectionMergeDecision::MergeRemovalMarker {
        marker: Some(marker.clone()), write_local: true, publish: false,
    });
    assert_eq!(resolve_section_row(Some(&marked), &bare).unwrap(), SectionMergeDecision::MergeRemovalMarker {
        marker: Some(marker), write_local: false, publish: true,
    });
}

#[test]
fn e2_json_and_vector_representations_have_one_content_rule() {
    let mut local = plugin("5", "writer");
    local.key2 = "json".into();
    local.value = SectionValueRow::Plugin { space: "json".into(), value: "{\"b\":2, \"a\":1}".into() };
    let mut incoming = local.clone();
    incoming.value = SectionValueRow::Plugin { space: "json".into(), value: "{\"a\":1,\"b\":2}".into() };
    assert_both(&local, &incoming, SectionMergeDecision::Settled, server::Outcome::Settled);

    let row = SectionRow {
        key1: "a".repeat(64), key2: String::new(), key3: String::new(),
        value: SectionValueRow::Hypa {
            producer: "producer".into(), model: "model".into(), endpoint: None,
            preprocess_version: 1, dimensions: 4, vector: vec![7; 16], metadata: None,
        },
        write_clock: Sequence::from(1u64), writer_id: "writer".into(),
    };
    let entry = row.to_entry(SectionKind::Hypa, true).unwrap();
    let mut object_entry = entry.clone();
    let SectionValue::Hypa(value) = &mut object_entry.value else { unreachable!() };
    value.vector = InlineOrObject::Object(ObjectReference {
        content_sha256: risunest_external_storage_format::content_identity::hash(&[7; 16]), byte_length: 16,
    });
    assert_eq!(resolve_section_row(Some(&entry), &object_entry).unwrap(), SectionMergeDecision::Settled);
}

#[test]
fn e2_server_apply_rechecks_the_local_version_and_rolls_back_conflicting_batches() {
    let directory = tempfile::tempdir().unwrap();
    let mut device = DeviceStore::open(directory.path()).unwrap();
    let local = plugin("9", "writer");
    device.apply_section_rows(Section::LocalPlugins, &[local.clone()]).unwrap();
    server::write_sections(&mut device, &[server::SectionWrite::Apply {
        domain: Domain::LocalPlugins, entry: local_entry(&plugin("8", "writer")).entry, object: None,
    }]).unwrap();
    assert_eq!(device.read_section_rows(Section::LocalPlugins).unwrap(), vec![local.clone()]);
    assert!(!device.read_section_entry(Section::LocalPlugins, &local_entry(&local).entry.key).unwrap().unwrap().published);

    let mut conflict = local.clone();
    conflict.value = SectionValueRow::Plugin { space: "string".into(), value: "conflict".into() };
    let mut other = plugin("10", "writer");
    other.key3 = "another".into();
    let before = device.section_state(Section::LocalPlugins).unwrap();
    assert!(server::write_sections(&mut device, &[
        server::SectionWrite::Apply { domain: Domain::LocalPlugins, entry: local_entry(&other).entry, object: None },
        server::SectionWrite::Apply { domain: Domain::LocalPlugins, entry: local_entry(&conflict).entry, object: None },
    ]).is_err());
    assert_eq!(device.read_section_rows(Section::LocalPlugins).unwrap(), vec![local.clone()]);
    assert_eq!(device.section_state(Section::LocalPlugins).unwrap(), before);

    server::write_sections(&mut device, &[server::SectionWrite::MarkVersion {
        domain: Domain::LocalPlugins, key: local_entry(&local).entry.key,
        version: plugin("8", "writer").version(),
    }]).unwrap();
    assert!(!device.read_section_entry(Section::LocalPlugins, &local_entry(&local).entry.key).unwrap().unwrap().published);
}

#[test]
fn e3_spools_deduplicate_vectors_are_read_only_and_clean_up_after_use() {
    use risunest_external_storage_format::{content_identity::hash, format::FingerprintBuilder};
    let mut spool = SectionSpoolBuilder::new(Section::Hypa).unwrap();
    let mut fingerprint = FingerprintBuilder::new(&SectionKind::Hypa.fingerprint_domain());
    for key in ["a", "b"] {
        let row = SectionRow {
            key1: key.repeat(64), key2: String::new(), key3: String::new(),
            value: SectionValueRow::Hypa {
                producer: "producer".into(), model: "model".into(), endpoint: None,
                preprocess_version: 1, dimensions: 2048, vector: vec![7; 8192], metadata: None,
            },
            write_clock: Sequence::from(1u64), writer_id: "writer".into(),
        };
        let entry = row.to_entry(SectionKind::Hypa, true).unwrap();
        let digest = hash(&entry.encode().unwrap());
        fingerprint.push(&entry.key, &digest).unwrap();
        spool.push(row, &entry.key, &digest).unwrap();
    }
    let prepared = spool.finish(&fingerprint.finish()).unwrap();
    let objects: i64 = prepared.connection.query_row("SELECT count(*) FROM vector_values", [], |row| row.get(0)).unwrap();
    assert_eq!(objects, 1);
    assert!(prepared.connection.execute("DELETE FROM row_index", []).is_err());
    let mut count = 0;
    prepared.visit(|row| {
        let SectionValueRow::Hypa { vector, .. } = row.value else { panic!("expected a vector") };
        assert_eq!(vector, vec![7; 8192]);
        count += 1;
        Ok(())
    }).unwrap();
    assert_eq!(count, 2);
    let path = prepared._directory.path().to_path_buf();
    drop(prepared);
    assert!(!path.exists());
}

#[test]
fn e3_spool_key_pages_limit_bytes_as_well_as_rows() {
    use risunest_external_storage_format::{content_identity::hash, format::FingerprintBuilder};
    let mut spool = SectionSpoolBuilder::new(Section::LocalPlugins).unwrap();
    let mut fingerprint = FingerprintBuilder::new(&SectionKind::LocalPlugins.fingerprint_domain());
    for index in 0..100 {
        let mut row = plugin("1", "writer");
        row.key3 = format!("{index:04}-{}", "x".repeat(48 * 1024));
        let entry = row.to_entry(SectionKind::LocalPlugins, true).unwrap();
        let digest = hash(&entry.encode().unwrap());
        fingerprint.push(&entry.key, &digest).unwrap();
        spool.push(row, &entry.key, &digest).unwrap();
    }
    let prepared = spool.finish(&fingerprint.finish()).unwrap();
    let first = {
        let mut statement = prepared.connection.prepare("SELECT key1,key2,key3 FROM row_index ORDER BY key1,key2,key3").unwrap();
        let mut rows = statement.query([]).unwrap();
        read_key_page(&mut rows).unwrap()
    };
    assert!(!first.is_empty() && first.len() < 100);
    let bytes: usize = first.iter().map(|key| key.0.len() + key.1.len() + key.2.len() + std::mem::size_of::<SectionKey>()).sum();
    assert!(bytes <= SECTION_PAGE_BYTES);
    drop(first);
    let mut visited = 0;
    prepared.visit(|_| { visited += 1; Ok(()) }).unwrap();
    assert_eq!(visited, 100);
}

fn set_plugin(store: &mut DeviceStore, key: &str, value: &str) {
    store.write_plugin_device_values("plugin", &[PluginDeviceMutation::Set {
        space: "string".into(), key: key.into(), value: value.into(),
    }]).unwrap();
}

#[test]
fn e4_captures_selected_sections_from_one_read_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(directory.path()).unwrap();
    set_plugin(&mut store, "value", "before");
    store.write_setting("dosync", &serde_json::json!("before")).unwrap();
    let snapshot = Connection::open_with_flags(
        directory.path().join(super::super::DEVICE_DATABASE_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ).unwrap();
    snapshot.execute_batch("PRAGMA query_only=ON; BEGIN;").unwrap();
    let mut plugins = SectionSpoolBuilder::new_backup(SectionKind::LocalPlugins).unwrap();
    capture_backup_rows(&snapshot, SectionKind::LocalPlugins, &mut plugins).unwrap();
    set_plugin(&mut store, "value", "after");
    store.write_setting("dosync", &serde_json::json!("after")).unwrap();
    let mut settings = SectionSpoolBuilder::new_backup(SectionKind::LocalSettings).unwrap();
    capture_backup_rows(&snapshot, SectionKind::LocalSettings, &mut settings).unwrap();
    snapshot.execute_batch("COMMIT;").unwrap();
    let plugins = plugins.finish_captured().unwrap();
    let settings = settings.finish_captured().unwrap();
    let mut plugin_value = None;
    plugins.visit_entries(|entry, _| {
        let SectionValue::LocalPlugin(value) = &entry.value else { unreachable!() };
        plugin_value = value.value.as_str().map(str::to_owned);
        Ok(())
    }).unwrap();
    let mut setting_value = None;
    settings.visit_entries(|entry, _| {
        if let SectionValue::LocalSetting(value) = &entry.value {
            setting_value = value.value.as_str().map(str::to_owned);
        }
        Ok(())
    }).unwrap();
    assert_eq!(plugin_value.as_deref(), Some("before"));
    assert_eq!(setting_value.as_deref(), Some("before"));
}

#[test]
fn e4_backup_entries_are_visited_in_canonical_key_order() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = DeviceStore::open(directory.path()).unwrap();
    store.write_setting("dosync", &serde_json::json!(true)).unwrap();
    for code_hash in ["a".repeat(64), "f".repeat(64)] {
        store.write_plugin_permission(&code_hash, "network", true).unwrap();
    }
    let prepared = store.capture_backup_sections(&[SectionKind::LocalSettings])
        .unwrap().pop().unwrap();
    let mut keys = Vec::new();
    prepared.visit_entries(|entry, _| { keys.push(entry.key.clone()); Ok(()) }).unwrap();
    let mut sorted = keys.clone(); sorted.sort(); assert_eq!(keys, sorted);

    let mut plugin_spool = SectionSpoolBuilder::new_backup(SectionKind::LocalPlugins).unwrap();
    for (owner, key) in [
        ("owner-z".to_owned(), "short".to_owned()),
        ("소유자".to_owned(), "인용-\"-키".to_owned()),
        ("owner-a".to_owned(), "x".repeat(4096)),
    ] {
        let mut row = plugin("0", ""); row.key1 = owner; row.key3 = key;
        plugin_spool.push_backup_row(row).unwrap();
    }
    let prepared = plugin_spool.finish_captured().unwrap();
    let mut keys = Vec::new();
    prepared.visit_entries(|entry, _| { keys.push(entry.key.clone()); Ok(()) }).unwrap();
    let mut sorted = keys.clone(); sorted.sort(); assert_eq!(keys, sorted);
}

#[test]
fn e4_prepared_restore_is_bounded_empty_aware_and_idempotent() {
    let source_directory = tempfile::tempdir().unwrap();
    let mut source = DeviceStore::open(source_directory.path()).unwrap();
    set_plugin(&mut source, "alpha", "from-backup");
    let prepared = source.capture_backup_sections(&[SectionKind::LocalPlugins])
        .unwrap().pop().unwrap();
    assert_eq!(prepared.kind(), SectionKind::LocalPlugins);
    assert_eq!(prepared.len(), 1);
    assert!(!prepared.is_empty());
    let target_directory = tempfile::tempdir().unwrap();
    let mut target = DeviceStore::open(target_directory.path()).unwrap();
    set_plugin(&mut target, "dropped", "before");
    target.restore_prepared_backup_section(&prepared).unwrap();
    let first_clock = target.section_state(Section::LocalPlugins).unwrap().max_write_clock;
    let first_rows = target.read_section_rows(Section::LocalPlugins).unwrap();
    target.restore_prepared_backup_section(&prepared).unwrap();
    assert_eq!(target.section_state(Section::LocalPlugins).unwrap().max_write_clock, first_clock);
    assert_eq!(target.read_section_rows(Section::LocalPlugins).unwrap(), first_rows);
    let empty_directory = tempfile::tempdir().unwrap();
    let mut empty = DeviceStore::open(empty_directory.path()).unwrap();
    let empty = empty.capture_backup_sections(&[SectionKind::LocalPlugins])
        .unwrap().pop().unwrap();
    assert!(empty.is_empty());
    target.restore_prepared_backup_section(&empty).unwrap();
    assert!(target.read_backup_section_rows(Section::LocalPlugins).unwrap().is_empty());
}
