use super::super::super::device_store::{
    hypa::HypaEmbeddingWrite,
    plugin_values::PluginDeviceMutation,
    sections::{SectionRow, SectionValueRow, TombstonePublication},
    Section,
};
use super::super::super::server_sync_sections as sections;
use super::*;
use risunest_external_storage_format::section::{
    local_plugin_entry_key, LocalPluginValue, PluginSpace, SectionEntry, SectionEntryVersion,
    SectionKind, SectionValue,
};
use risunest_sync_wire::Sequence;
use crate::persistent_store::StoreResult;

fn cache_key(seed: u8) -> String {
    hex::encode(Sha256::digest([seed]))
}

fn embedding(seed: u8, dimensions: usize, fill: u8) -> HypaEmbeddingWrite {
    HypaEmbeddingWrite {
        cache_key: cache_key(seed),
        producer: "hypa-v2".into(),
        model: "synthetic-embedding".into(),
        endpoint: None,
        preprocess_version: 1,
        dimensions: dimensions as i64,
        vector: vec![fill; dimensions * 4],
        metadata: None,
    }
}

fn read_vector(store: &PersistentStore, seed: u8) -> Option<Vec<u8>> {
    store
        .device_store()
        .unwrap()
        .read_hypa_embeddings(&[cache_key(seed)])
        .unwrap()
        .pop()
        .unwrap()
        .vector
}

fn section_clock(store: &PersistentStore, section: Section) -> Sequence {
    let value: String = store
        .device_store()
        .unwrap()
        .connection()
        .query_row(
            "SELECT max_write_clock FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    Sequence::try_from(value).unwrap()
}

struct Fleet {
    server: Arc<Store>,
    endpoint: String,
    paths: Arc<std::sync::Mutex<Vec<String>>>,
    _runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn fleet() -> Fleet {
    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(Store::init(dir.path()).unwrap());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let serving = server.clone();
    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = paths.clone();
    let task = runtime.spawn(async move {
        let router = http::router(serving).layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let recorded = recorded.clone();
                let path = request.uri().path().to_owned();
                async move {
                    recorded.lock().unwrap().push(path);
                    next.run(request).await
                }
            },
        ));
        axum::serve(listener, router).await.unwrap();
    });
    Fleet {
        server,
        endpoint,
        paths,
        _runtime: runtime,
        task,
        _dir: dir,
    }
}

impl Fleet {
    fn requests(&self) -> usize {
        self.paths.lock().unwrap().len()
    }
    fn requests_since(&self, mark: usize) -> Vec<String> {
        self.paths.lock().unwrap()[mark..].to_vec()
    }
    fn bind(&self, store: &mut PersistentStore) {
        let device = self.server.add_device().unwrap();
        store
            .server_bind(&ServerConfig {
                directory: None,
                endpoint: self.endpoint.clone(),
                library_id: device.library_id,
                device_id: device.device_id,
                token: device.token,
            })
            .unwrap();
    }
}

/// Invariant 28. A device that observed a remote counter issues a higher one for
/// its own edit, and the same version can never stand for two values.
#[test]
fn a_write_after_observing_a_remote_section_clock_outranks_it_and_a_reused_version_is_rejected() {
    let fleet = fleet();
    let (_first_dir, mut first) = prepared();
    let (_second_dir, mut second) = prepared();
    fleet.bind(&mut first);
    fleet.bind(&mut second);
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");

    // A small vector rides inside the entry; a large one becomes its own object.
    first
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(1, 4, 0x11), embedding(2, 1536, 0x22)])
        .unwrap();
    let published = section_clock(&first, Section::Hypa);
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(settle(&mut first).phase, "idle");
    assert_ne!(
        fleet
            .server
            .head()
            .unwrap()
            .section(Domain::Hypa)
            .unwrap()
            .changed_seq
            .as_str(),
        "0"
    );

    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(read_vector(&second, 1), Some(vec![0x11; 16]));
    assert_eq!(read_vector(&second, 2), Some(vec![0x22; 1536 * 4]));
    // The receiving device carries the observed counter forward, so its own next
    // write outranks what it received.
    assert!(section_clock(&second, Section::Hypa) >= published);
    second
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(1, 4, 0x33)])
        .unwrap();
    assert!(section_clock(&second, Section::Hypa) > published);
    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(settle(&mut second).phase, "idle");
    assert_eq!(settle(&mut first).phase, "idle");
    assert_eq!(read_vector(&first, 1), Some(vec![0x33; 16]));

    // The same key at the same version may only ever carry one value.
    let version = SectionEntryVersion {
        write_clock: Sequence::from(9),
        writer_id: "synthetic-writer".into(),
    };
    let entry = |value: &str| {
        SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("synthetic-plugin", "string", "token").unwrap(),
            SectionValue::LocalPlugin(LocalPluginValue {
                space: PluginSpace::String,
                value: json!(value),
            }),
            Some(version.clone()),
        )
        .unwrap()
    };
    let local = sections::LocalEntry {
        version: version.clone(),
        entry: entry("held"),
        object: None,
        published: true,
    };
    assert_eq!(
        sections::resolve(Some(&local), &entry("held")).unwrap(),
        sections::Outcome::Settled
    );
    assert!(sections::resolve(Some(&local), &entry("received")).is_err());
    fleet.task.abort();
}

