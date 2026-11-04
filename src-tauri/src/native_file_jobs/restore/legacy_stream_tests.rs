// The streaming legacy decoder must stage exactly what the previous decoder,
// which built the whole MessagePack value first, staged. That decoder is kept
// here unchanged as the reference.

use super::*;
use crate::native_file_jobs::{JobControl, JobKind, JobRegistry};
use flate2::{write::GzEncoder, Compression, GzBuilder};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Default)]
struct RecordingSink {
    characters: Mutex<Vec<Value>>,
    batches: Mutex<Vec<usize>>,
    root: Mutex<Option<String>>,
    presets: Mutex<Option<Vec<Value>>>,
    source_read: Option<Arc<AtomicU64>>,
    read_at_batches: Mutex<Vec<u64>>,
    job: OnceLock<Arc<JobControl>>,
    details_at_batches: Mutex<Vec<Option<JobDetail>>>,
}

impl ReplacementSink for RecordingSink {
    fn begin(&self) -> StoreResult<StagingResult> {
        Ok(StagingResult {
            staging_id: "recorded".to_owned(),
        })
    }

    fn put_root(&self, _staging_id: &str, root: &Value) -> StoreResult<()> {
        *self.root.lock().unwrap() = Some(serde_json::to_string(root)?);
        Ok(())
    }

    fn put_presets(&self, _staging_id: &str, presets: &[Value]) -> StoreResult<()> {
        *self.presets.lock().unwrap() = Some(presets.to_vec());
        Ok(())
    }

    fn put_legacy_root(&self, _staging_id: &str, root: &RootSpool) -> StoreResult<()> {
        let mut encoded=Vec::new();
        root.write_root(&mut encoded,false)?;
        *self.root.lock().unwrap()=Some(String::from_utf8(encoded).unwrap());
        let mut presets=Vec::new();
        root.visit("botPresets",|_,value| { presets.push(value); Ok(()) })?;
        *self.presets.lock().unwrap()=Some(presets);
        Ok(())
    }

    fn add_characters(&self, _staging_id: &str, characters: &[Value]) -> StoreResult<()> {
        self.characters.lock().unwrap().extend_from_slice(characters);
        self.batches.lock().unwrap().push(characters.len());
        if let Some(read) = &self.source_read {
            self.read_at_batches
                .lock()
                .unwrap()
                .push(read.load(Ordering::Acquire));
        }
        if let Some(job) = self.job.get() {
            self.details_at_batches
                .lock()
                .unwrap()
                .push(job.status().detail);
        }
        Ok(())
    }

    fn commit(&self, _staging_id: &str, _expected_revision: i64, _binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
        Err(StoreError::Store {
            message: "the recording sink does not activate".to_owned(),
        })
    }

    fn abort(&self, _staging_id: &str) -> StoreResult<()> {
        Ok(())
    }
}

struct CountingSource<'a> {
    inner: &'a [u8],
    read: Arc<AtomicU64>,
}

impl Read for CountingSource<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.read.fetch_add(read as u64, Ordering::AcqRel);
        Ok(read)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decoder {
    WholeValue,
    Streaming,
}

struct LegacyRun {
    outcome: Result<(u64, u64), NativeJobError>,
    sink: RecordingSink,
    detail: Option<JobDetail>,
    progress: JobProgress,
}

fn run_legacy_source<S: Read>(
    source: S,
    total: u64,
    limits: RestoreLimits,
    decoder: Decoder,
    sink: RecordingSink,
) -> LegacyRun {
    let job = JobRegistry::default()
        .create(JobKind::RestoreBlockRisuSave)
        .unwrap();
    job.start(JobPhase::ReadingSource).unwrap();
    let _ = sink.job.set(job.clone());
    let outcome = (|| {
        let mut reader = TrackedReader::new(source, total, &*job);
        let format = read_risu_save_format(&mut reader)?;
        let staging_id = sink.begin().map_err(store_error)?.staging_id;
        let parsed = match (format, decoder) {
            (RisuSaveFormat::Block, _) => panic!("legacy input selected the block format"),
            (RisuSaveFormat::LegacyRaw | RisuSaveFormat::HistoricalPrefixed, Decoder::WholeValue) => {
                reference_parse_and_stage_legacy(&mut reader, &staging_id, &job, &sink, limits)?
            }
            (RisuSaveFormat::LegacyRaw | RisuSaveFormat::HistoricalPrefixed, Decoder::Streaming) => {
                parse_and_stage_legacy(&mut reader, &staging_id, &job, &sink, limits)?
            }
            (_, Decoder::WholeValue) => {
                reference_parse_and_stage_compressed_legacy(&mut reader, &staging_id, &job, &sink, limits)?
            }
            (_, Decoder::Streaming) => {
                parse_and_stage_compressed_legacy(&mut reader, &staging_id, &job, &sink, limits)?
            }
        };
        reader.require_eof()?;
        Ok((parsed.character_count, parsed.preset_count))
    })();
    let status = job.status();
    LegacyRun {
        outcome,
        sink,
        detail: status.detail,
        progress: status.progress,
    }
}

fn run_legacy(wire: &[u8], limits: RestoreLimits, decoder: Decoder) -> LegacyRun {
    run_legacy_source(wire, wire.len() as u64, limits, decoder, RecordingSink::default())
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder: GzEncoder<Vec<u8>> = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn legacy_wire(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = LEGACY_RISU_SAVE_PREFIX.to_vec();
    bytes.push(kind);
    bytes.extend_from_slice(payload);
    bytes
}

fn legacy_wires(payload: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let compressed = gzip(payload);
    let mut historical = HISTORICAL_RISU_PREFIX.to_vec();
    historical.extend_from_slice(payload);
    vec![
        ("raw", legacy_wire(7, payload)),
        ("historical", historical),
        ("compressed", legacy_wire(8, &compressed)),
        ("stream", legacy_wire(9, &compressed)),
    ]
}

pub(super) fn messagepack_from_json(value: &Value) -> MessagePackValue {
    match value {
        Value::Null => MessagePackValue::Nil,
        Value::Bool(value) => MessagePackValue::Boolean(*value),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                MessagePackValue::from(value)
            } else if let Some(value) = number.as_u64() {
                MessagePackValue::from(value)
            } else {
                MessagePackValue::F64(number.as_f64().unwrap())
            }
        }
        Value::String(value) => MessagePackValue::from(value.as_str()),
        Value::Array(values) => MessagePackValue::Array(values.iter().map(messagepack_from_json).collect()),
        Value::Object(entries) => MessagePackValue::Map(
            entries
                .iter()
                .map(|(key, value)| (MessagePackValue::from(key.as_str()), messagepack_from_json(value)))
                .collect(),
        ),
    }
}

