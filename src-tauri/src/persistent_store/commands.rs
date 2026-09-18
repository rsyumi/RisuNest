pub(crate) mod data_health;
pub(crate) mod hypa;

use super::archive::ArchivePreview;
use super::device_store::plugin_values::{
    PluginDeviceHydration, PluginDeviceListItem, PluginDeviceMutation,
};
use super::device_store::sections::{section_from_id, CHOOSABLE_SECTIONS};
use super::export::ExportedRisuSave;
#[cfg(feature = "native-kei-upload-pilot")]
use super::kei::KeiUploadResult;
use super::content_change_index::{ContentChangeWindow, ContentKey};
use super::{
    AssetAlias, AssetAliasListQuery, AssetAliasPage, AssetOwnerHead, AssetOwnerLocator,
    AssetRepositoryAuthorityState, AssignedPluginStorage, CharacterPage, CharacterQuery,
    CharacterSummary, CheckpointMode, ClaimedPluginValue, ConversationPage, ConversationQuery,
    ConversationWindow, ConversationWindowQuery, LeaseResult, PersistentStorageStats,
    PersistentStore, PluginStorageCatalog, PluginStorageListItem, PresetCatalog, RevisionResult,
    SnapshotCreated, SnapshotInfo, StagingResult, StoreError, StoreResult, Versioned, WorkingSetCommit,
};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

#[cfg(feature = "official-publication-upload-pilot")]
use crate::publication_upload::{
    upload_open_file_attempt, OfficialPublicationUploadError, OfficialPublicationUploadRequest,
    OfficialPublicationUploadResult,
};