/// Invariant 29. One section's acknowledgement never releases another's history.
#[test]
fn an_applied_hypa_section_does_not_advance_the_plugin_section_floor() {
    let fleet = fleet();
    let (_author_dir, mut author) = prepared();
    let (_reader_dir, mut reader) = prepared();
    author
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::LocalPlugins, true)
        .unwrap();
    fleet.bind(&mut author);
    fleet.bind(&mut reader);
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");

    author
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(7, 8, 0x44)])
        .unwrap();
    author
        .device_store_mut()
        .unwrap()
        .write_plugin_device_values(
            "synthetic-plugin",
            &[PluginDeviceMutation::Set {
                space: "json".into(),
                key: "settings".into(),
                value: json!({"enabled":true}).to_string(),
            }],
        )
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");

    assert_eq!(read_vector(&reader, 7), Some(vec![0x44; 32]));
    // The reader takes no part in the plugin section, so it neither holds the
    // value nor reports the section as applied.
    assert_eq!(
        reader
            .device_store()
            .unwrap()
            .read_plugin_device_value("synthetic-plugin", "json", "settings")
            .unwrap(),
        None
    );
    assert_ne!(
        fleet
            .server
            .section_ack_floor(Domain::Hypa)
            .unwrap()
            .as_str(),
        "0"
    );
    assert_eq!(
        fleet
            .server
            .section_ack_floor(Domain::LocalPlugins)
            .unwrap()
            .as_str(),
        "0"
    );
    fleet.task.abort();
}

/// Two devices can name different first publications for one removal when a
/// confirmed publication did not finish its local bookkeeping. The earliest one
/// wins whichever side reads it, so neither adapter stalls on a version it
/// already holds and both converge. A forged value at a held version is still
/// refused.
#[test]
fn a_removal_settles_on_the_earliest_first_publication_marker() {
    let (_directory, mut store) = prepared();
    let removal = |generation: u64, at_ms: u64| {
        SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("synthetic-plugin", "string", "gone").unwrap(),
            SectionValue::tombstone(Sequence::from(generation), at_ms),
            Some(SectionEntryVersion {
                write_clock: Sequence::from(4u64),
                writer_id: "writer-a".into(),
            }),
        )
        .unwrap()
    };
    let held = removal(5, 1_760_000_000_000);
    sections::write_sections(
        store.device_store_mut().unwrap(),
        &[sections::SectionWrite::Apply {
            domain: Domain::LocalPlugins,
            entry: held.clone(),
            object: None,
        }],
    )
    .unwrap();
    let local = |store: &PersistentStore| {
        sections::read_local(store.device_store().unwrap(), Domain::LocalPlugins, &held.key)
            .unwrap()
            .expect("the removal is held")
    };

    assert_eq!(
        sections::resolve(Some(&local(&store)), &removal(3, 1_760_000_000_000)).unwrap(),
        sections::Outcome::Apply
    );
    assert_eq!(
        sections::resolve(Some(&local(&store)), &removal(9, 1_760_000_000_000)).unwrap(),
        sections::Outcome::Publish
    );
    assert_eq!(
        sections::resolve(Some(&local(&store)), &held).unwrap(),
        sections::Outcome::Settled
    );
    assert!(sections::resolve(
        Some(&local(&store)),
        &plugin_entry("gone", 4, "writer-a", Some("forged"))
    )
    .is_err());

    // Taking the earlier marker leaves nothing for the next cycle to propose.
    sections::write_sections(
        store.device_store_mut().unwrap(),
        &[sections::SectionWrite::Apply {
            domain: Domain::LocalPlugins,
            entry: removal(3, 1_760_000_000_000),
            object: None,
        }],
    )
    .unwrap();
    assert_eq!(
        sections::resolve(Some(&local(&store)), &removal(3, 1_760_000_000_000)).unwrap(),
        sections::Outcome::Settled
    );
}