pub(super) fn messagepack_bytes(value: &MessagePackValue) -> Vec<u8> {
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, value).unwrap();
    bytes
}

fn text(value: &str) -> MessagePackValue {
    MessagePackValue::from(value)
}

fn undefined() -> MessagePackValue {
    MessagePackValue::Ext(0, vec![0])
}

fn entry(key: &str, value: MessagePackValue) -> (MessagePackValue, MessagePackValue) {
    (text(key), value)
}

fn root_bytes(entries: Vec<(MessagePackValue, MessagePackValue)>) -> Vec<u8> {
    messagepack_bytes(&MessagePackValue::Map(entries))
}

// Incompressible enough that a gzip source is still read in many chunks.
fn noise(seed: u64, length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ALPHABET[(state & 63) as usize] as char
        })
        .collect()
}

fn golden_character(index: usize) -> MessagePackValue {
    let mut chats = vec![
        json!({
            "id": format!("chat-{index}"),
            "name": "Chat",
            "message": [
                { "role": "user", "data": format!("hello {index}"), "chatId": format!("message-{index}") },
                {
                    "role": "char", "data": "second", "chatId": format!("reply-{index}"),
                    "swipes": ["first", "second"], "swipeId": 1
                }
            ]
        }),
        json!({ "id": format!("chat-{index}"), "name": "Duplicate ID", "message": [] }),
    ];
    if index % 3 == 0 {
        chats.push(json!({ "name": "Without ID", "message": [{ "role": "user", "data": "kept" }] }));
    }
    let MessagePackValue::Map(mut entries) = messagepack_from_json(&json!({
        "type": "character",
        "chaId": format!("golden-{index}"),
        "name": format!("Golden {index}"),
        "chats": chats,
        "score": index as f64 + 0.25,
    })) else {
        unreachable!()
    };
    entries.extend([
        entry("note", undefined()),
        entry("10", text("ten")),
        entry("2", text("two")),
        entry("__proto__", text("character proto")),
        entry("created", MessagePackValue::Ext(-1, 1_700_000_000u32.to_be_bytes().to_vec())),
        entry("large", MessagePackValue::from(u64::MAX)),
        entry("half", MessagePackValue::F32(0.5)),
        entry("absent", MessagePackValue::Ext(-1, vec![0xff])),
    ]);
    MessagePackValue::Map(entries)
}

fn golden_characters(count: usize) -> MessagePackValue {
    MessagePackValue::Array((0..count).map(golden_character).collect())
}

// Root entries other than `characters`, including JavaScript map semantics
// that interact by key: index keys, `__proto__`, a duplicated key, and
// `undefined` before and after a value.
fn golden_other_entries() -> Vec<(MessagePackValue, MessagePackValue)> {
    vec![
        entry("username", text("First")),
        entry("10", text("root ten")),
        entry(
            "botPresets",
            messagepack_from_json(&json!([
                { "id": "preset-1", "name": "Preset", "temperature": 0.5 },
                { "id": "preset-2", "name": "Second" }
            ])),
        ),
        entry("later", undefined()),
        entry("__proto__", text("root proto")),
        entry("modules", messagepack_from_json(&json!([{ "id": "module-1", "name": "Module" }]))),
        entry("loadouts", messagepack_from_json(&json!([{ "id": "loadout-1", "name": "Loadout" }]))),
        entry("2", text("root two")),
        entry("dropped", text("removed by the later undefined")),
        entry("plugins", messagepack_from_json(&json!([{ "name": "plugin", "script": "" }]))),
        entry(
            "pluginCustomStorage",
            messagepack_from_json(&json!({ "plugin": { "enabled": true } })),
        ),
        entry("username", text("Last")),
        entry("later", text("defined later")),
        entry("dropped", undefined()),
        entry("account", messagepack_from_json(&json!({ "token": "synthetic" }))),
        entry(
            "roadmap14Unknown",
            MessagePackValue::Map(vec![
                entry("zeta", MessagePackValue::from(1)),
                entry("2", MessagePackValue::from(2)),
                entry("01", MessagePackValue::from(3)),
                entry("omitted", undefined()),
                entry("__proto__", text("nested proto")),
                entry("when", MessagePackValue::Ext(-1, vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2])),
            ]),
        ),
        entry("disableToggleBinding", MessagePackValue::Boolean(false)),
    ]
}

fn root_with_characters_at(position: usize, characters: MessagePackValue) -> Vec<u8> {
    let mut entries = golden_other_entries();
    entries.insert(position.min(entries.len()), entry("characters", characters));
    root_bytes(entries)
}

fn normalized_characters(characters: &[Value]) -> Vec<Value> {
    characters
        .iter()
        .cloned()
        .map(|mut character| {
            if character.get("chaId").and_then(Value::as_str).is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()) {
                character["chaId"] = json!("generated");
            }
            if let Some(chats) = character.get_mut("chats").and_then(Value::as_array_mut) {
                for chat in chats {
                    if chat
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
                    {
                        chat["id"] = json!("generated");
                    }
                }
            }
            character
        })
        .collect()
}

fn assert_same_outcome(label: &str, wire: &[u8], limits: RestoreLimits) -> (LegacyRun, LegacyRun) {
    let expected = run_legacy(wire, limits, Decoder::WholeValue);
    let actual = run_legacy(wire, limits, Decoder::Streaming);
    match (&expected.outcome, &actual.outcome) {
        (Ok(expected_counts), Ok(actual_counts)) => {
            assert_eq!(expected_counts, actual_counts, "{label}: counts");
            assert_eq!(
                normalized_characters(&expected.sink.characters.lock().unwrap()),
                normalized_characters(&actual.sink.characters.lock().unwrap()),
                "{label}: characters"
            );
            assert_eq!(
                *expected.sink.root.lock().unwrap(),
                *actual.sink.root.lock().unwrap(),
                "{label}: root"
            );
            assert_eq!(
                *expected.sink.presets.lock().unwrap(),
                *actual.sink.presets.lock().unwrap(),
                "{label}: presets"
            );
            assert_eq!(expected.detail, actual.detail, "{label}: final detail");
            assert_eq!(expected.progress, actual.progress, "{label}: final progress");
        }
        (Err(expected_error), Err(actual_error)) => {
            assert_eq!(expected_error.code, actual_error.code, "{label}: {actual_error}");
            assert_eq!(expected_error.message, actual_error.message, "{label}");
        }
        (expected_outcome, actual_outcome) => {
            panic!("{label}: whole-value decoder {expected_outcome:?}, streaming decoder {actual_outcome:?}")
        }
    }
    (expected, actual)
}

