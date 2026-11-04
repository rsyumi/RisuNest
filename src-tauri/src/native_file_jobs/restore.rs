#[path = "upstream_import.rs"]
mod upstream_import;
#[path = "pocket_features.rs"]
pub(super) mod pocket_features;
mod historical;
mod cold_plugin_storage;
use super::{
    ImportCounts, JobControl, JobDetail, JobPhase, JobProgress, JobResultSummary, JobStage,
    NativeJobError, OpenedJobSource, StageUnit,
};
use crate::persistent_store::{RevisionResult, StagingResult, StoreError, StoreResult};
use crate::persistent_store::upstream_stream::{FieldKind, RootSpool};
use flate2::bufread::GzDecoder;
use rmpv::Value as MessagePackValue;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const RISU_SAVE_HEADER: &[u8] = b"RISUSAVE\0";
const LEGACY_RISU_SAVE_PREFIX: &[u8] = b"\0RISUSAVE\0";
const HISTORICAL_RISU_PREFIX: &[u8] = b"\0\0RISU";
const READ_CHUNK_BYTES: usize = 64 * 1024;
const CHARACTER_BATCH_COUNT: usize = 16;
const CHARACTER_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MESSAGE_PAGE_COUNT: usize = 128;
const MESSAGE_PAGE_BYTES: u64 = 8 * 1024 * 1024;
pub(super) const RESTORE_RECORD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct RestoreLimits {
    pub(crate) max_encoded_block_bytes: u64,
    pub(crate) max_decoded_block_bytes: u64,
    pub(crate) max_record_bytes: u64,
}

impl Default for RestoreLimits {
    fn default() -> Self {
        Self {
            max_encoded_block_bytes: u32::MAX as u64,
            max_decoded_block_bytes: 1024 * 1024 * 1024,
            max_record_bytes: RESTORE_RECORD_BYTES,
        }
    }
}

/// Why an archive's cold payload entry was staged without a readable body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColdPayloadUnavailable {
    Damaged,
    TooLarge,
}

pub(crate) trait ReplacementSink: Send + Sync {
    fn prepare_activation_receipt(&self, _staging_id: &str, _job: &JobControl, _result: &JobResultSummary) -> StoreResult<Option<String>> { Ok(None) }
    fn activation_receipt(&self, _staging_id: &str, _authority: &str) -> StoreResult<Option<i64>> { Ok(None) }
    fn begin(&self) -> StoreResult<StagingResult>;
    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()>;
    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()>;
    fn put_legacy_root(&self, _staging_id: &str, _root: &RootSpool) -> StoreResult<()> {
        Err(StoreError::Store { message: "streamed legacy root staging is unavailable".into() })
    }
    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()>;
    fn prepare_character(&self, character: Value) -> StoreResult<Value> {
        Ok(character)
    }
    fn supports_incremental_characters(&self) -> bool {
        false
    }
    fn expands_cold_payloads(&self) -> bool {
        false
    }
    /// A staged payload for the key; an error stops the restore.
    fn cold_payload_path(&self, _key: &str) -> StoreResult<Option<PathBuf>> {
        Ok(None)
    }
    /// Set for a key whose entry the archive holds but could not stage.
    fn unavailable_cold_payload_reason(&self, _key: &str) -> Option<ColdPayloadUnavailable> {
        None
    }
    fn unavailable_cold_payload(&self, _key: &str, _character_name: &str) -> StoreResult<()> {
        Ok(())
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
    fn incomplete_restore_preview(&self) -> StoreResult<super::IncompleteRestorePreview> {
        Ok(super::IncompleteRestorePreview::default())
    }
    /// `binding_authority` is the authority `prepare_activation_receipt` recorded, which the commit keeps.
    fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult>;
    fn abort(&self, staging_id: &str) -> StoreResult<()>;
}

pub(crate) trait RestoreControl {
    fn is_cancel_requested(&self) -> bool;
    fn set_phase(&self, phase: JobPhase) -> Result<(), String>;
    fn set_progress(&self, progress: JobProgress) -> Result<(), String>;
    fn set_detail(&self, detail: JobDetail) -> Result<(), String>;
    fn record_restore_omission(&self, kind: &str, count: u64) -> Result<(), String>;
}

impl RestoreControl for JobControl {
    fn record_restore_omission(&self, kind: &str, count: u64) -> Result<(), String> {
        JobControl::record_restore_omission(self, kind, count)
    }
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
    pub(crate) result_source: Option<(u64, String)>,
    pub(crate) spool_directory: Option<PathBuf>,
    /// Bytes already counted before the database read started.
    pub(crate) base_bytes: u64,
    /// Fixed total for the whole job; `None` uses the database size.
    pub(crate) total_bytes: Option<u64>,
    /// Keeps the top-level item counter at this value instead of counting blocks.
    pub(crate) fixed_items: Option<(u64, Option<u64>)>,
    /// Cold payloads the sink has read from outside the source, counted with the blocks.
    pub(crate) cold_payloads: Option<Arc<ColdPayloadProgress>>,
    /// Counters gathered before the database read, carried into every report.
    pub(crate) counts: ImportCounts,
}

/// What a sink has read from outside the source: items count with the blocks and
/// bytes with the database read.
#[derive(Debug, Default)]
pub(crate) struct ColdPayloadProgress {
    pub(crate) items: AtomicU64,
    pub(crate) bytes: AtomicU64,
}

/// Reports cold payload bytes read while the database read waits for the sink,
/// so a block with many references keeps moving. The read's detail then has no
/// known total, matching what the reader reports next.
pub(crate) fn report_cold_payload_bytes(job: &JobControl, bytes: u64) -> Result<(), NativeJobError> {
    let Some(mut detail) = job.status().detail.filter(|detail| detail.stage_unit == StageUnit::Bytes) else {
        return Ok(());
    };
    detail.stage_completed = detail.stage_completed.saturating_add(bytes);
    detail.stage_total = None;
    job.set_detail(detail).map_err(|error| job_error(job, error))
}

pub(crate) fn restore_block_risu_save(
    mut source: OpenedJobSource,
    owned_directory: &Path,
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
        RestoreProgressScale { spool_directory: Some(owned_directory.to_path_buf()), ..Default::default() },
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
    restore_block_risu_save(source, &tempfile::tempdir().unwrap().path(), expected_revision, job, sink)
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
    let directory = tempfile::tempdir().unwrap();
    restore_risu_save_reader_controlled(
        source,
        total_bytes,
        expected_revision,
        job,
        sink,
        limits,
        true,
        RestoreProgressScale { spool_directory: Some(directory.path().to_path_buf()), ..Default::default() },
    )
}

fn restore_risu_save_reader_controlled<R: Read>(
    mut source: R,
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
    let spool_directory = scale.spool_directory.clone();
    let mut prefix = [0; 2];
    source.read_exact(&mut prefix).map_err(|_| invalid_source("could not read database header"))?;
    let unframed = if is_zlib_header(&prefix) { Some(RisuSaveFormat::HistoricalCompressed) }
        else { None };
    let mut reader = TrackedReader::new_with_scale(io::Cursor::new(prefix).chain(source), total_bytes, job, scale);
    let format = match unframed { Some(format) => format, None => read_risu_save_format(&mut reader)? };

    let staging_id = sink.begin().map_err(store_error)?.staging_id;
    let outcome = (|| {
        let parsed = match format {
            RisuSaveFormat::Block => parse_and_stage(&mut reader, &staging_id, job, sink, limits, spool_directory.as_deref().ok_or_else(|| store_error(StoreError::Store { message: "Restore spool directory is unavailable".into() }))?)?,
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
            RisuSaveFormat::HistoricalCompressed => {
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
        let incomplete = sink.incomplete_restore_preview().map_err(store_error)?;
        job.set_incomplete_restore_preview(incomplete).map_err(|error| job_error(job, error))?;
        let warning_codes = job.status()
            .incomplete_restore_preview.map(|preview| preview.warning_codes()).unwrap_or_default();
        let activation_revision = job
            .wait_for_restore_finalization()
            .map_err(|error| job_error(job, error))?
            .unwrap_or(expected_revision);
        if !preserved || activation_revision != expected_revision {
            sink.preserve_active_repositories(&staging_id, activation_revision)
                .map_err(store_error)?;
        }
        let mut result = JobResultSummary {
            export_exclusions: None,
            revision: 0,
            source_bytes: reader.scale.result_source.as_ref().map_or(reader.completed, |source| source.0),
            source_sha256: reader.scale.result_source.as_ref().map(|source| source.1.clone()).unwrap_or_else(|| hex::encode(reader.hasher.clone().finalize())),
            source_fingerprint_kind: crate::native_file_jobs::SourceFingerprintKind::WholeFileSha256,
            character_count: parsed.character_count,
            preset_count: parsed.preset_count,
            warning_codes,
            handoff_path: None,
            publication: None,
        };
        let authority = sink.prepare_activation_receipt(&staging_id, job, &result).map_err(store_error)?;
        result.revision = match sink.commit(&staging_id, activation_revision, authority.as_deref()) {
            Ok(committed) => committed.revision,
            Err(error) => {
                if let Some(authority) = authority.as_deref() {
                    match sink.activation_receipt(&staging_id, authority) {
                        Ok(Some(revision)) => {
                            result.warning_codes.push("cleanup-failed".to_owned());
                            revision
                        },
                        Ok(None) => return Err(store_error(error)),
                        Err(_) => return Err(NativeJobError::new("activation-outcome-unresolved", "Backup activation requires recovery")),
                    }
                } else { return Err(store_error(error)); }
            }
        };
        if let Some(authority) = authority {
            job.publish_file_activation(&result, authority).map_err(|error| job_error(job, error))?;
        }
        Ok(result)
    })();
    match outcome {
        Ok(result) => Ok(result),
        Err(error) if error.code == "activation-outcome-unresolved" => Err(error),
        Err(error) => match sink.abort(&staging_id) {
            Ok(()) => Err(error),
            Err(StoreError::Validation { message })
                if message == crate::persistent_store::commit::STAGE_ACTIVATED => Err(error),
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
    HistoricalCompressed,
}

pub(super) fn is_zlib_header(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] & 15 == 8 && bytes[0] >> 4 <= 7
        && u16::from_be_bytes([bytes[0], bytes[1]]) % 31 == 0
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
        return Err(limit_error("decoded legacy RisuSave limit exceeded"));
    }
    reader.set_stage(JobStage::DecodingDatabase)?;
    let spool_directory=reader.scale.spool_directory.clone();
    let source = RemainingSourceReader { reader, remaining };
    let decoded = DecodedLimitReader::new(
        source,
        limits.max_decoded_block_bytes,
        job,
        "legacy RisuSave",
    );
    let mut input = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    let database = stream_legacy_database(&mut input, staging_id, job, sink, spool_directory.as_deref())?;
    reader.complete_item()?;
    finish_legacy_database(database, staging_id, job, reader, sink, limits)
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
        return Err(limit_error("encoded legacy RisuSave limit exceeded"));
    }
    reader.set_stage(JobStage::DecodingDatabase)?;
    let spool_directory=reader.scale.spool_directory.clone();
    let source = RemainingSourceReader { reader, remaining };
    let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, source);
    let prefix = buffered.fill_buf().map_err(|error| messagepack_io_error(error, job))?;
    let decoder = if prefix.starts_with(&[0x1f, 0x8b]) {
        historical::CompressionReader::Gzip(GzDecoder::new(buffered))
    } else if is_zlib_header(prefix) {
        historical::CompressionReader::Zlib(flate2::bufread::ZlibDecoder::new(buffered))
    } else {
        historical::CompressionReader::Deflate(flate2::bufread::DeflateDecoder::new(buffered))
    };
    let decoded = DecodedLimitReader::new(
        decoder,
        limits.max_decoded_block_bytes,
        job,
        "legacy RisuSave",
    );
    let mut input = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    let prefix = input.fill_buf().map_err(|error| messagepack_error(rmpv::decode::Error::InvalidMarkerRead(error), job))?;
    let database = if prefix.first() == Some(&b'{') {
        historical::json_database(&mut input, staging_id, job, sink, spool_directory.as_deref())?
    } else {
        stream_legacy_database(&mut input, staging_id, job, sink, spool_directory.as_deref())?
    };
    let compressed = input.get_ref().inner.get_ref();
    if !compressed.buffer().is_empty() || compressed.get_ref().remaining != 0 {
        return Err(corrupt("trailing data in legacy RisuSave gzip stream"));
    }
    reader.complete_item()?;
    finish_legacy_database(database, staging_id, job, reader, sink, limits)
}

// The nesting a root entry and a `characters` element had left when the
// whole database was decoded as one value.
const LEGACY_ROOT_ENTRY_DEPTH: usize = rmpv::decode::MAX_DEPTH - 2;
const LEGACY_CHARACTER_DEPTH: usize = rmpv::decode::MAX_DEPTH - 4;

enum LegacyDatabase {
    Root {
        root: RootSpool,
        character_count: Option<u64>,
        excluded: HashSet<String>,
    },
    NotAnObject,
}

/// Characters go directly to the replacement sink. Root fields and collection
/// members use disk-backed slots so duplicate keys never retain old bodies.
fn stream_legacy_database<S: TrackedSource>(
    input: &mut BufReader<DecodedLimitReader<'_, S>>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    spool_directory: Option<&Path>,
) -> Result<LegacyDatabase, NativeJobError> {
    let marker =
        rmp::decode::read_marker(input).map_err(|error| messagepack_error(error.into(), job))?;
    let length = match marker {
        rmp::Marker::FixMap(length) => u32::from(length),
        rmp::Marker::Map16 => u32::from(u16::from_be_bytes(read_messagepack_length(input, job)?)),
        rmp::Marker::Map32 => u32::from_be_bytes(read_messagepack_length(input, job)?),
        marker => return decode_legacy_non_map(marker, input, job),
    };
    let root = RootSpool::new_in(spool_directory,job.cancellation_flag()).map_err(|error|legacy_staging_error(job,error))?;
    let mut character_count = None;
    let mut excluded = HashSet::new();
    for _ in 0..length {
        let key = rmpv::decode::read_value_with_max_depth(input, LEGACY_ROOT_ENTRY_DEPTH)
            .map_err(|error| messagepack_error(error, job))?;
        match key.as_str().map(|key| key == "characters") {
            None => return Err(invalid("legacy MessagePack object keys must be strings")),
            Some(true) if character_count.is_some() => {
                return Err(corrupt(
                    "legacy MessagePack database has a duplicate characters key",
                ))
            }
            Some(true) => {
                character_count = Some(stream_legacy_characters(input, staging_id, job, sink, &mut excluded)?);
            }
            Some(false) => {
                let key = key.as_str().expect("string key checked above");
                let key = if key == "__proto__" { "__proto_" } else { key };
                stream_legacy_root_entry(input, &root, key, job)?;
            }
        }
    }
    require_legacy_end(input, job)?;
    root.finish_decode().map_err(|error|legacy_staging_error(job,error))?;
    Ok(LegacyDatabase::Root {
        root,
        character_count,
        excluded,
    })
}

fn stream_legacy_root_entry<S: TrackedSource>(
    input: &mut BufReader<DecodedLimitReader<'_, S>>,
    root: &RootSpool,
    key: &str,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    let marker = rmp::decode::read_marker(input).map_err(|error|messagepack_error(error.into(),job))?;
    let collection = match marker {
        rmp::Marker::FixArray(length) => Some((FieldKind::Array,u32::from(length))),
        rmp::Marker::Array16 => Some((FieldKind::Array,u32::from(u16::from_be_bytes(read_messagepack_length(input,job)?)))),
        rmp::Marker::Array32 => Some((FieldKind::Array,u32::from_be_bytes(read_messagepack_length(input,job)?))),
        rmp::Marker::FixMap(length) => Some((FieldKind::Object,u32::from(length))),
        rmp::Marker::Map16 => Some((FieldKind::Object,u32::from(u16::from_be_bytes(read_messagepack_length(input,job)?)))),
        rmp::Marker::Map32 => Some((FieldKind::Object,u32::from_be_bytes(read_messagepack_length(input,job)?))),
        _ => None,
    };
    if let Some((kind,length)) = collection {
        root.begin(key,kind).map_err(|error|legacy_staging_error(job,error))?;
        for index in 0..length {
            if job.is_cancel_requested() { return Err(cancelled("restore cancelled while staging legacy database")); }
            let member = if kind == FieldKind::Object {
                let packed = rmpv::decode::read_value_with_max_depth(input,LEGACY_ROOT_ENTRY_DEPTH-2)
                    .map_err(|error|messagepack_error(error,job))?;
                let key = packed.as_str().ok_or_else(||invalid("legacy MessagePack object keys must be strings"))?;
                if key == "__proto__" { "__proto_".to_owned() } else { key.to_owned() }
            } else { index.to_string() };
            let packed = rmpv::decode::read_value_with_max_depth(input,LEGACY_ROOT_ENTRY_DEPTH-2)
                .map_err(|error|messagepack_error(error,job))?;
            let value = match messagepack_to_json(packed, job)? {
                JsonSlot::Value(value) => Some(value),
                JsonSlot::Undefined if kind == FieldKind::Array => Some(Value::Null),
                JsonSlot::Undefined => None,
            };
            root.put_member(key,&member,value.as_ref()).map_err(|error|legacy_staging_error(job,error))?;
        }
    } else {
        let prefix = [marker.to_u8()];
        let packed = rmpv::decode::read_value_with_max_depth(&mut prefix.as_slice().chain(&mut *input),LEGACY_ROOT_ENTRY_DEPTH)
            .map_err(|error|messagepack_error(error,job))?;
        let value = match messagepack_to_json(packed, job)? { JsonSlot::Value(value)=>Some(value), JsonSlot::Undefined=>None };
        root.put(key,value.as_ref()).map_err(|error|legacy_staging_error(job,error))?;
    }
    Ok(())
}

fn legacy_staging_error(job: &JobControl,error: StoreError) -> NativeJobError {
    if job.is_cancel_requested() { cancelled("restore cancelled while staging legacy database") }
    else { store_error(error) }
}

fn stream_legacy_characters<S: TrackedSource>(
    input: &mut BufReader<DecodedLimitReader<'_, S>>,
    staging_id: &str,
    job: &JobControl,
    sink: &dyn ReplacementSink,
    excluded: &mut HashSet<String>,
) -> Result<u64, NativeJobError> {
    let marker =
        rmp::decode::read_marker(input).map_err(|error| messagepack_error(error.into(), job))?;
    let total = match marker {
        rmp::Marker::FixArray(length) => u64::from(length),
        rmp::Marker::Array16 => u64::from(u16::from_be_bytes(read_messagepack_length(input, job)?)),
        rmp::Marker::Array32 => u64::from(u32::from_be_bytes(read_messagepack_length(input, job)?)),
        _ => return Err(invalid("legacy MessagePack characters must be an array")),
    };
    input
        .get_mut()
        .inner
        .report_counts(|counts| counts.characters_total = Some(total))?;
    let mut retained = 0;
    let mut batch = Vec::new();
    let mut batch_bytes = 0u64;
    let mut character_ids = HashSet::new();
    for index in 0..total {
        if job.is_cancel_requested() {
            return Err(cancelled("restore cancelled while staging legacy database"));
        }
        let start = decoded_position(input);
        let packed = rmpv::decode::read_value_with_max_depth(input, LEGACY_CHARACTER_DEPTH)
            .map_err(|error| messagepack_error(error, job))?;
        let bytes = (decoded_position(input) - start).saturating_add(binary_json_growth(&packed));
        let mut character = match messagepack_to_json(packed, job)? {
            JsonSlot::Value(value) => value,
            JsonSlot::Undefined => Value::Null,
        };
        if upstream_import::exclude_record(&character, excluded) {
            job.record_restore_omission("excludedCharacters", 1).map_err(|error| job_error(job, error))?;
            continue;
        }
        character = sink.prepare_character(character).map_err(store_error)?;
        if !sink.expands_cold_payloads() { omit_unavailable_cold(&mut character, job)?; }
        if upstream_import::exclude_record(&character, excluded) {
            job.record_restore_omission("excludedCharacters", 1).map_err(|error| job_error(job, error))?;
            continue;
        }
        if !prepare_legacy_character(&mut character, &mut character_ids) { continue; }
        retained += 1;
        pocket_features::character(&mut character, &format!("character:{index}"));
        assign_legacy_chat_ids(std::slice::from_mut(&mut character));
        if !batch.is_empty() && batch_bytes.saturating_add(bytes) > CHARACTER_BATCH_BYTES as u64 {
            sink.add_characters(staging_id, &batch)
                .map_err(store_error)?;
            batch.clear();
            batch_bytes = 0;
        }
        batch.push(character);
        batch_bytes = batch_bytes.saturating_add(bytes);
        input
            .get_mut()
            .inner
            .report_counts(|counts| counts.characters = retained)?;
        if batch.len() >= CHARACTER_BATCH_COUNT {
            sink.add_characters(staging_id, &batch)
                .map_err(store_error)?;
            batch.clear();
            batch_bytes = 0;
        }
    }
    if !batch.is_empty() {
        sink.add_characters(staging_id, &batch)
            .map_err(store_error)?;
    }
    Ok(retained)
}

// A root that is not a map is still decoded whole, so it fails the same way.
fn decode_legacy_non_map<B: Read>(
    marker: rmp::Marker,
    input: &mut B,
    job: &JobControl,
) -> Result<LegacyDatabase, NativeJobError> {
    let marker = [marker.to_u8()];
    let packed = rmpv::decode::read_value(&mut marker.as_slice().chain(&mut *input))
        .map_err(|error| messagepack_error(error, job))?;
    require_legacy_end(input, job)?;
    match messagepack_to_json(packed, job)? {
        JsonSlot::Value(_) => Ok(LegacyDatabase::NotAnObject),
        JsonSlot::Undefined => Err(invalid("legacy MessagePack database cannot be undefined")),
    }
}

fn read_messagepack_length<const N: usize>(
    input: &mut impl Read,
    job: &JobControl,
) -> Result<[u8; N], NativeJobError> {
    let mut bytes = [0u8; N];
    input
        .read_exact(&mut bytes)
        .map_err(|error| messagepack_error(rmpv::decode::Error::InvalidDataRead(error), job))?;
    Ok(bytes)
}

fn require_legacy_end(input: &mut impl Read, job: &JobControl) -> Result<(), NativeJobError> {
    let mut trailing = [0u8; 1];
    let read = input
        .read(&mut trailing)
        .map_err(|error| messagepack_io_error(error, job))?;
    if read != 0 {
        return Err(corrupt("trailing data after legacy MessagePack value"));
    }
    Ok(())
}

fn decoded_position<S: Read>(input: &BufReader<DecodedLimitReader<'_, S>>) -> u64 {
    input.get_ref().completed - input.buffer().len() as u64
}

fn finish_legacy_database<R: Read>(
    database: LegacyDatabase,
    staging_id: &str,
    job: &JobControl,
    reader: &mut TrackedReader<'_, R>,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
) -> Result<ParsedCounts, NativeJobError> {
    let (root, character_count, excluded) = match database {
        LegacyDatabase::Root {
            root,
            character_count: Some(count),
            excluded,
        } => (root, count, excluded),
        LegacyDatabase::Root { .. } => {
            return Err(invalid("legacy MessagePack characters must be an array"))
        }
        LegacyDatabase::NotAnObject => {
            return Err(invalid("legacy MessagePack database must be an object"))
        }
    };
    for key in NULLABLE_ROOT_COLLECTIONS {
        if root.scalar(key).map_err(store_error)? == Some(Value::Null) {
            root.remove(key).map_err(|error|legacy_staging_error(job,error))?;
        }
    }
    let preset_count = match root.kind("botPresets").map_err(store_error)? {
        Some(FieldKind::Array) => root.count("botPresets").map_err(store_error)?,
        Some(_) => return Err(invalid("legacy MessagePack botPresets must be an array")),
        None => 0,
    };
    if let Some(kind) = root.kind("pluginCustomStorage").map_err(store_error)? {
        if kind != FieldKind::Object {
            return Err(invalid(
                "legacy MessagePack pluginCustomStorage must be an object",
            ));
        }
    }
    for key in ["modules", "loadouts", "plugins"] {
        let kind = root.kind(key).map_err(store_error)?;
        if kind.is_some_and(|kind| kind != FieldKind::Array) {
            return Err(invalid(format!(
                "legacy MessagePack {key} must be an array"
            )));
        }
        if kind.is_none() { root.begin(key,FieldKind::Array).map_err(|error|legacy_staging_error(job,error))?; }
    }
    if root.kind("pluginCustomStorage").map_err(store_error)?.is_none() {
        root.begin("pluginCustomStorage",FieldKind::Object).map_err(|error|legacy_staging_error(job,error))?;
    }
    let pointer = root.member("pluginCustomStorage", cold_plugin_storage::INDEX_KEY).map_err(store_error)?;
    let legacy = root.member("pluginCustomStorage", cold_plugin_storage::LEGACY_MAP_KEY).map_err(store_error)?;
    for marker in [cold_plugin_storage::INDEX_KEY, cold_plugin_storage::LEGACY_MAP_KEY] {
        root.remove_member("pluginCustomStorage", marker).map_err(store_error)?;
    }
    if !sink.expands_cold_payloads() {
        let markers = u64::from(pointer.is_some()) + u64::from(legacy.is_some());
        if markers != 0 { job.record_restore_omission("pluginStorage", markers).map_err(|error| job_error(job, error))?; }
    } else if cold_plugin_storage::references(pointer.as_ref(), legacy.as_ref()) {
        cold_plugin_storage::import(&mut LegacyPluginStorage(&root), pointer, legacy, sink, job, limits)?;
    }
    reader.counts.presets = preset_count;

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(|error| job_error(job, error))?;
    reader.report_stage_items(JobStage::FinalizingStaging, 0, None)?;
    prepare_legacy_root(&root,&excluded).map_err(|error| {
        if job.is_cancel_requested() { cancelled("restore cancelled while staging legacy database") }
        else if let StoreError::Validation { message }=error { invalid(message) }
        else { store_error(error) }
    })?;
    sink.put_legacy_root(staging_id, &root).map_err(|error| {
        if job.is_cancel_requested() { cancelled("restore cancelled while staging legacy database") } else { store_error(error) }
    })?;
    Ok(ParsedCounts {
        character_count,
        preset_count,
    })
}

struct LegacyPluginStorage<'a>(&'a RootSpool);