fn removal_marker(store: &PersistentStore, key: &str) -> Option<(String, i64)> {
    use rusqlite::OptionalExtension;
    store
        .device_store()
        .unwrap()
        .connection()
        .query_row(
            "SELECT first_published_generation,first_published_at_ms
                FROM plugin_device_storage
                WHERE owner='synthetic-plugin' AND space='json' AND key=?1 AND tombstone=1",
            [key],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        )
        .optional()
        .unwrap()
        .and_then(|(generation, at_ms)| Some((generation?, at_ms?)))
}

/// A removal names the commit it first reached a remote in. The device that
/// published it stamps it once and every later projection encodes the same
/// bytes, so the replica settles instead of proposing it again, and the device
/// that receives it keeps the marker it arrived with.
#[test]
fn a_removal_carries_one_first_publication_marker_to_every_device() {
    let fleet = fleet();
    let (_author_dir, mut author) = prepared();
    let (_reader_dir, mut reader) = prepared();
    for store in [&mut author, &mut reader] {
        store
            .device_store_mut()
            .unwrap()
            .set_section_participating(Section::LocalPlugins, true)
            .unwrap();
    }
    fleet.bind(&mut author);
    fleet.bind(&mut reader);
    author
        .device_store_mut()
        .unwrap()
        .write_plugin_device_values(
            "synthetic-plugin",
            &[PluginDeviceMutation::Set {
                space: "json".into(),
                key: "settings".into(),
                value: json!({"enabled":true}).to_string(),
            }],
        )
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");
    assert!(removal_marker(&author, "settings").is_none());

    author
        .device_store_mut()
        .unwrap()
        .write_plugin_device_values(
            "synthetic-plugin",
            &[PluginDeviceMutation::Delete {
                space: "json".into(),
                key: "settings".into(),
            }],
        )
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut author).phase, "idle");
    let marker = removal_marker(&author, "settings").expect("the author stamped its removal");

    // Further cycles neither restamp the removal nor propose it again.
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(removal_marker(&author, "settings"), Some(marker.clone()));

    assert_eq!(settle(&mut reader).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");
    assert_eq!(
        reader
            .device_store()
            .unwrap()
            .read_plugin_device_value("synthetic-plugin", "json", "settings")
            .unwrap(),
        None
    );
    assert_eq!(removal_marker(&reader, "settings"), Some(marker));
    fleet.task.abort();
}

