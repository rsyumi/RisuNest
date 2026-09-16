use super::super::super::device_store::{
    hypa::HypaEmbeddingWrite, plugin_values::PluginDeviceMutation, Section,
};
use super::super::super::server_sync_sections as sections;
use super::*;
use risunest_external_storage_format::section::{
    local_plugin_entry_key, LocalPluginValue, PluginSpace, SectionEntry, SectionEntryVersion,
    SectionKind, SectionValue,
};
use risunest_sync_wire::Sequence;

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
    let task = runtime.spawn(async move {
        axum::serve(listener, http::router(serving)).await.unwrap();
    });
    Fleet {
        server,
        endpoint,
        _runtime: runtime,
        task,
        _dir: dir,
    }
}

impl Fleet {
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

/// Invariant 37. The rule both remote adapters share settles on one state no
/// matter which order the entries arrive in.
#[test]
fn both_sync_adapters_settle_on_the_same_section_values_whatever_the_order() {
    let plugin = |key: &str, clock: u64, writer: &str, value: Option<&str>| {
        SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("synthetic-plugin", "string", key).unwrap(),
            match value {
                Some(value) => SectionValue::LocalPlugin(LocalPluginValue {
                    space: PluginSpace::String,
                    value: json!(value),
                }),
                None => SectionValue::Tombstone,
            },
            Some(SectionEntryVersion {
                write_clock: Sequence::from(clock),
                writer_id: writer.into(),
            }),
        )
        .unwrap()
    };
    let entries = vec![
        plugin("alpha", 1, "writer-a", Some("first")),
        plugin("alpha", 4, "writer-b", Some("second")),
        plugin("alpha", 4, "writer-a", Some("loser")),
        plugin("beta", 2, "writer-b", Some("kept")),
        plugin("beta", 7, "writer-a", None),
        plugin("gamma", 3, "writer-a", Some("only")),
    ];
    let orders = [
        vec![0, 1, 2, 3, 4, 5],
        vec![5, 4, 3, 2, 1, 0],
        vec![2, 4, 0, 5, 1, 3],
        vec![4, 1, 3, 0, 2, 5],
    ];
    let mut settled: Option<Vec<(String, Option<String>, bool, String, String)>> = None;
    for order in orders {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        for index in order {
            let entry = &entries[index];
            let device = store.device_store().unwrap();
            let local = sections::read_local(device, Domain::LocalPlugins, &entry.key).unwrap();
            if sections::resolve(local.as_ref(), entry).unwrap() != sections::Outcome::Apply {
                continue;
            }
            sections::write_sections(
                store.device_store_mut().unwrap(),
                &[sections::SectionWrite::Apply {
                    domain: Domain::LocalPlugins,
                    entry: entry.clone(),
                    object: None,
                }],
            )
            .unwrap();
        }
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
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        drop(statement);
        match &settled {
            None => settled = Some(rows),
            Some(expected) => assert_eq!(&rows, expected),
        }
    }
    let settled = settled.unwrap();
    assert_eq!(settled.len(), 3);
    assert_eq!(settled[0].1.as_deref(), Some("second"));
    assert_eq!(settled[0].4, "writer-b");
    assert!(settled[1].2);
    assert_eq!(settled[2].1.as_deref(), Some("only"));
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
