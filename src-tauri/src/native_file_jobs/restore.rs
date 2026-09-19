#[path = "pocket_features.rs"]
pub(super) mod pocket_features;
use super::{
    ImportCounts, JobControl, JobDetail, JobPhase, JobProgress, JobResultSummary, JobStage,
    NativeJobError, OpenedJobSource, StageUnit,
};
use crate::persistent_store::{RevisionResult, StagingResult, StoreError, StoreResult};
use flate2::bufread::GzDecoder;
use rmpv::Value as MessagePackValue;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

const RISU_SAVE_HEADER: &[u8] = b"RISUSAVE\0";
const LEGACY_RISU_SAVE_PREFIX: &[u8] = b"\0RISUSAVE\0";
const HISTORICAL_RISU_PREFIX: &[u8] = b"\0\0RISU";
const READ_CHUNK_BYTES: usize = 64 * 1024;
const CHARACTER_BATCH_COUNT: usize = 16;
const CHARACTER_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MESSAGE_PAGE_COUNT: usize = 128;
const MESSAGE_PAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct RestoreLimits {
    pub(crate) max_encoded_block_bytes: u64,
    pub(crate) max_decoded_block_bytes: u64,
}

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_encoded_block_bytes: u32::MAX as u64,
            max_decoded_block_bytes: 1024 * 1024 * 1024,
        }
    }
}

pub(crate) trait ReplacementSink: Send + Sync {
    fn begin(&self) -> StoreResult<StagingResult>;
    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()>;
    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()>;
    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()>;
    fn supports_incremental_characters(&self) -> bool {
        false
    }
    fn put_character_detail(
        &self,
        _staging_id: &str,
        _detail: &Value,
        _conversation_count: i64,
    ) -> StoreResult<()> {
        Err(StoreError::Store {
            message: "incremental character staging is unavailable".to_owned(),
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn put_conversation_row(
        &self,
        _staging_id: &str,
        _character_id: &str,
        _configured_index: i64,
        _detail: &Value,
        _recent_at: i64,
        _message_count: i64,
    ) -> StoreResult<()> {
        Err(StoreError::Store {
            message: "incremental conversation staging is unavailable".to_owned(),
        })
    }
    fn add_conversation_messages(
        &self,
        _staging_id: &str,
        _character_id: &str,
        _conversation_id: &str,
        _start: i64,
        _messages: &[Value],
    ) -> StoreResult<()> {
        Err(StoreError::Store {
            message: "incremental message staging is unavailable".to_owned(),
        })
    }
    fn preserve_active_repositories(
        &self,
        _staging_id: &str,
        _expected_revision: i64,
    ) -> StoreResult<()> {
        Ok(())
    }
    /// Values the staged save left without an owner. A file RisuNest wrote
    /// carries ownership, so this is empty for it.
    fn staged_plugin_preview(
        &self,
        _staging_id: &str,
    ) -> StoreResult<crate::persistent_store::commit::StagedPluginPreview> {
        Ok(crate::persistent_store::commit::StagedPluginPreview::default())
    }
    fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult>;
    fn abort(&self, staging_id: &str) -> StoreResult<()>;
}

pub(crate) trait RestoreControl {
    fn is_cancel_requested(&self) -> bool;
    fn set_phase(&self, phase: JobPhase) -> Result<(), String>;
    fn set_progress(&self, progress: JobProgress) -> Result<(), String>;
    fn set_detail(&self, detail: JobDetail) -> Result<(), String>;
}

impl RestoreControl for JobControl {
    fn is_cancel_requested(&self) -> bool {
        JobControl::is_cancel_requested(self)
    }

    fn set_phase(&self, phase: JobPhase) -> Result<(), String> {
        JobControl::set_phase(self, phase)
    }

    fn set_progress(&self, progress: JobProgress) -> Result<(), String> {
        JobControl::set_progress(self, progress)
    }

    fn set_detail(&self, detail: JobDetail) -> Result<(), String> {
        JobControl::set_detail(self, detail)
    }
}

/// How the database read maps onto the job's top-level progress. A plain
/// RisuSave restore reports the file itself; a legacy backup restore has
/// already spent part of a larger budget reading and preparing the archive,
/// so it offsets the database bytes and keeps the archive's entry count.
#[derive(Debug, Clone, Default)]
pub(crate) struct RestoreProgressScale {
    /// Bytes already counted before the database read started.
    pub(crate) base_bytes: u64,
    /// Fixed total for the whole job; `None` uses the database size.
    pub(crate) total_bytes: Option<u64>,
    /// Keeps the top-level item counter at this value instead of counting blocks.
    pub(crate) fixed_items: Option<(u64, Option<u64>)>,
    /// Counters gathered before the database read, carried into every report.
    pub(crate) counts: ImportCounts,
}

pub(crate) fn restore_block_risu_save(
    mut source: OpenedJobSource,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
) -> Result<JobResultSummary, NativeJobError> {
    if super::raw_recovery::is_raw_recovery_archive(&mut source.file)? {
        return Err(NativeJobError::new(
            "rescue-format-not-restorable",
            "RisuNest rescue archives cannot be imported or restored",
        ));
    }
    restore_risu_save_reader_controlled(
        source.file,
        source.total_bytes,
        expected_revision,
        job,
        sink,
        RestoreLimits::default(),
        true,
        RestoreProgressScale::default(),
    )
}

pub(crate) fn restore_started_risu_save(
    source: OpenedJobSource,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    scale: RestoreProgressScale,
) -> Result<JobResultSummary, NativeJobError> {
    restore_risu_save_reader_controlled(
        source.file,
        source.total_bytes,
        expected_revision,
        job,
        sink,
        RestoreLimits::default(),
        false,
        scale,
    )
}

#[cfg(test)]
pub(crate) fn restore_risu_save(
    source: &std::path::Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
) -> Result<JobResultSummary, NativeJobError> {
    let source = super::open_regular_file_no_follow(source)?;
    restore_risu_save_reader(
        source.file,
        source.total_bytes,
        expected_revision,
        job,
        sink,
        RestoreLimits::default(),
    )
}

#[cfg(test)]
fn restore_risu_save_with_limits(
    source: &std::path::Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    let source = super::open_regular_file_no_follow(source)?;
    restore_risu_save_reader(
        source.file,
        source.total_bytes,
        expected_revision,
        job,
        sink,
        limits,
    )
}

#[cfg(test)]
pub(crate) fn restore_block_risu_save_with_limits(
    source: OpenedJobSource,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    restore_block_risu_save_reader(
        source.file,
        source.total_bytes,
        expected_revision,
        job,
        sink,
        limits,
    )
}

#[cfg(test)]
fn restore_block_risu_save_path(
    source: &std::path::Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
) -> Result<JobResultSummary, NativeJobError> {
    let source = super::open_regular_file_no_follow(source)?;
    restore_block_risu_save(source, expected_revision, job, sink)
}

#[cfg(test)]
fn restore_block_risu_save_path_with_limits(
    source: &std::path::Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    let source = super::open_regular_file_no_follow(source)?;
    restore_block_risu_save_with_limits(source, expected_revision, job, sink, limits)
}

#[cfg(test)]
fn restore_block_risu_save_reader<R: Read>(
    source: R,
    total_bytes: u64,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    restore_risu_save_reader(source, total_bytes, expected_revision, job, sink, limits)
}

#[cfg(test)]
fn restore_risu_save_reader<R: Read>(
    source: R,
    total_bytes: u64,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    restore_risu_save_reader_controlled(
        source,
        total_bytes,
        expected_revision,
        job,
        sink,
        limits,
        true,
        RestoreProgressScale::default(),
    )
}

fn restore_risu_save_reader_controlled<R: Read>(
    source: R,
    total_bytes: u64,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
    start_job: bool,
    scale: RestoreProgressScale,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled("restore cancelled before staging"));
    }
    if total_bytes < RISU_SAVE_HEADER.len() as u64 {
        return Err(truncated("truncated block RisuSave header"));
    }
    if start_job {
        job.start(JobPhase::ReadingSource)
            .map_err(|error| job_error(job, error))?;
    }
    let mut reader = TrackedReader::new_with_scale(source, total_bytes, job, scale);
    let format = read_risu_save_format(&mut reader)?;

    let staging_id = sink.begin().map_err(store_error)?.staging_id;
    let outcome = (|| {
        let parsed = match format {
            RisuSaveFormat::Block => parse_and_stage(&mut reader, &staging_id, job, sink, limits)?,
            RisuSaveFormat::LegacyRaw => {
                parse_and_stage_legacy(&mut reader, &staging_id, job, sink, limits)?
            }
            RisuSaveFormat::HistoricalPrefixed => {
                parse_and_stage_legacy(&mut reader, &staging_id, job, sink, limits)?
            }
            RisuSaveFormat::LegacyCompressed => {
                parse_and_stage_compressed_legacy(&mut reader, &staging_id, job, sink, limits)?
            }
            RisuSaveFormat::LegacyStream => {
                parse_and_stage_compressed_legacy(&mut reader, &staging_id, job, sink, limits)?
            }
        };
        reader.require_eof()?;
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled before activation"));
        }
        // The renderer keeps serving the app while this job reads and stages,
        // so it may commit in that window. Copying the active repositories here
        // is the fast path for the common case; activation repeats it whenever
        // the renderer reports a different revision to replace.
        let preserved = match sink.preserve_active_repositories(&staging_id, expected_revision) {
            Ok(()) => true,
            Err(StoreError::RevisionConflict { .. }) => false,
            Err(error) => return Err(store_error(error)),
        };
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled before activation"));
        }
        // A save that says nothing about ownership gets one pass over its
        // plugin values before the replacement is applied.
        let preview = sink
            .staged_plugin_preview(&staging_id)
            .map_err(store_error)?;
        job.set_plugin_value_preview(&staging_id, preview)
            .map_err(|error| job_error(job, error))?;
        let activation_revision = job
            .wait_for_restore_finalization()
            .map_err(|error| job_error(job, error))?
            .unwrap_or(expected_revision);
        if !preserved || activation_revision != expected_revision {
            sink.preserve_active_repositories(&staging_id, activation_revision)
                .map_err(store_error)?;
        }
        let revision = sink
            .commit(&staging_id, activation_revision)
            .map_err(store_error)?
            .revision;
        Ok(JobResultSummary {
            export_exclusions: None,
            revision,
            source_bytes: reader.completed,
            source_sha256: hex::encode(reader.hasher.finalize()),
            character_count: parsed.character_count,
            preset_count: parsed.preset_count,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: None,
        })
    })();
    match outcome {
        Ok(result) => Ok(result),
        Err(error) => match sink.abort(&staging_id) {
            Ok(()) => Err(error),
            Err(abort_error) => Err(cleanup_failed(format!(
                "{}; staging abort failed: {}",
                error.message, abort_error
            ))),
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RisuSaveFormat {
    Block,
    LegacyRaw,
    LegacyCompressed,
    LegacyStream,
    HistoricalPrefixed,
}

fn read_risu_save_format<R: Read>(
    reader: &mut TrackedReader<'_, R>,
) -> Result<RisuSaveFormat, NativeJobError> {
    let mut first = [0u8; 1];
    reader.read_exact_checked(&mut first)?;
    if first[0] == RISU_SAVE_HEADER[0] {
        let mut rest = [0u8; RISU_SAVE_HEADER.len() - 1];
        reader.read_exact_checked(&mut rest)?;
        if rest == RISU_SAVE_HEADER[1..] {
            return Ok(RisuSaveFormat::Block);
        }
        return Err(invalid("invalid RisuSave header"));
    }
    if first[0] != 0 {
        return Err(unsupported("unframed RisuSave input is unsupported"));
    }

    let mut second = [0u8; 1];
    reader.read_exact_checked(&mut second)?;
    if second[0] == 0 {
        let mut rest = [0u8; HISTORICAL_RISU_PREFIX.len() - 2];
        reader.read_exact_checked(&mut rest)?;
        if rest == HISTORICAL_RISU_PREFIX[2..] {
            return Ok(RisuSaveFormat::HistoricalPrefixed);
        }
        return Err(invalid("invalid historical RisuSave header"));
    }
    if second[0] != LEGACY_RISU_SAVE_PREFIX[1] {
        return Err(invalid("invalid legacy RisuSave header"));
    }
    let mut rest = [0u8; LEGACY_RISU_SAVE_PREFIX.len() - 2];
    reader.read_exact_checked(&mut rest)?;
    if rest != LEGACY_RISU_SAVE_PREFIX[2..] {
        return Err(invalid("invalid legacy RisuSave header"));
    }
    let mut kind = [0u8; 1];
    reader.read_exact_checked(&mut kind)?;
    match kind[0] {
        7 => Ok(RisuSaveFormat::LegacyRaw),
        8 => Ok(RisuSaveFormat::LegacyCompressed),
        9 => Ok(RisuSaveFormat::LegacyStream),
        value => Err(invalid(format!("unsupported legacy RisuSave kind {value}"))),
    }
}

struct ParsedCounts {
    character_count: u64,
    preset_count: u64,
}

fn parse_and_stage_legacy<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let remaining = reader.total.saturating_sub(reader.completed);
    if remaining > limits.max_decoded_block_bytes {
        return Err(invalid("decoded legacy RisuSave limit exceeded"));
    }
    reader.set_stage(JobStage::DecodingDatabase)?;
    let source = RemainingSourceReader { reader, remaining };
    let decoded = DecodedLimitReader::new(
        source,
        limits.max_decoded_block_bytes,
        job,
        "legacy RisuSave",
    );
    let value = decode_messagepack(decoded, job)?.0;
    reader.complete_item()?;
    stage_legacy_database(value, staging_id, job, reader, sink)
}

fn parse_and_stage_compressed_legacy<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let remaining = reader.total.saturating_sub(reader.completed);
    if remaining > limits.max_encoded_block_bytes {
        return Err(invalid("encoded legacy RisuSave limit exceeded"));
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
    let (value, decoded) = decode_messagepack(decoded, job)?;
    let decoder = decoded.into_inner();
    let buffered = decoder.into_inner();
    if !buffered.buffer().is_empty() || buffered.get_ref().remaining != 0 {
        return Err(corrupt("trailing data in legacy RisuSave gzip stream"));
    }
    reader.complete_item()?;
    stage_legacy_database(value, staging_id, job, reader, sink)
}

fn decode_messagepack<'a, R: Read>(
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
    let value = match messagepack_to_json(packed)? {
        JsonSlot::Value(value) => value,
        JsonSlot::Undefined => {
            return Err(invalid("legacy MessagePack database cannot be undefined"))
        }
    };
    Ok((value, buffered.into_inner()))
}