/// A cycle acknowledges the row version it observed during preparation. A
/// value or removal written before activation remains pending for a later cycle.
#[test]
fn a_prepared_section_ack_does_not_ack_a_later_value_or_removal() {
    let fleet = fleet();
    let (_store_dir, mut store) = prepared();
    store
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::LocalPlugins, true)
        .unwrap();
    fleet.bind(&mut store);
    assert_eq!(settle(&mut store).phase, "idle");

    store
        .device_store_mut()
        .unwrap()
        .write_plugin_device_values(
            "synthetic-plugin",
            &[
                PluginDeviceMutation::Set {
                    space: "json".into(),
                    key: "changed-after-prepare".into(),
                    value: json!({"revision":"old"}).to_string(),
                },
                PluginDeviceMutation::Set {
                    space: "json".into(),
                    key: "deleted-after-prepare".into(),
                    value: json!({"revision":"old"}).to_string(),
                },
            ],
        )
        .unwrap();
    assert_eq!(settle(&mut store).phase, "idle");
    assert_eq!(settle(&mut store).phase, "idle");

    let changed_key =
        local_plugin_entry_key("synthetic-plugin", "json", "changed-after-prepare").unwrap();
    let deleted_key =
        local_plugin_entry_key("synthetic-plugin", "json", "deleted-after-prepare").unwrap();
    let old_changed = sections::read_local(
        store.device_store().unwrap(),
        Domain::LocalPlugins,
        &changed_key,
    )
    .unwrap()
    .unwrap();
    let old_deleted = sections::read_local(
        store.device_store().unwrap(),
        Domain::LocalPlugins,
        &deleted_key,
    )
    .unwrap()
    .unwrap();
    assert!(old_changed.published);
    assert!(old_deleted.published);

    // Reproduce a confirmed remote publication whose local acknowledgement was
    // lost. Preparation now chooses `mark` for these exact remote versions.
    store
        .device_store_mut()
        .unwrap()
        .connection()
        .execute(
            "UPDATE plugin_device_storage SET published_clock=NULL
                WHERE owner='synthetic-plugin' AND space='json'
                  AND key IN ('changed-after-prepare','deleted-after-prepare')",
            [],
        )
        .unwrap();
    let crate::persistent_store::server_sync_engine::Preparation::Ready(mut ready) = store
        .server_prepare_cycle(&CycleOptions::default())
        .unwrap()
    else {
        panic!("expected a prepared cycle")
    };

    store
        .device_store_mut()
        .unwrap()
        .write_plugin_device_values(
            "synthetic-plugin",
            &[
                PluginDeviceMutation::Set {
                    space: "json".into(),
                    key: "changed-after-prepare".into(),
                    value: json!({"revision":"new"}).to_string(),
                },
                PluginDeviceMutation::Delete {
                    space: "json".into(),
                    key: "deleted-after-prepare".into(),
                },
            ],
        )
        .unwrap();

    store.server_activate_cycle(&mut ready).unwrap();
    assert_eq!(store.server_publish_cycle(&ready).unwrap().phase, "idle");

    let changed = sections::read_local(
        store.device_store().unwrap(),
        Domain::LocalPlugins,
        &changed_key,
    )
    .unwrap()
    .unwrap();
    let deleted: (String, String, Option<String>, bool, Option<String>) = store
        .device_store()
        .unwrap()
        .connection()
        .query_row(
            "SELECT write_clock,writer_id,published_clock,tombstone,first_published_generation
                FROM plugin_device_storage
                WHERE owner='synthetic-plugin' AND space='json' AND key='deleted-after-prepare'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    let deleted_version = SectionEntryVersion {
        write_clock: Sequence::try_from(deleted.0).unwrap(),
        writer_id: deleted.1,
    };
    assert_ne!(changed.version, old_changed.version);
    assert_ne!(deleted_version, old_deleted.version);
    assert!(!changed.published);
    assert_eq!(deleted.2, None);
    assert!(deleted.3);
    assert_eq!(deleted.4, None);
    assert!(matches!(
        changed.entry.value,
        SectionValue::LocalPlugin(LocalPluginValue { value, .. })
            if value == json!({"revision":"new"})
    ));
    fleet.task.abort();
}

#[test]
fn section_only_plugin_refresh_is_retained_across_activation_confirmation() {
    for (participating, local_edit) in [(true, false), (false, false), (true, true)] {
        let fleet = fleet();
        let (_author_dir, mut author) = prepared();
        let (_reader_dir, mut reader) = prepared();
        for store in [&mut author, &mut reader] {
            store.device_store_mut().unwrap()
                .set_section_participating(Section::LocalPlugins, true).unwrap();
            fleet.bind(store);
            assert_eq!(settle(store).phase, "idle");
        }
        author.device_store_mut().unwrap().write_plugin_device_values("synthetic-plugin", &[
            PluginDeviceMutation::Set { space: "string".into(), key: "received".into(), value: "new".into() },
        ]).unwrap();
        assert_eq!(settle(&mut author).phase, "idle");
        assert_eq!(settle(&mut author).phase, "idle");
        let crate::persistent_store::server_sync_engine::Preparation::Ready(mut ready) = reader
            .server_prepare_cycle(&CycleOptions::default()).unwrap()
        else { panic!("expected preparation") };
        assert_eq!(ready.applied, 0);
        if local_edit {
            let mut root = reader.read_root(None).unwrap().value;
            root["username"] = json!("local edit after preparation");
            reader.commit(&WorkingSetCommit {
                root: Some(root), ..empty_working_set_commit(ready.revision)
            }).unwrap();
            assert_eq!(reader.server_activate_cycle(&mut ready).unwrap_err().code, "local-revision-changed");
            assert_eq!(reader.device_store().unwrap()
                .read_plugin_device_value("synthetic-plugin", "string", "received").unwrap(), None);
            fleet.task.abort();
            continue;
        }
        if !participating {
            reader.device_store_mut().unwrap()
                .set_section_participating(Section::LocalPlugins, false).unwrap();
        }
        let revision = reader.server_activate_cycle(&mut ready).unwrap();
        assert_eq!(ready.plugin_changes(), (participating, participating));
        assert_eq!(reader.server_activate_cycle(&mut ready).unwrap(), revision);
        assert_eq!(ready.plugin_changes(), (participating, participating));
        fleet.task.abort();
    }
}

