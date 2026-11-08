use super::{JobControl, JobPhase, JobResultSummary, NativeJobError, OpenedJobSource};
use crate::device_backup::{
    capture_prepared_native_sections, journal_prepared_native_sections,
    prepare_native_sections, resume_journaled_native_restore, DeviceBackupError, DeviceBackupState,
    Spool,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    local_backup::CancellationProbe,
    persistent_store::PersistentStore,
    portable_backup::{self, Catalog, VerifiedArchive},
};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};
use tauri::Manager;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PortableSelection {
    pub(crate) library: bool,
    pub(crate) device_sections: Vec<String>,
    #[serde(default)]
    pub(crate) allow_source_preservation: bool,
}
impl Default for PortableSelection {
    fn default() -> Self {
        Self {
            library: true,
            device_sections: vec!["hypa".into(), "local-plugins".into(), "local-settings".into()],
            allow_source_preservation: false,
        }
    }
}
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestorePreview {
    pub(crate) library_included: bool,
    pub(crate) repair_required: bool,
    pub(crate) device_sections: Vec<String>,
    /// What is wrong with the archive's library. An archive the gate refuses still reports this,
    /// which is the only way a reader learns what to leave out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diagnosis: Option<crate::data_health::ScanResult>,
}
fn restore_preview(
    archive: &VerifiedArchive,
    probe: &dyn CancellationProbe,
) -> Result<RestorePreview, NativeJobError> {
    let mut sections = Vec::new();
    let mut statement = archive
        .db
        .prepare(
            "SELECT section FROM device_sections WHERE included=1 AND complete=1 ORDER BY section",
        )
        .map_err(error)?;
    let mut rows = statement.query([]).map_err(error)?;
    while let Some(row) = rows.next().map_err(error)? {
        if sections.len() == 1024 {
            return Err(error("Archive contains too many device sections"));
        }
        let section: String = row.get(0).map_err(error)?;
        risunest_external_storage_format::section::SectionKind::parse(&section).map_err(|_| {
            NativeJobError::new(
                "invalid-source",
                "Portable backup contains an unsupported device section",
            )
        })?;
        sections.push(section);
    }
    let diagnosis = match archive.manifest.library_included {
        true => {
            let mut findings = crate::data_health::Findings::new(2000);
            archive.scan_library(&mut findings, probe).map_err(error)?;
            Some(crate::data_health::ScanResult::new(
                0,
                archive_scanned_at(),
                crate::data_health::ScanDepth::Deep,
                findings,
            ))
        }
        false => None,
    };
    Ok(RestorePreview {
        library_included: archive.manifest.library_included,
        repair_required: archive.manifest.repair_required,
        device_sections: sections,
        diagnosis,
    })
}

fn archive_scanned_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn append_cleanup_failure(
    primary: &mut NativeJobError,
    operation: &str,
    cleanup: impl std::fmt::Display,
) {
    *primary = NativeJobError::new(
        &primary.code,
        format!("{}; {operation} failed: {cleanup}", primary.message),
    );
}

struct Probe<'a> {
    job: &'a JobControl,
    progress: std::sync::Mutex<(super::JobProgress, bool, Option<String>)>,
    export: Option<std::sync::Arc<ExportProgress>>,
}
struct ExportProgress {
    job: std::sync::Arc<JobControl>,
    detail: std::sync::Mutex<super::JobDetail>,
    failure: std::sync::Mutex<Option<String>>,
}
impl ExportProgress {
    fn begin(&self, stage: super::JobStage, total: Option<u64>) -> Result<(), NativeJobError> {
        let mut detail = self.detail.lock().map_err(|failure| error(failure.to_string()))?;
        *detail = detail.advance(stage, super::StageUnit::Bytes, total);
        self.job.set_detail(detail.clone()).map_err(error)
    }
    fn add(&self, bytes: u64, downloaded: bool) {
        let mut detail = self.detail.lock().unwrap();
        if downloaded {
            if detail.stage == super::JobStage::PreparingExport {
                *detail = detail.advance(super::JobStage::DownloadingExport, super::StageUnit::Bytes, None);
            }
            if detail.stage != super::JobStage::DownloadingExport { return; }
        } else if matches!(detail.stage, super::JobStage::PreparingExport | super::JobStage::DownloadingExport) { return; }
        detail.stage_completed = detail.stage_completed.saturating_add(bytes);
        if let Err(error) = self.job.set_detail(detail.clone()) {
            *self.failure.lock().unwrap() = Some(error);
        }
    }
}
impl<'a> Probe<'a> {
    fn new(job: &'a JobControl) -> Self { Self { job, progress: std::sync::Mutex::new((super::JobProgress::default(), false, None)), export: None } }
    fn for_export(job: &'a std::sync::Arc<JobControl>) -> Self {
        let mut probe = Self::new(job);
        probe.export = Some(std::sync::Arc::new(ExportProgress {
            job: job.clone(),
            detail: std::sync::Mutex::new(super::JobDetail::new(super::JobStage::PreparingExport, super::StageUnit::Bytes, 0, None, super::ImportCounts::default())),
            failure: std::sync::Mutex::new(None),
        }));
        probe
    }
    fn export_stage(&self, stage: super::JobStage, total: Option<u64>) -> Result<(), NativeJobError> {
        if let Some(export) = &self.export { export.begin(stage, total)?; }
        Ok(())
    }
    fn track_bytes(&self, total: u64) -> Result<(), NativeJobError> {
        let mut state = self.progress.lock().map_err(|_| error("portable progress mutex poisoned"))?;
        state.0.total_bytes = Some(total);
        state.1 = true;
        self.job.set_progress(state.0).map_err(error)
    }
    fn result(&self) -> Result<(), NativeJobError> {
        if let Some(export) = &self.export {
            if let Some(failure) = export.failure.lock().map_err(|failure| error(failure.to_string()))?.clone() { return Err(error(failure)); }
        }
        match self.progress.lock().map_err(|_| error("portable progress mutex poisoned"))?.2.as_ref() {
            Some(message) => Err(NativeJobError::new("job-error", message)), None => Ok(())
        }
    }
}
impl CancellationProbe for Probe<'_> {
    fn cancellation_flag(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> { Some(self.job.cancellation_flag()) }
    fn is_cancelled(&self) -> bool { self.job.is_cancel_requested() }
    fn backup_bytes_processed(&self, bytes: u64) {
        if let Some(export) = &self.export { export.add(bytes, false); }
        let mut state = self.progress.lock().unwrap();
        if state.1 {
            state.0.completed_bytes = state.0.completed_bytes.saturating_add(bytes).min(state.0.total_bytes.unwrap_or(u64::MAX));
            if let Err(error) = self.job.set_progress(state.0) { state.2 = Some(error); }
        }
    }
    fn backup_item_processed(&self) {
        let mut state = self.progress.lock().unwrap();
        state.0.completed_items = state.0.completed_items.saturating_add(1);
        if let Err(error) = self.job.set_progress(state.0) { state.2 = Some(error); }
    }
    fn backup_download_observer(&self) -> Option<std::sync::Arc<dyn Fn(u64) + Send + Sync>> {
        self.export.as_ref().map(|export| {
            let export = export.clone();
            std::sync::Arc::new(move |bytes| export.add(bytes, true)) as std::sync::Arc<dyn Fn(u64) + Send + Sync>
        })
    }
}
fn storage_full(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::StorageFull)
            || error.downcast_ref::<serde_json::Error>().is_some_and(|error| error.io_error_kind() == Some(std::io::ErrorKind::StorageFull))
            || error.downcast_ref::<rusqlite::Error>().is_some_and(|error| matches!(error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::DiskFull)) {
            return true;
        }
        match error.source() { Some(source) => error = source, None => return false }
    }
}
fn error(error: impl std::fmt::Display + 'static) -> NativeJobError {
    let value = &error as &dyn std::any::Any;
    if value.downcast_ref::<portable_backup::Error>().is_some_and(|error| matches!(error, portable_backup::Error::Cancelled))
        || value.downcast_ref::<Box<portable_backup::Error>>().is_some_and(|error| matches!(error.as_ref(), portable_backup::Error::Cancelled)) {
        return NativeJobError::new("cancelled", "Portable backup was cancelled");
    }
    let full = value.downcast_ref::<std::io::Error>().is_some_and(|error| storage_full(error))
        || value.downcast_ref::<portable_backup::Error>().is_some_and(|error| storage_full(error))
        || value.downcast_ref::<Box<portable_backup::Error>>().is_some_and(|error| storage_full(error.as_ref()))
        || value.downcast_ref::<rusqlite::Error>().is_some_and(|error| storage_full(error));
    let message = if let Some(error) = value.downcast_ref::<std::io::Error>() {
        format!("io failure ({:?})", error.kind())
    } else if let Some(error) = value.downcast_ref::<rusqlite::Error>() {
        match error {
            rusqlite::Error::FromSqlConversionFailure(index, _, _) => format!("column {index} is not readable"),
            rusqlite::Error::SqliteFailure(code, _) => format!("sqlite failure ({:?}, {})", code.code, code.extended_code),
            _ => "sqlite failure".to_owned(),
        }
    } else {
        crate::native_log::failure_text(&error)
    };
    NativeJobError::new(if full { "local-storage-full" } else { "portable-backup-failed" }, message)
}
fn device_error(failure: DeviceBackupError) -> NativeJobError {
    if failure.code == "device-cancelled" {
        NativeJobError::new("cancelled", failure.message)
    } else {
        error(failure)
    }
}
fn portable_error(failure: portable_backup::Error) -> NativeJobError {
    match failure {
        portable_backup::Error::Store(failure) => super::error::store_error(failure),
        portable_backup::Error::Cancelled => {
            NativeJobError::new("cancelled", "Portable backup was cancelled")
        }
        failure @ portable_backup::Error::RemotePayloadUnavailable => {
            NativeJobError::new("remote-asset-unavailable", failure.to_string())
        }
        failure @ portable_backup::Error::PayloadMissing => {
            NativeJobError::new("asset-missing", failure.to_string())
        }
        failure => error(failure),
    }
}
pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn new_pins(store: &PersistentStore, kind: CasJobKind) -> Result<DurableCasJob, NativeJobError> {
    let id = uuid::Uuid::new_v4().to_string();
    DurableCasJob::begin(
        store.repository_root(),
        &id,
        kind,
        crate::asset_repository::job_pins::CasJobOwner::native_file_job(&id),
        now(),
    )
    .map_err(error)
}

fn new_pins_for_job(
    store: &PersistentStore,
    kind: CasJobKind,
    job_id: &str,
) -> Result<DurableCasJob, NativeJobError> {
    DurableCasJob::begin(store.repository_root(), job_id, kind, crate::asset_repository::job_pins::CasJobOwner::native_file_job(job_id), now()).map_err(error)
}
pub(super) fn finish_durable_job(
    outcome: Result<JobResultSummary, NativeJobError>,
    durable: &mut DurableCasJob,
) -> Result<JobResultSummary, NativeJobError> {
    let release_outcome = if outcome.is_ok() {
        CasReleaseOutcome::Committed
    } else {
        CasReleaseOutcome::Aborted
    };
    match (outcome, durable.release(release_outcome)) {
        (Ok(mut result), Err(_)) => {
            result.warning_codes.push("cleanup-failed".into());
            Ok(result)
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(cleanup)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!(
                "{}; durable CAS job release failed: {cleanup}",
                error.message
            ),
        )),
        (Err(error), Ok(())) => Err(error),
    }
}

fn finish_pins(
    outcome: Result<JobResultSummary, NativeJobError>,
    pins: &mut DurableCasJob,
) -> Result<JobResultSummary, NativeJobError> {
    if pins.is_sealed() {
        return finish_durable_job(outcome, pins);
    }
    // Read-only exports did not install CAS objects. Their temporary pins are released without
    // claiming an object-store commit or registering previously unowned source objects.
    match (outcome, pins.release(CasReleaseOutcome::Aborted)) {
        (Ok(mut result), Err(_)) => {
            result.warning_codes.push("cleanup-failed".into());
            Ok(result)
        }
        (result, Ok(())) => result,
        (Err(failure), Err(_)) => Err(failure),
    }
}

fn capture(
    store: &mut PersistentStore,
    revision: i64,
    owned: &Path,
    pins: &mut DurableCasJob,
    probe: &dyn CancellationProbe,
    source_build: &str,
    allow_source_preservation: bool,
    job: Option<&JobControl>,
) -> Result<portable_backup::CapturedLibrary, NativeJobError> {
    match portable_backup::capture_library_with_ready(store, revision, owned, pins, allow_source_preservation, probe, source_build, &|| {
        match job {
            Some(job) => job.publish_export_capture(revision).map_err(|failure| portable_backup::Error::Io(std::io::Error::other(failure))),
            None => Ok(()),
        }
    }) {
        Err(portable_backup::Error::SourceNeedsPreservation) => {
            Err(NativeJobError::new(
                "source-preservation-confirmation-required",
                "Source preservation may include account information and requires confirmation",
            ))
        }
        result => result.map_err(portable_error),
    }
}

