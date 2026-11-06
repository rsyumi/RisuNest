use super::{
    restore, FailureOwner, ImportCounts, JobControl, JobDetail, JobPhase, JobProgress,
    JobResultSummary, JobStage, NativeJobError, OpenedJobSource, StageUnit,
};
use crate::asset_repository::job_pins::{
    CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
};
use crate::asset_repository::{owner_manifest_codec, PayloadCas};
use crate::asset_repository::import_payloads::{prepare_import_payloads, ImportPayload, ImportPinning};
use crate::local_backup::{
    parse_legacy_local_backup_v1_observed, write_legacy_local_backup_v1, CancellationProbe,
    LegacyBackupWriteEntry, LegacyBackupWriteSource, LocalBackupError, LocalBackupErrorCode,
    LocalBackupParseObserver, PayloadTarget, StagedLocalBackupEntry,
    StrictLocalBackupDatabaseRestore,
};
use crate::persistent_store::export::{self, destination};
use crate::persistent_store::{
    AssetAlias, AssetOwnerHead, AssetOwnerLocator, PersistentStore,
    RevisionResult, StagingResult, StoreResult,
};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::fmt;
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

mod cold_expansion;
pub(super) mod account_encryption;
mod compatible_assets;
mod compatible_export;
mod compatible_projection;
mod pocket_risu;
pub(crate) use compatible_export::export_compatible_local_backup;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CompatibilityTarget {
    #[serde(rename = "risuai")]
    RisuAi,
    #[serde(rename = "pocketrisu")]
    PocketRisu,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompatibilityReportItem {
    pub(crate) code: String,
    pub(crate) items: String,
    pub(crate) bytes: String,
    pub(crate) affected_conversations: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompatibilityReport {
    pub(crate) target: CompatibilityTarget,
    pub(crate) preserved: Vec<CompatibilityReportItem>,
    pub(crate) converted: Vec<CompatibilityReportItem>,
    pub(crate) excluded: Vec<CompatibilityReportItem>,
}
#[cfg(test)]
mod pocket_risu_tests;

const DATABASE_ENTRY: &str = "database.risudat";
const ENCRYPTION_ENTRY: &str = "encryption.risudat";
const MAX_METADATA_BYTES: u32 = 1024 * 1024;
const MAX_OWNER_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const ARCHIVE_FILE: &str = "archive.bin.part";

struct JobCancellation<'a>(&'a JobControl);

impl CancellationProbe for JobCancellation<'_> {
    fn cancellation_flag(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(self.0.cancellation_flag())
    }
    fn is_cancelled(&self) -> bool {
        self.0.is_cancel_requested()
    }
}

const PROGRESS_REPORT_INTERVAL_BYTES: u64 = 1024 * 1024;

/// Observes attachments being prepared in the CAS during a legacy restore.
pub(crate) trait LegacyPrepareObserver {
    fn begin(&self, _total: u64) {}
    fn item_started(&self, _logical_name: &str) {}
    fn item_bytes(&self, _bytes: u64) {}
    fn item_done(&self) {}
    fn items_done(&self, count: u64) {
        for _ in 0..count { self.item_done(); }
    }
}

#[cfg(test)]
pub(crate) struct NoopPrepareObserver;

#[cfg(test)]
impl LegacyPrepareObserver for NoopPrepareObserver {}

#[derive(Default)]
struct LegacyProgressState {
    read_bytes: u64,
    prepared_bytes: u64,
    last_reported_bytes: u64,
    counts: ImportCounts,
    stage: Option<JobStage>,
    stage_completed: u64,
    stage_total: Option<u64>,
    stage_unit: Option<StageUnit>,
    current_item: Option<String>,
}

/// Progress reporter for a legacy backup restore. It is the cancellation
/// probe, the archive parse observer, and the attachment observer at once, and
/// it charges every stage against one fixed budget of twice the archive size:
/// each byte is read from the archive once and then either verified in the
/// CAS or parsed as the database. Reports never fail the restore; a rejected
/// report only costs the dialog one update.
struct LegacyRestoreProgress<'a> {
    job: &'a JobControl,
    archive_bytes: u64,
    state: RefCell<LegacyProgressState>,
}

impl<'a> LegacyRestoreProgress<'a> {
    fn new(job: &'a JobControl, archive_bytes: u64) -> Self {
        Self {
            job,
            archive_bytes,
            state: RefCell::new(LegacyProgressState::default()),
        }
    }

    fn budget(&self) -> u64 {
        self.archive_bytes.saturating_mul(2).max(1)
    }

    fn completed_bytes(&self, state: &LegacyProgressState) -> u64 {
        state
            .read_bytes
            .saturating_add(state.prepared_bytes)
            .min(self.budget())
    }

    fn report(&self) {
        let mut state = self.state.borrow_mut();
        let completed = self.completed_bytes(&state);
        state.last_reported_bytes = completed;
        let _ = self.job.set_progress(JobProgress {
            completed_bytes: completed,
            total_bytes: Some(self.budget()),
            completed_items: state.counts.entries_read,
            total_items: state.counts.entries_total,
        });
        if let (Some(stage), Some(unit)) = (state.stage, state.stage_unit) {
            let mut detail = JobDetail::new(
                stage,
                unit,
                state.stage_completed,
                state.stage_total,
                state.counts.clone(),
            );
            if let Some(item) = &state.current_item {
                detail = detail.with_current_item(item.clone());
            }
            let _ = self.job.set_detail(detail);
        }
    }

    fn report_if_due(&self) {
        let due = {
            let state = self.state.borrow();
            self.completed_bytes(&state)
                .saturating_sub(state.last_reported_bytes)
                >= PROGRESS_REPORT_INTERVAL_BYTES
        };
        if due {
            self.report();
        }
    }

    fn classify(counts: &mut ImportCounts, logical_name: &str) {
        if matches!(logical_name, DATABASE_ENTRY | ENCRYPTION_ENTRY) {
            return;
        }
        match pocket_risu::classify(logical_name) {
            Ok(Some(pocket_risu::Entry::Payload { .. } | pocket_risu::Entry::Legacy { .. })) => counts.pocket_media += 1,
            Ok(Some(
                pocket_risu::Entry::Metadata { .. } | pocket_risu::Entry::Provenance { .. },
            )) => counts.pocket_metadata += 1,
            Ok(Some(pocket_risu::Entry::Cache | pocket_risu::Entry::Skipped)) => counts.skipped += 1,
            Ok(None) => {
                if cold_key(logical_name).is_some() {
                    counts.cold_storage += 1
                } else if inlay_key_hex(logical_name).is_some() {
                    counts.inlays += 1
                } else {
                    counts.assets += 1
                }
            }
            // Invalid names fail the preflight that follows; they are not counted.
            Err(_) => {}
        }
    }

    /// Scale for the database read: it continues the same byte budget and
    /// keeps the archive's entry counter instead of counting RisuSave blocks.
    fn restore_scale(&self) -> restore::RestoreProgressScale {
        let state = self.state.borrow();
        restore::RestoreProgressScale {
            result_source: None,
            spool_directory: None,
            base_bytes: state.read_bytes.saturating_add(state.prepared_bytes),
            total_bytes: Some(self.budget()),
            fixed_items: Some((state.counts.entries_read, state.counts.entries_total)),
            cold_payloads: None,
            counts: state.counts.clone(),
        }
    }
}

impl CancellationProbe for LegacyRestoreProgress<'_> {
    fn is_cancelled(&self) -> bool {
        self.job.is_cancel_requested()
    }
}

impl LocalBackupParseObserver for LegacyRestoreProgress<'_> {
    fn entry_started(&self, logical_name: &str, _byte_length: u64, index: usize) {
        {
            let mut state = self.state.borrow_mut();
            state.counts.entries_read = index as u64 + 1;
            Self::classify(&mut state.counts, logical_name);
            state.stage = Some(JobStage::ReadingArchive);
            state.stage_unit = Some(StageUnit::Bytes);
            state.stage_completed = state.read_bytes;
            state.stage_total = Some(self.archive_bytes);
            state.current_item = Some(logical_name.to_owned());
        }
        self.report();
    }

    fn bytes_read(&self, total_read: u64) {
        {
            let mut state = self.state.borrow_mut();
            state.read_bytes = total_read.min(self.archive_bytes);
            state.stage_completed = state.read_bytes;
        }
        self.report_if_due();
    }

    fn entries_complete(&self, count: usize) {
        {
            let mut state = self.state.borrow_mut();
            state.counts.entries_total = Some(count as u64);
            state.stage_completed = state.read_bytes;
            state.current_item = None;
        }
        self.report();
    }
}

