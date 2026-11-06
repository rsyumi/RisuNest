//! Diagnosis commands. A scan reads the leased generation through its own connection, so the
//! library stays writable while it runs, and it writes its result to the working folder so the
//! screen can show the last diagnosis and a deep scan can resume after a restart.

use super::{
    current_time_ms, with_store_mutex_admitted, with_store_mutex_mut_admitted,
    PersistentStoreState, RendererOperationGuard,
};
use crate::data_health::journal;
use crate::data_health::repair::{self, RepairAction, RepairCandidate, RepairPreview};
use crate::data_health::{codes, read_result, write_result, DeepProgress, ScanDepth, ScanResult};
use crate::local_backup::CancellationProbe;
use crate::persistent_store::snapshot::KeepCount;
use crate::persistent_store::{DataHealthReader, StoreError, StoreResult};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::State;
use crate::native_log::logged;

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

impl Session {
    /// Closes the reader before the lease goes back, and returns the folder the result belongs
    /// in. The reader's read transaction is not one the lease counts, so a release made while it
    /// is open runs the truncate checkpoint against it and waits out the store's busy timeout
    /// under the store mutex.
    fn close(self, state: &PersistentStoreState, operation_guard: &RendererOperationGuard) -> PathBuf {
        let Session {
            reader,
            lease,
            working_root,
        } = self;
        drop(reader);
        release(state, operation_guard, &lease);
        working_root
    }
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
    logged("pds_data_health_scan", quick_scan(&state, &health))
}

fn quick_scan(
    state: &PersistentStoreState,
    health: &DataHealthState,
) -> StoreResult<ScanResult> {
    let operation_guard = state.admit_renderer_operation()?;
    let probe = health.begin();
    let session = open_session(state, &operation_guard, None)?;
    let outcome = scan_quick(&session, &probe);
    let working_root = session.close(state, &operation_guard);
    let result = outcome?;
    persist(&working_root, &result)?;
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
    logged("pds_data_health_deep_scan", deep_scan(&state, &health, resume))
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
    let working_root = session.close(state, &operation_guard);
    let result = outcome?;
    persist(&working_root, &result)?;
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
    logged("pds_data_health_result", last_result(&state))
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
    logged("pds_data_health_repair_plan", repair_plan(&state))
}

fn repair_plan(state: &PersistentStoreState) -> StoreResult<Vec<RepairCandidate>> {
    let operation_guard = state.admit_renderer_operation()?;
    let diagnosis = current_diagnosis(state, &operation_guard)?.1;
    let returned = returned_bodies(state, &operation_guard, &diagnosis)?;
    Ok(repair::plan(&diagnosis).into_iter().filter(|candidate| !returned.contains(&candidate.id)).collect())
}

