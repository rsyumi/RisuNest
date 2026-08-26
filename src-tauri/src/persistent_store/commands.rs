use super::export::ExportedRisuSave;
#[cfg(feature = "native-kei-upload-pilot")]
use super::kei::KeiUploadResult;
use super::{
    AssetAlias, AssetOwnerHead, AssetOwnerLocator, CharacterPage, CharacterQuery, CheckpointMode,
    ConversationPage, ConversationQuery, ConversationWindow, ConversationWindowQuery, LeaseResult,
    PersistentStore, PluginStorageCatalog, PresetCatalog, RevisionResult, SnapshotCreated,
    SnapshotInfo, StagingResult, StoreError, StoreResult, Versioned, WorkingSetCommit,
};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};

#[cfg(feature = "official-publication-upload-pilot")]
use crate::publication_upload::{
    upload_open_file_attempt, OfficialPublicationUploadError, OfficialPublicationUploadRequest,
    OfficialPublicationUploadResult,
};

pub(crate) struct PersistentStoreState {
    store: Mutex<Option<PersistentStore>>,
    snapshot_operations: Mutex<()>,
}

impl Default for PersistentStoreState {
    fn default() -> Self {
        Self {
            store: Mutex::new(None),
            snapshot_operations: Mutex::new(()),
        }
    }
}

fn with_store<T>(
    state: State<'_, PersistentStoreState>,
    operation: impl FnOnce(&PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
    let store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;
    let store = store.as_ref().ok_or_else(|| StoreError::Validation {
        message: "persistent store has not been opened".to_owned(),
    })?;
    operation(store)
}

pub(crate) fn with_store_mut<T>(
    state: State<'_, PersistentStoreState>,
    operation: impl FnOnce(&mut PersistentStore) -> StoreResult<T>,
) -> StoreResult<T> {
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
    let prepared = with_store_mut(app.state(), |store| {
        store.prepare_replace_commit(staging_id, expected_revision)
    })?;
    let state = app.state::<PersistentStoreState>();
    let snapshot_operation =
        state
            .snapshot_operations
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("persistent snapshot mutex poisoned: {error}"),
            })?;
    let authorized = prepared.create_snapshot()?;
    drop(snapshot_operation);
    with_store_mut(app.state(), |store| {
        store.finish_prepared_replace(authorized)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_open(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
) -> Result<RevisionResult, StoreError> {
    let mut store = state.store.lock().map_err(|error| StoreError::Store {
        message: format!("persistent store mutex poisoned: {error}"),
    })?;

    if let Some(store) = store.as_ref() {
        return Ok(RevisionResult {
            revision: store.revision()?,
        });
    }

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| StoreError::Store {
            message: format!("failed to resolve application data directory: {error}"),
        })?;
    let persistent_store = PersistentStore::open(&app_data_dir)?;
    let revision = persistent_store.revision()?;
    *store = Some(persistent_store);

    Ok(RevisionResult { revision })
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
pub(crate) fn pds_read_plugin_storage(
    state: State<'_, PersistentStoreState>,
    key: String,
    lease: Option<String>,
) -> Result<Option<Versioned<Value>>, StoreError> {
    with_store(state, |store| {
        store.read_plugin_storage(&key, lease.as_deref())
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
pub(crate) fn pds_commit(
    state: State<'_, PersistentStoreState>,
    commit: WorkingSetCommit,
) -> Result<RevisionResult, StoreError> {
    with_store_mut(state, |store| store.commit(&commit))
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
) -> Result<(), StoreError> {
    with_store_mut(state, |store| store.replace_put_root(&staging_id, &root))
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
    with_store(state, |store| store.export_risu_save(&lease, omit_account))
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
    let prepared = with_store(state, |store| {
        store.prepare_kei_upload(&lease, &url, &expected_account_id, &token)
    })?;
    prepared.upload().await
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
    with_store(state, PersistentStore::snapshot_list)
}

#[tauri::command(async)]
pub(crate) fn pds_snapshot_restore_request(
    state: State<'_, PersistentStoreState>,
    path: String,
) -> Result<(), StoreError> {
    with_store(state, |store| {
        store.snapshot_restore_request(Path::new(&path))
    })
}

#[tauri::command(async)]
pub(crate) fn pds_get_app_kv(
    state: State<'_, PersistentStoreState>,
    key: String,
) -> Result<Option<Value>, StoreError> {
    with_store(state, |store| store.get_app_kv(&key))
}

#[tauri::command(async)]
pub(crate) fn pds_set_app_kv(
    state: State<'_, PersistentStoreState>,
    key: String,
    value: Value,
) -> Result<(), StoreError> {
    with_store(state, |store| store.set_app_kv(&key, &value))
}

#[tauri::command(async)]
pub(crate) fn pds_remove_app_kv(
    state: State<'_, PersistentStoreState>,
    key: String,
) -> Result<(), StoreError> {
    with_store(state, |store| store.remove_app_kv(&key))
}
