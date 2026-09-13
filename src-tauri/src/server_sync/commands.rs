use super::{
    client::{ServerClient, ServerConfig},
    Result, SyncError,
};
use crate::persistent_store::{
    commands::with_store_mut,
    server_sync_engine::{CycleOptions, CycleResult, Preparation, PreparedCycle},
    server_sync_journal::ReplicaStatus,
    PersistentStore,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tauri::{AppHandle, Manager};

struct PreparedJob {
    id: String,
    store: PersistentStore,
    cycle: PreparedCycle,
    // Drop after staged stores and cycle resources have settled.
    _admission: crate::native_file_jobs::admission::Permit,
}
#[derive(Default)]
pub(crate) struct ServerSyncCommandState {
    running: AtomicBool,
    cancelled: Mutex<Arc<AtomicBool>>,
    prepared: Mutex<Option<PreparedJob>>,
    verified_bytes: Mutex<Arc<std::sync::atomic::AtomicU64>>,
    backup_sources: Mutex<BTreeMap<String, String>>,
}
struct Running<'a>(&'a ServerSyncCommandState);
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::Release);
    }
}
impl ServerSyncCommandState {
    fn claim(&self) -> Result<Running<'_>> {
        self.running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| SyncError::new("server-sync-busy", 409))?;
        Ok(Running(self))
    }
    fn claim_preparation(&self) -> Result<(Running<'_>, Arc<AtomicBool>)> {
        let mut cancelled = self
            .cancelled
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        let running = self.claim()?;
        self.require_no_preparation()?;
        let flag = Arc::new(AtomicBool::new(false));
        *cancelled = flag.clone();
        Ok((running, flag))
    }
    fn require_no_preparation(&self) -> Result<()> {
        if self
            .prepared
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
            .is_some()
        {
            return Err(SyncError::new("server-sync-preparation-pending", 409));
        }
        Ok(())
    }
}
fn claim_library(app: &AppHandle) -> Result<crate::native_file_jobs::admission::Permit> {
    app.state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .server()
        .map_err(|code| SyncError::new(code, 409))
}
fn job_store(app: &AppHandle) -> Result<PersistentStore> {
    Ok(with_store_mut(app.state(), |store| {
        store.open_native_job_store()
    })?)
}
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|_| SyncError::new("server-sync-worker-unavailable", 503))?
}

