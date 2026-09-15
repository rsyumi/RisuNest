//! Diagnosis commands. A scan reads the leased generation through its own connection, so the
//! library stays writable while it runs, and it writes its result to the working folder so the
//! screen can show the last diagnosis and a deep scan can resume after a restart.

use super::{
    current_time_ms, with_store_mutex_admitted, with_store_mutex_mut_admitted,
    PersistentStoreState, RendererOperationGuard,
};
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
    let findings = session.reader.scan(ScanDepth::Quick, FINDING_LIMIT, probe)?;
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

/// The first step of a deep scan is the library itself, including the stored cold payloads a
/// quick scan leaves closed. The object pass that follows is what the pages continue.
fn scan_deep_first(session: &Session, probe: &dyn CancellationProbe) -> StoreResult<ScanResult> {
    let findings = session.reader.scan(ScanDepth::Deep, FINDING_LIMIT, probe)?;
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

#[cfg(test)]
mod tests;