fn assert_same_outcome_for_every_wire(label: &str, payload: &[u8]) -> Vec<(LegacyRun, LegacyRun)> {
    legacy_wires(payload)
        .into_iter()
        .map(|(kind, wire)| assert_same_outcome(&format!("{label} ({kind})"), &wire, RestoreLimits::default()))
        .collect()
}

#[test]
fn legacy_restore_stages_characters_while_the_source_is_still_being_read() {
    let characters = (0..64)
        .map(|index| {
            messagepack_from_json(&json!({
                "type": "character",
                "chaId": format!("streamed-{index}"),
                "name": format!("Streamed {index}"),
                "chats": [{
                    "id": format!("chat-{index}"),
                    "message": [{ "role": "user", "data": noise(index as u64, 32 * 1024) }]
                }]
            }))
        })
        .collect();
    let payload = root_bytes(vec![
        entry("username", text("Streamed")),
        entry("characters", MessagePackValue::Array(characters)),
        entry("botPresets", MessagePackValue::Array(Vec::new())),
    ]);
    for (label, wire) in [
        ("raw", legacy_wire(7, &payload)),
        ("compressed", legacy_wire(8, &gzip(&payload))),
    ] {
        let read = Arc::new(AtomicU64::new(0));
        let sink = RecordingSink {
            source_read: Some(read.clone()),
            ..RecordingSink::default()
        };
        let total = wire.len() as u64;
        let run = run_legacy_source(
            CountingSource { inner: &wire, read },
            total,
            RestoreLimits::default(),
            Decoder::Streaming,
            sink,
        );
        assert_eq!(run.outcome.unwrap(), (64, 0), "{label}");
        let batches = run.sink.batches.lock().unwrap().clone();
        assert_eq!(batches.iter().sum::<usize>(), 64, "{label}");
        assert!(batches.iter().all(|count| *count <= CHARACTER_BATCH_COUNT), "{label}: {batches:?}");
        let read_at = run.sink.read_at_batches.lock().unwrap().clone();
        assert!(
            read_at[0] < total / 2,
            "{label}: the first characters were staged after reading {} of {total} source bytes",
            read_at[0]
        );
        assert!(read_at.windows(2).all(|pair| pair[0] < pair[1]), "{label}: {read_at:?}");
    }

    // A `bin` value is charged at its converted JSON text, so the first record
    // flushes alone and the second still shares a batch with the third.
    let binary = |id: &str, length: usize| {
        MessagePackValue::Map(vec![
            entry("type", text("character")),
            entry("chaId", text(id)),
            entry("name", text(id)),
            entry("chats", MessagePackValue::Array(Vec::new())),
            entry("blob", MessagePackValue::Binary(vec![255; length])),
        ])
    };
    let payload = root_bytes(vec![
        entry(
            "characters",
            MessagePackValue::Array(vec![binary("binary-0", 400_000), binary("binary-1", 400_000), binary("small", 0)]),
        ),
        entry("botPresets", MessagePackValue::Array(Vec::new())),
    ]);
    let run = run_legacy(&legacy_wire(7, &payload), RestoreLimits::default(), Decoder::Streaming);
    assert_eq!(run.outcome.unwrap(), (3, 0));
    assert_eq!(*run.sink.batches.lock().unwrap(), [1, 2]);
}

#[test]
fn legacy_restore_reports_character_progress_during_the_database_read() {
    let payload = root_with_characters_at(3, golden_characters(40));
    for (label, wire) in legacy_wires(&payload) {
        let run = run_legacy(&wire, RestoreLimits::default(), Decoder::Streaming);
        run.outcome.unwrap();
        let batches = run.sink.batches.lock().unwrap().clone();
        let details = run.sink.details_at_batches.lock().unwrap().clone();
        let mut staged = 0u64;
        for (count, detail) in batches.iter().zip(&details) {
            staged += *count as u64;
            let detail = detail.as_ref().expect("the database read reports detail");
            assert_eq!(detail.stage, JobStage::DecodingDatabase, "{label}");
            assert_eq!(detail.stage_unit, StageUnit::Bytes, "{label}");
            assert_eq!(detail.counts.characters_total, Some(40), "{label}");
            assert_eq!(detail.counts.characters, staged, "{label}");
        }
        let detail = run.detail.unwrap();
        assert_eq!(detail.stage, JobStage::FinalizingStaging, "{label}");
        assert_eq!(detail.counts.characters, 40, "{label}");
        assert_eq!(detail.counts.characters_total, Some(40), "{label}");
        assert_eq!(detail.counts.presets, 2, "{label}");
        assert_eq!(run.progress.completed_bytes, wire.len() as u64, "{label}");
        assert_eq!(run.progress.completed_items, 1, "{label}");
    }
}

#[test]
fn legacy_restore_keeps_source_character_total_when_records_are_excluded() {
    for retained in [0, 17] {
        let mut characters = vec![messagepack_from_json(&json!({
            "type": "group", "chaId": "synthetic-group", "name": "Synthetic group"
        }))];
        characters.extend((0..retained).map(golden_character));
        characters.push(messagepack_from_json(&json!({
            "type": "character", "chaId": "§temp", "name": "Synthetic temporary character"
        })));
        let total = characters.len() as u64;
        let payload = root_with_characters_at(3, MessagePackValue::Array(characters));
        for (label, wire) in legacy_wires(&payload) {
            let run = run_legacy(&wire, RestoreLimits::default(), Decoder::Streaming);
            assert_eq!(run.outcome.unwrap(), (retained as u64, 2), "{label}");
            let staged = run.sink.characters.lock().unwrap();
            assert_eq!(staged.len(), retained, "{label}");
            for (index, character) in staged.iter().enumerate() {
                assert_eq!(character["chaId"], format!("golden-{index}"), "{label}");
            }
            for detail in run.sink.details_at_batches.lock().unwrap().iter() {
                assert_eq!(detail.as_ref().unwrap().counts.characters_total, Some(total), "{label}");
            }
            let detail = run.detail.unwrap();
            assert_eq!(detail.stage, JobStage::FinalizingStaging, "{label}");
            assert_eq!(detail.counts.characters, retained as u64, "{label}");
            assert_eq!(detail.counts.characters_total, Some(total), "{label}");
        }
    }
}

