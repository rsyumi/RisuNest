//! Diagnosis commands. A scan reads the leased generation through its own connection, so the
//! library stays writable while it runs, and it writes its result to the working folder so the
//! screen can show the last diagnosis and a deep scan can resume after a restart.

use super::{
    current_time_ms, with_store_mutex_admitted, with_store_mutex_mut_admitted,
    PersistentStoreState, RendererOperationGuard,
};
use crate::data_health::journal;
use crate::data_health::repair::{self, RepairCandidate, RepairPreview};
use crate::data_health::{read_result, write_result, DeepProgress, ScanDepth, ScanResult};
use crate::local_backup::CancellationProbe;
use crate::persistent_store::{DataHealthReader, StoreError, StoreResult};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;

/// How many findings one diagnosis keeps. Past this the scan counts what it drops, so a library
/// damaged everywhere cannot exhaust memory through its own report.
const FINDING_LIMIT: usize = 2000;
/// Bytes one deep page rereads before it returns. The renderer shows the progress it reports and
/// decides whether to continue, so a cancelled deep scan stops within one page.
const DEEP_PAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Holds the stop request for a running scan. The scan runs without the store mutex, so the
/// cancel command reaches it while it is still reading.
#[derive(Default)]
pub(crate) struct DataHealthState {
    cancelled: Arc<AtomicBool>,
}

struct Cancellation(Arc<AtomicBool>);

