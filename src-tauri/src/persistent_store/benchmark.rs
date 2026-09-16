use super::{
    CharacterQuery, CheckpointMode, ConversationMutation, ConversationQuery, PersistentStore,
    PluginStorageMutation, QueryOrder, WorkingSetCommit,
};
use rusqlite::{ffi, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Instant;

const NORMAL_CHARACTERS: usize = 500;
const CHATS_PER_CHARACTER: usize = 10;
const TURNS_PER_CHAT: usize = 100;
const STRESS_TURNS: usize = 10_000;
const STRESS_TEXT_BYTES: usize = 8 * 1024 * 1024;
const APPEND_MESSAGE_BYTES: usize = 512;
const RUNS: usize = 11;
const MAX_BATCH_CHARACTERS: usize = 16;
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;
const EXPORT_PAGE_SIZE: i64 = 128;
const POST_LEASE_RUNS: usize = 11;
const POST_LEASE_CHARACTERS: usize = 50;
const POST_LEASE_CHATS_PER_CHARACTER: usize = 5;
const POST_LEASE_TURNS_PER_CHAT: usize = 50;
const POST_LEASE_STRESS_TURNS: usize = 1_000;
const POST_LEASE_STRESS_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Statistics {
    min: u64,
    p50: u64,
    p95: u64,
    max: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    import_us: u64,
    import_sqlite_cache_write_bytes_proxy: u64,
    open_us: u64,
    materialize_us: u64,
    open_and_materialize_us: u64,
    append_commit_us: u64,
    append_sqlite_cache_write_bytes_proxy: u64,
    export_materialize_us: u64,
    acquire_revision_us: u64,
    export_traversal_us: u64,
    release_revision_us: u64,
    export_total_us: u64,
    export_traversal_json_bytes: u64,
    export_traversal_sha256: String,
    snapshot_vacuum_duration_ms: u64,
    snapshot_us: u64,
    snapshot_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Aggregate {
    import_us: Statistics,
    import_sqlite_cache_write_bytes_proxy: Statistics,
    open_us: Statistics,
    materialize_us: Statistics,
    open_and_materialize_us: Statistics,
    append_commit_us: Statistics,
    append_sqlite_cache_write_bytes_proxy: Statistics,
    export_materialize_us: Statistics,
    acquire_revision_us: Statistics,
    export_traversal_us: Statistics,
    release_revision_us: Statistics,
    export_total_us: Statistics,
    export_traversal_json_bytes: Statistics,
    snapshot_vacuum_duration_ms: Statistics,
    snapshot_us: Statistics,
    snapshot_bytes: Statistics,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OneGibSample {
    vacuum_duration_ms: u64,
    command_duration_us: u64,
    snapshot_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OneGibAggregate {
    vacuum_duration_ms: Statistics,
    command_duration_us: Statistics,
    snapshot_bytes: Statistics,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OneGibDiagnostic {
    discarded_warmup_runs: usize,
    measured_runs: usize,
    aggregate: OneGibAggregate,
    samples: Vec<OneGibSample>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FixtureDescription {
    characters: usize,
    chats_per_character: usize,
    turns_per_chat: usize,
    stress_turns: usize,
    stress_text_bytes: usize,
    stress_chat_json_bytes: u64,
    total_conversations: usize,
    total_messages: usize,
    serialized_bytes: u64,
    fnv1a64: String,
    sha256: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkResult {
    schema_version: u32,
    benchmark: &'static str,
    source_revision: Option<String>,
    fixture: FixtureDescription,
    export_traversal_sha256_provenance: &'static str,
    discarded_warmup_runs: usize,
    measured_runs: usize,
    write_metric: &'static str,
    aggregate: Aggregate,
    samples: Vec<Sample>,
    one_gib_diagnostic: Option<OneGibDiagnostic>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostLeaseMutationSample {
    commit_us: u64,
    wal_before_bytes: u64,
    wal_after_bytes: u64,
    wal_growth_bytes: u64,
    active_lease_count: usize,
    oldest_lease_age_us: u64,
    release_us: u64,
    wal_after_release_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostLeaseCommitSample {
    plugin: PostLeaseMutationSample,
    root: PostLeaseMutationSample,
    message: PostLeaseMutationSample,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostLeaseCommitAggregate {
    plugin_us: Statistics,
    root_us: Statistics,
    message_us: Statistics,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PostLeaseCommitBenchmarkResult {
    schema_version: u32,
    benchmark: &'static str,
    source_revision: Option<String>,
    discarded_warmup_runs: usize,
    measured_runs: usize,
    fixture_characters: usize,
    fixture_conversations: usize,
    fixture_messages: usize,
    fixture_serialized_bytes: u64,
    fixture_sha256: String,
    aggregate: PostLeaseCommitAggregate,
    samples: Vec<PostLeaseCommitSample>,
}

fn nearest_rank(values: &[u64]) -> Statistics {
    assert!(!values.is_empty(), "statistics require at least one sample");
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let nearest_rank_percentile = |percent: usize| {
        let rank = (percent * sorted.len()).div_ceil(100).max(1);
        sorted[rank - 1]
    };
    let midpoint = sorted.len() / 2;
    let p50 = if sorted.len() % 2 == 0 {
        sorted[midpoint - 1].saturating_add(sorted[midpoint]) / 2
    } else {
        sorted[midpoint]
    };
    Statistics {
        min: sorted[0],
        p50,
        p95: nearest_rank_percentile(95),
        max: sorted[sorted.len() - 1],
    }
}

fn deterministic_text(seed: usize, bytes: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..bytes)
        .map(|index| {
            ALPHABET[(seed.wrapping_mul(17) + index.wrapping_mul(31)) % ALPHABET.len()] as char
        })
        .collect()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn message(character: usize, chat: usize, turn: usize, payload_bytes: usize) -> Value {
    let id = format!("msg-{character:04}-{chat:02}-{turn:05}");
    json!({
        "role": if turn % 2 == 0 { "user" } else { "char" },
        "data": deterministic_text(character * 100_000 + chat * 10_000 + turn, payload_bytes),
        "chatId": id,
        "time": 1_800_000_000_000i64 + turn as i64
    })
}

fn contract_fixture() -> Value {
    serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json"))
        .expect("parse persistent store fixture")
}

fn conversation(
    template: &Value,
    character: usize,
    chat: usize,
    turns: usize,
    payload_bytes: usize,
) -> Value {
    let mut conversation = template.clone();
    conversation["id"] = json!(format!("chat-{character:04}-{chat:02}"));
    conversation["name"] = json!(format!("Chat {character}-{chat}"));
    conversation["lastDate"] = json!(1_800_000_000_000i64 + turns as i64);
    conversation["message"] = Value::Array(
        (0..turns)
            .map(|turn| message(character, chat, turn, payload_bytes))
            .collect(),
    );
    conversation
}

fn stress_conversation(
    template: &Value,
    character: usize,
    chat: usize,
    turns: usize,
    total_text_bytes: usize,
) -> Value {
    assert!(turns > 0, "stress conversation requires messages");
    let base_bytes = total_text_bytes / turns;
    let remainder = total_text_bytes % turns;
    let mut conversation = template.clone();
    conversation["id"] = json!(format!("chat-{character:04}-stress"));
    conversation["name"] = json!("Chat stress");
    conversation["lastDate"] = json!(1_900_000_000_000i64 + turns as i64);
    conversation["message"] = Value::Array(
        (0..turns)
            .map(|turn| {
                message(
                    character,
                    chat,
                    turn,
                    base_bytes + usize::from(turn < remainder),
                )
            })
            .collect(),
    );
    conversation
}

fn generate_save_large(
    normal_characters: usize,
    chats_per_character: usize,
    turns_per_chat: usize,
    stress_turns: usize,
    stress_text_bytes: usize,
) -> Value {
    assert!(normal_characters > 0, "save-large requires characters");
    let mut database = contract_fixture();
    let character_template = database["characters"][0].clone();
    let conversation_template = character_template["chats"][0].clone();
    let characters = (0..normal_characters)
        .map(|character| {
            let mut value = character_template.clone();
            value["chaId"] = json!(format!("character-{character:04}"));
            value["name"] = json!(format!("Character {character}"));
            value["image"] = json!(format!("character-{character:04}.png"));
            value["lastInteraction"] = json!(1_800_000_000_000i64 + character as i64);
            let mut chats = (0..chats_per_character)
                .map(|chat| {
                    conversation(&conversation_template, character, chat, turns_per_chat, 32)
                })
                .collect::<Vec<_>>();
            if character == 0 {
                chats.push(stress_conversation(
                    &conversation_template,
                    character,
                    chats_per_character,
                    stress_turns,
                    stress_text_bytes,
                ));
            }
            value["chats"] = Value::Array(chats);
            value
        })
        .collect::<Vec<_>>();
    database["fixture"] = json!("phase3-step5-save-large");
    database["characters"] = Value::Array(characters);
    database
}

fn root_without_characters(database: &Value) -> Value {
    let mut root = database.clone();
    root.as_object_mut()
        .expect("benchmark database object")
        .remove("characters");
    root
}

fn stage_in_public_batches(store: &mut PersistentStore, staging_id: &str, characters: &[Value]) {
    let mut start = 0;
    while start < characters.len() {
        let mut end = start;
        let mut bytes = 2;
        while end < characters.len() && end - start < MAX_BATCH_CHARACTERS {
            let character_bytes = serde_json::to_vec(&characters[end])
                .expect("serialize benchmark character")
                .len();
            let separator_bytes = usize::from(end > start);
            if end > start && bytes + separator_bytes + character_bytes > MAX_BATCH_BYTES {
                break;
            }
            bytes += separator_bytes + character_bytes;
            end += 1;
        }
        store
            .replace_add_characters(staging_id, &characters[start..end])
            .expect("stage benchmark character batch");
        start = end;
    }
}

fn sqlite_cache_write_bytes(connection: &Connection, reset: bool) -> u64 {
    let mut pages = 0;
    let mut highwater = 0;
    // This counts pages written from SQLite's page cache. It is a logical proxy, not physical I/O.
    let result = unsafe {
        ffi::sqlite3_db_status(
            connection.handle(),
            ffi::SQLITE_DBSTATUS_CACHE_WRITE,
            &mut pages,
            &mut highwater,
            i32::from(reset),
        )
    };
    assert_eq!(result, ffi::SQLITE_OK, "read SQLite cache-write counter");
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("read SQLite page size");
    (pages.max(0) as u64).saturating_mul(page_size.max(0) as u64)
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros().try_into().unwrap_or(u64::MAX)
}

fn assert_fixture_shape(
    database: &Value,
    expected_first_chat_messages: usize,
    expected_stress_messages: usize,
) {
    let characters = database["characters"]
        .as_array()
        .expect("materialized characters");
    assert_eq!(characters.len(), NORMAL_CHARACTERS);
    assert_eq!(
        characters[0]["chats"].as_array().unwrap().len(),
        CHATS_PER_CHARACTER + 1
    );
    assert_eq!(
        characters[0]["chats"][0]["message"]
            .as_array()
            .unwrap()
            .len(),
        expected_first_chat_messages
    );
    assert_eq!(
        characters[0]["chats"][CHATS_PER_CHARACTER]["message"]
            .as_array()
            .unwrap()
            .len(),
        expected_stress_messages
    );
}

struct FramedJsonDigest {
    hasher: Sha256,
}

impl FramedJsonDigest {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn update(&mut self, value: &Value) -> u64 {
        let bytes = serde_json::to_vec(value).expect("serialize export fragment");
        self.hasher.update((bytes.len() as u64).to_le_bytes());
        self.hasher.update(&bytes);
        bytes.len() as u64
    }

    fn finish(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

struct ExportTraversal {
    acquire_revision_us: u64,
    traversal_us: u64,
    release_revision_us: u64,
    json_bytes: u64,
    sha256: String,
}

fn export_traversal(store: &mut PersistentStore) -> ExportTraversal {
    let revision = store.revision().expect("read export revision");
    let acquire_started = Instant::now();
    let lease = store
        .acquire_revision(revision)
        .expect("acquire export revision");
    let acquire_revision_us = elapsed_us(acquire_started);
    let traversal_started = Instant::now();
    let mut digest = FramedJsonDigest::new();
    let mut bytes = digest.update(&store.read_root(Some(&lease.lease)).unwrap().value);
    let mut character_count = 0;
    let mut conversation_count = 0;
    let mut message_count = 0;
    for trash in [false, true] {
        let mut character_cursor = None;
        loop {
            let page = store
                .query_characters(
                    &CharacterQuery {
                        search: None,
                        order: QueryOrder::Configured,
                        trash,
                        limit: EXPORT_PAGE_SIZE,
                        cursor: character_cursor.clone(),
                    },
                    Some(&lease.lease),
                )
                .expect("traverse export characters");
            for character in &page.items {
                character_count += 1;
                let mut detail = store
                    .read_character(&character.id, Some(&lease.lease))
                    .expect("read export character")
                    .expect("export character exists")
                    .value;
                let mut serialized_conversations = Vec::new();
                let mut conversation_cursor = None;
                loop {
                    let conversations = store
                        .query_conversations(
                            &ConversationQuery {
                                character_id: character.id.clone(),
                                order: QueryOrder::Configured,
                                limit: EXPORT_PAGE_SIZE,
                                cursor: conversation_cursor.clone(),
                            },
                            Some(&lease.lease),
                        )
                        .expect("traverse export conversations");
                    for conversation in &conversations.items {
                        conversation_count += 1;
                        message_count += conversation.message_count as usize;
                        serialized_conversations.push(
                            store
                                .read_conversation(
                                    &character.id,
                                    &conversation.id,
                                    Some(&lease.lease),
                                )
                                .expect("read export conversation")
                                .expect("export conversation exists")
                                .value,
                        );
                    }
                    conversation_cursor = conversations.next_cursor;
                    if conversation_cursor.is_none() {
                        break;
                    }
                }
                detail["chats"] = Value::Array(serialized_conversations);
                bytes += digest.update(&detail);
            }
            character_cursor = page.next_cursor;
            if character_cursor.is_none() {
                break;
            }
        }
    }
    let traversal_us = elapsed_us(traversal_started);
    assert_eq!(character_count, NORMAL_CHARACTERS);
    assert_eq!(
        conversation_count,
        NORMAL_CHARACTERS * CHATS_PER_CHARACTER + 1
    );
    assert_eq!(
        message_count,
        NORMAL_CHARACTERS * CHATS_PER_CHARACTER * TURNS_PER_CHAT + STRESS_TURNS + 1
    );
    let release_started = Instant::now();
    store
        .release_revision(&lease.lease)
        .expect("release export revision");
    ExportTraversal {
        acquire_revision_us,
        traversal_us,
        release_revision_us: elapsed_us(release_started),
        json_bytes: bytes,
        sha256: digest.finish(),
    }
}

fn run_sample(database: &Value, root: &Value) -> Sample {
    let directory = tempfile::tempdir().expect("create benchmark directory");
    let mut store = PersistentStore::open(directory.path()).expect("open fresh benchmark store");
    assert_eq!(store.revision().unwrap(), 0);
    let default_staging = store.replace_begin().expect("begin default seed");
    store
        .replace_put_root(
            &default_staging.staging_id,
            &json!({ "formatVersion": 3, "fixture": "phase3-step5-default" }),
        )
        .expect("stage default root");
    assert_eq!(
        store
            .replace_commit(&default_staging.staging_id, Some(0))
            .expect("commit default seed")
            .revision,
        1
    );
    sqlite_cache_write_bytes(&store.connection, true);
    let import_started = Instant::now();
    let staging = store.replace_begin().expect("begin benchmark import");
    store
        .replace_put_root(&staging.staging_id, root)
        .expect("stage benchmark root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"]
                .as_array()
                .expect("benchmark presets"),
        )
        .expect("stage benchmark presets");
    stage_in_public_batches(
        &mut store,
        &staging.staging_id,
        database["characters"].as_array().unwrap(),
    );
    assert_eq!(
        store
            .replace_commit(&staging.staging_id, Some(1))
            .expect("commit benchmark import")
            .revision,
        2
    );
    let import_us = elapsed_us(import_started);
    let import_sqlite_cache_write_bytes_proxy = sqlite_cache_write_bytes(&store.connection, true);
    drop(store);

    let open_started = Instant::now();
    let mut store = PersistentStore::open(directory.path()).expect("reopen populated store");
    let open_us = elapsed_us(open_started);
    let materialize_started = Instant::now();
    let materialized = store
        .materialize(None)
        .expect("materialize populated store");
    let materialize_us = elapsed_us(materialize_started);
    let open_and_materialize_us = open_us.saturating_add(materialize_us);
    assert_eq!(&materialized, database);
    assert_fixture_shape(&materialized, TURNS_PER_CHAT, STRESS_TURNS);
    drop(materialized);

    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("normalize WAL before append");
    sqlite_cache_write_bytes(&store.connection, true);
    let append_started = Instant::now();
    let appended = message(0, 0, TURNS_PER_CHAT, APPEND_MESSAGE_BYTES);
    assert_eq!(
        store
            .commit(&WorkingSetCommit {
                expected_revision: 2,
                root_mutations: None,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "character-0000".to_owned(),
                    conversation_id: "chat-0000-00".to_owned(),
                    start: TURNS_PER_CHAT as i64,
                    delete_count: 0,
                    messages: vec![appended.clone()],
                    conversation: None,
                    configured_index: None,
                }]),
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: None,
            })
            .expect("append benchmark message")
            .revision,
        3
    );
    let append_commit_us = elapsed_us(append_started);
    let append_sqlite_cache_write_bytes_proxy = sqlite_cache_write_bytes(&store.connection, true);
    let appended_conversation = store
        .read_conversation("character-0000", "chat-0000-00", None)
        .unwrap()
        .unwrap();
    assert_eq!(
        appended_conversation.value["message"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &appended
    );

    let export_started = Instant::now();
    let export_materialize_started = Instant::now();
    let exported = store.materialize(None).expect("materialize export");
    let export_materialize_us = elapsed_us(export_materialize_started);
    let export_traversal = export_traversal(&mut store);
    let export_total_us = elapsed_us(export_started);
    assert_fixture_shape(&exported, TURNS_PER_CHAT + 1, STRESS_TURNS);
    assert_eq!(
        exported["characters"][0]["chats"][0]["message"]
            .as_array()
            .unwrap()
            .len(),
        TURNS_PER_CHAT + 1
    );
    drop(exported);

    let snapshot_started = Instant::now();
    let snapshot = store
        .snapshot_create("phase3-step5")
        .expect("create benchmark snapshot");
    let snapshot_us = elapsed_us(snapshot_started);
    assert!(snapshot.bytes > 0);
    assert!(store
        .snapshot_list()
        .unwrap()
        .iter()
        .any(|s| s.id == snapshot.id));
    assert!(store.storage_stats().unwrap().snapshot_bytes > 0);

    Sample {
        import_us,
        import_sqlite_cache_write_bytes_proxy,
        open_us,
        materialize_us,
        open_and_materialize_us,
        append_commit_us,
        append_sqlite_cache_write_bytes_proxy,
        export_materialize_us,
        acquire_revision_us: export_traversal.acquire_revision_us,
        export_traversal_us: export_traversal.traversal_us,
        release_revision_us: export_traversal.release_revision_us,
        export_total_us,
        export_traversal_json_bytes: export_traversal.json_bytes,
        export_traversal_sha256: export_traversal.sha256,
        snapshot_vacuum_duration_ms: snapshot.duration_ms,
        snapshot_us,
        snapshot_bytes: snapshot.bytes,
    }
}

fn aggregate(samples: &[Sample]) -> Aggregate {
    macro_rules! statistics {
        ($field:ident) => {
            nearest_rank(
                &samples
                    .iter()
                    .map(|sample| sample.$field)
                    .collect::<Vec<_>>(),
            )
        };
    }
    Aggregate {
        import_us: statistics!(import_us),
        import_sqlite_cache_write_bytes_proxy: statistics!(import_sqlite_cache_write_bytes_proxy),
        open_us: statistics!(open_us),
        materialize_us: statistics!(materialize_us),
        open_and_materialize_us: statistics!(open_and_materialize_us),
        append_commit_us: statistics!(append_commit_us),
        append_sqlite_cache_write_bytes_proxy: statistics!(append_sqlite_cache_write_bytes_proxy),
        export_materialize_us: statistics!(export_materialize_us),
        acquire_revision_us: statistics!(acquire_revision_us),
        export_traversal_us: statistics!(export_traversal_us),
        release_revision_us: statistics!(release_revision_us),
        export_total_us: statistics!(export_total_us),
        export_traversal_json_bytes: statistics!(export_traversal_json_bytes),
        snapshot_vacuum_duration_ms: statistics!(snapshot_vacuum_duration_ms),
        snapshot_us: statistics!(snapshot_us),
        snapshot_bytes: statistics!(snapshot_bytes),
    }
}

fn run_one_gib_diagnostic() -> OneGibDiagnostic {
    let directory = tempfile::tempdir().expect("create 1 GiB diagnostic directory");
    let mut store = PersistentStore::open(directory.path()).expect("open 1 GiB diagnostic store");
    store
        .connection
        .execute("CREATE TABLE benchmark_padding (payload BLOB NOT NULL)", [])
        .expect("create SQLite padding table");
    let transaction = store
        .connection
        .transaction()
        .expect("begin SQLite padding transaction");
    for _ in 0..1024 {
        transaction
            .execute(
                "INSERT INTO benchmark_padding (payload) VALUES (zeroblob(?1))",
                [1024 * 1024],
            )
            .expect("insert 1 MiB SQLite padding row");
    }
    transaction.commit().expect("commit SQLite padding");
    store
        .checkpoint(CheckpointMode::Truncate)
        .expect("checkpoint 1 GiB diagnostic setup");
    let page_count: i64 = store
        .connection
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .expect("read 1 GiB page count");
    let page_size: i64 = store
        .connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("read 1 GiB page size");
    assert!(page_count.saturating_mul(page_size) >= 1024 * 1024 * 1024);
    let mut samples = (0..RUNS)
        .map(|run| {
            let started = Instant::now();
            let snapshot = store
                .snapshot_create(&format!("phase3-step5-1g-diagnostic-{run}"))
                .expect("create 1 GiB diagnostic snapshot");
            let command_duration_us = elapsed_us(started);
            assert!(snapshot.bytes >= 1024 * 1024 * 1024);
            assert!(store
                .snapshot_list()
                .unwrap()
                .iter()
                .any(|s| s.id == snapshot.id));
            OneGibSample {
                vacuum_duration_ms: snapshot.duration_ms,
                command_duration_us,
                snapshot_bytes: snapshot.bytes,
            }
        })
        .collect::<Vec<_>>();
    samples.remove(0);
    let vacuum_duration_ms = nearest_rank(
        &samples
            .iter()
            .map(|sample| sample.vacuum_duration_ms)
            .collect::<Vec<_>>(),
    );
    let command_duration_us = nearest_rank(
        &samples
            .iter()
            .map(|sample| sample.command_duration_us)
            .collect::<Vec<_>>(),
    );
    let snapshot_bytes = nearest_rank(
        &samples
            .iter()
            .map(|sample| sample.snapshot_bytes)
            .collect::<Vec<_>>(),
    );
    OneGibDiagnostic {
        discarded_warmup_runs: 1,
        measured_runs: samples.len(),
        aggregate: OneGibAggregate {
            vacuum_duration_ms,
            command_duration_us,
            snapshot_bytes,
        },
        samples,
    }
}

fn wal_bytes(store: &PersistentStore) -> u64 {
    let path = std::path::PathBuf::from(format!("{}-wal", store.database_path.display()));
    match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read WAL size at {}: {error}", path.display()),
    }
}

fn post_lease_commit(
    store: &mut PersistentStore,
    input: WorkingSetCommit,
    label: &str,
) -> PostLeaseMutationSample {
    let revision = store.revision().expect("read post-lease revision");
    assert_eq!(input.expected_revision, revision);
    let lease = store
        .acquire_revision(revision)
        .expect("acquire post-lease revision");
    let wal_before_bytes = wal_bytes(store);
    let started = Instant::now();
    assert_eq!(
        store
            .commit(&input)
            .unwrap_or_else(|error| panic!("commit post-lease {label} mutation: {error}"))
            .revision,
        revision + 1
    );
    let commit_us = elapsed_us(started);
    let wal_after_bytes = wal_bytes(store);
    let diagnostics = store.lease_diagnostics();
    let release_started = Instant::now();
    store
        .release_revision(&lease.lease)
        .expect("release post-lease revision");
    PostLeaseMutationSample {
        commit_us,
        wal_before_bytes,
        wal_after_bytes,
        wal_growth_bytes: wal_after_bytes.saturating_sub(wal_before_bytes),
        active_lease_count: diagnostics.active_count,
        oldest_lease_age_us: diagnostics.oldest_age_us,
        release_us: elapsed_us(release_started),
        wal_after_release_bytes: wal_bytes(store),
    }
}

fn run_post_lease_commit_benchmark(database: &Value) -> Vec<PostLeaseCommitSample> {
    let directory = tempfile::tempdir().expect("create post-lease benchmark directory");
    let mut store =
        PersistentStore::open(directory.path()).expect("open post-lease benchmark store");
    let staging = store
        .replace_begin()
        .expect("begin post-lease fixture import");
    store
        .replace_put_root(&staging.staging_id, &root_without_characters(database))
        .expect("stage post-lease fixture root");
    store
        .replace_put_presets(
            &staging.staging_id,
            database["botPresets"]
                .as_array()
                .expect("post-lease benchmark presets"),
        )
        .expect("stage post-lease benchmark presets");
    stage_in_public_batches(
        &mut store,
        &staging.staging_id,
        database["characters"].as_array().unwrap(),
    );
    store
        .replace_commit(&staging.staging_id, Some(0))
        .expect("commit post-lease fixture import");

    (0..POST_LEASE_RUNS)
        .map(|run| {
            let revision = store.revision().unwrap();
            let plugin = post_lease_commit(
                &mut store,
                WorkingSetCommit {
                    expected_revision: revision,
                    root_mutations: None,
                    root: None,
                    replace_presets: None,
                    character: None,
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: None,
                    delete_character_id: None,
                    asset_owner_heads: None,
                    plugin_storage: Some(vec![PluginStorageMutation::Set {
                        owner: "synthetic-plugin".to_owned(),
                        key: "benchmark-plugin".to_owned(),
                        value: json!(deterministic_text(run, 1024)),
                    }]),
                },
                "plugin",
            );

            let mut root = store.read_root(None).unwrap().value;
            root["postLeaseBenchmarkRun"] = json!(run);
            let revision = store.revision().unwrap();
            let root = post_lease_commit(
                &mut store,
                WorkingSetCommit {
                    expected_revision: revision,
                    root_mutations: None,
                    root: Some(root),
                    replace_presets: None,
                    character: None,
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: None,
                    delete_character_id: None,
                    asset_owner_heads: None,
                    plugin_storage: None,
                },
                "root",
            );

            let revision = store.revision().unwrap();
            let message = post_lease_commit(
                &mut store,
                WorkingSetCommit {
                    expected_revision: revision,
                    root_mutations: None,
                    root: None,
                    replace_presets: None,
                    character: None,
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: Some(vec![ConversationMutation::ReplaceRange {
                        character_id: "character-0000".to_owned(),
                        conversation_id: "chat-0000-00".to_owned(),
                        start: 0,
                        delete_count: 1,
                        messages: vec![message(0, 0, 0, 32)],
                        conversation: None,
                        configured_index: None,
                    }]),
                    delete_character_id: None,
                    asset_owner_heads: None,
                    plugin_storage: None,
                },
                "message",
            );

            PostLeaseCommitSample {
                plugin,
                root,
                message,
            }
        })
        .collect()
}

#[test]
#[ignore = "release-only first-post-lease commit benchmark"]
fn first_post_lease_commit_measurements() {
    let mut database = generate_save_large(
        POST_LEASE_CHARACTERS,
        POST_LEASE_CHATS_PER_CHARACTER,
        POST_LEASE_TURNS_PER_CHAT,
        POST_LEASE_STRESS_TURNS,
        POST_LEASE_STRESS_TEXT_BYTES,
    );
    database["pluginCustomStorage"] = Value::Object(
        (0..256)
            .map(|index| {
                (
                    format!("plugin-{index:04}"),
                    json!(deterministic_text(index, 1024)),
                )
            })
            .collect(),
    );
    let serialized = serde_json::to_vec(&database).expect("serialize post-lease fixture");
    let mut samples = run_post_lease_commit_benchmark(&database);
    samples.remove(0);
    let result = PostLeaseCommitBenchmarkResult {
        schema_version: 2,
        benchmark: "first-post-lease-commit",
        source_revision: std::env::var("RISUNEST_POST_LEASE_BENCH_REVISION").ok(),
        discarded_warmup_runs: 1,
        measured_runs: samples.len(),
        fixture_characters: POST_LEASE_CHARACTERS,
        fixture_conversations: POST_LEASE_CHARACTERS * POST_LEASE_CHATS_PER_CHARACTER + 1,
        fixture_messages: POST_LEASE_CHARACTERS
            * POST_LEASE_CHATS_PER_CHARACTER
            * POST_LEASE_TURNS_PER_CHAT
            + POST_LEASE_STRESS_TURNS,
        fixture_serialized_bytes: serialized.len() as u64,
        fixture_sha256: sha256_hex(&serialized),
        aggregate: PostLeaseCommitAggregate {
            plugin_us: nearest_rank(
                &samples
                    .iter()
                    .map(|sample| sample.plugin.commit_us)
                    .collect::<Vec<_>>(),
            ),
            root_us: nearest_rank(
                &samples
                    .iter()
                    .map(|sample| sample.root.commit_us)
                    .collect::<Vec<_>>(),
            ),
            message_us: nearest_rank(
                &samples
                    .iter()
                    .map(|sample| sample.message.commit_us)
                    .collect::<Vec<_>>(),
            ),
        },
        samples,
    };
    let encoded = serde_json::to_string(&result).expect("serialize post-lease benchmark result");
    if let Ok(path) = std::env::var("RISUNEST_POST_LEASE_BENCH_OUTPUT") {
        std::fs::write(path, encoded.as_bytes()).expect("write post-lease benchmark result");
    }
    println!("{encoded}");
}

#[test]
#[ignore = "release-only Phase 3 Step 5 benchmark"]
fn phase3_step5_measurements() {
    let database = generate_save_large(
        NORMAL_CHARACTERS,
        CHATS_PER_CHARACTER,
        TURNS_PER_CHAT,
        STRESS_TURNS,
        STRESS_TEXT_BYTES,
    );
    let serialized = serde_json::to_vec(&database).expect("serialize save-large fixture");
    if let Ok(path) = std::env::var("RISUNEST_PHASE3_FIXTURE_OUTPUT") {
        std::fs::write(path, &serialized).expect("write save-large fixture");
    }
    let root = root_without_characters(&database);
    let stress_chat_json_bytes =
        serde_json::to_vec(&database["characters"][0]["chats"][CHATS_PER_CHARACTER])
            .expect("serialize stress chat")
            .len() as u64;
    let mut all_samples = (0..RUNS)
        .map(|_| run_sample(&database, &root))
        .collect::<Vec<_>>();
    all_samples.remove(0);
    let result = BenchmarkResult {
        schema_version: 1,
        benchmark: "phase3-step5-persistent-store",
        source_revision: std::env::var("RISUNEST_PHASE3_BENCH_REVISION").ok(),
        fixture: FixtureDescription {
            characters: NORMAL_CHARACTERS,
            chats_per_character: CHATS_PER_CHARACTER,
            turns_per_chat: TURNS_PER_CHAT,
            stress_turns: STRESS_TURNS,
            stress_text_bytes: STRESS_TEXT_BYTES,
            stress_chat_json_bytes,
            total_conversations: NORMAL_CHARACTERS * CHATS_PER_CHARACTER + 1,
            total_messages: NORMAL_CHARACTERS * CHATS_PER_CHARACTER * TURNS_PER_CHAT
                + STRESS_TURNS,
            serialized_bytes: serialized.len() as u64,
            fnv1a64: format!("{:016x}", fnv1a64(&serialized)),
            sha256: sha256_hex(&serialized),
        },
        export_traversal_sha256_provenance:
            "sha256-of-u64le-length-prefixed-json-fragments-in-export-traversal-order-after-append",
        discarded_warmup_runs: 1,
        measured_runs: all_samples.len(),
        write_metric: "SQLite DBSTATUS_CACHE_WRITE pages multiplied by page size, a logical cache-write proxy, not physical I/O",
        aggregate: aggregate(&all_samples),
        samples: all_samples,
        one_gib_diagnostic: (std::env::var("RISUNEST_PHASE3_BENCH_1G").as_deref()
            == Ok("true"))
        .then(run_one_gib_diagnostic),
    };
    let encoded = serde_json::to_string(&result).expect("serialize benchmark result");
    if let Ok(path) = std::env::var("RISUNEST_PHASE3_BENCH_OUTPUT") {
        std::fs::write(path, encoded.as_bytes()).expect("write benchmark result");
    }
    println!("{encoded}");
}

#[cfg(test)]
mod tests {
    use super::{generate_save_large, nearest_rank, sha256_hex, FramedJsonDigest};
    use serde_json::json;

    #[test]
    fn framed_export_digest_is_stable_and_preserves_fragment_boundaries() {
        let digest = |values: &[serde_json::Value]| {
            let mut digest = FramedJsonDigest::new();
            for value in values {
                digest.update(value);
            }
            digest.finish()
        };

        assert_eq!(
            digest(&[json!({ "a": 1 }), json!([2, 3])]),
            digest(&[json!({ "a": 1 }), json!([2, 3]),])
        );
        assert_ne!(digest(&[json!([1]), json!([2])]), digest(&[json!([1, 2])]));
    }

    #[test]
    fn sha256_identity_is_lowercase_and_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn deterministic_generator_has_expected_shape() {
        let first = generate_save_large(2, 2, 3, 8, 16);
        let second = generate_save_large(2, 2, 3, 8, 16);

        assert_eq!(first, second);
        let characters = first["characters"].as_array().expect("characters array");
        assert_eq!(characters.len(), 2);
        assert_eq!(characters[0]["chats"].as_array().unwrap().len(), 3);
        assert_eq!(characters[1]["chats"].as_array().unwrap().len(), 2);
        assert_eq!(
            characters[0]["chats"][0]["message"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            characters[0]["chats"][2]["message"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        let stress_text_bytes = characters[0]["chats"][2]["message"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["data"].as_str().unwrap().len())
            .sum::<usize>();
        assert_eq!(stress_text_bytes, 16);
    }

    #[test]
    fn nearest_rank_reports_min_median_p95_and_max() {
        let statistics = nearest_rank(&[90, 10, 100, 20, 30, 40, 50, 60, 70, 80]);

        assert_eq!(statistics.min, 10);
        assert_eq!(statistics.p50, 55);
        assert_eq!(statistics.p95, 100);
        assert_eq!(statistics.max, 100);
    }
}

const HYPA_ENTRIES: usize = 1_000;
const HYPA_DIMENSIONS: usize = 1_536;
const HYPA_RUNS: usize = 6;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HypaSample {
    batch_write_us: u64,
    per_entry_write_us: u64,
    batch_read_us: u64,
    per_key_read_us: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HypaAggregate {
    batch_write_us: Statistics,
    per_entry_write_us: Statistics,
    batch_read_us: Statistics,
    per_key_read_us: Statistics,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HypaPayload {
    binary_bytes: usize,
    json_number_array_bytes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HypaBenchmarkResult {
    schema_version: u32,
    benchmark: &'static str,
    source_revision: Option<String>,
    entries: usize,
    dimensions: usize,
    vector_transport: &'static str,
    one_vector: HypaPayload,
    discarded_warmup_runs: usize,
    measured_runs: usize,
    aggregate: HypaAggregate,
    samples: Vec<HypaSample>,
}

fn deterministic_vector(seed: u64, dimensions: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
    let mut bytes = Vec::with_capacity(dimensions * 4);
    for _ in 0..dimensions {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let value = (state >> 40) as f32 / 16_777_216.0 - 0.5;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn hypa_entries(salt: usize) -> Vec<super::device_store::hypa::HypaEmbeddingWrite> {
    (0..HYPA_ENTRIES)
        .map(|index| super::device_store::hypa::HypaEmbeddingWrite {
            cache_key: format!("{:064x}", (salt * HYPA_ENTRIES + index) as u128),
            producer: "hypa-v2".to_owned(),
            model: "bench-model".to_owned(),
            endpoint: None,
            preprocess_version: 1,
            dimensions: HYPA_DIMENSIONS as i64,
            vector: deterministic_vector((salt * HYPA_ENTRIES + index) as u64, HYPA_DIMENSIONS),
            metadata: None,
        })
        .collect()
}

fn hypa_sample(run: usize) -> HypaSample {
    let directory = tempfile::tempdir().expect("create benchmark device store directory");
    let mut store =
        super::device_store::DeviceStore::open(directory.path()).expect("open device store");

    let batched = hypa_entries(run * 2);
    let started = Instant::now();
    store
        .write_hypa_embeddings(&batched)
        .expect("write the batch in one call");
    let batch_write_us = started.elapsed().as_micros() as u64;

    let single = hypa_entries(run * 2 + 1);
    let started = Instant::now();
    for entry in &single {
        store
            .write_hypa_embeddings(std::slice::from_ref(entry))
            .expect("write one entry per call");
    }
    let per_entry_write_us = started.elapsed().as_micros() as u64;

    let keys = batched
        .iter()
        .map(|entry| entry.cache_key.clone())
        .collect::<Vec<_>>();
    let started = Instant::now();
    let read = store
        .read_hypa_embeddings(&keys)
        .expect("read the batch in one call");
    let batch_read_us = started.elapsed().as_micros() as u64;
    assert_eq!(read.len(), HYPA_ENTRIES);

    let started = Instant::now();
    for key in &keys {
        store
            .read_hypa_embeddings(std::slice::from_ref(key))
            .expect("read one key per call");
    }
    let per_key_read_us = started.elapsed().as_micros() as u64;

    HypaSample {
        batch_write_us,
        per_entry_write_us,
        batch_read_us,
        per_key_read_us,
    }
}

#[test]
#[ignore = "release-only embedding cache benchmark"]
fn hypa_embedding_cache_measurements() {
    let mut samples = (0..HYPA_RUNS).map(hypa_sample).collect::<Vec<_>>();
    samples.remove(0);

    let vector = deterministic_vector(0, HYPA_DIMENSIONS);
    let floats = vector
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect::<Vec<_>>();

    macro_rules! statistics {
        ($field:ident) => {
            nearest_rank(
                &samples
                    .iter()
                    .map(|sample| sample.$field)
                    .collect::<Vec<_>>(),
            )
        };
    }

    let result = HypaBenchmarkResult {
        schema_version: 1,
        benchmark: "hypa-embedding-cache-roundtrip",
        source_revision: std::env::var("RISUNEST_HYPA_BENCH_REVISION").ok(),
        entries: HYPA_ENTRIES,
        dimensions: HYPA_DIMENSIONS,
        vector_transport: "float32-little-endian",
        one_vector: HypaPayload {
            binary_bytes: vector.len(),
            json_number_array_bytes: serde_json::to_vec(&floats)
                .expect("serialize one vector as a JSON number array")
                .len(),
        },
        discarded_warmup_runs: 1,
        measured_runs: samples.len(),
        aggregate: HypaAggregate {
            batch_write_us: statistics!(batch_write_us),
            per_entry_write_us: statistics!(per_entry_write_us),
            batch_read_us: statistics!(batch_read_us),
            per_key_read_us: statistics!(per_key_read_us),
        },
        samples,
    };
    let encoded = serde_json::to_string(&result).expect("serialize benchmark result");
    if let Ok(path) = std::env::var("RISUNEST_HYPA_BENCH_OUTPUT") {
        std::fs::write(path, encoded.as_bytes()).expect("write benchmark result");
    }
    println!("{encoded}");
}