#[test]
fn legacy_restore_matches_the_whole_value_decoder_wherever_characters_appear() {
    let others = golden_other_entries().len();
    for count in [0, 1, 17, 40] {
        for position in [0, 1, others / 2, others] {
            let payload = root_with_characters_at(position, golden_characters(count));
            for (expected, actual) in
                assert_same_outcome_for_every_wire(&format!("{count} characters at {position}"), &payload)
            {
                let staged = actual.sink.characters.lock().unwrap().len();
                assert_eq!(staged, count);
                assert_eq!(expected.sink.characters.lock().unwrap().len(), count);
                let root: Value = serde_json::from_str(&actual.sink.root.lock().unwrap().clone().unwrap()).unwrap();
                assert_eq!(root["username"], "Last");
                assert_eq!(root["later"], "defined later");
                assert!(root.get("dropped").is_none());
                assert!(root.get("account").is_none());
                assert!(root.get("characters").is_none());
                assert_eq!(
                    root.as_object().unwrap().keys().take(2).map(String::as_str).collect::<Vec<_>>(),
                    ["2", "10"]
                );
            }
        }
    }
}

#[test]
fn legacy_restore_matches_the_whole_value_decoder_for_msgpackr_fixtures() {
    use base64::Engine;

    let parity: Value = serde_json::from_str(include_str!(
        "../../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/msgpackr-parity-v1.json"
    ))
    .unwrap();
    for key in ["payloadBase64", "edgePayloadBase64", "unknownExtensionBase64"] {
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity[key].as_str().unwrap())
            .unwrap();
        assert_same_outcome_for_every_wire(key, &payload);
    }
    let historical = base64::engine::general_purpose::STANDARD
        .decode(parity["historicalPrefixedBase64"].as_str().unwrap())
        .unwrap();
    assert_same_outcome("historicalPrefixedBase64", &historical, RestoreLimits::default())
        .1
        .outcome
        .unwrap();
    for (label, fixture) in [
        (
            "risusave-raw-v4",
            include_str!("../../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-raw-v4.input.base64"),
        ),
        (
            "risusave-compressed-v4",
            include_str!("../../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-compressed-v4.input.base64"),
        ),
        (
            "risusave-stream-v4",
            include_str!("../../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-stream-v4.input.base64"),
        ),
    ] {
        let wire = base64::engine::general_purpose::STANDARD
            .decode(fixture.trim())
            .unwrap();
        assert_same_outcome(label, &wire, RestoreLimits::default())
            .1
            .outcome
            .unwrap();
    }
}

#[test]
fn legacy_restore_reads_long_length_markers_like_the_whole_value_decoder() {
    let character = messagepack_bytes(&golden_character(0));
    let presets = messagepack_bytes(&MessagePackValue::Array(Vec::new()));
    let mut payload = vec![0xdf, 0, 0, 0, 2];
    payload.extend(messagepack_bytes(&text("characters")));
    payload.extend([0xdd, 0, 0, 0, 1]);
    payload.extend(&character);
    payload.extend(messagepack_bytes(&text("botPresets")));
    payload.extend(&presets);
    assert_same_outcome_for_every_wire("map32 and array32", &payload);

    let mut payload = vec![0xde, 0, 2];
    payload.extend(messagepack_bytes(&text("botPresets")));
    payload.extend(&presets);
    payload.extend(messagepack_bytes(&text("characters")));
    payload.extend([0xdc, 0, 1]);
    payload.extend(&character);
    assert_same_outcome_for_every_wire("map16 and array16", &payload);
}

#[derive(Default)]
struct RootOnlySink {
    observed: Mutex<Option<(String,u64,u64,u64)>>,
}

impl ReplacementSink for RootOnlySink {
    fn begin(&self)->StoreResult<StagingResult> { Ok(StagingResult { staging_id:"root-only".into() }) }
    fn put_root(&self,_:&str,_:&Value)->StoreResult<()> { panic!("legacy root must stay streamed") }
    fn put_presets(&self,_:&str,_:&[Value])->StoreResult<()> { panic!("legacy presets must stay streamed") }
    fn add_characters(&self,_:&str,characters:&[Value])->StoreResult<()> { assert!(characters.is_empty()); Ok(()) }
    fn put_legacy_root(&self,_:&str,root:&RootSpool)->StoreResult<()> {
        struct HashWriter(Sha256,u64);
        impl Write for HashWriter {
            fn write(&mut self,bytes:&[u8])->io::Result<usize> { self.0.update(bytes); self.1+=bytes.len() as u64; Ok(bytes.len()) }
            fn flush(&mut self)->io::Result<()> { Ok(()) }
        }
        let mut writer=HashWriter(Sha256::new(),0);
        root.write_root(&mut writer,false)?;
        let (records,maximum)=root.member_storage_shape()?;
        *self.observed.lock().unwrap()=Some((hex::encode(writer.0.finalize()),writer.1,records,maximum));
        Ok(())
    }
    fn commit(&self,_:&str,_:i64,_:Option<&str>)->StoreResult<RevisionResult> { panic!("decoder fixture never activates") }
    fn abort(&self,_:&str)->StoreResult<()> { Ok(()) }
}

#[test]
fn root_heavy_legacy_input_uses_disk_members_and_streamed_sink_for_every_wire() {
    let records=(0..96).map(|index|messagepack_from_json(&json!({
        "id":format!("synthetic-{index}"),"name":format!("Synthetic {index}"),"body":noise(index,32*1024)
    }))).collect::<Vec<_>>();
    let storage=(0..64).map(|index|entry(&format!("key-{index}"),text(&noise(index+200,16*1024)))).collect();
    let payload=root_bytes(vec![
        entry("modules",MessagePackValue::Array(records.clone())),
        entry("unknownCollection",MessagePackValue::Array(records.clone())),
        entry("pluginCustomStorage",MessagePackValue::Map(storage)),
        entry("botPresets",MessagePackValue::Array(records)),
        entry("characters",MessagePackValue::Array(Vec::new())),
        entry("account",messagepack_from_json(&json!({"synthetic":"omitted"}))),
    ]);
    let reference=run_legacy(&legacy_wire(7,&payload),RestoreLimits::default(),Decoder::WholeValue);
    assert_eq!(reference.outcome.unwrap(),(0,96));
    let expected=reference.sink.root.lock().unwrap().clone().unwrap();
    let hash=hex::encode(Sha256::digest(expected.as_bytes()));
    for (kind,wire) in legacy_wires(&payload) {
        let job=JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let sink=RootOnlySink::default();
        let mut reader=TrackedReader::new(wire.as_slice(),wire.len() as u64,&*job);
        let format=read_risu_save_format(&mut reader).unwrap();
        let parsed=match format {
            RisuSaveFormat::LegacyRaw|RisuSaveFormat::HistoricalPrefixed => parse_and_stage_legacy(&mut reader,"root-only",&job,&sink,RestoreLimits::default()),
            _ => parse_and_stage_compressed_legacy(&mut reader,"root-only",&job,&sink,RestoreLimits::default()),
        }.unwrap();
        assert_eq!(parsed.preset_count,96,"{kind}");
        let (actual,bytes,records,maximum)=sink.observed.lock().unwrap().clone().unwrap();
        assert_eq!(actual,hash,"{kind}");
        assert_eq!(bytes,expected.len() as u64,"{kind}");
        assert!(bytes>6*1024*1024,"{kind}");
        assert!(records>=352,"{kind}");
        assert!(maximum<64*1024,"{kind}: largest independently staged record {maximum}");
    }
}