impl cold_plugin_storage::Target for LegacyPluginStorage<'_> {
    fn contains(&self, key: &str) -> Result<bool, NativeJobError> {
        Ok(self.0.member("pluginCustomStorage", key).map_err(store_error)?.is_some())
    }

    fn insert(&mut self, key: &str, value: Value) -> Result<(), NativeJobError> {
        self.0.put_member("pluginCustomStorage", key, Some(&value)).map_err(store_error)
    }
}

// PocketRisu stores a root field that became undefined as null, and upstream
// RisuAI loads a null collection as absent.
const NULLABLE_ROOT_COLLECTIONS: [&str; 5] = ["botPresets", "pluginCustomStorage", "modules", "loadouts", "plugins"];

// Streamed counterpart of pocket_features::root.
fn prepare_legacy_root(root: &RootSpool, excluded: &HashSet<String>) -> StoreResult<()> {
    if root.kind("disableToggleBinding")?.is_some()
        && !root.scalar("disableToggleBinding")?.is_some_and(|value|value.is_boolean()) {
        root.remove("disableToggleBinding")?;
    }
    match root.kind("defaultToggleValues")? {
        Some(FieldKind::Object) => root.visit("defaultToggleValues",|key,value| {
            if pocket_features::toggle_entry(key,&value) { Ok(()) } else { root.remove_member("defaultToggleValues",key) }
        })?,
        Some(_) => root.remove("defaultToggleValues")?,
        None => {}
    }
    match root.kind("togglePresets")? {
        Some(FieldKind::Array) => root.visit("togglePresets",|key,mut value| {
            if pocket_features::toggle_preset(&mut value) { root.put_member("togglePresets",key,Some(&value)) }
            else { root.remove_member("togglePresets",key) }
        })?,
        Some(_) => root.remove("togglePresets")?,
        None => {}
    }
    root.remove("account")?;
    for field in ["characterOrder","loadouts"] {
        if root.kind(field)? != Some(FieldKind::Array) { continue; }
        root.visit(field,|key,value| {
            let mut wrapper=Map::new(); wrapper.insert(field.to_owned(),Value::Array(vec![value]));
            upstream_import::prepare_root(&mut wrapper,excluded);
            let values=wrapper[field].as_array().expect("prepared array");
            if let Some(value)=values.first() { root.put_member(field,key,Some(value)) } else { root.remove_member(field,key) }
        })?;
    }
    for (from,to) in [("groupTemplate","messageNameTemplate"),("groupOtherBotRole","namedMessageRole")] {
        root.rename(from,to)?;
    }
    if root.kind("protectedPresetValues")? == Some(FieldKind::Object) {
        for (from,to) in [("groupTemplate","messageNameTemplate"),("groupOtherBotRole","namedMessageRole")] {
            root.rename_member("protectedPresetValues",from,to)?;
        }
    }
    root.visit("botPresets",|key,mut value| {
        upstream_import::prepare_presets(std::slice::from_mut(&mut value));
        root.put_member("botPresets",key,Some(&value))
    })?;
    Ok(())
}

enum JsonSlot {
    Value(Value),
    Undefined,
}

// PocketRisu and upstream RisuAI assign absent/duplicate chat IDs when loading
// legacy saves, and PocketRisu skips chat entries that are not objects. Do that
// before staging; the persistent store remains strict.
fn assign_legacy_chat_ids(characters: &mut [Value]) {
    for character in characters {
        let Some(chats) = character.get_mut("chats").and_then(Value::as_array_mut) else {
            continue;
        };
        chats.retain(Value::is_object);
        let mut reserved: HashSet<String> = chats
            .iter()
            .filter_map(|chat| legacy_chat_id(chat.get("id")))
            .collect();
        let mut seen = HashSet::new();
        for chat in chats.iter_mut().filter_map(Value::as_object_mut) {
            let id = match legacy_chat_id(chat.get("id")) {
                Some(id) if !id.is_empty() && seen.insert(id.clone()) => id,
                _ => {
                    let id = loop {
                        let candidate = uuid::Uuid::new_v4().to_string();
                        if reserved.insert(candidate.clone()) {
                            break candidate;
                        }
                    };
                    seen.insert(id.clone());
                    id
                }
            };
            chat.insert("id".to_owned(), Value::String(id));
            default_name(chat);
        }
    }
}

// Upstream RisuAI skips empty character entries and fills a missing character
// ID and name when it loads a save. A repeated ID gets a new one, since lookups
// take the first character with that ID.
fn prepare_legacy_character(value: &mut Value, seen_ids: &mut HashSet<String>) -> bool {
    let Some(character) = value.as_object_mut() else {
        return false;
    };
    match character.get("chaId").and_then(Value::as_str) {
        Some(id) if !id.is_empty() && seen_ids.insert(id.to_owned()) => {}
        _ => {
            let id = loop {
                let candidate = uuid::Uuid::new_v4().to_string();
                if seen_ids.insert(candidate.clone()) {
                    break candidate;
                }
            };
            character.insert("chaId".to_owned(), Value::String(id));
        }
    }
    default_name(character);
    true
}

// The persistent store requires a name, which upstream RisuAI shows as empty when absent.
fn default_name(record: &mut Map<String, Value>) {
    if !record.get("name").is_some_and(Value::is_string) {
        record.insert("name".to_owned(), Value::String(String::new()));
    }
}

// A numeric chat ID keeps its text form, as it does as a JavaScript object key.
fn legacy_chat_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

// Each `bin` byte at index i becomes `"i":byte,` once converted, so a batch
// charges that JSON text rather than the one MessagePack byte.
fn binary_json_growth(value: &MessagePackValue) -> u64 {
    let digits = |value: usize| u64::from(value.checked_ilog10().unwrap_or(0)) + 1;
    match value {
        MessagePackValue::Binary(bytes) => bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| digits(index) + digits(usize::from(*byte)) + 3)
            .sum(),
        MessagePackValue::Array(values) => values.iter().map(binary_json_growth).sum(),
        MessagePackValue::Map(entries) => entries.iter().map(|(_, value)| binary_json_growth(value)).sum(),
        _ => 0,
    }
}

fn messagepack_to_json(value: MessagePackValue, job: &JobControl) -> Result<JsonSlot, NativeJobError> {
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
        MessagePackValue::Binary(bytes) => Ok(JsonSlot::Value(Value::Object(bytes.into_iter()
            .enumerate().map(|(index, byte)| (index.to_string(), Value::from(byte))).collect()))),
        MessagePackValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(match messagepack_to_json(value, job)? {
                    JsonSlot::Value(value) => value,
                    JsonSlot::Undefined => Value::Null,
                });
            }
            Ok(JsonSlot::Value(Value::Array(output)))
        }
        MessagePackValue::Map(entries) => messagepack_map_to_json(entries, job),
        MessagePackValue::Ext(kind, bytes) if kind == 0 && bytes == [0] => Ok(JsonSlot::Undefined),
        MessagePackValue::Ext(-1, bytes) if bytes == [0xff] => Ok(JsonSlot::Value(Value::Null)),
        MessagePackValue::Ext(-1, bytes) => {
            match timestamp_to_iso(&bytes) {
                Ok(value) => Ok(JsonSlot::Value(Value::String(value))),
                Err(error) if error.code == "unsupported-format" => {
                    job.record_restore_omission("values", 1).map_err(|error| job_error(job, error))?;
                    Ok(JsonSlot::Undefined)
                }
                Err(error) => Err(error),
            }
        }
        MessagePackValue::Ext(_, _) => {
            job.record_restore_omission("values", 1).map_err(|error| job_error(job, error))?;
            Ok(JsonSlot::Undefined)
        }
    }
}