impl CancellationProbe for Cancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl DataHealthState {
    fn begin(&self) -> Cancellation {
        self.cancelled.store(false, Ordering::SeqCst);
        Cancellation(Arc::clone(&self.cancelled))
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Everything a scan needs after the store mutex is released: the reader, the lease to give back
/// and the folder the result belongs in.
struct Session {
    reader: DataHealthReader,
    lease: String,
    working_root: PathBuf,
}

fn open_session(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    revision: Option<i64>,
) -> StoreResult<Session> {
    let (lease, working_root) = with_store_mutex_mut_admitted(state, operation_guard, |store| {
        let revision = match revision {
            Some(revision) => revision,
            None => store.revision()?,
        };
        let working_root = store.repository_root().to_owned();
        Ok((store.acquire_revision(revision)?.lease, working_root))
    })?;
    match with_store_mutex_admitted(state, operation_guard, |store| {
        store.data_health_reader(&lease)
    }) {
        Ok(reader) => Ok(Session {
            reader,
            lease,
            working_root,
        }),
        Err(error) => {
            release(state, operation_guard, &lease);
            Err(error)
        }
    }
}

fn release(state: &PersistentStoreState, operation_guard: &RendererOperationGuard, lease: &str) {
    let _ = with_store_mutex_mut_admitted(state, operation_guard, |store| {
        store.release_revision(lease)
    });
}

fn persist(working_root: &Path, result: &ScanResult) -> StoreResult<()> {
    write_result(working_root, result).map_err(|error| StoreError::Store {
        message: format!("failed to write the data health result: {error}"),
    })
}

fn working_root(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> StoreResult<PathBuf> {
    with_store_mutex_admitted(state, operation_guard, |store| {
        Ok(store.repository_root().to_owned())
    })
}

fn stored_result(working_root: &Path) -> StoreResult<Option<ScanResult>> {
    read_result(working_root).map_err(|error| StoreError::Store {
        message: format!("failed to read the data health result: {error}"),
    })
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_scan(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
) -> Result<ScanResult, StoreError> {
    quick_scan(&state, &health)
}

fn quick_scan(
    state: &PersistentStoreState,
    health: &DataHealthState,
) -> StoreResult<ScanResult> {
    let operation_guard = state.admit_renderer_operation()?;
    let probe = health.begin();
    let session = open_session(state, &operation_guard, None)?;
    let outcome = scan_quick(&session, &probe);
    release(state, &operation_guard, &session.lease);
    let result = outcome?;
    persist(&session.working_root, &result)?;
    Ok(result)
}

fn scan_quick(session: &Session, probe: &dyn CancellationProbe) -> StoreResult<ScanResult> {
    let findings = session.reader.scan(FINDING_LIMIT, probe)?;
    Ok(ScanResult::new(
        session.reader.revision(),
        current_time_ms()?,
        ScanDepth::Quick,
        findings,
    ))
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_deep_scan(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
    resume: bool,
) -> Result<ScanResult, StoreError> {
    deep_scan(&state, &health, resume)
}

fn deep_scan(
    state: &PersistentStoreState,
    health: &DataHealthState,
    resume: bool,
) -> StoreResult<ScanResult> {
    let operation_guard = state.admit_renderer_operation()?;
    let probe = health.begin();
    let carried = match resume {
        true => resumable(state, &operation_guard)?,
        false => None,
    };
    let session = open_session(
        state,
        &operation_guard,
        carried.as_ref().map(|result| result.revision),
    )?;
    let outcome = match carried {
        Some(carried) => scan_deep_page(&session, carried, &probe),
        None => scan_deep_first(&session, &probe),
    };
    release(state, &operation_guard, &session.lease);
    let result = outcome?;
    persist(&session.working_root, &result)?;
    Ok(result)
}

/// The persisted deep scan a resume continues, or nothing when the last result cannot be
/// continued. Starting over is always allowed; only the continuation needs a match.
fn resumable(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> StoreResult<Option<ScanResult>> {
    let stored = stored_result(&working_root(state, operation_guard)?)?;
    Ok(stored.filter(|result| {
        result.depth == ScanDepth::Deep && result.deep.as_ref().is_some_and(|deep| !deep.complete)
    }))
}

/// The first step of a deep scan is the library itself. The object pass that follows is what
/// the pages continue.
fn scan_deep_first(session: &Session, probe: &dyn CancellationProbe) -> StoreResult<ScanResult> {
    let findings = session.reader.scan(FINDING_LIMIT, probe)?;
    let totals = session.reader.object_totals()?;
    let mut result = ScanResult::new(
        session.reader.revision(),
        current_time_ms()?,
        ScanDepth::Deep,
        findings,
    );
    result.deep = Some(DeepProgress {
        cursor: None,
        completed_objects: 0,
        total_objects: totals.objects,
        completed_bytes: 0,
        total_bytes: totals.bytes,
        complete: totals.objects == 0,
    });
    Ok(result)
}

fn scan_deep_page(
    session: &Session,
    mut carried: ScanResult,
    probe: &dyn CancellationProbe,
) -> StoreResult<ScanResult> {
    let mut progress = carried.deep.take().ok_or_else(|| StoreError::Validation {
        message: "the stored diagnosis has no deep scan to resume".to_owned(),
    })?;
    let (page, findings) = session.reader.scan_objects(
        progress.cursor.as_deref(),
        DEEP_PAGE_BYTES,
        carried.remaining(FINDING_LIMIT),
        probe,
    )?;
    carried.absorb(findings);
    progress.cursor = page.cursor.or(progress.cursor);
    progress.completed_objects += page.objects;
    progress.completed_bytes += page.bytes;
    progress.complete = page.done;
    carried.scanned_at = current_time_ms()?;
    carried.deep = Some(progress);
    Ok(carried)
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_result(
    state: State<'_, PersistentStoreState>,
) -> Result<Option<ScanResult>, StoreError> {
    last_result(&state)
}

fn last_result(state: &PersistentStoreState) -> StoreResult<Option<ScanResult>> {
    let operation_guard = state.admit_renderer_operation()?;
    stored_result(&working_root(state, &operation_guard)?)
}

/// Asks a running scan to stop. The scan raises [`crate::data_health::CANCELLED`], and the
/// renderer that requested the stop recognises it rather than reporting a failure.
#[tauri::command(async)]
pub(crate) fn pds_data_health_cancel(health: State<'_, DataHealthState>) -> Result<(), StoreError> {
    health.cancel();
    Ok(())
}

/// The diagnosis a repair is selected against. A repair only applies to the library the scan
/// judged, so a diagnosis for another revision is refused instead of applied to the wrong rows.
fn current_diagnosis(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> StoreResult<(PathBuf, ScanResult)> {
    let root = working_root(state, operation_guard)?;
    let result = stored_result(&root)?.ok_or_else(|| StoreError::Validation {
        message: "check the data before repairing it".to_owned(),
    })?;
    let revision = with_store_mutex_admitted(state, operation_guard, |store| store.revision())?;
    if revision != result.revision {
        return Err(StoreError::RevisionConflict {
            expected: result.revision,
            actual: revision,
        });
    }
    Ok((root, result))
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_repair_plan(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<RepairCandidate>, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    Ok(repair::plan(&current_diagnosis(&state, &operation_guard)?.1))
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_repair_preview(
    state: State<'_, PersistentStoreState>,
    selection: Vec<String>,
) -> Result<RepairPreview, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    Ok(repair::preview(
        &current_diagnosis(&state, &operation_guard)?.1,
        &selection,
    ))
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_repair_apply(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
    selection: Vec<String>,
    snapshot: bool,
) -> Result<RepairApplied, StoreError> {
    apply_repair(&state, &health, &selection, snapshot)
}

fn apply_repair(
    state: &PersistentStoreState,
    health: &DataHealthState,
    selection: &[String],
    snapshot: bool,
) -> StoreResult<RepairApplied> {
    let operation_guard = state.admit_renderer_operation()?;
    let (root, diagnosis) = current_diagnosis(state, &operation_guard)?;
    let preview = repair::preview(&diagnosis, selection);
    if preview.selected.is_empty() {
        return Err(StoreError::Validation {
            message: "select at least one change to repair".to_owned(),
        });
    }
    let now = current_time_ms()?;
    // The snapshot is kept before anything changes, so it holds the library the reader selected
    // against rather than the repaired one.
    let snapshot = match snapshot {
        true => Some(
            with_store_mutex_mut_admitted(state, &operation_guard, |store| {
                store.snapshot_create("data-health-repair")
            })?
            .id,
        ),
        false => None,
    };
    let (revision, journal) = with_store_mutex_mut_admitted(state, &operation_guard, |store| {
        store.apply_repair(diagnosis.revision, &preview.selected, now)
    })?;
    journal::write(&root, &journal).map_err(|error| StoreError::Store {
        message: format!("failed to write the repair journal: {error}"),
    })?;
    drop(operation_guard);
    Ok(RepairApplied {
        revision: revision.revision,
        journal_id: journal.id,
        snapshot,
        result: quick_scan(state, health)?,
    })
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_journals(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<JournalSummary>, StoreError> {
    journals(&state)
}

fn journals(state: &PersistentStoreState) -> StoreResult<Vec<JournalSummary>> {
    let operation_guard = state.admit_renderer_operation()?;
    let root = working_root(state, &operation_guard)?;
    let revision = with_store_mutex_admitted(state, &operation_guard, |store| store.revision())?;
    Ok(journal::list(&root)
        .map_err(|error| StoreError::Store {
            message: format!("failed to read the repair journals: {error}"),
        })?
        .into_iter()
        .map(|entry| JournalSummary {
            id: entry.id,
            created_at: entry.created_at,
            from_revision: entry.from_revision,
            to_revision: entry.to_revision,
            changes: entry.records.len(),
            held_objects: entry.released_objects.len(),
            current: entry.to_revision == revision,
        })
        .collect())
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_undo(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
    journal_id: String,
) -> Result<RepairUndone, StoreError> {
    undo_repair(&state, &health, &journal_id)
}

fn undo_repair(
    state: &PersistentStoreState,
    health: &DataHealthState,
    journal_id: &str,
) -> StoreResult<RepairUndone> {
    let operation_guard = state.admit_renderer_operation()?;
    let root = working_root(state, &operation_guard)?;
    let entry = journal::read(&root, journal_id)
        .map_err(|error| StoreError::Store {
            message: format!("failed to read the repair journal: {error}"),
        })?
        .ok_or_else(|| StoreError::Validation {
            message: "that repair is no longer kept".to_owned(),
        })?;
    let revision = with_store_mutex_admitted(state, &operation_guard, |store| store.revision())?;
    let (committed, skipped) = with_store_mutex_mut_admitted(state, &operation_guard, |store| {
        store.undo_repair(&entry, revision)
    })?;
    journal::remove(&root, journal_id).map_err(|error| StoreError::Store {
        message: format!("failed to drop the repair journal: {error}"),
    })?;
    drop(operation_guard);
    Ok(RepairUndone {
        revision: committed.revision,
        skipped,
        result: quick_scan(state, health)?,
    })
}

#[cfg(test)]
mod tests;

/// What a repair changed, with the diagnosis the screen shows next. The rescan is quick, so the
/// result reflects the repaired library instead of the one the reader selected against.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairApplied {
    revision: i64,
    journal_id: String,
    snapshot: Option<String>,
    result: ScanResult,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairUndone {
    revision: i64,
    /// Records the reader changed after the repair, which the undo left as they are.
    skipped: Vec<String>,
    result: ScanResult,
}

/// One repair the reader can still undo.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JournalSummary {
    id: String,
    created_at: i64,
    from_revision: i64,
    to_revision: i64,
    changes: usize,
    held_objects: usize,
    /// Whether the library is still at the revision this repair produced.
    current: bool,
}
