use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use crate::persistent_store::{RevisionResult, StagingResult, StoreError, StoreResult};
use flate2::bufread::GzDecoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

const RISU_SAVE_HEADER: &[u8] = b"RISUSAVE\0";
const READ_CHUNK_BYTES: usize = 64 * 1024;
const CHARACTER_BATCH_COUNT: usize = 16;
const CHARACTER_BATCH_BYTES: usize = 8 * 1024 * 1024;

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
    fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult>;
    fn abort(&self, staging_id: &str) -> StoreResult<()>;
}

pub(crate) fn restore_block_risu_save(
    source: &Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
) -> Result<JobResultSummary, NativeJobError> {
    restore_block_risu_save_with_limits(
        source,
        expected_revision,
        job,
        sink,
        RestoreLimits::default(),
    )
}

pub(crate) fn restore_block_risu_save_with_limits(
    source: &Path,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    let total_bytes = source
        .metadata()
        .map_err(|error| invalid_source(format!("source metadata is unavailable: {error}")))?
        .len();
    if total_bytes < RISU_SAVE_HEADER.len() as u64 {
        return Err(truncated("truncated block RisuSave header"));
    }
    let file = File::open(source)
        .map_err(|error| invalid_source(format!("source cannot be opened: {error}")))?;
    restore_block_risu_save_reader(file, total_bytes, expected_revision, job, sink, limits)
}

