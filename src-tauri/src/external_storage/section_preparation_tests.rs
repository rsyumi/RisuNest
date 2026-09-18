use super::*;
use crate::persistent_store::{PersistentStore, device_store::plugin_values::PluginDeviceMutation};

fn row(key: &str, value: &str, clock: u64) -> SectionRow {
    SectionRow {
        key1: "plugin".into(), key2: "string".into(), key3: key.into(),
        value: SectionValueRow::Plugin { space: "string".into(), value: value.into() },
        write_clock: Sequence::from(clock), writer_id: "remote-writer".into(),
    }
}

fn store() -> (tempfile::TempDir, PersistentStore) {
    let root = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    store.device_store_mut().unwrap().set_section_participating(Section::LocalPlugins, true).unwrap();
    (root, store)
}

fn capture(rows: &[SectionRow], directory: &Path, generation: u64, floor: u64, maximum: u64) -> CapturedSection {
    capture_section(SectionKind::LocalPlugins, rows, true, Sequence::from(generation),
        Sequence::from(floor), Sequence::from(maximum), directory, &Cancellation::default()).unwrap()
}

fn prepare(store: &mut PersistentStore, source: &CapturedSection, arrival: SectionArrival) -> PreparedSectionInput {
    let token = store.device_store_mut().unwrap().section_state(section_of(source.kind).unwrap()).unwrap().participation_generation;
    prepare_received_section("connection", "lineage", arrival, &token, source, &Cancellation::default()).unwrap()
}

fn cursor(store: &mut PersistentStore, generation: u64, observed: u64) {
    store.device_store_mut().unwrap().write_section_cursor("connection", "lineage", Section::LocalPlugins, &SectionCursor {
        applied_generation: Sequence::from(generation), applied_gc_floor: Sequence::from(0u64),
        observed_max_write_clock: Sequence::from(observed),
    }).unwrap();
}

#[test]
fn e3_preparation_is_read_only_and_apply_never_needs_the_source_files() {
    for kind in [SectionKind::Hypa, SectionKind::LocalPlugins] {
        let (_root, mut store) = store();
        let files = tempfile::tempdir().unwrap();
        let expected = if kind == SectionKind::Hypa {
            SectionRow {
                key1: "a".repeat(64), key2: String::new(), key3: String::new(),
                value: SectionValueRow::Hypa { producer: "producer".into(), model: "model".into(),
                    endpoint: None, preprocess_version: 1, dimensions: 2048,
                    vector: vec![7; 8192], metadata: None },
                write_clock: Sequence::from(20u64), writer_id: "remote-writer".into(),
            }
        } else { row("key", "value", 20) };
        let source = capture_section(kind, &[expected.clone()], true, Sequence::from(5u64),
            Sequence::from(0u64), Sequence::from(20u64), files.path(), &Cancellation::default()).unwrap();
        let section = section_of(kind).unwrap();
        let before = store.device_store_mut().unwrap().section_state(section).unwrap();
        let revision = store.device_store_mut().unwrap().revision().unwrap();
        let prepared = prepare(&mut store, &source, SectionArrival::Continuing);
        assert_eq!(store.device_store_mut().unwrap().section_state(section).unwrap(), before);
        assert!(store.device_store_mut().unwrap().read_section_rows(section).unwrap().is_empty());
        assert!(store.device_store_mut().unwrap().read_section_cursor("connection", "lineage", section).unwrap().is_none());
        drop(files);
        assert!(source.sources.iter().all(|source| !source.path.exists()));
        apply_prepared_section(&mut store, &prepared).unwrap();
        assert_eq!(store.device_store_mut().unwrap().read_section_rows(section).unwrap(), vec![expected.clone()]);
        let state = store.device_store_mut().unwrap().section_state(section).unwrap();
        apply_prepared_section(&mut store, &prepared).unwrap();
        assert_eq!(store.device_store_mut().unwrap().section_state(section).unwrap(), state);
        assert_eq!(store.device_store_mut().unwrap().revision().unwrap(), revision);
        assert_eq!(store.device_store_mut().unwrap().read_section_rows(section).unwrap(), vec![expected]);
        let other = if section == Section::Hypa { Section::LocalPlugins } else { Section::Hypa };
        assert!(store.device_store_mut().unwrap().read_section_cursor("connection", "lineage", other).unwrap().is_none());
        assert!(store.device_store_mut().unwrap().read_section_cursor("connection", "other-lineage", section).unwrap().is_none());
        assert!(store.device_store_mut().unwrap().read_section_cursor("other-connection", "lineage", section).unwrap().is_none());
    }
}