#[test]
fn root_slots_preserve_last_values_undefined_defaults_and_nested_key_order() {
    let payload=root_bytes(vec![
        entry("modules",MessagePackValue::Nil),
        entry("botPresets",messagepack_from_json(&json!([{"name":"discarded"}]))),
        entry("defaultToggleValues",text("overwritten")),
        entry("personas",messagepack_from_json(&json!([{"id":"duplicate"},{"id":"duplicate"}]))),
        entry("marker",text("kept")),
        entry("modules",undefined()), entry("botPresets",undefined()),
        entry("personas",MessagePackValue::Array(Vec::new())),
        entry("defaultToggleValues",messagepack_from_json(&json!({"toggle_synthetic":"1"}))),
        entry("pluginCustomStorage",MessagePackValue::Map(vec![
            entry("later",undefined()),entry("10",text("ten")),entry("2",text("two")),
            entry("__proto__",text("old")),entry("__proto_",text("last")),
            entry("later",text("value")),entry("removed",text("gone")),entry("removed",undefined()),
        ])),
        entry("characters",MessagePackValue::Array(Vec::new())),
    ]);
    assert_same_outcome_for_every_wire("disk slot replacements",&payload);
    for value in [MessagePackValue::Array(Vec::new()),text("invalid")] {
        let payload=root_bytes(vec![entry("characters",MessagePackValue::Array(Vec::new())),entry("pluginCustomStorage",value)]);
        for (reference,_) in assert_same_outcome_for_every_wire("invalid plugin root shape",&payload) {
            assert_eq!(reference.outcome.unwrap_err().code,"invalid-input");
        }
    }
}

#[test]
fn cancellation_during_root_sink_finalization_aborts_stage_and_owned_spool() {
    struct CancelRootSink {
        store:Mutex<crate::persistent_store::PersistentStore>,
        job:Arc<JobControl>,
        aborted:AtomicU64,
    }
    impl ReplacementSink for CancelRootSink {
        fn begin(&self)->StoreResult<StagingResult> { self.store.lock().unwrap().replace_begin() }
        fn put_root(&self,_:&str,_:&Value)->StoreResult<()> { panic!("aggregate legacy root") }
        fn put_presets(&self,_:&str,_:&[Value])->StoreResult<()> { panic!("aggregate legacy presets") }
        fn add_characters(&self,_:&str,values:&[Value])->StoreResult<()> { assert!(values.is_empty()); Ok(()) }
        fn put_legacy_root(&self,id:&str,root:&RootSpool)->StoreResult<()> {
            assert!(root.count("modules")?>0);
            self.job.request_cancel().unwrap();
            self.store.lock().unwrap().replace_put_upstream_stream(id,root)
        }
        fn commit(&self,_:&str,_:i64,_:Option<&str>)->StoreResult<RevisionResult> { panic!("cancelled stage activated") }
        fn abort(&self,id:&str)->StoreResult<()> {
            self.aborted.fetch_add(1,Ordering::AcqRel);
            self.store.lock().unwrap().replace_abort(id)
        }
    }
    for (kind,wire) in legacy_wires(&root_bytes(vec![
        entry("characters",MessagePackValue::Array(Vec::new())),
        entry("modules",messagepack_from_json(&json!([{"id":"module","body":"synthetic"}]))),
    ])) {
        let directory=tempfile::tempdir().unwrap();
        let owned=directory.path().join("job"); std::fs::create_dir(&owned).unwrap();
        let job=JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let sink=CancelRootSink { store:Mutex::new(crate::persistent_store::PersistentStore::open(directory.path()).unwrap()),job:job.clone(),aborted:AtomicU64::new(0) };
        let before=sink.store.lock().unwrap().read_root(None).unwrap().value;
        let error=restore_risu_save_reader_controlled(wire.as_slice(),wire.len() as u64,0,&job,&sink,RestoreLimits::default(),true,
            RestoreProgressScale { spool_directory:Some(owned.clone()),..Default::default() }).unwrap_err();
        assert_eq!(error.code,"cancelled","{kind}");
        assert_eq!(sink.aborted.load(Ordering::Acquire),1,"{kind}");
        let store=sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(),0,"{kind}");
        assert_eq!(store.read_root(None).unwrap().value,before,"{kind}");
        assert_eq!(std::fs::read_dir(&owned).unwrap().count(),0,"{kind}");
    }
}