impl LegacyPrepareObserver for LegacyRestoreProgress<'_> {
    fn begin(&self, total: u64) {
        {
            let mut state = self.state.borrow_mut();
            state.stage = Some(JobStage::PreparingAttachments);
            state.stage_unit = Some(StageUnit::Items);
            state.stage_completed = 0;
            state.stage_total = Some(total);
            state.current_item = None;
        }
        self.report();
    }

    fn item_started(&self, logical_name: &str) {
        self.state.borrow_mut().current_item = Some(logical_name.to_owned());
        self.report();
    }

    fn item_bytes(&self, bytes: u64) {
        {
            let mut state = self.state.borrow_mut();
            state.prepared_bytes = state
                .prepared_bytes
                .saturating_add(bytes)
                .min(self.archive_bytes);
        }
        self.report_if_due();
    }

    fn item_done(&self) {
        self.items_done(1);
    }

    fn items_done(&self, count: u64) {
        {
            let mut state = self.state.borrow_mut();
            state.counts.attachments_prepared += count;
            state.stage_completed += count;
            state.current_item = None;
        }
        self.report();
    }
}

pub(crate) fn restore_legacy_local_backup(
    mut source: OpenedJobSource,
    expected_revision: i64,
    owned_directory: &std::path::Path,
    repository_root: &std::path::Path,
    app: AppHandle,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::ReadingSource)
        .map_err(job_state_error)?;
    let cas = PayloadCas::new(repository_root).map_err(io_job_error)?;
    let progress = LegacyRestoreProgress::new(job, source.total_bytes);
    let mut callback = LegacyDatabaseRestore {
        app,
        cas: &cas,
        repository_root,
        expected_revision,
        job,
        progress: &progress,
        result_source: None,
        result: None,
    };
    let parsed = parse_legacy_local_backup_v1_observed(
        &mut source.file,
        owned_directory,
        PayloadTarget::JobStaging,
        &mut callback,
        &progress,
        &progress,
    );
    match parsed {
        Ok(report) => {
            let mut result = callback.result.take().ok_or_else(|| {
                NativeJobError::new("store-error", "legacy backup database restore did not run")
            })??;
            result.source_bytes = report.source_bytes;
            result.source_sha256 = report.source_sha256;
            Ok(result)
        }
        Err(error) => match callback.result.take() {
            Some(Err(native)) => Err(native),
            _ => Err(local_backup_error(error)),
        },
    }
}

struct LegacyDatabaseRestore<'a> {
    result_source: Option<(u64, String)>,
    app: AppHandle,
    cas: &'a PayloadCas,
    repository_root: &'a std::path::Path,
    expected_revision: i64,
    job: &'a JobControl,
    progress: &'a LegacyRestoreProgress<'a>,
    result: Option<Result<JobResultSummary, NativeJobError>>,
}

impl StrictLocalBackupDatabaseRestore for LegacyDatabaseRestore<'_> {
    fn source_validated(&mut self, bytes: u64, sha256: &str) { self.result_source = Some((bytes, sha256.to_owned())); }
    fn restore_database(
        &mut self,
        database: &StagedLocalBackupEntry,
        entries: &[StagedLocalBackupEntry],
    ) -> Result<(), LocalBackupError> {
        preflight_legacy_restore_entries(entries)?;
        let path = database.staged_path.as_ref().ok_or_else(|| {
            LocalBackupError::database_restore("legacy backup database is not staged")
        })?;
        let decrypted = match account_encryption::prepare(entries, database, self.job) {
            Ok(value) => value,
            Err(error) => {
                self.result = Some(Err(error.clone()));
                return Err(LocalBackupError::database_restore(error.message));
            }
        };
        let file = File::open(decrypted.as_ref().map(|file| file.path()).unwrap_or(path)).map_err(LocalBackupError::io)?;
        let database_bytes = file.metadata().map_err(LocalBackupError::io)?.len();
        let mut durable = DurableCasJob::begin(
            self.repository_root,
            &self.job.id(),
            CasJobKind::LocalBackupRestore,
            crate::asset_repository::job_pins::CasJobOwner::native_file_job(&self.job.id()),
            now_millis(),
        )
        .map_err(LocalBackupError::io)?;
        let payloads = match prepare_legacy_restore_payloads_observed(
            entries,
            self.cas,
            &mut durable,
            self.progress,
            self.progress,
        ) {
            Ok(payloads) => payloads,
            Err(error) => {
                let _ = durable.release(CasReleaseOutcome::Aborted);
                return Err(error);
            }
        };
        if let Err(error) = check_cancelled(self.progress) {
            let _ = durable.release(CasReleaseOutcome::Aborted);
            return Err(error);
        }
        let sink = LegacyReplacementSink {
            app: self.app.clone(),
            asset_keys: payloads.asset_aliases.iter().map(|alias| alias.key.clone()).collect(),
            payloads,
            durable: Mutex::new(durable),
            incomplete: Mutex::new(super::IncompleteRestorePreview::default()),
            activation_pending: std::sync::atomic::AtomicBool::new(false),
        };
        let result = restore::restore_started_risu_save(
            OpenedJobSource {
                file,
                total_bytes: database_bytes,
                custody: None,
            },
            self.expected_revision,
            self.job,
            &sink,
            restore::RestoreProgressScale {
                result_source: self.result_source.clone(),
                spool_directory: path.parent().map(std::path::Path::to_path_buf),
                ..self.progress.restore_scale()
            },
        );
        match result {
            Ok(summary) => {
                self.result = Some(Ok(summary));
                Ok(())
            }
            Err(error) => {
                self.result = Some(Err(error.clone()));
                Err(LocalBackupError::database_restore(error.message))
            }
        }
    }
}

struct LegacyReplacementSink {
    activation_pending: std::sync::atomic::AtomicBool,
    app: AppHandle,
    payloads: PreparedLegacyRestorePayloads,
    /// The asset keys the backup carries a file for.
    asset_keys: HashSet<String>,
    durable: Mutex<DurableCasJob>,
    incomplete: Mutex<super::IncompleteRestorePreview>,
}

impl restore::ReplacementSink for LegacyReplacementSink {
    fn prepare_activation_receipt(&self, staging_id: &str, job: &JobControl, result: &JobResultSummary) -> StoreResult<Option<String>> {
        crate::persistent_store::commands::with_store(self.app.state(), |store| super::restore_receipts::prepare(store, job, staging_id, result))
    }