/// Drop-alias choices whose body has become available since the diagnosis. Custody changes do
/// not move the revision, so a stored diagnosis can still name a body this device can now reach.
fn returned_bodies(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    diagnosis: &ScanResult,
) -> StoreResult<BTreeSet<String>> {
    let absent = repair::plan(diagnosis)
        .into_iter()
        .filter_map(|candidate| match &candidate.action {
            RepairAction::DropAlias { kind, key } if diagnosis.items[candidate.finding].code == codes::ALIAS_OBJECT_ABSENT => {
                Some((candidate.id.clone(), (kind.clone(), key.clone())))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if absent.is_empty() {
        return Ok(BTreeSet::new());
    }
    let aliases = absent.iter().map(|(_, alias)| alias.clone()).collect::<Vec<_>>();
    let available = with_store_mutex_admitted(state, operation_guard, |store| store.alias_bodies_available(&aliases))?;
    Ok(absent.into_iter().zip(available).filter(|(_, available)| *available).map(|((id, _), _)| id).collect())
}

fn offered_selection(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    diagnosis: &ScanResult,
    selection: &[String],
) -> StoreResult<Vec<String>> {
    let returned = returned_bodies(state, operation_guard, diagnosis)?;
    Ok(selection.iter().filter(|id| !returned.contains(*id)).cloned().collect())
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_repair_preview(
    state: State<'_, PersistentStoreState>,
    selection: Vec<String>,
    expected_scanned_at: i64,
) -> Result<RepairPreview, StoreError> {
    logged("pds_data_health_repair_preview", (|| {
        let operation_guard = state.admit_renderer_operation()?;
        let diagnosis = current_diagnosis(&state, &operation_guard)?.1;
        require_diagnosis_identity(&diagnosis, diagnosis.revision, expected_scanned_at)?;
        let selection = offered_selection(&state, &operation_guard, &diagnosis, &selection)?;
        Ok(repair::preview(&diagnosis, &selection))
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_repair_apply(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
    selection: Vec<String>,
    snapshot: bool,
    keep_count: Option<u32>,
    expected_revision: i64,
    expected_scanned_at: i64,
) -> Result<RepairApplied, StoreError> {
    logged("pds_data_health_repair_apply", (|| {
        let snapshot = match snapshot {
            true => Some(KeepCount::requested(keep_count)?),
            false => None,
        };
        apply_repair(&state, &health, &selection, snapshot, expected_revision, expected_scanned_at)
    })())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntentDiscarded {
    discarded: bool,
    result: ScanResult,
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_discard_intent(
    state: State<'_, PersistentStoreState>,
    health: State<'_, DataHealthState>,
    finding: usize,
    expected_revision: i64,
    expected_scanned_at: i64,
) -> Result<IntentDiscarded, StoreError> {
    logged("pds_data_health_discard_intent", discard_intent(&state, &health, finding, expected_revision, expected_scanned_at))
}

fn discard_intent(
    state: &PersistentStoreState,
    health: &DataHealthState,
    finding: usize,
    expected_revision: i64,
    expected_scanned_at: i64,
) -> StoreResult<IntentDiscarded> {
    let guard = state.admit_renderer_operation()?;
    let (_, diagnosis) = current_diagnosis(state, &guard)?;
    require_diagnosis_identity(&diagnosis, expected_revision, expected_scanned_at)?;
    let selected = diagnosis.items.get(finding).filter(|item| {
        item.code == crate::data_health::codes::INTENT_QUARANTINED && item.owner.kind == "intent"
            && item.intent_action == Some(crate::data_health::IntentAction::Discard)
    }).ok_or_else(|| StoreError::Validation { message: "select a quarantined recovery record".into() })?;
    let token = selected.locator.as_ref().ok_or_else(|| StoreError::Validation {
        message: "check the data before discarding the recovery record".into(),
    })?;
    let discarded = with_store_mutex_mut_admitted(state, &guard, |store| {
        store.lww_discard_quarantined_intent(&selected.owner.id, &token.source_path, expected_revision)
    })?;
    drop(guard);
    Ok(IntentDiscarded { discarded, result: quick_scan(state, health)? })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntentCompleted {
    completed: bool,
    revision: i64,
    result: ScanResult,
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_complete_intent(
    state: State<'_, PersistentStoreState>, health: State<'_, DataHealthState>,
    finding: usize, expected_revision: i64, expected_scanned_at: i64,
) -> Result<IntentCompleted, StoreError> {
    logged("pds_data_health_complete_intent", complete_intent(&state, &health, finding, expected_revision, expected_scanned_at))
}

fn complete_intent(
    state: &PersistentStoreState, health: &DataHealthState,
    finding: usize, expected_revision: i64, expected_scanned_at: i64,
) -> StoreResult<IntentCompleted> {
    let guard = state.admit_renderer_operation()?;
    let (_, diagnosis) = current_diagnosis(state, &guard)?;
    require_diagnosis_identity(&diagnosis, expected_revision, expected_scanned_at)?;
    let selected = diagnosis.items.get(finding).filter(|item| {
        item.code == crate::data_health::codes::INTENT_QUARANTINED && item.owner.kind == "intent"
            && item.intent_action == Some(crate::data_health::IntentAction::Complete)
    }).ok_or_else(|| StoreError::Validation { message: "select a recovery record that can be completed".into() })?;
    let token = selected.locator.as_ref().ok_or_else(|| StoreError::Validation {
        message: "check the data before completing the recovery record".into(),
    })?;
    let (completed, revision) = with_store_mutex_mut_admitted(state, &guard, |store| {
        let completed = store.lww_complete_quarantined_intent(&selected.owner.id, &token.source_path, expected_revision)?;
        Ok((completed, store.revision()?))
    })?;
    drop(guard);
    Ok(IntentCompleted { completed, revision, result: quick_scan(state, health).map_err(|failure| StoreError::Committed {
        revision, message: failure.to_string(),
    })? })
}

fn require_diagnosis_identity(result: &ScanResult, revision: i64, scanned_at: i64) -> StoreResult<()> {
    if result.revision != revision {
        return Err(StoreError::RevisionConflict { expected: revision, actual: result.revision });
    }
    if result.scanned_at != scanned_at {
        return Err(StoreError::Validation { message: "the diagnosis changed; review the repair again".to_owned() });
    }
    Ok(())
}
fn apply_repair(
    state: &PersistentStoreState,
    health: &DataHealthState,
    selection: &[String],
    snapshot: Option<KeepCount>,
    expected_revision: i64,
    expected_scanned_at: i64,
) -> StoreResult<RepairApplied> {
    let operation_guard = state.admit_renderer_operation()?;
    let (root, diagnosis) = current_diagnosis(state, &operation_guard)?;
    require_diagnosis_identity(&diagnosis, expected_revision, expected_scanned_at)?;
    let selection = offered_selection(state, &operation_guard, &diagnosis, selection)?;
    let preview = repair::preview(&diagnosis, &selection);
    if preview.selected.is_empty() {
        return Err(StoreError::Validation {
            message: "select at least one change to repair".to_owned(),
        });
    }
    let now = current_time_ms()?;
    // The snapshot is kept before anything changes, so it holds the library the reader selected
    // against rather than the repaired one.
    let snapshot = match snapshot {
        Some(keep) => Some(
            with_store_mutex_mut_admitted(state, &operation_guard, |store| {
                store.snapshot_create_keeping("data-health-repair", keep)
            })?
            .id,
        ),
        None => None,
    };
    let (revision, journal) = with_store_mutex_mut_admitted(state, &operation_guard, |store| {
        store.apply_repair(diagnosis.revision, &preview.selected, now)
    })?;
    journal::write(&root, &journal).map_err(|error| StoreError::Committed {
        revision: revision.revision,
        message: format!("failed to write the repair journal: {error}"),
    })?;
    drop(operation_guard);
    Ok(RepairApplied {
        revision: revision.revision,
        journal_id: journal.id,
        snapshot,
        result: quick_scan(state, health).map_err(|error| StoreError::Committed {
            revision: revision.revision,
            message: error.to_string(),
        })?,
    })
}

#[tauri::command(async)]
pub(crate) fn pds_data_health_journals(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<JournalSummary>, StoreError> {
    logged("pds_data_health_journals", journals(&state))
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
    expected_revision: i64,
) -> Result<RepairUndone, StoreError> {
    logged("pds_data_health_undo", undo_repair(&state, &health, &journal_id, expected_revision))
}

fn undo_repair(
    state: &PersistentStoreState,
    health: &DataHealthState,
    journal_id: &str,
    expected_revision: i64,
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
    let (committed, skipped) = with_store_mutex_mut_admitted(state, &operation_guard, |store| {
        store.undo_repair(&entry, expected_revision)
    })?;
    journal::remove(&root, journal_id).map_err(|error| StoreError::Committed {
        revision: committed.revision,
        message: format!("failed to drop the repair journal: {error}"),
    })?;
    drop(operation_guard);
    Ok(RepairUndone {
        revision: committed.revision,
        skipped,
        result: quick_scan(state, health).map_err(|error| StoreError::Committed {
            revision: committed.revision,
            message: error.to_string(),
        })?,
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