fn messagepack_map_to_json(
    entries: Vec<(MessagePackValue, MessagePackValue)>,
    job: &JobControl,
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
        let value = messagepack_to_json(value, job)?;
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

/// Reaches the tracked source beneath the legacy decoders, whose counters
/// every source read reports.
trait TrackedSource: Read {
    fn report_counts(
        &mut self,
        update: impl FnOnce(&mut ImportCounts),
    ) -> Result<(), NativeJobError>;
}

impl<R: Read> TrackedSource for RemainingSourceReader<'_, '_, R> {
    fn report_counts(
        &mut self,
        update: impl FnOnce(&mut ImportCounts),
    ) -> Result<(), NativeJobError> {
        update(&mut self.reader.counts);
        self.reader.report()
    }
}

impl<S: TrackedSource> TrackedSource for GzDecoder<BufReader<S>> {
    fn report_counts(
        &mut self,
        update: impl FnOnce(&mut ImportCounts),
    ) -> Result<(), NativeJobError> {
        self.get_mut().get_mut().report_counts(update)
    }
}

fn omit_unavailable_cold(character: &mut Value, job: &dyn RestoreControl) -> Result<(), NativeJobError> {
    let mut missing = 0;
    if let Some(object) = character.as_object_mut() {
        if object.remove("coldstorage").and_then(|value| value.as_str().map(|key| !key.is_empty())).unwrap_or(false) { missing += 1; }
        object.remove("coldStoragedChats");
        if let Some(chats) = object.get_mut("chats").and_then(Value::as_array_mut) {
            for chat in chats {
                let cold = chat.get("message").and_then(Value::as_array).and_then(|messages| messages.first()).and_then(|message| message.get("data")).and_then(Value::as_str).is_some_and(|data| data.starts_with("\u{ef01}COLDSTORAGE\u{ef01}"));
                if cold { chat["message"] = Value::Array(vec![]); missing += 1; }
            }
        }
    }
    if missing > 0 { job.record_restore_omission("missing", missing).map_err(|error| job_error(job, error))?; }
    Ok(())
}

fn parse_and_stage<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    staging_id: &str,
    job: &dyn RestoreControl,
    sink: &dyn ReplacementSink,
    limits: RestoreLimits,
    spool_directory: &Path,
) -> Result<ParsedCounts, NativeJobError> {
    let mut excluded = HashSet::new();
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
            return Err(limit_error(format!("encoded block limit exceeded for {name}")));
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
            let retained = read_character_block(
                reader,
                staging_id,
                &name,
                compression,
                encoded_length,
                character_count,
                limits,
                job,
                sink,
                spool_directory,
            )?;
            if retained { character_count += 1; } else { excluded.insert(name.clone()); }
            reader.counts.characters = character_count;
            reader.counts.blocks += 1;
            reader.complete_item()?;
            continue;
        }

        let interpreted = matches!((block_type, name.as_str()),
            (0, "config") | (1, "root") | (4, "preset") | (5, "modules") |
            (9, "plugins") | (10, "loadouts") | (11, "pluginStorage") | (12, "pluginStorageMeta"))
            || matches!(block_type, 2 | 7);
        if !interpreted {
            skip_block(reader, &name, compression, encoded_length, limits, job)?;
            let kind = if block_type == 6 { "remote" } else { "data" };
            job.record_restore_omission(kind, 1).map_err(|error| job_error(job, error))?;
            reader.counts.blocks += 1;
            reader.complete_item()?;
            continue;
        }

        let (mut value, decoded_bytes) = read_block_value(
            reader,
            &name,
            compression,
            encoded_length,
            limits,
            job,
            block_type == 11 && name == "pluginStorage",
        )?;

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
                if let Some(value) = object.shift_remove("__directory") {
                    directory = Some(parse_directory(Some(value))?);
                }
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
                if upstream_import::exclude_record(&value, &mut excluded) {
                    job.record_restore_omission("excludedCharacters", 1).map_err(|error| job_error(job, error))?;
                    reader.counts.blocks += 1;
                    reader.complete_item()?;
                    continue;
                }
                value = sink.prepare_character(value).map_err(store_error)?;
                if upstream_import::exclude_record(&value, &mut excluded) {
                    job.record_restore_omission("excludedCharacters", 1).map_err(|error| job_error(job, error))?;
                    reader.counts.blocks += 1;
                    reader.complete_item()?;
                    continue;
                }
                if !sink.expands_cold_payloads() { omit_unavailable_cold(&mut value, job)?; }
                assign_legacy_chat_ids(std::slice::from_mut(&mut value));
                character_batch_bytes = character_batch_bytes.saturating_add(decoded_bytes);
                pocket_features::character(&mut value, &format!("character:{character_count}"));
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
            _ => unreachable!("uninterpreted blocks were skipped above"),
        }
        reader.counts.blocks += 1;
        reader.complete_item()?;
    }

    if !character_batch.is_empty() {
        sink.add_characters(staging_id, &character_batch)
            .map_err(store_error)?;
    }
    if let Some(directory) = directory {
        let missing = directory.difference(&loaded).count() as u64;
        if missing != 0 {
            job.record_restore_omission("missing", missing).map_err(|error| job_error(job, error))?;
        }
    }

    job.set_phase(JobPhase::StagingDatabase)
        .map_err(|error| job_error(job, error))?;
    reader.report_stage_items(JobStage::FinalizingStaging, 0, None)?;
    let mut root = root.ok_or_else(|| invalid("missing required block root"))?;
    for (key, separate, default) in [
        ("modules", modules, Value::Array(Vec::new())),
        ("loadouts", loadouts, Value::Array(Vec::new())),
        ("plugins", plugins, Value::Array(Vec::new())),
        ("pluginCustomStorage", plugin_storage, Value::Object(Map::new())),
    ] {
        let value = separate.or_else(|| root.shift_remove(key)).filter(|v| !v.is_null()).unwrap_or(default);
        root.insert(key.to_owned(), value);
    }
    if let Some(Value::Object(storage)) = root.get_mut("pluginCustomStorage") {
        let pointer = storage.shift_remove(cold_plugin_storage::INDEX_KEY);
        let legacy = storage.shift_remove(cold_plugin_storage::LEGACY_MAP_KEY);
        if !sink.expands_cold_payloads() {
            let markers = u64::from(pointer.is_some()) + u64::from(legacy.is_some());
            if markers != 0 { job.record_restore_omission("pluginStorage", markers).map_err(|error| job_error(job, error))?; }
        } else if cold_plugin_storage::references(pointer.as_ref(), legacy.as_ref()) {
            cold_plugin_storage::import(storage, pointer, legacy, sink, job, limits)?;
            reader.report_progress()?;
        }
    }
    if let Some(meta) = plugin_storage_meta {
        root.insert("pluginStorageMeta".to_owned(), meta);
    }
    let mut presets = match presets {
        Some(presets) => presets,
        None => match root.shift_remove("botPresets") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => values,
            _ => return Err(invalid("root botPresets must be an array")),
        },
    };
    pocket_features::root(&mut root);
    root.shift_remove("account");
    upstream_import::prepare_root(&mut root, &excluded);
    upstream_import::prepare_presets(&mut presets);
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

fn skip_block<R: Read>(reader: &mut TrackedReader<'_, R>, name: &str, compression: u8, encoded_length: u64, limits: RestoreLimits, job: &dyn RestoreControl) -> Result<(), NativeJobError> {
    let block = EncodedBlockReader { reader, remaining: encoded_length };
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    if compression == 0 {
        let mut decoded = DecodedLimitReader::new(block, limits.max_decoded_block_bytes, job, name);
        while decoded.read(&mut buffer).map_err(|error| gzip_io_error(name, error, job))? != 0 {}
    } else {
        let decoder = GzDecoder::new(BufReader::with_capacity(READ_CHUNK_BYTES, block));
        let mut decoded = DecodedLimitReader::new(decoder, limits.max_decoded_block_bytes, job, name);
        while decoded.read(&mut buffer).map_err(|error| gzip_io_error(name, error, job))? != 0 {}
        let buffered = decoded.into_inner().into_inner();
        if !buffered.buffer().is_empty() || buffered.get_ref().remaining != 0 { return Err(corrupt(format!("trailing data in gzip block {name}"))); }
    }
    Ok(())
}

fn read_block_value<R: Read>(
    reader: &mut TrackedReader<'_, R>,
    name: &str,
    compression: u8,
    encoded_length: u64,
    limits: RestoreLimits,
    job: &dyn RestoreControl,
    blank_is_empty_object: bool,
) -> Result<(Value, usize), NativeJobError> {
    let block = EncodedBlockReader {
        reader,
        remaining: encoded_length,
    };
    if compression == 0 {
        if encoded_length > limits.max_decoded_block_bytes {
            return Err(limit_error(format!("decoded block limit exceeded for {name}")));
        }
        let decoded = DecodedLimitReader::new(block, limits.max_decoded_block_bytes, job, name);
        let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
        let value = read_block_json(&mut buffered, blank_is_empty_object)
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
    let value = read_block_json(&mut json_reader, blank_is_empty_object)
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

// Upstream writes an unset pluginCustomStorage as an empty body and its decoder
// skips the block, which leaves the storage empty.
fn read_block_json<R: BufRead>(
    reader: &mut R,
    blank_is_empty_object: bool,
) -> Result<Value, serde_json::Error> {
    if blank_is_empty_object {
        loop {
            let buffer = reader.fill_buf().map_err(serde_json::Error::io)?;
            if buffer.is_empty() {
                return Ok(Value::Object(Map::new()));
            }
            let blank = buffer
                .iter()
                .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
                .count();
            let content = blank < buffer.len();
            reader.consume(blank);
            if content {
                break;
            }
        }
    }
    serde_json::from_reader(reader)
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
    spool_directory: &Path,
) -> Result<bool, NativeJobError> {
    let block = EncodedBlockReader {
        reader,
        remaining: encoded_length,
    };
    let staging_error = RefCell::new(None);
    let record_bytes = RecordBudget::new(limits.max_record_bytes);
    let retained = Cell::new(true);
    let seed = CharacterSeed {
        retained: &retained,
        chats_to_skip: 0,
        limits,
        record_bytes: &record_bytes,
        character_name: "",
        spool_directory,
        staging_id,
        expected_id: name,
        character_index,
        job,
        sink,
        staging_error: &staging_error,
    };
    if compression == 0 {
        if encoded_length > limits.max_decoded_block_bytes {
            return Err(limit_error(format!("decoded block limit exceeded for {name}")));
        }
        let decoded = DecodedLimitReader::new(block, limits.max_decoded_block_bytes, job, name);
        let mut buffered = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
        if let Err(error) = read_character_json(&mut buffered, seed) {
            if let Some(error) = staging_error.take() {
                return Err(store_error(error));
            }
            return Err(json_error(name, compression, error, job));
        }
        if !buffered.buffer().is_empty() || buffered.get_ref().inner.remaining != 0 {
            return Err(corrupt(format!("trailing data in block {name}")));
        }
        return Ok(retained.get());
    }

    let buffered = BufReader::with_capacity(READ_CHUNK_BYTES, block);
    let decoder = GzDecoder::new(buffered);
    let decoded = DecodedLimitReader::new(decoder, limits.max_decoded_block_bytes, job, name);
    let mut json_reader = BufReader::with_capacity(READ_CHUNK_BYTES, decoded);
    if let Err(error) = read_character_json(&mut json_reader, seed) {
        if let Some(error) = staging_error.take() {
            return Err(store_error(error));
        }
        return Err(json_error(name, compression, error, job));
    }
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
    Ok(retained.get())
}

pub(super) struct RecordBudget {
    bytes: Cell<Option<u64>>,
    limit: u64,
}

impl RecordBudget {
    pub(super) fn new(limit: u64) -> Self { Self { bytes: Cell::new(None), limit } }
    pub(super) fn set(&self, bytes: Option<u64>) { self.bytes.set(bytes); }
    fn get(&self) -> Option<u64> { self.bytes.get() }
}

pub(super) struct RecordLimitReader<'a, R> {
    pub(super) inner: R,
    pub(super) bytes: &'a RecordBudget,
}

impl<R: Read> Read for RecordLimitReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if let Some(bytes) = self.bytes.get() {
            let bytes = bytes.saturating_add(read as u64);
            self.bytes.set(Some(bytes));
            if bytes > self.bytes.limit {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "restore record limit exceeded"));
            }
        }
        Ok(read)
    }
}

pub(super) struct BoundedValueSeed<'a>(pub(super) &'a RecordBudget);
pub(super) struct BoundedStringSeed<'a>(pub(super) &'a RecordBudget);

impl<'de> DeserializeSeed<'de> for BoundedStringSeed<'_> {
    type Value = String;
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<String, D::Error> {
        self.0.set(Some(0));
        let value = String::deserialize(deserializer);
        self.0.set(None);
        value
    }
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.0.set(Some(0));
        let value = Value::deserialize(deserializer);
        self.0.set(None);
        value
    }
}

fn retained_json_bytes<E: de::Error>(value: &impl serde::Serialize, limit: u64) -> Result<u64, E> {
    struct SizeWriter(u64, u64);
    impl Write for SizeWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            if self.0 > self.1 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "restore record limit exceeded"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut size = SizeWriter(0, limit);
    serde_json::to_writer(&mut size, value).map_err(E::custom)?;
    Ok(size.0)
}

fn insert_detail<E: de::Error>(detail: &mut Map<String, Value>, limit: u64, key: String, value: Value) -> Result<(), E> {
    retained_json_bytes::<E>(&key, limit)?;
    retained_json_bytes::<E>(&value, limit)?;
    detail.insert(key, value);
    Ok(())
}

#[derive(Default)]
struct CharacterMetadata {
    kind: Option<String>,
    character_id: Option<String>,
    name: String,
    cold_key: Option<String>,
    chats_count: u64,
    envelope_occurrence: u64,
}

struct CharacterMetadataSeed<'a> {
    wrapped: bool,
    record_bytes: &'a RecordBudget,
}

impl<'de> DeserializeSeed<'de> for CharacterMetadataSeed<'_> {
    type Value = Option<CharacterMetadata>;
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        let record_bytes = self.record_bytes;
        record_bytes.set(Some(0));
        let result = deserializer.deserialize_any(self);
        record_bytes.set(None);
        result
    }
}

impl<'de> Visitor<'de> for CharacterMetadataSeed<'_> {
    type Value = Option<CharacterMetadata>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("a cold character payload") }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok(None) }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok(None) }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok(None) }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok(None) }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok(None) }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(None) }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        self.record_bytes.set(None);
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        self.record_bytes.set(None);
        let mut metadata = (!self.wrapped).then(CharacterMetadata::default);
        let mut character_count = 0;
        while let Some(key) = map.next_key_seed(BoundedStringSeed(self.record_bytes))? {
            if self.wrapped && key == "character" {
                character_count += 1;
                metadata = map.next_value_seed(CharacterMetadataSeed { wrapped: false, record_bytes: self.record_bytes })?;
                if let Some(metadata) = metadata.as_mut() { metadata.envelope_occurrence = character_count; }
            } else if !self.wrapped && key == "chats" {
                metadata.as_mut().unwrap().chats_count += 1;
                map.next_value::<IgnoredAny>()?;
            } else if !self.wrapped && matches!(key.as_str(), "chaId" | "name" | "coldstorage" | "type") {
                let value = map.next_value_seed(BoundedValueSeed(self.record_bytes))?;
                let metadata = metadata.as_mut().unwrap();
                match key.as_str() {
                    "type" => metadata.kind = value.as_str().map(str::to_owned),
                    "chaId" => metadata.character_id = value.as_str().map(str::to_owned),
                    "name" => metadata.name = value.as_str().unwrap_or_default().to_owned(),
                    _ => metadata.cold_key = value.as_str().filter(|key| !key.is_empty()).map(str::to_owned),
                }
            } else { map.next_value::<IgnoredAny>()?; }
        }
        Ok(metadata)
    }
}

fn staging_io<E: de::Error>(error: io::Error, staging_error: &RefCell<Option<StoreError>>) -> E {
    let message = error.to_string();
    *staging_error.borrow_mut() = Some(StoreError::from(error));
    E::custom(message)
}

fn read_character_metadata(file: &mut File, seed: &CharacterSeed<'_>, wrapped: bool) -> Result<Option<CharacterMetadata>, serde_json::Error> {
    file.rewind().map_err(|error| staging_io(error, seed.staging_error))?;
    let reader = DecodedLimitReader::new(file, seed.limits.max_decoded_block_bytes, seed.job, "cold character");
    let reader = RecordLimitReader { inner: BufReader::new(reader), bytes: seed.record_bytes };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let metadata = CharacterMetadataSeed { wrapped, record_bytes: seed.record_bytes }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(metadata)
}

fn read_character_json<R: Read>(reader: &mut R, seed: CharacterSeed<'_>) -> Result<(), serde_json::Error> {
    // Type and cold references may follow chats; inspect them before staging rows.
    let mut spool = tempfile::tempfile_in(seed.spool_directory).map_err(|error| staging_io(error, seed.staging_error))?;
    let mut buffer = [0u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer).map_err(serde_json::Error::io)?;
        if read == 0 { break; }
        spool.write_all(&buffer[..read]).map_err(|error| staging_io(error, seed.staging_error))?;
    }
    let metadata = read_character_metadata(&mut spool, &seed, false)?
        .ok_or_else(|| <serde_json::Error as de::Error>::custom("a character object is required"))?;
    if metadata.character_id.as_deref() != Some(seed.expected_id) {
        return Err(<serde_json::Error as de::Error>::custom("character block name does not match chaId"));
    }
    if upstream_import::excluded_character(metadata.kind.as_deref(), metadata.character_id.as_deref()) {
        seed.job.record_restore_omission("excludedCharacters", 1).map_err(<serde_json::Error as de::Error>::custom)?;
        seed.retained.set(false);
        return Ok(());
    }
    let mut name = metadata.name;
    let mut chats_to_skip = metadata.chats_count.saturating_sub(1);
    let mut envelope_occurrence = 0;
    let mut wrapped = false;
    if let Some(key) = metadata.cold_key {
        let path = seed.sink.cold_payload_path(&key).map_err(|error| {
            *seed.staging_error.borrow_mut() = Some(error);
            <serde_json::Error as de::Error>::custom("cold payload lookup failed")
        })?;
        if let Some(path) = path {
            let mut payload = File::open(path).map_err(|error| staging_io(error, seed.staging_error))?;
            if let Some(metadata) = read_character_metadata(&mut payload, &seed, true)? {
                if upstream_import::excluded_character(metadata.kind.as_deref(), metadata.character_id.as_deref()) {
                    seed.job.record_restore_omission("excludedCharacters", 1).map_err(<serde_json::Error as de::Error>::custom)?;
                    seed.retained.set(false);
                    return Ok(());
                }
                name = metadata.name;
                chats_to_skip = metadata.chats_count.saturating_sub(1);
                envelope_occurrence = metadata.envelope_occurrence;
                spool = payload;
                wrapped = true;
            }
        }
        if !wrapped {
            if !seed.sink.expands_cold_payloads() { seed.job.record_restore_omission("missing", 1).map_err(<serde_json::Error as de::Error>::custom)?; }
            seed.sink.unavailable_cold_payload(&key, &name).map_err(|error| {
                *seed.staging_error.borrow_mut() = Some(error);
                <serde_json::Error as de::Error>::custom("cold payload preview failed")
            })?;
        }
    }
    spool.rewind().map_err(|error| staging_io(error, seed.staging_error))?;
    let seed = CharacterSeed { character_name: &name, chats_to_skip, ..seed };
    let reader = DecodedLimitReader::new(spool, seed.limits.max_decoded_block_bytes, seed.job, "cold character");
    let reader = RecordLimitReader { inner: BufReader::new(reader), bytes: seed.record_bytes };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    if wrapped { deserializer.deserialize_map(ColdCharacterVisitor(seed, envelope_occurrence))?; }
    else { seed.deserialize(&mut deserializer)?; }
    deserializer.end()
}