pub(crate) struct PersistentStoreState {
    store: Mutex<Option<PersistentStore>>,
    snapshot_operations: Mutex<()>,
    renderer_gate: Arc<RendererGate>,
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
    pub(crate) fn admit_renderer_operation(&self) -> StoreResult<RendererOperationGuard> {
        let mut state = self
            .renderer_gate
            .state
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent renderer admission mutex poisoned: {error}"),
            })?;
        if state.maintenance_active {
            return Err(renderer_gate_error());
        }
        state.operations += 1;
        Ok(RendererOperationGuard {
            gate: Arc::clone(&self.renderer_gate),
        })
    }

    pub(crate) fn acquire_device_maintenance(&self) -> StoreResult<DeviceMaintenanceGuard> {
        let maintenance = self
            .try_acquire_renderer_maintenance()?
            .ok_or_else(renderer_gate_error)?;
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
        if state.maintenance_active {
            return Ok(None);
        }
        state.maintenance_active = true;
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
            snapshot_operations: Mutex::new(()),
            renderer_gate: Arc::new(RendererGate::default()),
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistentStoreOpenResult {
    revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    restore_failure: Option<String>,
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

fn finish_storage_command<T>(command: &'static str, result: StoreResult<T>) -> StoreResult<T> {
    result.map_err(|error| {
        crate::nlog!("error", "{command} failed: {error}");
        error
    })
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
    operation(store)
}

pub(crate) fn replace_commit_with_snapshot(
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
    let operation_guard = state.admit_renderer_operation()?;
    let app_data_dir = crate::app_data_root::resolve(&app).map_err(|error| StoreError::Store {
        message: format!("failed to resolve application data directory: {error}"),
    })?;
    open_renderer_persistent_store_admitted(&state, &operation_guard, &app_data_dir)
}

#[cfg(test)]
fn open_renderer_persistent_store(
    state: &PersistentStoreState,
    app_data_dir: &Path,
) -> StoreResult<PersistentStoreOpenResult> {
    let operation_guard = state.admit_renderer_operation()?;
    open_renderer_persistent_store_admitted(state, &operation_guard, app_data_dir)
}

fn open_renderer_persistent_store_admitted(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    app_data_dir: &Path,
) -> StoreResult<PersistentStoreOpenResult> {
    if !operation_guard.belongs_to(state) {
        return Err(StoreError::Validation {
            message: "renderer operation permit belongs to another persistent store".to_owned(),
        });
    }
    let mut store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    open_persistent_store(app_data_dir, &mut store)
}

fn open_persistent_store(
    app_data_dir: &Path,
    store: &mut Option<PersistentStore>,
) -> StoreResult<PersistentStoreOpenResult> {
    if let Some(store) = store.as_mut() {
        let revision = store.revision()?;
        let restore_failure = store.pending_restore_failure().map(str::to_owned);
        return Ok(PersistentStoreOpenResult {
            revision,
            restore_failure,
        });
    }

    let persistent_store = PersistentStore::open(app_data_dir)?;
    let revision = persistent_store.revision()?;
    let restore_failure = persistent_store
        .pending_restore_failure()
        .map(str::to_owned);
    *store = Some(persistent_store);

    Ok(PersistentStoreOpenResult {
        revision,
        restore_failure,
    })
}

#[tauri::command(async)]
pub(crate) fn pds_asset_gc_maintenance(
    state: State<'_, PersistentStoreState>,
) -> Result<crate::asset_repository::migration_gc::AssetGcDryRunPage, StoreError> {
    with_store_mut(state, |store| {
        store.asset_gc_product_maintenance_page(current_time_ms()?)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_root(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<Versioned<Value>, StoreError> {
    with_store(state, |store| store.read_root(lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_query_presets(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<PresetCatalog, StoreError> {
    with_store(state, |store| store.query_presets(lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_read_preset(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    with_store(state, |store| store.read_preset(&id, lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_query_characters(
    state: State<'_, PersistentStoreState>,
    query: CharacterQuery,
    lease: Option<String>,
) -> Result<CharacterPage, StoreError> {
    with_store(state, |store| {
        store.query_characters(&query, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_character(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    with_store(state, |store| store.read_character(&id, lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_read_character_summary(
    state: State<'_, PersistentStoreState>,
    id: String,
    lease: Option<String>,
) -> Result<Option<CharacterSummary>, StoreError> {
    with_store(state, |store| {
        store.read_character_summary(&id, lease.as_deref())
    })
}

/// Reading the window and the records it names through one lease is what keeps
/// a targeted pass equivalent to a reprojection.
#[tauri::command(async)]
pub(crate) fn pds_working_set_change_window(
    state: State<'_, PersistentStoreState>,
    lease: String,
) -> Result<ContentChangeWindow, StoreError> {
    with_store(state, |store| store.working_set_change_window(&lease))
}

#[tauri::command(async)]
pub(crate) fn pds_working_set_change_page(
    state: State<'_, PersistentStoreState>,
    lease: String,
    after_revision: i64,
    after_key: Option<ContentKey>,
    limit: usize,
) -> Result<Vec<ContentKey>, StoreError> {
    with_store(state, |store| {
        store.working_set_change_page(&lease, after_revision, after_key, limit)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_commit_working_set_change_cursor(
    state: State<'_, PersistentStoreState>,
    revision: i64,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.commit_working_set_change_cursor(revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_query_conversations(
    state: State<'_, PersistentStoreState>,
    query: ConversationQuery,
    lease: Option<String>,
) -> Result<ConversationPage, StoreError> {
    with_store(state, |store| {
        store.query_conversations(&query, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    conversation_id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    with_store(state, |store| {
        store.read_conversation(&character_id, &conversation_id, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation_metadata(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    conversation_id: String,
    lease: Option<String>,
) -> Result<Option<Versioned<super::PersistentConversationMetadata>>, StoreError> {
    with_store(state, |store| {
        store.read_conversation_metadata(&character_id, &conversation_id, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_conversation_window(
    state: State<'_, PersistentStoreState>,
    query: ConversationWindowQuery,
    lease: Option<String>,
) -> Result<Option<Versioned<ConversationWindow>>, StoreError> {
    with_store(state, |store| {
        store.read_conversation_window(&query, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_query_plugin_storage(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<PluginStorageCatalog, StoreError> {
    with_store(state, |store| store.query_plugin_storage(lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_storage(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<Vec<PluginStorageListItem>, StoreError> {
    with_store(state, |store| store.list_plugin_storage(lease.as_deref()))
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
    key: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    with_store(state, |store| {
        store.read_plugin_storage(&owner, &key, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_alias(
    state: State<'_, PersistentStoreState>,
    kind: String,
    key: String,
    lease: Option<String>,
) -> Result<Option<Versioned<AssetAlias>>, StoreError> {
    with_store(state, |store| {
        store.read_asset_alias(&kind, &key, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_aliases_by_keys(
    state: State<'_, PersistentStoreState>,
    kind: String,
    keys: Vec<String>,
    lease: Option<String>,
) -> Result<Versioned<Vec<AssetAlias>>, StoreError> {
    with_store(state, |store| {
        store.read_asset_aliases_by_keys(&kind, &keys, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_list_asset_aliases(
    state: State<'_, PersistentStoreState>,
    query: AssetAliasListQuery,
    lease: Option<String>,
) -> Result<AssetAliasPage, StoreError> {
    with_store(state, |store| {
        store.list_asset_alias_page(&query, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_repository_authority(
    state: State<'_, PersistentStoreState>,
    lease: Option<String>,
) -> Result<Versioned<AssetRepositoryAuthorityState>, StoreError> {
    with_store(state, |store| {
        store.read_asset_repository_authority(lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_asset_owner_head(
    state: State<'_, PersistentStoreState>,
    owner: AssetOwnerLocator,
    lease: Option<String>,
) -> Result<Option<Versioned<AssetOwnerHead>>, StoreError> {
    with_store(state, |store| {
        store.read_asset_owner_head(&owner, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_commit_asset_alias(
    state: State<'_, PersistentStoreState>,
    alias: AssetAlias,
    expected_revision: i64,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| {
        store.commit_asset_alias(&alias, expected_revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_delete_asset_alias(
    state: State<'_, PersistentStoreState>,
    kind: String,
    key: String,
    expected_revision: i64,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| {
        store.delete_asset_alias(&kind, &key, expected_revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_commit(
    state: State<'_, PersistentStoreState>,
    commit: WorkingSetCommit,
    asset_aliases: Vec<AssetAlias>,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| {
        store.commit_with_asset_aliases(&commit, &asset_aliases)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_archive_preview(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    lease: Option<String>,
) -> Result<ArchivePreview, StoreError> {
    with_store(state, |store| {
        store.archive_preview(&character_id, lease.as_deref())
    })
}

#[tauri::command(async)]
pub(crate) fn pds_archive_character(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    expected_revision: i64,
) -> Result<RevisionResult, StoreError> {
    let now_ms = current_time_ms()?;
    with_store_mut(state, |store| {
        store.archive_character(&character_id, expected_revision, now_ms)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_restore_character(
    state: State<'_, PersistentStoreState>,
    character_id: String,
    expected_revision: i64,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| {
        store.restore_character(&character_id, expected_revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_begin(
    state: State<'_, PersistentStoreState>,
) -> Result<StagingResult, StoreError> {
    with_store_mut(state, PersistentStore::replace_begin)
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_root(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    root: Value,
    plugin_storage_values: Option<Vec<super::PluginStorageValue>>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_put_root_with_plugin_storage(
            &staging_id,
            &root,
            plugin_storage_values.as_deref(),
        )
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_add_characters(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    characters: Vec<Value>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_add_characters(&staging_id, &characters)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_presets(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    presets: Vec<Value>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_put_presets(&staging_id, &presets)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_asset_aliases(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    aliases: Vec<AssetAlias>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_put_asset_aliases(&staging_id, &aliases)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_asset_owner_heads(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    heads: Vec<AssetOwnerHead>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_put_asset_owner_heads(&staging_id, &heads)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_put_asset_repository_authority(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    authority: AssetRepositoryAuthorityState,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.replace_put_asset_repository_authority(&staging_id, &authority)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_preserve_repositories(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
    expected_revision: Option<i64>,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| {
        store.replace_preserve_repositories(&staging_id, expected_revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_replace_commit(
    app: AppHandle,
    staging_id: String,
    expected_revision: Option<i64>,
) -> Result<RevisionResult, StoreError> {
    replace_commit_with_snapshot(&app, &staging_id, expected_revision)
}

#[tauri::command(async)]
pub(crate) fn pds_replace_abort(
    state: State<'_, PersistentStoreState>,
    staging_id: String,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| store.replace_abort(&staging_id))
}

#[tauri::command(async)]
pub(crate) fn pds_materialize(
    state: State<'_, PersistentStoreState>,
    revision: Option<i64>,
) -> Result<Value, StoreError> {
    with_store(state, |store| store.materialize(revision))
}

#[tauri::command(async)]
pub(crate) fn pds_acquire_revision(
    state: State<'_, PersistentStoreState>,
    revision: i64,
) -> Result<LeaseResult, StoreError> {
    with_store_mut(state, |store| store.acquire_revision(revision))
}

#[tauri::command(async)]
pub(crate) fn pds_release_revision(
    state: State<'_, PersistentStoreState>,
    lease: String,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| store.release_revision(&lease))
}

#[tauri::command(async)]
pub(crate) fn pds_export_risu_save(
    state: State<'_, PersistentStoreState>,
    lease: String,
    omit_account: bool,
) -> Result<ExportedRisuSave, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    let prepared = with_store_mutex_mut_admitted(&state, &operation_guard, |store| {
        store.detach_risu_save_export(&lease)
    })?;
    let outcome = prepared.create_attached_export(omit_account);
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
}

#[tauri::command(async)]
pub(crate) fn pds_export_risu_save_cleanup(
    state: State<'_, PersistentStoreState>,
    path: String,
) -> Result<(), StoreError> {
    with_store(state, |store| {
        store.cleanup_risu_save_export(Path::new(&path))
    })
}

#[cfg(feature = "official-publication-upload-pilot")]
#[tauri::command]
pub(crate) async fn official_publication_upload_file(
    state: State<'_, PersistentStoreState>,
    request: OfficialPublicationUploadRequest,
) -> Result<OfficialPublicationUploadResult, OfficialPublicationUploadError> {
    let (source, bytes) = with_store(state, |store| {
        store.open_risu_save_export_for_upload(Path::new(&request.path))
    })
    .map_err(|error| OfficialPublicationUploadError::Source {
        message: error.to_string(),
    })?;
    upload_open_file_attempt(request, source, bytes).await
}

#[cfg(feature = "native-kei-upload-pilot")]
#[tauri::command(async)]
pub(crate) async fn pds_kei_backup_upload(
    state: State<'_, PersistentStoreState>,
    lease: String,
    url: String,
    expected_account_id: String,
    token: String,
) -> Result<KeiUploadResult, StoreError> {
    let operation = state.admit_renderer_operation()?;
    let prepared = with_store_mutex_mut_admitted(&state, &operation, |store| {
        store.prepare_kei_upload(&lease, &url, &expected_account_id, &token)
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
}

#[tauri::command(async)]
pub(crate) fn pds_checkpoint(
    state: State<'_, PersistentStoreState>,
    mode: CheckpointMode,
) -> Result<(), StoreError> {
    with_store(state, |store| store.checkpoint(mode))
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_create(
    state: State<'_, PersistentStoreState>,
    reason: String,
) -> Result<SnapshotCreated, StoreError> {
    let _operation = state.admit_renderer_operation()?;
    let snapshot_operation =
        state
            .snapshot_operations
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent snapshot mutex poisoned: {error}"),
            })?;
    let store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    let store = store.as_ref().ok_or_else(|| StoreError::Validation {
        message: "persistent store has not been opened".to_owned(),
    })?;
    let result = store.snapshot_create(&reason);
    drop(snapshot_operation);
    result
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_list(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<SnapshotInfo>, StoreError> {
    #[cfg(target_os = "android")]
    {
        with_snapshot_directory(&state, super::snapshot::list)
    }
    #[cfg(not(target_os = "android"))]
    {
        with_store(state, PersistentStore::snapshot_list)
    }
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_delete(
    state: State<'_, PersistentStoreState>,
    id: String,
) -> Result<(), StoreError> {
    finish_storage_command(
        "pds_snapshot_delete",
        with_store(state, |store| store.snapshot_delete(&id)),
    )
}

#[tauri::command(async)]
pub(crate) fn pds_storage_stats(
    state: State<'_, PersistentStoreState>,
) -> Result<PersistentStorageStats, StoreError> {
    finish_storage_command(
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
    finish_storage_command(
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
    state: State<'_, PersistentStoreState>,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let operation_guard = state.admit_renderer_operation()?;
    finish_storage_command(
        "pds_asset_gc_execute",
        pds_asset_gc_execute_all(&state, &operation_guard),
    )
}

fn pds_asset_gc_execute_all(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
) -> Result<AssetGcMaintenanceResult, StoreError> {
    let now = current_time_ms()?;
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
        let page = pds_asset_gc_execute_page(state, operation_guard, cursor.as_deref(), now)?;
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

fn pds_asset_gc_execute_page(
    state: &PersistentStoreState,
    operation_guard: &RendererOperationGuard,
    cursor: Option<&str>,
    now: i64,
) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
    with_store_mutex_mut_admitted(state, operation_guard, |store| {
        store.asset_gc_delete_page_with_hook(128, cursor, now, 7 * 24 * 60 * 60 * 1_000, |_| Ok(()))
    })
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_restore_request(
    state: State<'_, PersistentStoreState>,
    id: String,
) -> Result<(), StoreError> {
    with_store(state, |store| store.snapshot_restore_request(&id))
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
    with_store(state, |store| {
        store.colliding_plugin_storage_keys(&owner, &keys)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_assign_plugin_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
    sources: Vec<PluginStorageSource>,
    collision: super::commit::AssignCollision,
    expected_revision: i64,
) -> Result<AssignedPluginStorage, StoreError> {
    let sources: Vec<(String, String)> = sources
        .into_iter()
        .map(|source| (source.owner, source.key))
        .collect();
    with_store_mut(state, |store| {
        store.assign_plugin_storage(&sources, &owner, collision, expected_revision)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_begin_plugin_claim_session(
    state: State<'_, PersistentStoreState>,
    owner: String,
    code_hash: String,
    runtime_instance: String,
) -> Result<Option<String>, StoreError> {
    with_store(state, |store| {
        store.begin_plugin_claim_session(&owner, &code_hash, &runtime_instance)
    })
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
    with_store_mut(state, |store| {
        store.claim_plugin_storage_value(
            &session_id,
            &owner,
            &code_hash,
            &runtime_instance,
            &key,
            expected_revision,
        )
    })
}

#[tauri::command(async)]
pub(crate) fn pds_close_plugin_claim_session(
    state: State<'_, PersistentStoreState>,
    session_id: String,
) -> Result<(), StoreError> {
    with_store(state, |store| store.close_plugin_claim_session(&session_id))
}

#[tauri::command(async)]
pub(crate) fn pds_hydrate_plugin_device_storage(
    state: State<'_, PersistentStoreState>,
    owner: String,
) -> Result<PluginDeviceHydration, StoreError> {
    with_store(state, |store| {
        store.device_store()?.hydrate_plugin_device_storage(&owner)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_read_plugin_device_value(
    state: State<'_, PersistentStoreState>,
    owner: String,
    space: String,
    key: String,
) -> Result<Option<String>, StoreError> {
    with_store(state, |store| {
        store
            .device_store()?
            .read_plugin_device_value(&owner, &space, &key)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_device_keys(
    state: State<'_, PersistentStoreState>,
    owner: String,
    space: String,
) -> Result<Vec<String>, StoreError> {
    with_store(state, |store| {
        store.device_store()?.list_plugin_device_keys(&owner, &space)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_list_plugin_device_storage(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<PluginDeviceListItem>, StoreError> {
    with_store(state, |store| {
        store.device_store()?.list_plugin_device_storage()
    })
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_device_values(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    owner: String,
    mutations: Vec<PluginDeviceMutation>,
) -> Result<(), StoreError> {
    let changed = with_store_mut(state, |store| {
        let device = store.device_store_mut()?;
        let before = device.revision()?;
        device.write_plugin_device_values(&owner, &mutations)?;
        Ok(device.revision()? != before)
    })?;
    if changed {
        crate::server_sync::events::notify_device_changed(&app);
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) fn pds_get_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
) -> Result<Option<Value>, StoreError> {
    with_store(state, |store| store.device_store()?.read_setting(&key))
}

/// A null value removes the setting.
#[tauri::command(async)]
pub(crate) fn pds_set_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
    value: Option<Value>,
) -> Result<(), StoreError> {
    with_store(state, |store| {
        let device = store.device_store()?;
        match value {
            Some(value) => device.write_setting(&key, &value),
            None => device.remove_setting(&key),
        }
    })
}

#[tauri::command(async)]
pub(crate) fn pds_patch_device_setting(
    state: State<'_, PersistentStoreState>,
    key: String,
    entries: serde_json::Map<String, Value>,
) -> Result<(), StoreError> {
    with_store_mut(state, |store| {
        store.device_store_mut()?.patch_setting(&key, &entries)
    })
}

/// Answers in request order so one call can fill the whole boot cache.
#[tauri::command(async)]
pub(crate) fn pds_read_device_settings(
    state: State<'_, PersistentStoreState>,
    keys: Vec<String>,
) -> Result<Vec<Option<Value>>, StoreError> {
    with_store(state, |store| store.device_store()?.read_settings(&keys))
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
    with_store(state, |store| {
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
    })
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_permission(
    state: State<'_, PersistentStoreState>,
    code_hash: String,
    permission: String,
    granted: bool,
) -> Result<(), StoreError> {
    with_store(state, |store| {
        store
            .device_store()?
            .write_plugin_permission(&code_hash, &permission, granted)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_write_plugin_permission_grant(
    state: State<'_, PersistentStoreState>,
    plugin_name: String,
    permission: String,
    last_grant_at: i64,
) -> Result<(), StoreError> {
    with_store(state, |store| {
        store.device_store()?.write_plugin_permission_grant(
            &plugin_name,
            &permission,
            last_grant_at,
        )
    })
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

fn set_section_participation(
    state: &PersistentStoreState,
    section: &str,
    participating: bool,
) -> Result<(), StoreError> {
    let Some(section) = section_from_id(section) else {
        return Err(StoreError::Validation {
            message: "unknown device section".to_owned(),
        });
    };
    with_store_mutex_mut(state, |store| {
        store
            .device_store_mut()?
            .set_section_participating(section, participating)
    })
}

/// Which sections this device currently exchanges with a remote.
#[tauri::command(async)]
pub(crate) fn pds_read_section_participation(
    state: State<'_, PersistentStoreState>,
) -> Result<Vec<SectionParticipationRow>, StoreError> {
    read_section_participation(&state)
}

#[tauri::command(async)]
pub(crate) fn pds_set_section_participating(
    state: State<'_, PersistentStoreState>,
    section: String,
    participating: bool,
) -> Result<(), StoreError> {
    set_section_participation(&state, &section, participating)
}

#[cfg(test)]
mod tests {
    mod asset_gc_performance;

    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

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

        let maintenance = state.acquire_device_maintenance().unwrap();
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
            let returned = finish_storage_command::<()>(command, Err(error))
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
            assert!(entry.message.contains("Authorization: ***"));
            assert!(!entry.message.contains("fixture-"));
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

        let reopened = PersistentStore::open(directory.path()).expect("recover persistent store");
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
    fn open_result_reports_a_skipped_pending_restore() {
        let directory = tempdir().expect("create restore failure directory");
        drop(PersistentStore::open(directory.path()).unwrap());
        fs::write(
            directory
                .path()
                .join("persistent/snapshots/pending-restore.json"),
            b"{ not json",
        )
        .unwrap();
        let mut slot = None;
        let result =
            open_persistent_store(directory.path(), &mut slot).expect("reopen with corrupt marker");
        let failure = slot
            .as_ref()
            .unwrap()
            .pending_restore_failure()
            .expect("skipped restore is reported")
            .to_owned();
        assert!(failure.contains("persistent snapshot restore skipped"));
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"revision": 0, "restoreFailure": failure})
        );
        let repeated = open_persistent_store(directory.path(), &mut slot).unwrap();
        assert_eq!(repeated.restore_failure.as_deref(), Some(failure.as_str()));
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
            snapshot_operations: Mutex::new(()),
            renderer_gate: Arc::new(RendererGate::default()),
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
            .join("assets-v2/job-pins/job-preview-released-job.journal");
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
        let total = 129;
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
        let observed_pages = std::cell::Cell::new(0);
        let now = current_time_ms().unwrap();
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
            let page = pds_asset_gc_execute_page(&state, &operation_guard, cursor.as_deref(), now)
                .expect("execute command page");
            assert!(
                state.store.try_lock().is_ok(),
                "store lock must be released between GC pages"
            );
            observed_pages.set(observed_pages.get() + 1);
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
        drop(operation_guard);
        let store = state.store.into_inner().unwrap().unwrap();

        assert!(observed_pages.get() >= 2);
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

        set_section_participation(&state, "local-plugins", true).unwrap();
        assert!(with_store_mutex(&state, |store| Ok(store
            .device_store()?
            .section_state(super::super::device_store::Section::LocalPlugins)?
            .participating))
        .unwrap());

        set_section_participation(&state, "hypa", false).unwrap();
        assert_eq!(
            read_section_participation(&state)
                .unwrap()
                .iter()
                .map(|row| (row.section.as_str(), row.participating))
                .collect::<Vec<_>>(),
            vec![("hypa", false), ("local-plugins", true)]
        );

        assert!(matches!(
            set_section_participation(&state, "library", true),
            Err(StoreError::Validation { .. })
        ));
    }
}