    fn activation_receipt(&self, staging_id: &str, authority: &str) -> StoreResult<Option<i64>> {
        let result = crate::persistent_store::commands::with_store(self.app.state(), |store| store.lww_file_restore_receipt(staging_id, staging_id, authority));
        if result.is_err() { self.activation_pending.store(true, std::sync::atomic::Ordering::Release); }
        if matches!(result, Ok(Some(_))) && self.release_durable(CasReleaseOutcome::Committed).is_err() {
            self.activation_pending.store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }
    fn begin(&self) -> StoreResult<StagingResult> {
        crate::persistent_store::commands::with_store_mut(
            self.app.state(),
            PersistentStore::replace_begin,
        )
    }

    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
        self.add_missing_assets(missing_asset_references(root, &self.asset_keys))?;
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_upstream_root(staging_id, root)
        })
    }

    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
        for preset in presets {
            self.add_missing_assets(missing_asset_references(preset, &self.asset_keys))?;
        }
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_upstream_presets(staging_id, presets)
        })
    }

    fn put_legacy_root(&self, staging_id: &str, root: &crate::persistent_store::upstream_stream::RootSpool) -> StoreResult<()> {
        self.add_missing_assets(missing_root_asset_references(root, &self.asset_keys)?)?;
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_upstream_stream(staging_id, root)
        })
    }

    fn prepare_character(&self, character: Value) -> StoreResult<Value> {
        self.add_missing_assets(missing_asset_references(&character, &self.asset_keys))?;
        let mut unavailable = Vec::new();
        let expanded = cold_expansion::expand_cold_payloads(std::slice::from_ref(&character), &self.payloads.cold_payloads, &mut unavailable)
            .map_err(|message| crate::persistent_store::StoreError::Store { message })?;
        if !unavailable.is_empty() {
            let mut preview = self.incomplete.lock().map_err(|error| crate::persistent_store::StoreError::Store { message: error.to_string() })?;
            preview.unavailable_cold_keys.extend(unavailable);
            if let Some(name) = character.get("name").and_then(Value::as_str) { preview.character_names.push(name.to_owned()); }
        }
        Ok(expanded.and_then(|mut values| values.pop()).unwrap_or(character))
    }

    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_characters(staging_id, characters)
        })
    }

    fn supports_incremental_characters(&self) -> bool { true }

    fn expands_cold_payloads(&self) -> bool { true }

    fn cold_payload_path(&self, key: &str) -> StoreResult<Option<std::path::PathBuf>> {
        Ok(self.payloads.cold_payloads.get(key).cloned())
    }

    fn unavailable_cold_payload_reason(&self, key: &str) -> Option<restore::ColdPayloadUnavailable> {
        self.payloads.unavailable_cold_payloads.get(key).copied()
    }

    fn unavailable_cold_payload(&self, key: &str, character_name: &str) -> StoreResult<()> {
        let mut preview = self.incomplete.lock().map_err(|error| crate::persistent_store::StoreError::Store { message: error.to_string() })?;
        preview.unavailable_cold_keys.push(key.to_owned());
        if !character_name.is_empty() { preview.character_names.push(character_name.to_owned()); }
        Ok(())
    }

    fn put_character_detail(&self, staging_id: &str, detail: &Value, conversation_count: i64) -> StoreResult<()> {
        self.add_missing_assets(missing_asset_references(detail, &self.asset_keys))?;
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_character_detail(staging_id, detail, conversation_count)
        })
    }

    fn put_conversation_row(&self, staging_id: &str, character_id: &str, configured_index: i64, detail: &Value, recent_at: i64, message_count: i64) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_conversation_row(staging_id, character_id, configured_index, detail, recent_at, message_count)
        })
    }

    fn add_conversation_messages(&self, staging_id: &str, character_id: &str, conversation_id: &str, start: i64, messages: &[Value]) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_conversation_messages(staging_id, character_id, conversation_id, start, messages)
        })
    }

    fn incomplete_restore_preview(&self) -> StoreResult<super::IncompleteRestorePreview> {
        let mut preview = self.incomplete.lock().map_err(|error| crate::persistent_store::StoreError::Store { message: error.to_string() })?.clone();
        preview.unavailable_cold_keys.sort();
        preview.unavailable_cold_keys.dedup();
        preview.character_names.sort();
        preview.character_names.dedup();
        preview.invalid_inlays = self.payloads.invalid_inlays.clone();
        preview.omissions.inlays = self.payloads.skipped_inlays;
        Ok(preview)
    }

    fn staged_plugin_preview(
        &self,
        staging_id: &str,
    ) -> StoreResult<crate::persistent_store::commit::StagedPluginPreview> {
        crate::persistent_store::commands::with_store(self.app.state(), |store| {
            store.staged_plugin_preview(staging_id)
        })
    }

    fn commit(&self, staging_id: &str, expected_revision: i64, binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_asset_aliases(staging_id, &self.payloads.asset_aliases)?;
            self.durable
                .lock()
                .map_err(|error| crate::persistent_store::StoreError::Store {
                    message: format!("legacy backup CAS job mutex is poisoned: {error}"),
                })?
                .seal(store, now_millis())
                .map_err(crate::persistent_store::StoreError::from)
        })?;
        let committed = crate::persistent_store::commands::commit_staged_replacement(
            &self.app,
            staging_id,
            Some(expected_revision),
            binding_authority,
        );
        if committed.is_ok() {
            self.release_durable(CasReleaseOutcome::Committed)?;
        }
        committed
    }

    fn abort(&self, staging_id: &str) -> StoreResult<()> {
        let abort = crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_abort(staging_id)
        });
        let release = self.release_durable(CasReleaseOutcome::Aborted);
        abort.and(release)
    }
}

impl LegacyReplacementSink {
    fn add_missing_assets(&self, missing: u64) -> StoreResult<()> {
        if missing > 0 {
            let mut preview = self.incomplete.lock().map_err(|error| crate::persistent_store::StoreError::Store { message: error.to_string() })?;
            preview.omissions.missing_assets = preview.omissions.missing_assets.saturating_add(missing);
        }
        Ok(())
    }

    fn release_durable(&self, outcome: CasReleaseOutcome) -> StoreResult<()> {
        self.durable
            .lock()
            .map_err(|error| crate::persistent_store::StoreError::Store {
                message: format!("legacy backup CAS job mutex is poisoned: {error}"),
            })?
            .release(outcome)
            .map_err(crate::persistent_store::StoreError::from)
    }
}

impl Drop for LegacyReplacementSink {
    fn drop(&mut self) {
        if self.activation_pending.load(std::sync::atomic::Ordering::Acquire) { return; }
        if let Ok(durable) = self.durable.get_mut() {
            let _ = durable.release(CasReleaseOutcome::Aborted);
        }
    }
}

/// The asset references in `value` the backup carries no file for. RisuAI
/// leaves out files it does not keep, and the reference is restored as it is.
fn missing_asset_references(value: &Value, carried: &HashSet<String>) -> u64 {
    let mut missing = 0;
    let _ = compatible_projection::visit_asset_fields(value, &mut |_, value| {
        if value.as_str().is_some_and(|key| key.starts_with("assets/") && !carried.contains(key)) {
            missing += 1;
        }
        Ok::<(), std::convert::Infallible>(())
    });
    missing
}

/// The same count for the root fields of a streamed database.
fn missing_root_asset_references(
    root: &crate::persistent_store::upstream_stream::RootSpool,
    carried: &HashSet<String>,
) -> StoreResult<u64> {
    let mut missing = 0;
    for key in ["customBackground", "userIcon"] {
        if let Some(value) = root.scalar(key)? {
            missing += missing_asset_references(&Value::Object(Map::from_iter([(key.to_owned(), value)])), carried);
        }
    }
    for key in ["personas", "modules", "characterOrder", "botPresets"] {
        root.visit(key, |_, member| {
            missing += missing_asset_references(&member, carried);
            Ok(())
        })?;
    }
    Ok(missing)
}

fn local_backup_error(error: LocalBackupError) -> NativeJobError {
    let item = match error.code {
        LocalBackupErrorCode::RecordTooLarge => Some("record"),
        LocalBackupErrorCode::DecodedBlockTooLarge => Some("backup"),
        LocalBackupErrorCode::LengthOverflow => Some("backup-entry"),
        _ => None,
    };
    if let Some(item) = item { return NativeJobError::too_large(item, error.message); }
    let code = match error.code {
        LocalBackupErrorCode::CompatibilityImportRequired => "compatibility-import-required",
        LocalBackupErrorCode::UnsupportedEncryption | LocalBackupErrorCode::UnsupportedFormat => {
            "unsupported-format"
        }
        LocalBackupErrorCode::Cancelled => "cancelled",
        LocalBackupErrorCode::Io => "store-error",
        _ => "invalid-source",
    };
    NativeJobError::new(code, error.message)
}

fn io_job_error(error: io::Error) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

fn store_job_error(error: crate::persistent_store::StoreError) -> NativeJobError {
    let code = match &error {
        crate::persistent_store::StoreError::RevisionConflict { .. } => "revision-conflict",
        crate::persistent_store::StoreError::Validation { .. } => "invalid-source",
        _ => "store-error",
    };
    NativeJobError::new(code, error.to_string())
}

fn job_state_error(error: String) -> NativeJobError {
    NativeJobError::new("store-error", error)
}