#[test]
fn legacy_restore_rejects_what_the_whole_value_decoder_rejects() {
    let character = golden_character(0);
    let presets = entry("botPresets", MessagePackValue::Array(Vec::new()));
    let characters = |value: MessagePackValue| root_bytes(vec![entry("characters", value), presets.clone()]);
    let mut cases: Vec<(&str, Vec<u8>)> = vec![
        ("missing characters", root_bytes(vec![presets.clone()])),
        ("empty root", root_bytes(Vec::new())),
        ("nil characters", characters(MessagePackValue::Nil)),
        ("map characters", characters(MessagePackValue::Map(Vec::new()))),
        ("string characters", characters(text("characters"))),
        ("undefined characters", characters(undefined())),
        (
            "invalid timestamp in a character",
            characters(MessagePackValue::Array(vec![MessagePackValue::Map(vec![
                entry("chaId", text("timestamp")),
                entry("data", MessagePackValue::Ext(-1, vec![0; 5])),
            ])])),
        ),
        (
            "integer key before characters",
            root_bytes(vec![
                (MessagePackValue::from(1), text("one")),
                entry("characters", MessagePackValue::Array(vec![character.clone()])),
                presets.clone(),
            ]),
        ),
        (
            "integer key after characters",
            root_bytes(vec![
                entry("characters", MessagePackValue::Array(vec![character.clone()])),
                (MessagePackValue::from(1), text("one")),
                presets.clone(),
            ]),
        ),
        (
            "binary characters key",
            root_bytes(vec![
                (MessagePackValue::Binary(b"characters".to_vec()), MessagePackValue::Array(Vec::new())),
                presets.clone(),
            ]),
        ),
        (
            "botPresets not an array",
            root_bytes(vec![
                entry("characters", MessagePackValue::Array(vec![character.clone()])),
                entry("botPresets", text("presets")),
            ]),
        ),
        (
            "pluginCustomStorage not an object",
            root_bytes(vec![
                entry("pluginCustomStorage", MessagePackValue::Array(Vec::new())),
                entry("characters", MessagePackValue::Array(vec![character.clone()])),
            ]),
        ),
        (
            "modules not an array",
            root_bytes(vec![
                entry("characters", MessagePackValue::Array(vec![character.clone()])),
                entry("modules", text("modules")),
            ]),
        ),
        ("array root", messagepack_bytes(&MessagePackValue::Array(vec![character.clone()]))),
        ("string root", messagepack_bytes(&text("database"))),
        ("nil root", messagepack_bytes(&MessagePackValue::Nil)),
        ("undefined root", messagepack_bytes(&undefined())),
        ("reserved marker root", vec![0xc1]),
        (
            "array root with an unknown extension",
            messagepack_bytes(&MessagePackValue::Array(vec![MessagePackValue::Ext(42, vec![1])])),
        ),
        ("empty payload", Vec::new()),
        ("map16 length cut", vec![0xde, 0]),
        ("map32 length cut", vec![0xdf, 0, 0]),
        ("reserved marker characters", {
            let mut bytes = vec![0x82];
            bytes.extend(messagepack_bytes(&text("characters")));
            bytes.push(0xc1);
            bytes.extend(messagepack_bytes(&text("botPresets")));
            bytes.extend(messagepack_bytes(&MessagePackValue::Array(Vec::new())));
            bytes
        }),
        ("array32 claims more characters than present", {
            let mut bytes = vec![0x81];
            bytes.extend(messagepack_bytes(&text("characters")));
            bytes.extend([0xdd, 0xff, 0xff, 0xff, 0xff]);
            bytes.extend(messagepack_bytes(&character));
            bytes
        }),
    ];
    let complete = root_with_characters_at(4, golden_characters(3));
    let mut trailing = complete.clone();
    trailing.push(0);
    cases.push(("trailing data", trailing));
    let mut array_trailing = messagepack_bytes(&MessagePackValue::Array(Vec::new()));
    array_trailing.push(0);
    cases.push(("array root with trailing data", array_trailing));
    for (label, payload) in &cases {
        for (expected, actual) in assert_same_outcome_for_every_wire(label, payload) {
            assert!(expected.outcome.is_err(), "{label}: accepted {:?}", expected.outcome);
            assert!(actual.outcome.is_err(), "{label}");
        }
    }
    // The store rejects these when it stages them; the decoders pass them on alike.
    for (label, element) in [("character undefined", undefined()), ("character not a map", text("character"))] {
        assert_same_outcome_for_every_wire(label, &characters(MessagePackValue::Array(vec![element])));
    }
}

#[test]
fn legacy_restore_normalizes_shapes_pocketrisu_stores_or_loads() {
    let character = messagepack_from_json(&json!({
        "chaId": "pocket", "name": "Pocket", "type": "character",
        "chats": [
            null,
            {
                "id": 7, "bindedPersona": null,
                "savedToggleValues": { "toggle_kept": "1", "other": "x", "toggle_number": 2 },
                "message": [{ "role": "char", "data": "kept", "swipes": ["kept", null], "swipeId": 0.5 }]
            },
            "not a chat",
            { "id": "second", "savedToggleValues": null, "message": [{ "role": "char", "swipes": ["only"] }] }
        ]
    }));
    let unnamed = messagepack_from_json(&json!({
        "name": null, "type": "character", "chats": [{ "id": "unnamed", "message": [] }]
    }));
    let duplicate = messagepack_from_json(&json!({ "chaId": "pocket", "name": "Copy", "type": "character", "chats": [] }));
    let mut entries = vec![entry("characters", MessagePackValue::Array(vec![MessagePackValue::Nil, character, unnamed, duplicate]))];
    for key in ["botPresets", "modules", "loadouts", "plugins", "pluginCustomStorage", "disableToggleBinding", "defaultToggleValues", "defaultToggleUnsetKeys"] {
        entries.push(entry(key, MessagePackValue::Nil));
    }
    entries.push(entry("togglePresets", messagepack_from_json(&json!([
        { "name": "Imported", "values": { "toggle_a": "1", "custom": "2" } },
        { "values": {} },
        { "name": "No values" }
    ]))));
    entries.push(entry("personas", messagepack_from_json(&json!([{ "id": "same" }, { "id": "same" }, { "id": 3 }]))));
    for (_, actual) in assert_same_outcome_for_every_wire("PocketRisu shapes", &root_bytes(entries)) {
        assert!(actual.outcome.is_ok(), "{:?}", actual.outcome.as_ref().err());
        let root: Value = serde_json::from_str(actual.sink.root.lock().unwrap().as_deref().unwrap()).unwrap();
        assert!(root.get("disableToggleBinding").is_none());
        assert!(root.get("defaultToggleValues").is_none());
        assert_eq!(root["defaultToggleUnsetKeys"], Value::Null);
        for key in ["modules", "loadouts", "plugins"] {
            assert_eq!(root[key], json!([]), "{key}");
        }
        assert_eq!(root["pluginCustomStorage"], json!({}));
        assert_eq!(*actual.sink.presets.lock().unwrap(), Some(Vec::new()));
        assert_eq!(root["togglePresets"], json!([
            { "name": "Imported", "values": { "toggle_a": "1" } },
            { "name": "No values", "values": {} }
        ]));
        assert_eq!(root["personas"].as_array().unwrap().len(), 3);
        let characters = actual.sink.characters.lock().unwrap();
        assert_eq!(characters.len(), 3);
        assert_eq!(characters[0]["chaId"], "pocket");
        assert!(uuid::Uuid::parse_str(characters[1]["chaId"].as_str().unwrap()).is_ok());
        assert_eq!(characters[1]["name"], "");
        assert_eq!(characters[1]["chats"][0]["name"], "");
        assert!(uuid::Uuid::parse_str(characters[2]["chaId"].as_str().unwrap()).is_ok());
        assert_eq!(characters[2]["name"], "Copy");
        let chats = characters[0]["chats"].as_array().unwrap();
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0]["id"], "7");
        assert_eq!(chats[0]["savedToggleValues"], json!({ "toggle_kept": "1" }));
        assert!(chats[0].get("bindedPersona").is_none());
        let kept = &chats[0]["message"][0]["responseVariants"];
        assert_eq!(kept["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(kept["candidates"][0]["messages"][0]["data"], "kept");
        assert!(chats[1].get("savedToggleValues").is_none());
        let only = &chats[1]["message"][0];
        assert_eq!(only["data"], "only");
        assert_eq!(only["responseVariants"]["selectedId"], "second:response:0:0");
    }
}