fn write_verified(
    catalog: Catalog,
    repair: bool,
    path: &Path,
    owned: &Path,
    probe: &Probe<'_>,
) -> Result<VerifiedArchive, NativeJobError> {
    catalog
        .write_candidate(path, repair, probe)
        .map_err(portable_error)?;
    probe.export_stage(super::JobStage::VerifyingExport, None)?;
    let verified = VerifiedArchive::open(File::open(path).map_err(error)?, owned, probe)
        .map_err(portable_error)?;
    if !verified.manifest.repair_required && verified.manifest.library_included {
        verified.validate_library(probe).map_err(error)?;
    }
    Ok(verified)
}

fn publish(
    source: &Path,
    destination: &Path,
    owned: &Path,
    job: &JobControl,
    probe: &Probe<'_>,
) -> Result<crate::persistent_store::export::destination::DestinationWriteResult, NativeJobError> {
    let copied = std::cell::Cell::new(0_u64);
    crate::persistent_store::export::destination::write_portable_destination_controlled(
        owned,
        source,
        destination
            .parent()
            .ok_or_else(|| error("destination has no parent"))?,
        destination,
        || job.is_cancel_requested(),
        |progress| {
            let previous = copied.replace(progress.copied_bytes);
            probe.backup_bytes_processed(progress.copied_bytes.saturating_sub(previous));
        },
        || {
            job.commit_export_publication().map_err(|message| {
                if job.is_cancel_requested() {
                    crate::persistent_store::export::destination::DestinationWriteError::Cancelled
                } else {
                    crate::persistent_store::export::destination::DestinationWriteError::Io {
                        operation: "commit portable export publication",
                        source: std::io::Error::other(message),
                    }
                }
            })
        },
    )
    .map_err(|failure| {
        if let crate::persistent_store::export::destination::DestinationWriteError::Io { source, .. } = &failure {
            if storage_full(source) { return NativeJobError::new("local-storage-full", source.to_string()); }
        }
        super::error::destination_error_with(
            failure,
            "Verified portable archive is unavailable",
            "Portable backup destination is invalid",
            "Portable backup publication was cancelled",
        )
    })
}

pub(crate) fn export_portable(
    destination: Option<&Path>,
    revision: i64,
    owned: &Path,
    handoffs: &Path,
    store: PersistentStore,
    job: &std::sync::Arc<JobControl>,
    device: Option<(&tauri::AppHandle, &PortableSelection)>,
    source_build: &str,
) -> Result<JobResultSummary, NativeJobError> {
    super::job_transition(job, job.start(JobPhase::WritingExport))?;
    let fallback = PortableSelection::default();
    let selection = device.map(|(_, selection)| selection).unwrap_or(&fallback);
    export_portable_running(
        destination,
        revision,
        owned,
        handoffs,
        store,
        job,
        selection,
        source_build,
    )
}

fn export_portable_running(
    destination: Option<&Path>,
    revision: i64,
    owned: &Path,
    handoffs: &Path,
    mut store: PersistentStore,
    job: &std::sync::Arc<JobControl>,
    selection: &PortableSelection,
    source_build: &str,
) -> Result<JobResultSummary, NativeJobError> {
    #[cfg(test)] let _source_observer = portable_backup::source_io::attach(&job.source_io_scope);
    if !selection.library || selection.device_sections.len()!=3 || selection.device_sections.iter().cloned().collect::<std::collections::BTreeSet<_>>()
        != ["hypa".to_owned(),"local-plugins".to_owned(),"local-settings".to_owned()].into() {
        return Err(NativeJobError::new("invalid-full-backup-scope","A full backup requires every section"));
    }
    if store.revision().map_err(error)? != revision {
        return Err(NativeJobError::new(
            "revision-conflict",
            "Library changed before portable backup capture",
        ));
    }
    if selection.library {
        require_capacity(owned, store.portable_export_lower_bound().map_err(error)?)?;
    }
    let probe = Probe::for_export(job);
    let mut pins = new_pins(&store, CasJobKind::OfficialPublicationOrExportPreparation)?;
    let outcome = (|| {
        let captured = if selection.library {
            capture(&mut store, revision, owned, &mut pins, &probe, source_build, selection.allow_source_preservation, Some(job))?
        } else {
            let catalog =
                Catalog::create(owned, source_build, revision).map_err(error)?;
            catalog
                .db
                .execute(
                    "UPDATE backup_info SET value='false' WHERE key='libraryIncluded'",
                    [],
                )
                .map_err(error)?;
            portable_backup::CapturedLibrary {
                catalog,
                repair_required: false,
            }
        };
        let payload_bytes: i64 = captured.catalog.db.query_row("SELECT coalesce(sum(byte_length),0) FROM objects", [], |row| row.get(0)).map_err(error)?;
        probe.track_bytes((payload_bytes.max(0) as u64).saturating_mul(3))?;
        probe.export_stage(super::JobStage::WritingExport, None)?;
        let path = owned.join("archive.risunest.part");
        let archive = write_verified(
            captured.catalog,
            captured.repair_required,
            &path,
            owned,
            &probe,
        )?;
        let counts = counts(&archive)?;
        let repair = archive.manifest.repair_required;
        drop(archive);
        let (destination, handoff) = match destination {
            Some(path) => (path.to_path_buf(), None),
            None => {
                fs::create_dir_all(handoffs).map_err(error)?;
                let path =
                    handoffs.join(format!("risunest-backup-{}.risunest", uuid::Uuid::new_v4()));
                (path.clone(), Some(path.to_string_lossy().into_owned()))
            }
        };
        super::job_transition(job, job.set_phase(JobPhase::PublishingDestination))?;
        probe.export_stage(super::JobStage::PublishingDestination, Some(fs::metadata(&path).map_err(error)?.len()))?;
        probe.result()?;
        let published = publish(&path, &destination, owned, job, &probe)?;
        probe.result()?;
        Ok(JobResultSummary {
            export_exclusions: None,
            revision,
            source_bytes: published.bytes,
            source_sha256: published.sha256,
            source_fingerprint_kind: crate::native_file_jobs::SourceFingerprintKind::WholeFileSha256,
            character_count: counts.0,
            preset_count: counts.1,
            warning_codes: if repair {
                vec!["source-preserved-repair-required".into()]
            } else {
                vec![]
            },
            handoff_path: handoff,
            publication: None,
        })
    })();
    finish_pins(outcome, &mut pins)
}

pub(crate) struct NativePortableRestoreContext<'a> {
    pub(crate) persistent: &'a crate::persistent_store::commands::PersistentStoreState,
    pub(crate) coordinator: &'a DeviceBackupState,
}

fn begin_native_restore(
    context: &NativePortableRestoreContext<'_>,
    selection: &PortableSelection,
    store: &mut PersistentStore,
    revision: i64,
    stage: Option<&str>,
    job: &JobControl,
    source: &[crate::device_backup::PreparedDeviceSection],
    probe: &dyn CancellationProbe,
    durable_session_started: &mut bool,
) -> Result<String, NativeJobError> {
    let guard = context.persistent
        .acquire_device_maintenance()
        .map_err(error)?;
    let coordinator = context.coordinator;
    coordinator.attach_maintenance_guard(guard).map_err(error)?;
    let id = match coordinator.create_native_portable_session(
        &job.id(),
        selection.library,
        &selection.device_sections,
        revision,
        stage.map(str::to_owned),
    ) {
        Ok(id) => id,
        Err(failure) => {
            let _ = coordinator.release_unused_maintenance();
            return Err(error(failure));
        }
    };
    if let Err(failure) = job.set_device_session(&id) {
        let failure = error(failure);
        let _ = coordinator.fail(&id, "archive-restore-setup-failed");
        let _ = coordinator.recovery_complete(&id);
        let _ = coordinator.cleanup(&id);
        return Err(failure);
    }
    *durable_session_started = true;
    let prepared = (|| {
        let header = crate::persistent_store::lww::Header { binding_authority: store.lww_binding_authority().map_err(error)?, request_id: job.id() };
        let staged: &PersistentStore = store;
        coordinator.set_library_replacement(&id,&header,stage.into_iter().flat_map(|stage| staged.replacement_source_units(stage))).map_err(error)?;
        journal_prepared_native_sections(&coordinator, &id, Spool::Source, source)
            .map_err(error)?;
        coordinator.source_ready(&id).map_err(error)?;
        let rollback = capture_prepared_native_sections(store, &selection.device_sections, probe)
            .map_err(error)?;
        journal_prepared_native_sections(&coordinator, &id, Spool::Rollback, &rollback)
            .map_err(error)?;
        coordinator.prepared(&id).map_err(error)?;
        Ok(())
    })();
    if let Err(failure) = prepared {
        return Err(failure);
    }
    Ok(id)
}

pub(crate) fn restore_portable(
    source: OpenedJobSource,
    already_owned: bool,
    revision: i64,
    owned: &Path,
    store: PersistentStore,
    job: &JobControl,
    device: Option<(&tauri::AppHandle, Option<&PortableSelection>)>,
) -> Result<JobResultSummary, NativeJobError> {
    if let Some((app,selection))=device {
        let persistent=app.state::<crate::persistent_store::commands::PersistentStoreState>();
        let coordinator=app.state::<DeviceBackupState>();
        let context=NativePortableRestoreContext {persistent:&persistent,coordinator:&coordinator};
        restore_portable_with_context(source,already_owned,revision,owned,store,job,Some((&context,selection)))
    } else {
        restore_portable_with_context(source,already_owned,revision,owned,store,job,None)
    }
}

