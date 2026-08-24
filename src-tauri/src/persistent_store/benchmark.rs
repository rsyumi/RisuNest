use super::{
    CharacterQuery, CheckpointMode, ConversationMutation, ConversationQuery, PersistentStore,
    QueryOrder, WorkingSetCommit,
};
use rusqlite::{ffi, Connection};
use serde::Serialize;
use serde_json::{json, Value};
use std::{path::Path, time::Instant};

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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchmarkResult {
    schema_version: u32,
    benchmark: &'static str,
    source_revision: Option<String>,
    fixture: FixtureDescription,
    discarded_warmup_runs: usize,
    measured_runs: usize,
    write_metric: &'static str,
    aggregate: Aggregate,
    samples: Vec<Sample>,
    one_gib_diagnostic: Option<OneGibDiagnostic>,
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

struct ExportTraversal {
    acquire_revision_us: u64,
    traversal_us: u64,
    release_revision_us: u64,
    json_bytes: u64,
}

fn export_traversal(store: &mut PersistentStore) -> ExportTraversal {
    let revision = store.revision().expect("read export revision");
    let acquire_started = Instant::now();
    let lease = store
        .acquire_revision(revision)
        .expect("acquire export revision");
    let acquire_revision_us = elapsed_us(acquire_started);
    let traversal_started = Instant::now();
    let mut bytes = serde_json::to_vec(&store.read_root(Some(&lease.lease)).unwrap().value)
        .unwrap()
        .len() as u64;
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
                bytes += serde_json::to_vec(&detail).unwrap().len() as u64;
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
                root: None,
                character: None,
                replace_character: None,
                add_character: None,
                conversations: Some(vec![ConversationMutation::ReplaceRange {
                    character_id: "character-0000".to_owned(),
                    conversation_id: "chat-0000-00".to_owned(),
                    start: TURNS_PER_CHAT as i64,
                    delete_count: 0,
                    messages: vec![appended.clone()],
                    conversation: None,
                }]),
                delete_character_id: None,
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
    assert_fixture_shape(&exported, TURNS_PER_CHAT + 1, STRESS_TURNS);
    assert_eq!(
        exported["characters"][0]["chats"][0]["message"]
            .as_array()
            .unwrap()
            .len(),
        TURNS_PER_CHAT + 1
    );
    drop(exported);
    let export_materialize_us = elapsed_us(export_materialize_started);
    let export_traversal = export_traversal(&mut store);
    let export_total_us = elapsed_us(export_started);

    let snapshot_started = Instant::now();
    let snapshot = store
        .snapshot_create("phase3-step5")
        .expect("create benchmark snapshot");
    let snapshot_us = elapsed_us(snapshot_started);
    assert!(snapshot.bytes > 0);
    assert!(Path::new(&snapshot.path).is_file());
    assert_eq!(
        std::fs::metadata(&snapshot.path).unwrap().len(),
        snapshot.bytes
    );

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
            assert!(Path::new(&snapshot.path).is_file());
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

#[test]
#[ignore = "release-only Phase 3 Step 5 benchmark"]
fn phase3_step5_measurements() {
    assert_eq!(
        std::env::var("VITE_DISABLE_REALM").as_deref(),
        Ok("true"),
        "Phase 3 benchmarks require VITE_DISABLE_REALM=true"
    );
    let database = generate_save_large(
        NORMAL_CHARACTERS,
        CHATS_PER_CHARACTER,
        TURNS_PER_CHAT,
        STRESS_TURNS,
        STRESS_TEXT_BYTES,
    );
    let serialized = serde_json::to_vec(&database).expect("serialize save-large fixture");
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
        },
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
    use super::{generate_save_large, nearest_rank};

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
