pub(crate) mod data_health;
pub(crate) mod gallery;
pub(crate) mod image_geometry;
pub(crate) mod hypa;

use super::archive::ArchivePreview;
use super::device_store::invalid;
use super::device_store::plugin_values::{
    PluginDeviceHydration, PluginDeviceListItem, PluginDeviceMutation, PluginDeviceRead, PluginDeviceRange,
};
use super::device_store::sections::{section_from_id, CHOOSABLE_SECTIONS};
use super::export::ExportedRisuSave;
#[cfg(feature = "native-kei-upload-pilot")]
use super::kei::KeiUploadResult;
use super::content_change_index::{ContentChangeWindow, ContentKey};
use super::{
    AssetAlias, AssetAliasListQuery, AssetAliasPage, AssetOwnerHead, AssetOwnerLocator,
    AssignedPluginStorage, CharacterPage, CharacterQuery,
    CharacterSummary, CheckpointMode, ClaimedPluginValue, ConversationPage, ConversationQuery,
    ConversationWindow, ConversationWindowQuery, LeaseResult, PersistentStorageStats,
    PersistentStore, PluginStorageCatalog, PluginStorageListItem, PresetCatalog, RevisionResult,
    SnapshotCreated, SnapshotInfo, StagingResult, StoreError, StoreResult, Versioned, WorkingSetCommit,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use crate::native_log::logged;

#[cfg(feature = "official-publication-upload-pilot")]
use crate::publication_upload::{
    upload_open_file_attempt, OfficialPublicationUploadError, OfficialPublicationUploadRequest,
    OfficialPublicationUploadResult,
};

pub(crate) struct PersistentStoreState {
    store: Mutex<Option<PersistentStore>>,
    status_store: Mutex<Option<(String, PersistentStore)>>,
    snapshot_operations: Mutex<()>,
    renderer_gate: Arc<RendererGate>,
    archive_operations: Mutex<HashMap<String, Arc<AtomicBool>>>,
    retired_purge_started: AtomicBool,
}

struct ArchiveOperationGuard<'a> {
    state: &'a PersistentStoreState,
    operation_id: String,
    cancelled: Arc<AtomicBool>,
}

impl ArchiveOperationGuard<'_> {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Drop for ArchiveOperationGuard<'_> {
    fn drop(&mut self) {
        self.state
            .archive_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.operation_id);
    }
}

#[derive(Default)]
struct RendererGate {
    state: Mutex<RendererGateState>,
    drained: Condvar,
}

#[derive(Default)]
struct RendererGateState {
    maintenance_active: bool,
    operations: usize,
    /// Advances whenever renderer maintenance starts.
    epoch: u64,
    /// Long operations that stop when maintenance starts instead of delaying it.
    cancellations: Vec<std::sync::Weak<AtomicBool>>,
}

impl RendererGateState {
    fn begin_maintenance(&mut self) {
        self.maintenance_active = true;
        for cancel in self.cancellations.drain(..) {
            if let Some(cancel) = cancel.upgrade() { cancel.store(true, Ordering::Release); }
        }
    }
}

/// Covers the entire native side effect, including work outside the SQLite lock.
/// Maintenance closes admission first, then waits for these permits to drain.
pub(crate) struct RendererOperationGuard {
    gate: Arc<RendererGate>,
}

impl Drop for RendererOperationGuard {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.operations -= 1;
        if state.operations == 0 {
            self.gate.drained.notify_all();
        }
    }
}

/// Native ownership survives renderer reloads. The device coordinator must retain
/// this guard until its durable session is resolved, including after errors.
/// Independent native job stores are excluded by the native job admission permit
/// that the caller must acquire before this guard.
pub(crate) struct DeviceMaintenanceGuard {
    gate: Arc<RendererGate>,
}

impl Drop for DeviceMaintenanceGuard {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .maintenance_active = false;
    }
}

fn renderer_gate_error() -> StoreError {
    StoreError::Validation {
        message: "persistent storage is unavailable during device backup maintenance".to_owned(),
    }
}

impl PersistentStoreState {
    fn begin_archive_operation(
        &self,
        operation_id: String,
    ) -> StoreResult<ArchiveOperationGuard<'_>> {
        if operation_id.is_empty() || operation_id.len() > 128 {
            return Err(StoreError::Validation {
                message: "character archive operation id is invalid".to_owned(),
            });
        }
        let mut operations = self.archive_operations.lock().map_err(|error| StoreError::Store {
            message: format!("character archive operation mutex poisoned: {error}"),
        })?;
        if operations.contains_key(&operation_id) {
            return Err(StoreError::Validation {
                message: "character archive operation id is already active".to_owned(),
            });
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        operations.insert(operation_id.clone(), Arc::clone(&cancelled));
        Ok(ArchiveOperationGuard {
            state: self,
            operation_id,
            cancelled,
        })
    }

    fn cancel_archive_operation(&self, operation_id: &str) -> StoreResult<bool> {
        let operations = self.archive_operations.lock().map_err(|error| StoreError::Store {
            message: format!("character archive operation mutex poisoned: {error}"),
        })?;
        let Some(cancelled) = operations.get(operation_id) else {
            return Ok(false);
        };
        cancelled.store(true, Ordering::Release);
        Ok(true)
    }

    pub(crate) fn maintenance_active(&self) -> bool {
        self.renderer_gate
            .state
            .lock()
            .is_ok_and(|state| state.maintenance_active)
    }

    pub(crate) fn admit_renderer_operation(&self) -> StoreResult<RendererOperationGuard> {
        self.try_admit_renderer_operation()?
            .ok_or_else(renderer_gate_error)
    }

    /// Sets `cancel` when renderer maintenance starts, so an admitted operation
    /// that may run for minutes does not hold maintenance until it finishes.
    pub(crate) fn cancel_on_renderer_maintenance(&self, cancel: &Arc<AtomicBool>) -> StoreResult<()> {
        let mut state = self.renderer_gate.state.lock().map_err(|_| renderer_gate_error())?;
        if state.maintenance_active {
            cancel.store(true, Ordering::Release);
            return Ok(());
        }
        state.cancellations.retain(|registered| registered.strong_count() > 0);
        state.cancellations.push(Arc::downgrade(cancel));
        Ok(())
    }

    /// The maintenance epoch a command records when it is dispatched, before it
    /// waits for a blocking thread.
    pub(crate) fn renderer_dispatch_epoch(&self) -> StoreResult<u64> {
        self.renderer_gate
            .state
            .lock()
            .map(|state| state.epoch)
            .map_err(|_| renderer_gate_error())
    }

    /// Admits an operation dispatched at `epoch` only if no renderer maintenance
    /// or reset has started since, so a request queued before it never runs after it.
    pub(crate) fn admit_renderer_operation_dispatched_at(&self, epoch: u64) -> StoreResult<RendererOperationGuard> {
        self.try_admit_renderer_operation_at(Some(epoch))?
            .ok_or_else(renderer_gate_error)
    }

    /// Admits an operation unless device maintenance holds the gate.
    fn try_admit_renderer_operation(&self) -> StoreResult<Option<RendererOperationGuard>> {
        self.try_admit_renderer_operation_at(None)
    }

    fn try_admit_renderer_operation_at(&self, dispatched: Option<u64>) -> StoreResult<Option<RendererOperationGuard>> {
        let mut state = self
            .renderer_gate
            .state
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent renderer admission mutex poisoned: {error}"),
            })?;
        if state.maintenance_active || dispatched.is_some_and(|epoch| epoch != state.epoch) {
            return Ok(None);
        }
        state.operations += 1;
        Ok(Some(RendererOperationGuard {
            gate: Arc::clone(&self.renderer_gate),
        }))
    }

    pub(crate) fn open_admitted_native_job_store(
        &self,
        operation: &RendererOperationGuard,
    ) -> StoreResult<PersistentStore> {
        with_store_mutex_admitted(self, operation, PersistentStore::open_native_job_store)
    }

    pub(crate) fn with_status_job_store<T>(&self, operation: impl FnOnce(&PersistentStore) -> T) -> StoreResult<T> {
        let permit = self.admit_renderer_operation()?;
        let mut cached = self.status_store.lock().map_err(|_| renderer_gate_error())?;
        with_store_mutex_admitted(self, &permit, |store| {
            if cached.as_ref().is_none_or(|(instance, _)| instance != &store.gc_instance) {
                *cached = Some((store.gc_instance.clone(), store.open_native_job_store()?));
            }
            Ok(())
        })?;
        Ok(operation(&cached.as_ref().unwrap().1))
    }

    pub(crate) fn acquire_cleanup_maintenance(&self, timeout: std::time::Duration) -> StoreResult<DeviceMaintenanceGuard> {
        let deadline = std::time::Instant::now() + timeout;
        let mut state = self.renderer_gate.state.lock().map_err(|_| renderer_gate_error())?;
        if state.maintenance_active { return Err(renderer_gate_error()); }
        state.epoch += 1;
        state.begin_maintenance();
        let maintenance = DeviceMaintenanceGuard { gate: Arc::clone(&self.renderer_gate) };
        while state.operations != 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                drop(state);
                drop(maintenance);
                return Err(renderer_gate_error());
            }
            let waited = self.renderer_gate.drained.wait_timeout(state, remaining)
                .map_err(|_| renderer_gate_error())?;
            state = waited.0;
        }
        drop(state);
        self.status_store.lock().map_err(|_| renderer_gate_error())?.take();
        self.store.lock().map_err(|_| renderer_gate_error())?.take();
        Ok(maintenance)
    }

    pub(crate) fn acquire_device_maintenance(&self) -> StoreResult<DeviceMaintenanceGuard> {
        let maintenance = self
            .try_acquire_renderer_maintenance()?
            .ok_or_else(renderer_gate_error)?;
        self.status_store.lock().map_err(|_| renderer_gate_error())?.take();
        // No new operation can enter between draining and closing the store.
        // Reopening after the guard is released refreshes the connection after
        // native restore and discards leases owned by the previous renderer.
        self.store
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent store mutex poisoned: {error}"),
            })?
            .take();
        Ok(maintenance)
    }

    /// Prevents application writes while raw files are copied without changing
    /// the lifetime or state of an already-open SQLite connection.
    pub(crate) fn acquire_raw_capture(&self) -> StoreResult<DeviceMaintenanceGuard> {
        self.try_acquire_renderer_maintenance()?
            .ok_or_else(renderer_gate_error)
    }

    fn try_acquire_renderer_maintenance(&self) -> StoreResult<Option<DeviceMaintenanceGuard>> {
        let mut state = self
            .renderer_gate
            .state
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent renderer admission mutex poisoned: {error}"),
            })?;
        // Advanced even when maintenance is already active, so a page reset
        // then still refuses what the outgoing document dispatched.
        state.epoch += 1;
        if state.maintenance_active {
            return Ok(None);
        }
        state.begin_maintenance();
        let maintenance = DeviceMaintenanceGuard {
            gate: Arc::clone(&self.renderer_gate),
        };
        while state.operations != 0 {
            state = self
                .renderer_gate
                .drained
                .wait(state)
                .map_err(|error| StoreError::Store {
                    message: format!("persistent renderer drain mutex poisoned: {error}"),
                })?;
        }
        Ok(Some(maintenance))
    }

    /// Opens the renderer store again while `maintenance` still keeps renderer
    /// operations out, for a native commit that the renderer adopts in place.
    pub(crate) fn reopen_under_maintenance(
        &self,
        maintenance: &DeviceMaintenanceGuard,
        app_data_dir: &Path,
    ) -> StoreResult<()> {
        if !Arc::ptr_eq(&maintenance.gate, &self.renderer_gate) {
            return Err(StoreError::Validation {
                message: "device maintenance guard belongs to another persistent store".to_owned(),
            });
        }
        let mut store = self.store.lock().map_err(|error| StoreError::Store {
            message: format!("persistent store mutex poisoned: {error}"),
        })?;
        open_persistent_store(app_data_dir, &mut store).map(|_| ())
    }

    pub(crate) fn reset_renderer_session(&self) -> StoreResult<()> {
        let Some(_maintenance) = self.try_acquire_renderer_maintenance()? else {
            return Ok(());
        };
        let mut store = self.store.lock().map_err(|error| StoreError::Store {
            message: format!("persistent store mutex poisoned: {error}"),
        })?;
        if let Some(store) = store.as_mut() {
            store.release_all_revision_leases()?;
        }
        Ok(())
    }
}

impl Default for PersistentStoreState {
    fn default() -> Self {
        Self {
            store: Mutex::new(None),
            status_store: Mutex::new(None),
            snapshot_operations: Mutex::new(()),
            renderer_gate: Arc::new(RendererGate::default()),
            archive_operations: Mutex::new(HashMap::new()),
            retired_purge_started: AtomicBool::new(false),
        }
    }
}

const RETIRED_PURGE_BATCH_ROWS: usize = 512;
const RETIRED_PURGE_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);
const RETIRED_PURGE_BUSY: std::time::Duration = std::time::Duration::from_secs(1);
const RETIRED_PURGE_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

impl PersistentStoreState {
    /// One bounded purge step that never waits for the store. Returns whether
    /// retired libraries remain, or nothing while the store is busy, closed or
    /// under device maintenance.
    fn purge_retired_step(&self, limit_rows: usize) -> StoreResult<Option<bool>> {
        let Some(_operation) = self.try_admit_renderer_operation()? else {
            return Ok(None);
        };
        let mut store = match self.store.try_lock() {
            Ok(store) => store,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(error)) => {
                return Err(StoreError::Store {
                    message: format!("persistent store mutex poisoned: {error}"),
                })
            }
        };
        let Some(store) = store.as_mut() else {
            return Ok(None);
        };
        if let Ok(device) = store.device_store_mut() { device.plugin_writes.expire(); }
        store.purge_retired_batch(limit_rows).map(Some)
    }
}