pub(crate) fn restore_portable_with_context(
    mut source: OpenedJobSource,
    already_owned: bool,
    revision: i64,
    owned: &Path,
    store: PersistentStore,
    job: &JobControl,
    device: Option<(&NativePortableRestoreContext<'_>, Option<&PortableSelection>)>,
) -> Result<JobResultSummary, NativeJobError> {
    let custody=source.custody.take();
    let outcome=restore_portable_inner(source,already_owned,revision,owned,store,job,device);
    if let Some(custody)=custody {
        if let Err(cleanup)=custody.finish() {
            return Err(NativeJobError::new("cleanup-failed",match outcome {
                Ok(_) => cleanup.message,
                Err(failure) => format!("{}; source cleanup failed: {}",failure.message,cleanup.message),
            }));
        }
    }
    outcome
}

fn restore_portable_inner(
    source: OpenedJobSource,
    already_owned: bool,
    mut revision: i64,
    owned: &Path,
    mut store: PersistentStore,
    job: &JobControl,
    device: Option<(&NativePortableRestoreContext<'_>, Option<&PortableSelection>)>,
) -> Result<JobResultSummary, NativeJobError> {
    #[cfg(test)] let _source_observer = portable_backup::source_io::attach(&job.source_io_scope);
    job.start(JobPhase::ReadingSource).map_err(error)?;
    let probe = Probe::new(job);
    probe.track_bytes(source.total_bytes.saturating_mul(3))?;
    let _ = already_owned;
    let mut input = source.file;
    if super::raw_recovery::is_raw_recovery_archive(&mut input)? {
        return Err(NativeJobError::new("rescue-format-not-restorable", "RisuNest rescue archives cannot be imported or restored"));
    }
    let archive = VerifiedArchive::open_for_restore(input, owned, &probe).map_err(error)?;
    let source_sha256 = archive.manifest.catalog_sha256.clone();
    let fallback = match device {
        Some((_, None)) => job
            .wait_for_portable_selection(restore_preview(&archive, &probe)?)
            .map_err(error)?,
        _ => PortableSelection::default(),
    };
    let selection = device
        .and_then(|(_, selection)| selection)
        .unwrap_or(&fallback);
    let context = device.map(|(context, _)| context);
    if !selection.library || selection.device_sections.len()!=3 || selection.device_sections.iter().cloned().collect::<std::collections::BTreeSet<_>>()
        != ["hypa".to_owned(),"local-plugins".to_owned(),"local-settings".to_owned()].into() {
        return Err(NativeJobError::new("invalid-full-backup-scope","A full backup requires every section"));
    }
    job.set_replaces_library(selection.library).map_err(error)?;
    if selection.library {
        archive.validate_library(&probe).map_err(error)?;
    }
    let mut pins = new_pins_for_job(&store, CasJobKind::LocalBackupRestore, &job.id())?;
    let mut journal_owned = false;
    let outcome = (|| {
        job.set_phase(JobPhase::StagingDatabase).map_err(error)?;
        let stage = store
            .stage_portable_records(&archive.db, &probe)
            .map_err(error)?;
        store.stage_portable_units(&stage.staging_id,&archive.db,&probe).map_err(error)?;
        let scratch = crate::external_storage::leftovers::scratch_directory(store.repository_root()).map_err(error)?;
        let prepared_device =
            prepare_native_sections(&archive, &selection.device_sections, &scratch, &probe).map_err(error)?;
        let mut committed = false;
        let activated = (|| {
            let counts = store.portable_staged_counts(&stage.staging_id).map_err(error)?;
            if let Some(finalized) = job.wait_for_restore_finalization().map_err(error)? {
                revision = finalized;
            }
            if store.revision().map_err(error)? != revision {
                return Err(NativeJobError::new(
                    "revision-conflict",
                    "Library changed before portable restore activation",
                ));
            }
            if selection.library {
                if probe.is_cancelled() {
                    return Err(NativeJobError::new(
                        "cancelled",
                        "Portable restore cancelled before activation",
                    ));
                }
            }
            probe.result()?;
            archive.check_identity().map_err(error)?;
            let skipped: u64 = prepared_device.iter().map(|section| section.rows().skipped()).sum();
            let warning_codes = if skipped == 0 { vec![] } else { vec![format!("plugin-values-skipped:{skipped}")] };
            // The activation receipt names the authority the commit ran under, not
            // whatever the binding reads after the commit.
            let (final_revision, authority) = if !prepared_device.is_empty() {
                let context = context.ok_or_else(|| {
                    NativeJobError::new(
                        "device-maintenance-unavailable",
                        "Native device restore requires the application maintenance state",
                    )
                })?;
                let mut durable_session_started = false;
                let session = match begin_native_restore(
                    context,
                    selection,
                    &mut store,
                    revision,
                    Some(stage.staging_id.as_str()),
                    job,
                    &prepared_device,
                    &probe,
                    &mut durable_session_started,
                ) {
                    Ok(session) => session,
                    Err(failure) => {
                        if durable_session_started {
                            committed = true;
                            journal_owned = selection.library;
                        }
                        return Err(failure);
                    }
                };
                // From this point the existing durable device journal and the PDS
                // stage own recovery. A failed attempt resumes this accepted restore.
                committed = true;
                journal_owned = selection.library;
                let result = resume_journaled_native_restore(
                    context.coordinator,
                    &session,
                    &mut store,
                )
                .map_err(error)?;
                let header = context.coordinator.library_replacement_header(&session).map_err(error)?;
                (result, header.binding_authority)
            } else {
                let header = crate::persistent_store::lww::Header { binding_authority: store.lww_binding_authority().map_err(error)?, request_id: job.id() };
                let result = store.lww_commit_staged_replacement(&header,&stage.staging_id).map_err(error)?;
                committed = true;
                store.release_replacement_source(&stage.staging_id).map_err(error)?;
                (result.revision, header.binding_authority)
            };
            #[cfg(test)] after_library_commit(&mut store);
            job.publish_portable_activation(final_revision, authority.0.to_string()).map_err(error)?;
            job.wait_for_portable_adoption().map_err(error)?;
            // Device recovery leaves a journal this job still holds, so the job releases it itself.
            journal_owned = false;
            let retained=job.requires_restore_finalization && context.is_some();
            if retained {
                job.prepare_portable_body_retry(&stage.staging_id,&source_sha256,archive.source_identity_guard().map_err(error)?).map_err(error)?;
            }
            job.set_phase(JobPhase::CopyingMissingBodies).map_err(error)?;
            loop {
                let bodies=(|| {
                    if selection.library {
                        let inventory = portable_backup::RestoreInventory::build(&archive, owned, &probe).map_err(error)?;
                        install(&archive,&inventory,&mut store,&mut pins,&probe)?;
                        if let Some(report)=inventory.preserve(&archive,&store,&mut pins,&probe).map_err(error)? {
                            job.set_preservation_report(report).map_err(error)?;
                        }
                    }
                    pins.seal(&mut store,now()).map_err(error)?;
                    archive.check_identity().map_err(error)
                })();
                match bodies {
                    Ok(())=>break,
                    Err(failure) if retained=>{
                        if archive.check_identity().is_err() {
                            job.portable_body_source_required().map_err(error)?;
                            return Err(NativeJobError::new("portable-body-source-required","Portable library is restored, but the backup source changed"));
                        }
                        if !job.wait_for_portable_body_retry(&failure).map_err(error)? {
                            return Err(NativeJobError::new("portable-body-source-required","Portable library is restored, but the backup source is required"));
                        }
                    }
                    Err(failure)=>return Err(failure),
                }
            }
            if retained {job.portable_bodies_completed().map_err(error)?;}
            Ok(JobResultSummary {
                export_exclusions: None,
                revision: final_revision,
                source_bytes: source.total_bytes,
                source_sha256,
                source_fingerprint_kind: crate::native_file_jobs::SourceFingerprintKind::PortableCatalogSha256,
                character_count: if selection.library { counts.0 } else { 0 },
                preset_count: if selection.library { counts.1 } else { 0 },
                warning_codes,
                handoff_path: None,
                publication: None,
            })
        })();
        match activated {
            Err(mut failure) if !committed => {
                if let Err(cleanup) = store.replace_abort(&stage.staging_id) {
                    append_cleanup_failure(&mut failure, "staging abort", cleanup);
                }
                Err(failure)
            }
            outcome => outcome,
        }
    })();
    if journal_owned && outcome.is_err() {
        outcome
    } else {
        finish_pins(outcome, &mut pins)
    }
}

fn require_capacity(directory: &Path, bytes: u64) -> Result<(), NativeJobError> {
    if let Ok(available) = fs2::available_space(directory) {
        check_capacity(available, bytes)?;
    }
    Ok(())
}

fn check_capacity(available: u64, bytes: u64) -> Result<(), NativeJobError> {
    let required = bytes.checked_add(1024 * 1024).ok_or_else(|| NativeJobError::new("insufficient-storage", "Backup staging size exceeds available storage"))?;
    if available < required {
        return Err(NativeJobError::new("insufficient-storage", "Not enough storage to stage the backup"));
    }
    Ok(())
}

fn install(
    archive: &VerifiedArchive,
    inventory: &portable_backup::RestoreInventory,
    store: &mut PersistentStore,
    pins: &mut DurableCasJob,
    probe: &dyn CancellationProbe,
) -> Result<(), NativeJobError> {
    let cas = PayloadCas::new(store.repository_root()).map_err(error)?;
    let mut statement = inventory
        .db
        .prepare("SELECT hash,owner FROM live_objects ORDER BY hash")
        .map_err(error)?;
    let mut rows = statement.query([]).map_err(error)?;
    let mut batch = Vec::with_capacity(crate::persistent_store::portable::PRESENCE_BATCH);
    loop {
        batch.clear();
        while batch.len() < crate::persistent_store::portable::PRESENCE_BATCH {
            let Some(row) = rows.next().map_err(error)? else { break };
            let hash: String = row.get(0).map_err(error)?;
            let owner: bool = row.get(1).map_err(error)?;
            let hash_bytes=hex::decode(&hash).map_err(error)?;
            let size:i64=archive.db.query_row("SELECT byte_length FROM objects WHERE sha256=?1",[&hash_bytes],|row|row.get(0)).map_err(error)?;
            batch.push((hash, u64::try_from(size).map_err(error)?, owner));
        }
        if batch.is_empty() {
            return Ok(());
        }
        if probe.is_cancelled() {
            return Err(NativeJobError::new("cancelled", "Portable restore cancelled during CAS staging"));
        }
        let objects = batch.iter().map(|(hash, size, _)| (hash.as_str(), *size)).collect::<Vec<_>>();
        let present = store.portable_objects_present(&objects).map_err(error)?;
        for ((hash, _, owner), present) in batch.iter().zip(present) {
            if present { continue; }
            if probe.is_cancelled() {
                return Err(NativeJobError::new("cancelled", "Portable restore cancelled during CAS staging"));
            }
            let (input, size) = archive.open_object(hash).map_err(error)?;
            let mut input = CancelledRead { input, probe };
            pins.prepare_reader_expected(
                &cas,
                &mut input,
                hash,
                size,
                if *owner {
                    CasObjectRole::OwnerManifest
                } else {
                    CasObjectRole::DirectObject
                },
            )
            .map_err(error)?;
        }
    }
}
struct CancelledRead<'a> {
    input: portable_backup::ArchiveObjectReader,
    probe: &'a dyn CancellationProbe,
}
impl Read for CancelledRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.probe.is_cancelled() {
            return Err(std::io::Error::other("portable restore cancelled"));
        }
        let count = self.input.read(bytes)?;
        self.probe.backup_bytes_processed(count as u64);
        Ok(count)
    }
}

fn counts(archive: &VerifiedArchive) -> Result<(u64, u64), NativeJobError> {
    Ok((archive.db.query_row("SELECT count(*) FROM characters", [], |row| row.get::<_,i64>(0)).map_err(error)? as u64,
        archive.db.query_row("SELECT count(*) FROM bot_presets", [], |row| row.get::<_,i64>(0)).map_err(error)? as u64))
}

#[cfg(test)]
thread_local! {
    static AFTER_LIBRARY_COMMIT: std::cell::RefCell<Option<Box<dyn FnOnce(&mut PersistentStore)>>> = const { std::cell::RefCell::new(None) };
}