fn pinned_backups(state: &ServerSyncCommandState) -> Result<BTreeSet<String>> {
    Ok(state
        .backup_sources
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
        .values()
        .cloned()
        .collect())
}
fn management_block(store: &PersistentStore) -> Result<Option<&'static str>> {
    Ok(store
        .server_status()?
        .operation_pending
        .then_some("resolve-pending-operation-first"))
}
#[tauri::command]
pub(crate) async fn server_sync_backup_inventory(
    app: AppHandle,
    before: Option<super::management::BackupCursor>,
) -> Result<super::management::BackupInventory> {
    blocking(move || {
        let state = app.state::<ServerSyncCommandState>();
        let store = job_store(&app)?;
        let block =
            if state.running.load(Ordering::Acquire) || state.require_no_preparation().is_err() {
                Some("server-sync-busy")
            } else {
                management_block(&store)?
            };
        super::management::inventory(
            store.repository_root(),
            before.as_ref(),
            block,
            &pinned_backups(&state)?,
        )
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_backup_cleanup(
    app: AppHandle,
) -> Result<super::management::DeletionCleanup> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        let store = job_store(&app)?;
        super::management::cleanup_deleted_backups(
            store.repository_root(),
            management_block(&store)?,
            &pinned_backups(&state)?,
        )
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_backup_delete(app: AppHandle, id: String) -> Result<()> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        let store = job_store(&app)?;
        super::management::delete_backup(
            store.repository_root(),
            &id,
            management_block(&store)?,
            &pinned_backups(&state)?,
        )
    })
    .await
}
fn manage_cache(app: &AppHandle, clean: bool) -> Result<super::management::CacheUsage> {
    let state = app.state::<ServerSyncCommandState>();
    let _admission = if clean {
        Some(claim_library(app)?)
    } else {
        None
    };
    let _running = if clean { Some(state.claim()?) } else { None };
    if clean {
        state.require_no_preparation()?;
    }
    let store = job_store(app)?;
    let mut block = management_block(&store)?;
    if !clean && (state.running.load(Ordering::Acquire) || state.require_no_preparation().is_err())
    {
        block = Some("server-sync-busy");
    }
    let backups = super::management::inventory(
        store.repository_root(),
        None,
        block,
        &pinned_backups(&state)?,
    )?;
    if backups.incomplete_count > 0 {
        block = Some("incomplete-preservation");
    }
    if !pinned_backups(&state)?.is_empty() {
        block = Some("backup-in-use");
    }
    let active = store.server_stored_config()?.map(|config| {
        risunest_sync_wire::hash(format!("{}:{}", config.library_id, config.device_id).as_bytes())
    });
    let mut references = BTreeSet::new();
    if block.is_none() {
        if let Some(id) = &active {
            let path = store.repository_root().join("server-sync").join(id);
            if path.exists() {
                let cache = super::cache::Cache {
                    cas: crate::asset_repository::PayloadCas::new(&path)?,
                };
                match store.server_cache_references(&cache) {
                    Ok(hashes) => references = hashes,
                    Err(_) => block = Some("cache-references-unavailable"),
                }
            }
        }
    }
    super::management::cache_usage(
        store.repository_root(),
        active.as_deref(),
        &references,
        block,
        clean,
    )
}
#[tauri::command]
pub(crate) async fn server_sync_cache_usage(
    app: AppHandle,
) -> Result<super::management::CacheUsage> {
    blocking(move || manage_cache(&app, false)).await
}
#[tauri::command]
pub(crate) async fn server_sync_cache_cleanup(
    app: AppHandle,
) -> Result<super::management::CacheUsage> {
    blocking(move || manage_cache(&app, true)).await
}
#[tauri::command]
pub(crate) async fn server_sync_backups(app: AppHandle) -> Result<Vec<super::backups::Backup>> {
    blocking(move || {
        let store = job_store(&app)?;
        super::backups::list(store.repository_root())
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_backup_source(
    app: AppHandle,
    id: String,
    side: super::backups::Side,
) -> Result<BackupSource> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let (_running, cancelled) = state.claim_preparation()?;
        let store = job_store(&app)?;
        if store.server_status()?.operation_pending {
            return Err(SyncError::new("resolve-pending-operation-first", 409));
        }
        let path = super::backups::source(store.repository_root(), &id, side, || {
            cancelled.load(Ordering::Acquire)
        })?;
        let lease = uuid::Uuid::new_v4().to_string();
        state
            .backup_sources
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
            .insert(lease.clone(), id);
        Ok(BackupSource { path, lease })
    })
    .await
}

#[derive(Serialize)]
pub(crate) struct BackupSource {
    path: String,
    lease: String,
}