fn stage_legacy_database<R: Read>(
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
    let mut characters = match root.shift_remove("characters") {
        Some(Value::Array(characters)) => characters,
        _ => return Err(invalid("legacy MessagePack characters must be an array")),
    };
    for (index, character) in characters.iter_mut().enumerate() {
        pocket_features::character(character, &format!("character:{index}")).map_err(invalid)?;
    }
    assign_legacy_chat_ids(&mut characters)?;
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

    let character_total = characters.len() as u64;
    reader.counts.characters_total = Some(character_total);
    reader.counts.presets = presets.len() as u64;
    reader.report_stage_items(JobStage::StagingCharacters, 0, Some(character_total))?;
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
        reader.report_stage_items(
            JobStage::StagingCharacters,
            start as u64,
            Some(character_total),
        )?;
    }

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(|error| job_error(job, error))?;
    reader.report_stage_items(JobStage::FinalizingStaging, 0, None)?;
    pocket_features::root(&root).map_err(invalid)?;
    sink.put_root(staging_id, &Value::Object(root))
        .map_err(store_error)?;
    sink.put_presets(staging_id, &presets)
        .map_err(store_error)?;
    Ok(ParsedCounts {
        character_count: characters.len() as u64,
        preset_count: presets.len() as u64,
    })
}

enum JsonSlot {
    Value(Value),
    Undefined,
}

// PocketRisu and upstream RisuAI assign absent/duplicate chat IDs when loading
// legacy saves. Do that before staging; the persistent store remains strict.
fn assign_legacy_chat_ids(characters: &mut [Value]) -> Result<(), NativeJobError> {
    for character in characters {
        let Some(chats) = character.get_mut("chats").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut reserved: HashSet<String> = chats
            .iter()
            .filter_map(|chat| chat.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect();
        let mut seen = HashSet::new();
        for chat in chats {
            let chat = chat
                .as_object_mut()
                .ok_or_else(|| invalid("legacy chat must be an object"))?;
            match chat.get("id") {
                Some(Value::String(id)) if !id.is_empty() && seen.insert(id.clone()) => continue,
                None | Some(Value::Null) | Some(Value::String(_)) => {}
                _ => return Err(invalid("legacy chat ID must be a string when present")),
            }
            let id = loop {
                let candidate = uuid::Uuid::new_v4().to_string();
                if reserved.insert(candidate.clone()) {
                    break candidate;
                }
            };
            seen.insert(id.clone());
            chat.insert("id".to_owned(), Value::String(id));
        }
    }
    Ok(())
}

fn messagepack_to_json(value: MessagePackValue) -> Result<JsonSlot, NativeJobError> {
    match value {
        MessagePackValue::Nil => Ok(JsonSlot::Value(Value::Null)),
        MessagePackValue::Boolean(value) => Ok(JsonSlot::Value(Value::Bool(value))),
        MessagePackValue::Integer(value) => {
            let number = if let Some(value) = value.as_i64() {
                js_number(value as f64)
            } else if let Some(value) = value.as_u64() {
                js_number(value as f64)
            } else {
                return Err(corrupt("invalid legacy MessagePack integer"));
            };
            Ok(JsonSlot::Value(number))
        }
        MessagePackValue::F32(value) => Ok(JsonSlot::Value(js_number(value as f64))),
        MessagePackValue::F64(value) => Ok(JsonSlot::Value(js_number(value))),
        MessagePackValue::String(value) => value
            .into_str()
            .map(|value| JsonSlot::Value(Value::String(value)))
            .ok_or_else(|| corrupt("invalid UTF-8 legacy MessagePack string")),
        MessagePackValue::Binary(_) => Err(invalid(
            "binary values are unsupported in a legacy MessagePack database",
        )),
        MessagePackValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(match messagepack_to_json(value)? {
                    JsonSlot::Value(value) => value,
                    JsonSlot::Undefined => Value::Null,
                });
            }
            Ok(JsonSlot::Value(Value::Array(output)))
        }
        MessagePackValue::Map(entries) => messagepack_map_to_json(entries),
        MessagePackValue::Ext(kind, bytes) if kind == 0 && bytes == [0] => Ok(JsonSlot::Undefined),
        MessagePackValue::Ext(-1, bytes) if bytes == [0xff] => Ok(JsonSlot::Value(Value::Null)),
        MessagePackValue::Ext(-1, bytes) => {
            Ok(JsonSlot::Value(Value::String(timestamp_to_iso(&bytes)?)))
        }
        MessagePackValue::Ext(kind, _) => Err(unsupported(format!(
            "unsupported legacy MessagePack extension {kind}"
        ))),
    }
}

fn messagepack_map_to_json(
    entries: Vec<(MessagePackValue, MessagePackValue)>,
) -> Result<JsonSlot, NativeJobError> {
    let mut indexed = BTreeMap::<u32, (String, Option<Value>)>::new();
    let mut ordinary = serde_json::Map::new();
    let mut undefined = HashSet::new();
    for (key, value) in entries {
        let key = key
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("legacy MessagePack object keys must be strings"))?;
        let key = if key == "__proto__" {
            "__proto_".to_owned()
        } else {
            key
        };
        let value = messagepack_to_json(value)?;
        if let Some(index) = javascript_array_index(&key) {
            indexed.insert(
                index,
                (
                    key,
                    match value {
                        JsonSlot::Value(value) => Some(value),
                        JsonSlot::Undefined => None,
                    },
                ),
            );
        } else {
            match value {
                JsonSlot::Value(value) => {
                    undefined.remove(&key);
                    ordinary.insert(key, value);
                }
                JsonSlot::Undefined => {
                    ordinary.insert(key.clone(), Value::Null);
                    undefined.insert(key);
                }
            }
        }
    }
    for key in undefined {
        ordinary.shift_remove(&key);
    }
    let mut output = serde_json::Map::new();
    for (_, (key, value)) in indexed {
        if let Some(value) = value {
            output.insert(key, value);
        }
    }
    output.extend(ordinary);
    Ok(JsonSlot::Value(Value::Object(output)))
}

fn javascript_array_index(value: &str) -> Option<u32> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return None;
    }
    let index = value.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == value).then_some(index)
}

fn js_number(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value == 0.0 {
        return Value::Number(0.into());
    }
    if value.fract() == 0.0 {
        if value >= i64::MIN as f64 && value < 9_223_372_036_854_775_808.0 {
            return Value::Number((value as i64).into());
        }
        if value >= 0.0 && value < 18_446_744_073_709_551_616.0 {
            return Value::Number((value as u64).into());
        }
    }
    Value::Number(
        serde_json::Number::from_f64(value)
            .expect("finite MessagePack number must be representable as JSON"),
    )
}

fn timestamp_to_iso(bytes: &[u8]) -> Result<String, NativeJobError> {
    let (seconds, nanoseconds) = match bytes.len() {
        4 => (u32::from_be_bytes(bytes.try_into().unwrap()) as i64, 0),
        8 => {
            let packed = u64::from_be_bytes(bytes.try_into().unwrap());
            ((packed & 0x3_ffff_ffff) as i64, (packed >> 34) as u32)
        }
        12 => (
            i64::from_be_bytes(bytes[4..].try_into().unwrap()),
            u32::from_be_bytes(bytes[..4].try_into().unwrap()),
        ),
        _ => return Err(corrupt("invalid MessagePack timestamp extension length")),
    };
    if nanoseconds >= 1_000_000_000 {
        return Err(corrupt("invalid MessagePack timestamp nanoseconds"));
    }
    const JS_DATE_LIMIT_MILLISECONDS: f64 = 8_640_000_000_000_000.0;
    let milliseconds = seconds as f64 * 1000.0 + nanoseconds as f64 / 1_000_000.0;
    if !milliseconds.is_finite() || milliseconds.abs() > JS_DATE_LIMIT_MILLISECONDS {
        return Err(unsupported(
            "MessagePack timestamp is outside the JavaScript Date range",
        ));
    }
    let milliseconds = milliseconds.trunc() as i64;
    let clipped_seconds = milliseconds.div_euclid(1000);
    let clipped_nanoseconds = milliseconds.rem_euclid(1000) as u32 * 1_000_000;
    let datetime = time::OffsetDateTime::from_unix_timestamp(clipped_seconds)
        .and_then(|value| value.replace_nanosecond(clipped_nanoseconds))
        .map_err(|_| unsupported("MessagePack timestamp cannot be represented natively"))?;
    let year = datetime.year();
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!(
            "{}{abs:06}",
            if year < 0 { '-' } else { '+' },
            abs = year.abs()
        )
    };
    Ok(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        datetime.month() as u8,
        datetime.day(),
        datetime.hour(),
        datetime.minute(),
        datetime.second(),
        datetime.millisecond(),
    ))
}

struct RemainingSourceReader<'a, 'b, R: Read> {
    reader: &'a mut TrackedReader<'b, R>,
    remaining: u64,
}