struct ColdCharacterVisitor<'a>(CharacterSeed<'a>, u64);
impl<'de> Visitor<'de> for ColdCharacterVisitor<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("a cold character envelope") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let record_bytes = self.0.record_bytes;
        let mut seed = Some(self.0);
        let mut occurrence = 0;
        while let Some(key) = map.next_key_seed(BoundedStringSeed(record_bytes))? {
            if key == "character" {
                occurrence += 1;
                if occurrence == self.1 {
                    map.next_value_seed(seed.take().ok_or_else(|| de::Error::custom("cold character already staged"))?)?;
                } else { map.next_value::<IgnoredAny>()?; }
            } else { map.next_value::<IgnoredAny>()?; }
        }
        if seed.is_some() { return Err(de::Error::custom("cold character is missing")); }
        Ok(())
    }
}

trait IgnoredJsonValue: Sized {
    fn ignored<E: de::Error>() -> Result<Self, E>;
}

impl IgnoredJsonValue for i64 {
    fn ignored<E: de::Error>() -> Result<Self, E> {
        Ok(0)
    }
}

impl IgnoredJsonValue for bool {
    fn ignored<E: de::Error>() -> Result<Self, E> {
        Ok(false)
    }
}

macro_rules! ignored_visits {
    (@scalars $value:ty) => {
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
    };
    ($value:ty) => {
        ignored_visits!(@scalars $value);

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
    retained: &'a Cell<bool>,
    chats_to_skip: u64,
    limits: RestoreLimits,
    record_bytes: &'a RecordBudget,
    character_name: &'a str,
    spool_directory: &'a Path,
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
        let mut chats_to_skip = self.0.chats_to_skip;
        while let Some(key) = map.next_key_seed(BoundedStringSeed(self.0.record_bytes))? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading character"));
            }
            if key == "chats" {
                if chats_to_skip != 0 {
                    chats_to_skip -= 1;
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
                if saw_chats {
                    return Err(de::Error::custom("duplicate chats field in character"));
                }
                saw_chats = true;
                conversation_count = map.next_value_seed(ChatsSeed {
                    limits: self.0.limits,
                    record_bytes: self.0.record_bytes,
                    character_name: self.0.character_name,
                    spool_directory: self.0.spool_directory,
                    staging_id: self.0.staging_id,
                    character_id: self.0.expected_id,
                    character_index: self.0.character_index,
                    job: self.0.job,
                    sink: self.0.sink,
                    staging_error: self.0.staging_error,
                })?;
            } else {
                let value = map.next_value_seed(BoundedValueSeed(self.0.record_bytes))?;
                insert_detail::<A::Error>(&mut detail, self.0.record_bytes.limit, key, value)?;
            }
        }
        detail.remove("coldstorage");
        detail.remove("coldStoragedChats");
        let character_id = detail
            .get("chaId")
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("character block requires chaId"))?;
        if character_id != self.0.expected_id {
            return Err(de::Error::custom(
                "character block name does not match chaId",
            ));
        }
        default_name(&mut detail);
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
    limits: RestoreLimits,
    record_bytes: &'a RecordBudget,
    character_name: &'a str,
    spool_directory: &'a Path,
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
        let seen_ids = RefCell::new(HashSet::new());
        while let Some(staged) = sequence
            .next_element_seed(ConversationSeed {
                limits: self.0.limits,
                record_bytes: self.0.record_bytes,
                character_name: self.0.character_name,
                spool_directory: self.0.spool_directory,
                seen_ids: &seen_ids,
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
        {
            count += i64::from(staged);
        }
        Ok(count)
    }

    ignored_visits!(i64);
}

struct ConversationSeed<'a> {
    limits: RestoreLimits,
    record_bytes: &'a RecordBudget,
    character_name: &'a str,
    spool_directory: &'a Path,
    seen_ids: &'a RefCell<HashSet<String>>,
    staging_id: &'a str,
    character_id: &'a str,
    configured_index: i64,
    fallback: String,
    job: &'a dyn RestoreControl,
    sink: &'a dyn ReplacementSink,
    staging_error: &'a RefCell<Option<StoreError>>,
}

// Stages one conversation; a chat entry that is not an object is skipped, as in PocketRisu.
impl<'de> DeserializeSeed<'de> for ConversationSeed<'_> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(ConversationVisitor(self))
    }
}

struct ConversationVisitor<'a>(ConversationSeed<'a>);

impl<'de> Visitor<'de> for ConversationVisitor<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a conversation object")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(false)
    }

    ignored_visits!(@scalars bool);

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut detail = Map::new();
        let mut messages = MessageSpool::empty();
        let mut saw_messages = false;
        while let Some(key) = map.next_key_seed(BoundedStringSeed(self.0.record_bytes))? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading conversation"));
            }
            if key == "message" {
                if saw_messages && !self.0.sink.expands_cold_payloads() {
                    return Err(de::Error::custom("duplicate message field in conversation"));
                }
                saw_messages = true;
                messages = map.next_value_seed(MessagesSeed {
                    record_bytes: self.0.record_bytes,
                    spool_directory: self.0.spool_directory,
                    staging_error: self.0.staging_error,
                    job: self.0.job,
                })?;
            } else {
                let value = map.next_value_seed(BoundedValueSeed(self.0.record_bytes))?;
                insert_detail::<A::Error>(&mut detail, self.0.record_bytes.limit, key, value)?;
            }
        }
        {
            if let Some(key) = messages.cold_key.take() {
                let path = self.0.sink.cold_payload_path(&key).map_err(|error| {
                    *self.0.staging_error.borrow_mut() = Some(error);
                    de::Error::custom("cold payload lookup failed")
                })?;
                let payload = match path {
                    Some(path) => read_cold_chat(&path, &self.0).map_err(de::Error::custom)?,
                    None => None,
                };
                if let Some((expanded, fields)) = payload {
                    messages = expanded;
                    if let Some(mut fields) = fields {
                        for field in ["savedToggleValues", "bindedPersona"] {
                            if let Some(value) = fields.remove(field) { detail.insert(field.to_owned(), value); }
                        }
                        for field in ["hypaV2Data", "hypaV3Data", "scriptstate", "localLore"] {
                            detail.remove(field);
                            if let Some(value) = fields.remove(field) { detail.insert(field.to_owned(), value); }
                        }
                    }
                } else {
                    if !self.0.sink.expands_cold_payloads() { self.0.job.record_restore_omission("missing", 1).map_err(de::Error::custom)?; }
                    self.0.sink.unavailable_cold_payload(&key, self.0.character_name).map_err(|error| {
                        *self.0.staging_error.borrow_mut() = Some(error);
                        de::Error::custom("cold payload preview failed")
                    })?;
                    messages = MessageSpool::empty();
                }
            }
        }
        let conversation_id = match legacy_chat_id(detail.get("id")) {
            Some(id) if !id.is_empty() && self.0.seen_ids.borrow_mut().insert(id.clone()) => id,
            _ => loop {
                let id = uuid::Uuid::new_v4().to_string();
                if self.0.seen_ids.borrow_mut().insert(id.clone()) { break id; }
            },
        };
        detail.insert("id".into(), Value::String(conversation_id.clone()));
        default_name(&mut detail);
        let mut normalized_detail = Value::Object(detail);
        pocket_features::chat(&mut normalized_detail, &self.0.fallback);
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
            messages.count,
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
            .map_err(de::Error::custom)?;
        Ok(true)
    }
}

type ColdChatPayload = (MessageSpool, Option<Map<String, Value>>);

fn read_cold_chat(path: &Path, seed: &ConversationSeed<'_>) -> Result<Option<ColdChatPayload>, serde_json::Error> {
    let file = File::open(path).map_err(|error| staging_io(error, seed.staging_error))?;
    let reader = DecodedLimitReader::new(file, seed.limits.max_decoded_block_bytes, seed.job, "cold chat");
    let reader = RecordLimitReader { inner: BufReader::new(reader), bytes: seed.record_bytes };
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let payload = deserializer.deserialize_any(ColdChatVisitor(MessagesSeed {
        record_bytes: seed.record_bytes,
        spool_directory: seed.spool_directory,
        staging_error: seed.staging_error,
        job: seed.job,
    }))?;
    deserializer.end()?;
    Ok(payload)
}

struct ColdChatVisitor<'a>(MessagesSeed<'a>);

impl<'de> Visitor<'de> for ColdChatVisitor<'_> {
    type Value = Option<ColdChatPayload>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("a cold chat object or message array") }
    fn visit_seq<A: SeqAccess<'de>>(self, sequence: A) -> Result<Self::Value, A::Error> {
        MessagesVisitor(self.0).visit_seq(sequence).map(|messages| Some((messages, None)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut messages = None;
        let mut fields = Map::new();
        while let Some(key) = map.next_key_seed(BoundedStringSeed(self.0.record_bytes))? {
            if key == "message" {
                messages = Some(map.next_value_seed(MessagesSeed { ..self.0 })?);
            } else if matches!(key.as_str(), "savedToggleValues" | "bindedPersona" | "hypaV2Data" | "hypaV3Data" | "scriptstate" | "localLore") {
                let value = map.next_value_seed(BoundedValueSeed(self.0.record_bytes))?;
                insert_detail::<A::Error>(&mut fields, self.0.record_bytes.limit, key, value)?;
            } else { map.next_value::<IgnoredAny>()?; }
        }
        Ok(messages.filter(|messages| messages.is_array).map(|messages| (messages, Some(fields))))
    }
}

struct MessageSpool {
    file: Option<File>,
    count: i64,
    last_time: Option<i64>,
    cold_key: Option<String>,
    is_array: bool,
    record_limit: u64,
}

impl IgnoredJsonValue for MessageSpool {
    fn ignored<E: de::Error>() -> Result<Self, E> {
        Ok(Self::empty())
    }
}