fn destination_job_error(
    error: destination::DestinationWriteError,
    phase_error: Option<String>,
) -> NativeJobError {
    if let Some(error) = phase_error {
        return job_state_error(error);
    }
    match error {
        destination::DestinationWriteError::InvalidSource => {
            NativeJobError::new("store-error", "legacy backup staged archive is invalid")
        }
        destination::DestinationWriteError::InvalidDestination => NativeJobError::new(
            "invalid-destination",
            "legacy backup destination is invalid",
        ),
        destination::DestinationWriteError::Cancelled => {
            NativeJobError::new("cancelled", "legacy backup export was cancelled")
        }
        destination::DestinationWriteError::Io { source, .. } => io_job_error(source),
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) struct PreparedLegacyRestorePayloads {
    pub(crate) asset_aliases: Vec<AssetAlias>,
    pub(crate) invalid_inlays: Vec<String>,
    pub(crate) skipped_inlays: u64,
    /// Upstream cold payload key to the staged file that holds its body.
    pub(crate) cold_payloads: HashMap<String, std::path::PathBuf>,
    /// Upstream cold payload keys whose entry could not be staged.
    pub(crate) unavailable_cold_payloads: HashMap<String, restore::ColdPayloadUnavailable>,
}

#[cfg(test)]
pub(crate) fn prepare_legacy_restore_payloads(
    entries: &[StagedLocalBackupEntry],
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    prepare_legacy_restore_payloads_observed(
        entries,
        cas,
        durable,
        cancellation,
        &NoopPrepareObserver,
    )
}

/// Whether an archive entry becomes an asset or cold alias. Metadata and cache
/// entries only feed the payloads they describe.
fn produces_alias(logical_name: &str) -> bool {
    if matches!(logical_name, DATABASE_ENTRY | ENCRYPTION_ENTRY) {
        return false;
    }
    matches!(
        pocket_risu::classify(logical_name),
        Ok(Some(pocket_risu::Entry::Payload { .. } | pocket_risu::Entry::Legacy { .. })) | Ok(None)
    )
}

pub(crate) fn prepare_legacy_restore_payloads_observed(
    entries: &[StagedLocalBackupEntry],
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    prepare_legacy_restore_payloads_with_limits(entries, cas, durable, cancellation, observer, restore::RestoreLimits::default())
}

pub(super) fn prepare_legacy_restore_payloads_with_limits(
    entries: &[StagedLocalBackupEntry],
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
    limits: restore::RestoreLimits,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    preflight_legacy_restore_entries(entries)?;
    let pocket_metadata = pocket_risu::index_metadata(entries, cancellation)?;
    let mut asset_aliases = Vec::new();
    let mut invalid_inlays = Vec::new();
    let mut skipped_inlays = 0;
    let mut cold_payloads = HashMap::new();
    let mut unavailable_cold_payloads = HashMap::new();
    let mut asset_keys = HashSet::new();
    let mut incoming = Vec::new();
    let mut incoming_names = Vec::new();
    observer.begin(
        entries
            .iter()
            .filter(|entry| produces_alias(&entry.logical_name))
            .count() as u64,
    );

    for entry in entries {
        check_cancelled(cancellation)?;
        if matches!(
            entry.logical_name.as_str(),
            DATABASE_ENTRY | ENCRYPTION_ENTRY
        ) {
            continue;
        }
        if let Some(pocket_entry) = pocket_risu::classify(&entry.logical_name)? {
            if matches!(pocket_entry, pocket_risu::Entry::Skipped) { skipped_inlays += 1; }
            if let pocket_risu::Entry::Legacy { id } = pocket_entry {
                observer.item_started(&entry.logical_name);
                match pocket_risu::prepare_legacy(entry, id, &pocket_metadata, cas, durable, cancellation) {
                    Ok(None) => skipped_inlays += 1,
                    Ok(Some(alias)) => {
                        if !asset_keys.insert(("inlay".to_owned(), id.to_owned())) {
                            return Err(invalid("legacy backup contains a duplicate Inlay key"));
                        }
                        asset_aliases.push(alias);
                    }
                    Err(error) if error.code == LocalBackupErrorCode::DatabaseRestore => invalid_inlays.push(entry.logical_name.clone()),
                    Err(error) => return Err(error),
                }
                observer.item_done();
            }
            if let pocket_risu::Entry::Payload { id, ext } = pocket_entry {
                if !asset_keys.insert(("inlay".to_owned(), id.to_owned())) {
                    return Err(invalid("legacy backup contains a duplicate Inlay key"));
                }
                let alias = match pocket_risu::prepare(
                    entry,
                    id,
                    ext,
                    &pocket_metadata,
                    cancellation,
                ) {
                    Ok(Some(alias)) => alias,
                    Ok(None) => { skipped_inlays += 1; observer.item_done(); continue; }
                    Err(error) if error.code == LocalBackupErrorCode::DatabaseRestore => {
                        invalid_inlays.push(entry.logical_name.clone());
                        observer.item_done();
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                asset_aliases.push(alias);
                incoming.push(import_payload(entry)?);
                incoming_names.push(entry.logical_name.as_str());
            }
            continue;
        }
        if let Some(key) = cold_key(&entry.logical_name) {
            observer.item_started(&entry.logical_name);
            let staged = prepare_cold_with_limits(entry, cancellation, observer, limits)?;
            if cold_payloads.contains_key(&key) || unavailable_cold_payloads.contains_key(&key) {
                return Err(invalid(
                    "legacy backup contains a duplicate cold payload key",
                ));
            }
            match staged {
                Ok(path) => { cold_payloads.insert(key, path); }
                Err(reason) => { unavailable_cold_payloads.insert(key, reason); }
            }
        } else if let Some(encoded_key) = inlay_key_hex(&entry.logical_name) {
            observer.item_started(&entry.logical_name);
            let alias = match prepare_inlay(entry, encoded_key, cas, durable, cancellation, observer) {
                Ok(alias) => alias,
                Err(error) if error.code == LocalBackupErrorCode::DatabaseRestore => {
                    invalid_inlays.push(entry.logical_name.clone());
                    observer.item_done();
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !asset_keys.insert((alias.kind.clone(), alias.key.clone())) {
                return Err(invalid("legacy backup contains a duplicate Inlay key"));
            }
            asset_aliases.push(alias);
        } else {
            let alias = prepare_asset(entry)?;
            if !asset_keys.insert((alias.kind.clone(), alias.key.clone())) {
                return Err(invalid("legacy backup contains a duplicate asset key"));
            }
            asset_aliases.push(alias);
            incoming.push(import_payload(entry)?);
            incoming_names.push(entry.logical_name.as_str());
            continue;
        }
        observer.item_done();
    }
    let mut completed = 0;
    let mut displayed = None;
    let mut last_item_report: Option<std::time::Instant> = None;
    prepare_import_payloads(cas, durable, &incoming, ImportPinning::DuringPreparation, &|| cancellation.is_cancelled(), |bytes, count, current| {
        observer.item_bytes(bytes);
        if completed < count {
            observer.items_done((count - completed) as u64);
            completed = count;
            displayed = None;
        }
        if let Some(index) = current.filter(|index| Some(*index) != displayed
            && last_item_report.is_none_or(|last| last.elapsed() >= std::time::Duration::from_millis(50))) {
            observer.item_started(incoming_names[index]);
            displayed = Some(index);
            last_item_report = Some(std::time::Instant::now());
        }
    }).map_err(|error| cancellation_io(error, cancellation))?;
    check_cancelled(cancellation)?;
    Ok(PreparedLegacyRestorePayloads {
        invalid_inlays,
        skipped_inlays,
        asset_aliases,
        cold_payloads,
        unavailable_cold_payloads,
    })
}

fn preflight_legacy_restore_entries(
    entries: &[StagedLocalBackupEntry],
) -> Result<(), LocalBackupError> {
    for entry in entries {
        pocket_risu::classify(&entry.logical_name)?;
    }
    Ok(())
}

fn prepare_asset(
    entry: &StagedLocalBackupEntry,
) -> Result<AssetAlias, LocalBackupError> {
    let name = entry
        .logical_name
        .rsplit('/')
        .next()
        .unwrap_or(&entry.logical_name)
        .to_owned();
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .to_owned();
    Ok(AssetAlias {
        key: format!("assets/{}", entry.logical_name),
        object_hash: Some(entry.sha256.clone()),
        kind: "asset".to_owned(),
        size: to_alias_size(entry.byte_length)?,
        mime: String::new(),
        name,
        ext,
        inlay_type: None,
        width: None,
        height: None,
        metadata: Value::Object(Map::new()),
    })
}

fn import_payload(entry: &StagedLocalBackupEntry) -> Result<ImportPayload, LocalBackupError> {
    Ok(ImportPayload {
        path: entry.staged_path.clone().ok_or_else(|| invalid("legacy backup payload is not staged"))?,
        hash: entry.sha256.clone(),
        size: entry.byte_length,
        sync_required: true,
        progress_items: 1,
    })
}

fn prepare_inlay(
    entry: &StagedLocalBackupEntry,
    encoded_key: &str,
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
) -> Result<AssetAlias, LocalBackupError> {
    let expected_key = String::from_utf8(
        hex::decode(encoded_key).map_err(|_| invalid("invalid Inlay entry name"))?,
    )
    .map_err(|_| invalid("invalid Inlay entry key"))?;
    let mut file = open_staged(entry)?;
    if entry.byte_length < 4 { return Err(invalid("legacy backup Inlay metadata length is invalid")); }
    let mut length = [0_u8; 4];
    file.read_exact(&mut length).map_err(LocalBackupError::io)?;
    let header_length = u32::from_le_bytes(length);
    if header_length == 0
        || header_length > MAX_METADATA_BYTES
        || u64::from(header_length) + 4 > entry.byte_length
    {
        return Err(invalid("legacy backup Inlay metadata length is invalid"));
    }
    let mut header = vec![0_u8; header_length as usize];
    file.read_exact(&mut header).map_err(LocalBackupError::io)?;
    let metadata: Value = serde_json::from_slice(&header)
        .map_err(|_| invalid("legacy backup Inlay metadata is invalid"))?;
    let object = metadata
        .as_object()
        .ok_or_else(|| invalid("legacy backup Inlay metadata must be an object"))?;
    let key = string_field(object, "key")?;
    if key != expected_key || key.is_empty() || key.starts_with("assets/") {
        return Err(invalid(
            "legacy backup Inlay key does not match its entry name",
        ));
    }
    if string_field(object, "kind")? != "inlay" {
        return Err(invalid("legacy backup Inlay kind is invalid"));
    }
    let inlay_type = string_field(object, "inlayType")?;
    if !matches!(
        inlay_type.as_str(),
        "image" | "video" | "audio" | "signature"
    ) {
        return Err(invalid("legacy backup Inlay type is invalid"));
    }
    let mut source = CancellationReader::observed(file, cancellation, observer);
    let payload = durable
        .prepare_reader(cas, &mut source, CasObjectRole::DirectObject)
        .map_err(|error| cancellation_io(error, cancellation))?;
    let dimension = |field: &str| -> Result<Option<i64>, LocalBackupError> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .filter(|value| *value >= 0)
                .map(Some)
                .ok_or_else(|| invalid(format!("legacy backup Inlay {field} is invalid"))),
        }
    };
    let mut retained_metadata = object.clone();
    for key in [
        "key",
        "kind",
        "size",
        "mime",
        "name",
        "ext",
        "inlayType",
        "width",
        "height",
    ] {
        retained_metadata.remove(key);
    }
    Ok(AssetAlias {
        key,
        object_hash: Some(payload.content_hash),
        kind: "inlay".to_owned(),
        size: to_alias_size(payload.byte_size)?,
        mime: string_field(object, "mime")?,
        name: string_field(object, "name")?,
        ext: string_field(object, "ext")?,
        inlay_type: Some(inlay_type),
        width: dimension("width")?,
        height: dimension("height")?,
        metadata: Value::Object(retained_metadata),
    })
}

/// Validates a staged upstream cold payload without materializing its full body.
/// The body is spliced into its referencing record when characters are staged.
/// An entry that is not one JSON value, or exceeds the decoded limit, is staged
/// as unavailable, and whatever references it reports the omission.
#[cfg(test)]
fn prepare_cold_with_limit(
    entry: &StagedLocalBackupEntry,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
    max_decoded_bytes: u64,
) -> Result<Result<std::path::PathBuf, restore::ColdPayloadUnavailable>, LocalBackupError> {
    prepare_cold_with_limits(entry, cancellation, observer, restore::RestoreLimits {
        max_decoded_block_bytes: max_decoded_bytes, ..Default::default()
    })
}

fn prepare_cold_with_limits(
    entry: &StagedLocalBackupEntry,
    cancellation: &dyn CancellationProbe,
    observer: &dyn LegacyPrepareObserver,
    limits: restore::RestoreLimits,
) -> Result<Result<std::path::PathBuf, restore::ColdPayloadUnavailable>, LocalBackupError> {
    let file = open_staged(entry)?;
    if entry.byte_length > limits.max_decoded_block_bytes || file.metadata().map_err(LocalBackupError::io)?.len() > limits.max_decoded_block_bytes {
        return Ok(Err(restore::ColdPayloadUnavailable::TooLarge));
    }
    let source = CancellationReader::observed(file, cancellation, observer);
    // Cold plugin values may be any JSON. Apply record limits only when a character
    // or conversation references the payload, through the common restore reader.
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(source));
    let parsed = <IgnoredAny as serde::Deserialize>::deserialize(&mut deserializer)
        .and_then(|_| deserializer.end());
    if let Err(error) = parsed {
        check_cancelled(cancellation)?;
        if error.is_io() {
            return Err(LocalBackupError::io(error.into()));
        }
        return Ok(Err(restore::ColdPayloadUnavailable::Damaged));
    }

    check_cancelled(cancellation)?;
    entry
        .staged_path
        .clone()
        .map(Ok)
        .ok_or_else(|| invalid("legacy backup cold payload is not staged"))
}

fn validate_referenced_cold_records<R: Read>(source: R, max_record_bytes: u64) -> Result<(), serde_json::Error> {
    let record_bytes = restore::RecordBudget::new(max_record_bytes);
    record_bytes.set(Some(0));
    let source = restore::RecordLimitReader { inner: BufReader::new(source), bytes: &record_bytes };
    let mut deserializer = serde_json::Deserializer::from_reader(source);
    serde::de::Deserializer::deserialize_any(&mut deserializer, ColdRootVisitor(&record_bytes))?;
    deserializer.end()
}

struct ColdRootVisitor<'a>(&'a restore::RecordBudget);

impl<'de> Visitor<'de> for ColdRootVisitor<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a cold payload array or object")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<bool, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.0.set(None);
        while sequence.next_element_seed(restore::BoundedValueSeed(self.0))?.is_some() {}
        Ok(true)
    }

    fn visit_map<A>(self, mut map: A) -> Result<bool, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.0.set(None);
        let mut compatible = false;
        while let Some(key) = map.next_key_seed(restore::BoundedStringSeed(self.0))? {
            compatible |= matches!(key.as_str(), "character" | "message");
            match key.as_str() {
                "message" => {
                    map.next_value_seed(ColdFeatureSeed("messages", self.0))?;
                }
                "character" => {
                    map.next_value_seed(ColdFeatureSeed("character", self.0))?;
                }
                "savedToggleValues" | "bindedPersona" => {
                    map.next_value_seed(restore::BoundedValueSeed(self.0))?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(compatible)
    }
}

// Read feature fields within the record limit while keeping the cold-payload read streaming.
struct ColdFeatureSeed<'a>(&'static str, &'a restore::RecordBudget);
impl<'de> serde::de::DeserializeSeed<'de> for ColdFeatureSeed<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        let record_bytes = self.1;
        record_bytes.set(Some(0));
        let result = deserializer.deserialize_any(self);
        record_bytes.set(None);
        result
    }
}
impl<'de> Visitor<'de> for ColdFeatureSeed<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a cold feature container")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        self.1.set(None);
        if self.0 == "messages" {
            while sequence.next_element_seed(restore::BoundedValueSeed(self.1))?.is_some() {}
        } else {
            while sequence
                .next_element_seed(ColdFeatureSeed("chat", self.1))?
                .is_some()
            {}
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        self.1.set(None);
        while let Some(key) = map.next_key_seed(restore::BoundedStringSeed(self.1))? {
            match key.as_str() {
                "chats" if self.0 == "character" => {
                    map.next_value_seed(ColdFeatureSeed("chats", self.1))?;
                }
                "message" if self.0 == "chat" => {
                    map.next_value_seed(ColdFeatureSeed("messages", self.1))?;
                }
                "savedToggleValues" | "bindedPersona" if self.0 == "chat" => {
                    map.next_value_seed(restore::BoundedValueSeed(self.1))?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

struct CancellationReader<'a, R> {
    inner: R,
    cancellation: &'a dyn CancellationProbe,
    observer: Option<&'a dyn LegacyPrepareObserver>,
}

impl<'a, R> CancellationReader<'a, R> {
    fn new(inner: R, cancellation: &'a dyn CancellationProbe) -> Self {
        Self {
            inner,
            cancellation,
            observer: None,
        }
    }

    fn observed(
        inner: R,
        cancellation: &'a dyn CancellationProbe,
        observer: &'a dyn LegacyPrepareObserver,
    ) -> Self {
        Self {
            inner,
            cancellation,
            observer: Some(observer),
        }
    }
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("legacy backup operation was cancelled"));
        }
        let read = self.inner.read(buffer)?;
        if let Some(observer) = self.observer {
            observer.item_bytes(read as u64);
        }
        Ok(read)
    }
}