#[test]
fn e3_a_participation_change_rejects_prepared_rows_even_after_turning_back_on() {
    for turn_back_on in [false, true] {
        let (_root, mut store) = store();
        let files = tempfile::tempdir().unwrap();
        let source = capture(&[row("key", "value", 10)], files.path(), 2, 0, 10);
        cursor(&mut store, 1, 1);
        let prepared = prepare(&mut store, &source, SectionArrival::Continuing);
        let device = store.device_store_mut().unwrap();
        let previous_cursor = device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap();
        device.set_section_participating(Section::LocalPlugins, false).unwrap();
        assert_eq!(device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap(), previous_cursor);
        if turn_back_on {
            device.set_section_participating(Section::LocalPlugins, true).unwrap();
            assert!(!device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap().unwrap().joined());
        }
        let state = device.section_state(Section::LocalPlugins).unwrap();
        let before_cursor = device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap();
        assert!(apply_prepared_section(&mut store, &prepared).is_err());
        let device = store.device_store_mut().unwrap();
        assert_eq!(device.section_state(Section::LocalPlugins).unwrap(), state);
        assert_eq!(device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap(), before_cursor);
        assert!(device.read_section_rows(Section::LocalPlugins).unwrap().is_empty());
    }
}

#[test]
fn e3_invalid_metadata_keys_fingerprints_and_versions_never_reach_apply() {
    let (_root, mut store) = store();
    let files = tempfile::tempdir().unwrap();
    let source = capture(&[row("key", "value", 10)], files.path(), 2, 0, 10);
    let before = store.device_store_mut().unwrap().section_state(Section::LocalPlugins).unwrap();
    for case in 0..7 {
        let mut bad = source.clone();
        match case {
            0 => bad.generation = Sequence::from(0u64),
            1 => bad.gc_floor = Sequence::from(3u64),
            2 => bad.max_write_clock = Sequence::from(9u64),
            3 => bad.content_fingerprint[0] ^= 1,
            4 => bad.sources.push(bad.sources[0].clone()),
            5 => bad.sources[0].key = "wrong-key".into(),
            _ => bad.kind = SectionKind::LocalSettings,
        }
        assert!(prepare_received_section("connection", "lineage", SectionArrival::Continuing,
            &before.participation_generation, &bad, &Cancellation::default()).is_err(), "case {case}");
    }
    let unversioned = capture_section(SectionKind::LocalPlugins, &[row("key", "value", 10)], false,
        Sequence::from(2u64), Sequence::from(0u64), Sequence::from(10u64),
        &files.path().join("unversioned"), &Cancellation::default()).unwrap();
    assert!(prepare_received_section("connection", "lineage", SectionArrival::Continuing,
        &before.participation_generation, &unversioned, &Cancellation::default()).is_err());
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(prepare_received_section("connection", "lineage", SectionArrival::Continuing,
        &before.participation_generation, &source, &cancel).is_err());
    assert_eq!(store.device_store_mut().unwrap().section_state(Section::LocalPlugins).unwrap(), before);
    assert!(store.device_store_mut().unwrap().read_section_rows(Section::LocalPlugins).unwrap().is_empty());
}

#[test]
fn e3_a_late_conflicting_row_rolls_back_values_clocks_and_the_cursor() {
    let (_root, mut store) = store();
    let files = tempfile::tempdir().unwrap();
    let held = row("z", "held", 10);
    store.device_store_mut().unwrap().apply_section_rows(Section::LocalPlugins, &[held.clone()]).unwrap();
    cursor(&mut store, 1, 10);
    let source = capture(&[row("a", "first", 11), row("z", "different", 10)], files.path(), 2, 0, 11);
    let prepared = prepare(&mut store, &source, SectionArrival::Continuing);
    let device = store.device_store_mut().unwrap();
    let state = device.section_state(Section::LocalPlugins).unwrap();
    let before_cursor = device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap();
    for _ in 0..2 {
        assert!(apply_prepared_section(&mut store, &prepared).is_err());
        let device = store.device_store_mut().unwrap();
        assert_eq!(device.read_section_rows(Section::LocalPlugins).unwrap(), vec![held.clone()]);
        assert_eq!(device.section_state(Section::LocalPlugins).unwrap(), state);
        assert_eq!(device.read_section_cursor("connection", "lineage", Section::LocalPlugins).unwrap(), before_cursor);
    }
}