/// Invariant 18. A choice made after a cycle was planned cancels that cycle's
/// section work instead of carrying out the previous choice.
#[test]
fn a_participation_change_during_a_cycle_cancels_its_section_work() {
    let fleet = fleet();
    let (_author_dir, mut author) = prepared();
    let (_reader_dir, mut reader) = prepared();
    fleet.bind(&mut author);
    fleet.bind(&mut reader);
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");
    author
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(3, 4, 0x55)])
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut author).phase, "idle");
    let published = fleet.server.head().unwrap();

    reader
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(4, 4, 0x66)])
        .unwrap();
    let crate::persistent_store::server_sync_engine::Preparation::Ready(mut ready) = reader
        .server_prepare_cycle(&CycleOptions::default())
        .unwrap()
    else {
        panic!("expected a prepared cycle")
    };
    reader
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::Hypa, false)
        .unwrap();
    reader.server_activate_cycle(&mut ready).unwrap();
    assert_eq!(reader.server_publish_cycle(&ready).unwrap().phase, "idle");

    // Nothing received was written and nothing held was proposed.
    assert_eq!(read_vector(&reader, 3), None);
    assert_eq!(read_vector(&reader, 4), Some(vec![0x66; 16]));
    assert_eq!(
        fleet
            .server
            .head()
            .unwrap()
            .section(Domain::Hypa)
            .unwrap()
            .changed_seq,
        published.section(Domain::Hypa).unwrap().changed_seq
    );
    fleet.task.abort();
}

/// Invariant 19. A copy restored onto this device installs no replica identity,
/// cursor or unfinished operation, and reissues no operation of its own.
#[test]
fn a_restored_replica_installs_no_section_cursor_and_reissues_no_operation() {
    let fleet = fleet();
    let (_store_dir, mut store) = prepared();
    fleet.bind(&mut store);
    store
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(5, 4, 0x77)])
        .unwrap();
    assert_eq!(settle(&mut store).phase, "idle");
    assert_eq!(settle(&mut store).phase, "idle");
    let head = store.server_status().unwrap().head.unwrap();
    assert!(!store
        .server_applied_sections(&head.epoch)
        .unwrap()
        .is_empty());

    crate::persistent_store::server_sync_outbox::restored_copy(&store.connection).unwrap();
    let revision = store.revision().unwrap();
    assert!(store
        .server_reserve(&head, "a".repeat(64), "synthetic-stage".into(), revision)
        .is_err());

    let replacement = fleet.server.add_device().unwrap();
    store
        .server_replace_registration(
            &ServerConfig {
                directory: None,
                endpoint: fleet.endpoint.clone(),
                library_id: replacement.library_id,
                device_id: replacement.device_id,
                token: replacement.token,
            },
            revision,
        )
        .unwrap();
    assert!(store.server_status().unwrap().head.is_none());
    assert!(store.server_pending().unwrap().is_none());
    assert!(store
        .server_applied_sections(&head.epoch)
        .unwrap()
        .is_empty());
    fleet.task.abort();
}

/// Invariant 37. The server ledger and the external storage adapter settle on
/// the same section rows, whichever order the values arrive in.
#[test]
fn both_sync_adapters_settle_on_the_same_section_values_whatever_the_order() {
    let entries = plugin_case_table();
    let orders = [
        vec![0, 1, 2, 3, 4, 5],
        vec![5, 4, 3, 2, 1, 0],
        vec![2, 4, 0, 5, 1, 3],
        vec![4, 1, 3, 0, 2, 5],
    ];
    let mut settled: Option<Vec<PluginRowSnapshot>> = None;
    for order in orders {
        let ledger = tempfile::tempdir().unwrap();
        let mut over_ledger = PersistentStore::open(ledger.path()).unwrap();
        let external = tempfile::tempdir().unwrap();
        let mut over_external = PersistentStore::open(external.path()).unwrap();
        for index in order {
            let entry = &entries[index];
            apply_over_the_ledger(&mut over_ledger, entry).unwrap();
            apply_over_external_storage(&mut over_external, entry).unwrap();
        }
        let rows = plugin_rows(&over_ledger);
        assert_eq!(plugin_rows(&over_external), rows);
        match &settled {
            None => settled = Some(rows),
            Some(expected) => assert_eq!(&rows, expected),
        }
    }
    let settled = settled.unwrap();
    assert_eq!(settled.len(), 3);
    assert_eq!(settled[0].value.as_deref(), Some("second"));
    assert_eq!(settled[0].writer_id, "writer-b");
    assert!(settled[1].tombstone);
    assert_eq!(settled[2].value.as_deref(), Some("only"));
}

