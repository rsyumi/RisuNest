//! Reads each archive payload that has no asset name record once, without holding the store.
use super::{with_store_mutex_admitted, PersistentStoreState, RendererOperationGuard};
use crate::native_log::logged;
use crate::persistent_store::StoreResult;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Records what the archives this device holds name, without fetching any. Runs once in the
/// background after the store opens, so the cleanup after a permanent delete finds them recorded.
pub(super) fn record_held(state: &PersistentStoreState) {
    let Ok(Some(operation_guard)) = state.try_admit_renderer_operation() else { return };
    let _ = logged("pds_archive_names_record_held", record(state, &operation_guard, false));
}

/// Before the unused-file cleanup preview: also fetches each payload only another storage holds.
/// A payload that is still not recorded afterwards keeps every alias the cleanup would delete.
pub(super) fn record_before_cleanup(state: &PersistentStoreState, operation_guard: &RendererOperationGuard) {
    let _ = logged("pds_archive_names_record", record(state, operation_guard, true));
}

fn record(state: &PersistentStoreState, operation_guard: &RendererOperationGuard, fetch: bool) -> StoreResult<()> {
    let fill = with_store_mutex_admitted(state, operation_guard, |store| store.archive_name_fill())?;
    if fill.is_empty() {
        return Ok(());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    state.cancel_on_renderer_maintenance(&cancelled)?;
    fill.run(fetch, &cancelled)
}