#[tauri::command]
pub(crate) fn server_sync_backup_release(app: AppHandle, lease: String) -> Result<()> {
    app.state::<ServerSyncCommandState>()
        .backup_sources
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
        .remove(&lease);
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum PreparedReply {
    Report {
        result: CycleResult,
    },
    Ready {
        #[serde(rename = "preparationId")]
        preparation_id: String,
        #[serde(rename = "localRevision")]
        local_revision: i64,
        head: risunest_sync_wire::RemoteHead,
        #[serde(rename = "appliedRecords")]
        applied_records: usize,
    },
}
#[tauri::command]
pub(crate) async fn server_sync_status(app: AppHandle) -> Result<ReplicaStatus> {
    blocking(move || job_store(&app)?.server_status()).await
}
#[tauri::command]
pub(crate) async fn server_sync_asset_status(
    app: AppHandle,
) -> Result<crate::persistent_store::asset_residency::ResidencyStatus> {
    blocking(move || job_store(&app)?.asset_residency_status()).await
}
#[tauri::command]
pub(crate) async fn server_sync_asset_policy(
    app: AppHandle,
    policy: super::residency::AssetPolicy,
) -> Result<crate::persistent_store::asset_residency::ResidencyStatus> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let (_running, cancelled) = state.claim_preparation()?;
        job_store(&app)?.asset_residency_set_policy(policy, || {
            if cancelled.load(Ordering::Acquire) {
                Err(SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        })
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_asset_evict(
    app: AppHandle,
) -> Result<crate::persistent_store::asset_residency::ResidencyStatus> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let (_running, cancelled) = state.claim_preparation()?;
        job_store(&app)?.asset_residency_evict(|| {
            if cancelled.load(Ordering::Acquire) {
                Err(SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        })
    })
    .await
}
#[tauri::command]
pub(crate) fn server_sync_verified_bytes(app: AppHandle) -> Result<String> {
    let state = app.state::<ServerSyncCommandState>();
    let counter = state
        .verified_bytes
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
    Ok(counter.load(Ordering::Relaxed).to_string())
}
#[tauri::command]
pub(crate) async fn server_sync_bind(
    app: AppHandle,
    config: ServerConfig,
) -> Result<ReplicaStatus> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        let mut client = ServerClient::new(config.clone())?;
        client.resolve_identity(true)?;
        let mut store = job_store(&app)?;
        store.server_bind(client.config())?;
        store.server_status()
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_unbind(app: AppHandle) -> Result<()> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        job_store(&app)?.server_unbind()
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_reregister(
    app: AppHandle,
    config: ServerConfig,
    expected_revision: i64,
) -> Result<ReplicaStatus> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        let mut store = job_store(&app)?;
        let old = store
            .server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        if old.library_id != config.library_id || old.device_id == config.device_id {
            return Err(SyncError::new("new-device-registration-required", 409));
        }
        let mut client = ServerClient::new(config.clone())?;
        client.resolve_identity(true)?;
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct DeviceStatus {
            device_id: String,
            active: bool,
        }
        let (_, status): (_, DeviceStatus) = client.json(
            reqwest::Method::GET,
            &format!("devices/{}/status", old.device_id),
            &[],
            None::<&()>,
            &[],
        )?;
        if status.device_id != old.device_id {
            return Err(SyncError::new("device-identity-mismatch", 409));
        }
        if status.active {
            return Err(SyncError::new("revoke-previous-device-first", 409));
        }
        store.server_replace_registration(client.config(), expected_revision)?;
        store.server_status()
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_reconcile(
    app: AppHandle,
    expected_revision: i64,
) -> Result<ReplicaStatus> {
    blocking(move || {
        let _admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        state.require_no_preparation()?;
        let mut store = job_store(&app)?;
        let config = store
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let mut client = ServerClient::new(config.clone())?;
        let head = client.resolve_identity(false)?;
        store.server_cache_endpoint(&config, client.config())?;
        store.server_reconcile_epoch(&head, expected_revision)?;
        store.server_status()
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_prepare(
    app: AppHandle,
    mut options: CycleOptions,
) -> Result<PreparedReply> {
    blocking(move || {
        let admission = claim_library(&app)?;
        let state = app.state::<ServerSyncCommandState>();
        let (_running, flag) = state.claim_preparation()?;
        state.require_no_preparation()?;
        options.cancellation = Some(flag);
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        *state
            .verified_bytes
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))? = counter.clone();
        options.verified_bytes = Some(counter);
        let mut store = job_store(&app)?;
        match store.server_prepare_cycle(&options)? {
            Preparation::Report(result) => Ok(PreparedReply::Report { result }),
            Preparation::Ready(cycle) => {
                let id = uuid::Uuid::new_v4().to_string();
                let reply = PreparedReply::Ready {
                    preparation_id: id.clone(),
                    local_revision: cycle.revision,
                    head: cycle.through.clone(),
                    applied_records: cycle.applied,
                };
                if options
                    .cancellation
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Acquire))
                {
                    return Err(SyncError::new("cancelled", 409));
                }
                *state
                    .prepared
                    .lock()
                    .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))? =
                    Some(PreparedJob {
                        id,
                        _admission: admission,
                        store,
                        cycle,
                    });
                Ok(reply)
            }
        }
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_activate(app: AppHandle, preparation_id: String) -> Result<i64> {
    blocking(move || {
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        let mut slot = state
            .prepared
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        if !slot.as_ref().is_some_and(|job| job.id == preparation_id) {
            return Err(SyncError::new("stale-server-preparation", 409));
        }
        let job = slot.as_mut().unwrap();
        job.store.server_activate_cycle(&mut job.cycle)
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_publish(
    app: AppHandle,
    preparation_id: String,
) -> Result<CycleResult> {
    blocking(move || {
        let state = app.state::<ServerSyncCommandState>();
        let _running = state.claim()?;
        let mut slot = state
            .prepared
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        if !slot.as_ref().is_some_and(|job| job.id == preparation_id) {
            return Err(SyncError::new("stale-server-preparation", 409));
        }
        let mut job = slot.take().unwrap();
        drop(slot);
        job.store.server_publish_cycle(&job.cycle)
    })
    .await
}
#[tauri::command]
pub(crate) fn server_sync_cancel(app: AppHandle) -> Result<()> {
    let state = app.state::<ServerSyncCommandState>();
    let cancelled = state
        .cancelled
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
    cancelled.store(true, Ordering::Release);
    if !state.running.load(Ordering::Acquire) {
        state
            .prepared
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
            .take();
    }
    Ok(())
}