/// Invariant 37. One version can only ever stand for one value, and both
/// adapters refuse the second one rather than picking a winner.
#[test]
fn neither_adapter_accepts_a_second_value_for_a_version_it_already_holds() {
    let first = plugin_entry("alpha", 3, "writer-a", Some("first"));
    let forged = plugin_entry("alpha", 3, "writer-a", Some("forged"));

    let ledger = tempfile::tempdir().unwrap();
    let mut over_ledger = PersistentStore::open(ledger.path()).unwrap();
    apply_over_the_ledger(&mut over_ledger, &first).unwrap();
    assert!(apply_over_the_ledger(&mut over_ledger, &forged).is_err());

    let external = tempfile::tempdir().unwrap();
    let mut over_external = PersistentStore::open(external.path()).unwrap();
    apply_over_external_storage(&mut over_external, &first).unwrap();
    assert!(apply_over_external_storage(&mut over_external, &forged).is_err());

    assert_eq!(plugin_rows(&over_ledger), plugin_rows(&over_external));
    assert_eq!(plugin_rows(&over_ledger)[0].value.as_deref(), Some("first"));
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PluginRowSnapshot {
    key: String,
    value: Option<String>,
    tombstone: bool,
    write_clock: String,
    writer_id: String,
}

fn plugin_entry(key: &str, clock: u64, writer: &str, value: Option<&str>) -> SectionEntry {
    SectionEntry::new(
        SectionKind::LocalPlugins,
        local_plugin_entry_key("synthetic-plugin", "string", key).unwrap(),
        match value {
            Some(value) => SectionValue::LocalPlugin(LocalPluginValue {
                space: PluginSpace::String,
                value: json!(value),
            }),
            None => SectionValue::tombstone(Sequence::from(3u64), 1_760_000_000_000),
        },
        Some(SectionEntryVersion {
            write_clock: Sequence::from(clock),
            writer_id: writer.into(),
        }),
    )
    .unwrap()
}

/// A local miss, an older arrival, a newer arrival, a tie the writer identity
/// breaks, a removal and a key only one side ever held.
fn plugin_case_table() -> Vec<SectionEntry> {
    vec![
        plugin_entry("alpha", 1, "writer-a", Some("first")),
        plugin_entry("alpha", 4, "writer-b", Some("second")),
        plugin_entry("alpha", 4, "writer-a", Some("loser")),
        plugin_entry("beta", 2, "writer-b", Some("kept")),
        plugin_entry("beta", 7, "writer-a", None),
        plugin_entry("gamma", 3, "writer-a", Some("only")),
    ]
}

/// The server adapter decides per key and then writes what it decided.
fn apply_over_the_ledger(store: &mut PersistentStore, entry: &SectionEntry) -> StoreResult<()> {
    let local = sections::read_local(store.device_store()?, Domain::LocalPlugins, &entry.key)?;
    if sections::resolve(local.as_ref(), entry)? != sections::Outcome::Apply {
        return Ok(());
    }
    sections::write_sections(
        store.device_store_mut()?,
        &[sections::SectionWrite::Apply {
            domain: Domain::LocalPlugins,
            entry: entry.clone(),
            object: None,
        }],
    )
}

/// The external storage adapter hands the whole row to the device file, which
/// decides and writes in one place.
fn apply_over_external_storage(
    store: &mut PersistentStore,
    entry: &SectionEntry,
) -> StoreResult<()> {
    let (owner, space, key) =
        risunest_external_storage_format::section::decode_local_plugin_entry_key(&entry.key)
            .expect("decode the plugin entry key");
    let version = entry.version.as_ref().expect("the case table sets a version");
    let row = SectionRow {
        key1: owner,
        key2: space.clone(),
        key3: key,
        value: match &entry.value {
            SectionValue::LocalPlugin(value) => SectionValueRow::Plugin {
                space,
                value: match &value.value {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string(other)?,
                },
            },
            _ => SectionValueRow::Tombstone {
                first_published: Some(TombstonePublication {
                    generation: Sequence::from(3u64),
                    at_ms: 1_760_000_000_000,
                }),
            },
        },
        write_clock: version.write_clock.clone(),
        writer_id: version.writer_id.clone(),
    };
    store
        .device_store_mut()?
        .apply_section_rows(Section::LocalPlugins, &[row])
        .map(|_| ())
}

fn plugin_rows(store: &PersistentStore) -> Vec<PluginRowSnapshot> {
    let device = store.device_store().unwrap();
    let mut statement = device
        .connection()
        .prepare(
            "SELECT key,value,tombstone,write_clock,writer_id FROM plugin_device_storage
                ORDER BY owner,space,key",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(PluginRowSnapshot {
                key: row.get(0)?,
                value: row.get(1)?,
                tombstone: row.get(2)?,
                write_clock: row.get(3)?,
                writer_id: row.get(4)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

/// Publication is recorded against the binding that received it, so a device
/// bound to another server proposes everything it holds again.
#[test]
fn a_rebound_replica_proposes_the_section_values_it_already_holds() {
    let first = fleet();
    let (_store_dir, mut store) = prepared();
    first.bind(&mut store);
    store
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(9, 4, 0x88)])
        .unwrap();
    assert_eq!(settle(&mut store).phase, "idle");
    assert_eq!(settle(&mut store).phase, "idle");
    assert_ne!(
        first
            .server
            .head()
            .unwrap()
            .section(Domain::Hypa)
            .unwrap()
            .changed_seq
            .as_str(),
        "0"
    );

    store.server_unbind().unwrap();
    let second = fleet();
    second.bind(&mut store);
    assert_eq!(settle(&mut store).phase, "idle");
    assert_eq!(settle(&mut store).phase, "idle");
    assert_ne!(
        second
            .server
            .head()
            .unwrap()
            .section(Domain::Hypa)
            .unwrap()
            .changed_seq
            .as_str(),
        "0"
    );
    let (_reader_dir, mut reader) = prepared();
    second.bind(&mut reader);
    assert_eq!(settle(&mut reader).phase, "idle");
    assert_eq!(read_vector(&reader, 9), Some(vec![0x88; 16]));
    first.task.abort();
    second.task.abort();
}

/// A head confirmation decides which sections moved. When only a section this
/// device left out changed, the cycle reads no record from the server.
#[test]
fn a_head_change_in_a_section_this_device_left_out_reads_no_record() {
    let fleet = fleet();
    let (_author_dir, mut author) = prepared();
    let (_reader_dir, mut reader) = prepared();
    fleet.bind(&mut author);
    fleet.bind(&mut reader);
    reader
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::Hypa, false)
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");

    let before = fleet.server.head().unwrap();
    author
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(11, 4, 0x99)])
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut author).phase, "idle");
    let after = fleet.server.head().unwrap();
    assert_ne!(after.head_id, before.head_id);
    assert_ne!(
        after.section(Domain::Hypa).unwrap(),
        before.section(Domain::Hypa).unwrap()
    );
    assert_eq!(
        after.section(Domain::Library).unwrap(),
        before.section(Domain::Library).unwrap()
    );

    let mark = fleet.requests();
    assert_eq!(settle(&mut reader).phase, "idle");
    let records = fleet
        .requests_since(mark)
        .into_iter()
        .filter(|path| {
            path.starts_with("/checkpoints")
                || path.starts_with("/read-pins")
                || path.starts_with("/changes")
                || path.starts_with("/objects")
        })
        .collect::<Vec<_>>();
    assert_eq!(records, Vec::<String>::new());
    assert_eq!(read_vector(&reader, 11), None);
    fleet.task.abort();
}