#[test]
fn legacy_restore_rejects_every_truncation_like_the_whole_value_decoder() {
    let payload = root_bytes(vec![
        entry("username", text("Cut")),
        entry("characters", golden_characters(2)),
        entry("botPresets", messagepack_from_json(&json!([{ "id": "preset", "name": "Preset" }]))),
    ]);
    for length in 0..payload.len() {
        let cut = &payload[..length];
        for (kind, wire) in [("raw", legacy_wire(7, cut)), ("compressed", legacy_wire(8, &gzip(cut)))] {
            let (expected, _) = assert_same_outcome(&format!("cut at {length} ({kind})"), &wire, RestoreLimits::default());
            assert!(expected.outcome.is_err(), "cut at {length} ({kind})");
        }
    }
}

#[test]
fn legacy_restore_rejects_a_duplicate_characters_key_the_whole_value_decoder_accepted() {
    let payload = root_bytes(vec![
        entry("characters", MessagePackValue::Array(vec![golden_character(0)])),
        entry("username", text("Duplicate")),
        entry("characters", MessagePackValue::Array(vec![golden_character(1)])),
        entry("botPresets", MessagePackValue::Array(Vec::new())),
    ]);
    for (label, wire) in legacy_wires(&payload) {
        let expected = run_legacy(&wire, RestoreLimits::default(), Decoder::WholeValue);
        assert_eq!(expected.outcome.unwrap(), (1, 0), "{label}");
        assert_eq!(
            expected.sink.characters.lock().unwrap()[0]["chaId"],
            "golden-1",
            "{label}: the whole-value decoder kept the last characters"
        );
        let error = run_legacy(&wire, RestoreLimits::default(), Decoder::Streaming)
            .outcome
            .unwrap_err();
        assert_eq!(error.code, "corrupt-input", "{label}");
        assert_eq!(
            error.message, "legacy MessagePack database has a duplicate characters key",
            "{label}"
        );
    }
}

#[test]
fn legacy_restore_applies_the_whole_value_depth_limit() {
    // Unoptimized recursion over a thousand levels needs more than the test thread's stack.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(assert_depth_limit_parity)
        .unwrap()
        .join()
        .unwrap();
}

fn assert_depth_limit_parity() {
    fn nested(levels: usize) -> MessagePackValue {
        (0..levels).fold(MessagePackValue::Array(Vec::new()), |inner, _| {
            MessagePackValue::Array(vec![inner])
        })
    }
    for (label, placement) in [("character", true), ("root entry", false)] {
        let mut accepted = 0;
        let mut rejected = 0;
        for levels in 500..=515 {
            let deep = nested(levels);
            let payload = if placement {
                root_bytes(vec![
                    entry(
                        "characters",
                        MessagePackValue::Array(vec![MessagePackValue::Map(vec![
                            entry("chaId", text("deep")),
                            entry("deep", deep),
                        ])]),
                    ),
                    entry("botPresets", MessagePackValue::Array(Vec::new())),
                ])
            } else {
                root_bytes(vec![
                    entry("deep", deep),
                    entry("characters", MessagePackValue::Array(Vec::new())),
                ])
            };
            let (expected, _) = assert_same_outcome(
                &format!("{label} nested {levels} levels"),
                &legacy_wire(7, &payload),
                RestoreLimits::default(),
            );
            match expected.outcome {
                Ok(_) => accepted += 1,
                Err(error) => {
                    assert_eq!(error.message, "invalid legacy MessagePack: depth limit exceeded");
                    rejected += 1;
                }
            }
        }
        assert!(accepted > 0 && rejected > 0, "{label}: {accepted} accepted, {rejected} rejected");
    }
}

#[test]
fn legacy_restore_keeps_the_decoded_size_limit() {
    assert_eq!(RestoreLimits::default().max_decoded_block_bytes, 1024 * 1024 * 1024);
    let payload = root_with_characters_at(2, golden_characters(3));
    let decoded = payload.len() as u64;
    for (kind, wire) in legacy_wires(&payload) {
        for (limit, accepted) in [(decoded, true), (decoded - 1, false), (decoded / 2, false), (8, false)] {
            let limits = RestoreLimits {
                max_encoded_block_bytes: u32::MAX as u64,
                max_decoded_block_bytes: limit,
                ..Default::default()
            };
            let (expected, _) = assert_same_outcome(&format!("{kind} limit {limit}"), &wire, limits);
            assert_eq!(expected.outcome.is_ok(), accepted, "{kind} limit {limit}");
            if let Err(error) = expected.outcome {
                assert_eq!(error.code, "payload-too-large");
                assert!(error.message.contains("limit exceeded"), "{error}");
            }
        }
    }

    // A raw database over the default limit is refused before any of it is read.
    let wire = legacy_wire(7, &payload);
    let total = LEGACY_RISU_SAVE_PREFIX.len() as u64 + 1 + RestoreLimits::default().max_decoded_block_bytes + 1;
    for decoder in [Decoder::WholeValue, Decoder::Streaming] {
        let read = Arc::new(AtomicU64::new(0));
        let run = run_legacy_source(
            CountingSource { inner: &wire, read: read.clone() },
            total,
            RestoreLimits::default(),
            decoder,
            RecordingSink::default(),
        );
        let error = run.outcome.unwrap_err();
        assert_eq!(error.code, "payload-too-large", "{decoder:?}");
        assert_eq!(error.message, "decoded legacy RisuSave limit exceeded", "{decoder:?}");
        assert_eq!(read.load(Ordering::Acquire), LEGACY_RISU_SAVE_PREFIX.len() as u64 + 1);
    }
}

#[test]
fn legacy_restore_checks_the_gzip_trailer_like_the_whole_value_decoder() {
    let payload = root_with_characters_at(5, golden_characters(4));
    let compressed = gzip(&payload);
    let mut with_garbage = compressed.clone();
    with_garbage.extend_from_slice(b"garbage");
    let mut concatenated = compressed.clone();
    concatenated.extend_from_slice(&gzip(&payload));
    let mut bad_checksum = compressed.clone();
    let checksum = bad_checksum.len() - 8;
    bad_checksum[checksum] ^= 0xff;
    let mut bad_length = compressed.clone();
    let length = bad_length.len() - 1;
    bad_length[length] ^= 0xff;
    let mut cut_trailer = compressed.clone();
    cut_trailer.truncate(cut_trailer.len() - 3);
    let mut decoded_trailing = payload.clone();
    decoded_trailing.push(0xc0);
    let non_map = messagepack_bytes(&MessagePackValue::Array(Vec::new()));
    let mut non_map_with_garbage = gzip(&non_map);
    non_map_with_garbage.extend_from_slice(b"garbage");
    for (label, gzip_payload, expected_message) in [
        ("garbage after the member", with_garbage, "trailing data in legacy RisuSave gzip stream"),
        ("concatenated member", concatenated, "trailing data in legacy RisuSave gzip stream"),
        ("bad checksum", bad_checksum, "invalid legacy MessagePack"),
        ("bad length", bad_length, "invalid legacy MessagePack"),
        ("cut trailer", cut_trailer, "truncated legacy MessagePack payload"),
        ("decoded trailing data", gzip(&decoded_trailing), "trailing data after legacy MessagePack value"),
        ("non-map root with garbage", non_map_with_garbage, "trailing data in legacy RisuSave gzip stream"),
    ] {
        for kind in [8, 9] {
            let (expected, _) = assert_same_outcome(
                &format!("{label} ({kind})"),
                &legacy_wire(kind, &gzip_payload),
                RestoreLimits::default(),
            );
            let error = expected.outcome.unwrap_err();
            assert!(error.message.contains(expected_message), "{label}: {error}");
        }
    }
}