fn restore_block_risu_save_reader<R: Read>(
    source: R,
    total_bytes: u64,
    expected_revision: i64,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled("restore cancelled before staging"));
    }
    if total_bytes < RISU_SAVE_HEADER.len() as u64 {
        return Err(truncated("truncated block RisuSave header"));
    }
    job.start(JobPhase::ReadingSource)
        .map_err(|error| job_error(job, error))?;
    let mut reader = TrackedReader::new(source, total_bytes, job);
    let mut header = [0u8; RISU_SAVE_HEADER.len()];
    reader.read_exact_checked(&mut header)?;
    if header != RISU_SAVE_HEADER {
        return Err(invalid("invalid block RisuSave header"));
    }

    let staging_id = sink.begin().map_err(store_error)?.staging_id;
    let outcome = (|| {
        let parsed = parse_and_stage(&mut reader, &staging_id, job, sink, limits)?;
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled before activation"));
        }
        job.set_phase(JobPhase::ActivatingDatabase)
            .map_err(|error| job_error(job, error))?;
        let revision = sink
            .commit(&staging_id, expected_revision)
            .map_err(store_error)?
            .revision;
        Ok(JobResultSummary {
            revision,
            source_bytes: reader.completed,
            source_sha256: hex::encode(reader.hasher.finalize()),
            character_count: parsed.character_count,
            preset_count: parsed.preset_count,
            warning_codes: Vec::new(),
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

struct ParsedCounts {
    character_count: u64,
    preset_count: u64,
}

fn parse_and_stage<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &JobControl,
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
        let (value, decoded_bytes) =
            read_block_value(reader, &name, compression, encoded_length, limits, job)?;

        match block_type {
            0 if name == "config" => {
                if !value.is_object() {
                    return Err(invalid("config block must be a JSON object"));
                }
            }
            1 if name == "root" => {
                let mut object = value
                    .as_object()
                    .cloned()
                    .ok_or_else(|| invalid("root block must be a JSON object"))?;
                directory = Some(parse_directory(object.remove("__directory"))?);
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
                if !character_batch.is_empty()
                    && (character_batch.len() >= CHARACTER_BATCH_COUNT
                        || character_batch_bytes.saturating_add(decoded_bytes)
                            > CHARACTER_BATCH_BYTES)
                {
                    sink.add_characters(staging_id, &character_batch)
                        .map_err(store_error)?;
                    character_batch.clear();
                    character_batch_bytes = 0;
                }
                character_batch_bytes = character_batch_bytes.saturating_add(decoded_bytes);
                character_batch.push(value);
                character_count += 1;
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
                presets = Some(
                    value
                        .as_array()
                        .cloned()
                        .ok_or_else(|| invalid("preset block must be a JSON array"))?,
                );
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
            _ => {
                return Err(invalid(format!(
                    "unsupported block type {block_type} for {name}"
                )))
            }
        }
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
    let presets = presets.ok_or_else(|| invalid("missing required block preset"))?;
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
    let values = value
        .and_then(|value| value.as_array().cloned())
        .ok_or_else(|| invalid("root block requires __directory string array"))?;
    let mut directory = HashSet::new();
    for value in values {
        let name = value
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("root __directory contains an invalid name"))?;
        if name == "root" || !directory.insert(name.to_owned()) {
            return Err(invalid(format!("duplicate block {name} in root directory")));
        }
    }
    Ok(directory)
}

fn read_block_value<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    name: &str,
    compression: u8,
    encoded_length: u64,
    limits: RestoreLimits,
    job: &JobControl,
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
    job: &'a JobControl,
    name: &'a str,
}

impl<'a, R: Read> DecodedLimitReader<'a, R> {
    fn new(inner: R, max_bytes: u64, job: &'a JobControl, name: &'a str) -> Self {
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
                io::ErrorKind::Interrupted,
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
    job: &'a JobControl,
}

impl<'a, R: Read> TrackedReader<'a, R> {
    fn new(source: R, total: u64, job: &'a JobControl) -> Self {
        Self {
            source,
            total,
            completed: 0,
            completed_items: 0,
            hasher: Sha256::new(),
            job,
        }
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
        let read = self
            .source
            .read(buffer)
            .map_err(|error| invalid_source(format!("source read failed: {error}")))?;
        if read == 0 {
            return Err(truncated("truncated block RisuSave source"));
        }
        self.hasher.update(&buffer[..read]);
        self.completed += read as u64;
        self.job
            .set_progress(JobProgress {
                completed_bytes: self.completed,
                total_bytes: Some(self.total),
                completed_items: self.completed_items,
                total_items: None,
            })
            .map_err(|error| job_error(self.job, error))?;
        Ok(read)
    }

    fn complete_item(&mut self) -> Result<(), NativeJobError> {
        self.completed_items += 1;
        self.job
            .set_progress(JobProgress {
                completed_bytes: self.completed,
                total_bytes: Some(self.total),
                completed_items: self.completed_items,
                total_items: None,
            })
            .map_err(|error| job_error(self.job, error))
    }
}

fn native_error_to_io(error: NativeJobError) -> io::Error {
    let kind = match error.code.as_str() {
        "cancelled" => io::ErrorKind::Interrupted,
        "truncated-input" => io::ErrorKind::UnexpectedEof,
        "corrupt-input" | "invalid-input" => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error)
}

fn json_error(
    name: &str,
    compression: u8,
    error: serde_json::Error,
    job: &JobControl,
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

fn gzip_io_error(name: &str, error: io::Error, job: &JobControl) -> NativeJobError {
    if job.is_cancel_requested() || error.kind() == io::ErrorKind::Interrupted {
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

fn job_error(job: &JobControl, message: String) -> NativeJobError {
    if job.is_cancel_requested() {
        cancelled(message)
    } else {
        NativeJobError::new("store-error", message)
    }
}

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
    use flate2::{write::GzEncoder, Compression, GzBuilder};
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Write;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    struct StoreSink {
        store: Mutex<PersistentStore>,
        fail_character_batches: bool,
        abort_calls: AtomicUsize,
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

    fn assert_failed_restore_preserves_active(bytes: &[u8], expected: &str) {
        let (directory, sink) = fixture();
        let source = directory.path().join("invalid.risudat");
        fs::write(&source, bytes).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let error = restore_block_risu_save(&source, 1, &job, &sink).unwrap_err();

        assert!(
            error.message.contains(expected),
            "unexpected error: {error}"
        );
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(Some(1)).unwrap()["username"], "Old");
    }

    #[test]
    fn strict_block_restore_activates_valid_file_through_staged_store() {
        let (directory, sink) = fixture();
        let source = directory.path().join("valid.risudat");
        valid_save(&source);
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let result = restore_block_risu_save(&source, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        assert_eq!(result.character_count, 1);
        assert_eq!(result.preset_count, 1);
        assert_eq!(result.source_bytes, fs::metadata(source).unwrap().len());
        assert_eq!(result.source_sha256.len(), 64);
        let store = sink.store.lock().unwrap();
        let restored = store.materialize(Some(2)).unwrap();
        assert_eq!(restored["username"], "Imported");
        assert_eq!(restored["modules"][0]["name"], "Module");
        assert_eq!(restored["pluginCustomStorage"]["plugin"]["enabled"], true);
        assert_eq!(restored["characters"][0]["chaId"], "char-1");
    }

    #[test]
    fn current_exporter_round_trip_supports_a_character_larger_than_64_mib() {
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
        store
            .replace_add_characters(
                &staging,
                &[json!({
                    "type": "character",
                    "chaId": "large-character",
                    "name": "Large character",
                    "description": "x".repeat(64 * 1024 * 1024 + 1),
                    "chats": [],
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
        };
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_block_risu_save(&exported_path, 1, &job, &sink).unwrap();

        assert_eq!(result.revision, 2);
        assert_eq!(result.character_count, 1);
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
    fn strict_restore_rejects_invalid_required_block_shapes() {
        let mut modules = valid_blocks();
        modules[2] = block(5, false, "modules", &json!({ "not": "an array" }));
        assert_failed_restore_preserves_active(&save_bytes(modules), "modules block");

        let mut plugins = valid_blocks();
        plugins[4] = block(9, false, "plugins", &json!(null));
        assert_failed_restore_preserves_active(&save_bytes(plugins), "plugins block");
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
        let error = restore_block_risu_save(&source, 1, &job, &sink).unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert!(error.message.contains("cancelled"));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);

        let (directory, sink) = fixture();
        let source = directory.path().join("conflict.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        let error = restore_block_risu_save(&source, 0, &job, &sink).unwrap_err();
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

        let error = restore_block_risu_save(&source, 1, &job, &cancelling).unwrap_err();

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
        let error = restore_block_risu_save_with_limits(
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
    fn strict_restore_aborts_staging_when_a_database_batch_write_fails() {
        let (directory, mut sink) = fixture();
        sink.fail_character_batches = true;
        let source = directory.path().join("disk-full.risudat");
        valid_save(&source);
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let error = restore_block_risu_save(&source, 1, &job, &sink).unwrap_err();

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
            .start(
                NativeFileJobStartRequest {
                    kind: JobKind::RestoreBlockRisuSave,
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
            .start(
                NativeFileJobStartRequest {
                    kind: JobKind::RestoreBlockRisuSave,
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