impl<R: Read> Read for RemainingSourceReader<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let allowed = buffer
            .len()
            .min(self.remaining.min(READ_CHUNK_BYTES as u64) as usize);
        let read = self
            .reader
            .read_chunk(&mut buffer[..allowed])
            .map_err(native_error_to_io)?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn parse_and_stage<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &dyn RestoreControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let mut loaded = HashSet::new();
    let mut directory = None;
    let mut root = None;
    let mut presets = None;
    let mut modules = None;
    let mut loadouts = None;
    let mut plugins = None;
    let mut plugin_storage = None;
    let mut plugin_storage_meta = None;
    let mut character_batch = Vec::new();
    let mut character_batch_bytes = 0usize;
    let mut character_count = 0u64;

    while reader.completed < reader.total {
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled while reading source"));
        }
        let mut prefix = [0u8; 3];
        reader.read_exact_checked(&mut prefix)?;
        let block_type = prefix[0];
        let compression = prefix[1];
        if compression > 1 {
            return Err(invalid(format!("invalid compression flag {compression}")));
        }
        let mut name_bytes = vec![0u8; prefix[2] as usize];
        reader.read_exact_checked(&mut name_bytes)?;
        let name =
            String::from_utf8(name_bytes).map_err(|_| invalid("invalid UTF-8 block name"))?;
        if name.is_empty() || !loaded.insert(name.clone()) {
            return Err(invalid(format!("duplicate block {name}")));
        }
        let mut length_bytes = [0u8; 4];
        reader.read_exact_checked(&mut length_bytes)?;
        let encoded_length = u32::from_le_bytes(length_bytes) as u64;
        if encoded_length > limits.max_encoded_block_bytes {
            return Err(invalid(format!("encoded block limit exceeded for {name}")));
        }
        if encoded_length > reader.total.saturating_sub(reader.completed) {
            return Err(truncated(format!("truncated block body for {name}")));
        }
        // Compressed framing does not provide a trustworthy decoded size.
        // Release completed characters before materializing that next block.
        if !character_batch.is_empty()
            && (compression != 0
                || !matches!(block_type, 2 | 7)
                || (character_batch_bytes as u64).saturating_add(encoded_length)
                    > CHARACTER_BATCH_BYTES as u64)
        {
            sink.add_characters(staging_id, &character_batch)
                .map_err(store_error)?;
            character_batch.clear();
            character_batch_bytes = 0;
        }
        if matches!(block_type, 2 | 7) && sink.supports_incremental_characters() {
            read_character_block(
                reader,
                staging_id,
                &name,
                compression,
                encoded_length,
                character_count,
                limits,
                job,
                sink,
            )?;
            character_count += 1;
            reader.counts.characters = character_count;
            reader.counts.blocks += 1;
            reader.complete_item()?;
            continue;
        }

        let (mut value, decoded_bytes) =
            read_block_value(reader, &name, compression, encoded_length, limits, job)?;

        match block_type {
            0 if name == "config" => {
                if !value.is_object() {
                    return Err(invalid("config block must be a JSON object"));
                }
            }
            1 if name == "root" => {
                let Value::Object(mut object) = value else {
                    return Err(invalid("root block must be a JSON object"));
                };
                directory = Some(parse_directory(object.shift_remove("__directory"))?);
                root = Some(object);
            }
            2 | 7 => {
                let character_id = value
                    .get("chaId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid(format!("character block {name} requires chaId")))?;
                if character_id != name {
                    return Err(invalid(format!(
                        "character block name does not match chaId {name}"
                    )));
                }
                character_batch_bytes = character_batch_bytes.saturating_add(decoded_bytes);
                pocket_features::character(&mut value, &format!("character:{character_count}"))
                    .map_err(invalid)?;
                character_batch.push(value);
                character_count += 1;
                reader.counts.characters = character_count;
                if character_batch.len() >= CHARACTER_BATCH_COUNT
                    || character_batch_bytes >= CHARACTER_BATCH_BYTES
                {
                    sink.add_characters(staging_id, &character_batch)
                        .map_err(store_error)?;
                    character_batch.clear();
                    character_batch_bytes = 0;
                }
            }
            4 if name == "preset" => {
                let Value::Array(list) = value else {
                    return Err(invalid("preset block must be a JSON array"));
                };
                reader.counts.presets = list.len() as u64;
                presets = Some(list);
            }
            5 if name == "modules" => {
                if !value.is_array() {
                    return Err(invalid("modules block must be a JSON array"));
                }
                modules = Some(value);
            }
            9 if name == "plugins" => {
                if !value.is_array() {
                    return Err(invalid("plugins block must be a JSON array"));
                }
                plugins = Some(value);
            }
            10 if name == "loadouts" => {
                if !value.is_array() {
                    return Err(invalid("loadouts block must be a JSON array"));
                }
                loadouts = Some(value);
            }
            11 if name == "pluginStorage" => {
                if !value.is_object() {
                    return Err(invalid("pluginStorage block must be a JSON object"));
                }
                plugin_storage = Some(value);
            }
            12 if name == "pluginStorageMeta" => {
                if !value.is_object() {
                    return Err(invalid("pluginStorageMeta block must be a JSON object"));
                }
                plugin_storage_meta = Some(value);
            }
            3 | 6 | 8 => {
                return Err(unsupported(format!(
                    "block type {block_type} for {name} requires the compatibility parser"
                )))
            }
            _ => {
                return Err(invalid(format!(
                    "unsupported block type {block_type} for {name}"
                )))
            }
        }
        reader.counts.blocks += 1;
        reader.complete_item()?;
    }

    if !character_batch.is_empty() {
        sink.add_characters(staging_id, &character_batch)
            .map_err(store_error)?;
    }
    let required = [
        "preset",
        "modules",
        "loadouts",
        "plugins",
        "pluginStorage",
        "config",
    ];
    let directory = directory.ok_or_else(|| invalid("missing required block root"))?;
    for name in required {
        if !directory.contains(name) || !loaded.contains(name) {
            return Err(invalid(format!("missing required block {name}")));
        }
    }
    for name in &directory {
        if !loaded.contains(name) {
            return Err(invalid(format!("missing required block {name}")));
        }
    }
    if loaded
        .iter()
        .any(|name| name != "root" && !directory.contains(name))
    {
        return Err(invalid(
            "file contains a block not listed by root directory",
        ));
    }

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(|error| job_error(job, error))?;
    reader.report_stage_items(JobStage::FinalizingStaging, 0, None)?;
    let mut root = root.ok_or_else(|| invalid("missing required block root"))?;
    root.insert(
        "modules".to_owned(),
        modules.ok_or_else(|| invalid("missing required block modules"))?,
    );
    root.insert(
        "loadouts".to_owned(),
        loadouts.ok_or_else(|| invalid("missing required block loadouts"))?,
    );
    root.insert(
        "plugins".to_owned(),
        plugins.ok_or_else(|| invalid("missing required block plugins"))?,
    );
    root.insert(
        "pluginCustomStorage".to_owned(),
        plugin_storage.ok_or_else(|| invalid("missing required block pluginStorage"))?,
    );
    if let Some(meta) = plugin_storage_meta {
        root.insert("pluginStorageMeta".to_owned(), meta);
    }
    let presets = presets.ok_or_else(|| invalid("missing required block preset"))?;
    pocket_features::root(&root).map_err(invalid)?;
    sink.put_root(staging_id, &Value::Object(root))
        .map_err(store_error)?;
    sink.put_presets(staging_id, &presets)
        .map_err(store_error)?;
    Ok(ParsedCounts {
        character_count,
        preset_count: presets.len() as u64,
    })
}

fn parse_directory(value: Option<Value>) -> Result<HashSet<String>, NativeJobError> {
    let Some(Value::Array(values)) = value else {
        return Err(invalid("root block requires __directory string array"));
    };
    let mut directory = HashSet::new();
    for value in values {
        let Value::String(name) = value else {
            return Err(invalid("root __directory contains an invalid name"));
        };
        if name.is_empty() {
            return Err(invalid("root __directory contains an invalid name"));
        }
        if name == "root" || directory.contains(&name) {
            return Err(invalid(format!("duplicate block {name} in root directory")));
        }
        directory.insert(name);
    }
    Ok(directory)
}

fn read_block_value<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    name: &str,
    compression: u8,
    encoded_length: u64,
    limits: RestoreLimits,
    job: &dyn RestoreControl,
) -> Result<(Value, usize), NativeJobError> {
    let block = EncodedBlockReader {
        reader,
        remaining: encoded_length,
    };
    if compression == 0 {
        if encoded_length > limits.max_decoded_block_bytes {
            return Err(invalid(format!("decoded block limit exceeded for {name}")));
        }
        let decoded = DecodedLimitReader::new(block, limits.max_decoded_block_bytes, job, name);
        let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
        let value = serde_json::from_reader(&mut buffered)
            .map_err(|error| json_error(name, compression, error, job))?;
        if !buffered.buffer().is_empty() || buffered.get_ref().inner.remaining != 0 {
            return Err(corrupt(format!("trailing data in block {name}")));
        }
        return Ok((value, buffered.get_ref().completed as usize));
    }

    let buffered = BufReader::with_capacity(READ_CHUNK_BYTES, block);
    let decoder = GzDecoder::new(buffered);
    let decoded = DecodedLimitReader::new(decoder, limits.max_decoded_block_bytes, job, name);
    let mut json_reader = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    let value = serde_json::from_reader(&mut json_reader)
        .map_err(|error| json_error(name, compression, error, job))?;
    let decoded_bytes = json_reader.get_ref().completed as usize;
    let mut decoded = json_reader.into_inner();
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    loop {
        let read = decoded
            .read(&mut buffer)
            .map_err(|error| gzip_io_error(name, error, job))?;
        if read == 0 {
            break;
        }
    }
    let decoder = decoded.into_inner();
    let buffered = decoder.into_inner();
    if !buffered.buffer().is_empty() || buffered.get_ref().remaining != 0 {
        return Err(corrupt(format!("trailing data in gzip block {name}")));
    }
    Ok((value, decoded_bytes))
}

fn read_character_block<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    name: &str,
    compression: u8,
    encoded_length: u64,
    character_index: u64,
    limits: RestoreLimits,
    job: &dyn RestoreControl,
    sink: &dyn ReplacementSink,
) -> Result<(), NativeJobError> {
    let block = EncodedBlockReader {
        reader,
        remaining: encoded_length,
    };
    let staging_error = RefCell::new(None);
    let seed = CharacterSeed {
        staging_id,
        expected_id: name,
        character_index,
        job,
        sink,
        staging_error: &staging_error,
    };
    if compression == 0 {
        if encoded_length > limits.max_decoded_block_bytes {
            return Err(invalid(format!("decoded block limit exceeded for {name}")));
        }
        let decoded = DecodedLimitReader::new(block, limits.max_decoded_block_bytes, job, name);
        let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
        let mut deserializer = serde_json::Deserializer::from_reader(&mut buffered);
        if let Err(error) = seed.deserialize(&mut deserializer) {
            if let Some(error) = staging_error.take() {
                return Err(store_error(error));
            }
            return Err(json_error(name, compression, error, job));
        }
        deserializer
            .end()
            .map_err(|error| json_error(name, compression, error, job))?;
        if !buffered.buffer().is_empty() || buffered.get_ref().inner.remaining != 0 {
            return Err(corrupt(format!("trailing data in block {name}")));
        }
        return Ok(());
    }

    let buffered = BufReader::with_capacity(READ_CHUNK_BYTES, block);
    let decoder = GzDecoder::new(buffered);
    let decoded = DecodedLimitReader::new(decoder, limits.max_decoded_block_bytes, job, name);
    let mut json_reader = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    let mut deserializer = serde_json::Deserializer::from_reader(&mut json_reader);
    if let Err(error) = seed.deserialize(&mut deserializer) {
        if let Some(error) = staging_error.take() {
            return Err(store_error(error));
        }
        return Err(json_error(name, compression, error, job));
    }
    deserializer
        .end()
        .map_err(|error| json_error(name, compression, error, job))?;
    drop(deserializer);
    let mut decoded = json_reader.into_inner();
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    while decoded
        .read(&mut buffer)
        .map_err(|error| gzip_io_error(name, error, job))?
        != 0
    {}
    let decoder = decoded.into_inner();
    let buffered = decoder.into_inner();
    if !buffered.buffer().is_empty() || buffered.get_ref().remaining != 0 {
        return Err(corrupt(format!("trailing data in gzip block {name}")));
    }
    Ok(())
}

trait IgnoredJsonValue: Sized {
    fn ignored<E: de::Error>() -> Result<Self, E>;
}

impl IgnoredJsonValue for i64 {
    fn ignored<E: de::Error>() -> Result<Self, E> {
        Ok(0)
    }
}

macro_rules! ignored_visits {
    ($value:ty) => {
        fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_string<E>(self, _value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            IgnoredAny::deserialize(deserializer)?;
            <$value as IgnoredJsonValue>::ignored()
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            <$value as IgnoredJsonValue>::ignored()
        }
    };
}

struct CharacterSeed<'a> {
    staging_id: &'a str,
    expected_id: &'a str,
    character_index: u64,
    job: &'a dyn RestoreControl,
    sink: &'a dyn ReplacementSink,
    staging_error: &'a RefCell<Option<StoreError>>,
}

impl<'de> DeserializeSeed<'de> for CharacterSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(CharacterVisitor(self))
    }
}

struct CharacterVisitor<'a>(CharacterSeed<'a>);