#[test]
fn e3_rejoin_keeps_unpublished_edits_without_reviving_floor_reclaimed_values() {
    let (_root, mut store) = store();
    let files = tempfile::tempdir().unwrap();
    let device = store.device_store_mut().unwrap();
    device.write_plugin_device_values("plugin", &[PluginDeviceMutation::Set {
        space: "string".into(), key: "offline".into(), value: "mine".into(),
    }]).unwrap();
    let mut removed = row("removed", "", 3);
    removed.value = SectionValueRow::Tombstone { first_published: Some(TombstonePublication {
        generation: Sequence::from(1u64), at_ms: 1,
    }) };
    device.apply_section_rows(Section::LocalPlugins, &[row("stale", "old", 2), removed]).unwrap();
    cursor(&mut store, 1, 3);
    let source = capture(&[row("offline", "theirs", 100), row("remote-only", "remote", 100)], files.path(), 5, 3, 100);
    let prepared = prepare(&mut store, &source, SectionArrival::Continuing);
    apply_prepared_section(&mut store, &prepared).unwrap();
    let device = store.device_store_mut().unwrap();
    let rows = device.read_section_rows(Section::LocalPlugins).unwrap();
    assert_eq!(rows.len(), 2);
    let local = rows.iter().find(|row| row.key3 == "offline").unwrap();
    assert_eq!(local.value, SectionValueRow::Plugin { space: "string".into(), value: "mine".into() });
    assert!(local.write_clock > Sequence::from(100u64));
    assert!(device.sections_await_publication("connection", "lineage").unwrap());
    let state = device.section_state(Section::LocalPlugins).unwrap();
    let revision = device.revision().unwrap();
    apply_prepared_section(&mut store, &prepared).unwrap();
    let device = store.device_store_mut().unwrap();
    assert_eq!(device.read_section_rows(Section::LocalPlugins).unwrap(), rows);
    assert_eq!(device.section_state(Section::LocalPlugins).unwrap(), state);
    assert_eq!(device.revision().unwrap(), revision);
}

#[test]
fn e3_missing_and_corrupt_large_objects_fail_before_device_mutation() {
    let (_root, mut store) = store();
    let files = tempfile::tempdir().unwrap();
    let large = SectionRow {
        key1: "a".repeat(64), key2: String::new(), key3: String::new(),
        value: SectionValueRow::Hypa { producer: "producer".into(), model: "model".into(),
            endpoint: None, preprocess_version: 1, dimensions: 2048, vector: vec![7; 8192], metadata: None },
        write_clock: Sequence::from(2u64), writer_id: "remote".into(),
    };
    let source = capture_section(SectionKind::Hypa, &[large], true, Sequence::from(2u64),
        Sequence::from(0u64), Sequence::from(2u64), files.path(), &Cancellation::default()).unwrap();
    let state = store.device_store_mut().unwrap().section_state(Section::Hypa).unwrap();
    let mut missing = source.clone();
    missing.sources.retain(|file| file.kind == wire::CatalogEntryKind::SectionEntry);
    assert!(prepare_received_section("connection", "lineage", SectionArrival::Continuing,
        &state.participation_generation, &missing, &Cancellation::default()).is_err());
    let object = source.sources.iter().find(|file| file.kind == wire::CatalogEntryKind::SectionObject).unwrap();
    fs::write(&object.path, vec![8; 8192]).unwrap();
    assert!(prepare_received_section("connection", "lineage", SectionArrival::Continuing,
        &state.participation_generation, &source, &Cancellation::default()).is_err());
    assert_eq!(store.device_store_mut().unwrap().section_state(Section::Hypa).unwrap(), state);
    assert!(store.device_store_mut().unwrap().read_section_rows(Section::Hypa).unwrap().is_empty());
}

#[test]
fn e3_empty_sections_and_multiple_key_pages_are_applied_exactly() {
    let (_root, mut store) = store();
    let files = tempfile::tempdir().unwrap();
    let empty = capture(&[], &files.path().join("empty"), 1, 0, 40);
    let prepared = prepare(&mut store, &empty, SectionArrival::Continuing);
    apply_prepared_section(&mut store, &prepared).unwrap();
    assert_eq!(store.device_store_mut().unwrap().section_state(Section::LocalPlugins).unwrap().max_write_clock, Sequence::from(40u64));
    let rows: Vec<_> = (0..513).map(|index| row(&format!("key-{index:04}"), "value", 50)).collect();
    let source = capture(&rows, &files.path().join("pages"), 2, 0, 50);
    let prepared = prepare(&mut store, &source, SectionArrival::Continuing);
    apply_prepared_section(&mut store, &prepared).unwrap();
    assert_eq!(store.device_store_mut().unwrap().read_section_rows(Section::LocalPlugins).unwrap(), rows);
}