fn reference_parse_and_stage_legacy<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let remaining = reader.total.saturating_sub(reader.completed);
    if remaining > limits.max_decoded_block_bytes {
        return Err(limit_error("decoded legacy RisuSave limit exceeded"));
    }
    reader.set_stage(JobStage::DecodingDatabase)?;
    let source = RemainingSourceReader { reader, remaining };
    let decoded = DecodedLimitReader::new(
        source,
        limits.max_decoded_block_bytes,
        job,
        "legacy RisuSave",
    );
    let value = reference_decode_messagepack(decoded, job)?.0;
    reader.complete_item()?;
    reference_stage_legacy_database(value, staging_id, job, reader, sink)
}

fn reference_parse_and_stage_compressed_legacy<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let remaining = reader.total.saturating_sub(reader.completed);
    if remaining > limits.max_encoded_block_bytes {
        return Err(limit_error("encoded legacy RisuSave limit exceeded"));
    }
    reader.set_stage(JobStage::DecodingDatabase)?;
    let source = RemainingSourceReader { reader, remaining };
    let buffered = BufReader::with_capacity(READ_CHUNK_BYTES, source);
    let decoder = GzDecoder::new(buffered);
    let decoded = DecodedLimitReader::new(
        decoder,
        limits.max_decoded_block_bytes,
        job,
        "legacy RisuSave",
    );
    let (value, decoded) = reference_decode_messagepack(decoded, job)?;
    let decoder = decoded.into_inner();
    let buffered = decoder.into_inner();
    if !buffered.buffer().is_empty() || buffered.get_ref().remaining != 0 {
        return Err(corrupt("trailing data in legacy RisuSave gzip stream"));
    }
    reader.complete_item()?;
    reference_stage_legacy_database(value, staging_id, job, reader, sink)
}

fn reference_decode_messagepack<'a, R: Read>(
    decoded: DecodedLimitReader<'a, R>,
    job: &JobControl,
) -> Result<(Value, DecodedLimitReader<'a, R>), NativeJobError> {
    let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    let packed =
        rmpv::decode::read_value(&mut buffered).map_err(|error| messagepack_error(error, job))?;
    let mut trailing = [0u8; 1];
    let read = buffered
        .read(&mut trailing)
        .map_err(|error| messagepack_io_error(error, job))?;
    if read != 0 {
        return Err(corrupt("trailing data after legacy MessagePack value"));
    }
    let value = match messagepack_to_json(packed, job)? {
        JsonSlot::Value(value) => value,
        JsonSlot::Undefined => {
            return Err(invalid("legacy MessagePack database cannot be undefined"))
        }
    };
    Ok((value, buffered.into_inner()))
}

fn reference_stage_legacy_database<R: Read>(
    value: Value,
    staging_id: &str,
    job: &JobControl,
    reader: &mut TrackedReader<'_, R>,
    sink: &dyn ReplacementSink,
) -> Result<ParsedCounts, NativeJobError> {
    let mut root = match value {
        Value::Object(root) => root,
        _ => return Err(invalid("legacy MessagePack database must be an object")),
    };
    let characters = match root.shift_remove("characters") {
        Some(Value::Array(characters)) => characters,
        _ => return Err(invalid("legacy MessagePack characters must be an array")),
    };
    let character_total = characters.len() as u64;
    let mut character_ids = HashSet::new();
    let mut characters: Vec<Value> = characters
        .into_iter()
        .enumerate()
        .filter_map(|(index, mut character)| {
            if !prepare_legacy_character(&mut character, &mut character_ids) {
                return None;
            }
            pocket_features::character(&mut character, &format!("character:{index}"));
            Some(character)
        })
        .collect();
    assign_legacy_chat_ids(&mut characters);
    for key in NULLABLE_ROOT_COLLECTIONS {
        if root.get(key).is_some_and(Value::is_null) {
            root.shift_remove(key);
        }
    }
    let presets = match root.shift_remove("botPresets") {
        Some(Value::Array(presets)) => presets,
        Some(_) => return Err(invalid("legacy MessagePack botPresets must be an array")),
        None => Vec::new(),
    };
    if let Some(storage) = root.get("pluginCustomStorage") {
        if !storage.is_object() {
            return Err(invalid(
                "legacy MessagePack pluginCustomStorage must be an object",
            ));
        }
    }
    for key in ["modules", "loadouts", "plugins"] {
        if root.get(key).is_some_and(|value| !value.is_array()) {
            return Err(invalid(format!(
                "legacy MessagePack {key} must be an array"
            )));
        }
        root.entry(key.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
    root.entry("pluginCustomStorage".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));

    reader.counts.characters_total = Some(character_total);
    reader.counts.presets = presets.len() as u64;
    let mut start = 0;
    while start < characters.len() {
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled while staging legacy database"));
        }
        let mut end = start;
        let mut bytes = 0usize;
        while end < characters.len() && end - start < CHARACTER_BATCH_COUNT {
            let character_bytes = serde_json::to_vec(&characters[end])
                .map_err(|error| corrupt(format!("legacy character is not JSON: {error}")))?
                .len();
            if end > start && bytes.saturating_add(character_bytes) > CHARACTER_BATCH_BYTES {
                break;
            }
            bytes = bytes.saturating_add(character_bytes);
            end += 1;
        }
        sink.add_characters(staging_id, &characters[start..end])
            .map_err(store_error)?;
        start = end;
        reader.counts.characters = start as u64;
    }

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(|error| job_error(job, error))?;
    reader.report_stage_items(JobStage::FinalizingStaging, 0, None)?;
    pocket_features::root(&mut root);
    root.shift_remove("account");
    sink.put_root(staging_id, &Value::Object(root))
        .map_err(store_error)?;
    sink.put_presets(staging_id, &presets)
        .map_err(store_error)?;
    Ok(ParsedCounts {
        character_count: characters.len() as u64,
        preset_count: presets.len() as u64,
    })
}