/// Runs the hook a test installed on the restore thread between the library commit and the activation receipt.
#[cfg(test)]
fn after_library_commit(store: &mut PersistentStore) {
    if let Some(hook) = AFTER_LIBRARY_COMMIT.with(|slot| slot.borrow_mut().take()) { hook(store); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_backup::NeverCancelled;
    use crate::persistent_store::portable::digest_raw_tables;
    use std::io::Write;

    fn restore_portable(source:OpenedJobSource,already_owned:bool,revision:i64,owned:&Path,store:PersistentStore,job:&JobControl,device:Option<(&tauri::AppHandle,Option<&PortableSelection>)>)->Result<JobResultSummary,NativeJobError> {
        let persistent=crate::persistent_store::commands::PersistentStoreState::default();
        let coordinator=DeviceBackupState::initialize(store.repository_root().join("device-backup"));
        let context=NativePortableRestoreContext {persistent:&persistent,coordinator:&coordinator};
        let selection=PortableSelection::default();
        restore_portable_with_context(source,already_owned,revision,owned,store,job,Some((&context,Some(device.and_then(|(_,selection)|selection).unwrap_or(&selection)))))
    }

    fn raw_recovery_archive(path: &Path) -> u64 {
        let file = File::create(path).unwrap();
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
        fs::metadata(path).unwrap().len()
    }

    #[test]
    fn portable_restore_rejects_a_renamed_raw_recovery_archive() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("renamed.risunest");
        let bytes = raw_recovery_archive(&source);
        let target = directory.path().join("target");
        let store = library(&target);
        let owned = directory.path().join("restore-job");
        fs::create_dir(&owned).unwrap();
        let job = super::super::JobRegistry::default()
            .create_internal(
                super::super::JobKind::RestorePortableBackup,
                Some(1),
                vec![],
                false,
            )
            .unwrap();

        let error = restore_portable(
            OpenedJobSource {
                file: File::open(source).unwrap(),
                custody: None,
                total_bytes: bytes,
            },
            true,
            1,
            &owned,
            store,
            &job,
            None,
        )
        .unwrap_err();

        assert_eq!(error.code, "rescue-format-not-restorable");
        let store = PersistentStore::open(&target).unwrap();
        assert_eq!(store.revision().unwrap(), 1);
    }

    fn library(root: &Path) -> PersistentStore {
        let mut store = PersistentStore::open(root).unwrap();
        let database: serde_json::Value =
            serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json")).unwrap();
        let stage = store.replace_begin().unwrap();
        let mut value = database.clone();
        value.as_object_mut().unwrap().remove("characters");
        value.as_object_mut().unwrap().remove("botPresets");
        store.replace_put_root(&stage.staging_id, &value).unwrap();
        store
            .replace_put_presets(
                &stage.staging_id,
                database["botPresets"].as_array().unwrap(),
            )
            .unwrap();
        store
            .replace_add_characters(
                &stage.staging_id,
                database["characters"].as_array().unwrap(),
            )
            .unwrap();
        store.replace_commit(&stage.staging_id, Some(0)).unwrap();
        store
    }

    #[test]
    fn cancelled_device_capture_returns_the_canonical_worker_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let owned = directory.path().join("job");
        fs::create_dir(&owned).unwrap();
        let store = library(&source);
        let revision = store.revision().unwrap();
        let registry = super::super::JobRegistry::default();
        let job = registry
            .create_internal(
                super::super::JobKind::ExportPortableBackup,
                Some(revision),
                vec![],
                false,
            )
            .unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        assert_eq!(
            job.request_cancel().unwrap(),
            super::super::CancelOutcome::Requested
        );

        let failure = export_portable_running(
            None,
            revision,
            &owned,
            &directory.path().join("handoffs"),
            store,
            &job,
            &PortableSelection::default(),
            "9.8.7-synthetic",
        )
        .unwrap_err();

        assert_eq!(failure.code, "cancelled");
        assert_eq!(failure.message, "Portable backup was cancelled");
        assert_eq!(job.status().state, super::super::JobState::Cancelling);
        super::super::finish_worker_outcome(
            &job,
            super::super::JobKind::ExportPortableBackup,
            Err(failure),
            vec![],
            None,
        );
        let status = job.status();
        assert_eq!(status.state, super::super::JobState::Cancelled);
        assert!(status.error.is_none());
        assert!(status.result.is_none());
        assert!(!owned.join("archive.risunest.part").exists());
        assert!(!directory.path().join("handoffs").exists());
    }

    #[test]
    fn non_cancellation_device_errors_keep_the_portable_failure_contract() {
        let failure = device_error(DeviceBackupError {
            code: "device-storage-failed".into(),
            message: "synthetic storage failure".into(),
            cause: None,
        });

        assert_eq!(failure.code, "portable-backup-failed");
        assert_eq!(
            failure.message,
            "device-storage-failed: synthetic storage failure"
        );
    }

    fn add_test_aliases(root: &Path, hash: &str, size: usize, count: usize) {
        let mut db = rusqlite::Connection::open(root.join("persistent/persistent.sqlite")).unwrap();
        let generation: String = serde_json::from_str(
            &db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='activeGeneration'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        )
        .unwrap();
        let tx = db.transaction().unwrap();
        for index in 0..count {
            tx.execute("INSERT INTO asset_aliases(generation,logical_key,object_hash,kind,size,mime,name,ext,inlay_type,width,height,metadata) VALUES(?1,?2,?3,'asset',?4,'application/octet-stream','','',NULL,NULL,NULL,'{}')",rusqlite::params![generation, format!("assets/synthetic-{index}.bin"),hash,size as i64]).unwrap();
        }
        tx.commit().unwrap();
    }

    fn test_archive(root: &Path, with_asset: bool) -> (std::path::PathBuf, u64, Option<String>) {
        let source = root.join("source");
        let jobs = root.join("export-job");
        fs::create_dir(&jobs).unwrap();
        let mut store = library(&source);
        let hash = with_asset.then(|| {
            let payload = b"synthetic incoming object";
            crate::server_sync::lww_tests::put_asset(&mut store, "assets/synthetic-0.bin", payload).object_hash.unwrap()
        });
        let revision = store.revision().unwrap();
        let job = super::super::JobRegistry::default()
            .create_internal(
                super::super::JobKind::ExportPortableBackup,
                Some(revision),
                vec![],
                false,
            )
            .unwrap();
        let result =
            export_portable(None, revision, &jobs, &root.join("handoffs"), store, &job, None, "9.8.7-synthetic").unwrap();
        (
            result.handoff_path.unwrap().into(),
            result.source_bytes,
            hash,
        )
    }

    #[test]
    fn portable_progress_reports_real_bytes_without_resetting_between_passes() {
        let job = super::super::JobRegistry::default().create(super::super::JobKind::ExportPortableBackup).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        let probe = Probe::for_export(&job);
        portable_backup::copy_hash(&mut &b"fixture"[..], &mut std::io::sink(), 7, &probe).unwrap();
        assert_eq!(job.status().progress.completed_items, 1);
        assert_eq!(job.status().progress.total_bytes, None);
        assert_eq!(job.status().detail.unwrap().stage_completed, 0);
        probe.backup_download_observer().unwrap()(5);
        portable_backup::copy_hash(&mut &b"fixture"[..], &mut std::io::sink(), 7, &probe).unwrap();
        let downloaded = job.status().detail.unwrap();
        assert_eq!(downloaded.stage, super::super::JobStage::DownloadingExport);
        assert_eq!(downloaded.stage_completed, 5);
        assert_eq!(downloaded.stage_total, None);
        probe.track_bytes(21).unwrap();
        for (stage, expected) in [(super::super::JobStage::WritingExport, 7), (super::super::JobStage::VerifyingExport, 14), (super::super::JobStage::PublishingDestination, 21)] {
            probe.export_stage(stage, None).unwrap();
            portable_backup::copy_hash(&mut &b"fixture"[..], &mut std::io::sink(), 7, &probe).unwrap();
            assert_eq!(job.status().progress.completed_bytes, expected);
            assert_eq!(job.status().progress.total_bytes, Some(21));
            assert_eq!(job.status().detail.unwrap().stage_completed, 7);
            probe.result().unwrap();
        }
    }

    fn portable_body_retry_case(retire: bool, source_changed: bool) {
        use super::super::{JobKind, JobState, NativeFileJobState, PortableBodyRetryRequest, WorkerPermit};
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().unwrap();
        let (path, bytes, hash) = test_archive(directory.path(), true);
        let hash = hash.unwrap();
        let target = directory.path().join("target");
        drop(library(&target));
        let retained_hash = PayloadCas::new(&target).unwrap().prepare_bytes(b"synthetic existing retained").unwrap().content_hash;
        let state = Arc::new(NativeFileJobState::initialize(target.join("native-file-jobs")));
        let coordinator = Arc::new(DeviceBackupState::initialize(target.join("device-backup")));
        portable_backup::source_io::reset_source_io();
        let (job, _admission) = state.create_portable_restore_fixture(1).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_job = Arc::downgrade(&job);
        let cancel_once = cancelled.clone();
        let cancel_hash = hash.clone();
        portable_backup::source_io::on_object_read(move |actual| {
            if actual == cancel_hash && !cancel_once.swap(true, Ordering::AcqRel) {
                assert_eq!(cancel_job.upgrade().unwrap().request_cancel().unwrap(), super::super::CancelOutcome::Requested);
            }
        });
        let owned = directory.path().join("restore-job");
        fs::create_dir(&owned).unwrap();
        let source = OpenedJobSource {file: File::open(&path).unwrap(), custody: None, total_bytes: bytes};
        let worker_job = job.clone();
        let worker_coordinator = coordinator.clone();
        let worker_target = target.clone();
        let permit = WorkerPermit::acquire(state.active_workers.clone(), state.max_concurrent_jobs).unwrap();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = permit;
            let persistent = crate::persistent_store::commands::PersistentStoreState::default();
            let context = NativePortableRestoreContext {persistent: &persistent, coordinator: &worker_coordinator};
            let result = restore_portable_with_context(source, true, 1, &owned, PersistentStore::open(&worker_target).unwrap(), &worker_job, Some((&context, None)));
            let receipt = result.as_ref().map(|result| result.revision).map_err(|error| error.code.clone());
            super::super::finish_worker_outcome(&worker_job, JobKind::RestorePortableBackup, result, vec![], None);
            done.send(receipt).unwrap();
        });
        struct Retire(Arc<NativeFileJobState>);
        impl Drop for Retire {fn drop(&mut self) {let _ = self.0.begin_cleanup();}}
        let _retire_on_failure = Retire(state.clone());
        let wait = |ready: &dyn Fn(&super::super::JobStatus) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let status = job.status();
                if ready(&status) {break status;}
                assert!(Instant::now() < deadline, "portable worker did not reach required phase: {status:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait(&|status| status.phase == JobPhase::AwaitingBackupSelection);
        state.select_portable_restore_fixture(&job.id(), PortableSelection::default()).unwrap();
        wait(&|status| status.phase == JobPhase::AwaitingActivation);
        state.finalize(&job.id(), Some(1)).unwrap();
        let activated = wait(&|status| status.activation_revision.is_some());
        let revision = activated.activation_revision.unwrap();
        let authority = activated.activation_authority.unwrap();
        let session = activated.device_session_id.unwrap();
        crate::device_backup::complete_native_recovery_command_for_test(&coordinator, &session).unwrap();
        state.confirm_portable_restore_adoption(&coordinator, &job.id(), &revision.to_string(), &authority, &session).unwrap();
        coordinator.cleanup(&session).unwrap();
        let failed = wait(&|status| status.portable_body_retry.as_ref().is_some_and(|receipt| receipt.available));
        assert_eq!(failed.state, JobState::Cancelled);
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!state.cleanup_drained());
        assert!(state.forget(&job.id()).is_err());
        let body = failed.portable_body_retry.unwrap();
        let request = PortableBodyRetryRequest {job_id: job.id(), staging_id: body.staging_id, catalog_sha256: body.catalog_sha256, activation_revision: revision.to_string(), binding_authority: authority, device_session_id: session};
        let mut wrong = request.clone();
        wrong.catalog_sha256 = "f".repeat(64);
        assert_eq!(state.retry_portable_restore_bodies(&wrong).unwrap_err().code, "invalid-activation-receipt");
        wrong = request.clone();
        wrong.activation_revision = (revision + 1).to_string();
        assert!(state.retry_portable_restore_bodies(&wrong).is_err());
        let db = rusqlite::Connection::open(target.join("persistent/persistent.sqlite")).unwrap();
        let generation: String = db.query_row("SELECT value FROM meta WHERE key='activeGeneration'", [], |row| row.get(0)).unwrap();
        db.execute("UPDATE meta SET value=?1 WHERE key='activeGeneration'", ["\"synthetic-other-generation\""]).unwrap();
        assert_eq!(state.retry_portable_restore_bodies(&request).unwrap_err().code, "invalid-activation-receipt");
        db.execute("UPDATE meta SET value=?1 WHERE key='activeGeneration'", [&generation]).unwrap();
        drop(db);
        if retire {
            if source_changed {
                let mut changed = fs::OpenOptions::new().append(true).open(&path).unwrap();
                changed.write_all(b"changed synthetic source").unwrap();
                changed.sync_all().unwrap();
                drop(changed);
                assert_eq!(state.retry_portable_restore_bodies(&request).unwrap_err().code, "portable-body-source-required");
            } else {state.begin_cleanup().unwrap();}
            assert_eq!(completion.recv_timeout(Duration::from_secs(30)).unwrap().unwrap_err(), "portable-body-source-required");
            worker.join().unwrap();
            assert_eq!(DurableCasJob::open(&target, &job.id()).err().map(|error| error.kind()), Some(std::io::ErrorKind::NotFound));
            assert!(state.cleanup_drained());
            state.close_for_cleanup().unwrap();
            let restarted = NativeFileJobState::initialize(target.join("native-file-jobs"));
            assert_eq!(restarted.retry_portable_restore_bodies(&request).unwrap_err().code, "portable-body-source-required");
        } else {
            state.retry_portable_restore_bodies(&request).unwrap();
            let completed = completion.recv_timeout(Duration::from_secs(30)).unwrap();
            assert!(completed.is_ok(), "body retry failed: {:?}", job.status());
            assert_eq!(completed.unwrap(), revision);
            worker.join().unwrap();
            let status = job.status();
            assert_eq!(status.state, JobState::Succeeded);
            assert!(!status.portable_body_retry.unwrap().pending);
            assert!(PayloadCas::new(&target).unwrap().stat_object(&hash).unwrap().is_some());
            assert!(state.cleanup_drained());
        }
        let store = PersistentStore::open(&target).unwrap();
        assert_eq!(store.revision().unwrap(), revision);
        assert_eq!(store.lww_binding_authority().unwrap().0.to_string(), request.binding_authority);
        assert!(PayloadCas::new(&target).unwrap().stat_object(&retained_hash).unwrap().is_some());
    }

    #[test]
    fn portable_body_retry_cancel_resumes_same_activated_owner() {portable_body_retry_case(false, false);}

    #[test]
    fn portable_body_retry_cleanup_releases_waiting_source_owner() {portable_body_retry_case(true, false);}

    #[test]
    fn portable_body_retry_changed_source_retires_without_reactivation() {portable_body_retry_case(true, true);}

    /// A library restore driven through its worker up to the activation receipt the renderer adopts.
    struct ActivatedPortableRestore {
        _directory: tempfile::TempDir,
        target: std::path::PathBuf,
        hash: String,
        state: std::sync::Arc<super::super::NativeFileJobState>,
        coordinator: std::sync::Arc<DeviceBackupState>,
        job: std::sync::Arc<JobControl>,
        _admission: std::sync::Arc<std::sync::Mutex<Option<super::super::admission::Permit>>>,
        worker: std::thread::JoinHandle<()>,
        completion: std::sync::mpsc::Receiver<Result<i64, String>>,
        revision: i64,
        authority: String,
        session: String,
    }

    fn wait_for_job(job: &JobControl, ready: impl Fn(&super::super::JobStatus) -> bool) -> super::super::JobStatus {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let status = job.status();
            if ready(&status) {break status;}
            assert!(std::time::Instant::now() < deadline, "portable worker did not reach required phase: {status:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn activate_portable_restore(after_commit: Option<Box<dyn FnOnce(&mut PersistentStore) + Send>>) -> ActivatedPortableRestore {
        use super::super::{JobKind, NativeFileJobState, WorkerPermit};
        use std::sync::{Arc, mpsc};
        let directory = tempfile::tempdir().unwrap();
        let (path, bytes, hash) = test_archive(directory.path(), true);
        let target = directory.path().join("target");
        drop(library(&target));
        let state = Arc::new(NativeFileJobState::initialize(target.join("native-file-jobs")));
        let coordinator = Arc::new(DeviceBackupState::initialize(target.join("device-backup")));
        portable_backup::source_io::reset_source_io();
        let (job, admission) = state.create_portable_restore_fixture(1).unwrap();
        let owned = directory.path().join("restore-job");
        fs::create_dir(&owned).unwrap();
        let source = OpenedJobSource {file: File::open(&path).unwrap(), custody: None, total_bytes: bytes};
        let worker_job = job.clone();
        let worker_coordinator = coordinator.clone();
        let worker_target = target.clone();
        let permit = WorkerPermit::acquire(state.active_workers.clone(), state.max_concurrent_jobs).unwrap();
        let (done, completion) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _permit = permit;
            if let Some(hook) = after_commit {
                super::AFTER_LIBRARY_COMMIT.with(|slot| *slot.borrow_mut() = Some(hook));
            }
            let persistent = crate::persistent_store::commands::PersistentStoreState::default();
            let context = NativePortableRestoreContext {persistent: &persistent, coordinator: &worker_coordinator};
            let result = restore_portable_with_context(source, true, 1, &owned, PersistentStore::open(&worker_target).unwrap(), &worker_job, Some((&context, None)));
            let receipt = result.as_ref().map(|result| result.revision).map_err(|error| error.code.clone());
            super::super::finish_worker_outcome(&worker_job, JobKind::RestorePortableBackup, result, vec![], None);
            done.send(receipt).unwrap();
        });
        wait_for_job(&job, |status| status.phase == JobPhase::AwaitingBackupSelection);
        state.select_portable_restore_fixture(&job.id(), PortableSelection::default()).unwrap();
        wait_for_job(&job, |status| status.phase == JobPhase::AwaitingActivation);
        state.finalize(&job.id(), Some(1)).unwrap();
        let activated = wait_for_job(&job, |status| status.activation_revision.is_some());
        assert!(PayloadCas::new(&target).unwrap().stat_object(hash.as_ref().unwrap()).unwrap().is_none());
        ActivatedPortableRestore {
            _directory: directory,
            target,
            hash: hash.unwrap(),
            state,
            coordinator,
            job,
            _admission: admission,
            worker,
            completion,
            revision: activated.activation_revision.unwrap(),
            authority: activated.activation_authority.unwrap(),
            session: activated.device_session_id.unwrap(),
        }
    }

    fn assert_restored_body_settled(target: &Path, job_id: &str, hash: &str, revision: i64) {
        assert!(PayloadCas::new(target).unwrap().stat_object(hash).unwrap().is_some());
        let catalog = rusqlite::Connection::open(target.join("persistent/persistent.sqlite")).unwrap();
        let registered: i64 = catalog.query_row("SELECT count(*) FROM asset_objects WHERE object_hash=?1", [hash], |row| row.get(0)).unwrap();
        assert_eq!(registered, 1);
        drop(catalog);
        assert_eq!(DurableCasJob::open(target, job_id).err().map(|error| error.kind()), Some(std::io::ErrorKind::NotFound));
        assert_eq!(PersistentStore::open(target).unwrap().revision().unwrap(), revision);
    }

    #[test]
    fn portable_restore_completes_device_recovery_before_its_missing_body_transfer() {
        let restore = activate_portable_restore(None);
        assert!(crate::asset_repository::job_pins::durable_cas_job_held(&restore.target, &restore.job.id()).unwrap());
        crate::device_backup::complete_native_recovery_command_for_test(&restore.coordinator, &restore.session).unwrap();
        assert!(!restore.coordinator.is_blocking().unwrap());
        restore.state.confirm_portable_restore_adoption(&restore.coordinator, &restore.job.id(), &restore.revision.to_string(), &restore.authority, &restore.session).unwrap();
        let completed = restore.completion.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        assert_eq!(completed, Ok(restore.revision), "{:?}", restore.job.status());
        restore.worker.join().unwrap();
        let status = restore.job.status();
        assert_eq!(status.state, super::super::JobState::Succeeded);
        assert!(status.restore_adoption_confirmed);
        assert_restored_body_settled(&restore.target, &restore.job.id(), &restore.hash, restore.revision);
        assert!(restore.state.cleanup_drained());
    }

    #[test]
    fn portable_restore_stopped_after_activation_completes_recovery_at_next_start() {
        let restore = activate_portable_restore(None);
        restore.state.begin_cleanup().unwrap();
        assert!(restore.completion.recv_timeout(std::time::Duration::from_secs(30)).unwrap().is_err());
        restore.worker.join().unwrap();
        let job_id = restore.job.id();
        let left = DurableCasJob::open(&restore.target, &job_id).unwrap();
        assert!(!left.is_sealed() && !left.is_released());
        drop(left);
        assert!(restore.coordinator.is_blocking().unwrap());
        drop(restore.coordinator);

        let restarted = DeviceBackupState::initialize(restore.target.join("device-backup"));
        restarted.attach_maintenance_guard(crate::persistent_store::commands::PersistentStoreState::default().acquire_device_maintenance().unwrap()).unwrap();
        let decision = restarted.bootstrap_for_entry().unwrap();
        let session = decision.session.unwrap();
        assert_eq!((decision.mode, session.phase.as_str(), session.action.as_str()), ("maintenance", "committed", "native-complete"));
        assert_eq!(session.session_id, restore.session);
        crate::device_backup::complete_native_recovery_command_for_test(&restarted, &session.session_id).unwrap();
        assert!(!restarted.is_blocking().unwrap());
        assert_eq!(restarted.bootstrap_for_entry().unwrap().mode, "normal");
        assert_eq!(DurableCasJob::open(&restore.target, &job_id).err().map(|error| error.kind()), Some(std::io::ErrorKind::NotFound));
        assert_eq!(PersistentStore::open(&restore.target).unwrap().revision().unwrap(), restore.revision);
    }

    #[test]
    fn portable_activation_receipt_names_the_authority_its_commit_used() {
        use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
        let committed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observed = committed.clone();
        let restore = activate_portable_restore(Some(Box::new(move |store: &mut PersistentStore| {
            let before = store.lww_binding_state().unwrap();
            *observed.lock().unwrap() = Some(before.target_authority.0.to_string());
            store.switch_lww_binding(&SwitchBindingRequest {
                initial_publication: false,
                header: crate::persistent_store::lww::Header { binding_authority: before.target_authority, request_id: "synthetic-switch-after-portable-commit".into() },
                expected_selection_epoch: before.selection_epoch, target: SyncTarget::None, inspection_id: None,
            }).unwrap();
        })));
        let committed = committed.lock().unwrap().clone().unwrap();
        let current = PersistentStore::open(&restore.target).unwrap().lww_binding_authority().unwrap().0.to_string();
        assert_ne!(current, committed, "the synthetic switch must change the binding authority");
        assert_eq!(restore.authority, committed);
        restore.state.begin_cleanup().unwrap();
        assert!(restore.completion.recv_timeout(std::time::Duration::from_secs(30)).unwrap().is_err());
        restore.worker.join().unwrap();
    }

    /// Exports a library whose one asset body is not on this device, held as `held` says.
    fn export_without_local_body(held: bool) -> (NativeJobError, bool) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let mut store = library(&source);
        let body = b"synthetic body kept elsewhere";
        let hash = crate::server_sync::lww_tests::put_asset(&mut store, "assets/synthetic-elsewhere.bin", body)
            .object_hash
            .unwrap();
        let cas = crate::asset_repository::PayloadCas::new(&source).unwrap();
        fs::remove_file(cas.object_path(&hash).unwrap().unwrap()).unwrap();
        if held {
            // Custody is recorded, but the server cannot be reached.
            crate::server_sync::residency::test_remote::hold(&source, &[(&hash, body.len() as u64)]);
        }
        let jobs = directory.path().join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let revision = store.revision().unwrap();
        let export = super::super::JobRegistry::default()
            .create_internal(super::super::JobKind::ExportPortableBackup, Some(revision), vec![], false)
            .unwrap();
        let destination = directory.path().join("backup.risunest");
        let failure = export_portable(Some(&destination), revision, &jobs, &directory.path().join("handoffs"), store, &export, None, "9.8.7-synthetic")
            .unwrap_err();
        (failure, destination.exists())
    }

    #[test]
    fn an_export_names_a_body_it_could_not_fetch_or_find_and_writes_nothing() {
        let (unfetched, written) = export_without_local_body(true);
        assert_eq!(unfetched.code, "remote-asset-unavailable");
        assert!(!written);
        let (missing, written) = export_without_local_body(false);
        assert_eq!(missing.code, "asset-missing");
        assert!(!written);
    }

    #[test]
    fn portable_disk_full_keeps_its_classification_through_wrappers() {
        let disk_full = || std::io::Error::from(std::io::ErrorKind::StorageFull);
        assert_eq!(error(disk_full()).code, "local-storage-full");
        assert_eq!(error(portable_backup::Error::Io(disk_full())).code, "local-storage-full");
        assert_eq!(error(Box::new(portable_backup::Error::Io(disk_full()))).code, "local-storage-full");
        assert_eq!(portable_error(portable_backup::Error::Zip(zip::result::ZipError::Io(disk_full()))).code, "local-storage-full");
        assert_eq!(error(Box::new(portable_backup::Error::Cancelled)).code, "cancelled");
        assert_eq!(portable_error(portable_backup::Error::Store(crate::persistent_store::StoreError::RevisionConflict { expected: 1, actual: 2 })).code, "revision-conflict");
    }

    #[test]
    fn portable_wrappers_keep_payloads_out_of_returned_jobs_and_log_sinks() {
        const PRIVATE: &str = "synthetic-private-payload";
        let shape = || serde_json::from_str::<u32>(&format!("\"{PRIVATE}\"")).unwrap_err();
        let failures = [
            portable_error(portable_backup::Error::Json(shape())),
            portable_error(portable_backup::Error::Sql(rusqlite::Error::FromSqlConversionFailure(
                3, rusqlite::types::Type::Text, Box::new(shape()),
            ))),
            error(Box::new(portable_backup::Error::Json(shape()))),
            portable_error(portable_backup::Error::Io(std::io::Error::other(PRIVATE))),
            portable_error(portable_backup::Error::Io(std::io::Error::other(shape()))),
            portable_error(portable_backup::Error::Zip(zip::result::ZipError::Io(std::io::Error::other(PRIVATE)))),
            portable_error(portable_backup::Error::Sql(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(1), Some(PRIVATE.to_owned()),
            ))),
        ];
        let directory = tempfile::tempdir().unwrap();
        let log = crate::native_log::NativeLogState::initialize(directory.path());
        log.set_file_enabled(true).unwrap();
        for failure in failures {
            assert_eq!(failure.code, "portable-backup-failed");
            assert!(!serde_json::to_string(&failure).unwrap().contains(PRIVATE));
            let job = super::super::JobRegistry::default()
                .create(super::super::JobKind::RestorePortableBackup).unwrap();
            job.start(JobPhase::ReadingSource).unwrap();
            super::super::finish_worker_outcome(&job, super::super::JobKind::RestorePortableBackup, Err(failure), vec![], None);
            let status = job.status();
            let returned = status.error.as_ref().unwrap();
            assert_eq!(returned.code, "portable-backup-failed");
            assert!(!returned.message.contains(PRIVATE));
            let entry = crate::native_log::global_state().tail(None).into_iter()
                .find(|entry| entry.target == "native-file-job" && entry.message.contains(&job.id())).unwrap();
            assert!(!entry.message.contains(PRIVATE));
            log.record(&entry.level, &entry.target, &entry.message);
        }
        assert!(log.tail(None).iter().all(|entry| !entry.message.contains(PRIVATE)));
        assert!(!fs::read_to_string(log.file_path()).unwrap().contains(PRIVATE));
        let mut wrapped = portable_backup::Error::Json(shape());
        assert!(!crate::native_log::failure_text(&&mut wrapped).contains(PRIVATE));
        assert!(wrapped.to_string().contains("json-shape at line 1 column"));
    }

    #[test]
    fn portable_export_writes_runtime_build_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let (path, _, _) = test_archive(directory.path(), false);
        let mut zip = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
        let format: serde_json::Value = serde_json::from_reader(zip.by_name("format.json").unwrap()).unwrap();
        assert_eq!(format["sourceAppBuild"], "9.8.7-synthetic");
        assert_eq!(format["formatVersion"], 1);
    }

    #[test]
    fn device_only_portable_export_is_refused_before_writing_a_partial_backup() {
        let directory = tempfile::tempdir().unwrap();
        let store = library(&directory.path().join("source"));
        let owned = directory.path().join("jobs");
        fs::create_dir_all(&owned).unwrap();
        let job = super::super::JobRegistry::default().create(super::super::JobKind::ExportPortableBackup).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        let selection = PortableSelection { library: false, device_sections: vec!["local-settings".into()], allow_source_preservation: false };
        let failure = export_portable_running(None, 1, &owned, &directory.path().join("handoffs"), store, &job, &selection, "9.8.7-device").unwrap_err();
        assert_eq!(failure.code,"invalid-full-backup-scope");
        assert!(!directory.path().join("handoffs").exists());
    }

    #[test]
    fn capacity_preflight_requires_copy_plus_margin_without_overflow() {
        assert!(check_capacity(1024 * 1024 + 7, 7).is_ok());
        assert_eq!(check_capacity(1024 * 1024 + 6, 7).unwrap_err().code, "insufficient-storage");
        assert_eq!(check_capacity(u64::MAX, u64::MAX).unwrap_err().code, "insufficient-storage");
    }

    #[test]
    fn restore_does_not_read_or_back_up_one_hundred_thousand_existing_asset_references() {
        let directory = tempfile::tempdir().unwrap();
        let (path, bytes, _) = test_archive(directory.path(), false);
        let target = directory.path().join("target");
        let store = library(&target);
        let payload = b"synthetic old payload stays untouched";
        let prepared = PayloadCas::new(&target)
            .unwrap()
            .prepare_bytes(payload)
            .unwrap();
        add_test_aliases(&target, &prepared.content_hash, payload.len(), 100_000);
        let object = target.join(&prepared.physical_key);
        #[cfg(windows)]
        let locked = {
            use std::os::windows::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&object)
                .unwrap()
        };
        let jobs = directory.path().join("restore-job");
        fs::create_dir(&jobs).unwrap();
        let job = super::super::JobRegistry::default()
            .create_internal(
                super::super::JobKind::RestorePortableBackup,
                Some(1),
                vec![],
                false,
            )
            .unwrap();
        let result = restore_portable(
            OpenedJobSource {
                file: File::open(path).unwrap(),
                custody: None,
                total_bytes: bytes,
            },
            true,
            1,
            &jobs,
            store,
            &job,
            None,
        )
        .unwrap();
        assert_eq!(result.revision, 2);
        #[cfg(windows)]
        drop(locked);
        assert_eq!(fs::read(&object).unwrap(), payload);
        assert!(!target.join("persistent/recovery").exists());
        let store = PersistentStore::open(&target).unwrap();
        assert!(store.snapshot_list().unwrap().is_empty());
        let db = rusqlite::Connection::open(target.join("persistent/persistent.sqlite")).unwrap();
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT count(*) FROM asset_aliases WHERE generation=(SELECT id FROM generations WHERE state='active')",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn restore_checks_present_bodies_in_metadata_batches_instead_of_per_object() {
        let directory = tempfile::tempdir().unwrap();
        let (path, bytes, hash) = test_archive(directory.path(), true);
        let hash = hash.unwrap();
        let target = directory.path().join("target");
        let store = library(&target);
        PayloadCas::new(&target).unwrap().prepare_bytes(b"synthetic incoming object").unwrap();
        let jobs = directory.path().join("restore-job");
        fs::create_dir(&jobs).unwrap();
        let job = super::super::JobRegistry::default()
            .create_internal(super::super::JobKind::RestorePortableBackup, Some(1), vec![], false)
            .unwrap();
        crate::asset_repository::body_io::reset_body_io();
        let result = restore_portable(
            OpenedJobSource { file: File::open(path).unwrap(), custody: None, total_bytes: bytes },
            true, 1, &jobs, store, &job, None,
        )
        .unwrap();
        let io = crate::asset_repository::body_io::take_body_io();
        assert_eq!(result.revision, 2);
        assert_eq!(io.stat_requests, 0);
        assert_eq!(io.presence_queries, 0);
        assert!(io.batch_stat_requests >= 1);
        assert_eq!(PayloadCas::new(&target).unwrap().stat_object(&hash).unwrap(), Some(b"synthetic incoming object".len() as u64));
    }

    #[test]
    fn failed_restore_preserves_old_database_and_existing_objects_without_recovery_archive() {
        for corrupt_existing in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let (path, bytes, hash) = test_archive(directory.path(), true);
            let hash = hash.unwrap();
            let target = directory.path().join("target");
            let store = library(&target);
            let observer = store.open_native_job_store().unwrap();
            let before = store.read_root(None).unwrap().value;
            let expected = PersistentStore::open(&directory.path().join("source")).unwrap().read_root(None).unwrap().value;
            let object = target
                .join("assets/objects")
                .join(&hash[..2])
                .join(&hash[2..]);
            fs::create_dir_all(object.parent().unwrap()).unwrap();
            let payload: &[u8] = if corrupt_existing {
                b"synthetic damaged object"
            } else {
                b"synthetic incoming object"
            };
            fs::write(&object, payload).unwrap();
            add_test_aliases(&target, &hash, payload.len(), 1);
            if !corrupt_existing {
                let db = rusqlite::Connection::open(target.join("persistent/persistent.sqlite"))
                    .unwrap();
                db.execute_batch("CREATE TRIGGER reject_replacement BEFORE UPDATE ON generations BEGIN SELECT RAISE(ABORT,'synthetic commit failure'); END;").unwrap();
            }
            let jobs = directory.path().join("restore-job");
            fs::create_dir(&jobs).unwrap();
            let job = super::super::JobRegistry::default()
                .create_internal(
                    super::super::JobKind::RestorePortableBackup,
                    Some(1),
                    vec![],
                    false,
                )
                .unwrap();
            let failure = restore_portable(
                OpenedJobSource {
                    file: File::open(path).unwrap(),
                    custody: None,
                    total_bytes: bytes,
                },
                true,
                1,
                &jobs,
                store,
                &job,
                None
            )
            .unwrap_err();
            assert_eq!(fs::read(&object).unwrap(), payload);
            if corrupt_existing {
                assert_eq!(failure.code, "portable-backup-failed");
                assert!(failure.message.contains("existing portable payload size mismatch"), "{}", failure.message);
                assert_eq!(observer.revision().unwrap(), 2);
                assert_eq!(job.status().activation_revision, Some(2));
                assert_eq!(observer.read_root(None).unwrap().value, expected);
                let db = rusqlite::Connection::open(target.join("persistent/persistent.sqlite")).unwrap();
                assert_eq!(db.query_row::<i64,_,_>("SELECT revision FROM lww_requests WHERE request_id=?1", [&job.id()], |row| row.get(0)).unwrap(), 2);
                assert_eq!(db.query_row::<String,_,_>("SELECT object_hash FROM asset_aliases WHERE generation=(SELECT id FROM generations WHERE state='active') AND logical_key='assets/synthetic-0.bin'", [], |row| row.get(0)).unwrap(), hash);
            } else {
                assert_eq!(failure.code, "portable-backup-failed");
                assert_eq!(failure.message, "device-storage-failed: Native backup section storage failed");
                assert_eq!(observer.revision().unwrap(), 1);
                assert_eq!(observer.read_root(None).unwrap().value, before);
                assert_eq!(job.status().activation_revision, None);
            }
            assert!(observer.snapshot_list().unwrap().is_empty());
            assert!(!target.join("persistent/recovery").exists());
        }
    }
    #[test]
    fn native_portable_stale_revision_aborts_without_changing_library() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let jobs = directory.path().join("jobs");
        let restore_jobs = directory.path().join("restore");
        fs::create_dir(&jobs).unwrap();
        fs::create_dir(&restore_jobs).unwrap();
        let store = library(&source);
        let revision = store.revision().unwrap();
        let registry = super::super::JobRegistry::default();
        let export = registry
            .create_internal(
                super::super::JobKind::ExportPortableBackup,
                Some(revision),
                vec![],
                false,
            )
            .unwrap();
        let exported = export_portable(
            None,
            revision,
            &jobs,
            &directory.path().join("handoffs"),
            store,
            &export,
            None, "9.8.7-synthetic")
        .unwrap();
        let target_store = library(&target);
        let revision = target_store.revision().unwrap();
        let before = target_store.read_root(None).unwrap().value;
        let before_roots = rusqlite::Connection::open(target.join("persistent/persistent.sqlite"))
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM root", [], |r| r.get(0))
            .unwrap();
        let restore = registry
            .create_internal(
                super::super::JobKind::RestorePortableBackup,
                Some(revision + 1),
                vec![],
                false,
            )
            .unwrap();
        let failure = restore_portable(
            OpenedJobSource {
                file: File::open(exported.handoff_path.unwrap()).unwrap(),
                custody: None,
                total_bytes: exported.source_bytes,
            },
            true,
            revision + 1,
            &restore_jobs,
            target_store,
            &restore,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.code, "revision-conflict");
        let db = rusqlite::Connection::open(target.join("persistent/persistent.sqlite")).unwrap();
        assert_eq!(
            db.query_row::<i64, _, _>("SELECT count(*) FROM root", [], |r| r.get(0))
                .unwrap(),
            before_roots
        );
        drop(db);
        // The refused restore left the target library and its revision as they were.
        let target_store = PersistentStore::open(&target).unwrap();
        assert_eq!(target_store.revision().unwrap(), revision);
        assert_eq!(target_store.read_root(None).unwrap().value, before);
    }
    #[test]
    fn native_portable_bad_json_is_preserved_raw_and_unknown_generation_uses_sqlite_salvage() {
        for unknown in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source");
            let jobs = directory.path().join("jobs");
            let handoffs = directory.path().join("handoffs");
            fs::create_dir(&jobs).unwrap();
            let store = library(&source);
            let revision = store.revision().unwrap();
            drop(store);
            let db =
                rusqlite::Connection::open(source.join("persistent/persistent.sqlite")).unwrap();
            if unknown {
                db.execute_batch("CREATE TABLE future_records(generation TEXT,value TEXT)").unwrap();
                db.execute("INSERT INTO future_records VALUES('synthetic',?1)",
                    [r#"{"account":{"id":"synthetic-account","token":"synthetic-token"}}"#]).unwrap();
            } else {
                db.execute("UPDATE messages SET value='not JSON'", [])
                    .unwrap();
            }
            drop(db);
            let mut refused_store = PersistentStore::open(&source).unwrap();
            let mut refused_pins = new_pins(&refused_store, CasJobKind::OfficialPublicationOrExportPreparation).unwrap();
            let denied = capture(&mut refused_store, revision, &jobs, &mut refused_pins,
                &NeverCancelled, "9.8.7-synthetic", false, None);
            assert!(matches!(denied, Err(ref failure) if failure.code == "source-preservation-confirmation-required"));
            refused_pins.release(CasReleaseOutcome::Aborted).unwrap();
            drop(refused_store);
            assert!(!handoffs.exists());
            let registry = super::super::JobRegistry::default();
            let job = registry
                .create_internal(
                    super::super::JobKind::ExportPortableBackup,
                    Some(revision),
                    vec![],
                    false,
                )
                .unwrap();
            job.start(JobPhase::WritingExport).unwrap();
            let exported = export_portable_running(
                None,
                revision,
                &jobs,
                &handoffs,
                PersistentStore::open(&source).unwrap(),
                &job,
                &PortableSelection { allow_source_preservation: true, ..PortableSelection::default() },
                "9.8.7-synthetic")
            .unwrap();
            assert_eq!(
                exported.warning_codes,
                vec!["source-preserved-repair-required"]
            );
            let archive = VerifiedArchive::open(
                File::open(exported.handoff_path.unwrap()).unwrap(),
                &jobs,
                &NeverCancelled,
            )
            .unwrap();
            assert!(archive.manifest.repair_required);
            assert!(archive.validate_library(&NeverCancelled).is_err());
            if archive.manifest.library_included {
                // The gate refuses this archive, so only the collecting scan can say what is wrong.
                let mut findings = crate::data_health::Findings::new(64);
                archive
                    .scan_library(&mut findings, &NeverCancelled)
                    .unwrap();
                assert!(findings
                    .items
                    .iter()
                    .any(|finding| finding.code == crate::data_health::codes::RECORD_INVALID));
            }
            if unknown {
                assert_eq!(
                    archive.manifest.profile,
                    portable_backup::Profile::SourceSqlite
                );
                assert!(archive.db.query_row::<bool,_,_>("SELECT EXISTS(SELECT 1 FROM files WHERE logical_key='source.sqlite' AND state='present')",[],|r|r.get(0)).unwrap());
                let hash: String = archive.db.query_row("SELECT lower(hex(object_hash)) FROM files WHERE logical_key='source.sqlite'", [], |row| row.get(0)).unwrap();
                let preserved_path = jobs.join("preserved-synthetic.sqlite");
                let mut preserved = File::create(&preserved_path).unwrap();
                archive.copy_object(&hash, &mut preserved, &NeverCancelled).unwrap();
                drop(preserved);
                let recovered = rusqlite::Connection::open(&preserved_path).unwrap();
                let account: String = recovered.query_row("SELECT value FROM future_records", [], |row| row.get(0)).unwrap();
                assert!(account == r#"{"account":{"id":"synthetic-account","token":"synthetic-token"}}"#);
            } else {
                assert_eq!(archive.manifest.profile, portable_backup::Profile::Portable);
                assert!(archive
                    .db
                    .query_row::<bool, _, _>(
                        "SELECT EXISTS(SELECT 1 FROM messages WHERE value='not JSON')",
                        [],
                        |r| r.get(0)
                    )
                    .unwrap());
            }
        }
    }
    fn portable_preserved_cas_case(present: bool) {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let jobs = directory.path().join("jobs");
        let restore_jobs = directory.path().join("restore");
        let handoffs = directory.path().join("handoffs");
        fs::create_dir_all(&jobs).unwrap();
        fs::create_dir_all(&restore_jobs).unwrap();
        let store = library(&source);
        let revision = store.revision().unwrap();
        let payload = b"synthetic unclassified original";
        let hash = hex::encode(Sha256::digest(payload));
        let physical_key = crate::asset_repository::object_physical_key(&hash);
        let source_object = source.join(&physical_key);
        fs::create_dir_all(source_object.parent().unwrap()).unwrap();
        fs::write(source_object, payload).unwrap();
        let registry = super::super::JobRegistry::default();
        let export = registry
            .create_internal(
                super::super::JobKind::ExportPortableBackup,
                Some(revision),
                vec![],
                false,
            )
            .unwrap();
        let exported =
            export_portable(None, revision, &jobs, &handoffs, store, &export, None, "9.8.7-synthetic").unwrap();
        let path = std::path::PathBuf::from(exported.handoff_path.unwrap());
        let expected_catalog=portable_backup::VerifiedArchive::open_for_restore(File::open(&path).unwrap(),&restore_jobs,&crate::local_backup::NeverCancelled).unwrap().manifest.catalog_sha256;
        let mut target_store = library(&target);
        if present {
            PayloadCas::new(&target).unwrap().prepare_bytes(payload).unwrap();
            let archive = VerifiedArchive::open_for_restore(File::open(&path).unwrap(), &restore_jobs, &NeverCancelled).unwrap();
            let inventory = portable_backup::RestoreInventory::build(&archive, &restore_jobs, &NeverCancelled).unwrap();
            let mut sealed = new_pins(&target_store, CasJobKind::LocalBackupRestore).unwrap();
            sealed.seal(&mut target_store, now()).unwrap();
            assert!(inventory.preserve(&archive, &target_store, &mut sealed, &NeverCancelled).is_err(), "sealing before preserved CAS registration loses pin admission");
            sealed.release(CasReleaseOutcome::Aborted).unwrap();
        }
        portable_backup::source_io::reset_source_io();
        let target_revision = target_store.revision().unwrap();
        crate::asset_repository::body_io::reset_body_io();
        let restore = registry
            .create_internal(
                super::super::JobKind::RestorePortableBackup,
                Some(target_revision),
                vec![],
                false,
            )
            .unwrap();
        let result = restore_portable(
            OpenedJobSource {
                file: File::open(&path).unwrap(),
                custody: None,
                total_bytes: exported.source_bytes,
            },
            true,
            target_revision,
            &restore_jobs,
            target_store,
            &restore,
            None,
        )
        .unwrap();
        assert_eq!(exported.source_fingerprint_kind,crate::native_file_jobs::SourceFingerprintKind::WholeFileSha256);
        assert_eq!(result.source_fingerprint_kind,crate::native_file_jobs::SourceFingerprintKind::PortableCatalogSha256);
        assert_eq!(result.source_sha256, expected_catalog);
        let report = restore.status().preservation_report.unwrap();
        assert_eq!(report.files, "1");
        assert_eq!(report.bytes, if present {"0".into()} else {payload.len().to_string()});
        assert!(report.deletable);
        if present {
            assert!(!Path::new(&report.path).join("objects").join(&hash).exists());
            let index = rusqlite::Connection::open(Path::new(&report.path).join("index.sqlite")).unwrap();
            assert_eq!(index.query_row::<String, _, _>("SELECT storage_kind FROM source_files WHERE object_hash=?1", [&hash], |row| row.get(0)).unwrap(), "cas");
            assert!(!portable_backup::source_io::take_source_io().objects.contains_key(&hash));
            if let Some(object) = crate::asset_repository::body_io::take_body_io().objects.get(&hash) {
                for work in [&object.work, &object.owned_work] {
                    assert_eq!((work.opens, work.read_bytes, work.staging_write_attempts, work.publication_attempts), (0, 0, 0, 0));
                    assert!(work.body_sha.is_empty());
                }
            }
        } else {
            assert_eq!(fs::read(Path::new(&report.path).join("objects").join(&hash)).unwrap(), payload);
        }
        assert_eq!(PayloadCas::new(&target).unwrap().stat_object(&hash).unwrap().is_some(), present);
        assert!(!target.join("persistent/recovery").exists());
        let mut target_store = PersistentStore::open(&target).unwrap();
        let mut pins = new_pins(
            &target_store,
            CasJobKind::OfficialPublicationOrExportPreparation,
        )
        .unwrap();
        let captured = capture(
            &mut target_store,
            result.revision,
            &jobs,
            &mut pins,
            &NeverCancelled,
            "9.8.7-synthetic",
            false,
            None,
        )
        .unwrap();
        let (key,stored_hash,metadata):(String,String,String)=captured.catalog.db.query_row("SELECT logical_key,lower(hex(object_hash)),metadata FROM files WHERE kind='preserved' AND logical_key LIKE 'source-preservation/%'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert!(key.starts_with("source-preservation/"));
        assert!(key.ends_with(&format!("/{physical_key}")));
        assert_eq!(stored_hash, hash);
        assert_eq!(metadata, "{\"storage\":\"cas\"}");
        pins.release(CasReleaseOutcome::Aborted).unwrap();
    }
    #[test]
    fn native_portable_restore_preserves_unregistered_cas_files_outside_live_cas() {portable_preserved_cas_case(false);}

    #[test]
    fn native_portable_present_unreferenced_cas_is_preserved_before_pin_seal() {portable_preserved_cas_case(true);}
    #[test]
    fn native_portable_export_restore_preserves_sql_without_recovery_backup() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let target = directory.path().join("target");
        let jobs = directory.path().join("jobs");
        let handoffs = directory.path().join("handoffs");
        fs::create_dir_all(&jobs).unwrap();
        let mut store = library(&source);
        let revision = store.revision().unwrap();
        let mut catalog = Catalog::create(&jobs, "synthetic", revision).unwrap();
        let lease = store.acquire_revision(revision).unwrap().lease;
        store
            .capture_portable_records(&lease, &mut catalog.db, &NeverCancelled)
            .unwrap();
        store.release_revision(&lease).unwrap();
        catalog.validate_library(&NeverCancelled).unwrap();
        let registry = super::super::JobRegistry::default();
        let export = registry
            .create_internal(
                super::super::JobKind::ExportPortableBackup,
                Some(revision),
                vec![],
                false,
            )
            .unwrap();
        let result =
            export_portable(None, revision, &jobs, &handoffs, store, &export, None, "9.8.7-synthetic").unwrap();
        assert!(result.warning_codes.is_empty());
        let path = std::path::PathBuf::from(result.handoff_path.unwrap());
        let before =
            VerifiedArchive::open(File::open(&path).unwrap(), &jobs, &NeverCancelled).unwrap();
        let expected = digest_raw_tables(&before.db, &NeverCancelled).unwrap();
        drop(before);
        let target_store = library(&target);
        let target_revision = target_store.revision().unwrap();
        let restore_jobs = directory.path().join("restore-job");
        fs::create_dir(&restore_jobs).unwrap();
        let restore = registry
            .create_internal(
                super::super::JobKind::RestorePortableBackup,
                Some(target_revision),
                vec![],
                false,
            )
            .unwrap();
        let bytes = fs::metadata(&path).unwrap().len();
        let restored = restore_portable(
            OpenedJobSource {
                file: File::open(path).unwrap(),
                custody: None,
                total_bytes: bytes,
            },
            true,
            target_revision,
            &restore_jobs,
            target_store,
            &restore,
            None,
        )
        .unwrap();
        assert_eq!(restored.revision, target_revision + 1);
        assert!(!target.join("persistent/recovery").exists());
        assert!(PersistentStore::open(&target)
            .unwrap()
            .snapshot_list()
            .unwrap()
            .is_empty());
        assert_eq!(PersistentStore::open(&target).unwrap().replacement_source_row_total().unwrap(), 0);
        let mut store = PersistentStore::open(&target).unwrap();
        let mut pins =
            new_pins(&store, CasJobKind::OfficialPublicationOrExportPreparation).unwrap();
        let captured = capture(
            &mut store,
            restored.revision,
            &jobs,
            &mut pins,
            &NeverCancelled,
            "9.8.7-synthetic",
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            expected,
            digest_raw_tables(&captured.catalog.db, &NeverCancelled).unwrap()
        );
        pins.release(CasReleaseOutcome::Aborted).unwrap();
    }

    fn export_and_restore(directory: &Path, store: PersistentStore) -> PersistentStore {
        let jobs = directory.join("jobs");
        let handoffs = directory.join("handoffs");
        fs::create_dir_all(&jobs).unwrap();
        let revision = store.revision().unwrap();
        let registry = super::super::JobRegistry::default();
        let export = registry
            .create_internal(super::super::JobKind::ExportPortableBackup, Some(revision), vec![], false)
            .unwrap();
        let result =
            export_portable(None, revision, &jobs, &handoffs, store, &export, None, "9.8.7-synthetic").unwrap();
        let path = std::path::PathBuf::from(result.handoff_path.unwrap());
        let target = directory.join("target");
        let target_store = library(&target);
        let target_revision = target_store.revision().unwrap();
        let restore_jobs = directory.join("restore-job");
        fs::create_dir(&restore_jobs).unwrap();
        let restore = registry
            .create_internal(super::super::JobKind::RestorePortableBackup, Some(target_revision), vec![], false)
            .unwrap();
        let bytes = fs::metadata(&path).unwrap().len();
        restore_portable(
            OpenedJobSource { file: File::open(path).unwrap(), custody: None, total_bytes: bytes },
            true,
            target_revision,
            &restore_jobs,
            target_store,
            &restore,
            None,
        )
        .unwrap();
        PersistentStore::open(&target).unwrap()
    }

    #[test]
    fn portable_export_restore_carries_a_large_unit_above_the_metadata_bound() {
        use crate::persistent_store::{lww::UnitMutation, WorkingSetCommit};
        let directory = tempfile::tempdir().unwrap();
        let mut store = library(&directory.path().join("source"));
        let large = serde_json::json!("x".repeat(risunest_sync_wire::MAX_METADATA_BYTES));
        store
            .commit(&WorkingSetCommit {
                expected_revision: store.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: risunest_sync_wire::unit::UnitKey::new(&["root", "additionalPrompt"]).unwrap(),
                    value: large.clone(),
                }]),
                ..Default::default()
            })
            .unwrap();
        let restored = export_and_restore(directory.path(), store);
        assert!(restored.read_root(None).unwrap().value["additionalPrompt"] == large);
    }

    #[test]
    fn portable_export_restore_carries_an_indivisible_message_above_the_metadata_bound() {
        use crate::persistent_store::WorkingSetCommit;
        let directory = tempfile::tempdir().unwrap();
        let mut store = library(&directory.path().join("source"));
        let data = "x".repeat(risunest_sync_wire::MAX_METADATA_BYTES);
        store
            .commit(&WorkingSetCommit {
                expected_revision: store.revision().unwrap(),
                add_character: Some(serde_json::json!({
                    "type": "character", "chaId": "synthetic-large", "name": "Large",
                    "chats": [{"id": "synthetic-large-chat", "name": "Large", "message": [{"role": "user", "data": data}]}],
                })),
                ..Default::default()
            })
            .unwrap();
        let restored = export_and_restore(directory.path(), store);
        let database = restored.materialize(None).unwrap();
        let character = database["characters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|character| character["chaId"] == "synthetic-large")
            .unwrap();
        assert!(character["chats"][0]["message"][0]["data"] == data.as_str());
    }

    #[test]
    fn portable_export_restore_carries_large_message_and_archive_controls() {
        use crate::persistent_store::{archive::ArchivedObject, lww::{ApplyReceive, Change, Header, Progress, StageReceive}, portable_identity::IdentityRemap};
        use risunest_external_storage_format::message_pages::{MessageHash, MessageManifest, ManifestPage, MANIFEST_SCHEMA, MIN_PAGE_MESSAGES};
        use risunest_sync_wire::{descriptor::RecordDescriptor, stamp::Stamp, unit::{UnitKey, UnitValue}};
        let directory = tempfile::tempdir().unwrap();
        let mut store = library(&directory.path().join("source"));
        store.archive_character("char-a", store.revision().unwrap(), 10).unwrap();
        let lease = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap().0.lease;
        let units = store.lww_backup_unit_values(&lease).unwrap();
        store.release_revision(&lease).unwrap();
        let UnitValue::Object { descriptor, .. } = &units[&UnitKey::new(&["archive", "char-a"]).unwrap()] else { panic!("archive control") };
        let mut archived: ArchivedObject = serde_json::from_slice(&store.lww_object_body(&descriptor.object_hash).unwrap().unwrap()).unwrap();
        archived.identity_remap = vec![IdentityRemap::default(); 35_000];
        let archive = risunest_sync_wire::payload_value::encode(&serde_json::to_value(archived).unwrap()).unwrap();
        let archive_hash = risunest_sync_wire::hash(&archive);
        let mut archive_descriptor = descriptor.clone();
        archive_descriptor.object_hash = archive_hash.clone();
        let message = (0..10_000).map(|index| serde_json::json!({"role":"user","data":format!("synthetic-{index}")}))
            .find(|message| MessageHash::from_bytes(&risunest_sync_wire::payload_value::encode(message).unwrap()).boundary().unwrap()).unwrap();
        let page = risunest_external_storage_format::logical_records::encode_message_page(&vec![message; MIN_PAGE_MESSAGES]).unwrap();
        let large_page = risunest_external_storage_format::logical_records::encode_message_page(&[serde_json::json!({"role":"user","data":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)})]).unwrap();
        let mut pages = vec![ManifestPage { hash: page.hash.clone(), message_count: MIN_PAGE_MESSAGES as u32, byte_length: page.size.into() }; 9500];
        pages[0] = ManifestPage { hash: large_page.hash.clone(), message_count: 1, byte_length: large_page.size.into() };
        let manifest = MessageManifest { schema: MANIFEST_SCHEMA.into(), message_count: (1 + 9499 * MIN_PAGE_MESSAGES as u64).into(),
            pages }.encode().unwrap();
        assert!(archive.len() > risunest_sync_wire::MAX_METADATA_BYTES);
        assert!(manifest.bytes.len() > risunest_sync_wire::MAX_METADATA_BYTES);
        eprintln!("synthetic portable controls: archive={} manifest={} large_page={} pages=9500 messages={}",
            archive.len(), manifest.bytes.len(), large_page.bytes.len(), 1 + 9499 * MIN_PAGE_MESSAGES);
        for (hash, body) in [(&archive_hash, &archive), (&manifest.hash, &manifest.bytes), (&page.hash, &page.bytes), (&large_page.hash, &large_page.bytes)] {
            store.lww_put_object(hash, body).unwrap();
        }
        let mut manifest_descriptor = RecordDescriptor::content(manifest.hash.clone());
        manifest_descriptor.dependencies = vec![page.hash.clone(), large_page.hash.clone()];
        manifest_descriptor.dependencies.sort();
        let originals = std::collections::BTreeMap::from([
            (UnitKey::new(&["archive", "held-archive"]).unwrap(), UnitValue::object(archive_descriptor).unwrap()),
            (UnitKey::new(&["messages", "held-messages", "held-chat"]).unwrap(), UnitValue::object(manifest_descriptor).unwrap()),
        ]);
        let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: "large-portable-controls".into() };
        store.lww_stage_receive(&StageReceive {
            header: header.clone(),
            changes: originals.iter().map(|(key, value)| Change { key: key.clone(), value: value.clone(),
                stamp: Stamp { physical_ms: 1u64.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000001".into() } }).collect(),
            progress: Progress { kind: "server".into(), cursor: 1u64.into(), writer_id: None }, admitted_time_upper_ms: u64::MAX.into(),
        }).unwrap();
        store.lww_apply_receive(&ApplyReceive { header: header.clone(), generating: Vec::new() }).unwrap();
        store.lww_finish_receive(&header).unwrap();
        let mut restored = export_and_restore(directory.path(), store);
        let lease = restored.lww_acquire_backup_capture(restored.revision().unwrap()).unwrap().0.lease;
        let restored_units = restored.lww_backup_unit_values(&lease).unwrap();
        for (key, value) in originals { assert_eq!(restored_units.get(&key), Some(&value)); }
        assert_eq!(restored.lww_object_body(&archive_hash).unwrap().unwrap(), archive);
        assert_eq!(restored.lww_object_body(&manifest.hash).unwrap().unwrap(), manifest.bytes);
        restored.release_revision(&lease).unwrap();
        restored.restore_character("char-a", restored.revision().unwrap()).unwrap();
        assert!(restored.read_character("char-a", None).unwrap().is_some());
    }

    #[test]
    fn a_library_with_archived_characters_exports_without_preservation_and_restores_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let mut store = library(&source);
        crate::persistent_store::tests::data_health_tests::add_character_with_additional_asset(&mut store, "char-owner");
        let character = |database: &serde_json::Value, id: &str| {
            database["characters"].as_array().unwrap().iter().find(|character| character["chaId"] == id).cloned()
        };
        let before = store.materialize(None).unwrap();
        for id in ["char-a", "char-owner"] {
            let revision = store.revision().unwrap();
            store.archive_character(id, revision, 10).unwrap();
        }
        let jobs = directory.path().join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let revision = store.revision().unwrap();
        let export = super::super::JobRegistry::default()
            .create_internal(super::super::JobKind::ExportPortableBackup, Some(revision), vec![], false)
            .unwrap();
        let result = export_portable(None, revision, &jobs, &directory.path().join("handoffs"), store, &export, None, "9.8.7-synthetic")
            .expect("a library with archived characters exports without a preservation prompt");
        assert!(result.warning_codes.is_empty(), "{:?}", result.warning_codes);
        let path = std::path::PathBuf::from(result.handoff_path.unwrap());
        let archive = VerifiedArchive::open(File::open(&path).unwrap(), &jobs, &NeverCancelled).unwrap();
        let archived: i64 = archive
            .db
            .query_row("SELECT count(*) FROM characters WHERE archived_object IS NOT NULL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(archived, 2);
        drop(archive);

        let target = directory.path().join("target");
        let target_store = library(&target);
        let target_revision = target_store.revision().unwrap();
        let restore_jobs = directory.path().join("restore-job");
        fs::create_dir(&restore_jobs).unwrap();
        let restore = super::super::JobRegistry::default()
            .create_internal(super::super::JobKind::RestorePortableBackup, Some(target_revision), vec![], false)
            .unwrap();
        let bytes = fs::metadata(&path).unwrap().len();
        restore_portable(
            OpenedJobSource { file: File::open(path).unwrap(), custody: None, total_bytes: bytes },
            true,
            target_revision,
            &restore_jobs,
            target_store,
            &restore,
            None,
        )
        .unwrap();
        let mut restored = PersistentStore::open(&target).unwrap();
        for id in ["char-a", "char-owner"] {
            let revision = restored.revision().unwrap();
            restored.restore_character(id, revision).unwrap();
        }
        let after = restored.materialize(None).unwrap();
        for id in ["char-a", "char-b", "char-c", "char-owner"] {
            assert_eq!(character(&after, id), character(&before, id), "{id}");
        }
    }

    #[test]
    fn portable_backup_manifest_reads_do_not_grow_with_oversized_pages() {
        use crate::persistent_store::{hash_work, WorkingSetCommit};
        let data = "x".repeat(risunest_sync_wire::MAX_METADATA_BYTES);
        let manifest_reads = |messages: usize| {
            let directory = tempfile::tempdir().unwrap();
            let mut store = library(&directory.path().join("source"));
            let message = (0..messages)
                .map(|index| serde_json::json!({"role": "user", "data": format!("{index} {data}")}))
                .collect::<Vec<_>>();
            store
                .commit(&WorkingSetCommit {
                    expected_revision: store.revision().unwrap(),
                    add_character: Some(serde_json::json!({
                        "type": "character", "chaId": "synthetic-large", "name": "Large",
                        "chats": [{"id": "synthetic-large-chat", "name": "Large", "message": message}],
                    })),
                    ..Default::default()
                })
                .unwrap();
            hash_work::reset_hash_work();
            let restored = export_and_restore(directory.path(), store);
            let reads = hash_work::take_hash_work().domains["native_backup_manifest_source"].calls;
            let database = restored.materialize(None).unwrap();
            let character = database["characters"]
                .as_array()
                .unwrap()
                .iter()
                .find(|character| character["chaId"] == "synthetic-large")
                .unwrap();
            let restored_messages = character["chats"][0]["message"].as_array().unwrap();
            assert_eq!(restored_messages.len(), messages);
            assert!(restored_messages[messages - 1]["data"] == format!("{} {data}", messages - 1).as_str());
            reads
        };
        // Each oversized page lookup reuses the manifests read for the first.
        let one = manifest_reads(1);
        assert!(one > 0);
        assert_eq!(manifest_reads(3), one);
    }

    #[test]
    fn a_page_reload_during_an_export_keeps_its_journal_until_the_export_finishes() {
        use crate::asset_repository::commands::{CasJobOwnerProbe, DurableCasJobState};
        use crate::asset_repository::job_pins::durable_cas_job_ids;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let jobs = directory.path().join("jobs");
        fs::create_dir_all(&jobs).unwrap();
        let mut store = library(&source);
        crate::server_sync::lww_tests::put_asset(&mut store, "assets/synthetic-export.bin", b"synthetic export object");
        let revision = store.revision().unwrap();
        let swept = std::sync::Arc::new(std::sync::Mutex::new(None));
        portable_backup::source_io::reset_source_io();
        {
            let root = source.clone();
            let swept = swept.clone();
            portable_backup::source_io::on_capture_ready(move |_, _| {
                let before = durable_cas_job_ids(&root).unwrap();
                let native_jobs = || -> Result<Vec<super::super::JobStatus>, String> { Ok(Vec::new()) };
                let open_store = || PersistentStore::open(&root).map_err(|error| error.to_string());
                DurableCasJobState::default()
                    .sweep_after_page_start(&root, &CasJobOwnerProbe {
                        native_jobs: &native_jobs,
                        device_job_owned: &|_| Ok(false),
                        external_job_active: &|_| Ok(false),
                        open_store: &open_store,
                    })
                    .unwrap();
                *swept.lock().unwrap() = Some((before, durable_cas_job_ids(&root).unwrap()));
            });
        }
        let export = super::super::JobRegistry::default()
            .create_internal(super::super::JobKind::ExportPortableBackup, Some(revision), vec![], false)
            .unwrap();
        let exported = export_portable(None, revision, &jobs, &directory.path().join("handoffs"), store, &export, None, "9.8.7-synthetic");
        portable_backup::source_io::reset_source_io();
        exported.unwrap();
        let (before, after) = swept.lock().unwrap().take().unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(after, before);
        assert!(durable_cas_job_ids(&source).unwrap().is_empty());
    }
}