fn open_staged(entry: &StagedLocalBackupEntry) -> Result<File, LocalBackupError> {
    let path = entry
        .staged_path
        .as_ref()
        .ok_or_else(|| invalid("legacy backup payload is not staged"))?;
    File::open(path).map_err(LocalBackupError::io)
}

fn string_field(object: &Map<String, Value>, field: &str) -> Result<String, LocalBackupError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("legacy backup Inlay {field} is invalid")))
}

fn inlay_key_hex(name: &str) -> Option<&str> {
    let key = name.strip_prefix("inlay_")?.strip_suffix(".risuinlay")?;
    (!key.is_empty() && key.len() % 2 == 0 && key.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))).then_some(key)
}

fn cold_key(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("coldstorage/") {
        let key = rest.strip_suffix(".json")?;
        return (!key.is_empty() && !key.contains(['/', '\\'])).then(|| key.to_owned());
    }
    let key = name.strip_prefix("coldstorage_")?.strip_suffix(".json")?;
    uuid::Uuid::parse_str(key)
        .ok()
        .map(|value| value.to_string())
}

fn to_alias_size(size: u64) -> Result<i64, LocalBackupError> {
    i64::try_from(size).map_err(|_| invalid("legacy backup payload is too large"))
}

fn check_cancelled(cancellation: &dyn CancellationProbe) -> Result<(), LocalBackupError> {
    if cancellation.is_cancelled() {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::Cancelled,
            "legacy backup job was cancelled",
        ));
    }
    Ok(())
}