impl<'de> Visitor<'de> for CharacterVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a character object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut detail = Map::new();
        let mut conversation_count = 0i64;
        let mut saw_chats = false;
        while let Some(key) = map.next_key::<String>()? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading character"));
            }
            if key == "chats" {
                if saw_chats {
                    return Err(de::Error::custom("duplicate chats field in character"));
                }
                saw_chats = true;
                conversation_count = map.next_value_seed(ChatsSeed {
                    staging_id: self.0.staging_id,
                    character_id: self.0.expected_id,
                    character_index: self.0.character_index,
                    job: self.0.job,
                    sink: self.0.sink,
                    staging_error: self.0.staging_error,
                })?;
            } else {
                detail.insert(key, map.next_value()?);
            }
        }
        let character_id = detail
            .get("chaId")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("character block requires chaId"))?;
        if character_id != self.0.expected_id {
            return Err(de::Error::custom(
                "character block name does not match chaId",
            ));
        }
        self.0.sink.put_character_detail(
            self.0.staging_id,
            &Value::Object(detail),
            conversation_count,
        )
            .map_err(|error| {
                *self.0.staging_error.borrow_mut() = Some(error);
                de::Error::custom("incremental character staging failed")
            })
    }
}

struct ChatsSeed<'a> {
    staging_id: &'a str,
    character_id: &'a str,
    character_index: u64,
    job: &'a dyn RestoreControl,
    sink: &'a dyn ReplacementSink,
    staging_error: &'a RefCell<Option<StoreError>>,
}

impl<'de> DeserializeSeed<'de> for ChatsSeed<'_> {
    type Value = i64;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(ChatsVisitor(self))
    }
}

struct ChatsVisitor<'a>(ChatsSeed<'a>);

impl<'de> Visitor<'de> for ChatsVisitor<'_> {
    type Value = i64;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a chats array or ignored non-array value")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut count = 0i64;
        while sequence
            .next_element_seed(ConversationSeed {
                staging_id: self.0.staging_id,
                character_id: self.0.character_id,
                configured_index: count,
                fallback: format!(
                    "character:{}:chat:{count}",
                    self.0.character_index
                ),
                job: self.0.job,
                sink: self.0.sink,
                staging_error: self.0.staging_error,
            })?
            .is_some()
        {
            count += 1;
        }
        Ok(count)
    }

    ignored_visits!(i64);
}

struct ConversationSeed<'a> {
    staging_id: &'a str,
    character_id: &'a str,
    configured_index: i64,
    fallback: String,
    job: &'a dyn RestoreControl,
    sink: &'a dyn ReplacementSink,
    staging_error: &'a RefCell<Option<StoreError>>,
}

impl<'de> DeserializeSeed<'de> for ConversationSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_map(ConversationVisitor(self))
    }
}

struct ConversationVisitor<'a>(ConversationSeed<'a>);

impl<'de> Visitor<'de> for ConversationVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a conversation object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut detail = Map::new();
        let mut messages = MessageSpool::new().map_err(de::Error::custom)?;
        let mut saw_messages = false;
        while let Some(key) = map.next_key::<String>()? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading conversation"));
            }
            if key == "message" {
                if saw_messages {
                    return Err(de::Error::custom("duplicate message field in conversation"));
                }
                saw_messages = true;
                messages = map.next_value_seed(MessagesSeed {
                    job: self.0.job,
                })?;
            } else {
                detail.insert(key, map.next_value()?);
            }
        }
        let conversation_id = detail
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| de::Error::custom("conversation requires a nonempty id"))?
            .to_owned();
        let mut normalized_detail = Value::Object(detail);
        pocket_features::chat(&mut normalized_detail, &self.0.fallback)
            .map_err(de::Error::custom)?;
        let recent_at = normalized_detail
            .get("lastDate")
            .and_then(Value::as_i64)
            .or(messages.last_time)
            .unwrap_or_default();
        self.0.sink.put_conversation_row(
            self.0.staging_id,
            self.0.character_id,
            self.0.configured_index,
            &normalized_detail,
            recent_at,
            messages.entries.len() as i64,
        )
            .map_err(|error| {
                *self.0.staging_error.borrow_mut() = Some(error);
                de::Error::custom("incremental conversation staging failed")
            })?;
        messages
            .replay(
                self.0.staging_id,
                self.0.character_id,
                &conversation_id,
                self.0.job,
                self.0.sink,
                self.0.staging_error,
            )
            .map_err(de::Error::custom)
    }
}

struct MessageSpool {
    file: File,
    entries: Vec<(u64, u64)>,
    last_time: Option<i64>,
}

impl IgnoredJsonValue for MessageSpool {
    fn ignored<E: de::Error>() -> Result<Self, E> {
        Self::new().map_err(E::custom)
    }
}

impl MessageSpool {
    fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            entries: Vec::new(),
            last_time: None,
        })
    }

    fn replay(
        &mut self,
        staging_id: &str,
        character_id: &str,
        conversation_id: &str,
        job: &dyn RestoreControl,
        sink: &dyn ReplacementSink,
        staging_error: &RefCell<Option<StoreError>>,
    ) -> Result<(), String> {
        let mut page = Vec::with_capacity(MESSAGE_PAGE_COUNT);
        let mut page_bytes = 0u64;
        let mut start = 0i64;
        for (index, (offset, length)) in self.entries.iter().copied().enumerate() {
            if job.is_cancel_requested() {
                return Err("restore cancelled while staging messages".to_owned());
            }
            if !page.is_empty()
                && (page.len() >= MESSAGE_PAGE_COUNT
                    || page_bytes.saturating_add(length) > MESSAGE_PAGE_BYTES)
            {
                sink.add_conversation_messages(
                    staging_id,
                    character_id,
                    conversation_id,
                    start,
                    &page,
                )
                .map_err(|error| {
                    let message = error.to_string();
                    *staging_error.borrow_mut() = Some(error);
                    message
                })?;
                start += page.len() as i64;
                page.clear();
                page_bytes = 0;
            }
            self.file
                .seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let mut bytes = vec![0u8; length as usize];
            self.file
                .read_exact(&mut bytes)
                .map_err(|error| error.to_string())?;
            let mut message: Value =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            pocket_features::message(
                &mut message,
                &format!("{conversation_id}:response:{index}"),
            )?;
            page.push(message);
            page_bytes = page_bytes.saturating_add(length);
        }
        if !page.is_empty() {
            sink.add_conversation_messages(
                staging_id,
                character_id,
                conversation_id,
                start,
                &page,
            )
            .map_err(|error| {
                let message = error.to_string();
                *staging_error.borrow_mut() = Some(error);
                message
            })?;
        }
        Ok(())
    }
}

struct MessagesSeed<'a> {
    job: &'a dyn RestoreControl,
}

impl<'de> DeserializeSeed<'de> for MessagesSeed<'_> {
    type Value = MessageSpool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(MessagesVisitor(self))
    }
}

struct MessagesVisitor<'a>(MessagesSeed<'a>);

impl<'de> Visitor<'de> for MessagesVisitor<'_> {
    type Value = MessageSpool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a message array or ignored non-array value")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut spool = MessageSpool::new().map_err(de::Error::custom)?;
        while let Some(message) = sequence.next_element::<Value>()? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading messages"));
            }
            let offset = spool.file.stream_position().map_err(de::Error::custom)?;
            serde_json::to_writer(&mut spool.file, &message).map_err(de::Error::custom)?;
            let end = spool.file.stream_position().map_err(de::Error::custom)?;
            spool.entries.push((offset, end - offset));
            spool.last_time = message.get("time").and_then(Value::as_i64);
        }
        Ok(spool)
    }

    ignored_visits!(MessageSpool);
}

struct EncodedBlockReader<'a, 'b, R: Read> {
    reader: &'a mut TrackedReader<'b, R>,
    remaining: u64,
}

impl<R: Read> Read for EncodedBlockReader<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let allowed = buffer
            .len()
            .min(self.remaining.min(READ_CHUNK_BYTES as u64) as usize);
        let read = self
            .reader
            .read_chunk(&mut buffer[..allowed])
            .map_err(native_error_to_io)?;
        self.remaining -= read as u64;
        Ok(read)
    }
}

struct DecodedLimitReader<'a, R: Read> {
    inner: R,
    completed: u64,
    max_bytes: u64,
    job: &'a dyn RestoreControl,
    name: &'a str,
}

impl<'a, R: Read> DecodedLimitReader<'a, R> {
    fn new(inner: R, max_bytes: u64, job: &'a dyn RestoreControl, name: &'a str) -> Self {
        Self {
            inner,
            completed: 0,
            max_bytes,
            job,
            name,
        }
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for DecodedLimitReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.job.is_cancel_requested() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "restore cancelled while decoding block",
            ));
        }
        let read = self.inner.read(buffer)?;
        self.completed = self.completed.saturating_add(read as u64);
        if self.completed > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("decoded block limit exceeded for {}", self.name),
            ));
        }
        Ok(read)
    }
}

struct TrackedReader<'a, R: Read> {
    source: R,
    total: u64,
    completed: u64,
    completed_items: u64,
    hasher: Sha256,
    job: &'a dyn RestoreControl,
    scale: RestoreProgressScale,
    counts: ImportCounts,
    stage: JobStage,
}

impl<'a, R: Read> TrackedReader<'a, R> {
    fn new(source: R, total: u64, job: &'a dyn RestoreControl) -> Self {
        Self::new_with_scale(source, total, job, RestoreProgressScale::default())
    }

    fn new_with_scale(
        source: R,
        total: u64,
        job: &'a dyn RestoreControl,
        scale: RestoreProgressScale,
    ) -> Self {
        let counts = scale.counts.clone();
        Self {
            source,
            total,
            completed: 0,
            completed_items: 0,
            hasher: Sha256::new(),
            job,
            scale,
            counts,
            stage: JobStage::ReadingDatabase,
        }
    }

    fn top_level_progress(&self) -> JobProgress {
        let total = self.scale.total_bytes.unwrap_or(self.total);
        let (completed_items, total_items) = match self.scale.fixed_items {
            Some((completed, total)) => (completed, total),
            None => (self.completed_items, None),
        };
        JobProgress {
            completed_bytes: self
                .scale
                .base_bytes
                .saturating_add(self.completed)
                .min(total),
            total_bytes: Some(total),
            completed_items,
            total_items,
        }
    }

    /// Reports the byte-level database read under the current stage.
    fn report(&mut self) -> Result<(), NativeJobError> {
        self.job
            .set_progress(self.top_level_progress())
            .map_err(|error| job_error(self.job, error))?;
        self.job
            .set_detail(JobDetail::new(
                self.stage,
                StageUnit::Bytes,
                self.completed,
                Some(self.total),
                self.counts.clone(),
            ))
            .map_err(|error| job_error(self.job, error))
    }

    fn set_stage(&mut self, stage: JobStage) -> Result<(), NativeJobError> {
        self.stage = stage;
        self.report()
    }

    /// Reports an item-counted stage (character batches, final staging) that
    /// happens after the database bytes were read.
    fn report_stage_items(
        &mut self,
        stage: JobStage,
        completed: u64,
        total: Option<u64>,
    ) -> Result<(), NativeJobError> {
        self.stage = stage;
        self.job
            .set_detail(JobDetail::new(
                stage,
                StageUnit::Items,
                completed,
                total,
                self.counts.clone(),
            ))
            .map_err(|error| job_error(self.job, error))
    }

    fn read_exact_checked(&mut self, buffer: &mut [u8]) -> Result<(), NativeJobError> {
        let mut offset = 0;
        while offset < buffer.len() {
            let end = (offset + READ_CHUNK_BYTES).min(buffer.len());
            let read = self.read_chunk(&mut buffer[offset..end])?;
            offset += read;
        }
        Ok(())
    }

    fn read_chunk(&mut self, buffer: &mut [u8]) -> Result<usize, NativeJobError> {
        if self.job.is_cancel_requested() {
            return Err(cancelled("restore cancelled while reading source"));
        }
        let remaining = self.total.saturating_sub(self.completed);
        if remaining == 0 {
            return Err(truncated("truncated block RisuSave source"));
        }
        let read_limit = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let read = self
            .source
            .read(&mut buffer[..read_limit])
            .map_err(|error| invalid_source(format!("source read failed: {error}")))?;
        if read == 0 {
            return Err(truncated("truncated block RisuSave source"));
        }
        self.hasher.update(&buffer[..read]);
        self.completed += read as u64;
        self.report()?;
        Ok(read)
    }

    fn require_eof(&mut self) -> Result<(), NativeJobError> {
        if self.completed != self.total {
            return Err(truncated("truncated block RisuSave source"));
        }
        let mut extra = [0u8; 1];
        match self.source.read(&mut extra) {
            Ok(0) => Ok(()),
            Ok(_) => Err(corrupt(
                "source bytes were appended after the selected file was opened",
            )),
            Err(error) => Err(invalid_source(format!("source EOF check failed: {error}"))),
        }
    }