impl MessageSpool {
    fn empty() -> Self {
        Self { file: None, count: 0, last_time: None, cold_key: None, is_array: false, record_limit: RESTORE_RECORD_BYTES }
    }
    fn new(directory: &Path, record_limit: u64) -> io::Result<Self> {
        Ok(Self {
            file: Some(tempfile::tempfile_in(directory)?),
            count: 0,
            last_time: None,
            cold_key: None,
            is_array: true,
            record_limit,
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
        let Some(file) = self.file.as_mut() else { return Ok(()); };
        let spool_error = |error: io::Error| {
            let message = error.to_string();
            *staging_error.borrow_mut() = Some(StoreError::from(error));
            message
        };
        let mut page = Vec::with_capacity(MESSAGE_PAGE_COUNT);
        let mut page_bytes = 0u64;
        let mut start = 0i64;
        file.rewind().map_err(&spool_error)?;
        let mut reader = BufReader::with_capacity(READ_CHUNK_BYTES, file);
        for index in 0..self.count {
            if job.is_cancel_requested() {
                return Err("restore cancelled while staging messages".to_owned());
            }
            let mut length = [0u8; 4];
            reader.read_exact(&mut length).map_err(&spool_error)?;
            let length = u32::from_le_bytes(length) as u64;
            if length > self.record_limit {
                return Err(spool_error(io::Error::new(io::ErrorKind::InvalidData, "message spool record limit exceeded")));
            }
            let mut bytes = vec![0u8; length as usize];
            reader.read_exact(&mut bytes).map_err(&spool_error)?;
            let mut message: Value =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            pocket_features::message(
                &mut message,
                &format!("{conversation_id}:response:{index}"),
            );
            let length = retained_json_bytes::<serde_json::Error>(&message, self.record_limit).map_err(|error| error.to_string())?;
            if !page.is_empty()
                && (page.len() >= MESSAGE_PAGE_COUNT
                    || page_bytes.saturating_add(length) > MESSAGE_PAGE_BYTES)
            {
                sink.add_conversation_messages(staging_id, character_id, conversation_id, start, &page)
                    .map_err(|error| {
                        let message = error.to_string();
                        *staging_error.borrow_mut() = Some(error);
                        message
                    })?;
                start += page.len() as i64;
                page.clear();
                page_bytes = 0;
            }
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
    record_bytes: &'a RecordBudget,
    spool_directory: &'a Path,
    staging_error: &'a RefCell<Option<StoreError>>,
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
        let spool_error = |error: io::Error| -> A::Error {
            let message = error.to_string();
            *self.0.staging_error.borrow_mut() = Some(StoreError::from(error));
            de::Error::custom(message)
        };
        let mut spool = MessageSpool::new(self.0.spool_directory, self.0.record_bytes.limit).map_err(&spool_error)?;
        let mut file = io::BufWriter::with_capacity(READ_CHUNK_BYTES, spool.file.take().expect("created message spool"));
        while let Some(message) = sequence.next_element_seed(BoundedValueSeed(self.0.record_bytes))? {
            if self.0.job.is_cancel_requested() {
                return Err(de::Error::custom("restore cancelled while reading messages"));
            }
            if spool.count == 0 {
                spool.cold_key = message.get("data").and_then(Value::as_str)
                    .and_then(|data| data.strip_prefix("\u{ef01}COLDSTORAGE\u{ef01}")).map(str::to_owned);
            }
            let bytes = serde_json::to_vec(&message).map_err(|error| {
                *self.0.staging_error.borrow_mut() = Some(StoreError::Store { message: error.to_string() });
                de::Error::custom(error)
            })?;
            if bytes.len() as u64 > self.0.record_bytes.limit { return Err(de::Error::custom("restore record limit exceeded")); }
            file.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(&spool_error)?;
            file.write_all(&bytes).map_err(&spool_error)?;
            spool.count = spool.count.checked_add(1).ok_or_else(|| de::Error::custom("message count overflow"))?;
            spool.last_time = message.get("time").and_then(Value::as_i64);
        }
        // Replay rewinds and reads the file, so every buffered record must reach it first.
        spool.file = Some(file.into_inner().map_err(|error| spool_error(error.into_error()))?);
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
            None => {
                let cold = self.scale.cold_payloads.as_ref().map_or(0, |cold| cold.items.load(Ordering::Acquire));
                (self.completed_items.saturating_add(cold), None)
            }
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
        self.report_progress()?;
        let cold = self.scale.cold_payloads.as_ref().map_or(0, |cold| cold.bytes.load(Ordering::Acquire));
        self.job
            .set_detail(JobDetail::new(
                self.stage,
                StageUnit::Bytes,
                self.completed.saturating_add(cold),
                // Cold payloads not read yet have no known size.
                (cold == 0).then_some(self.total),
                self.counts.clone(),
            ))
            .map_err(|error| job_error(self.job, error))
    }

    /// Reports the top-level counters alone, for cold payloads read after the
    /// database bytes, so the item stage keeps its detail.
    fn report_progress(&self) -> Result<(), NativeJobError> {
        self.job
            .set_progress(self.top_level_progress())
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
        return limit_error(message);
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
        return limit_error(message);
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
    if message.contains("decoded block limit exceeded") || message.contains("restore record limit exceeded") {
        return limit_error(message);
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
        return limit_error(error.to_string());
    }
    corrupt(format!("invalid gzip in block {name}: {error}"))
}

fn limit_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::too_large(if message.as_ref().contains("record limit") { "record" } else { "backup" }, message)
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
pub(super) fn store_error(error: StoreError) -> NativeJobError {
    match error {
        StoreError::RevisionConflict { expected, actual } => NativeJobError::new(
            "revision-conflict",
            format!("revision conflict: expected {expected}, actual {actual}"),
        ),
        StoreError::SnapshotReleased => {
            NativeJobError::new("store-error", "persistent snapshot was released")
        }
        StoreError::RawBodyUnavailable | StoreError::CommitBusy => NativeJobError::new("store-error", error.to_string()),
        // A sync binding switch before activation refuses the commit like a concurrent revision.
        StoreError::Validation { message } if message == "binding-authority-changed" => {
            NativeJobError::new("revision-conflict", message)
        }
        StoreError::Committed { message, .. } | StoreError::CommitDecode { message } | StoreError::Validation { message } | StoreError::SchemaMismatch { message } | StoreError::Store { message } => {
            NativeJobError::new("store-error", message)
        }
    }
}

#[cfg(test)]
mod legacy_stream_tests;

#[cfg(test)]
pub(super) mod tests {
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

    include!("restore/resource_measurements.rs");

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

    pub(crate) struct StoreSink {
        pub(crate) store: Mutex<PersistentStore>,
        incremental: bool,
        prepared_characters: std::collections::HashMap<String, Value>,
        fail_character_batches: bool,
        record_activation: bool,
        lose_commit_reply: bool,
        switch_binding_before_commit: bool,
        pub(crate) abort_calls: AtomicUsize,
        preserve_calls: AtomicUsize,
        full_character_calls: AtomicUsize,
        incremental_character_calls: AtomicUsize,
        max_message_page: AtomicUsize,
        message_pages: Mutex<Vec<usize>>,
        cold_payloads: Option<std::collections::HashMap<String, PathBuf>>,
        unavailable_cold_payloads: std::collections::HashMap<String, ColdPayloadUnavailable>,
        unavailable_cold: Mutex<Vec<(String, String)>>,
        cancel_after_page: Option<(Arc<JobRegistry>, String)>,
    }

    impl ReplacementSink for StoreSink {
        fn prepare_activation_receipt(&self, staging_id: &str, job: &JobControl, result: &JobResultSummary) -> StoreResult<Option<String>> {
            if !self.record_activation { return Ok(None); }
            super::super::restore_receipts::prepare(&self.store.lock().unwrap(), job, staging_id, result)
        }

        fn activation_receipt(&self, staging_id: &str, authority: &str) -> StoreResult<Option<i64>> {
            self.store.lock().unwrap().lww_file_restore_receipt(staging_id, staging_id, authority)
        }

        fn begin(&self) -> StoreResult<StagingResult> {
            self.store.lock().unwrap().replace_begin()
        }

        fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
            self.store
                .lock()
                .unwrap()
                .replace_put_upstream_root(staging_id, root)
        }

        fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
            self.store
                .lock()
                .unwrap()
                .replace_put_upstream_presets(staging_id, presets)
        }

        fn put_legacy_root(&self, staging_id: &str, root: &RootSpool) -> StoreResult<()> {
            self.store.lock().unwrap().replace_put_upstream_stream(staging_id, root)
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
            self.incremental
        }

        fn prepare_character(&self, character: Value) -> StoreResult<Value> {
            Ok(character.get("chaId").and_then(Value::as_str)
                .and_then(|id| self.prepared_characters.get(id)).cloned().unwrap_or(character))
        }

        fn expands_cold_payloads(&self) -> bool { self.cold_payloads.is_some() }

        fn cold_payload_path(&self, key: &str) -> StoreResult<Option<PathBuf>> {
            Ok(self.cold_payloads.as_ref().and_then(|payloads| payloads.get(key)).cloned())
        }

        fn unavailable_cold_payload_reason(&self, key: &str) -> Option<ColdPayloadUnavailable> {
            self.unavailable_cold_payloads.get(key).copied()
        }

        fn unavailable_cold_payload(&self, key: &str, name: &str) -> StoreResult<()> {
            self.unavailable_cold.lock().unwrap().push((key.to_owned(), name.to_owned()));
            Ok(())
        }

        fn incomplete_restore_preview(&self) -> StoreResult<super::super::IncompleteRestorePreview> {
            let mut preview = super::super::IncompleteRestorePreview::default();
            for (key, name) in self.unavailable_cold.lock().unwrap().iter() {
                preview.unavailable_cold_keys.push(key.clone());
                preview.character_names.push(name.clone());
            }
            Ok(preview)
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
            self.message_pages.lock().unwrap().push(messages.len());
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
            )?;
            if let Some((registry, id)) = &self.cancel_after_page { registry.cancel(id).unwrap(); }
            Ok(())
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

        fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
            let mut store = self.store.lock().unwrap();
            if self.switch_binding_before_commit {
                use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
                let before = store.lww_binding_state().unwrap();
                store.switch_lww_binding(&SwitchBindingRequest {
                    initial_publication: false,
                    header: crate::persistent_store::lww::Header { binding_authority: before.target_authority, request_id: "synthetic-switch-before-restore".into() },
                    expected_selection_epoch: before.selection_epoch, target: SyncTarget::None, inspection_id: None,
                }).unwrap();
            }
            let prepared = store.prepare_replace_commit(staging_id, Some(expected_revision))?;
            let result = store.finish_prepared_replace_under(prepared, binding_authority)?;
            if self.lose_commit_reply { return Err(StoreError::Store { message: "Synthetic post-commit cleanup failure".into() }); }
            Ok(result)
        }

        fn abort(&self, staging_id: &str) -> StoreResult<()> {
            self.abort_calls.fetch_add(1, Ordering::AcqRel);
            self.store.lock().unwrap().replace_abort(staging_id)
        }
    }

    pub(crate) fn fixture() -> (TempDir, StoreSink) {
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
                incremental: true,
                prepared_characters: Default::default(),
                fail_character_batches: false,
                record_activation: false,
                lose_commit_reply: false,
                switch_binding_before_commit: false,
                abort_calls: AtomicUsize::new(0),
                preserve_calls: AtomicUsize::new(0),
                full_character_calls: AtomicUsize::new(0),
                incremental_character_calls: AtomicUsize::new(0),
                max_message_page: AtomicUsize::new(0),
                message_pages: Mutex::new(Vec::new()),
                cold_payloads: None,
                unavailable_cold_payloads: Default::default(),
                unavailable_cold: Mutex::new(Vec::new()),
                cancel_after_page: None,
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

    pub(crate) fn block(block_type: u8, compressed: bool, name: &str, value: &Value) -> Vec<u8> {
        let json = serde_json::to_vec(value).unwrap();
        let payload = if compressed { gzip(&json) } else { json };
        raw_block(block_type, u8::from(compressed), name, &payload)
    }

    pub(crate) fn valid_blocks() -> Vec<Vec<u8>> {
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

    pub(crate) fn save_bytes(blocks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
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

    fn cold_block_source(directory: &Path, body: &[u8], compressed: bool) -> PathBuf {
        let mut blocks = valid_blocks();
        let encoded = if compressed { gzip(body) } else { body.to_vec() };
        blocks[6] = raw_block(2, u8::from(compressed), "char-1", &encoded);
        let source = directory.join("cold.risudat");
        fs::write(&source, save_bytes(blocks)).unwrap();
        source
    }

    fn write_cold_payload(directory: &Path, key: &str, value: &Value) -> (String, PathBuf) {
        let path = directory.join(format!("{key}.json"));
        serde_json::to_writer(File::create(&path).unwrap(), value).unwrap();
        (key.to_owned(), path)
    }

    fn cold_chat(key: &str) -> Value {
        json!({"id":"chat-1", "name":"Chat", "message":[{"role":"char", "data":format!("\u{ef01}COLDSTORAGE\u{ef01}{key}")}]})
    }

    #[test]
    fn upstream_import_discards_groups_and_rooms_before_staging_their_chats() {
        for (compressed, incremental) in [(false, false), (true, false), (false, true), (true, true)] {
            let (directory, mut sink) = fixture();
            sink.incremental = incremental;
            let mut blocks = valid_blocks();
            blocks[0] = block(1, false, "root", &json!({
                "__directory": ["preset", "modules", "loadouts", "plugins", "pluginStorage", "char-1", "group", "§temp", "config"],
                "characterOrder": ["group", "§temp", {"id":"folder","data":["group","char-1"]}],
                "groupTemplate": "{{char}}: {{slot}}"
            }));
            blocks[1] = block(4, false, "preset", &json!([{"name":"Preset","groupOtherBotRole":"system"}]));
            blocks[3] = block(10, false, "loadouts", &json!([{"characterIds":["group","char-1","§temp"]}]));
            let group = br#"{"chaId":"group","name":"Group","chats":[{"id":"skipped","message":[{"role":"user","data":"synthetic"}]}],"characters":["char-1"],"type":"group"}"#;
            let payload = if compressed { gzip(group) } else { group.to_vec() };
            blocks.push(raw_block(2, u8::from(compressed), "group", &payload));
            blocks.push(block(2, compressed, "§temp", &json!({"type":"character","chaId":"§temp","name":"Room","chats":[]})));
            let source = directory.path().join("upstream.risudat");
            fs::write(&source, save_bytes(blocks)).unwrap();
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            assert_eq!(result.character_count, 1);
            assert_eq!(job.status().incomplete_restore_preview.unwrap().omissions.excluded_characters, 2);
            assert!(result.warning_codes.contains(&"excluded-backup-characters:2".to_owned()));
            assert_eq!(job.status().detail.unwrap().counts.skipped, 0);
            assert_eq!(restored["characters"].as_array().unwrap().len(), 1);
            assert_eq!(restored["characters"][0]["chaId"], "char-1");
            assert_eq!(restored["characterOrder"], json!([{"id":"folder","data":["char-1"]}]));
            assert_eq!(restored["loadouts"][0]["characterIds"], json!(["char-1"]));
            assert_eq!(restored["messageNameTemplate"], "{{char}}: {{slot}}");
            assert_eq!(restored["botPresets"][0]["namedMessageRole"], "system");
            assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), usize::from(incremental));
        }
    }

    #[test]
    fn upstream_import_discards_a_group_restored_from_a_cold_character() {
        for incremental in [false, true] {
        let (directory, mut sink) = fixture();
        sink.incremental = incremental;
        sink.prepared_characters.insert("char-1".to_owned(), json!({"chaId":"char-1","name":"Group","type":"group","chats":[]}));
        sink.cold_payloads = Some(std::collections::HashMap::from([
            write_cold_payload(directory.path(), "character", &json!({"character":{
                "chaId":"char-1","name":"Group","chats":[cold_chat("unused")],"type":"group"
            }})),
        ]));
        let source = cold_block_source(directory.path(), br#"{"chaId":"char-1","name":"Stub","chats":[],"coldstorage":"character"}"#, true);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        assert_eq!(result.character_count, 0);
        assert_eq!(job.status().incomplete_restore_preview.unwrap().omissions.excluded_characters, 1);
        assert!(result.warning_codes.contains(&"excluded-backup-characters:1".to_owned()));
        assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 0);
        assert!(sink.unavailable_cold.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn legacy_import_counts_each_excluded_character_once_before_and_after_cold_expansion() {
        for historical_json in [false, true] {
        let (directory, mut sink) = fixture();
        sink.prepared_characters.insert("cold".to_owned(), json!({"chaId":"cold","type":"group","chats":[]}));
        let database = json!({"characters":[
            {"chaId":"kept","type":"character","name":"Kept","chats":[]},
            {"chaId":"§temp","type":"group","name":"Both","chats":[]},
            {"type":"group","name":"Without ID","chats":[]},
            {"chaId":"cold","type":"character","name":"Prepared","chats":[]}
        ], "botPresets":[], "characterOrder":["§temp","cold","kept"]});
        let payload = super::legacy_stream_tests::messagepack_bytes(&super::legacy_stream_tests::messagepack_from_json(&database));
        let source = directory.path().join("excluded-legacy.risudat");
        let bytes = if historical_json {
            let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&serde_json::to_vec(&database).unwrap()).unwrap();
            encoder.finish().unwrap()
        } else { legacy_wire(7, &payload) };
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();
        assert_eq!(result.character_count, 1);
        assert_eq!(job.status().incomplete_restore_preview.unwrap().omissions.excluded_characters, 3);
        assert!(result.warning_codes.contains(&"excluded-backup-characters:3".to_owned()));
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert_eq!(restored["characters"][0]["name"], "Kept");
        assert_eq!(restored["characterOrder"], json!(["kept"]));
        }
    }

    #[test]
    fn cold_streaming_resolves_trailing_character_reference_before_staging_stub_chats() {
        for compressed in [false, true] {
            let (directory, mut sink) = fixture();
            let messages = (0..300).map(|index| json!({"role":"user", "data":format!("synthetic-{index}"), "chatId":format!("m-{index}")})).collect::<Vec<_>>();
            sink.cold_payloads = Some(std::collections::HashMap::from([
                write_cold_payload(directory.path(), "character", &json!({"character":{"chaId":"char-1", "name":"Expanded", "chats":[cold_chat("chat")], "coldStoragedChats":["chat"]}})),
                write_cold_payload(directory.path(), "chat", &json!({"message":messages, "scriptstate":{"restored":true}, "bindedPersona":"persona"})),
            ]));
            let source = cold_block_source(directory.path(), br#"{"chaId":"char-1","chats":[{"id":"discarded-stub","message":[{"role":"user","data":"must not stage"}]}],"name":"Stub","coldstorage":"character"}"#, compressed);
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            let character = &restored["characters"][0];
            assert_eq!(character["name"], "Expanded");
            assert!(character.get("coldstorage").is_none());
            assert!(character.get("coldStoragedChats").is_none());
            assert_eq!(character["chats"].as_array().unwrap().len(), 1);
            assert_eq!(character["chats"][0]["id"], "chat-1");
            assert_eq!(character["chats"][0]["message"], json!(messages));
            assert_eq!(character["chats"][0]["scriptstate"], json!({"restored":true}));
            assert_eq!(character["chats"][0]["bindedPersona"], "persona");
            assert_eq!(sink.full_character_calls.load(Ordering::Acquire), 0);
            assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 1);
            assert_eq!(sink.max_message_page.load(Ordering::Acquire), MESSAGE_PAGE_COUNT);
        }
    }

    #[test]
    fn cold_streaming_preserves_array_fields_and_applies_object_field_removal() {
        for bare in [false, true] {
            let (directory, mut sink) = fixture();
            let messages = json!([{"role":"char", "data":"second", "swipes":["first","second"], "swipeId":1}]);
            let payload = if bare { messages.clone() } else { json!({"message":messages}) };
            sink.cold_payloads = Some(std::collections::HashMap::from([write_cold_payload(directory.path(), "chat", &payload)]));
            let mut chat = cold_chat("chat");
            chat["savedToggleValues"] = json!({"toggle_kept":"true"});
            chat["bindedPersona"] = json!("kept-persona");
            chat["scriptstate"] = json!({"old":true});
            chat["localLore"] = json!([]);
            let source = cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","name":"Synthetic","chats":[chat]})).unwrap(), false);
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            let chat = &restored["characters"][0]["chats"][0];
            assert_eq!(chat["savedToggleValues"], json!({"toggle_kept":"true"}));
            assert_eq!(chat["bindedPersona"], "kept-persona");
            assert_eq!(chat.get("scriptstate").is_some(), bare);
            assert_eq!(chat.get("localLore").is_some(), bare);
            let variants = &chat["message"][0]["responseVariants"];
            assert_eq!(variants["candidates"].as_array().unwrap().len(), 2);
            assert_eq!(variants["selectedId"], variants["candidates"][1]["id"]);
        }
    }

    #[test]
    fn cold_streaming_reports_missing_and_wrong_role_payloads_with_late_character_name() {
        let (directory, mut sink) = fixture();
        sink.cold_payloads = Some(std::collections::HashMap::from([
            write_cold_payload(directory.path(), "wrong-chat", &json!({"character":{"chaId":"other"}})),
            write_cold_payload(directory.path(), "wrong-character", &json!({"character":[], "message":[]})),
        ]));
        let source = cold_block_source(directory.path(), &format!(r#"{{"chaId":"char-1","chats":[{},{}],"name":"Late name","coldstorage":"wrong-character"}}"#, cold_chat("missing-chat"), cold_chat("wrong-chat")).into_bytes(), true);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        assert_eq!(*sink.unavailable_cold.lock().unwrap(), vec![
            ("wrong-character".to_owned(), "Late name".to_owned()),
            ("missing-chat".to_owned(), "Late name".to_owned()),
            ("wrong-chat".to_owned(), "Late name".to_owned()),
        ]);
        assert!(!result.warning_codes.is_empty());
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert!(restored["characters"][0].get("coldstorage").is_none());
        assert!(restored["characters"][0]["chats"].as_array().unwrap().iter().all(|chat| chat["message"].as_array().unwrap().is_empty()));
    }

    #[test]
    fn cold_streaming_rejects_payload_decoded_overflow_before_activation() {
        for character_payload in [false, true] {
            let (directory, mut sink) = fixture();
            let payload = if character_payload {
                json!({"character":{"chaId":"char-1", "name":"x".repeat(8192), "chats":[]}})
            } else { json!([{"role":"user", "data":"x".repeat(8192)}]) };
            sink.cold_payloads = Some(std::collections::HashMap::from([write_cold_payload(directory.path(), "large", &payload)]));
            let character = if character_payload { json!({"chaId":"char-1", "chats":[], "coldstorage":"large"}) }
                else { json!({"chaId":"char-1", "chats":[cold_chat("large")]}) };
            let source = cold_block_source(directory.path(), &serde_json::to_vec(&character).unwrap(), true);
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let error = restore_block_risu_save_path_with_limits(&source, 1, &job, &sink, RestoreLimits { max_decoded_block_bytes:4096, ..Default::default() }).unwrap_err();
            assert!(error.message.contains("decoded block limit exceeded"), "{error:?}");
            assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
            assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        }
    }

    #[test]
    fn cold_streaming_bounds_one_message_before_materializing_the_rest_of_the_payload() {
        let (directory, mut sink) = fixture();
        let path = directory.path().join("oversized.json");
        let mut file = File::create(&path).unwrap();
        file.write_all(b"[{\"role\":\"user\",\"data\":\"").unwrap();
        for _ in 0..3 { file.write_all(&[b'x'; 64 * 1024]).unwrap(); }
        file.write_all(b"\"}]").unwrap();
        drop(file);
        sink.cold_payloads = Some(std::collections::HashMap::from([("chat".to_owned(), path)]));
        let source = cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","chats":[cold_chat("chat")]})).unwrap(), false);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let error = restore_block_risu_save_path_with_limits(&source, 1, &job, &sink, RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap_err();
        assert!(error.message.contains("restore record limit exceeded"), "{error:?}");
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        assert_eq!(sink.max_message_page.load(Ordering::Acquire), 0);
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cold_streaming_cancellation_during_page_replay_aborts_and_cleans_owned_spools() {
        let (directory, mut sink) = fixture();
        let messages = (0..300).map(|index| json!({"role":"user","data":format!("synthetic-{index}")})).collect::<Vec<_>>();
        sink.cold_payloads = Some(std::collections::HashMap::from([write_cold_payload(directory.path(), "chat", &json!(messages))]));
        let source = cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","name":"Synthetic","chats":[cold_chat("chat")]})).unwrap(), true);
        let registry = Arc::new(JobRegistry::default());
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        sink.cancel_after_page = Some((registry, job.id()));
        let spool_directory = directory.path().join("owned-spools");
        fs::create_dir(&spool_directory).unwrap();
        let error = restore_risu_save_reader_controlled(
            File::open(&source).unwrap(), fs::metadata(&source).unwrap().len(), 1, &job, &sink,
            RestoreLimits::default(), true,
            RestoreProgressScale { spool_directory:Some(spool_directory.clone()), ..Default::default() },
        ).unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert_eq!(sink.max_message_page.load(Ordering::Acquire), MESSAGE_PAGE_COUNT);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        assert_eq!(fs::read_dir(spool_directory).unwrap().count(), 0);
    }

    #[test]
    fn cold_streaming_sequential_spool_preserves_small_records_at_exact_page_boundaries() {
        for count in [0, 1, 127, 128, 129, 256, 10_001] {
            let (directory, mut sink) = fixture();
            let messages = (0..count).map(|index| json!({"role":"user","data":format!("{index}"),"chatId":format!("m-{index}")})).collect::<Vec<_>>();
            sink.cold_payloads = Some(std::collections::HashMap::from([write_cold_payload(directory.path(), "chat", &json!(messages))]));
            let source = cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","name":"Synthetic","chats":[cold_chat("chat")]})).unwrap(), true);
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            assert_eq!(restored["characters"][0]["chats"][0]["message"], json!(messages));
            let expected = messages.chunks(MESSAGE_PAGE_COUNT).map(|page| page.len()).collect::<Vec<_>>();
            assert_eq!(*sink.message_pages.lock().unwrap(), expected);
        }
    }

    #[test]
    fn streaming_character_fields_are_bounded_individually() {
        let (directory, sink) = fixture();
        let character = json!({"chaId":"char-1", "name":"Synthetic", "chats":[],
            "description":"a".repeat(40 * 1024), "creatorNotes":"b".repeat(40 * 1024)});
        let source = cold_block_source(directory.path(), &serde_json::to_vec(&character).unwrap(), true);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        restore_block_risu_save_path_with_limits(&source, 1, &job, &sink,
            RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap();
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert_eq!(restored["characters"][0]["description"], character["description"]);
        assert_eq!(restored["characters"][0]["creatorNotes"], character["creatorNotes"]);
        let padded = format!(r#"{{"chaId":"char-1","name":"Synthetic","chats":[],"description":{}"x"}}"#, " ".repeat(64 * 1024));
        let source = cold_block_source(directory.path(), padded.as_bytes(), true);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let error = restore_block_risu_save_path_with_limits(&source, 2, &job, &sink,
            RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap_err();
        assert!(error.message.contains("restore record limit exceeded"));
        assert_eq!(sink.store.lock().unwrap().materialize(Some(2)).unwrap(), restored);
    }

    #[test]
    fn cold_streaming_sequential_spool_respects_message_page_byte_limit() {
        let (directory, mut sink) = fixture();
        let messages = json!([
            {"role":"user", "data":"a".repeat(MESSAGE_PAGE_BYTES as usize / 2)},
            {"role":"char", "data":"b".repeat(MESSAGE_PAGE_BYTES as usize / 2)},
            {"role":"user", "data":"c".repeat(MESSAGE_PAGE_BYTES as usize + 1024)},
        ]);
        sink.cold_payloads = Some(std::collections::HashMap::from([write_cold_payload(directory.path(), "chat", &messages)]));
        let source = cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","name":"Synthetic","chats":[cold_chat("chat")]})).unwrap(), false);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert_eq!(restored["characters"][0]["chats"][0]["message"], messages);
        assert_eq!(*sink.message_pages.lock().unwrap(), [1, 1, 1]);
    }

    #[test]
    fn cold_streaming_sequential_spool_rejects_truncated_and_oversized_lengths_as_store_errors() {
        for bytes in [vec![4, 0], vec![4, 0, 0, 0, b'{'], (1025u32).to_le_bytes().to_vec()] {
            let (directory, sink) = fixture();
            let mut spool = MessageSpool::new(directory.path(), 1024).unwrap();
            spool.count = 1;
            spool.file.as_mut().unwrap().write_all(&bytes).unwrap();
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let staging_error = RefCell::new(None);
            assert!(spool.replay("unused-staging", "char-1", "chat-1", job.as_ref(), &sink, &staging_error).is_err());
            assert!(staging_error.borrow().is_some());
            assert_eq!(sink.max_message_page.load(Ordering::Acquire), 0);
            assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        }
    }

    fn write_container_entry(output: &mut File, name: &str, path: &Path) {
        output.write_all(&(name.len() as u32).to_le_bytes()).unwrap();
        output.write_all(name.as_bytes()).unwrap();
        output.write_all(&u32::try_from(fs::metadata(path).unwrap().len()).unwrap().to_le_bytes()).unwrap();
        io::copy(&mut File::open(path).unwrap(), output).unwrap();
    }

    #[test]
    #[ignore = "explicit large compatible backup acceptance run"]
    fn compatible_backup_roundtrips_large_fields_and_message() {
        for target in [super::super::legacy_backup::CompatibilityTarget::RisuAi, super::super::legacy_backup::CompatibilityTarget::PocketRisu] {
            let source = tempfile::tempdir().unwrap();
            let mut store = PersistentStore::open(source.path()).unwrap();
            let staging = store.replace_begin().unwrap().staging_id;
            store.replace_put_upstream_root(&staging, &json!({"username":"Synthetic","plugins":[],"modules":[]})).unwrap();
            let first = "a".repeat(33 * 1024 * 1024);
            let second = "b".repeat(33 * 1024 * 1024);
            let message = "c".repeat(9 * 1024 * 1024);
            store.replace_add_characters(&staging, &[
                json!({"chaId":"large","type":"character","name":"Synthetic","desc":first,"firstMessage":second,
                    "chats":[{"id":"large-chat","name":"Synthetic chat","message":[{"role":"char","data":message,"chatId":"large-message"}]}]}),
            ]).unwrap();
            let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
            let owned = source.path().join("owned");
            let handoff = source.path().join("handoff");
            fs::create_dir(&owned).unwrap();
            fs::create_dir(&handoff).unwrap();
            let output = source.path().join("export.bin");
            let export_job = JobRegistry::default().create(JobKind::ExportCompatibleLocalBackup).unwrap();
            super::super::legacy_backup::export_compatible_local_backup(target, Some(&output), revision,
                &owned, &handoff, store, &export_job).unwrap();
            let (directory, mut sink) = fixture();
            let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
            restore_container_payloads(&output, directory.path(), &mut sink, &job).unwrap();
            let restored = sink.store.lock().unwrap().materialize(None).unwrap();
            let character = &restored["characters"][0];
            assert!(character["desc"].as_str().is_some_and(|value| value == first));
            assert!(character["firstMessage"].as_str().is_some_and(|value| value == second));
            assert!(character["chats"][0]["message"][0]["data"].as_str().is_some_and(|value| value == message));
        }
    }

    fn restore_cold_container(source: &Path, directory: &Path, sink: &mut StoreSink, job: &JobControl) -> JobResultSummary {
        try_restore_cold_container(source, directory, sink, job).unwrap()
    }

    fn try_restore_cold_container(source: &Path, directory: &Path, sink: &mut StoreSink, job: &JobControl) -> Result<JobResultSummary, crate::local_backup::LocalBackupError> {
        restore_container_payloads(source, directory, sink, job).map(|(result, _)| result)
    }

    /// Restores a legacy backup container and reports its prepared asset aliases and invalid inlays.
    fn restore_container_payloads(source: &Path, directory: &Path, sink: &mut StoreSink, job: &JobControl) -> Result<(JobResultSummary, Value), crate::local_backup::LocalBackupError> {
        restore_container_payloads_with_limits(source, directory, sink, job, RestoreLimits::default())
    }

    fn restore_container_payloads_with_limits(source: &Path, directory: &Path, sink: &mut StoreSink, job: &JobControl, limits: RestoreLimits) -> Result<(JobResultSummary, Value), crate::local_backup::LocalBackupError> {
        restore_container_payloads_with_key(source, directory, sink, job, limits, None)
    }

    fn restore_container_payloads_with_key(source: &Path, directory: &Path, sink: &mut StoreSink, job: &JobControl, limits: RestoreLimits, key: Option<&str>) -> Result<(JobResultSummary, Value), crate::local_backup::LocalBackupError> {
        use crate::local_backup::{LocalBackupError, NeverCancelled, PayloadTarget, StagedLocalBackupEntry, StrictLocalBackupDatabaseRestore};
        struct Restore<'a> { directory: &'a Path, sink: &'a mut StoreSink, job: &'a JobControl, result: Option<JobResultSummary>, payloads: Value, limits: RestoreLimits, key: Option<&'a str> }
        impl StrictLocalBackupDatabaseRestore for Restore<'_> {
            fn restore_database(&mut self, database: &StagedLocalBackupEntry, entries: &[StagedLocalBackupEntry]) -> Result<(), LocalBackupError> {
                use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};
                use crate::asset_repository::PayloadCas;
                let plaintext = super::super::legacy_backup::account_encryption::prepare_with_key(entries, database, self.job, |_| {
                    self.key.map(str::to_owned).ok_or_else(|| NativeJobError::new("fixture-key-required", "Synthetic account key required"))
                }).map_err(|error| LocalBackupError::database_restore(error.code))?;
                let cas = PayloadCas::new(self.directory).unwrap();
                let mut durable = DurableCasJob::begin(self.directory, &self.job.id(), CasJobKind::LocalBackupRestore, crate::asset_repository::job_pins::CasJobOwner::for_test(), 0).unwrap();
                let payloads = super::super::legacy_backup::prepare_legacy_restore_payloads_with_limits(entries, &cas, &mut durable, &NeverCancelled, &super::super::legacy_backup::NoopPrepareObserver, self.limits)?;
                let mut cold_keys: Vec<&String> = payloads.cold_payloads.keys().collect();
                cold_keys.sort();
                self.payloads = json!({
                    "aliases": payloads.asset_aliases,
                    "invalidInlays": payloads.invalid_inlays,
                    "coldPayloads": cold_keys,
                });
                self.sink.cold_payloads = Some(payloads.cold_payloads);
                self.sink.unavailable_cold_payloads = payloads.unavailable_cold_payloads;
                durable.seal(&mut self.sink.store.lock().unwrap(), 0).unwrap();
                let path = plaintext.as_ref().map(|file| file.path()).unwrap_or_else(|| database.staged_path.as_deref().unwrap());
                self.result = Some(restore_block_risu_save_path_with_limits(path, 1, self.job, self.sink, self.limits)
                    .map_err(|error| LocalBackupError::database_restore(error.message))?);
                durable.release(CasReleaseOutcome::Committed).unwrap();
                Ok(())
            }
        }
        // Entries are staged in the job's private directory, the only place asset payloads may be staged from.
        let staging = directory.join("native-file-jobs").join("jobs").join(job.id());
        fs::create_dir_all(&staging).unwrap();
        let mut restore = Restore { directory, sink, job, result: None, payloads: Value::Null, limits, key };
        crate::local_backup::parse_legacy_local_backup_v1(&mut File::open(source).unwrap(), &staging, PayloadTarget::JobStaging, &mut restore, &NeverCancelled)?;
        Ok((restore.result.unwrap(), restore.payloads))
    }

    // tools/web-risu-export `npm run test:native` exports a synthetic archive, runs this test and checks the report.
    #[test]
    #[ignore = "run by tools/web-risu-export npm run test:native"]
    fn web_export_archive_restores_through_the_legacy_backup_path() {
        let archive = std::env::var_os("RISUNEST_WEB_EXPORT_ARCHIVE").expect("RISUNEST_WEB_EXPORT_ARCHIVE names the exported archive");
        let report = std::env::var_os("RISUNEST_WEB_EXPORT_REPORT").expect("RISUNEST_WEB_EXPORT_REPORT names the report file");
        let (directory, mut sink) = fixture();
        let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
        let (result, payloads) = restore_container_payloads(Path::new(&archive), directory.path(), &mut sink, &job).unwrap();
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        let unavailable: Vec<String> = sink.unavailable_cold.lock().unwrap().iter().map(|(key, _)| key.clone()).collect();
        fs::write(report, serde_json::to_vec(&json!({
            "characterCount": result.character_count,
            "root": restored,
            "payloads": payloads,
            "unavailableCold": unavailable,
        })).unwrap()).unwrap();
    }

    #[test]
    fn cold_streaming_expands_character_stub_in_embedded_block_backup() {
        let (directory, mut sink) = fixture();
        let (_, payload) = write_cold_payload(directory.path(), "archived", &json!({"character":{"chaId":"char-1", "name":"Expanded in container", "chats":[{"id":"real-chat","name":"Chat","message":[{"role":"user","data":"preserved"}]}]}}));
        let database = cold_block_source(directory.path(), br#"{"chaId":"char-1","chats":[{"id":"stub","message":[]}],"coldstorage":"archived"}"#, true);
        let archive = directory.path().join("backup.bin");
        let mut output = File::create(&archive).unwrap();
        write_container_entry(&mut output, "database.risudat", &database);
        write_container_entry(&mut output, "coldstorage/archived.json", &payload);
        drop(output);
        let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
        restore_cold_container(&archive, directory.path(), &mut sink, &job);
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert_eq!(restored["characters"][0]["name"], "Expanded in container");
        assert_eq!(restored["characters"][0]["chats"].as_array().unwrap().len(), 1);
        assert_eq!(restored["characters"][0]["chats"][0]["id"], "real-chat");
        assert_eq!(restored["characters"][0]["chats"][0]["message"][0]["data"], "preserved");
        assert_eq!(sink.full_character_calls.load(Ordering::Acquire), 0);
        assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 1);
    }

    /// An upstream cold storage id, in the form upstream names its items.
    fn cold_id(index: u16) -> String {
        format!("00000000-0000-4000-8000-{index:012}")
    }

    /// The archive entry upstream writes for a cold storage item.
    fn cold_entry(index: u16, body: &[u8]) -> (String, Vec<u8>) {
        (format!("coldstorage_{}.json", cold_id(index)), body.to_vec())
    }

    fn cold_plugin_index(index: u16, entries: Value) -> (String, Vec<u8>) {
        let body = json!({"type":"risu-plugin-storage-index","version":1,"entries":entries,"trash":[{"id":cold_id(999),"at":0}]});
        cold_entry(index, &serde_json::to_vec(&body).unwrap())
    }

    struct ColdPluginRestore {
        _directory: TempDir,
        sink: StoreSink,
        outcome: Result<JobResultSummary, crate::local_backup::LocalBackupError>,
    }

    impl ColdPluginRestore {
        /// A restored value no plugin owns yet, which the assignment flow offers.
        fn unowned(&self, key: &str) -> Option<Value> {
            self.sink.store.lock().unwrap()
                .read_plugin_storage(crate::persistent_store::plugin_owner::UNOWNED_OWNER, key, None)
                .unwrap().map(|row| row.value)
        }

        fn restored(&self) -> Value {
            self.sink.store.lock().unwrap().materialize(None).unwrap()
        }
    }

    /// Restores an upstream backup archive through the legacy backup path. Its
    /// database carries `storage` as plugin storage, in block or MessagePack form,
    /// beside the given archive entries.
    fn restore_cold_plugin_archive(storage: &Value, character: Option<Value>, legacy_database: bool, entries: &[(String, Vec<u8>)], limits: RestoreLimits, cut_bytes: u64) -> ColdPluginRestore {
        let (directory, mut sink) = fixture();
        let database = directory.path().join("database.risudat");
        if legacy_database {
            let character = character.unwrap_or_else(|| json!({"chaId":"char-1","name":"Imported","chats":[]}));
            let value = json!({"username":"Imported","characters":[character],"botPresets":[],"pluginCustomStorage":storage});
            fs::write(&database, legacy_wire(7, &super::legacy_stream_tests::messagepack_bytes(&super::legacy_stream_tests::messagepack_from_json(&value)))).unwrap();
        } else {
            let mut blocks = valid_blocks();
            blocks[5] = block(11, false, "pluginStorage", storage);
            if let Some(character) = character { blocks[6] = block(2, false, "char-1", &character); }
            fs::write(&database, save_bytes(blocks)).unwrap();
        }
        let archive = directory.path().join("backup.bin");
        let mut output = File::create(&archive).unwrap();
        write_container_entry(&mut output, "database.risudat", &database);
        for (index, (name, body)) in entries.iter().enumerate() {
            let path = directory.path().join(format!("entry-{index}.json"));
            fs::write(&path, body).unwrap();
            write_container_entry(&mut output, name, &path);
        }
        let length = output.metadata().unwrap().len();
        output.set_len(length - cut_bytes).unwrap();
        drop(output);
        let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
        let outcome = restore_container_payloads_with_limits(&archive, directory.path(), &mut sink, &job, limits).map(|(result, _)| result);
        if let Ok(result) = &outcome {
            let previewed = job.status().incomplete_restore_preview.map(|preview| preview.warning_codes()).unwrap_or_default();
            assert_eq!(previewed, result.warning_codes);
        }
        ColdPluginRestore { _directory: directory, sink, outcome }
    }

    #[test]
    fn cold_plugin_values_import_unowned_from_archive_payloads() {
        // A large value that reads like a character payload is still any JSON to a plugin.
        let large = json!({"character": "x".repeat(192 * 1024)});
        let limits = RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() };
        let entries = vec![
            cold_plugin_index(1, json!([
                {"k":"object","s":"{\"a\":1}"}, {"k":"null","s":"null"}, {"k":"false","s":"false"}, {"k":"zero","s":"0"}, {"k":"empty","s":"\"\""},
                {"k":"large","l":cold_id(2)}, {"k":"largeNull","l":cold_id(4)},
                {"k":"repeated","s":"1"}, {"k":"repeated","s":"2"}, {"k":"both","l":cold_id(5),"s":"\"text loses\""},
                {"s":"\"no key\""}, {"k":"noValue"},
            ])),
            cold_entry(2, &serde_json::to_vec(&large).unwrap()),
            cold_entry(3, br#"{"legacy":true}"#),
            cold_entry(4, b"null"),
            cold_entry(5, br#""item wins""#),
            cold_entry(6, br#""legacy loses""#),
            cold_plugin_index(7, json!([{"k":"inline","s":"\"index loses\""}, {"k":"inlineNull","s":"1"}, {"k":"indexed","s":"\"index wins\""}])),
        ];
        let index_only = vec![
            ("object", json!({"a":1})), ("null", Value::Null), ("false", json!(false)), ("zero", json!(0)), ("empty", json!("")),
            ("large", large), ("largeNull", Value::Null), ("repeated", json!(2)), ("both", json!("item wins")),
        ];
        let legacy_only = vec![("legacy", json!({"legacy":true})), ("legacyNull", Value::Null)];
        let mixed = json!({"inline":"kept","inlineNull":null,"_coldpluginIndex":cold_id(7),"_coldplugin":{"inline":cold_id(6),"indexed":cold_id(6),"legacy":cold_id(3)}});
        let precedence = vec![("inline", json!("kept")), ("inlineNull", Value::Null), ("indexed", json!("index wins")), ("legacy", json!({"legacy":true}))];
        for (storage, expected, legacy_database) in [
            (json!({"_coldpluginIndex": cold_id(1)}), index_only, false),
            (json!({"_coldplugin": {"legacy": cold_id(3), "legacyNull": cold_id(4), "notAnId": 5}}), legacy_only, false),
            (mixed.clone(), precedence.clone(), false),
            (mixed, precedence, true),
        ] {
            let restore = restore_cold_plugin_archive(&storage, None, legacy_database, &entries, limits, 0);
            assert_eq!(restore.outcome.as_ref().unwrap().warning_codes, Vec::<String>::new(), "{storage}");
            for (key, value) in &expected {
                assert_eq!(restore.unowned(key).as_ref(), Some(value), "{key} in {storage}");
            }
            let restored = restore.restored();
            assert_eq!(restored["pluginCustomStorage"].as_object().unwrap().len(), expected.len(), "{}", restored["pluginCustomStorage"]);
            assert_eq!(restored["characters"][0]["name"], "Imported");
        }

        // A save with no archive around it has nothing to read the markers from.
        for marker in ["_coldplugin", "_coldpluginIndex"] {
            let (directory, sink) = fixture();
            let mut database = json!({"characters":[],"botPresets":[],"pluginCustomStorage":{"inline":true}});
            database["pluginCustomStorage"][marker] = if marker == "_coldplugin" { json!({"plugin":"value"}) } else { json!("index") };
            let wire = legacy_wire(7, &super::legacy_stream_tests::messagepack_bytes(&super::legacy_stream_tests::messagepack_from_json(&database)));
            let source = directory.path().join("legacy.risudat");
            fs::write(&source, wire).unwrap();
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let result = restore_risu_save(&source, 1, &job, &sink).unwrap();
            assert_eq!(result.warning_codes, vec!["skipped-plugin-storage:1"]);
            assert_eq!(sink.store.lock().unwrap().materialize(None).unwrap()["pluginCustomStorage"], json!({"inline":true}));

            let (directory, sink) = fixture();
            let mut blocks = valid_blocks();
            blocks[5] = block(11, false, "pluginStorage", &database["pluginCustomStorage"]);
            let source = directory.path().join("block.risudat");
            fs::write(&source, save_bytes(blocks)).unwrap();
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            assert_eq!(result.warning_codes, vec!["skipped-plugin-storage:1"]);
            assert_eq!(sink.store.lock().unwrap().materialize(None).unwrap()["pluginCustomStorage"], json!({"inline":true}));
        }
    }

    #[test]
    fn unreadable_cold_plugin_values_are_reported_while_the_library_restores() {
        let small = RestoreLimits { max_decoded_block_bytes: 64 * 1024, ..Default::default() };
        let pointer = json!({"inline": 1, "_coldpluginIndex": cold_id(1)});
        let index = |entries: Value| cold_plugin_index(1, json!([entries, {"k":"kept","s":"1"}]));
        // A key the index names never falls back to the older legacy map.
        let shadowed = json!({"inline": 1, "_coldpluginIndex": cold_id(1), "_coldplugin": {"gone": cold_id(3)}});
        for (storage, character, entries, limits, warning, absent) in [
            // The index names an item the archive does not hold, or the archive lacks the index.
            (&shadowed, None, vec![index(json!({"k":"gone","l":cold_id(9)})), cold_entry(3, b"\"legacy\"")], RestoreLimits::default(), "missing-backup-data:1", "gone"),
            (&pointer, None, vec![], RestoreLimits::default(), "missing-backup-data:1", "kept"),
            (&pointer, None, vec![cold_entry(1, br#"{"type":"risu-plugin-storage-index","version":1,"entries":{},"trash":[]}"#)], RestoreLimits::default(), "damaged-plugin-storage:1", "kept"),
            (&pointer, None, vec![cold_entry(1, br#"{"type":"risu-plugin-storage-index","version":2,"entries":[],"trash":[]}"#)], RestoreLimits::default(), "skipped-plugin-storage:1", "kept"),
            (&pointer, None, vec![index(json!({"k":"bad","s":"{"}))], RestoreLimits::default(), "damaged-plugin-storage:1", "bad"),
            (&pointer, None, vec![index(json!({"k":"_coldplugin","s":"1"}))], RestoreLimits::default(), "skipped-plugin-storage:1", "_coldplugin"),
            (&json!({"inline": 1, "_coldplugin": {"_coldpluginIndex": cold_id(3), "kept": cold_id(3)}}), None, vec![cold_entry(3, b"1")], RestoreLimits::default(), "skipped-plugin-storage:1", "_coldpluginIndex"),
            // Entries the archive holds but could not stage.
            (&pointer, None, vec![index(json!({"k":"broken","l":cold_id(2)})), cold_entry(2, b"{broken")], RestoreLimits::default(), "damaged-plugin-storage:1", "broken"),
            (&pointer, None, vec![index(json!({"k":"trailing","l":cold_id(2)})), cold_entry(2, b"\"value\" trailing")], RestoreLimits::default(), "damaged-plugin-storage:1", "trailing"),
            (&pointer, None, vec![index(json!({"k":"large","l":cold_id(2)})), cold_entry(2, &serde_json::to_vec(&"x".repeat(96 * 1024)).unwrap())], small, "oversized-plugin-storage:1", "large"),
            (&json!({"inline": 1, "kept": 1}), Some(json!({"chaId":"char-1","name":"Imported","chats":[cold_chat(&cold_id(2))]})), vec![cold_entry(2, b"{broken")], RestoreLimits::default(), "unavailable-cold-payloads:1", "broken"),
        ] {
            let restore = restore_cold_plugin_archive(storage, character, false, &entries, limits, 0);
            assert_eq!(restore.outcome.as_ref().unwrap().warning_codes, vec![warning], "{entries:?}");
            assert_eq!(restore.unowned(absent), None, "{absent}");
            assert_eq!(restore.unowned("inline"), Some(json!(1)));
            if absent != "kept" { assert_eq!(restore.unowned("kept"), Some(json!(1)), "{warning}"); }
            let restored = restore.restored();
            let storage = restored["pluginCustomStorage"].as_object().unwrap();
            assert!(!storage.contains_key("_coldpluginIndex") && !storage.contains_key("_coldplugin"), "{storage:?}");
            assert_eq!(restored["characters"][0]["name"], "Imported");
        }
    }

    #[test]
    fn damaged_archives_with_cold_plugin_values_fail_without_changing_the_library() {
        let storage = json!({"_coldpluginIndex": cold_id(1)});
        let index = cold_plugin_index(1, json!([{"k":"value","s":"1"}]));
        let duplicate = (format!("coldstorage/{}.json", cold_id(1)), b"{broken".to_vec());
        for (entries, cut_bytes, expected) in [
            (vec![index.clone()], 4, "truncated"),
            (vec![duplicate, index], 0, "duplicate cold payload key"),
        ] {
            let restore = restore_cold_plugin_archive(&storage, None, false, &entries, RestoreLimits::default(), cut_bytes);
            let error = restore.outcome.as_ref().unwrap_err();
            assert!(error.message.contains(expected), "{error:?}");
            let store = restore.sink.store.lock().unwrap();
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        }
    }

    #[test]
    fn cold_streaming_container_rejects_oversized_referenced_records_before_activation() {
        for (nested, prefix, suffix) in [
            (false, br#"[{"role":"user","data":""#.as_slice(), b"\"}]".as_slice()),
            (true, br#"{"character":{"chaId":"char-1","chats":[{"message":[{"data":""#.as_slice(), b"\"}]}]}}".as_slice()),
            (false, br#"{"savedToggleValues":{"toggle_large":""#.as_slice(), b"\"},\"message\":[]}".as_slice()),
            (true, br#"{"character":""#.as_slice(), b"\",\"message\":[]}".as_slice()),
            (false, br#"{""#.as_slice(), b"\":0,\"message\":[]}".as_slice()),
        ] {
            let (directory, mut sink) = fixture();
            let payload = directory.path().join("oversized.json");
            let mut output = File::create(&payload).unwrap();
            output.write_all(prefix).unwrap();
            for _ in 0..3 { output.write_all(&[b'x';64*1024]).unwrap(); }
            output.write_all(suffix).unwrap();
            drop(output);
            let character = if nested { json!({"chaId":"char-1","chats":[],"coldstorage":"large"}) }
                else { json!({"chaId":"char-1","chats":[cold_chat("large")]}) };
            let database = cold_block_source(directory.path(), &serde_json::to_vec(&character).unwrap(), true);
            let archive = directory.path().join("oversized.bin");
            let mut output = File::create(&archive).unwrap();
            write_container_entry(&mut output, "database.risudat", &database);
            write_container_entry(&mut output, "coldstorage/large.json", &payload);
            drop(output);
            let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
            let error = restore_container_payloads_with_limits(&archive, directory.path(), &mut sink, &job, RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap_err();
            assert!(error.message.contains("restore record limit exceeded"), "{error:?}");
            assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1, "failed staging must be discarded before activation");
            assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
        }
    }

    #[test]
    fn cold_streaming_container_keeps_last_duplicate_character_chats_and_messages() {
        for (envelope, empty) in [(false, false), (true, false), (false, true), (true, true)] {
            let (directory, mut sink) = fixture();
            let payload = directory.path().join("duplicates.json");
            let header = "\u{ef01}COLDSTORAGE\u{ef01}payload";
            let character = format!(r#"{{"chaId":"char-1","name":"Final","chats":[{{"id":"discarded","message":[]}}],"chats":[{{"id":"kept","name":"Chat","message":[{{"role":"user","data":"discarded"}}],"message":[{{"role":"user","data":"{header}"}}]}}]}}"#);
            let database_body;
            let final_messages = if empty { "[]" } else { r#"[{"role":"user","data":"kept"}]"# };
            if envelope {
                let final_character = format!(r#"{{"chaId":"char-1","name":"Final","chats":[{{"id":"discarded","message":[]}}],"chats":[{{"id":"kept","name":"Chat","message":[{{"role":"user","data":"discarded","time":9876}}],"message":{final_messages}}}]}}"#);
                fs::write(&payload, format!(r#"{{"character":{{"chaId":"char-1","name":"Discarded","chats":[{{"id":"old","message":[]}}]}},"character":{final_character}}}"#)).unwrap();
                database_body = br#"{"chaId":"char-1","chats":[],"coldstorage":"payload"}"#.to_vec();
            } else {
                fs::write(&payload, format!(r#"{{"message":[{{"role":"user","data":"discarded","time":9876}}],"message":{final_messages}}}"#)).unwrap();
                database_body = character.into_bytes();
            }
            let database = cold_block_source(directory.path(), &database_body, true);
            let archive = directory.path().join("duplicates.bin");
            let mut output = File::create(&archive).unwrap();
            write_container_entry(&mut output, "database.risudat", &database);
            write_container_entry(&mut output, "coldstorage/payload.json", &payload);
            drop(output);
            let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
            restore_cold_container(&archive, directory.path(), &mut sink, &job);
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            let character = &restored["characters"][0];
            assert_eq!(character["name"], "Final");
            assert_eq!(character["chats"].as_array().unwrap().len(), 1);
            assert_eq!(character["chats"][0]["id"], "kept");
            assert_eq!(character["chats"][0]["message"], serde_json::from_str::<Value>(final_messages).unwrap());
            assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 1);
            assert_eq!(*sink.message_pages.lock().unwrap(), if empty { vec![] } else { vec![1] });
        }
        for final_messages in ["[]", "null"] {
            let (directory, mut sink) = fixture();
            let header = "\u{ef01}COLDSTORAGE\u{ef01}missing-discarded-payload";
            let character = format!(r#"{{"chaId":"char-1","name":"Synthetic","chats":[{{"id":"kept","name":"Chat","message":[{{"role":"user","data":"{header}","time":9876}}],"message":{final_messages}}}]}}"#);
            let database = cold_block_source(directory.path(), character.as_bytes(), true);
            let archive = directory.path().join("reset.bin");
            let mut output = File::create(&archive).unwrap();
            write_container_entry(&mut output, "database.risudat", &database);
            drop(output);
            let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
            let result = restore_cold_container(&archive, directory.path(), &mut sink, &job);
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            assert_eq!(restored["characters"][0]["chats"][0]["message"], json!([]));
            assert!(sink.message_pages.lock().unwrap().is_empty());
            assert!(sink.unavailable_cold.lock().unwrap().is_empty());
            assert!(result.warning_codes.is_empty());
        }
    }

    #[test]
    fn cold_streaming_character_metadata_limits_wrong_role_scalar_before_allocation() {
        let (directory, mut sink) = fixture();
        let payload = directory.path().join("scalar.json");
        let mut output = File::create(&payload).unwrap();
        output.write_all(br#"{"character":""#).unwrap();
        for _ in 0..3 { output.write_all(&[b'x';64*1024]).unwrap(); }
        output.write_all(b"\"}").unwrap();
        drop(output);
        sink.cold_payloads = Some(std::collections::HashMap::from([("scalar".to_owned(), payload)]));
        let source = cold_block_source(directory.path(), br#"{"chaId":"char-1","chats":[],"coldstorage":"scalar"}"#, true);
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let error = restore_block_risu_save_path_with_limits(&source, 1, &job, &sink, RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap_err();
        assert!(error.message.contains("restore record limit exceeded"), "{error:?}");
        assert_eq!(sink.incremental_character_calls.load(Ordering::Acquire), 0);
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
    }

    #[test]
    #[ignore = "explicit 200 MiB restore memory measurement"]
    fn cold_streaming_200_mib_direct_and_container_memory_measurement() {
        const COUNT: usize = 51_200;
        for container in [false, true] {
            let (directory, mut sink) = fixture();
            let payload = directory.path().join("large-chat.json");
            let mut output = std::io::BufWriter::new(File::create(&payload).unwrap());
            output.write_all(b"[").unwrap();
            let text = "x".repeat(4096);
            for index in 0..COUNT {
                if index != 0 { output.write_all(b",").unwrap(); }
                serde_json::to_writer(&mut output, &json!({"role":"user","data":text,"chatId":format!("message-{index}")})).unwrap();
            }
            output.write_all(b"]").unwrap();
            output.flush().unwrap();
            drop(output);
            let database = if container {
                cold_block_source(directory.path(), &serde_json::to_vec(&json!({"chaId":"char-1","chats":[cold_chat("large")]})).unwrap(), true)
            } else {
                let character = directory.path().join("large-character.json");
                let mut output = File::create(&character).unwrap();
                output.write_all(br#"{"chaId":"char-1","chats":[{"id":"chat-1","message":"#).unwrap();
                io::copy(&mut File::open(&payload).unwrap(), &mut output).unwrap();
                output.write_all(b"}]}").unwrap();
                drop(output);
                let source = directory.path().join("direct.risudat");
                let mut output = File::create(&source).unwrap();
                output.write_all(RISU_SAVE_HEADER).unwrap();
                let blocks = valid_blocks();
                for block in &blocks[..6] { output.write_all(block).unwrap(); }
                output.write_all(&[2,0,6]).unwrap();
                output.write_all(b"char-1").unwrap();
                output.write_all(&u32::try_from(fs::metadata(&character).unwrap().len()).unwrap().to_le_bytes()).unwrap();
                io::copy(&mut File::open(character).unwrap(), &mut output).unwrap();
                output.write_all(&blocks[7]).unwrap();
                source
            };
            let archive = directory.path().join("large.bin");
            if container {
                let mut output = File::create(&archive).unwrap();
                write_container_entry(&mut output, "database.risudat", &database);
                write_container_entry(&mut output, "coldstorage/large.json", &payload);
            }
            let job = JobRegistry::default().create(if container { JobKind::RestoreLegacyLocalBackup } else { JobKind::RestoreBlockRisuSave }).unwrap();
            let measurement = crate::test_memory::measure_working_set(|| {
                if container { restore_cold_container(&archive, directory.path(), &mut sink, &job) }
                else { restore_block_risu_save_path(&database, 1, &job, &sink).unwrap() }
            });
            assert_eq!(measurement.value.revision, 2);
            assert_eq!(sink.full_character_calls.load(Ordering::Acquire), 0);
            assert!(sink.max_message_page.load(Ordering::Acquire) <= MESSAGE_PAGE_COUNT);
            let query = serde_json::from_value(json!({"characterId":"char-1","conversationId":"chat-1","startIndex":COUNT-1,"limit":1})).unwrap();
            let window = sink.store.lock().unwrap().read_conversation_window(&query, None).unwrap().unwrap().value;
            assert_eq!(window.total_messages, COUNT as i64);
            assert_eq!(window.messages.len(), 1);
            assert_eq!(window.messages[0]["data"], text);
            assert_eq!(window.messages[0]["chatId"], format!("message-{}", COUNT-1));
            println!("COLD_STREAMING_MEMORY {{\"container\":{container},\"messageCount\":{COUNT},\"baselineWorkingSetBytes\":{:?},\"peakWorkingSetBytes\":{:?},\"retainedWorkingSetBytes\":{:?}}}", measurement.baseline_working_set_bytes, measurement.peak_working_set_bytes, measurement.retained_working_set_bytes);
            if let (Some(baseline), Some(peak)) = (measurement.baseline_working_set_bytes, measurement.peak_working_set_bytes) {
                assert!(peak.saturating_sub(baseline) < 192 * 1024 * 1024, "restore retained a payload-sized working set");
            }
        }
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
    fn block_restore_assigns_missing_and_duplicate_chat_ids() {
        let (directory, sink) = fixture();
        let mut blocks = valid_blocks();
        blocks[6] = block(2, false, "char-1", &json!({
            "chaId":"char-1","name":"Synthetic","chats":[
                {"name":"Chat","message":[]},{"id":null,"name":"Chat","message":[]},{"id":"","name":"Chat","message":[]},
                {"id":"existing","name":"Chat","message":[]},{"id":"existing","name":"Chat","message":[]}
            ]
        }));
        let path = directory.path().join("ids.risudat");
        fs::write(&path, save_bytes(blocks)).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        restore_block_risu_save_path(&path, 1, &job, &sink).unwrap();
        let restored = sink.store.lock().unwrap().materialize(None).unwrap();
        let chats = restored["characters"][0]["chats"].as_array().unwrap();
        let ids: HashSet<_> = chats.iter().map(|chat| chat["id"].as_str().unwrap()).collect();
        assert_eq!(chats.len(), 5);
        assert_eq!(ids.len(), 5);
        assert!(!ids.contains(""));
        assert_eq!(chats[3]["id"], "existing");
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

    #[test]
    fn a_failure_after_the_library_commit_keeps_the_restored_library() {
        for lose_reply in [false, true] {
        let (directory, mut sink) = fixture();
        sink.record_activation = true;
        sink.lose_commit_reply = lose_reply;
        let path = directory.path().join("committed.risudat");
        fs::write(&path, save_bytes(valid_blocks())).unwrap();
        if !lose_reply { sink.store.lock().unwrap().device_store().unwrap().connection().execute_batch(
            "CREATE TRIGGER fail_completion BEFORE UPDATE OF complete ON lww_intents
             BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END;",
        ).unwrap(); }
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let outcome = restore_block_risu_save_path(&path, 1, &job, &sink);
        if lose_reply {
            let result = outcome.unwrap();
            assert_eq!(result.revision, 2);
            assert!(result.warning_codes.contains(&"cleanup-failed".to_owned()));
            assert_eq!(job.status().activation_revision, Some(2));
        } else {
            assert_eq!(outcome.unwrap_err().code, "activation-outcome-unresolved");
        }
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 0);
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.materialize(None).unwrap()["username"], "Imported");
        }
    }

    #[test]
    fn a_binding_switch_after_the_activation_receipt_refuses_the_commit_and_leaves_the_receipt_resolvable() {
        let (directory, mut sink) = fixture();
        sink.record_activation = true;
        sink.switch_binding_before_commit = true;
        let path = directory.path().join("switched.risudat");
        fs::write(&path, save_bytes(valid_blocks())).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        let error = restore_block_risu_save_path(&path, 1, &job, &sink).unwrap_err();

        assert_eq!(error.code, "revision-conflict", "{error:?}");
        assert!(error.message.contains("binding-authority-changed"), "{error:?}");
        assert_eq!(sink.abort_calls.load(Ordering::Acquire), 1);
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        let restarted = JobRegistry::default();
        super::super::restore_receipts::reconcile(&store, &restarted).unwrap();
        let recovered = restarted.status(&job.id()).unwrap();
        assert_eq!(recovered.state, JobState::Cancelled);
        assert_eq!(recovered.activation_revision, None);
    }

    fn msgpackr_parity_fixture() -> Value {
        serde_json::from_str(MSGPACKR_PARITY_FIXTURE).unwrap()
    }

    fn legacy_wire(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0, b'R', b'I', b'S', b'U', b'S', b'A', b'V', b'E', 0, kind];
        bytes.extend_from_slice(payload);
        bytes
    }

    fn stage_legacy_json(
        database: &Value,
        staging_id: &str,
        job: &JobControl,
        sink: &dyn ReplacementSink,
    ) -> Result<ParsedCounts, NativeJobError> {
        let payload = super::legacy_stream_tests::messagepack_bytes(
            &super::legacy_stream_tests::messagepack_from_json(database),
        );
        let wire = legacy_wire(7, &payload);
        let mut reader = TrackedReader::new(wire.as_slice(), wire.len() as u64, job);
        read_risu_save_format(&mut reader)?;
        let parsed =
            parse_and_stage_legacy(&mut reader, staging_id, job, sink, RestoreLimits::default())?;
        reader.require_eof()?;
        Ok(parsed)
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

    fn persistent_projection(value: Value, actual: &Value) -> Value {
        let mut database = value.as_object().unwrap().clone();
        for field in ["botPresets", "modules", "loadouts"] {
            let records = database.get_mut(field).unwrap().as_array_mut().unwrap();
            let actual_records = actual[field].as_array().unwrap();
            assert_eq!(records.len(), actual_records.len());
            let mut ids = std::collections::HashSet::new();
            for (record, actual_record) in records.iter_mut().zip(actual_records) {
                let id = actual_record["id"].as_str().unwrap();
                assert!(ids.insert(id));
                if let Some(existing) = record.get("id") {
                    assert_eq!(existing, &actual_record["id"]);
                } else {
                    assert!(uuid::Uuid::parse_str(id).is_ok());
                    record["id"] = json!(id);
                }
            }
        }
        if !database["botPresets"].as_array().unwrap().is_empty() {
            let selected = database.get("botPresetsId").and_then(Value::as_u64).unwrap_or(0) as usize;
            let selected_id = database["botPresets"][selected]["id"].clone();
            database.insert("botPresetsId".to_owned(), selected_id);
            if let Some(api_type) = database.get("apiType").cloned() {
                database.get_mut("botPresets").unwrap()[selected]["apiType"] = api_type;
            }
        }
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
                "type":"character", "chaId":"char-1",
                "chats":[null, {"id":7, "name":"Chat", "message":[{
                    "role":"char", "data":"second", "chatId":"message-1",
                    "swipes":["first", null, "second"], "swipeId":2
                }]}, "not a chat", {"id":"chat-2", "bindedPersona":null,
                    "savedToggleValues":{"toggle_a":"1", "other":"x"}, "message":[]}]
            }),
        );
        fs::write(&source, save_bytes(blocks)).unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        let restored = sink.store.lock().unwrap().materialize(None).unwrap();
        let chats = restored["characters"][0]["chats"].as_array().unwrap();
        assert_eq!(chats.len(), 2);
        assert_eq!(chats[0]["id"], "7");
        let message = &chats[0]["message"][0];
        assert_eq!(message["data"], "second");
        assert_eq!(
            message["responseVariants"]["candidates"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(message["swipes"], json!(["first", null, "second"]));
        assert_eq!(chats[1]["savedToggleValues"], json!({"toggle_a":"1"}));
        assert!(chats[1].get("bindedPersona").is_none());
        assert_eq!(restored["characters"][0]["name"], "");
        assert_eq!(chats[1]["name"], "");
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
    fn strict_block_restore_reads_an_unset_upstream_plugin_storage_as_empty() {
        // Upstream encodes JSON.stringify(undefined) as a zero-byte body, gzip of nothing when compressed.
        for (compression, body) in [(0, Vec::new()), (1, gzip(b"")), (0, b" \r\n\t".to_vec()), (1, gzip(b"\n"))] {
            let (directory, sink) = fixture();
            let mut blocks = valid_blocks();
            blocks[5] = raw_block(11, compression, "pluginStorage", &body);
            let source = directory.path().join("unset-plugin-storage.risudat");
            fs::write(&source, save_bytes(blocks)).unwrap();
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
            let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
            assert_eq!(restored["pluginCustomStorage"], json!({}));
            assert_eq!(restored["characters"][0]["chaId"], "char-1");
        }
        let mut plugins = valid_blocks();
        plugins[4] = raw_block(9, 0, "plugins", b"");
        assert_failed_restore_preserves_active(&save_bytes(plugins), "invalid JSON in block plugins");
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
        let expected = persistent_projection(parity["expectedProjection"].clone(), &first);
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
        stage_legacy_json(&database, &staging_id, &job, &sink).unwrap();
        sink.commit(&staging_id, 1, None).unwrap();
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
        stage_legacy_json(&database, &staging_id, &job, &sink).unwrap();
        sink.commit(&staging_id, 1, None).unwrap();

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
            ("modules", json!(7)),
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
            let error = match stage_legacy_json(&database, &staging_id, &job, &sink) {
                Ok(_) => panic!("{key} must reject a present non-array value"),
                Err(error) => error,
            };
            assert_eq!(error.code, "invalid-input");
            assert!(error.message.contains(key));
        }
    }

    #[test]
    fn raw_msgpackr_restore_omits_unknown_extensions_with_warning() {
        let payload = legacy_stream_tests::messagepack_bytes(&MessagePackValue::Map(vec![
            ("characters".into(), MessagePackValue::Array(vec![])),
            ("kept".into(), "readable".into()),
            ("unknownExtension".into(), MessagePackValue::Ext(42, vec![1])),
            ("array".into(), MessagePackValue::Array(vec![MessagePackValue::Ext(42, vec![2]), "readable".into()])),
        ]));
        let bytes = legacy_wire(7, &payload);

        let (directory, sink) = fixture();
        let source = directory.path().join("unknown-extension.risudat");
        fs::write(&source, bytes).unwrap();
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let result = restore_risu_save(&source, 1, &job, &sink).unwrap();
        assert_eq!(result.revision, 2);
        assert!(result.warning_codes.contains(&"skipped-backup-values:2".to_owned()));
        let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
        assert!(restored.get("unknownExtension").is_none());
        assert_eq!(restored["kept"], "readable");
        assert_eq!(restored["array"], json!([null, "readable"]));
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
        let expected = persistent_projection(parity["edgeExpectedProjection"].clone(), &first);
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
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap(), &first);
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
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap(), &first);
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
                ..Default::default()
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "payload-too-large");
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
                    ..Default::default()
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "payload-too-large");
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
        let expected = persistent_projection(serde_json::from_str(W0_LEGACY_EXPECTED).unwrap(), &first);
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
        let expected = persistent_projection(parity["expectedProjection"].clone(), &first);
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
    fn released_historical_writers_restore_standalone_and_inside_archives() {
        let fixture: Value = serde_json::from_str(include_str!("legacy_backup/fixtures/historical-writers.json")).unwrap();
        let mut projection = None;
        for format in fixture["formats"].as_array().unwrap() {
            let bytes = base64::engine::general_purpose::STANDARD.decode(format["bytes"].as_str().unwrap()).unwrap();
            for mode in ["standalone", "archive", "encrypted", "{}", "null", "[]", "true", r#""synthetic""#,
                r#"{"type":"none"}"#, r#"{"type":"synthetic"}"#, r#"{"type":42}"#] {
                let archived = mode != "standalone";
                let encrypted = mode == "encrypted";
                let (directory, mut sink) = self::fixture();
                let source = directory.path().join("historical.risudat");
                fs::write(&source, &bytes).unwrap();
                assert_eq!(super::super::backup_source::detect(File::open(&source).unwrap()).unwrap(), super::super::backup_source::BackupSourceFormat::BlockRisuSave);
                let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
                if archived {
                    let archive = directory.path().join("historical.bin");
                    let mut output = File::create(&archive).unwrap();
                    if encrypted {
                        let ciphertext = base64::engine::general_purpose::STANDARD.decode(format["encrypted"].as_str().unwrap()).unwrap();
                        fs::write(&source, ciphertext).unwrap();
                        let metadata = directory.path().join("metadata");
                        fs::write(&metadata, br#"{"type":"account","time":1}"#).unwrap();
                        write_container_entry(&mut output, "encryption.risudat", &metadata);
                    } else if mode != "archive" {
                        let metadata = directory.path().join("metadata");
                        fs::write(&metadata, mode).unwrap();
                        write_container_entry(&mut output, "encryption.risudat", &metadata);
                    }
                    write_container_entry(&mut output, "database.risudat", &source);
                    drop(output);
                    let key = if encrypted { fixture["key"].as_str() } else { None };
                    restore_container_payloads_with_key(&archive, directory.path(), &mut sink, &job, RestoreLimits::default(), key).unwrap();
                } else {
                    restore_risu_save(&source, 1, &job, &sink).unwrap();
                }
                let restored = sink.store.lock().unwrap().materialize(Some(2)).unwrap();
                assert_eq!(restored["username"], fixture["database"]["username"]);
                assert_eq!(restored["characters"], fixture["database"]["characters"]);
                assert_eq!(restored["historicalUnknown"], fixture["database"]["historicalUnknown"]);
                assert_eq!(restored["pluginCustomStorage"], fixture["database"]["pluginCustomStorage"]);
                assert!(restored.get("account").is_none());
                if let Some(expected) = &projection { assert_eq!(&restored, expected, "{format}"); }
                else { projection = Some(restored); }
            }
        }
    }

    #[test]
    fn readable_non_account_metadata_does_not_allow_an_invalid_database() {
        for metadata in ["{}", "null", r#"{"type":"synthetic"}"#] {
            let (directory, mut sink) = fixture();
            let before = sink.store.lock().unwrap().materialize(None).unwrap();
            let source = directory.path().join("invalid.risudat");
            fs::write(&source, b"not a database").unwrap();
            let meta = directory.path().join("metadata");
            fs::write(&meta, metadata).unwrap();
            let archive = directory.path().join("invalid.bin");
            let mut output = File::create(&archive).unwrap();
            write_container_entry(&mut output, "encryption.risudat", &meta);
            write_container_entry(&mut output, "database.risudat", &source);
            drop(output);
            let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
            let failure = restore_container_payloads_with_key(&archive, directory.path(), &mut sink, &job, RestoreLimits::default(), None).unwrap_err();
            assert!(!failure.to_string().contains("backup-encryption-unsupported"));
            assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 1);
            assert_eq!(sink.store.lock().unwrap().materialize(None).unwrap(), before);
        }
    }

    #[test]
    fn strict_legacy_format_selection_rejects_ambiguous_and_near_miss_inputs() {
        use base64::Engine;

        let parity = msgpackr_parity_fixture();
        let payload = base64::engine::general_purpose::STANDARD
            .decode(parity["payloadBase64"].as_str().unwrap())
            .unwrap();

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
            incremental: true,
            prepared_characters: Default::default(),
            fail_character_batches: false,
            record_activation: false,
            lose_commit_reply: false,
            switch_binding_before_commit: false,
            abort_calls: AtomicUsize::new(0),
            preserve_calls: AtomicUsize::new(0),
            full_character_calls: AtomicUsize::new(0),
            incremental_character_calls: AtomicUsize::new(0),
            max_message_page: AtomicUsize::new(0),
            message_pages: Mutex::new(Vec::new()),
            cold_payloads: None,
            unavailable_cold_payloads: Default::default(),
            unavailable_cold: Mutex::new(Vec::new()),
            cancel_after_page: None,
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
    fn strict_restore_rejects_duplicate_blocks() {
        let mut missing = valid_blocks();
        missing.pop();
        let (directory, sink) = fixture();
        let source = directory.path().join("missing.risudat");
        fs::write(&source, save_bytes(missing)).unwrap();
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        assert!(result.warning_codes.contains(&"missing-backup-data:1".to_owned()));
        assert_eq!(sink.store.lock().unwrap().revision().unwrap(), 2);

        let mut duplicate = valid_blocks();
        duplicate.push(block(4, false, "preset", &json!([])));
        assert_failed_restore_preserves_active(&save_bytes(duplicate), "duplicate block");
    }

    #[test]
    fn readable_uninterpreted_blocks_restore_with_warnings() {
        for (kind, name, warning) in [
            (3, "standalone-chat", "skipped-backup-data:1"),
            (6, "remote", "remote-backup-data:1"),
            (8, "custom-root-component", "skipped-backup-data:1"),
            (12, "kv:synthetic", "skipped-backup-data:1"),
            (99, "future-content", "skipped-backup-data:1"),
        ] {
        let mut blocks = valid_blocks();
        // Block type 12 also carries plugin ownership, which must still apply.
        blocks[5] = block(11, false, "pluginStorage", &json!({"owned":"kept","free":1}));
        blocks.insert(6, block(12, false, "pluginStorageMeta", &json!({"owned":{"plugin":"synthetic-plugin","updatedAt":1}})));
        blocks.push(block(
            kind,
            true,
            name,
            &json!({ "key": "customField", "data": { "retained": true } }),
        ));
        let (directory, sink) = fixture();
        let source = directory.path().join("root-component.risudat");
        fs::write(&source, save_bytes(blocks)).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();

        let result = restore_block_risu_save_path(&source, 1, &job, &sink).unwrap();
        assert!(result.warning_codes.contains(&warning.to_owned()), "{:?}", result.warning_codes);
        assert_eq!(job.status().incomplete_restore_preview.unwrap().warning_codes(), vec![warning]);
        let store = sink.store.lock().unwrap();
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.read_plugin_storage("synthetic-plugin", "owned", None).unwrap().map(|row| row.value), Some(json!("kept")));
        assert_eq!(store.read_plugin_storage(crate::persistent_store::plugin_owner::UNOWNED_OWNER, "free", None).unwrap().map(|row| row.value), Some(json!(1)));
        }
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

        let mut unreadable_unknown = valid_blocks();
        unreadable_unknown.push(raw_block(99, 1, "future-content", b"not-gzip"));
        assert_failed_restore_preserves_active(&save_bytes(unreadable_unknown), "invalid gzip");

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
            thread::spawn(move || restore_block_risu_save(opened, tempfile::tempdir().unwrap().path(), 1, &job, sink.as_ref()))
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

        let result = restore_block_risu_save(opened, tempfile::tempdir().unwrap().path(), 1, &job, &sink).unwrap();

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

        let error = restore_block_risu_save(opened, tempfile::tempdir().unwrap().path(), 1, &job, &sink).unwrap_err();

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

        let error = restore_block_risu_save(opened, tempfile::tempdir().unwrap().path(), 1, &job, &sink).unwrap_err();

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

        fn put_legacy_root(&self, staging_id: &str, root: &RootSpool) -> StoreResult<()> {
            self.inner.put_legacy_root(staging_id, root)
        }

        fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
            self.inner.add_characters(staging_id, characters)
        }

        fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
            self.inner.commit(staging_id, expected_revision, binding_authority)
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
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "payload-too-large");
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
                import_destination: None,
                operation_id: None,
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

        fn put_legacy_root(&self, staging_id: &str, root: &RootSpool) -> StoreResult<()> {
            self.inner.put_legacy_root(staging_id, root)
        }

        fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
            self.inner.add_characters(staging_id, characters)
        }

        fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
            self.inner.commit(staging_id, expected_revision, binding_authority)
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
        assert!(relaunched.capability_error.lock().unwrap().is_none());
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
