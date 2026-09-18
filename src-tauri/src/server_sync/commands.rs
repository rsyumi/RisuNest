use super::{
    client::{ServerClient, ServerConfig},
    Result, SyncError,
};
use crate::persistent_store::{
    commands::with_store_mut,
    server_sync_engine::{CycleItemCounter, CycleOptions, CycleResult, Preparation, PreparedCycle},
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
struct BackupSourceLease {
    source: super::backups::ReferenceSource,
    released: bool,
    guards: usize,
}
#[derive(Default)]
pub(crate) struct ServerSyncCommandState {
    running: AtomicBool,
    cancelled: Mutex<Arc<AtomicBool>>,
    prepared: Mutex<Option<PreparedJob>>,
    verified_bytes: Mutex<Arc<std::sync::atomic::AtomicU64>>,
    cycle_items: Mutex<Arc<CycleItemCounter>>,
    retryable_failure: Arc<Mutex<Option<String>>>,
    backup_sources: Mutex<BTreeMap<String, BackupSourceLease>>,
}

pub(crate) struct ReferenceSourceGuard {
    app: AppHandle,
    token: String,
    source: super::backups::ReferenceSource,
}

impl ReferenceSourceGuard {
    pub(crate) fn source(&self) -> &super::backups::ReferenceSource { &self.source }
}

impl Drop for ReferenceSourceGuard {
    fn drop(&mut self) {
        self.app.state::<ServerSyncCommandState>().finish_reference_source(&self.token);
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CycleItemCounts {
    done: u64,
    total: u64,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BackupCleanup {
    Complete,
    Pending,
}
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupDeleteResult {
    local_deleted: bool,
    cleanup: BackupCleanup,
}
fn finish_backup_delete(cleanup: impl FnOnce() -> Result<()>) -> BackupDeleteResult {
    BackupDeleteResult {
        local_deleted: true,
        cleanup: if cleanup().is_ok() {
            BackupCleanup::Complete
        } else {
            BackupCleanup::Pending
        },
    }
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
    fn insert_reference_source(&self, token: String, source: super::backups::ReferenceSource) -> Result<()> {
        let mut sources = self.backup_sources.lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        match sources.entry(token) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(BackupSourceLease { source, released: false, guards: 0 });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(SyncError::new("server-sync-state-unavailable", 503))
            }
        }
    }
    fn claim_reference_source(&self, token: &str) -> Result<super::backups::ReferenceSource> {
        let mut sources = self.backup_sources.lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        let entry = sources.get_mut(token)
            .filter(|entry| !entry.released)
            .ok_or_else(|| SyncError::new("source-unavailable", 409))?;
        entry.guards = entry.guards.checked_add(1)
            .ok_or_else(|| SyncError::new("server-sync-state-unavailable", 503))?;
        Ok(entry.source.clone())
    }
    fn finish_reference_source(&self, token: &str) {
        let Ok(mut sources) = self.backup_sources.lock() else { return; };
        let remove = if let Some(entry) = sources.get_mut(token) {
            entry.guards = entry.guards.saturating_sub(1);
            entry.released && entry.guards == 0
        } else {
            false
        };
        if remove { sources.remove(token); }
    }
    fn release_reference_source(&self, token: &str) -> Result<()> {
        let mut sources = self.backup_sources.lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
        let remove = if let Some(entry) = sources.get_mut(token) {
            entry.released = true;
            entry.guards == 0
        } else {
            false
        };
        if remove { sources.remove(token); }
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
        .map(|entry| entry.source.id().to_owned())
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
        let cleanup = super::management::cleanup_deleted_backups(
            store.repository_root(),
            management_block(&store)?,
            &pinned_backups(&state)?,
        )?;
        store.asset_residency_release_unused(|| Ok(()))?;
        Ok(cleanup)
    })
    .await
}
#[tauri::command]
pub(crate) async fn server_sync_backup_delete(
    app: AppHandle,
    id: String,
) -> Result<BackupDeleteResult> {
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
        )?;
        Ok(finish_backup_delete(|| {
            store.asset_residency_release_unused(|| Ok(()))
        }))
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
pub(crate) async fn server_sync_backups(
    app: AppHandle,
    before: Option<super::backups::BackupCursor>,
) -> Result<super::backups::BackupList> {
    blocking(move || {
        let store = job_store(&app)?;
        super::backups::list(store.repository_root(), before.as_ref())
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
        let source = super::backups::source(store.repository_root(), &id, side, &|| {
            if cancelled.load(Ordering::Acquire) {
                Err(SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        })?;
        let lease = format!("server:{}", uuid::Uuid::new_v4());
        state.insert_reference_source(lease.clone(), source)?;
        Ok(BackupSource {
            source: ConflictReferenceSource::ConflictReference { token: lease.clone() },
            lease,
        })
    })
    .await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupSource {
    source: ConflictReferenceSource,
    lease: String,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ConflictReferenceSource {
    ConflictReference { token: String },
}

fn validate_reference_token(token: &str) -> Result<()> {
    let id = token.strip_prefix("server:")
        .ok_or_else(|| SyncError::new("source-unavailable", 409))?;
    let uuid = uuid::Uuid::parse_str(id)
        .map_err(|_| SyncError::new("source-unavailable", 409))?;
    if uuid.to_string() != id || uuid.get_version() != Some(uuid::Version::Random) {
        return Err(SyncError::new("source-unavailable", 409));
    }
    Ok(())
}

pub(crate) fn claim_reference_source(app: &AppHandle, token: &str) -> Result<ReferenceSourceGuard> {
    validate_reference_token(token)?;
    let state = app.state::<ServerSyncCommandState>();
    let source = state.claim_reference_source(token)?;
    let guard = ReferenceSourceGuard {
        app: app.clone(),
        token: token.to_owned(),
        source,
    };
    let store = job_store(app)?;
    super::backups::validate_reference_source(
        store.repository_root(), guard.source(), &|| Ok(()),
    )?;
    Ok(guard)
}

#[tauri::command]
pub(crate) fn server_sync_backup_release(app: AppHandle, lease: String) -> Result<()> {
    app.state::<ServerSyncCommandState>().release_reference_source(&lease)
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
pub(crate) fn server_sync_progress_counts(app: AppHandle) -> Result<CycleItemCounts> {
    let state = app.state::<ServerSyncCommandState>();
    let counter = state
        .cycle_items
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?;
    Ok(CycleItemCounts {
        done: counter.done.load(Ordering::Relaxed),
        total: counter.total.load(Ordering::Relaxed),
    })
}
#[tauri::command]
pub(crate) fn server_sync_retryable_failure(app: AppHandle) -> Result<Option<String>> {
    Ok(app
        .state::<ServerSyncCommandState>()
        .retryable_failure
        .lock()
        .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))?
        .clone())
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
        let client = ServerClient::new(config.clone())?;
        client.resolve_identity(true)?;
        let mut store = job_store(&app)?;
        store.server_bind(&client.config())?;
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
        let client = ServerClient::new(config.clone())?;
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
        store.server_replace_registration(&client.config(), expected_revision)?;
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
        let client = ServerClient::new(config.clone())?;
        let head = client.resolve_identity(false)?;
        store.server_cache_endpoint(&config, &client.config())?;
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
        let items = Arc::new(CycleItemCounter::default());
        *state
            .cycle_items
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))? = items.clone();
        options.cycle_items = Some(items);
        *state
            .retryable_failure
            .lock()
            .map_err(|_| SyncError::new("server-sync-state-unavailable", 503))? = None;
        options.retryable_failure = Some(state.retryable_failure.clone());
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

#[cfg(test)]
mod backup_source_tests {
    use super::*;
    use crate::server_sync::backups::{references::Capture, Side};
    use risunest_sync_wire::RemoteHead;

    #[test]
    fn deleted_backup_reports_remote_cleanup_as_complete_or_pending() {
        let complete = finish_backup_delete(|| Ok(()));
        assert_eq!(complete, BackupDeleteResult {
            local_deleted: true,
            cleanup: BackupCleanup::Complete,
        });
        assert_eq!(serde_json::to_value(complete).unwrap(), serde_json::json!({
            "localDeleted": true,
            "cleanup": "complete",
        }));

        let pending = finish_backup_delete(|| {
            Err(SyncError::new("synthetic-network-release-failure", 503))
        });
        assert_eq!(pending, BackupDeleteResult {
            local_deleted: true,
            cleanup: BackupCleanup::Pending,
        });
        assert_eq!(serde_json::to_value(pending).unwrap(), serde_json::json!({
            "localDeleted": true,
            "cleanup": "pending",
        }));
    }

    #[test]
    fn namespaced_tokens_and_worker_guards_pin_until_release_and_last_drop() {
        let root = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(root.path()).unwrap();
        let head = RemoteHead::genesis("library".into(), "epoch".into()).unwrap();
        let capture = Capture::begin(root.path(), 0, "generation", &head).unwrap();
        capture.complete_side(Side::Local).unwrap();
        capture.complete_side(Side::Remote).unwrap();
        let id = capture.finish(&mut store, &|| Ok(())).unwrap().id;
        let source = super::super::backups::source(root.path(), &id, Side::Local, &|| Ok(())).unwrap();
        let state = ServerSyncCommandState::default();
        let token = format!("server:{}", uuid::Uuid::new_v4());
        validate_reference_token(&token).unwrap();
        assert_eq!(serde_json::to_value(BackupSource {
            source: ConflictReferenceSource::ConflictReference { token: token.clone() },
            lease: token.clone(),
        }).unwrap(), serde_json::json!({
            "source": { "type": "conflictReference", "token": token.clone() },
            "lease": token.clone(),
        }));
        for invalid in ["external:00000000-0000-4000-8000-000000000000", "server:INVALID",
            "server:", "server:00000000-0000-0000-0000-000000000000",
            "server:00000000-0000-1000-8000-000000000000"] {
            assert_eq!(validate_reference_token(invalid).unwrap_err().code, "source-unavailable");
        }
        state.insert_reference_source(token.clone(), source).unwrap();
        let first = state.claim_reference_source(&token).unwrap();
        let second = state.claim_reference_source(&token).unwrap();
        assert_eq!(first.id(), id);
        assert_eq!(second.side(), Side::Local);
        assert_eq!(pinned_backups(&state).unwrap(), BTreeSet::from([id.clone()]));

        state.release_reference_source(&token).unwrap();
        assert_eq!(state.claim_reference_source(&token).unwrap_err().code, "source-unavailable");
        assert_eq!(pinned_backups(&state).unwrap(), BTreeSet::from([id.clone()]));
        state.finish_reference_source(&token);
        state.release_reference_source(&token).unwrap();
        assert_eq!(pinned_backups(&state).unwrap(), BTreeSet::from([id]));
        state.finish_reference_source(&token);
        assert!(pinned_backups(&state).unwrap().is_empty());
        state.release_reference_source(&token).unwrap();
    }
}