    fn complete_item(&mut self) -> Result<(), NativeJobError> {
        self.completed_items += 1;
        self.report()
    }
}

fn native_error_to_io(error: NativeJobError) -> io::Error {
    let kind = match error.code.as_str() {
        "cancelled" => io::ErrorKind::Other,
        "truncated-input" => io::ErrorKind::UnexpectedEof,
        "corrupt-input" | "invalid-input" => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

fn messagepack_error(error: rmpv::decode::Error, job: &JobControl) -> NativeJobError {
    if job.is_cancel_requested() {
        return cancelled("restore cancelled while decoding legacy MessagePack");
    }
    let message = error.to_string();
    if message.contains("decoded block limit exceeded for legacy RisuSave") {
        return invalid(message);
    }
    if message.contains("source read failed") {
        return invalid_source(message);
    }
    if message.contains("truncated block RisuSave source")
        || message.contains("failed to fill whole buffer")
        || message.contains("unexpected end")
    {
        return truncated("truncated legacy MessagePack payload");
    }
    corrupt(format!("invalid legacy MessagePack: {error}"))
}

fn messagepack_io_error(error: io::Error, job: &JobControl) -> NativeJobError {
    if job.is_cancel_requested() {
        return cancelled("restore cancelled while decoding legacy MessagePack");
    }
    let message = error.to_string();
    if message.contains("decoded block limit exceeded for legacy RisuSave") {
        return invalid(message);
    }
    if message.contains("source read failed") {
        return invalid_source(message);
    }
    if error.kind() == io::ErrorKind::UnexpectedEof
        || message.contains("truncated block RisuSave source")
    {
        return truncated("truncated legacy MessagePack payload");
    }
    corrupt(format!("invalid legacy MessagePack: {error}"))
}

fn json_error(
    name: &str,
    compression: u8,
    error: serde_json::Error,
    job: &dyn RestoreControl,
) -> NativeJobError {
    if job.is_cancel_requested() {
        return cancelled(format!("restore cancelled while decoding block {name}"));
    }
    let message = error.to_string();
    if message.contains("decoded block limit exceeded") {
        return invalid(message);
    }
    if message.contains("source read failed") {
        return invalid_source(message);
    }
    if message.contains("truncated block RisuSave source") {
        return truncated(message);
    }
    if compression == 1 && error.is_io() {
        return corrupt(format!("invalid gzip in block {name}: {error}"));
    }
    corrupt(format!("invalid JSON in block {name}: {error}"))
}

fn gzip_io_error(name: &str, error: io::Error, job: &dyn RestoreControl) -> NativeJobError {
    if job.is_cancel_requested() {
        return cancelled(format!("restore cancelled while decoding block {name}"));
    }
    if error.to_string().contains("decoded block limit exceeded") {
        return invalid(error.to_string());
    }
    corrupt(format!("invalid gzip in block {name}: {error}"))
}

fn invalid(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-input", message)
}

fn unsupported(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("unsupported-format", message)
}

fn truncated(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("truncated-input", message)
}

fn corrupt(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("corrupt-input", message)
}

fn cancelled(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("cancelled", message)
}

fn invalid_source(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-source", message)
}

fn cleanup_failed(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("cleanup-failed", message)
}

// Intentional deviation from super::error::job_error: restore reports a
// cancel-requested job as "cancelled" and everything else as "store-error".
fn job_error(job: &dyn RestoreControl, message: String) -> NativeJobError {
    if job.is_cancel_requested() {
        cancelled(message)
    } else {
        NativeJobError::new("store-error", message)
    }
}

// Intentional deviation from super::error::store_error: the restore job
// protocol reports StoreError::Validation as "store-error" (not
// "invalid-input") and pins its own message texts; TS restore surfaces read
// these codes.
fn store_error(error: StoreError) -> NativeJobError {
    match error {
        StoreError::RevisionConflict { expected, actual } => NativeJobError::new(
            "revision-conflict",
            format!("revision conflict: expected {expected}, actual {actual}"),
        ),
        StoreError::SnapshotReleased => {
            NativeJobError::new("store-error", "persistent snapshot was released")
        }
        StoreError::Validation { message } | StoreError::Store { message } => {
            NativeJobError::new("store-error", message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::persistent_store::{PersistentStore, RevisionResult, StagingResult, StoreResult};
    use base64::Engine;
    use flate2::{write::GzEncoder, Compression, GzBuilder};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Write;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const MSGPACKR_PARITY_FIXTURE: &str = include_str!(
        "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/msgpackr-parity-v1.json"
    );
    const W0_RAW_FIXTURE: &str = include_str!(
        "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-raw-v4.input.base64"
    );
    const W0_COMPRESSED_FIXTURE: &str = include_str!(
        "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-compressed-v4.input.base64"
    );
    const W0_STREAM_FIXTURE: &str = include_str!(
        "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-stream-v4.input.base64"
    );
    const W0_LEGACY_EXPECTED: &str = include_str!(
        "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-raw-v4.expected.json"
    );

    struct StoreSink {
        store: Mutex<PersistentStore>,
        fail_character_batches: bool,
        abort_calls: AtomicUsize,
        preserve_calls: AtomicUsize,
        full_character_calls: AtomicUsize,
        incremental_character_calls: AtomicUsize,
        max_message_page: AtomicUsize,
    }

    impl ReplacementSink for StoreSink {
        fn begin(&self) -> StoreResult<StagingResult> {
            self.store.lock().unwrap().replace_begin()
        }

        fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
            self.store
                .lock()
                .unwrap()
                .replace_put_root(staging_id, root)
        }

        fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
            self.store
                .lock()
                .unwrap()
                .replace_put_presets(staging_id, presets)
        }

        fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
            self.full_character_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_character_batches {
                return Err(crate::persistent_store::StoreError::Store {
                    message: "simulated disk full".to_owned(),
                });
            }
            self.store
                .lock()
                .unwrap()
                .replace_add_characters(staging_id, characters)
        }

        fn supports_incremental_characters(&self) -> bool {
            true
        }

        fn put_character_detail(
            &self,
            staging_id: &str,
            detail: &Value,
            conversation_count: i64,
        ) -> StoreResult<()> {
            self.incremental_character_calls.fetch_add(1, Ordering::AcqRel);
            if self.fail_character_batches {
                return Err(crate::persistent_store::StoreError::Store {
                    message: "simulated disk full".to_owned(),
                });
            }
            self.store.lock().unwrap().replace_put_character_detail(
                staging_id,
                detail,
                conversation_count,
            )
        }

        fn put_conversation_row(
            &self,
            staging_id: &str,
            character_id: &str,
            configured_index: i64,
            detail: &Value,
            recent_at: i64,
            message_count: i64,
        ) -> StoreResult<()> {
            if self.fail_character_batches {
                return Err(crate::persistent_store::StoreError::Store {
                    message: "simulated disk full".to_owned(),
                });
            }
            self.store.lock().unwrap().replace_put_conversation_row(
                staging_id,
                character_id,
                configured_index,
                detail,
                recent_at,
                message_count,
            )
        }

        fn add_conversation_messages(
            &self,
            staging_id: &str,
            character_id: &str,
            conversation_id: &str,
            start: i64,
            messages: &[Value],
        ) -> StoreResult<()> {
            self.max_message_page.fetch_max(messages.len(), Ordering::AcqRel);
            if self.fail_character_batches {
                return Err(crate::persistent_store::StoreError::Store {
                    message: "simulated disk full".to_owned(),
                });
            }
            self.store.lock().unwrap().replace_add_conversation_messages(
                staging_id,
                character_id,
                conversation_id,
                start,
                messages,
            )
        }

        fn preserve_active_repositories(
            &self,
            staging_id: &str,
            expected_revision: i64,
        ) -> StoreResult<()> {
            self.preserve_calls.fetch_add(1, Ordering::AcqRel);
            self.store
                .lock()
                .unwrap()
                .replace_preserve_repositories(staging_id, Some(expected_revision))
                .map(|_| ())
        }

        fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult> {
            self.store
                .lock()
                .unwrap()
                .replace_commit(staging_id, Some(expected_revision))
        }

        fn abort(&self, staging_id: &str) -> StoreResult<()> {
            self.abort_calls.fetch_add(1, Ordering::AcqRel);
            self.store.lock().unwrap().replace_abort(staging_id)
        }
    }

    fn fixture() -> (TempDir, StoreSink) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&staging, &json!({ "username": "Old" }))
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store.replace_add_characters(&staging, &[]).unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
        (
            directory,
            StoreSink {
                store: Mutex::new(store),
                fail_character_batches: false,
                abort_calls: AtomicUsize::new(0),
                preserve_calls: AtomicUsize::new(0),
                full_character_calls: AtomicUsize::new(0),
                incremental_character_calls: AtomicUsize::new(0),
                max_message_page: AtomicUsize::new(0),
            },
        )
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder: GzEncoder<Vec<u8>> = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn raw_block(block_type: u8, compression: u8, name: &str, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![block_type, compression, name.len() as u8];
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn block(block_type: u8, compressed: bool, name: &str, value: &Value) -> Vec<u8> {
        let json = serde_json::to_vec(value).unwrap();
        let payload = if compressed { gzip(&json) } else { json };
        raw_block(block_type, u8::from(compressed), name, &payload)
    }

    fn valid_blocks() -> Vec<Vec<u8>> {
        let character = json!({
            "type": "character",
            "chaId": "char-1",
            "name": "Imported",
            "chats": [{
                "id": "chat-1",
                "name": "Chat",
                "message": [{ "role": "user", "data": "hello", "chatId": "message-1" }]
            }]
        });
        vec![
            block(
                1,
                true,
                "root",
                &json!({
                    "username": "Imported",
                    "__directory": [
                        "preset", "modules", "loadouts", "plugins", "pluginStorage", "char-1", "config"
                    ]
                }),
            ),
            block(4, false, "preset", &json!([{ "name": "Preset" }])),
            block(5, true, "modules", &json!([{ "name": "Module" }])),
            block(10, false, "loadouts", &json!([{ "name": "Loadout" }])),
            block(9, true, "plugins", &json!([{ "name": "Plugin" }])),
            block(
                11,
                false,
                "pluginStorage",
                &json!({ "plugin": { "enabled": true } }),
            ),
            block(2, true, "char-1", &character),
            block(0, false, "config", &json!({ "version": 1 })),
        ]
    }

    fn save_bytes(blocks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut bytes = b"RISUSAVE\0".to_vec();
        for block in blocks {
            bytes.extend(block);
        }
        bytes
    }

    fn valid_save(path: &Path) {
        let bytes = save_bytes(valid_blocks());
        fs::write(path, bytes).unwrap();
    }

    fn raw_recovery_archive(path: &Path) {
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file(
                "manifest.json",
                zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        archive
            .write_all(br#"{"format":"risunest-raw-recovery","version":1}"#)
            .unwrap();
        archive.finish().unwrap();
    }

    #[test]
    fn block_restore_rejects_a_renamed_raw_recovery_archive() {
        let (directory, sink) = fixture();
        let source = directory.path().join("renamed.risudat");
        raw_recovery_archive(&source);
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let error = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "rescue-format-not-restorable");
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(Some(1)).unwrap()["username"], "Old");
    }

    fn msgpackr_parity_fixture() -> Value {
        serde_json::from_str(MSGPACKR_PARITY_FIXTURE).unwrap()
    }

    fn legacy_wire(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0, b'R', b'I', b'S', b'U', b'S', b'A', b'V', b'E', 0, kind];
        bytes.extend_from_slice(payload);
        bytes
    }

    fn assert_failed_restore_preserves_active(bytes: &[u8], expected: &str) {
        let (directory, sink) = fixture();
        let source = directory.path().join("invalid.risudat");
        fs::write(&source, bytes).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let error = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap_err();

        assert!(
            error.message.contains(expected),
            "unexpected error: {error}"
        );
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(Some(1)).unwrap()["username"], "Old");
    }

    fn assert_failed_general_restore_preserves_active(
        bytes: &[u8],
        expected: &str,
    ) -> NativeJobError {
        let (directory, sink) = fixture();
        let source = directory.path().join("invalid-general.risudat");
        fs::write(&source, bytes).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let error = restore_risu_save(&source, 1, &job, &sink).unwrap_err();

        assert!(
            error.message.contains(expected),
            "unexpected error: {error}"
        );
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(Some(1)).unwrap()["username"], "Old");
        error
    }

    fn persistent_projection(value: Value) -> Value {
        let mut database = value.as_object().unwrap().clone();
        let characters = database.remove("characters").unwrap();
        let presets = database.remove("botPresets").unwrap();
        let plugin_storage = database.remove("pluginCustomStorage").unwrap();
        database.insert("characters".to_owned(), characters);
        database.insert("botPresets".to_owned(), presets);
        database.insert("pluginCustomStorage".to_owned(), plugin_storage);
        Value::Object(database)
    }

    fn canonical_hash(value: &Value) -> String {
        fn length_delimited(bytes: &[u8], output: &mut Vec<u8>) {
            output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            output.extend_from_slice(bytes);
        }

        fn encode(value: &Value) -> Vec<u8> {
            let mut output = Vec::new();
            match value {
                Value::Null => output.push(b'N'),
                Value::Bool(true) => output.push(b'T'),
                Value::Bool(false) => output.push(b'F'),
                Value::Number(number) => {
                    output.push(b'D');
                    length_delimited(&number.as_f64().unwrap().to_be_bytes(), &mut output);
                }
                Value::String(value) => {
                    output.push(b'S');
                    length_delimited(value.as_bytes(), &mut output);
                }
                Value::Array(values) => {
                    output.push(b'L');
                    output.extend_from_slice(&(values.len() as u32).to_be_bytes());
                    for value in values {
                        length_delimited(&encode(value), &mut output);
                    }
                }
                Value::Object(entries) => {
                    output.push(b'O');
                    output.extend_from_slice(&(entries.len() as u32).to_be_bytes());
                    for (key, value) in entries {
                        length_delimited(key.as_bytes(), &mut output);
                        length_delimited(&encode(value), &mut output);
                    }
                }
            }
            output
        }

        use sha2::Digest as _;
        hex::encode(sha2::Sha256::digest(encode(value)))
    }

    #[test]
    fn external_block_import_still_converts_pocket_swipes() {
        let (directory, sink) = fixture();
        let source = directory.path().join("external-swipes.risudat");
        let mut blocks = valid_blocks();
        blocks[6] = block(
            2,
            true,
            "char-1",
            &json!({
                "type":"character", "chaId":"char-1", "name":"Imported",
                "chats":[{"id":"chat-1", "name":"Chat", "message":[{
                    "role":"char", "data":"second", "chatId":"message-1",
                    "swipes":["first", "second"], "swipeId":1
                }]}]
            }),
        );
        fs::write(&source, save_bytes(blocks)).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        let restored = sink.store.lock().unwrap().materialize(None).unwrap();
        let message = &restored["characters"][0]["chats"][0]["message"][0];
        assert_eq!(message["data"], "second");
        assert_eq!(
            message["responseVariants"]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(message["swipes"], json!(["first", "second"]));
    }

    #[test]
    fn strict_block_restore_activates_valid_file_through_staged_store() {
        let (directory, sink) = fixture();
        let source = directory.path().join("valid.risudat");
        valid_save(&source);
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        assert_eq!(result.character_count, 1);
        assert_eq!(result.preset_count, 1);
        assert_eq!(result.source_bytes, fs::metadata(source).unwrap().len());
        assert_eq!(result.source_sha256.len(), 64);
        assert_eq!(sink.preserve_calls.load(Ordering::Acquire), 1);
        let store = sink.store.lock().unwrap();
        let restored = store.materialize(Some(2)).unwrap();
        assert_eq!(restored["username"], "Imported");
        assert_eq!(restored["modules"][0]["name"], "Module");
        assert_eq!(restored["pluginCustomStorage"]["plugin"]["enabled"], true);
        assert_eq!(restored["characters"][0]["chaId"], "char-1");
    }

    #[test]
    fn strict_raw_msgpackr_restore_preserves_parity_and_block_round_trip() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["payloadBase64"].as_str().unwrap())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("raw-parity.risudat");
        fs::write(&source, legacy_wire(7, &payload)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(parity["expectedProjection"].clone());
        assert_eq!(first, expected);
        assert_eq!(
            first["roadmap14Unknown"]["persistedDate"],
            "2020-01-02T03:04:05.678Z"
        );
        assert_eq!(first["roadmap14Unknown"]["invalidDate"], Value::Null);
        assert_eq!(
            first["roadmap14Unknown"]["year10000"],
            "+010000-01-01T00:00:00.000Z"
        );
        assert_eq!(
            first["roadmap14Unknown"]["maximumDate"],
            "+275760-09-13T00:00:00.000Z"
        );
        assert_eq!(
            first["roadmap14Unknown"]["prototypeCollision"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["before", "__proto_", "after"]
        );
        assert_eq!(
            first["roadmap14Unknown"]["prototypeCollision"]["__proto_"],
            "literal-last"
        );
        assert!(first["roadmap14Unknown"]["prototypeCollision"]
            .get("__proto__")
            .is_none());
        assert!(first["roadmap14Unknown"].get("omitted").is_none());
        assert_eq!(
            first["roadmap14Unknown"]["ordered"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["2", "10", "zeta", "01", "alpha"]
        );

        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let second = restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        assert_eq!(second.revision, 3);
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn legacy_chat_ids_are_assigned_without_discarding_chats_or_changing_existing_ids() {
        let (_directory, sink) = fixture();
        let database = json!({
            "characters": [{
                "chaId": "synthetic-character", "name": "Synthetic", "type": "character",
                "chats": [
                    {"name": "Without ID", "message": [{"role": "user", "data": "Keep this"}]},
                    {"id": "retained-chat", "name": "Existing ID", "message": []},
                    {"id": "retained-chat", "name": "Duplicate ID", "message": []}
                ]
            }], "botPresets": []
        });
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let staging_id = sink.begin().unwrap().staging_id;
        let mut reader = TrackedReader::new(io::empty(), 0, &*job);
        stage_legacy_database(database.clone(), &staging_id, &job, &mut reader, &sink).unwrap();
        sink.commit(&staging_id, 1).unwrap();
        let restored = sink.store.lock().unwrap().materialize(None).unwrap();
        let chats = restored["characters"][0]["chats"].as_array().unwrap();
        assert_eq!(chats.len(), 3);
        let detail = job.status().detail.expect("legacy staging reports detail");
        assert_eq!(detail.stage, JobStage::FinalizingStaging);
        assert_eq!(detail.counts.characters, 1);
        assert_eq!(detail.counts.characters_total, Some(1));
        assert_eq!(chats[1]["id"], "retained-chat");
        assert_eq!(
            chats[0]["message"],
            database["characters"][0]["chats"][0]["message"]
        );
        let ids = chats
            .iter()
            .map(|chat| chat["id"].as_str().unwrap())
            .collect::<HashSet<_>>();
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| !id.is_empty()));
    }

    #[test]
    fn legacy_restore_supplies_required_optional_root_sections_for_block_round_trip() {
        let (directory, sink) = fixture();
        let database = json!({
            "characters": [],
            "botPresets": [],
            "username": "Synthetic"
        });
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let staging_id = sink.begin().unwrap().staging_id;
        let mut reader = TrackedReader::new(io::empty(), 0, &*job);
        stage_legacy_database(database, &staging_id, &job, &mut reader, &sink).unwrap();
        sink.commit(&staging_id, 1).unwrap();

        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let restored = store.materialize(Some(2)).unwrap();
            assert_eq!(restored["modules"], json!([]));
            assert_eq!(restored["loadouts"], json!([]));
            assert_eq!(restored["plugins"], json!([]));
            assert_eq!(restored["pluginCustomStorage"], json!({}));
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 3);
        sink.store
            .lock()
            .unwrap()
            .cleanup_risu_save_export(&exported_path)
            .unwrap();
        drop(directory);
    }

    #[test]
    fn legacy_restore_rejects_present_nonarray_optional_root_sections() {
        for (key, invalid_value) in [
            ("modules", Value::Null),
            ("loadouts", json!({})),
            ("plugins", json!("invalid")),
        ] {
            let (_directory, sink) = fixture();
            let mut database = json!({
                "characters": [],
                "botPresets": [],
                "pluginCustomStorage": {}
            });
            database[key] = invalid_value;
            let job = JobRegistry::default()
                .create(JobKind::RestoreBlockRisuSave)
                .unwrap();
            job.start(JobPhase::ReadingSource).unwrap();
            let staging_id = sink.begin().unwrap().staging_id;
            let mut reader = TrackedReader::new(io::empty(), 0, &*job);
            let error = match stage_legacy_database(database, &staging_id, &job, &mut reader, &sink)
            {
                Ok(_) => panic!("{key} must reject a present non-array value"),
                Err(error) => error,
            };
            assert_eq!(error.code, "invalid-input");
            assert!(error.message.contains(key));
        }
    }

    #[test]
    fn strict_raw_msgpackr_restore_rejects_unknown_extensions_without_activation() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["unknownExtensionBase64"].as_str().unwrap())
            .unwrap();
        let bytes = legacy_wire(7, &payload);

        let error = assert_failed_general_restore_preserves_active(&bytes, "extension 42");
        assert_eq!(error.code, "unsupported-format");
    }

    #[test]
    fn strict_raw_msgpackr_restore_preserves_undefined_collision_state() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["edgePayloadBase64"].as_str().unwrap())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("raw-undefined-collision.risudat");
        fs::write(&source, legacy_wire(7, &payload)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        restore_risu_save(&source, 1, &job, &sink).unwrap();

        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let unknown = &restored["roadmap14Unknown"];
        assert_eq!(
            unknown["undefinedSanitizedFirst"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["before", "__proto_", "between", "after"]
        );
        assert_eq!(
            unknown["undefinedSanitizedFirst"]["__proto_"],
            "literal-last"
        );
        assert_eq!(
            unknown["undefinedSanitizedLast"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["before", "between", "after"]
        );
        assert!(unknown["undefinedSanitizedLast"].get("__proto_").is_none());
    }

    #[test]
    fn strict_raw_msgpackr_edge_fixture_preserves_timeclip_and_block_round_trip() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["edgePayloadBase64"].as_str().unwrap())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("raw-edge-parity.risudat");
        fs::write(&source, legacy_wire(7, &payload)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        restore_risu_save(&source, 1, &job, &sink).unwrap();

        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(parity["edgeExpectedProjection"].clone());
        assert_eq!(first, expected);

        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn strict_raw_msgpackr_restore_accepts_w0_fixture_and_block_round_trip() {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(W0_RAW_FIXTURE.trim())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("raw-w0.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap());
        assert_eq!(first, expected);
        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn strict_compressed_msgpackr_restore_accepts_w0_fixture_and_block_round_trip() {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(W0_COMPRESSED_FIXTURE.trim())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("compressed-w0.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap());
        assert_eq!(first, expected);
        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn strict_compressed_msgpackr_restore_rejects_trailing_members_and_decoded_overflow() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["payloadBase64"].as_str().unwrap())
            .unwrap();
        let compressed = gzip(&payload);

        let mut truncated = compressed.clone();
        truncated.truncate(truncated.len() - 3);
        assert_failed_general_restore_preserves_active(
            &legacy_wire(8, &truncated),
            "truncated legacy MessagePack payload",
        );

        let mut bad_checksum = compressed.clone();
        let last = bad_checksum.len() - 1;
        bad_checksum[last] ^= 0xff;
        assert_failed_general_restore_preserves_active(
            &legacy_wire(8, &bad_checksum),
            "invalid legacy MessagePack",
        );

        let mut with_garbage = compressed.clone();
        with_garbage.extend_from_slice(b"garbage");
        assert_failed_general_restore_preserves_active(
            &legacy_wire(8, &with_garbage),
            "trailing data in legacy RisuSave gzip stream",
        );

        let mut concatenated = compressed.clone();
        concatenated.extend_from_slice(&gzip(&payload));
        assert_failed_general_restore_preserves_active(
            &legacy_wire(8, &concatenated),
            "trailing data in legacy RisuSave gzip stream",
        );

        let (directory, sink) = fixture();
        let source = directory.path().join("compressed-overflow.risudat");
        fs::write(&source, legacy_wire(8, &compressed)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let error = restore_risu_save_with_limits(
            &source,
            1,
            &job,
            &sink,
            RestoreLimits {
                max_encoded_block_bytes: u32::MAX as u64,
                max_decoded_block_bytes: 64,
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "invalid-input");
        assert!(error.message.contains("decoded block limit"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn compressed_legacy_preflights_encoded_size_for_both_kinds() {
        let encoded = GzBuilder::new()
            .mtime(0)
            .comment(vec![b'x'; 512])
            .write(Vec::new(), Compression::default())
            .finish()
            .unwrap();

        for kind in [8, 9] {
            let (directory, sink) = fixture();
            let source = directory
                .path()
                .join(format!("encoded-overflow-{kind}.risudat"));
            fs::write(&source, legacy_wire(kind, &encoded)).unwrap();
            let job = JobRegistry::default()
                .create(JobKind::RestoreBlockRisuSave)
                .unwrap();

            let error = restore_risu_save_with_limits(
                &source,
                1,
                &job,
                &sink,
                RestoreLimits {
                    max_encoded_block_bytes: 64,
                    max_decoded_block_bytes: 1024,
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "invalid-input");
            assert!(error
                .message
                .contains("encoded legacy RisuSave limit exceeded"));
            assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        }
    }

    #[test]
    fn strict_gzip_stream_msgpackr_restore_accepts_w0_fixture_and_block_round_trip() {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(W0_STREAM_FIXTURE.trim())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("stream-w0.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap());
        assert_eq!(first, expected);
        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn strict_historical_prefixed_msgpackr_restore_preserves_parity_and_block_round_trip() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(parity["historicalPrefixedBase64"].as_str().unwrap())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("historical-prefixed.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        let first = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let expected = persistent_projection(parity["expectedProjection"].clone());
        assert_eq!(first, expected);
        let exported_path = {
            let mut store = sink.store.lock().unwrap();
            let lease = store.acquire_revision(2).unwrap().lease;
            let exported = store.export_risu_save(&lease, false).unwrap();
            store.release_revision(&lease).unwrap();
            PathBuf::from(exported.path)
        };
        let second_job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        restore_risu_save(&exported_path, 2, &second_job, &sink).unwrap();
        let second = sink.store.lock().unwrap().materialize(Some(3)).unwrap();
        assert_eq!(canonical_hash(&second), canonical_hash(&first));
        assert_eq!(second, first);
    }

    #[test]
    fn strict_legacy_format_selection_rejects_ambiguous_and_near_miss_inputs() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["payloadBase64"].as_str().unwrap())
            .unwrap();

        assert_failed_general_restore_preserves_active(
            &payload,
            "unframed RisuSave input is unsupported",
        );
        assert_failed_general_restore_preserves_active(
            &gzip(&payload),
            "unframed RisuSave input is unsupported",
        );

        let mut near_miss = b"\0\0RISX".to_vec();
        near_miss.extend_from_slice(&payload);
        assert_failed_general_restore_preserves_active(
            &near_miss,
            "invalid historical RisuSave header",
        );
        assert_failed_general_restore_preserves_active(
            &legacy_wire(10, &payload),
            "unsupported legacy RisuSave kind 10",
        );

        let mut historical_with_trailing = b"\0\0RISU".to_vec();
        historical_with_trailing.extend_from_slice(&payload);
        historical_with_trailing.push(0);
        assert_failed_general_restore_preserves_active(
            &historical_with_trailing,
            "trailing data after legacy MessagePack value",
        );
    }

    #[test]
    fn current_exporter_round_trip_streams_a_message_history_larger_than_64_mib() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": "Large export",
                    "modules": [],
                    "loadouts": [],
                    "plugins": [],
                    "pluginCustomStorage": {},
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        let messages = (0..16_385).map(|index| json!({
            "role": if index % 2 == 0 { "user" } else { "char" },
            "data": "x".repeat(4 * 1024),
            "chatId": format!("large-message-{index}"),
        })).collect::<Vec<_>>();
        store
            .replace_add_characters(
                &staging,
                &[json!({
                    "type": "character",
                    "chaId": "large-character",
                    "name": "Large character",
                    "chats": [{
                        "id": "large-conversation",
                        "name": "Large conversation",
                        "message": messages,
                    }],
                })],
            )
            .unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
        let lease = store.acquire_revision(1).unwrap().lease;
        let exported = store.export_risu_save(&lease, false).unwrap();
        let exported_path = PathBuf::from(&exported.path);
        let sink = StoreSink {
            store: Mutex::new(store),
            fail_character_batches: false,
            abort_calls: AtomicUsize::new(0),
            preserve_calls: AtomicUsize::new(0),
            full_character_calls: AtomicUsize::new(0),
            incremental_character_calls: AtomicUsize::new(0),
            max_message_page: AtomicUsize::new(0),
        };
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let measurement = crate::test_memory::measure_working_set(|| {
            restore_block_risu_save_path(&exported_path, 1, &job, &sink)
        });
        let result = measurement.value.unwrap();

        assert_eq!(result.revision, 2);
        assert_eq!(result.character_count, 1);
        assert_eq!(sink.full_character_calls.load(Ordering::Acquire), 0);
        assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 1);
        assert!(sink.max_message_page.load(Ordering::Acquire) <= MESSAGE_PAGE_COUNT);
        println!(
            "BOUNDED_RISUSAVE_MEMORY {{\"messageCount\":16385,\"messageBytes\":4096,\"baselineWorkingSetBytes\":{:?},\"peakWorkingSetBytes\":{:?},\"retainedWorkingSetBytes\":{:?},\"maxMessagePage\":{}}}",
            measurement.baseline_working_set_bytes,
            measurement.peak_working_set_bytes,
            measurement.retained_working_set_bytes,
            sink.max_message_page.load(Ordering::Acquire),
        );
        let mut store = sink.store.lock().unwrap();
        store.release_revision(&lease).unwrap();
        store.cleanup_risu_save_export(&exported_path).unwrap();
    }

    #[test]
    fn strict_restore_rejects_missing_and_duplicate_required_blocks() {
        let mut missing = valid_blocks();
        missing.pop();
        assert_failed_restore_preserves_active(&save_bytes(missing), "missing required block");

        let mut duplicate = valid_blocks();
        duplicate.push(block(4, false, "preset", &json!([])));
        assert_failed_restore_preserves_active(&save_bytes(duplicate), "duplicate block");
    }

    #[test]
    fn js_compatible_subset_block_requests_structured_fallback() {
        let mut blocks = valid_blocks();
        blocks.push(block(
            8,
            false,
            "custom-root-component",
            &json!({ "key": "customField", "data": { "retained": true } }),
        ));
        let (directory, sink) = fixture();
        let source = directory.path().join("root-component.risudat");
        fs::write(&source, save_bytes(blocks)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let error = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "unsupported-format");
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn strict_restore_rejects_truncation_invalid_json_gzip_and_headers() {
        let valid = save_bytes(valid_blocks());
        for end in [5, 12, valid.len() - 1] {
            assert_failed_restore_preserves_active(&valid[..end], "truncated");
        }

        let mut invalid_json = valid_blocks();
        invalid_json[2] = raw_block(5, 0, "modules", b"not-json");
        assert_failed_restore_preserves_active(&save_bytes(invalid_json), "invalid JSON");

        let mut invalid_gzip = valid_blocks();
        invalid_gzip[2] = raw_block(5, 1, "modules", b"not-gzip");
        assert_failed_restore_preserves_active(&save_bytes(invalid_gzip), "invalid gzip");

        let mut trailing_garbage = gzip(br#"[{"name":"Module"}]"#);
        trailing_garbage.extend_from_slice(b"garbage");
        let mut invalid_gzip = valid_blocks();
        invalid_gzip[2] = raw_block(5, 1, "modules", &trailing_garbage);
        assert_failed_restore_preserves_active(
            &save_bytes(invalid_gzip),
            "trailing data in gzip block",
        );

        let mut multiple_members = gzip(br#"[{"name":"Module"}]"#);
        multiple_members.extend_from_slice(&gzip(br#"[]"#));
        let mut invalid_gzip = valid_blocks();
        invalid_gzip[2] = raw_block(5, 1, "modules", &multiple_members);
        assert_failed_restore_preserves_active(
            &save_bytes(invalid_gzip),
            "trailing data in gzip block",
        );

        let mut unknown_flag = valid_blocks();
        unknown_flag[2] = raw_block(5, 2, "modules", b"{}");
        assert_failed_restore_preserves_active(&save_bytes(unknown_flag), "compression flag");

        let mut unknown_type = valid_blocks();
        unknown_type[2] = raw_block(99, 0, "modules", b"{}");
        assert_failed_restore_preserves_active(&save_bytes(unknown_type), "block type");
    }

    #[test]
    fn legacy_header_is_restored_through_the_native_job_entrypoint() {
        let (directory, sink) = fixture();
        let source = directory.path().join("legacy.risudat");
        let raw_fixture = base64::engine::general_purpose::STANDARD
            .decode(include_str!(
                "../../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-raw-v4.input.base64"
            ).trim())
            .unwrap();
        fs::write(&source, raw_fixture).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        assert!(result.character_count > 0);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 2);
    }

    #[test]
    fn activation_replaces_against_the_revision_the_renderer_confirms() {
        let (directory, sink) = fixture();
        let source = directory.path().join("selected.risudat");
        valid_save(&source);
        let opened = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: source.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        let sink = Arc::new(sink);
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(1), Vec::new())
            .unwrap();

        let restoring = {
            let job = Arc::clone(&job);
            let sink = Arc::clone(&sink);
            thread::spawn(move || restore_block_risu_save(opened, 1, &job, sink.as_ref()))
        };

        let deadline = Instant::now() + Duration::from_secs(5);
        while job.status().phase != JobPhase::AwaitingActivation {
            assert!(
                Instant::now() < deadline,
                "restore did not reach activation wait"
            );
            thread::sleep(Duration::from_millis(2));
        }

        // The renderer keeps serving the app while the job reads, so it can
        // commit before it fences the replacement.
        let confirmed = {
            let mut store = sink.store.lock().unwrap();
            let staging = store.replace_begin().unwrap().staging_id;
            store
                .replace_put_root(&staging, &json!({ "username": "Edited while importing" }))
                .unwrap();
            store.replace_put_presets(&staging, &[]).unwrap();
            store.replace_add_characters(&staging, &[]).unwrap();
            store.replace_commit(&staging, Some(1)).unwrap().revision
        };
        assert_eq!(confirmed, 2);

        assert_eq!(
            job.request_finalize(Some(confirmed)).unwrap(),
            FinalizeOutcome::Requested
        );

        let result = restoring.join().unwrap().unwrap();

        assert_eq!(result.revision, 3);
        assert_eq!(result.character_count, 1);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 3);
        // The pre-activation copy was made against the stale revision, so
        // activation had to repeat it.
        assert_eq!(sink.preserve_calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn opened_source_identity_survives_path_replacement_without_reopening() {
        let (directory, sink) = fixture();
        let source = directory.path().join("selected.risudat");
        valid_save(&source);
        let opened = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: source.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        let moved = directory.path().join("selected-original.risudat");
        fs::rename(&source, &moved).unwrap();
        fs::write(&source, b"replacement path bytes").unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_block_risu_save(opened, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        assert_eq!(result.character_count, 1);
    }

    #[test]
    fn bytes_appended_after_single_open_are_rejected_before_activation() {
        let (directory, sink) = fixture();
        let source = directory.path().join("selected.risudat");
        valid_save(&source);
        let opened = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: source.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        let mut append = fs::OpenOptions::new().append(true).open(&source).unwrap();
        append.write_all(b"appended after picker open").unwrap();
        append.sync_all().unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let error = restore_block_risu_save(opened, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "corrupt-input");
        assert!(error.message.contains("appended"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn source_shrunk_after_single_open_is_truncated_before_activation() {
        let (directory, sink) = fixture();
        let source = directory.path().join("selected.risudat");
        valid_save(&source);
        let opened = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: source.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(RISU_SAVE_HEADER.len() as u64)
            .unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let error = restore_block_risu_save(opened, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "truncated-input");
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn strict_restore_rejects_invalid_required_block_shapes() {
        let mut modules = valid_blocks();
        modules[2] = block(5, false, "modules", &json!({ "not": "an array" }));
        assert_failed_restore_preserves_active(&save_bytes(modules), "modules block");

        let mut plugins = valid_blocks();
        plugins[4] = block(9, false, "plugins", &json!(null));
        assert_failed_restore_preserves_active(&save_bytes(plugins), "plugins block");

        let mut plugin_storage = valid_blocks();
        plugin_storage[5] = block(11, false, "pluginStorage", &json!([]));
        assert_failed_restore_preserves_active(&save_bytes(plugin_storage), "pluginStorage block");
    }

    #[test]
    fn strict_restore_cancellation_and_revision_conflict_preserve_active_revision() {
        let bytes = save_bytes(valid_blocks());

        let (directory, sink) = fixture();
        let source = directory.path().join("cancel.risudat");
        fs::write(&source, &bytes).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        registry.cancel(&job.id()).unwrap();
        let error = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert!(error.message.contains("cancelled"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);

        let (directory, sink) = fixture();
        let source = directory.path().join("conflict.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let error = restore_block_risu_save_path(&source, 0, &job, &sink).unwrap_err();
        assert_eq!(error.code, "revision-conflict");
        assert!(error.message.contains("revision conflict"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    struct CancelAfterBeginSink<'a> {
        inner: &'a StoreSink,
        job: &'a JobControl,
    }

    impl ReplacementSink for CancelAfterBeginSink<'_> {
        fn begin(&self) -> StoreResult<StagingResult> {
            let staging = self.inner.begin()?;
            self.job.cancel_requested.store(true, Ordering::Release);
            Ok(staging)
        }

        fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
            self.inner.put_root(staging_id, root)
        }

        fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
            self.inner.put_presets(staging_id, presets)
        }

        fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
            self.inner.add_characters(staging_id, characters)
        }

        fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult> {
            self.inner.commit(staging_id, expected_revision)
        }

        fn abort(&self, staging_id: &str) -> StoreResult<()> {
            self.inner.abort(staging_id)
        }
    }

    #[test]
    fn cancellation_after_replace_begin_always_aborts_staging() {
        let (directory, sink) = fixture();
        let source = directory.path().join("cancel-after-begin.risudat");
        valid_save(&source);
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let cancelling = CancelAfterBeginSink {
            inner: &sink,
            job: &job,
        };

        let error = restore_block_risu_save_path(&source, 1, &job, &cancelling).unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn legacy_cancellation_after_replace_begin_aborts_staging() {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(W0_RAW_FIXTURE.trim())
            .unwrap();
        let (directory, sink) = fixture();
        let source = directory.path().join("legacy-cancel-after-begin.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let cancelling = CancelAfterBeginSink {
            inner: &sink,
            job: &job,
        };

        let error = restore_risu_save(&source, 1, &job, &cancelling).unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn strict_restore_bounds_decoded_blocks_and_sweeps_abandoned_staging_on_reopen() {
        let mut blocks = valid_blocks();
        blocks[2] = raw_block(5, 1, "modules", &gzip(br#"["0123456789"]"#));
        let bytes = save_bytes(blocks);
        let (directory, sink) = fixture();
        let source = directory.path().join("oversized.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let error = restore_block_risu_save_path_with_limits(
            &source,
            1,
            &job,
            &sink,
            RestoreLimits {
                max_encoded_block_bytes: 1024,
                max_decoded_block_bytes: 8,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid-input");
        assert!(error.message.contains("decoded block limit"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);

        let staging_id = sink
            .store
            .lock()
            .unwrap()
            .replace_begin()
            .unwrap()
            .staging_id;
        drop(sink);
        let mut reopened = PersistentStore::open(directory.path()).unwrap();
        assert!(reopened.replace_commit(&staging_id, None).is_err());
        assert_eq!(reopened.revision().unwrap(), 1);
    }

    #[test]
    fn stages_completed_characters_before_reading_the_next_compressed_body() {
        let (_directory, mut sink) = fixture();
        sink.fail_character_batches = true;
        let first = block(2, true, "char-1", &json!({ "chaId": "char-1", "chats": [] }));
        let second = block(2, true, "char-2", &json!({ "chaId": "char-2", "chats": [] }));
        let first_end = RISU_SAVE_HEADER.len() + first.len();
        let bytes = save_bytes([first, second]);
        let mut reader = io::Cursor::new(&bytes);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let error = restore_block_risu_save_reader(
            &mut reader, bytes.len() as u64, 1, &job, &sink, RestoreLimits::default(),
        ).unwrap_err();
        assert_eq!(error.code, "store-error");
        assert_eq!(reader.position(), first_end as u64);
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    fn consumed_directory_values_keep_name_and_duplicate_validation() {
        assert_eq!(parse_directory(Some(json!(["preset", "char-1"]))).unwrap(), HashSet::from(["preset".to_owned(), "char-1".to_owned()]));
        for value in [None, Some(json!({})), Some(json!([null])), Some(json!([""])), Some(json!(["root"])), Some(json!(["a", "a"]))] {
            assert!(parse_directory(value).is_err());
        }
    }

    #[test]
    fn strict_restore_aborts_staging_when_a_database_batch_write_fails() {
        let (directory, mut sink) = fixture();
        sink.fail_character_batches = true;
        let source = directory.path().join("disk-full.risudat");
        valid_save(&source);
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let error = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "store-error");
        assert!(error.message.contains("simulated disk full"));
        let mut store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        let staged = store.replace_begin().unwrap().staging_id;
        store.replace_abort(&staged).unwrap();
    }

    #[test]
    fn native_restore_job_remains_pollable_and_cleans_its_owned_directory() {
        let (directory, sink) = fixture();
        let source = directory.path().join("job.risudat");
        valid_save(&source);
        let jobs_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(jobs_root.clone());
        let sink = Arc::new(sink);

        let started = state
            .start_with_sink(
                NativeFileJobStartRequest::RestoreBlockRisuSave {
                    source: JobSource::DesktopPath {
                        path: source.to_string_lossy().into_owned(),
                    },
                    expected_revision: 1,
                },
                sink.clone(),
            )
            .unwrap();

        let status = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                break status;
            }
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(status.state, JobState::Succeeded);
        assert_eq!(status.result.unwrap().revision, 2);
        assert_eq!(
            state.status(&started.job_id).unwrap().state,
            JobState::Succeeded
        );
        assert!(!jobs_root.join("jobs").join(&started.job_id).exists());
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 2);
    }

    #[test]
    fn android_spool_restore_waits_for_finalize_and_cleans_the_claimed_source() {
        let (directory, sink) = fixture();
        let jobs_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(jobs_root.clone());
        let token = Uuid::new_v4().to_string();
        let spool = jobs_root.join("sources").join(&token);
        fs::create_dir_all(&spool).unwrap();
        let source = spool.join("source.risudat");
        valid_save(&source);
        let source_bytes = fs::metadata(&source).unwrap().len();
        fs::write(
            spool.join("ownership.json"),
            serde_json::to_vec(&SpoolOwnership {
                format: ANDROID_SPOOL_FORMAT.to_owned(),
                version: ANDROID_SPOOL_VERSION,
                token: token.clone(),
                created_at_millis: 1,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Ready,
                display_name: "backup.risudat".to_owned(),
                bytes: Some(source_bytes),
                total_bytes: Some(source_bytes),
            })
            .unwrap(),
        )
        .unwrap();
        let sink = Arc::new(sink);

        let started = state
            .spawn(
                NativeFileJobTask::Restore {
                    opened_source: None,
                    source: JobSource::AndroidSpool {
                        token: token.clone(),
                    },
                    expected_revision: 1,
                    sink: RestoreJobSink::Test(sink.clone()),
                },
                true,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let waiting = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state == JobState::WaitingForInput || status.state.is_terminal() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "restore did not reach activation wait"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(waiting.state, JobState::WaitingForInput);
        assert_eq!(waiting.phase, JobPhase::AwaitingActivation);
        assert_eq!(waiting.expected_revision, Some(1));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        assert!(!spool.exists());
        assert!(jobs_root
            .join("jobs")
            .join(&started.job_id)
            .join("android-source")
            .exists());
        assert!(state
            .list()
            .unwrap()
            .iter()
            .any(|status| status.job_id == started.job_id));

        assert_eq!(
            state.finalize(&started.job_id, None).unwrap(),
            FinalizeOutcome::Requested
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let completed = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "restore did not finish after finalize"
            );
            thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(completed.state, JobState::Succeeded);
        assert_eq!(completed.result.unwrap().revision, 2);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 2);
        assert!(!jobs_root.join("jobs").join(&started.job_id).exists());
    }

    struct BlockingBeginSink {
        inner: Arc<StoreSink>,
        entered: Arc<(Mutex<bool>, Condvar)>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ReplacementSink for BlockingBeginSink {
        fn begin(&self) -> StoreResult<StagingResult> {
            let (entered, entered_signal) = &*self.entered;
            *entered.lock().unwrap() = true;
            entered_signal.notify_all();
            let (released, released_signal) = &*self.released;
            let mut released = released.lock().unwrap();
            while !*released {
                released = released_signal.wait(released).unwrap();
            }
            self.inner.begin()
        }

        fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
            self.inner.put_root(staging_id, root)
        }

        fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
            self.inner.put_presets(staging_id, presets)
        }

        fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
            self.inner.add_characters(staging_id, characters)
        }

        fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult> {
            self.inner.commit(staging_id, expected_revision)
        }

        fn abort(&self, staging_id: &str) -> StoreResult<()> {
            self.inner.abort(staging_id)
        }
    }

    #[test]
    fn terminal_cleanup_failure_is_reported_as_a_bounded_warning() {
        let (directory, sink) = fixture();
        let source = directory.path().join("cleanup-warning.risudat");
        valid_save(&source);
        let jobs_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(jobs_root.clone());
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let sink = Arc::new(BlockingBeginSink {
            inner: Arc::new(sink),
            entered: Arc::clone(&entered),
            released: Arc::clone(&released),
        });
        let started = state
            .start_with_sink(
                NativeFileJobStartRequest::RestoreBlockRisuSave {
                    source: JobSource::DesktopPath {
                        path: source.to_string_lossy().into_owned(),
                    },
                    expected_revision: 1,
                },
                sink,
            )
            .unwrap();
        let (entered_lock, entered_signal) = &*entered;
        let mut has_entered = entered_lock.lock().unwrap();
        while !*has_entered {
            has_entered = entered_signal.wait(has_entered).unwrap();
        }
        drop(has_entered);
        fs::write(
            jobs_root
                .join("jobs")
                .join(&started.job_id)
                .join("ownership.json"),
            br#"{"jobId":"different-job"}"#,
        )
        .unwrap();
        let (released_lock, released_signal) = &*released;
        *released_lock.lock().unwrap() = true;
        released_signal.notify_all();

        let status = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                break status;
            }
            thread::sleep(Duration::from_millis(2));
        };

        assert_eq!(status.state, JobState::Succeeded);
        assert_eq!(status.result.unwrap().warning_codes, vec!["cleanup-failed"]);
        assert!(jobs_root.join("jobs").join(&started.job_id).exists());
        let relaunched = NativeFileJobState::initialize(jobs_root);
        assert!(relaunched.capability_error.is_none());
    }

    struct BlockingReader {
        bytes: Vec<u8>,
        offset: usize,
        entered: Arc<(Mutex<bool>, Condvar)>,
        released: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Read for BlockingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.offset >= RISU_SAVE_HEADER.len() && self.offset < self.bytes.len() {
                let (entered, entered_signal) = &*self.entered;
                *entered.lock().unwrap() = true;
                entered_signal.notify_all();
                let (released, released_signal) = &*self.released;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = released_signal.wait(released).unwrap();
                }
            }
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let count = buffer.len().min(self.bytes.len() - self.offset).min(3);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    #[test]
    fn blocking_parser_io_holds_neither_registry_nor_persistent_store_mutex() {
        let (_directory, sink) = fixture();
        let sink = Arc::new(sink);
        let registry = Arc::new(JobRegistry::default());
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let bytes = save_bytes(valid_blocks());
        let total = bytes.len() as u64;
        let worker_sink = sink.clone();
        let worker_job = job.clone();
        let worker_entered = entered.clone();
        let worker_released = released.clone();
        let worker = thread::spawn(move || {
            restore_block_risu_save_reader(
                BlockingReader {
                    bytes,
                    offset: 0,
                    entered: worker_entered,
                    released: worker_released,
                },
                total,
                1,
                &worker_job,
                worker_sink.as_ref(),
                RestoreLimits::default(),
            )
        });

        let (entered_lock, entered_signal) = &*entered;
        let mut has_entered = entered_lock.lock().unwrap();
        while !*has_entered {
            has_entered = entered_signal.wait(has_entered).unwrap();
        }
        drop(has_entered);
        assert!(sink.store.try_lock().is_ok());
        assert_eq!(registry.status(&job.id()).unwrap().state, JobState::Running);

        let (released_lock, released_signal) = &*released;
        *released_lock.lock().unwrap() = true;
        released_signal.notify_all();
        assert_eq!(worker.join().unwrap().unwrap().revision, 2);
    }
}