fn cancellation_io(error: io::Error, cancellation: &dyn CancellationProbe) -> LocalBackupError {
    if cancellation.is_cancelled() {
        LocalBackupError::new(
            LocalBackupErrorCode::Cancelled,
            "legacy backup job was cancelled",
        )
    } else {
        LocalBackupError::io(error)
    }
}

fn invalid(message: impl Into<String>) -> LocalBackupError {
    LocalBackupError::new(LocalBackupErrorCode::DatabaseRestore, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::PayloadCas;
    use crate::import_export_jobs::{parse_json_card, JobStaging};
    use crate::local_backup::{
        parse_legacy_local_backup_v1, NeverCancelled, PayloadTarget, StagedLocalBackupEntry,
        StrictLocalBackupDatabaseRestore,
    };
    use crate::native_file_jobs::content::content_classification_limits;
    use crate::native_file_jobs::{
        character_json_export::export_character_json, JobKind, JobRegistry, JobState,
    };
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::fs;
    use std::io::Cursor;

    struct PayloadPlanner<'a> {
        cas: &'a PayloadCas,
        durable: DurableCasJob,
        prepared: Option<PreparedLegacyRestorePayloads>,
    }

    impl StrictLocalBackupDatabaseRestore for PayloadPlanner<'_> {
        fn restore_database(
            &mut self,
            _database: &StagedLocalBackupEntry,
            entries: &[StagedLocalBackupEntry],
        ) -> Result<(), crate::local_backup::LocalBackupError> {
            self.prepared = Some(prepare_legacy_restore_payloads(
                entries,
                self.cas,
                &mut self.durable,
                &NeverCancelled,
            )?);
            Ok(())
        }
    }

    fn entry(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn cold_preparation_streams_opaque_values_without_applying_chat_record_limits() {
        struct ObservedBytes { total: Cell<u64>, largest: Cell<u64> }
        impl LegacyPrepareObserver for ObservedBytes {
            fn item_bytes(&self, bytes: u64) { self.total.set(self.total.get() + bytes); self.largest.set(self.largest.get().max(bytes)); }
        }
        for (prefix, suffix) in [
            (br#"[{"role":"user","data":""#.as_slice(), b"\"}]".as_slice()),
            (br#"{"character":{"chats":[{"message":[{"data":""#.as_slice(), b"\"}]}]}}".as_slice()),
            (br#"{"savedToggleValues":{"toggle_large":""#.as_slice(), b"\"},\"message\":[]}".as_slice()),
            (br#"{"character":""#.as_slice(), b"\",\"message\":[]}".as_slice()),
            (br#"{""#.as_slice(), b"\":0,\"message\":[]}".as_slice()),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("cold.json");
            let mut output = File::create(&path).unwrap();
            output.write_all(prefix).unwrap();
            for _ in 0..3 { output.write_all(&[b'x';64*1024]).unwrap(); }
            output.write_all(suffix).unwrap();
            drop(output);
            let entry = StagedLocalBackupEntry { logical_name:"coldstorage/large.json".to_owned(), byte_length:fs::metadata(&path).unwrap().len(), sha256:String::new(), staged_path:Some(path) };
            let observed = ObservedBytes { total: Cell::new(0), largest: Cell::new(0) };
            prepare_cold_with_limits(&entry, &NeverCancelled, &observed, restore::RestoreLimits { max_record_bytes: 64 * 1024, ..Default::default() }).unwrap().unwrap();
            assert_eq!(observed.total.get(), entry.byte_length);
            assert!(observed.largest.get() <= 64 * 1024);
            let error = cold_expansion::load_with_limits(entry.staged_path.as_ref().unwrap(), entry.byte_length, 64 * 1024).unwrap_err();
            assert!(error.contains("restore record limit exceeded"), "{error}");
        }
    }

    #[test]
    fn cold_preparation_checks_decoded_file_size_before_any_parse() {
        struct UnexpectedRead;
        impl LegacyPrepareObserver for UnexpectedRead {
            fn item_bytes(&self, _: u64) { panic!("oversized cold file was read"); }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cold.json");
        fs::write(&path, br#"{"message":[]}"#).unwrap();
        for (byte_length, limit) in [(0, 8), (1024, 8), (1024, 64)] {
            let entry = StagedLocalBackupEntry { logical_name:"coldstorage/large.json".to_owned(), byte_length, sha256:String::new(), staged_path:Some(path.clone()) };
            let staged = prepare_cold_with_limit(&entry, &NeverCancelled, &UnexpectedRead, limit).unwrap();
            assert_eq!(staged, Err(restore::ColdPayloadUnavailable::TooLarge));
        }
    }

    #[test]
    fn cancellation_reader_uses_a_non_retryable_error_for_exact_reads() {
        struct AlwaysCancelled;
        impl CancellationProbe for AlwaysCancelled {
            fn is_cancelled(&self) -> bool {
                true
            }
        }

        let mut reader = CancellationReader::new(io::empty(), &AlwaysCancelled);
        let error = reader.read_exact(&mut [0_u8; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            cancellation_io(error, &AlwaysCancelled).code,
            LocalBackupErrorCode::Cancelled
        );

        // A cancelled cold entry read cancels the restore instead of staging the entry as damaged.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cold.json");
        fs::write(&path, b"{broken").unwrap();
        let entry = StagedLocalBackupEntry { logical_name:"coldstorage/cold.json".to_owned(), byte_length:7, sha256:String::new(), staged_path:Some(path) };
        assert_eq!(prepare_cold_with_limit(&entry, &AlwaysCancelled, &NoopPrepareObserver, 1024).unwrap_err().code, LocalBackupErrorCode::Cancelled);
    }

    #[derive(Default)]
    struct ArchiveCapture {
        entries: Vec<(String, Vec<u8>)>,
    }

    impl StrictLocalBackupDatabaseRestore for ArchiveCapture {
        fn restore_database(
            &mut self,
            _database: &StagedLocalBackupEntry,
            entries: &[StagedLocalBackupEntry],
        ) -> Result<(), crate::local_backup::LocalBackupError> {
            self.entries = entries
                .iter()
                .map(|entry| {
                    let bytes = fs::read(
                        entry
                            .staged_path
                            .as_ref()
                            .expect("job-staged archive entry has a path"),
                    )
                    .map_err(crate::local_backup::LocalBackupError::io)?;
                    Ok((entry.logical_name.clone(), bytes))
                })
                .collect::<Result<_, crate::local_backup::LocalBackupError>>()?;
            Ok(())
        }
    }

    #[test]
    fn parallel_assets_and_pocket_media_report_their_current_logical_name() {
        struct Observer { names: RefCell<Vec<String>>, completed: Cell<u64> }
        impl LegacyPrepareObserver for Observer {
            fn item_started(&self, name: &str) { self.names.borrow_mut().push(name.to_owned()); }
            fn items_done(&self, count: u64) { self.completed.set(self.completed.get() + count); }
        }
        struct Planner<'a> { cas: &'a PayloadCas, durable: &'a mut DurableCasJob, observer: &'a Observer }
        impl StrictLocalBackupDatabaseRestore for Planner<'_> {
            fn restore_database(&mut self, _: &StagedLocalBackupEntry, entries: &[StagedLocalBackupEntry]) -> Result<(), LocalBackupError> {
                prepare_legacy_restore_payloads_observed(entries, self.cas, self.durable, &NeverCancelled, self.observer)?;
                Ok(())
            }
        }
        for name in ["portrait.png", "inlay/pocket.webp"] {
            let root = tempfile::tempdir().unwrap();
            drop(PersistentStore::open(root.path()).unwrap());
            let cas = PayloadCas::new(root.path()).unwrap();
            let mut durable = DurableCasJob::begin(root.path(), &Uuid::new_v4().to_string(), CasJobKind::LocalBackupRestore,
                crate::asset_repository::job_pins::CasJobOwner::for_test(), 0).unwrap();
            let observer = Observer { names: RefCell::new(Vec::new()), completed: Cell::new(0) };
            let mut planner = Planner { cas: &cas, durable: &mut durable, observer: &observer };
            let bytes = [entry(name.as_bytes(), b"synthetic body"), entry(b"database.risudat", b"RISUSAVE\0")].concat();
            parse_legacy_local_backup_v1(&mut Cursor::new(bytes), &test_legacy_staging(root.path()), PayloadTarget::JobStaging,
                &mut planner, &NeverCancelled).unwrap();
            assert!(observer.names.borrow().iter().any(|reported| reported == name));
            assert_eq!(observer.completed.get(), 1);
            durable.release(CasReleaseOutcome::Aborted).unwrap();
        }
    }

    #[test]
    fn legacy_restore_progress_charges_read_prepare_and_database_against_one_budget() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let progress = LegacyRestoreProgress::new(&job, 1_000);

        progress.entry_started("assets/portrait.png", 300, 0);
        progress.bytes_read(300);
        progress.entry_started("inlay/picture.webp", 200, 1);
        progress.entry_started("inlay_sidecar/picture", 10, 2);
        progress.entry_started("inlay_meta/picture", 10, 3);
        progress.entry_started("inlay_thumb/picture", 10, 4);
        progress.entry_started("coldstorage/cold-1.json", 40, 5);
        progress.entry_started("database.risudat", 430, 6);
        progress.bytes_read(1_000);
        progress.entries_complete(7);

        let status = job.status();
        assert_eq!(
            status.progress,
            JobProgress {
                completed_bytes: 1_000,
                total_bytes: Some(2_000),
                completed_items: 7,
                total_items: Some(7),
            }
        );
        let detail = status.detail.expect("archive read reports detail");
        assert_eq!(detail.stage, JobStage::ReadingArchive);
        assert_eq!(detail.stage_unit, StageUnit::Bytes);
        assert_eq!(
            (detail.stage_completed, detail.stage_total),
            (1_000, Some(1_000))
        );
        assert!(detail.current_item.is_none());
        assert_eq!(
            detail.counts,
            ImportCounts {
                entries_read: 7,
                entries_total: Some(7),
                assets: 1,
                pocket_media: 1,
                pocket_metadata: 2,
                skipped: 1,
                cold_storage: 1,
                ..ImportCounts::default()
            }
        );

        progress.begin(3);
        progress.item_started("assets/portrait.png");
        let started = job.status().detail.unwrap();
        assert_eq!(started.stage, JobStage::PreparingAttachments);
        assert_eq!(started.current_item.as_deref(), Some("assets/portrait.png"));
        progress.item_bytes(300);
        progress.item_done();

        let status = job.status();
        assert_eq!(status.progress.completed_bytes, 1_300);
        assert_eq!(status.progress.total_bytes, Some(2_000));
        assert_eq!(status.progress.completed_items, 7);
        let detail = status.detail.unwrap();
        assert_eq!(detail.stage, JobStage::PreparingAttachments);
        assert_eq!(detail.stage_unit, StageUnit::Items);
        assert_eq!((detail.stage_completed, detail.stage_total), (1, Some(3)));
        assert_eq!(detail.counts.attachments_prepared, 1);
        assert!(detail.current_item.is_none());

        let scale = progress.restore_scale();
        assert_eq!(scale.base_bytes, 1_300);
        assert_eq!(scale.total_bytes, Some(2_000));
        assert_eq!(scale.fixed_items, Some((7, Some(7))));
        assert_eq!(scale.counts.cold_storage, 1);

        progress.items_done(2);
        let detail = job.status().detail.unwrap();
        assert_eq!((detail.stage_completed, detail.stage_total), (3, Some(3)));
        assert_eq!(detail.counts.attachments_prepared, 3);
        assert_eq!(progress.restore_scale().base_bytes, 1_300);
    }

    #[test]
    fn legacy_restore_progress_never_exceeds_its_budget_or_reports_backwards() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let progress = LegacyRestoreProgress::new(&job, 100);
        progress.entry_started("assets/a.png", 100, 0);
        progress.bytes_read(100);
        progress.entries_complete(1);
        progress.begin(1);
        progress.item_started("assets/a.png");
        // A reader that overshoots (for example a re-read) is clamped to the archive size.
        progress.item_bytes(150);
        progress.item_done();
        let status = job.status();
        assert_eq!(status.progress.completed_bytes, 200);
        assert_eq!(status.progress.total_bytes, Some(200));
        assert_eq!(status.state, JobState::Running);
        assert_eq!(progress.restore_scale().base_bytes, 200);
    }



    #[test]
    fn counts_asset_references_the_backup_carries_no_file_for() {
        use serde_json::json;
        let carried = HashSet::from(["assets/carried.png".to_owned()]);
        let character = json!({
            "image": "assets/carried.png",
            "emotionImages": [["joy", "assets/gone-joy.png"]],
            "additionalAssets": [["extra", "assets/gone-extra.png", "png"]],
            "ccAssets": [{"uri": "assets/gone-cc.png"}, {"uri": "embeded://card.png"}],
            "vits": {"files": {"voice": "assets/gone-voice.bin"}},
            "firstMessage": "assets/gone-in-text.png",
        });
        assert_eq!(missing_asset_references(&character, &carried), 4);
        let root = crate::persistent_store::upstream_stream::RootSpool::new(Default::default()).unwrap();
        for (key, value) in [
            ("customBackground", json!("assets/gone-background.png")),
            ("userIcon", json!("assets/carried.png")),
            ("personas", json!([{"icon": "assets/gone-persona.png"}])),
            ("modules", json!([{"assets": [["module", "assets/gone-module.png", "png"]]}])),
            ("characterOrder", json!(["character", {"img": "assets/gone-folder.png"}])),
            ("botPresets", json!([{"image": "assets/gone-preset.png"}])),
            ("mainPrompt", json!("assets/gone-in-text.png")),
        ] {
            root.put(key, Some(&value)).unwrap();
        }
        assert_eq!(missing_root_asset_references(&root, &carried).unwrap(), 5);
    }

    #[test]
    fn prepares_asset_inlay_and_cold_payloads_without_mutating_live_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let _store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let inlay_metadata = serde_json::json!({
            "key": "inlay-id",
            "kind": "inlay",
            "inlayType": "image",
            "mime": "image/webp",
            "name": "image.webp",
            "ext": "webp",
            "size": 4,
            "width": 2,
            "height": 3,
            "pocketRisu": {
                "createdAt": 11,
                "updatedAt": 22,
                "charId": "synthetic-character",
                "chatId": "synthetic-chat"
            },
            "unknownMetadata": {
                "nested": {
                    "retained": true
                }
            }
        });
        let header = serde_json::to_vec(&inlay_metadata).unwrap();
        let mut inlay = Vec::new();
        inlay.extend_from_slice(&(header.len() as u32).to_le_bytes());
        inlay.extend_from_slice(&header);
        inlay.extend_from_slice(b"webp");
        let cold_key = "123e4567-e89b-42d3-a456-426614174000";
        let bytes = [
            entry(b"portrait.png", b"original"),
            entry(b"inlay_696e6c61792d6964.risuinlay", &inlay),
            entry(
                format!("coldstorage_{cold_key}.json").as_bytes(),
                br#"{"message":[{"data":"cold"}]}"#,
            ),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                crate::asset_repository::job_pins::CasJobOwner::for_test(),
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };

        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            &test_legacy_staging(directory.path()),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();

        let prepared = planner.prepared.take().unwrap();
        assert_eq!(prepared.asset_aliases.len(), 2);
        assert_eq!(prepared.asset_aliases[0].key, "assets/portrait.png");
        assert_eq!(prepared.asset_aliases[1].key, "inlay-id");
        assert_eq!(
            prepared.asset_aliases[1].metadata,
            serde_json::json!({
                "pocketRisu": {
                    "createdAt": 11,
                    "updatedAt": 22,
                    "charId": "synthetic-character",
                    "chatId": "synthetic-chat"
                },
                "unknownMetadata": {
                    "nested": {
                        "retained": true
                    }
                }
            })
        );
        assert!(prepared.cold_payloads[cold_key].is_file());
        for alias in &prepared.asset_aliases {
            assert!(cas
                .object_path(alias.object_hash.as_deref().unwrap())
                .unwrap()
                .is_some());
        }
        assert!(!directory
            .path()
            .join("persistent/persistent.sqlite3")
            .exists());
        assert_eq!(planner.durable.pin_count(), 2);
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn legacy_backup_empty_mime_asset_roundtrips_through_native_json_with_exact_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"legacy-empty-mime-payload";
        let bytes = [
            entry(b"legacy.bin", payload),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                crate::asset_repository::job_pins::CasJobOwner::for_test(),
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };
        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            &test_legacy_staging(directory.path()),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();
        let prepared_payloads = planner.prepared.take().unwrap();
        let alias = prepared_payloads.asset_aliases[0].clone();
        assert_eq!(alias.key, "assets/legacy.bin");
        assert!(alias.mime.is_empty());

        let mut store = PersistentStore::open(directory.path()).unwrap();
        let character = serde_json::json!({
            "type": "character",
            "chaId": "legacy-json-character",
            "name": "Legacy JSON",
            "image": "",
            "ccAssets": [{
                "type": "x-risu-asset",
                "uri": alias.key,
                "name": "legacy",
                "ext": "bin"
            }],
            "additionalAssets": [],
            "emotionImages": [],
            "triggerscript": [],
            "customscript": [],
            "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&staging, &serde_json::json!({}))
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store.replace_put_asset_aliases(&staging, &[alias]).unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let metadata = serde_json::json!({
            "spec": "chara_card_v3",
            "spec_version": "3.0",
            "data": {
                "name": "Legacy JSON",
                "extensions": {"risuai": {
                    "triggerscript": [],
                    "customScripts": []
                }},
                "assets": [
                    {"type": "x-risu-asset", "uri": "assets/legacy.bin", "name": "legacy", "ext": "bin"},
                    {"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}
                ]
            }
        });
        let prepared = store.prepare_risu_save_export(revision).unwrap();
        let owned = directory.path().join("json-owned");
        let parsed_root = directory.path().join("json-parsed");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&parsed_root).unwrap();
        let destination = directory.path().join("legacy.json");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();

        export_character_json(
            prepared,
            "legacy-json-character",
            metadata,
            &owned,
            &directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();

        let exported = fs::read(&destination).unwrap();
        assert!(
            String::from_utf8_lossy(&exported).contains("data:application/octet-stream;base64,")
        );
        let staging = JobStaging::open(&parsed_root).unwrap();
        let parsed = parse_json_card(
            &mut exported.as_slice(),
            &staging,
            &content_classification_limits(),
            &|| false,
        )
        .unwrap();
        assert_eq!(parsed.payloads[0].payload.byte_size, payload.len() as u64);
        assert_eq!(
            parsed.payloads[0].payload.sha256,
            hex::encode(Sha256::digest(payload))
        );
        assert_eq!(
            fs::read(parsed_root.join(&parsed.payloads[0].payload.staged_name)).unwrap(),
            payload
        );
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn encrypted_archive_keeps_ciphertext_for_the_native_reader() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = [
            entry(b"encryption.risudat", br#"{"type":"account","time":123}"#),
            entry(b"database.risudat", b"synthetic ciphertext"),
        ].concat();
        let mut capture = ArchiveCapture::default();
        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes), &test_legacy_staging(directory.path()),
            PayloadTarget::JobStaging, &mut capture, &NeverCancelled,
        ).unwrap();
        assert_eq!(capture.entries[1], ("database.risudat".to_owned(), b"synthetic ciphertext".to_vec()));
    }

    #[test]
    fn imports_pocket_risu_110_inlays_with_sidecars_after_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let _store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let bytes = [
            entry(b"portrait.png", b"original"),
            entry(b"inlay/pocket.webp", b"webp"),
            entry(b"inlay/voice.mp3", b"audio"),
            entry(b"inlay_meta/pocket", br#"{"derived":true}"#),
            entry(b"inlay_thumb/pocket", b"thumbnail"),
            entry(
                b"inlay_sidecar/pocket",
                br#"{"ext":"webp","name":"original.webp","type":"image","width":2,"height":3}"#,
            ),
            entry(
                b"inlay_sidecar/voice",
                br#"{"ext":"mp3","name":"voice.mp3","type":"audio"}"#,
            ),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                crate::asset_repository::job_pins::CasJobOwner::for_test(),
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };

        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            &test_legacy_staging(directory.path()),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();

        let prepared = planner.prepared.take().unwrap();
        assert_eq!(prepared.asset_aliases.len(), 3);
        for (key, bytes, mime, inlay_type) in [
            ("pocket", b"webp".as_slice(), "image/webp", "image"),
            ("voice", b"audio".as_slice(), "audio/mpeg", "audio"),
        ] {
            let alias = prepared
                .asset_aliases
                .iter()
                .find(|alias| alias.key == key)
                .unwrap();
            assert_eq!(alias.kind, "inlay");
            assert_eq!(alias.mime, mime);
            assert_eq!(alias.inlay_type.as_deref(), Some(inlay_type));
            assert_eq!(
                fs::read(
                    cas.object_path(alias.object_hash.as_deref().unwrap())
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                bytes
            );
        }
        let image = &prepared.asset_aliases[1];
        assert_eq!(image.name, "original.webp");
        assert_eq!((image.width, image.height), (Some(2), Some(3)));
        assert!(!directory
            .path()
            .join("persistent/persistent.sqlite3")
            .exists());
        assert_eq!(planner.durable.pin_count(), 3);
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }


}

#[cfg(test)]
fn test_legacy_staging(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join("native-file-jobs/jobs").join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&path).unwrap();
    path
}