fn remote_cursor(store: &PersistentStore) -> Option<(String, String)> {
    store
        .connection
        .query_row(
            "SELECT head,domains FROM server_sync_remote_cursor WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}

fn rebuilt_the_mirror(fleet: &Fleet, mark: usize) -> bool {
    fleet
        .requests_since(mark)
        .iter()
        .any(|path| path.starts_with("/checkpoints"))
}

/// Letting a section go asks for nothing the mirror does not already hold, so
/// the reader keeps its place. Taking one on leaves the mirror short of that
/// section's records, which only a checkpoint over every domain can fill.
#[test]
fn leaving_a_section_keeps_the_mirror_and_taking_one_on_rebuilds_it() {
    let fleet = fleet();
    let (_author_dir, mut author) = prepared();
    let (_reader_dir, mut reader) = prepared();
    fleet.bind(&mut author);
    fleet.bind(&mut reader);
    for store in [&mut author, &mut reader] {
        store
            .device_store_mut()
            .unwrap()
            .set_section_participating(Section::LocalPlugins, true)
            .unwrap();
    }
    author
        .device_store_mut()
        .unwrap()
        .write_hypa_embeddings(&[embedding(21, 4, 0x41)])
        .unwrap();
    assert_eq!(settle(&mut author).phase, "idle");
    assert_eq!(settle(&mut reader).phase, "idle");
    let settled = remote_cursor(&reader).expect("a settled reader holds a cursor");

    let mark = fleet.requests();
    reader
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::Hypa, false)
        .unwrap();
    assert_eq!(settle(&mut reader).phase, "idle");
    let narrowed = remote_cursor(&reader).expect("the cursor survives a narrowing");
    assert_eq!(narrowed.0, settled.0);
    assert_ne!(narrowed.1, settled.1);
    assert!(!rebuilt_the_mirror(&fleet, mark));

    let mark = fleet.requests();
    reader
        .device_store_mut()
        .unwrap()
        .set_section_participating(Section::Hypa, true)
        .unwrap();
    assert_eq!(settle(&mut reader).phase, "idle");
    assert_eq!(
        remote_cursor(&reader).expect("the rebuilt cursor is stored").1,
        settled.1
    );
    assert!(rebuilt_the_mirror(&fleet, mark));
    assert_eq!(read_vector(&reader, 21), read_vector(&author, 21));
    fleet.task.abort();
}

#[test]
fn prepared_section_values_apply_before_loading_the_next_and_roll_back_on_late_failure() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    store.connection.execute_batch("CREATE TEMP TABLE server_section_records(domain TEXT,key TEXT,action TEXT,remote TEXT,version TEXT,PRIMARY KEY(domain,key));").unwrap();
    for index in 0..514 {
        store.connection.execute("INSERT INTO server_section_records VALUES(?1,?2,'apply','remote','version')", params![Domain::LocalPlugins.as_str(), format!("key-{index:04}")]).unwrap();
    }
    let before = store.device_store().unwrap().section_state(Section::LocalPlugins).unwrap();
    let payload = "v".repeat(8192);
    let mut loaded = 0;
    let result = store.write_prepared_server_sections(|_, key, _, _, _| {
        loaded += 1;
        if loaded == 514 { return Err(crate::server_sync::SyncError::new("synthetic-read-failure", 500)); }
        Ok(Some(sections::SectionWrite::Apply {
            domain: Domain::LocalPlugins, entry: plugin_entry(key, 10, "remote", Some(&payload)), object: None,
        }))
    });
    assert!(result.is_err());
    assert_eq!(loaded, 514);
    assert!(plugin_rows(&store).is_empty());
    assert_eq!(store.device_store().unwrap().section_state(Section::LocalPlugins).unwrap(), before);

    let first = plugin_entry("key-0000", 10, "remote", Some("original"));
    apply_over_the_ledger(&mut store, &first).unwrap();
    loaded = 0;
    let result = store.write_prepared_server_sections(|_, key, _, _, _| {
        loaded += 1;
        Ok(Some(sections::SectionWrite::Apply {
            domain: Domain::LocalPlugins, entry: plugin_entry(key, 10, "remote", Some(&payload)), object: None,
        }))
    });
    assert!(result.is_err());
    assert_eq!(loaded, 1, "a conflicting value must stop before loading later bodies");
    assert_eq!(plugin_rows(&store)[0].value.as_deref(), Some("original"));

    store.write_prepared_server_sections(|_, key, _, _, _| Ok(Some(sections::SectionWrite::Apply {
        domain: Domain::LocalPlugins, entry: plugin_entry(key, 11, "remote", Some(&payload)), object: None,
    }))).unwrap();
    let rows = plugin_rows(&store);
    assert_eq!(rows.len(), 514);
    assert!(rows.iter().all(|row| row.value.as_deref() == Some(payload.as_str())));
}