/// Deletes retired libraries in the background for the life of the process,
/// starting with whatever an earlier run left behind.
fn start_retired_purge<R: tauri::Runtime>(app: &AppHandle<R>, state: &PersistentStoreState) {
    if state.retired_purge_started.swap(true, Ordering::AcqRel) {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("pds-retired-purge".to_owned())
        .spawn(move || {
            let state = app.state::<PersistentStoreState>();
            loop {
                let pause = match logged(
                    "pds_retired_purge",
                    state.purge_retired_step(RETIRED_PURGE_BATCH_ROWS),
                ) {
                    Ok(Some(true)) => RETIRED_PURGE_PAUSE,
                    Ok(None) => RETIRED_PURGE_BUSY,
                    Ok(Some(false)) | Err(_) => RETIRED_PURGE_IDLE,
                };
                std::thread::sleep(pause);
            }
        });
    if spawned.is_err() {
        state.retired_purge_started.store(false, Ordering::Release);
    }
}

#[cfg(test)]
impl PersistentStoreState {
    pub(crate) fn with_test_store(store: PersistentStore) -> Self {
        Self { store: Mutex::new(Some(store)), ..Self::default() }
    }

    /// A store the renderer has not opened yet, whose opening starts no purge.
    pub(crate) fn unopened_without_purge() -> Self {
        Self { retired_purge_started: AtomicBool::new(true), ..Self::default() }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistentStoreOpenResult {
    revision: i64,
}

fn current_time_ms() -> StoreResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::Store {
            message: format!("system clock is before the Unix epoch: {error}"),
        })?;
    i64::try_from(duration.as_millis()).map_err(|_| StoreError::Store {
        message: "system time exceeds the persistent asset maintenance range".to_owned(),
    })
}

pub(crate) fn with_store<T>(
    state: State<'_, PersistentStoreState>,
    operation: impl FnOnce(&PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    with_store_mutex(&state, operation)
}

pub(crate) fn with_store_if_ready<T>(
    state: &PersistentStoreState,
    operation: impl FnOnce(&PersistentStore) -> StoreResult<T>,
) -> StoreResult<Option<T>> {
    let Some(_guard) = state.try_admit_renderer_operation()? else { return Ok(None); };
    let store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    store.as_ref().map(operation).transpose()
}

fn with_store_mutex<T>(
    state: &PersistentStoreState,
    operation: impl FnOnce(&PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    let operation_guard = state.admit_renderer_operation()?;
    with_store_mutex_admitted(state, &operation_guard, operation)
}

fn with_store_mutex_admitted<T>(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    operation: impl FnOnce(&PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    if !operation_guard.belongs_to(state) {
        return Err(StoreError::Validation {
            message: "renderer operation permit belongs to another persistent store".to_owned(),
        });
    }
    let store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    let store = store.as_ref().ok_or_else(|| StoreError::Validation {
        message: "persistent store has not been opened".to_owned(),
    })?;
    operation(store)
}

impl RendererOperationGuard {
    fn belongs_to(&self, state: &PersistentStoreState) -> bool {
        Arc::ptr_eq(&self.gate, &state.renderer_gate)
    }
}

#[cfg(any(target_os = "android", test))]
fn with_snapshot_directory<T>(
    state: &PersistentStoreState,
    operation: impl FnOnce(&Path) -> StoreResult<T>,
) -> StoreResult<T> {
    let _operation = state.admit_renderer_operation()?;
    let directory = {
        let store = state.store.lock().map_err(|error| StoreError::Store {
            message: format!("persistent store mutex poisoned: {error}"),
        })?;
        store
            .as_ref()
            .ok_or_else(|| StoreError::Validation {
                message: "persistent store has not been opened".to_owned(),
            })?
            .snapshots_dir
            .clone()
    };
    // Archive owns its cross-process lock; listing does not need the live database.
    operation(&directory)
}

pub(crate) fn with_store_mut<T>(
    state: State<'_, PersistentStoreState>,
    operation: impl FnOnce(&mut PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    with_store_mutex_mut(&state, operation)
}

fn with_store_mutex_mut<T>(
    state: &PersistentStoreState,
    operation: impl FnOnce(&mut PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    let operation_guard = state.admit_renderer_operation()?;
    with_store_mutex_mut_admitted(state, &operation_guard, operation)
}

pub(crate) fn with_store_mut_admitted<T>(
    state: State<'_, PersistentStoreState>,
    operation_guard: &RendererOperationGuard,
    operation: impl FnOnce(&mut PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    with_store_mutex_mut_admitted(&state, operation_guard, operation)
}

fn with_store_mutex_mut_admitted<T>(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    operation: impl FnOnce(&mut PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    if !operation_guard.belongs_to(state) {
        return Err(StoreError::Validation {
            message: "renderer operation permit belongs to another persistent store".to_owned(),
        });
    }
    let mut store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    let store = store.as_mut().ok_or_else(|| StoreError::Validation {
        message: "persistent store has not been opened".to_owned(),
    })?;
    store.lww_recover_intents()?;
    operation(store)
}

pub(crate) fn commit_staged_replacement(
    app: &AppHandle,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<RevisionResult> {
    let state = app.state::<PersistentStoreState>();
    let operation_guard = state.admit_renderer_operation()?;
    let prepared = with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
        store.prepare_replace_commit(staging_id, expected_revision)
    })?;
    with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
        store.finish_prepared_replace(prepared)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_open(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
) -> Result<PersistentStoreOpenResult, StoreError> {
    logged("pds_open", open_for_renderer(&app, &state))
}

/// Opens the store the renderer works through and starts the work that waits for it.
pub(crate) fn open_for_renderer<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &PersistentStoreState,
) -> StoreResult<PersistentStoreOpenResult> {
    let operation_guard = state.admit_renderer_operation()?;
    let app_data_dir = crate::app_paths::data_root(app).map_err(|message| StoreError::Store { message })?;
    let (opened, first_open) = open_renderer_persistent_store_admitted(state, &operation_guard, &app_data_dir)?;
    start_retired_purge(app, state);
    // A page that starts before the store opens cannot judge the asset journals
    // whose owners only the store records, so they are judged again now.
    if first_open {
        if let Some(cas) = app.try_state::<crate::asset_repository::commands::DurableCasJobState>() {
            if let Err(error) = cas.sweep_settled_jobs(app) {
                crate::nlog!("warn", "Asset journals await cleanup after the store opened: {error}");
            }
        }
    }
    Ok(opened)
}

#[cfg(test)]
pub(crate) fn open_renderer_persistent_store(
    state: &PersistentStoreState,
    app_data_dir: &Path,
) -> StoreResult<PersistentStoreOpenResult> {
    let operation_guard = state.admit_renderer_operation()?;
    open_renderer_persistent_store_admitted(state, &operation_guard, app_data_dir).map(|(opened, _)| opened)
}

/// Also returns whether this call opened the store.
fn open_renderer_persistent_store_admitted(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    app_data_dir: &Path,
) -> StoreResult<(PersistentStoreOpenResult, bool)> {
    if !operation_guard.belongs_to(state) {
        return Err(StoreError::Validation {
            message: "renderer operation permit belongs to another persistent store".to_owned(),
        });
    }
    let mut store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    let opening = store.is_none();
    open_persistent_store(app_data_dir, &mut store).map(|opened| (opened, opening))
}

fn open_persistent_store(
    app_data_dir: &Path,
    store: &mut Option<PersistentStore>,
) -> StoreResult<PersistentStoreOpenResult> {
    if let Some(store) = store.as_mut() {
        let revision = store.revision()?;
        return Ok(PersistentStoreOpenResult {
            revision,
        });
    }

    let persistent_store = PersistentStore::open(app_data_dir)?;
    let revision = persistent_store.revision()?;
    *store = Some(persistent_store);

    Ok(PersistentStoreOpenResult {
        revision,
    })
}

#[tauri::command(async)]
pub(crate) fn pds_asset_gc_maintenance(
    state: State<'_, PersistentStoreState>,
) -> Result<crate::asset_repository::migration_gc::AssetGcDryRunPage, StoreError> {
    logged("pds_asset_gc_maintenance", with_store_mut(state, |store| {
        store.asset_gc_product_maintenance_page(current_time_ms()?)
    }))
}

/// Sweeps one batch of message objects in each store, continuing from each
/// store's cursor. It skips while device maintenance holds the renderer gate.
#[tauri::command(async)]
pub(crate) fn pds_message_object_sweep(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
) -> Result<(), StoreError> {
    let Some(operation_guard) = state.try_admit_renderer_operation()? else {
        return Ok(());
    };
    logged(
        "pds_message_object_sweep",
        sweep_message_pages(
            file_admission(&app).as_ref(), &state, &operation_guard, current_time_ms()?, SweepBudget::OneBatch,
        ),
    )
}

fn file_admission(app: &AppHandle) -> Option<Arc<crate::native_file_jobs::admission::Admission>> {
    app.try_state::<crate::native_file_jobs::NativeFileJobState>()
        .map(|jobs| Arc::clone(&jobs.admission))
}

#[tauri::command(async)]
pub(crate) fn pds_read_root(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<Versioned<Value>, StoreError> {
    logged("pds_read_root", with_store(state, |store| store.read_root(lease.as_deref())))
}

/// The current revision, for callers that would otherwise read the whole root for it.
#[tauri::command(async)]
pub(crate) fn pds_read_revision(state: State<'_, PersistentStoreState>) -> Result<i64, StoreError> {
    logged("pds_read_revision", with_store(state, |store| store.revision()))
}

#[tauri::command(async)]
pub(crate) fn pds_read_library_identity(state: State<'_, PersistentStoreState>) -> Result<String, StoreError> {
    logged("pds_read_library_identity", with_store(state, |store| super::active_generation(&store.connection)))
}

#[tauri::command(async)]
pub(crate) fn pds_query_presets(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<PresetCatalog, StoreError> {
    logged("pds_query_presets", with_store(state, |store| store.query_presets(lease.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_read_preset(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    logged("pds_read_preset", with_store(state, |store| store.read_preset(&id, lease.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_query_characters(
    state: State<'_, PersistentStoreState>,
    query: CharacterQuery,
    lease: Option<String>,
) -> Result<CharacterPage, StoreError> {
    logged("pds_query_characters", with_store(state, |store| {
        store.query_characters(&query, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_character(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    logged("pds_read_character", with_store(state, |store| store.read_character(&id, lease.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_read_character_summary(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<CharacterSummary>, StoreError> {
    logged("pds_read_character_summary", with_store(state, |store| {
        store.read_character_summary(&id, lease.as_deref())
    }))
}

/// Reading the window and the records it names through one lease is what keeps
/// a targeted pass equivalent to a reprojection.
#[tauri::command(async)]
pub(crate) fn pds_working_set_change_window(
    state: State<'_, PersistentStoreState>,
    lease: String,
) -> Result<ContentChangeWindow, StoreError> {
    logged("pds_working_set_change_window", with_store(state, |store| store.working_set_change_window(&lease)))
}

#[tauri::command(async)]
pub(crate) fn pds_working_set_change_page(
    state: State<'_, PersistentStoreState>,
    lease: String,
    after_revision: i64,
    after_key: Option<ContentKey>,
    limit: usize,
) -> Result<Vec<ContentKey>, StoreError> {
    logged("pds_working_set_change_page", with_store(state, |store| {
        store.working_set_change_page(&lease, after_revision, after_key, limit)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_commit_working_set_change_cursor(
    state: State<'_, PersistentStoreState>,
    revision: i64,
) -> Result<(), StoreError> {
    logged("pds_commit_working_set_change_cursor", with_store_mut(state, |store| {
        store.commit_working_set_change_cursor(revision)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_query_conversations(
    state: State<'_, PersistentStoreState>,
    query: ConversationQuery,
    lease: Option<String>,
) -> Result<ConversationPage, StoreError> {
    logged("pds_query_conversations", with_store(state, |store| {
        store.query_conversations(&query, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    conversation_id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<super::StoredConversation>>, StoreError> {
    logged("pds_read_conversation", with_store(state, |store| {
        store.read_stored_conversation(&character_id, &conversation_id, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation_metadata(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    conversation_id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<super::PersistentConversationMetadata>>, StoreError> {
    logged("pds_read_conversation_metadata", with_store(state, |store| {
        store.read_conversation_metadata(&character_id, &conversation_id, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation_window(
    state: State<'_, PersistentStoreState>,
    query: ConversationWindowQuery,
    lease: Option<String>,
) -> Result<Option<Versioned<ConversationWindow<super::StoredJson>>>, StoreError> {
    logged("pds_read_conversation_window", with_store(state, |store| {
        store.read_stored_conversation_window(&query, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation_message_metadata_window(
    state: State<'_, PersistentStoreState>,
    query: ConversationWindowQuery,
    lease: Option<String>,
) -> Result<Option<Versioned<super::ConversationMessageMetadataWindow>>, StoreError> {
    logged("pds_read_conversation_message_metadata_window", with_store(state, |store| {
        store.read_conversation_message_metadata_window(&query, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_query_plugin_storage(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
    owner: Option<String>,
) -> Result<PluginStorageCatalog, StoreError> {
    logged("pds_query_plugin_storage", with_store(state, |store| store.query_plugin_storage_for_owner(lease.as_deref(), owner.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_storage_page(
    state: State<'_, PersistentStoreState>,
    query: super::PluginStorageValueQuery,
    lease: Option<String>,
) -> Result<super::PluginStorageValuePage, StoreError> {
    logged("pds_read_plugin_storage_page", with_store(state, |store| store.read_plugin_storage_page(&query, lease.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_storage(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<Vec<PluginStorageListItem>, StoreError> {
    logged("pds_list_plugin_storage", with_store(state, |store| store.list_plugin_storage(lease.as_deref())))
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
    key: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    logged("pds_read_plugin_storage", with_store(state, |store| {
        store.read_plugin_storage(&owner, &key, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_alias(
    state: State<'_, PersistentStoreState>,
    kind: String,
    key: String,
    lease: Option<String>,
) -> Result<Option<Versioned<AssetAlias>>, StoreError> {
    logged("pds_read_asset_alias", with_store(state, |store| {
        store.read_asset_alias(&kind, &key, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_aliases_by_keys(
    state: State<'_, PersistentStoreState>,
    kind: String,
    keys: Vec<String>,
    lease: Option<String>,
) -> Result<Versioned<Vec<AssetAlias>>, StoreError> {
    logged("pds_read_asset_aliases_by_keys", with_store(state, |store| {
        store.read_asset_aliases_by_keys(&kind, &keys, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_list_asset_aliases(
    state: State<'_, PersistentStoreState>,
    query: AssetAliasListQuery,
    lease: Option<String>,
) -> Result<AssetAliasPage, StoreError> {
    logged("pds_list_asset_aliases", with_store(state, |store| {
        store.list_asset_alias_page(&query, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_owner_head(
    state: State<'_, PersistentStoreState>,
    owner: AssetOwnerLocator,
    lease: Option<String>,
) -> Result<Option<Versioned<AssetOwnerHead>>, StoreError> {
    logged("pds_read_asset_owner_head", with_store(state, |store| {
        store.read_asset_owner_head(&owner, lease.as_deref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_commit_asset_alias(
    state: State<'_, PersistentStoreState>,
    alias: AssetAlias,
    expected_revision: i64,
    expected_library: Option<String>,
) -> Result<RevisionResult, StoreError> {
    logged("pds_commit_asset_alias", with_store_mut(state, |store| {
        if let Some(expected) = expected_library {
            if super::active_generation(&store.connection)? != expected {
                return Err(StoreError::Validation { message: "Official asset library changed".into() });
            }
        }
        store.commit_asset_alias(&alias, expected_revision)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_delete_asset_alias(
    state: State<'_, PersistentStoreState>,
    kind: String,
    key: String,
    expected_revision: i64,
) -> Result<RevisionResult, StoreError> {
    logged("pds_delete_asset_alias", with_store_mut(state, |store| {
        store.delete_asset_alias(&kind, &key, expected_revision)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_commit(
    state: State<'_, PersistentStoreState>,
    commit: WorkingSetCommit,
    asset_aliases: Vec<AssetAlias>,
) -> Result<RevisionResult, StoreError> {
    logged("pds_commit", with_store_mut(state, |store| {
        store.commit_with_asset_aliases(&commit, &asset_aliases)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_archive_preview(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    lease: Option<String>,
) -> Result<ArchivePreview, StoreError> {
    logged("pds_archive_preview", with_store(state, |store| {
        store.archive_preview(&character_id, lease.as_deref())
    }))
}

/// Archiving and restoring may fetch a body another storage holds, through clients that block
/// and so cannot run on the async runtime.
async fn archive_operation<R: tauri::Runtime, T: Send + 'static>(
    app: AppHandle<R>,
    command: &'static str,
    operation: impl FnOnce(&PersistentStoreState) -> StoreResult<T> + Send + 'static,
) -> Result<T, StoreError> {
    let joined = tauri::async_runtime::spawn_blocking(move || operation(&app.state::<PersistentStoreState>())).await;
    logged(command, joined.unwrap_or_else(|error| Err(StoreError::Store {
        message: format!("character archive worker stopped: {error}"),
    })))
}

#[tauri::command(async)]
pub(crate) async fn pds_archive_character(
    app: AppHandle,
    character_id: String,
    expected_revision: i64,
    operation_id: String,
) -> Result<RevisionResult, StoreError> {
    archive_character(app, character_id, expected_revision, operation_id).await
}

async fn archive_character<R: tauri::Runtime>(
    app: AppHandle<R>,
    character_id: String,
    expected_revision: i64,
    operation_id: String,
) -> Result<RevisionResult, StoreError> {
    archive_operation(app, "pds_archive_character", move |state| {
        let now_ms = current_time_ms()?;
        let operation = state.begin_archive_operation(operation_id)?;
        with_store_mutex_mut(state, |store| {
            store.archive_character_with_cancellation(
                &character_id,
                expected_revision,
                now_ms,
                &|| operation.is_cancelled(),
                Some(operation.cancelled.clone()),
            )
        })
    })
    .await
}

#[tauri::command(async)]
pub(crate) async fn pds_restore_character(
    app: AppHandle,
    character_id: String,
    expected_revision: i64,
    operation_id: String,
) -> Result<RevisionResult, StoreError> {
    restore_character(app, character_id, expected_revision, operation_id).await
}

pub(super) async fn restore_character<R: tauri::Runtime>(
    app: AppHandle<R>,
    character_id: String,
    expected_revision: i64,
    operation_id: String,
) -> Result<RevisionResult, StoreError> {
    archive_operation(app, "pds_restore_character", move |state| {
        let operation = state.begin_archive_operation(operation_id)?;
        with_store_mutex_mut(state, |store| {
            store.restore_character_with_cancellation(
                &character_id,
                expected_revision,
                &|| operation.is_cancelled(),
                Some(operation.cancelled.clone()),
            )
        })
    })
    .await
}

#[tauri::command(async)]
pub(crate) fn pds_cancel_character_archive_operation(
    state: State<'_, PersistentStoreState>,
    operation_id: String,
) -> Result<bool, StoreError> {
    logged("pds_cancel_character_archive_operation", state.cancel_archive_operation(&operation_id))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_begin(
    state: State<'_, PersistentStoreState>,
) -> Result<StagingResult, StoreError> {
    logged("pds_replace_begin", with_store_mut(state, PersistentStore::replace_begin))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_root(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    root: Value,
    plugin_storage_values: Option<Vec<super::PluginStorageValue>>,
) -> Result<(), StoreError> {
    logged("pds_replace_put_root", with_store_mut(state, |store| {
        store.replace_put_root_with_plugin_storage(
            &staging_id,
            &root,
            plugin_storage_values.as_deref(),
        )
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_characters(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    characters: Vec<Value>,
) -> Result<(), StoreError> {
    logged("pds_replace_add_characters", with_store_mut(state, |store| {
        store.replace_add_characters(&staging_id, &characters)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_presets(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    presets: Vec<Value>,
) -> Result<(), StoreError> {
    logged("pds_replace_put_presets", with_store_mut(state, |store| {
        store.replace_put_presets(&staging_id, &presets)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_presets(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    presets: Vec<Value>,
) -> Result<(), StoreError> {
    logged("pds_replace_add_presets", with_store_mut(state, |store| {
        store.replace_add_presets(&staging_id, &presets)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_plugin_storage_values(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    values: Vec<super::PluginStorageValue>,
) -> Result<(), StoreError> {
    logged("pds_replace_add_plugin_storage_values", with_store_mut(state, |store| {
        store.replace_add_plugin_storage_values(&staging_id, &values)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_plugin_storage(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    storage: serde_json::Map<String, Value>,
    meta: Option<serde_json::Map<String, Value>>,
) -> Result<(), StoreError> {
    logged("pds_replace_add_plugin_storage", with_store_mut(state, |store| {
        store.replace_add_plugin_storage(&staging_id, &storage, meta.as_ref())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_character_detail(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    detail: Value,
    conversation_count: i64,
) -> Result<(), StoreError> {
    logged("pds_replace_put_character_detail", with_store_mut(state, |store| {
        store.replace_put_character_detail(&staging_id, &detail, conversation_count)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_conversation(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    character_id: String,
    configured_index: i64,
    conversation: Value,
    message_count: i64,
    last_message_time: Option<Value>,
) -> Result<(), StoreError> {
    logged("pds_replace_put_conversation", with_store_mut(state, |store| {
        store.replace_put_conversation(
            &staging_id,
            &character_id,
            configured_index,
            &conversation,
            message_count,
            last_message_time.as_ref(),
        )
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_conversation_messages(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    character_id: String,
    conversation_id: String,
    start: i64,
    messages: Vec<Value>,
) -> Result<(), StoreError> {
    logged("pds_replace_add_conversation_messages", with_store_mut(state, |store| {
        store.replace_add_conversation_messages(
            &staging_id,
            &character_id,
            &conversation_id,
            start,
            &messages,
        )
    }))
}

/// A staged replace request assembled by the Android chunked transport. The
/// tag names the command whose arguments it carries.
#[cfg(any(test, target_os = "android"))]
#[derive(serde::Deserialize)]
#[serde(tag = "command", content = "args")]
pub(crate) enum StagedReplaceRequest {
    #[serde(rename = "pds_replace_put_root", rename_all = "camelCase")]
    PutRoot {
        staging_id: String,
        root: Value,
        plugin_storage_values: Option<Vec<super::PluginStorageValue>>,
    },
    #[serde(rename = "pds_replace_put_presets", rename_all = "camelCase")]
    PutPresets { staging_id: String, presets: Vec<Value> },
    #[serde(rename = "pds_replace_add_presets", rename_all = "camelCase")]
    AddPresets { staging_id: String, presets: Vec<Value> },
    #[serde(rename = "pds_replace_add_characters", rename_all = "camelCase")]
    AddCharacters { staging_id: String, characters: Vec<Value> },
    #[serde(rename = "pds_replace_put_character_detail", rename_all = "camelCase")]
    PutCharacterDetail {
        staging_id: String,
        detail: Value,
        conversation_count: i64,
    },
    #[serde(rename = "pds_replace_put_conversation", rename_all = "camelCase")]
    PutConversation {
        staging_id: String,
        character_id: String,
        configured_index: i64,
        conversation: Value,
        message_count: i64,
        last_message_time: Option<Value>,
    },
    #[serde(rename = "pds_replace_add_conversation_messages", rename_all = "camelCase")]
    AddConversationMessages {
        staging_id: String,
        character_id: String,
        conversation_id: String,
        start: i64,
        messages: Vec<Value>,
    },
    #[serde(rename = "pds_replace_add_plugin_storage_values", rename_all = "camelCase")]
    AddPluginStorageValues {
        staging_id: String,
        values: Vec<super::PluginStorageValue>,
    },
    #[serde(rename = "pds_replace_add_plugin_storage", rename_all = "camelCase")]
    AddPluginStorage {
        staging_id: String,
        storage: serde_json::Map<String, Value>,
        meta: Option<serde_json::Map<String, Value>>,
    },
}

#[cfg(any(test, target_os = "android"))]
impl StagedReplaceRequest {
    pub(crate) fn apply(self, store: &mut PersistentStore) -> StoreResult<()> {
        match self {
            Self::PutRoot { staging_id, root, plugin_storage_values } => store
                .replace_put_root_with_plugin_storage(
                    &staging_id,
                    &root,
                    plugin_storage_values.as_deref(),
                ),
            Self::PutPresets { staging_id, presets } => {
                store.replace_put_presets(&staging_id, &presets)
            }
            Self::AddPresets { staging_id, presets } => {
                store.replace_add_presets(&staging_id, &presets)
            }
            Self::AddCharacters { staging_id, characters } => {
                store.replace_add_characters(&staging_id, &characters)
            }
            Self::PutCharacterDetail { staging_id, detail, conversation_count } => {
                store.replace_put_character_detail(&staging_id, &detail, conversation_count)
            }
            Self::PutConversation {
                staging_id,
                character_id,
                configured_index,
                conversation,
                message_count,
                last_message_time,
            } => store.replace_put_conversation(
                &staging_id,
                &character_id,
                configured_index,
                &conversation,
                message_count,
                last_message_time.as_ref(),
            ),
            Self::AddConversationMessages {
                staging_id,
                character_id,
                conversation_id,
                start,
                messages,
            } => store.replace_add_conversation_messages(
                &staging_id,
                &character_id,
                &conversation_id,
                start,
                &messages,
            ),
            Self::AddPluginStorageValues { staging_id, values } => {
                store.replace_add_plugin_storage_values(&staging_id, &values)
            }
            Self::AddPluginStorage { staging_id, storage, meta } => {
                store.replace_add_plugin_storage(&staging_id, &storage, meta.as_ref())
            }
        }
    }
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_asset_aliases(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    aliases: Vec<AssetAlias>,
) -> Result<(), StoreError> {
    logged("pds_replace_put_asset_aliases", with_store_mut(state, |store| {
        store.replace_put_asset_aliases(&staging_id, &aliases)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_asset_owner_heads(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    heads: Vec<AssetOwnerHead>,
) -> Result<(), StoreError> {
    logged("pds_replace_put_asset_owner_heads", with_store_mut(state, |store| {
        store.replace_put_asset_owner_heads(&staging_id, &heads)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_preserve_repositories(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    expected_revision: Option<i64>,
) -> Result<RevisionResult, StoreError> {
    logged("pds_replace_preserve_repositories", with_store_mut(state, |store| {
        store.replace_preserve_repositories(&staging_id, expected_revision)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_commit(
    app: AppHandle,
    staging_id: String,
    expected_revision: Option<i64>,
) -> Result<RevisionResult, StoreError> {
    logged("pds_replace_commit", commit_staged_replacement(&app, &staging_id, expected_revision))
}

#[tauri::command(async)]
pub(crate) fn pds_replace_abort(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
) -> Result<(), StoreError> {
    logged("pds_replace_abort", with_store_mut(state, |store| store.replace_abort(&staging_id)))
}

#[tauri::command(async)]
pub(crate) fn pds_materialize(
    state: State<'_, PersistentStoreState>,
    revision: Option<i64>,
) -> Result<Value, StoreError> {
    logged("pds_materialize", with_store(state, |store| store.materialize(revision)))
}

#[tauri::command(async)]
pub(crate) fn pds_acquire_revision(
    state: State<'_, PersistentStoreState>,
    revision: i64,
) -> Result<LeaseResult, StoreError> {
    logged("pds_acquire_revision", with_store_mut(state, |store| store.acquire_revision(revision)))
}

#[tauri::command(async)]
pub(crate) fn pds_release_revision(
    state: State<'_, PersistentStoreState>,
    lease: String,
) -> Result<(), StoreError> {
    logged("pds_release_revision", with_store_mut(state, |store| store.release_revision(&lease)))
}

#[tauri::command(async)]
pub(crate) fn pds_export_risu_save(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    lease: String,
    omit_account: bool,
) -> Result<ExportedRisuSave, StoreError> {
    logged("pds_export_risu_save", (|| {
        let account = crate::account_credential::export_account(&app, omit_account)
            .map_err(|message| StoreError::Validation { message })?;
        let operation_guard = state.admit_renderer_operation()?;
        let prepared = with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
            store.detach_risu_save_export(&lease)
        })?;
        let outcome = prepared.create_attached_export(omit_account, account.as_ref());
        let reattach = with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
            store.reattach_risu_save_export(prepared)
        });
        match (outcome, reattach) {
            (Ok(exported), Ok(())) => Ok(exported),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(reattach_error)) => Err(StoreError::Store {
                message: format!(
                    "{error}; failed to reattach revision lease after native export: {reattach_error}"
                ),
            }),
        }
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_export_risu_save_cleanup(
    state: State<'_, PersistentStoreState>,
    path: String,
) -> Result<(), StoreError> {
    logged("pds_export_risu_save_cleanup", with_store(state, |store| {
        store.cleanup_risu_save_export(Path::new(&path))
    }))
}

#[cfg(feature = "official-publication-upload-pilot")]
#[tauri::command]
pub(crate) async fn official_publication_upload_file(
    state: State<'_, PersistentStoreState>,
    request: OfficialPublicationUploadRequest,
) -> Result<OfficialPublicationUploadResult, OfficialPublicationUploadError> {
    crate::publication_upload::logged_upload_outcome("official_publication_upload_file", async {
        let (source, bytes) = with_store(state, |store| {
            store.open_risu_save_export_for_upload(Path::new(&request.path))
        })
        .map_err(|error| OfficialPublicationUploadError::Source {
            message: error.to_string(),
        })?;
        upload_open_file_attempt(request, source, bytes).await
    }.await)
}

#[cfg(feature = "native-kei-upload-pilot")]
#[tauri::command(async)]
pub(crate) async fn pds_kei_backup_upload(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    lease: String,
    url: String,
    expected_account_id: String,
    token: String,
) -> Result<KeiUploadResult, StoreError> {
    logged("pds_kei_backup_upload", async move {
        let account = crate::account_credential::export_account(&app, false)
            .map_err(|message| StoreError::Validation { message })?
            .ok_or_else(|| StoreError::Validation { message: "KEI account credential is unavailable".to_owned() })?;
        let operation = state.admit_renderer_operation()?;
        let prepared = with_store_mutex_mut_admitted(&state, &operation, |store| {
            store.prepare_kei_upload(&lease, &url, &expected_account_id, &token, &account)
        })?;
        // Keep admission with the native task even if its invoking renderer goes
        // away while serialization or detached-reader checkpointing is running.
        tauri::async_runtime::spawn(async move {
            let _operation = operation;
            prepared.upload().await
        })
        .await
        .map_err(|error| StoreError::Store {
            message: format!("failed to join KEI upload operation: {error}"),
        })?
    }.await)
}

#[tauri::command(async)]
pub(crate) fn pds_checkpoint(
    state: State<'_, PersistentStoreState>,
    mode: CheckpointMode,
) -> Result<(), StoreError> {
    logged("pds_checkpoint", with_store(state, |store| store.checkpoint(mode)))
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_create(
    state: State<'_, PersistentStoreState>,
    reason: String,
) -> Result<SnapshotCreated, StoreError> {
    logged("pds_snapshot_create", (|| {
        let _operation = state.admit_renderer_operation()?;
        let snapshot_operation =
            state
                .snapshot_operations
                .lock()
                .map_err(|error| StoreError::Store {
                    message: format!("persistent snapshot mutex poisoned: {error}"),
                })?;
        let started = std::time::Instant::now();
        let (mut archive, scratch, current_bytes, inventory) = {
            let mut store = state.store.lock().map_err(|error| StoreError::Store {
                message: format!("persistent store mutex poisoned: {error}"),
            })?;
            let store = store.as_mut().ok_or_else(|| StoreError::Validation {
                message: "persistent store has not been opened".to_owned(),
            })?;
            let archive = super::snapshot_archive::Archive::open(&store.snapshots_dir)?;
            let inventory = store.active_readers.defer_asset_inventory();
            let (scratch, current_bytes) =
                super::snapshot::capture_scratch(store, &archive)?;
            (archive, scratch, current_bytes, inventory)
        };
        let result = super::snapshot::archive_scratch(
            &mut archive,
            scratch,
            current_bytes,
            &reason,
            started,
        );
        drop(inventory);
        drop(archive);
        drop(snapshot_operation);
        result
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_list(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<SnapshotInfo>, StoreError> {
    #[cfg(target_os = "android")]
    {
        logged("pds_snapshot_list", with_snapshot_directory(&state, super::snapshot::list))
    }
    #[cfg(not(target_os = "android"))]
    {
        logged("pds_snapshot_list", with_store(state, PersistentStore::snapshot_list))
    }
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_delete(
    state: State<'_, PersistentStoreState>,
    id: String,
) -> Result<(), StoreError> {
    logged(
        "pds_snapshot_delete",
        with_store(state, |store| store.snapshot_delete(&id)),
    )
}

#[tauri::command(async)]
pub(crate) fn pds_storage_stats(
    state: State<'_, PersistentStoreState>,
) -> Result<PersistentStorageStats, StoreError> {
    logged(
        "pds_storage_stats",
        with_store(state, PersistentStore::storage_stats),
    )
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetGcMaintenanceResult {
    candidate_count: u64,
    candidate_bytes: u64,
    deleted_count: u64,
    deleted_bytes: u64,
    blockers: Vec<String>,
    /// What the cleanup looked at and decided, so a preview can be argued with rather than
    /// only believed. Bounded, because a large library holds more objects than a list can show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<super::AssetGcCandidateDetail>,
    /// Rows past the bound, counted rather than listed.
    #[serde(default, skip_serializing_if = "is_zero")]
    omitted: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// Rows one preview lists before it counts the rest.
const GC_DETAIL_LIMIT: usize = 500;

fn asset_gc_result(
    report: crate::asset_repository::migration_gc::AssetGcDryRunReport,
) -> AssetGcMaintenanceResult {
    AssetGcMaintenanceResult {
        candidate_count: report.potential_delete_hashes.len() as u64,
        candidate_bytes: report.potential_delete_bytes,
        deleted_count: report.deleted_hashes.len() as u64,
        deleted_bytes: report.deleted_bytes,
        blockers: report.blockers,
        candidates: Vec::new(),
        omitted: 0,
    }
}

#[tauri::command(async)]
pub(crate) fn pds_asset_gc_preview(
    state: State<'_, PersistentStoreState>,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    logged(
        "pds_asset_gc_preview",
        pds_asset_gc_preview_all(&state, &operation_guard),
    )
}

fn pds_asset_gc_preview_all(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let now = current_time_ms()?;
    // Preview roots are scoped to this invocation. Deletion always collects fresh roots.
    let preview = with_store_mutex_admitted(state, operation_guard, |store| {
        store.prepare_asset_gc_preview()
    })?;
    let mut cursor = None;
    let mut result = AssetGcMaintenanceResult {
        candidate_count: 0,
        candidate_bytes: 0,
        deleted_count: 0,
        deleted_bytes: 0,
        blockers: Vec::new(),
        candidates: Vec::new(),
        omitted: 0,
    };
    loop {
        let (page, details) = with_store_mutex_admitted(state, operation_guard, |store| {
            store.asset_gc_preview_page_detail(
                &preview,
                128,
                cursor.as_deref(),
                now,
                7 * 24 * 60 * 60 * 1_000,
            )
        })?;
        let page_result = asset_gc_result(page.report);
        result.candidate_count += page_result.candidate_count;
        result.candidate_bytes += page_result.candidate_bytes;
        result.blockers.extend(page_result.blockers);
        for detail in details {
            if result.candidates.len() < GC_DETAIL_LIMIT {
                result.candidates.push(detail);
            } else {
                result.omitted += 1;
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(result)
}

#[tauri::command(async)]
pub(crate) fn pds_asset_gc_execute(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    logged(
        "pds_asset_gc_execute",
        pds_asset_gc_execute_all(&state, &operation_guard).and_then(|result| {
            sweep_message_pages(
                file_admission(&app).as_ref(), &state, &operation_guard, current_time_ms()?, SweepBudget::UntilWrapped,
            )?;
            Ok(result)
        }),
    )
}

/// How many batches each store gets in one sweep call.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SweepBudget {
    OneBatch,
    UntilWrapped,
}

/// Message objects are swept only while no replacement or server sync holds
/// the library, so a refused or missing permit ends the pass until next time.
fn sweep_message_pages(
    admission: Option<&Arc<crate::native_file_jobs::admission::Admission>>,
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    now: i64,
    budget: SweepBudget,
) -> StoreResult<()> {
    let Some(admission) = admission else {
        return Ok(());
    };
    for target in [super::MessageObjectStore::Library, super::MessageObjectStore::Device] {
        loop {
            let Ok(_permit) = admission.file(false) else {
                return Ok(());
            };
            let sweep = with_store_mutex_mut_admitted(state, operation_guard, |store| {
                store.sweep_message_page_objects(target, now, super::MESSAGE_PAGE_SWEEP_LIMIT)
            })?;
            if sweep.wrapped || budget == SweepBudget::OneBatch {
                break;
            }
        }
    }
    Ok(())
}

fn pds_asset_gc_execute_all(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let now = current_time_ms()?;
    // This fresh command-local mark only filters candidates; deletion rechecks current roots.
    let marks = with_store_mutex_admitted(state, operation_guard, |store| {
        store.prepare_asset_gc_delete_marks()
    })?;
    let mut cursor = None;
    let mut library = None;
    let mut result = AssetGcMaintenanceResult {
        candidate_count: 0,
        candidate_bytes: 0,
        deleted_count: 0,
        deleted_bytes: 0,
        blockers: Vec::new(),
        candidates: Vec::new(),
        omitted: 0,
    };
    loop {
        let page = pds_asset_gc_execute_page(state, operation_guard, &marks, &mut library, cursor.as_deref(), now)?;
        let page_result = asset_gc_result(page.report);
        result.candidate_count += page_result.candidate_count;
        result.candidate_bytes += page_result.candidate_bytes;
        result.deleted_count += page_result.deleted_count;
        result.deleted_bytes += page_result.deleted_bytes;
        result.blockers.extend(page_result.blockers);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(result)
}

/// Candidates one cleanup page deletes.
const ASSET_GC_EXECUTE_PAGE: i64 = 1024;
#[cfg(test)]
thread_local! {
    static TEST_ASSET_GC_EXECUTE_PAGE: std::cell::Cell<i64> = const { std::cell::Cell::new(ASSET_GC_EXECUTE_PAGE) };
}
fn asset_gc_execute_page() -> i64 {
    #[cfg(test)]
    return TEST_ASSET_GC_EXECUTE_PAGE.with(std::cell::Cell::get);
    #[cfg(not(test))]
    ASSET_GC_EXECUTE_PAGE
}

fn pds_asset_gc_execute_page(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    marks: &crate::asset_repository::migration_gc::AssetGcMarks,
    library: &mut Option<super::AssetGcLibraryRoots>,
    cursor: Option<&str>,
    now: i64,
) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
    with_store_mutex_mut_admitted(state, operation_guard, |store| {
        store.asset_gc_delete_marked_page_reusing_library(marks, library, asset_gc_execute_page(), cursor, now, 7 * 24 * 60 * 60 * 1_000, |_| Ok(()))
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PluginStorageSource {
    owner: String,
    key: String,
}

#[tauri::command(async)]
pub(crate) fn pds_colliding_plugin_storage_keys(
    state: State<'_, PersistentStoreState>,
    owner: String,
    keys: Vec<String>,
) -> Result<Vec<String>, StoreError> {
    logged("pds_colliding_plugin_storage_keys", with_store(state, |store| {
        store.colliding_plugin_storage_keys(&owner, &keys)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_assign_plugin_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
    sources: Vec<PluginStorageSource>,
    collision: super::commit::AssignCollision,
    expected_revision: i64,
) -> Result<AssignedPluginStorage, StoreError> {
    logged("pds_assign_plugin_storage", (|| {
        let sources: Vec<(String, String)> = sources
            .into_iter()
            .map(|source| (source.owner, source.key))
            .collect();
        with_store_mut(state, |store| {
            store.assign_plugin_storage(&sources, &owner, collision, expected_revision)
        })
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_begin_plugin_claim_session(
    state: State<'_, PersistentStoreState>,
    owner: String,
    code_hash: String,
    runtime_instance: String,
) -> Result<Option<String>, StoreError> {
    logged("pds_begin_plugin_claim_session", with_store(state, |store| {
        store.begin_plugin_claim_session(&owner, &code_hash, &runtime_instance)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_claim_plugin_storage_value(
    state: State<'_, PersistentStoreState>,
    session_id: String,
    owner: String,
    code_hash: String,
    runtime_instance: String,
    key: String,
    expected_revision: i64,
) -> Result<ClaimedPluginValue, StoreError> {
    logged("pds_claim_plugin_storage_value", with_store_mut(state, |store| {
        store.claim_plugin_storage_value(
            &session_id,
            &owner,
            &code_hash,
            &runtime_instance,
            &key,
            expected_revision,
        )
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_close_plugin_claim_eligibility(
    state: State<'_, PersistentStoreState>,
) -> Result<(), StoreError> {
    logged("pds_close_plugin_claim_eligibility", with_store(state, |store| store.close_plugin_claim_eligibility()))
}

#[tauri::command(async)]
pub(crate) fn pds_close_plugin_claim_session(
    state: State<'_, PersistentStoreState>,
    session_id: String,
) -> Result<(), StoreError> {
    logged("pds_close_plugin_claim_session", with_store(state, |store| store.close_plugin_claim_session(&session_id)))
}

#[tauri::command(async)]
pub(crate) fn pds_hydrate_plugin_device_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
) -> Result<PluginDeviceHydration, StoreError> {
    logged("pds_hydrate_plugin_device_storage", with_store(state, |store| {
        store.device_store()?.hydrate_plugin_device_storage(&owner)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_device_value(
    state: State<'_, PersistentStoreState>,
    owner: String,
    space: String,
    key: String,
) -> Result<Option<PluginDeviceRead>, StoreError> {
    logged("pds_read_plugin_device_value", with_store(state, |store| {
        store
            .device_store()?
            .read_plugin_device_transfer(&owner, &space, &key)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_device_range(
    state: State<'_, PersistentStoreState>, owner: String, space: String, key: String, version: String, offset: u64,
) -> Result<PluginDeviceRange, StoreError> {
    logged("pds_read_plugin_device_range", with_store(state, |store| {
        store.device_store()?.read_plugin_device_range(&owner, &space, &key, &version, offset)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_open_plugin_device_write(
    state: State<'_, PersistentStoreState>, owner: String, space: String, key: String, encoded_length: u64,
) -> Result<super::device_store::plugin_values::sessions::OpenedWrite, StoreError> {
    logged("pds_open_plugin_device_write", with_store_mut(state, |store| {
        store.device_store_mut()?.open_plugin_device_write(&owner, &space, &key, encoded_length)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_append_plugin_device_write(
    state: State<'_, PersistentStoreState>, owner: String, session_id: String, offset: u64, part: String,
) -> Result<u64, StoreError> {
    logged("pds_append_plugin_device_write", with_store_mut(state, |store| {
        store.device_store_mut()?.append_plugin_device_write(&owner, &session_id, offset, &part)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_open_plugin_device_binary_write(app: AppHandle, state: State<'_, PersistentStoreState>, owner: String, key: String, byte_length: u64) -> Result<super::device_store::plugin_values::sessions::OpenedWrite, StoreError> {
    logged("pds_open_plugin_device_binary_write", with_store_mut(state, |store| {
        let device = store.device_store_mut()?;
        let opened = device.open_plugin_device_binary_write(&owner, &key, byte_length)?;
        let id = opened.session_id.clone();
        let receiver: std::sync::Arc<crate::asset_byte_transport::AndroidReceiver> = std::sync::Arc::new(move |offset, bytes| {
            with_store_mut(app.state::<PersistentStoreState>(), |store| store.device_store_mut()?.append_plugin_device_bytes(&id, offset, bytes)).map_err(|error| error.to_string())
        });
        let receiver = crate::asset_byte_transport::register_android_receiver(&opened.session_id, receiver).map_err(|error| {
            device.abort_plugin_device_write(&owner, &opened.session_id);
            invalid(&error)
        })?;
        device.attach_plugin_binary_receiver(&opened.session_id, receiver)?;
        Ok(opened)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_append_plugin_device_bytes(state: State<'_, PersistentStoreState>, upload_id: String, offset: u64, data: Vec<u8>) -> Result<u64, StoreError> {
    logged("pds_append_plugin_device_bytes", with_store_mut(state, |store| store.device_store_mut()?.append_plugin_device_bytes(&upload_id, offset, &data)))
}

#[tauri::command(async)]
pub(crate) fn pds_append_plugin_device_bytes_raw(state: State<'_, PersistentStoreState>, request: tauri::ipc::Request<'_>) -> Result<u64, StoreError> {
    logged("pds_append_plugin_device_bytes_raw", (|| {
        let (id, offset, bytes) = crate::asset_byte_transport::raw_part(&request).map_err(|_| invalid("Invalid plugin binary part"))?;
        with_store_mut(state, |store| store.device_store_mut()?.append_plugin_device_bytes(&id, offset, bytes))
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_device_bytes(state: State<'_, PersistentStoreState>, owner: String, space: String, key: String, version: String, offset: u64) -> Result<tauri::ipc::Response, StoreError> {
    logged("pds_read_plugin_device_bytes", with_store(state, |store| store.device_store()?.read_plugin_device_bytes(&owner, &space, &key, &version, offset)).map(tauri::ipc::Response::new))
}

#[tauri::command(async)]
pub(crate) fn pds_abort_plugin_device_write(
    state: State<'_, PersistentStoreState>, owner: String, session_id: String,
) -> Result<(), StoreError> {
    logged("pds_abort_plugin_device_write", with_store_mut(state, |store| {
        store.device_store_mut()?.abort_plugin_device_write(&owner, &session_id);
        Ok(())
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_device_keys(
    state: State<'_, PersistentStoreState>,
    owner: String,
    space: String,
) -> Result<Vec<String>, StoreError> {
    logged("pds_list_plugin_device_keys", with_store(state, |store| {
        store.device_store()?.list_plugin_device_keys(&owner, &space)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_device_storage(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<PluginDeviceListItem>, StoreError> {
    logged("pds_list_plugin_device_storage", with_store(state, |store| {
        store.device_store()?.list_plugin_device_storage()
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_plugin_device_sequence(
    state: State<'_, PersistentStoreState>, owner: String, space: String, key: String,
) -> Result<String, StoreError> {
    logged("pds_plugin_device_sequence", with_store(state, |store| store.device_store()?.plugin_device_sequence(&owner, &space, &key)))
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_device_values(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    owner: String,
    mutations: Vec<PluginDeviceMutation>,
    observations: Option<Vec<super::device_store::plugin_values::PluginDeviceObservation>>,
) -> Result<Vec<super::device_store::plugin_values::PluginDeviceObservation>, StoreError> {
    logged("pds_write_plugin_device_values", (|| {
        let (changed, observed) = with_store_mut(state, |store| {
            let device = store.device_store_mut()?;
            let before = device.revision()?;
            let observed = device.write_plugin_device_values_observed(&owner, &mutations, &observations.unwrap_or_default(), super::device_store::plugin_values::text_value::PART_BYTES as u64, super::device_store::plugin_values::text_value::REQUEST_BYTES)?;
            Ok((device.revision()? != before, observed))
        })?;
        if changed {
            crate::server_sync::events::notify_device_changed(&app);
        }
        Ok(observed)
    })())
}

#[tauri::command(async)]
pub(crate) fn pds_get_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
) -> Result<Option<Value>, StoreError> {
    logged("pds_get_device_setting", with_store(state, |store| store.device_store()?.read_setting(&key)))
}

/// A null value removes the setting.
#[tauri::command(async)]
pub(crate) fn pds_set_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
    value: Option<Value>,
) -> Result<(), StoreError> {
    logged("pds_set_device_setting", with_store(state, |store| {
        let device = store.device_store()?;
        match value {
            Some(value) => device.write_setting(&key, &value),
            None => device.remove_setting(&key),
        }
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_patch_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
    entries: serde_json::Map<String, Value>,
) -> Result<(), StoreError> {
    logged("pds_patch_device_setting", with_store_mut(state, |store| {
        store.device_store_mut()?.patch_setting(&key, &entries)
    }))
}

/// Answers in request order so one call can fill the whole boot cache.
#[tauri::command(async)]
pub(crate) fn pds_read_device_settings(
    state: State<'_, PersistentStoreState>,
    keys: Vec<String>,
) -> Result<Vec<Option<Value>>, StoreError> {
    logged("pds_read_device_settings", with_store(state, |store| store.device_store()?.read_settings(&keys)))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginPermissionRow {
    code_hash: String,
    permission: String,
    granted: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginPermissionGrantRow {
    plugin_name: String,
    permission: String,
    last_grant_at: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginPermissionState {
    permissions: Vec<PluginPermissionRow>,
    grants: Vec<PluginPermissionGrantRow>,
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_permissions(
    state: State<'_, PersistentStoreState>,
) -> Result<PluginPermissionState, StoreError> {
    logged("pds_read_plugin_permissions", with_store(state, |store| {
        let device = store.device_store()?;
        Ok(PluginPermissionState {
            permissions: device
                .read_plugin_permissions()?
                .into_iter()
                .map(|row| PluginPermissionRow {
                    code_hash: row.code_hash,
                    permission: row.permission,
                    granted: row.granted,
                })
                .collect(),
            grants: device
                .read_plugin_permission_grants()?
                .into_iter()
                .map(|row| PluginPermissionGrantRow {
                    plugin_name: row.plugin_name,
                    permission: row.permission,
                    last_grant_at: row.last_grant_at,
                })
                .collect(),
        })
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_permission(
    state: State<'_, PersistentStoreState>,
    code_hash: String,
    permission: String,
    granted: bool,
) -> Result<(), StoreError> {
    logged("pds_write_plugin_permission", with_store(state, |store| {
        store
            .device_store()?
            .write_plugin_permission(&code_hash, &permission, granted)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_permission_grant(
    state: State<'_, PersistentStoreState>,
    plugin_name: String,
    permission: String,
    last_grant_at: i64,
) -> Result<(), StoreError> {
    logged("pds_write_plugin_permission_grant", with_store(state, |store| {
        store.device_store()?.write_plugin_permission_grant(
            &plugin_name,
            &permission,
            last_grant_at,
        )
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_clear_plugin_permissions(
    state: State<'_, PersistentStoreState>,
) -> Result<(), StoreError> {
    logged("pds_clear_plugin_permissions", with_store_mut(state, |store| {
        store.device_store_mut()?.clear_plugin_permissions()
    }))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SectionParticipationRow {
    section: String,
    participating: bool,
}

fn read_section_participation(
    state: &PersistentStoreState,
) -> Result<Vec<SectionParticipationRow>, StoreError> {
    with_store_mutex(state, |store| {
        let device = store.device_store()?;
        CHOOSABLE_SECTIONS
            .into_iter()
            .map(|section| {
                Ok(SectionParticipationRow {
                    section: section.as_str().to_owned(),
                    participating: device.section_state(section)?.participating,
                })
            })
            .collect()
    })
}

fn set_section_participation<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &PersistentStoreState,
    section: &str,
    participating: bool,
) -> Result<(), StoreError> {
    let Some(section) = section_from_id(section) else {
        return Err(StoreError::Validation {
            message: "unknown device section".to_owned(),
        });
    };
    let changed = with_store_mutex_mut(state, |store| {
        let device = store.device_store_mut()?;
        let before = device.section_state(section)?.participating;
        device.set_section_participating(section, participating)?;
        Ok(before != participating)
    })?;
    if changed { crate::server_sync::events::notify_device_changed(app); }
    Ok(())
}

/// Which sections this device currently exchanges with a remote.
#[tauri::command(async)]
pub(crate) fn pds_read_section_participation(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<SectionParticipationRow>, StoreError> {
    logged("pds_read_section_participation", read_section_participation(&state))
}

#[tauri::command(async)]
pub(crate) fn pds_set_section_participating(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    section: String,
    participating: bool,
) -> Result<(), StoreError> {
    logged("pds_set_section_participating", set_section_participation(&app, &state, &section, participating))
}

#[cfg(test)]
mod tests {
    mod asset_gc_performance;
    mod snapshot_lock;

    use super::*;
    #[test]
    fn admitted_external_asset_publication_allows_global_commits_and_rechecks_ack_authority() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            use crate::external_storage::{contract::*, lww_tests::{CycleFixture, HeldAssetTransfer, small_asset}};
            for switch_authority in [false, true] {
                let mut fixture = CycleFixture::new();
                let original_hash = small_asset(&mut fixture.a, "synthetic-overlap", &[51; 64 * 1024]);
                let state = PersistentStoreState::with_test_store(fixture.a);
                let operation = state.admit_renderer_operation().unwrap();
                let mut job_store = state.open_admitted_native_job_store(&operation).unwrap();
                let writer = job_store.lww_clock_state().unwrap().writer_id;
                let original = job_store.lww_read_outbox(0.into(), 100).unwrap().entries;
                let held = HeldAssetTransfer::new(fixture.provider.clone());
                fixture.sender.provider = held.clone();
                let target = fixture.sender.target_scope();
                let cancel = Cancellation::default();
                let ordinary_ran = std::cell::Cell::new(false);
                let publication = async {
                    let result = fixture.sender.publish(&mut job_store, 0.into(), &[], &cancel).await;
                    if !ordinary_ran.get() {
                        return Err(result.err().unwrap_or_else(|| ProviderError::new(ErrorKind::Corrupt)));
                    }
                    Ok(result)
                };
                let ordinary_write = async {
                    held.entered.notified().await;
                    let local_operation = state.admit_renderer_operation().unwrap();
                    with_store_mutex_mut_admitted(&state, &local_operation, |store| {
                        if switch_authority {
                            let current = store.lww_binding_state()?;
                            let next = super::super::sync_selection::SyncTarget::External("synthetic-other".into());
                            let inspection = store.register_lww_binding_inspection(current.target_authority,
                                &next, "synthetic-other-target", "synthetic-other-library")?;
                            store.switch_lww_binding(&super::super::sync_selection::SwitchBindingRequest {
                                initial_publication: false,
                                header: super::super::lww::Header { binding_authority: current.target_authority, request_id: "overlap-switch".into() },
                                expected_selection_epoch: current.selection_epoch, target: next, inspection_id: Some(inspection),
                            })?;
                        } else {
                            small_asset(store, "synthetic-overlap", &[52; 64 * 1024]);
                            held.fail_pack_after.store(2, std::sync::atomic::Ordering::SeqCst);
                        }
                        Ok(())
                    }).unwrap();
                    {
                        let gate = state.renderer_gate.state.lock().unwrap();
                        assert!(!gate.maintenance_active);
                        assert_eq!(gate.operations, 2);
                    }
                    let captured = state.open_admitted_native_job_store(&operation).unwrap();
                    assert!(crate::asset_repository::job_pins::collect_durable_cas_job_roots(captured.repository_root())
                        .object_hashes.contains(&original_hash));
                    ordinary_ran.set(true);
                    held.resume.notify_one();
                    Ok::<_, ProviderError>(())
                };
                let (result, ()) = tokio::try_join!(publication, ordinary_write).unwrap();
                assert!(ordinary_ran.get());
                assert!(result.is_err());
                let current = state.open_admitted_native_job_store(&operation).unwrap();
                if switch_authority {
                    assert_eq!(current.external_lww_next_sequence(&target, &writer).unwrap(), 1);
                    assert_eq!(fixture.provider.uploaded_ids().iter().filter(|id| parse_segment_object_id(id).is_ok()).count(), 0);
                    assert_eq!(current.external_lww_pending(&target, &writer).unwrap().unwrap().0.entries, original);
                } else {
                    assert_eq!(current.external_lww_next_sequence(&target, &writer).unwrap(), 2);
                    let remaining = current.lww_read_outbox(0.into(), 100).unwrap().entries;
                    assert_eq!(remaining.len(), 1);
                    assert_eq!(remaining[0].key, original[0].key);
                    assert_ne!(remaining[0].version, original[0].version);
                    assert_ne!(remaining[0].stamp, original[0].stamp);
                    assert_eq!(fixture.provider.uploaded_ids().iter().filter(|id| parse_segment_object_id(id).is_ok()).count(), 1);
                }
                drop(job_store); drop(current); drop(operation);
                assert!(state.try_acquire_renderer_maintenance().unwrap().is_some());
            }
        });
    }
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    /// The intent recovery every mutable command runs first takes no device-store
    /// write when nothing settled, so another connection's long write does not fail it.
    #[test]
    fn a_held_device_store_write_does_not_fail_a_change_cursor_commit() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::with_test_store(PersistentStore::open(directory.path()).unwrap());
        let holder = rusqlite::Connection::open(directory.path().join("persistent").join("device.sqlite")).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let committed = with_store_mutex_mut(&state, |store| {
            let revision = store.revision()?;
            store.commit_working_set_change_cursor(revision)
        });
        holder.execute_batch("ROLLBACK").unwrap();
        committed.unwrap();
    }

    #[test]
    fn cleanup_maintenance_timeout_preserves_admission_for_retry() {
        let state = PersistentStoreState::default();
        let operation = state.admit_renderer_operation().unwrap();
        assert!(state.acquire_cleanup_maintenance(std::time::Duration::from_millis(1)).is_err());
        assert!(state.admit_renderer_operation().is_ok());
        drop(operation);
        let maintenance = state.acquire_cleanup_maintenance(std::time::Duration::from_secs(1)).unwrap();
        assert!(state.admit_renderer_operation().is_err());
        drop(maintenance);
        assert!(state.admit_renderer_operation().is_ok());
    }

    #[test]
    fn an_operation_dispatched_before_maintenance_or_a_reset_is_refused_after_it() {
        let state = PersistentStoreState::default();
        let dispatched = state.renderer_dispatch_epoch().unwrap();
        assert!(state.admit_renderer_operation_dispatched_at(dispatched).is_ok());

        drop(state.acquire_raw_capture().unwrap());
        assert!(state.admit_renderer_operation_dispatched_at(dispatched).is_err());
        let after_capture = state.renderer_dispatch_epoch().unwrap();
        assert!(state.admit_renderer_operation_dispatched_at(after_capture).is_ok());

        drop(state.acquire_cleanup_maintenance(std::time::Duration::from_secs(1)).unwrap());
        assert!(state.admit_renderer_operation_dispatched_at(after_capture).is_err());

        let before_reset = state.renderer_dispatch_epoch().unwrap();
        state.reset_renderer_session().unwrap();
        assert!(state.admit_renderer_operation_dispatched_at(before_reset).is_err());

        let maintenance = state.acquire_raw_capture().unwrap();
        let during = state.renderer_dispatch_epoch().unwrap();
        assert!(state.admit_renderer_operation_dispatched_at(during).is_err());
        state.reset_renderer_session().unwrap();
        drop(maintenance);
        assert!(state.admit_renderer_operation_dispatched_at(during).is_err());
        assert!(state.admit_renderer_operation_dispatched_at(state.renderer_dispatch_epoch().unwrap()).is_ok());
        assert!(state.admit_renderer_operation().is_ok());
    }

    #[test]
    fn message_object_sweep_waits_for_file_admission_and_covers_both_stores() {
        use crate::native_file_jobs::admission::Admission;
        let directory = tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let orphan = b"synthetic-orphan-object".to_vec();
        let hash = risunest_sync_wire::hash(&orphan);
        store.lww_put_object(&hash, &orphan).unwrap();
        super::super::message_pages::put_object(store.device_store().unwrap().connection(), &hash, &orphan).unwrap();
        let state = PersistentStoreState::with_test_store(store);
        let marks = || -> (i64, i64) {
            let guard = state.store.lock().unwrap();
            let store = guard.as_ref().unwrap();
            let count = "SELECT count(*) FROM message_page_object_marks";
            (
                store.connection.query_row(count, [], |row| row.get(0)).unwrap(),
                store.device_store().unwrap().connection().query_row(count, [], |row| row.get(0)).unwrap(),
            )
        };
        let operation = state.admit_renderer_operation().unwrap();
        let admission = Arc::new(Admission::default());
        sweep_message_pages(None, &state, &operation, 1_000, SweepBudget::UntilWrapped).unwrap();
        assert_eq!(marks(), (0, 0));
        let exclusive = admission.file(true).unwrap();
        sweep_message_pages(Some(&admission), &state, &operation, 1_000, SweepBudget::UntilWrapped).unwrap();
        assert_eq!(marks(), (0, 0));
        drop(exclusive);
        let server = admission.server().unwrap();
        sweep_message_pages(Some(&admission), &state, &operation, 1_000, SweepBudget::UntilWrapped).unwrap();
        assert_eq!(marks(), (0, 0));
        drop(server);
        sweep_message_pages(Some(&admission), &state, &operation, 1_000, SweepBudget::UntilWrapped).unwrap();
        assert_eq!(marks(), (1, 1));
        drop(operation);
        let maintenance = state.acquire_cleanup_maintenance(std::time::Duration::from_secs(1)).unwrap();
        assert!(state.try_admit_renderer_operation().unwrap().is_none());
        drop(maintenance);
        assert!(state.try_admit_renderer_operation().unwrap().is_some());
    }

    #[test]
    fn a_periodic_sweep_visits_one_batch_per_store_and_resumes_from_the_cursor() {
        use crate::native_file_jobs::admission::Admission;
        let limit = super::super::MESSAGE_PAGE_SWEEP_LIMIT;
        let open = || {
            let directory = tempdir().unwrap();
            let store = PersistentStore::open(directory.path()).unwrap();
            for db in [&store.connection, store.device_store().unwrap().connection()] {
                let tx = db.unchecked_transaction().unwrap();
                for index in 0..=limit {
                    let body = format!("synthetic-orphan-object-{index}").into_bytes();
                    super::super::message_pages::put_object(&tx, &risunest_sync_wire::hash(&body), &body).unwrap();
                }
                tx.commit().unwrap();
            }
            (directory, PersistentStoreState::with_test_store(store))
        };
        let count = |state: &PersistentStoreState, table: &str| -> (usize, usize) {
            let guard = state.store.lock().unwrap();
            let store = guard.as_ref().unwrap();
            let sql = format!("SELECT count(*) FROM {table}");
            let read = |db: &rusqlite::Connection| db.query_row(&sql, [], |row| row.get::<_, i64>(0)).unwrap() as usize;
            (read(&store.connection), read(store.device_store().unwrap().connection()))
        };
        let admission = Arc::new(Admission::default());

        let (_periodic_directory, periodic) = open();
        assert_eq!(count(&periodic, "message_page_objects"), (limit + 1, limit + 1));
        let operation = periodic.admit_renderer_operation().unwrap();
        sweep_message_pages(Some(&admission), &periodic, &operation, 1_000, SweepBudget::OneBatch).unwrap();
        assert_eq!(count(&periodic, "message_page_object_marks"), (limit, limit), "a periodic sweep ran more than one batch");
        assert_eq!(count(&periodic, "message_page_sweep_cursor"), (1, 1));
        sweep_message_pages(Some(&admission), &periodic, &operation, 1_000, SweepBudget::OneBatch).unwrap();
        assert_eq!(
            count(&periodic, "message_page_object_marks"),
            (limit + 1, limit + 1),
            "the next periodic sweep did not continue from the cursor",
        );
        assert_eq!(count(&periodic, "message_page_sweep_cursor"), (0, 0));

        let (_cleanup_directory, cleanup) = open();
        let operation = cleanup.admit_renderer_operation().unwrap();
        sweep_message_pages(Some(&admission), &cleanup, &operation, 1_000, SweepBudget::UntilWrapped).unwrap();
        assert_eq!(count(&cleanup, "message_page_object_marks"), (limit + 1, limit + 1));
    }

    #[test]
    fn a_sweep_pass_reads_message_object_roots_once_until_a_reference_changes() {
        use super::super::MessageObjectStore::{Device, Library};
        let directory = tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let mut hashes = Vec::new();
        for db in [&store.connection, store.device_store().unwrap().connection()] {
            let tx = db.unchecked_transaction().unwrap();
            for index in 0..6 {
                let body = format!("synthetic-orphan-object-{index}").into_bytes();
                let hash = risunest_sync_wire::hash(&body);
                super::super::message_pages::put_object(&tx, &hash, &body).unwrap();
                hashes.push(hash);
            }
            tx.commit().unwrap();
        }
        hashes.sort();
        hashes.dedup();
        let marked = |store: &PersistentStore, hash: &str| -> bool {
            store.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM message_page_object_marks WHERE hash=?1)", [hash], |row| row.get(0),
            ).unwrap()
        };
        let batch = |store: &mut PersistentStore, target| store.sweep_message_page_objects(target, 1_000, 2).unwrap().wrapped;
        let pass = |store: &mut PersistentStore, target| {
            let mut batches = 1;
            while !batch(store, target) { batches += 1; }
            batches
        };
        let reference = |db: &rusqlite::Connection, capture: &str, hash: &str| {
            db.execute("INSERT INTO external_storage_capture_files VALUES(?1,'synthetic-catalog',?2)", [capture, hash]).unwrap();
        };

        for target in [Library, Device] {
            let computed = store.message_object_roots_computed();
            assert!(pass(&mut store, target) >= 2);
            assert_eq!(store.message_object_roots_computed() - computed, 1, "one pass read the roots more than once");
        }
        assert!(hashes.iter().all(|hash| marked(&store, hash)));
        let computed = store.message_object_roots_computed();
        assert!(!batch(&mut store, Library));
        assert!(!batch(&mut store, Device));
        assert_eq!(store.message_object_roots_computed(), computed, "the next single batch read the roots again");

        reference(&store.connection, "synthetic-capture-own", &hashes[5]);
        pass(&mut store, Library);
        assert_eq!(store.message_object_roots_computed() - computed, 1);
        assert!(!marked(&store, &hashes[5]), "a reference added between batches was not seen");

        let computed = store.message_object_roots_computed();
        let other = rusqlite::Connection::open(&store.database_path).unwrap();
        reference(&other, "synthetic-capture-other", &hashes[3]);
        drop(other);
        pass(&mut store, Library);
        assert_eq!(store.message_object_roots_computed() - computed, 1);
        assert!(!marked(&store, &hashes[3]), "a reference another connection committed was not seen");

        let computed = store.message_object_roots_computed();
        let lease = store.acquire_revision(store.revision().unwrap()).unwrap();
        assert!(!batch(&mut store, Library));
        assert!(!batch(&mut store, Library));
        assert_eq!(store.message_object_roots_computed() - computed, 1);
        store.release_revision(&lease.lease).unwrap();

        let computed = store.message_object_roots_computed();
        for target in [Library, Device] { pass(&mut store, target); }
        assert_eq!(store.message_object_roots_computed() - computed, 2, "lease release must invalidate both targets");

        let value = risunest_sync_wire::unit::UnitValue::object(
            risunest_sync_wire::descriptor::RecordDescriptor::content(hashes[0].clone()),
        ).unwrap();
        let device = store.device_store().unwrap().connection();
        device.execute("INSERT INTO lww_intents VALUES('root-cache-device','0','{}',?1,'unused',0)",
            [serde_json::json!({"pending": value}).to_string()]).unwrap();
        let computed = store.message_object_roots_computed();
        for target in [Library, Device] { pass(&mut store, target); }
        assert_eq!(store.message_object_roots_computed() - computed, 2, "device references must invalidate both targets");
        for db in [&store.connection, store.device_store().unwrap().connection()] {
            assert!(!db.query_row("SELECT EXISTS(SELECT 1 FROM message_page_object_marks WHERE hash=?1)",
                [&hashes[0]], |row| row.get::<_, bool>(0)).unwrap());
        }
        let other = rusqlite::Connection::open(store.device_store().unwrap().connection().path().unwrap()).unwrap();
        other.execute("DELETE FROM lww_intents WHERE request_id='root-cache-device'", []).unwrap();
        drop(other);
        let computed = store.message_object_roots_computed();
        for target in [Library, Device] { pass(&mut store, target); }
        assert_eq!(store.message_object_roots_computed() - computed, 2, "another device connection must invalidate both targets");
        for db in [&store.connection, store.device_store().unwrap().connection()] {
            assert!(db.query_row("SELECT EXISTS(SELECT 1 FROM message_page_object_marks WHERE hash=?1)",
                [&hashes[0]], |row| row.get::<_, bool>(0)).unwrap());
        }
        let computed = store.message_object_roots_computed();
        for target in [Library, Device] { pass(&mut store, target); }
        assert_eq!(store.message_object_roots_computed(), computed);
        store.invalidate_message_object_roots();
        for target in [Library, Device] { pass(&mut store, target); }
        assert_eq!(store.message_object_roots_computed() - computed, 2, "explicit invalidation must clear both targets");
    }

    #[test]
    fn archive_operation_cancellation_reaches_the_active_operation_and_is_released() {
        let state = PersistentStoreState::default();
        let operation = state
            .begin_archive_operation("archive-operation".to_owned())
            .expect("register archive operation");

        assert!(!operation.is_cancelled());
        assert!(state
            .cancel_archive_operation("archive-operation")
            .expect("cancel archive operation"));
        assert!(operation.is_cancelled());
        drop(operation);
        assert!(!state
            .cancel_archive_operation("archive-operation")
            .expect("completed operation was released"));
    }

    #[test]
    fn message_root_caches_spill_to_owned_files_and_remove_them_on_invalidation() {
        let directory = tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let tx = store.connection.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO external_storage_capture_files(capture_id,catalog_path,file_hash) VALUES(?1,'synthetic-catalog',?2)").unwrap();
            for index in 0..12_000 {
                insert.execute(rusqlite::params![format!("scratch-roots-{index}"), format!("{index:064x}")]).unwrap();
            }
        }
        tx.commit().unwrap();
        for target in [super::super::MessageObjectStore::Library, super::super::MessageObjectStore::Device] {
            store.sweep_message_page_objects(target, 1_000, 1).unwrap();
        }
        let paths = [&store.message_object_roots.library, &store.message_object_roots.device]
            .map(|cache| cache.as_ref().unwrap().2.path().to_path_buf());
        for path in &paths {
            let file = path.join("roots.sqlite");
            assert!(std::fs::metadata(&file).unwrap().len() > 512 * 1024);
            let db = rusqlite::Connection::open_with_flags(&file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert!(db.query_row("SELECT count(*) FROM roots", [], |row| row.get::<_, i64>(0)).unwrap() >= 12_000);
        }
        let computed = store.message_object_roots_computed();
        for target in [super::super::MessageObjectStore::Library, super::super::MessageObjectStore::Device] {
            store.sweep_message_page_objects(target, 1_000, 1).unwrap();
        }
        assert_eq!(store.message_object_roots_computed(), computed);
        store.invalidate_message_object_roots();
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn snapshot_directory_operation_releases_live_store_but_retains_renderer_admission() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        assert!(with_snapshot_directory(&state, super::super::snapshot::list).is_err());
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        with_snapshot_directory(&state, |path| {
            assert_eq!(path, directory.path().join("persistent/snapshots"));
            assert!(state.store.try_lock().is_ok());
            assert_eq!(state.renderer_gate.state.lock().unwrap().operations, 1);
            let archive = super::super::snapshot_archive::Archive::open(path)?;
            assert!(archive.list()?.is_empty());
            with_store_mutex(&state, |store| {
                store.set_app_kv("device-backup-commit:synthetic", &json!(1))
            })
        })
        .unwrap();
        assert_eq!(state.renderer_gate.state.lock().unwrap().operations, 0);
        let maintenance = state.acquire_device_maintenance().unwrap();
        assert!(with_snapshot_directory(&state, super::super::snapshot::list).is_err());
        drop(maintenance);
    }

    #[test]
    fn device_maintenance_blocks_renderer_writers_and_reopens_restored_store() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        with_store_mutex(&state, |store| {
            store.set_app_kv("device-backup-commit:synthetic", &json!(1))
        })
        .unwrap();

        state.with_status_job_store(|store| store.connection.execute_batch("CREATE TEMP TABLE synthetic_status_connection (value INTEGER)")).unwrap().unwrap();
        assert!(state.status_store.lock().unwrap().is_some());
        let maintenance = state.acquire_device_maintenance().unwrap();
        assert!(state.status_store.lock().unwrap().is_none());
        assert!(state.store.lock().unwrap().is_none());
        assert!(state.admit_renderer_operation().is_err());
        assert!(state.acquire_device_maintenance().is_err());
        state
            .reset_renderer_session()
            .expect("active maintenance already owns renderer cleanup");
        assert!(open_renderer_persistent_store(&state, directory.path()).is_err());
        assert!(
            with_store_mutex(&state, |store| {
                store.set_app_kv("device-backup-commit:synthetic", &json!(2))
            })
            .is_err()
        );
        assert!(with_store_mutex_mut(&state, |store| store.replace_begin()).is_err());

        // Only the maintenance worker's independent store can write while the
        // renderer fence is held. A new renderer observes the completed result.
        let native = PersistentStore::open(directory.path()).unwrap();
        native
            .set_app_kv("device-backup-commit:synthetic", &json!(3))
            .unwrap();
        drop(native);
        drop(maintenance);
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        assert_eq!(
            with_store_mutex(&state, |store| store
                .get_app_kv("device-backup-commit:synthetic"))
            .unwrap(),
            Some(json!(3))
        );
    }

    #[test]
    fn raw_capture_gate_keeps_the_open_connection_and_source_files_unchanged() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        let database = directory.path().join("persistent/persistent.sqlite");
        let before = fs::read(&database).unwrap();
        let sidecars_before = ["persistent.sqlite-wal", "persistent.sqlite-shm"]
            .map(|name| directory.path().join("persistent").join(name).exists());

        let capture = state.acquire_raw_capture().unwrap();
        assert!(state.store.lock().unwrap().is_some());
        assert!(state.admit_renderer_operation().is_err());
        assert_eq!(fs::read(&database).unwrap(), before);
        assert_eq!(
            ["persistent.sqlite-wal", "persistent.sqlite-shm"]
                .map(|name| directory.path().join("persistent").join(name).exists()),
            sidecars_before,
        );
        drop(capture);

        assert!(state.store.lock().unwrap().is_some());
        drop(state.admit_renderer_operation().unwrap());
    }

    #[test]
    fn device_recovery_fence_prevents_first_renderer_open() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        let maintenance = state.acquire_device_maintenance().unwrap();
        for _ in 0..2 {
            assert!(open_renderer_persistent_store(&state, directory.path()).is_err());
            assert!(state.admit_renderer_operation().is_err());
            assert!(!directory.path().join("persistent").exists());
        }
        drop(maintenance);
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        assert!(directory.path().join("persistent").is_dir());
    }

    #[test]
    fn device_maintenance_drains_admitted_writes_before_closing_store() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let directory = tempdir().unwrap();
        let state = Arc::new(PersistentStoreState::default());
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_state = Arc::clone(&state);
        let writer = thread::spawn(move || {
            with_store_mutex_mut(&writer_state, |store| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                store.set_app_kv("device-backup-commit:drained-writer", &json!(true))
            })
            .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let maintenance_state = Arc::clone(&state);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let maintainer = thread::spawn(move || {
            acquired_tx
                .send(maintenance_state.acquire_device_maintenance().unwrap())
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.renderer_gate.state.lock().unwrap().maintenance_active {
            assert!(
                Instant::now() < deadline,
                "maintenance admission did not close"
            );
            thread::yield_now();
        }
        assert!(state.admit_renderer_operation().is_err());
        assert!(matches!(
            acquired_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        let maintenance = acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        maintainer.join().unwrap();
        assert!(state.store.lock().unwrap().is_none());
        let native = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(
            native.get_app_kv("device-backup-commit:drained-writer").unwrap(),
            Some(json!(true))
        );
        drop(native);
        drop(maintenance);
    }

    #[test]
    fn admitted_operation_can_finish_after_maintenance_closes_new_admission() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant};

        let directory = tempdir().unwrap();
        let state = Arc::new(PersistentStoreState::default());
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        let operation_guard = state.admit_renderer_operation().unwrap();
        let maintenance_state = Arc::clone(&state);
        let (maintenance_tx, maintenance_rx) = mpsc::channel();
        let maintainer = thread::spawn(move || {
            maintenance_tx
                .send(maintenance_state.acquire_device_maintenance().unwrap())
                .unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.renderer_gate.state.lock().unwrap().maintenance_active {
            assert!(
                Instant::now() < deadline,
                "maintenance admission did not close"
            );
            thread::yield_now();
        }

        assert!(with_store_mutex_mut(&state, |_| Ok(())).is_err());
        with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
            store.set_app_kv("device-backup-commit:admitted-operation", &json!(true))
        })
        .expect("existing permit must finish its store work");
        assert!(matches!(
            maintenance_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(operation_guard);
        let maintenance = maintenance_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        maintainer.join().unwrap();
        let native = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(
            native.get_app_kv("device-backup-commit:admitted-operation").unwrap(),
            Some(json!(true))
        );
        drop(native);
        drop(maintenance);
    }

    #[test]
    fn maintenance_cancels_a_registered_long_operation_instead_of_waiting_for_it() {
        use std::thread;
        use std::time::{Duration, Instant};

        let state = Arc::new(PersistentStoreState::default());
        let operation_guard = state.admit_renderer_operation().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        state.cancel_on_renderer_maintenance(&cancel).unwrap();
        let operation = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !cancel.load(Ordering::Acquire) {
                if Instant::now() >= deadline { return false; }
                thread::yield_now();
            }
            drop(operation_guard);
            true
        });
        let maintenance = state.acquire_device_maintenance().unwrap();
        assert!(operation.join().unwrap(), "maintenance waited for the operation instead of cancelling it");
        let late = Arc::new(AtomicBool::new(false));
        state.cancel_on_renderer_maintenance(&late).unwrap();
        assert!(late.load(Ordering::Acquire));
        drop(maintenance);
    }

    #[test]
    fn failed_maintenance_acquisition_reopens_renderer_admission() {
        use std::thread;

        let state = Arc::new(PersistentStoreState::default());
        let poison_state = Arc::clone(&state);
        assert!(thread::spawn(move || {
            let _store = poison_state.store.lock().unwrap();
            panic!("synthetic store mutex poison");
        })
        .join()
        .is_err());

        assert!(state.acquire_device_maintenance().is_err());
        assert!(!state.renderer_gate.state.lock().unwrap().maintenance_active);
        drop(
            state
                .admit_renderer_operation()
                .expect("failed maintenance must reopen admission"),
        );
    }

    #[test]
    fn renderer_session_reset_releases_leases_without_closing_the_store() {
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        open_renderer_persistent_store(&state, directory.path()).unwrap();
        let lease = with_store_mutex_mut(&state, |store| store.acquire_revision(0)).unwrap();

        state.reset_renderer_session().unwrap();

        with_store_mutex(&state, |store| {
            assert_eq!(store.active_readers.active_count(), 0);
            assert!(matches!(
                store.read_root(Some(&lease.lease)),
                Err(StoreError::SnapshotReleased)
            ));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn task4_storage_command_errors_are_logged_masked_without_changing_returned_categories() {
        let cases = [
            (
                "pds_snapshot_delete",
                StoreError::Validation {
                    message: "snapshot-delete-marker Authorization: Bearer fixture-snapshot-secret"
                        .to_owned(),
                },
                "validation",
            ),
            (
                "pds_storage_stats",
                StoreError::Store {
                    message: "storage-stats-marker Authorization: Bearer fixture-stats-secret"
                        .to_owned(),
                },
                "store-error",
            ),
            (
                "pds_asset_gc_preview",
                StoreError::Store {
                    message: "gc-preview-marker Authorization: Bearer fixture-preview-secret"
                        .to_owned(),
                },
                "store-error",
            ),
            (
                "pds_asset_gc_execute",
                StoreError::Store {
                    message: "gc-execute-marker Authorization: Bearer fixture-execute-secret"
                        .to_owned(),
                },
                "store-error",
            ),
        ];

        for (command, error, expected_code) in cases {
            let returned = logged::<(), _>(command, Err(error))
                .expect_err("storage command error must be returned");
            let returned_json = serde_json::to_value(&returned).expect("serialize returned error");
            assert_eq!(returned_json["code"], expected_code);
            assert!(returned_json["message"]
                .as_str()
                .expect("returned error keeps its detail")
                .contains("fixture-"));

            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.contains(command))
                .expect("storage command error reaches the native log");
            assert_eq!(entry.level, "error");
            assert_eq!(entry.target, "native-command");
            assert!(entry.message.starts_with(&format!(
                "{command} failed: code={expected_code} cause="
            )));
            assert!(entry.message.contains("Authorization: ***"));
            assert!(!entry.message.contains("fixture-"));
        }
    }

    #[test]
    fn routine_storage_refusals_are_warnings_without_their_detail() {
        for (command, error) in [
            (
                "synthetic_revision_conflict_command",
                StoreError::RevisionConflict { expected: 4, actual: 5 },
            ),
            ("synthetic_commit_busy_command", StoreError::CommitBusy),
            ("synthetic_snapshot_released_command", StoreError::SnapshotReleased),
            (
                "synthetic_health_cancel_command",
                StoreError::Validation { message: crate::data_health::CANCELLED.to_owned() },
            ),
            (
                "synthetic_archive_cancel_command",
                StoreError::Validation { message: crate::persistent_store::archive::ARCHIVE_CANCELLED_MESSAGE.to_owned() },
            ),
            (
                "synthetic_export_cancel_command",
                StoreError::Validation { message: crate::persistent_store::export::EXPORT_CANCELLED_MESSAGE.to_owned() },
            ),
        ] {
            let code = serde_json::to_value(&error).unwrap()["code"].as_str().unwrap().to_owned();
            let _ = logged::<(), _>(command, Err(error));
            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.starts_with(&format!("{command} failed: ")))
                .expect("a routine refusal is logged");
            assert_eq!(entry.level, "warn");
            assert!(entry.message.starts_with(&format!("{command} failed: code={code} at=")));
        }
        assert!(logged("synthetic_quiet_storage_command", Ok::<(), StoreError>(())).is_ok());
        assert!(crate::native_log::global_state()
            .tail(None)
            .iter()
            .all(|entry| !entry.message.starts_with("synthetic_quiet_storage_command")));
    }

    #[test]
    fn a_json_payload_string_never_reaches_the_returned_error_or_the_log() {
        let shape = || serde_json::from_str::<u32>("\"private-payload-value\"").unwrap_err();
        let cases = [
            ("synthetic_json_shape_command", StoreError::from(shape()), "json-shape at line 1 column"),
            (
                "synthetic_json_syntax_command",
                StoreError::from(serde_json::from_str::<Value>("[\"private-payload-value\" x").unwrap_err()),
                "json-syntax at line 1 column",
            ),
            (
                "synthetic_json_eof_command",
                StoreError::from(serde_json::from_str::<Value>("[\"private-payload-value\"").unwrap_err()),
                "json-incomplete at line 1 column",
            ),
            (
                "synthetic_json_io_command",
                StoreError::from(std::io::Error::new(std::io::ErrorKind::InvalidData, shape())),
                "json-shape at line 1 column",
            ),
            (
                "synthetic_json_row_command",
                StoreError::from(rusqlite::Error::FromSqlConversionFailure(
                    10,
                    rusqlite::types::Type::Text,
                    Box::new(shape()),
                )),
                "json-shape at line 1 column",
            ),
        ];
        for (command, error, summary) in cases {
            let text = error.to_string();
            assert!(text.contains(summary), "{text}");
            assert!(!text.contains("private-payload-value"));
            let returned = serde_json::to_value(&error).unwrap();
            assert!(!returned.to_string().contains("private-payload-value"));
            let _ = logged::<(), _>(command, Err(error));
            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.starts_with(&format!("{command} failed: ")))
                .expect("the failure is logged");
            assert!(entry.message.contains(summary));
            assert!(!entry.message.contains("private-payload-value"));
        }
    }

    #[test]
    fn reads_the_root_revision_without_the_root() {
        let directory = tempdir().expect("create temporary directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let staging = store.replace_begin().expect("begin staging");
        store
            .replace_put_root(&staging.staging_id, &json!({ "username": "synthetic" }))
            .expect("stage root");
        store.replace_commit(&staging.staging_id, Some(0)).expect("commit root");
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        app.manage(PersistentStoreState::with_test_store(store));
        let root = pds_read_root(app.state(), None).expect("read root");
        assert_eq!(root.revision, 1);
        assert_eq!(pds_read_revision(app.state()).expect("read revision"), root.revision);
        let library = pds_read_library_identity(app.state()).unwrap();
        let alias: AssetAlias = serde_json::from_value(json!({ "kind": "asset", "key": "assets/synthetic", "objectHash": null,
            "size": 0, "mime": "", "name": "synthetic", "ext": "bin" })).unwrap();
        pds_commit_asset_alias(app.state(), alias.clone(), 1, Some(library.clone())).unwrap();
        assert_eq!(pds_read_library_identity(app.state()).unwrap(), library);
        with_store_mut(app.state(), |store| {
            let stage = store.replace_begin()?;
            store.replace_put_root(&stage.staging_id, &json!({}))?;
            store.replace_commit(&stage.staging_id, Some(2))
        }).unwrap();
        assert_ne!(pds_read_library_identity(app.state()).unwrap(), library);
        assert!(pds_commit_asset_alias(app.state(), alias, 3, Some(library)).is_err());
        assert_eq!(pds_read_revision(app.state()).unwrap(), 3);
    }

    #[test]
    fn storage_commands_log_their_own_failures() {
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        app.manage(PersistentStoreState::default());
        assert!(pds_read_root(app.state(), None).is_err());
        assert!(tauri::async_runtime::block_on(archive_character(app.handle().clone(), "synthetic".into(), 0, String::new())).is_err());
        assert!(data_health::pds_data_health_repair_plan(app.state()).is_err());
        assert!(hypa::pds_hypa_embedding_usage(app.state()).is_err());
        assert!(super::super::sync_selection::pds_lww_binding_content(app.state()).is_err());
        for (command, file) in [
            ("pds_read_root", "commands.rs:"),
            ("pds_archive_character", "commands.rs:"),
            ("pds_data_health_repair_plan", "data_health.rs:"),
            ("pds_hypa_embedding_usage", "hypa.rs:"),
            ("pds_lww_binding_content", "sync_selection.rs:"),
        ] {
            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.starts_with(&format!("{command} failed: code=validation cause=")))
                .unwrap_or_else(|| panic!("{command} logs its failure"));
            assert_eq!((entry.level.as_str(), entry.target.as_str()), ("error", "native-command"));
            assert!(entry.message.contains(file), "{}", entry.message);
        }
    }

    #[test]
    fn every_storage_error_logs_the_code_it_returns() {
        use crate::native_log::CommandFailure;
        for error in [
            StoreError::RevisionConflict { expected: 1, actual: 2 },
            StoreError::SnapshotReleased,
            StoreError::RawBodyUnavailable,
            StoreError::CommitBusy,
            StoreError::CommitDecode { message: String::new() },
            StoreError::Committed { revision: 1, message: String::new() },
            StoreError::Validation { message: String::new() },
            StoreError::SchemaMismatch { message: String::new() },
            StoreError::Store { message: String::new() },
        ] {
            assert_eq!(serde_json::to_value(&error).unwrap()["code"], error.code().as_ref());
        }
    }

    #[test]
    fn crashed_replacement_recovers_database_staging() {
        let directory = tempdir().expect("create temporary directory");
        let abandoned_staging_id;
        {
            let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
            let committed = store.replace_begin().expect("begin committed staging");
            store
                .replace_put_root(&committed.staging_id, &json!({ "username": "active" }))
                .expect("stage active root");
            store
                .replace_commit(&committed.staging_id, Some(0))
                .expect("commit active root");

            let abandoned = store.replace_begin().expect("begin abandoned staging");
            abandoned_staging_id = abandoned.staging_id.clone();
            store
                .replace_put_root(&abandoned.staging_id, &json!({ "username": "abandoned" }))
                .expect("stage abandoned root");
        }

        let mut reopened = PersistentStore::open(directory.path()).expect("recover persistent store");
        while reopened.purge_retired_batch(64).expect("purge retired libraries") {}
        let revision_before_sweep = reopened.revision().expect("read revision before sweep");
        let root_before_sweep = reopened.read_root(None).expect("read root before sweep");

        assert_eq!(
            reopened.revision().expect("read revision after sweep"),
            revision_before_sweep
        );
        assert_eq!(
            reopened.read_root(None).expect("read root after sweep"),
            root_before_sweep
        );
        assert_eq!(revision_before_sweep, 1);
        assert_eq!(root_before_sweep.value["username"], "active");
        for (table, _) in super::super::GENERATION_TABLES {
            let remaining: i64 = reopened
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE generation = ?1"),
                    [&abandoned_staging_id],
                    |row| row.get(0),
                )
                .expect("count swept P4 database staging rows");
            assert_eq!(remaining, 0, "{table}");
        }
    }

    #[test]
    fn retired_purge_steps_skip_maintenance_and_a_held_store() {
        let directory = tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        for username in ["first", "second"] {
            let stage = store.replace_begin().unwrap();
            store.replace_put_root(&stage.staging_id, &json!({ "username": username })).unwrap();
            let revision = store.revision().unwrap();
            store.replace_commit(&stage.staging_id, Some(revision)).unwrap();
        }
        let state = PersistentStoreState::with_test_store(store);
        let maintenance = state.acquire_raw_capture().unwrap();
        assert_eq!(state.purge_retired_step(1).unwrap(), None);
        drop(maintenance);
        {
            let _held = state.store.lock().unwrap();
            assert_eq!(state.purge_retired_step(1).unwrap(), None);
        }
        assert_eq!(state.purge_retired_step(1).unwrap(), Some(true));
        let mut steps = 1;
        while state.purge_retired_step(1).unwrap() == Some(true) {
            steps += 1;
        }
        assert!(steps > 1);
        let store = state.store.lock().unwrap();
        let retired: i64 = store
            .as_ref()
            .unwrap()
            .connection
            .query_row("SELECT COUNT(*) FROM generations WHERE state <> 'active'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retired, 0);
    }

    #[test]
    fn open_preserves_unreferenced_objects_and_the_maintenance_cursor() {
        let directory = tempdir().unwrap();
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let generation = super::super::active_generation(&store.connection).unwrap();
        let mut payloads = Vec::new();
        for index in 0..5 {
            let payload = cas
                .prepare_bytes(format!("open-object-{index}").as_bytes())
                .unwrap();
            register_command_gc_candidate(&mut store, &payload);
            if index > 0 {
                store.connection.execute(
                    "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
                     VALUES (?1, ?2, ?3, 'asset', ?4, 'application/octet-stream', ?2, 'bin')",
                    rusqlite::params![generation, format!("assets/open-{index}.bin"), payload.content_hash, payload.byte_size as i64],
                ).unwrap();
            }
            payloads.push(payload);
        }
        drop(store);
        let mut slot = None;
        for _ in 0..2 {
            assert_eq!(
                open_persistent_store(directory.path(), &mut slot)
                    .unwrap()
                    .revision,
                0
            );
            let store = slot.as_ref().unwrap();
            assert!(payloads
                .iter()
                .all(|payload| cas.stat_object(&payload.content_hash).unwrap()
                    == Some(payload.byte_size)));
            let deletions: i64 = store
                .connection
                .query_row("SELECT COUNT(*) FROM asset_object_deletions", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(deletions, 0);
            let cursor: Option<String> = store
                .connection
                .query_row(
                    "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(cursor, None);
        }
    }

    #[test]
    fn open_allows_missing_marked_objects_but_explicit_preview_detects_them() {
        let directory = tempdir().unwrap();
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        let payload = cas.prepare_bytes(b"missing marked open object").unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        register_command_gc_candidate(&mut store, &payload);
        let generation = super::super::active_generation(&store.connection).unwrap();
        store.connection.execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/missing.bin', ?2, 'asset', ?3, 'application/octet-stream', 'missing', 'bin')",
            rusqlite::params![generation, payload.content_hash, payload.byte_size as i64],
        ).unwrap();
        drop(store);
        fs::remove_file(directory.path().join(&payload.physical_key)).unwrap();
        let mut slot = None;
        for _ in 0..2 {
            assert_eq!(
                open_persistent_store(directory.path(), &mut slot)
                    .unwrap()
                    .revision,
                0
            );
            let state = PersistentStoreState {
                store: Mutex::new(slot.take()),
                ..PersistentStoreState::default()
            };
            let operation_guard = state.admit_renderer_operation().unwrap();
            assert!(pds_asset_gc_preview_all(&state, &operation_guard).is_err());
            drop(operation_guard);
            slot = state.store.into_inner().unwrap();
        }
    }

    #[test]
    fn open_result_returns_current_revision_without_gc_report() {
        let directory = tempdir().unwrap();
        let mut slot = None;
        let first = open_persistent_store(directory.path(), &mut slot).unwrap();
        assert_eq!(serde_json::to_value(first).unwrap(), json!({"revision": 0}));
        let store = slot.as_mut().unwrap();
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(&staging.staging_id, &json!({"username": "synthetic"}))
            .unwrap();
        store.replace_commit(&staging.staging_id, Some(0)).unwrap();
        let repeated = open_persistent_store(directory.path(), &mut slot).unwrap();
        assert_eq!(
            serde_json::to_value(repeated).unwrap(),
            json!({"revision": 1})
        );
    }


    #[test]
    fn product_maintenance_commands_share_one_store_serialization_lock() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Barrier,
        };
        use std::{thread, time::Duration};

        let directory = tempdir().expect("create serialized maintenance directory");
        let state = Arc::new(PersistentStoreState {
            store: Mutex::new(Some(
                PersistentStore::open(directory.path()).expect("open persistent store"),
            )),
            status_store: Mutex::new(None),
            snapshot_operations: Mutex::new(()),
            renderer_gate: Arc::new(RendererGate::default()),
            archive_operations: Mutex::new(HashMap::new()),
            retired_purge_started: AtomicBool::new(false),
        });
        let barrier = Arc::new(Barrier::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            workers.push(thread::spawn(move || {
                barrier.wait();
                with_store_mutex_mut(&state, |_| {
                    let concurrent = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(concurrent, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .expect("run serialized maintenance operation");
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().expect("join maintenance worker");
        }
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    fn register_command_gc_candidate(
        store: &mut PersistentStore,
        prepared: &crate::asset_repository::PreparedPayload,
    ) {
        store
            .asset_object_catalog()
            .register(
                &[
                    crate::persistent_store::asset_object_catalog::AssetObjectRegistration {
                        object_hash: prepared.content_hash.clone(),
                        byte_size: prepared.byte_size,
                    },
                ],
                0,
            )
            .expect("register command GC candidate");
    }

    #[test]
    fn asset_gc_preview_command_totals_multiple_pages_without_mutating_maintenance_state() {
        use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};

        let directory = tempdir().expect("create preview command directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
        let total = 129;
        let mut prepared = Vec::new();
        for index in 0..total {
            let payload = cas
                .prepare_bytes(format!("preview-command-{index}").as_bytes())
                .expect("prepare candidate");
            register_command_gc_candidate(&mut store, &payload);
            prepared.push(payload);
        }
        store
            .connection
            .execute(
                "UPDATE asset_gc_maintenance_state
                 SET catalog_cursor = 'preserve-preview-cursor' WHERE singleton = 1",
                [],
            )
            .expect("seed maintenance cursor");
        let mut released = DurableCasJob::begin(
            directory.path(),
            "preview-released-job",
            CasJobKind::LocalBackupRestore,
            crate::asset_repository::job_pins::CasJobOwner::for_test(),
            0,
        )
        .expect("begin released journal fixture");
        released
            .seal(&mut store, 1)
            .expect("seal released journal fixture");
        released
            .leave_release_record_for_cleanup_retry(CasReleaseOutcome::Aborted)
            .expect("leave released journal fixture");
        let released_journal = directory
            .path()
            .join("assets/job-pins/job-preview-released-job.journal");
        assert!(released_journal.is_file());
        let expected_bytes = prepared
            .iter()
            .map(|payload| payload.byte_size)
            .sum::<u64>();

        let state = PersistentStoreState {
            store: Mutex::new(Some(store)),
            ..PersistentStoreState::default()
        };
        let operation_guard = state.admit_renderer_operation().unwrap();
        let result =
            pds_asset_gc_preview_all(&state, &operation_guard).expect("preview every command page");
        drop(operation_guard);
        let store = state.store.into_inner().unwrap().unwrap();

        assert_eq!(result.candidate_count, total);
        assert_eq!(result.candidate_bytes, expected_bytes);
        assert_eq!(result.deleted_count, 0);
        assert_eq!(result.deleted_bytes, 0);
        assert!(result.blockers.is_empty());
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .expect("read preserved maintenance cursor"),
            Some("preserve-preview-cursor".to_owned())
        );
        assert_eq!(
            store
                .query_asset_object_catalog(256, None)
                .expect("read preserved catalog")
                .items
                .len(),
            total as usize
        );
        assert!(prepared.iter().all(|payload| {
            cas.stat_object(&payload.content_hash)
                .expect("stat preserved candidate")
                == Some(payload.byte_size)
        }));
        assert!(released_journal.is_file());
    }

    #[test]
    fn asset_gc_execute_command_totals_and_deletes_multiple_pages() {
        let directory = tempdir().expect("create execute command directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).expect("open CAS");
        super::TEST_ASSET_GC_EXECUTE_PAGE.with(|page| page.set(10));
        // Two full pages and a partial one.
        let total = 25;
        let mut prepared = Vec::new();
        for index in 0..total {
            let payload = cas
                .prepare_bytes(format!("execute-command-{index}").as_bytes())
                .expect("prepare candidate");
            register_command_gc_candidate(&mut store, &payload);
            prepared.push(payload);
        }
        let expected_bytes = prepared
            .iter()
            .map(|payload| payload.byte_size)
            .sum::<u64>();

        let state = PersistentStoreState {
            store: Mutex::new(Some(store)),
            ..PersistentStoreState::default()
        };
        let operation_guard = state.admit_renderer_operation().unwrap();
        super::super::ASSET_GC_ROOT_COLLECTIONS.with(|count| count.set((0, 0)));
        super::super::ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| count.set(0));
        let result = pds_asset_gc_execute_all(&state, &operation_guard).expect("execute complete cleanup");
        super::super::ASSET_GC_ROOT_COLLECTIONS.with(|count| {
            assert_eq!(count.get(), (1, 3), "one preliminary scan and one final check per deleting page");
        });
        super::super::ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| {
            assert_eq!(count.get(), 2, "an unchanged library is scanned once for marks and once for the final check");
        });
        assert!(state.store.try_lock().is_ok());
        drop(operation_guard);
        let store = state.store.into_inner().unwrap().unwrap();
        assert_eq!(result.candidate_count, total);
        assert_eq!(result.candidate_bytes, expected_bytes);
        assert_eq!(result.deleted_count, total);
        assert_eq!(result.deleted_bytes, expected_bytes);
        assert!(result.blockers.is_empty());
        assert!(store
            .query_asset_object_catalog(256, None)
            .expect("read deleted catalog")
            .items
            .is_empty());
        assert!(prepared.iter().all(|payload| {
            cas.stat_object(&payload.content_hash)
                .expect("stat deleted candidate")
                .is_none()
        }));
    }
    /// The local data screen reads the shipped defaults, then writes the one
    /// row the user changed.
    #[test]
    fn local_data_command_sets_device_sections_participating() {
        use tauri::Listener;
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap();
        let changes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = changes.clone();
        app.listen("risu-server-sync-device-changed", move |_| { received.fetch_add(1, std::sync::atomic::Ordering::Relaxed); });
        let directory = tempdir().unwrap();
        let state = PersistentStoreState::default();
        open_renderer_persistent_store(&state, directory.path()).unwrap();

        let initial = read_section_participation(&state).unwrap();
        assert_eq!(
            initial
                .iter()
                .map(|row| (row.section.as_str(), row.participating))
                .collect::<Vec<_>>(),
            vec![("hypa", true), ("local-plugins", false)]
        );

        set_section_participation(app.handle(), &state, "local-plugins", true).unwrap();
        assert_eq!(changes.load(std::sync::atomic::Ordering::Relaxed), 1);
        set_section_participation(app.handle(), &state, "local-plugins", true).unwrap();
        assert_eq!(changes.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(with_store_mutex(&state, |store| Ok(store
            .device_store()?
            .section_state(super::super::device_store::Section::LocalPlugins)?
            .participating))
        .unwrap());

        assert!(set_section_participation(app.handle(), &state, "hypa", false).is_err());
        assert_eq!(
            read_section_participation(&state)
                .unwrap()
                .iter()
                .map(|row| (row.section.as_str(), row.participating))
                .collect::<Vec<_>>(),
            vec![("hypa", true), ("local-plugins", true)]
        );

        assert!(matches!(
            set_section_participation(app.handle(), &state, "library", true),
            Err(StoreError::Validation { .. })
        ));
    }
}
