//! Bounded strings over Android's JSON/JNI bridge. No partial payload changes the store.
use crate::persistent_store::{
    commands::with_store_mut, AssetAlias, RevisionResult, StoreError, StoreResult, WorkingSetCommit,
};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};
use tauri::{AppHandle, Manager, State, WebviewWindow};

const CAPACITY: usize = 32 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.to_owned(),
    }
}
fn guard(window: &WebviewWindow) -> StoreResult<()> {
    if window.label() != "main" {
        return Err(invalid("commit transport requires the main webview"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    commit: WorkingSetCommit,
    asset_aliases: Vec<AssetAlias>,
}
fn decode(bytes: &[u8]) -> StoreResult<Envelope> {
    serde_json::from_slice(bytes).map_err(|_| invalid("invalid commit envelope JSON"))
}

struct Transfer {
    id: String,
    total: usize,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Pool {
    transfer: Option<Transfer>,
    finishing: Option<String>,
}
impl Pool {
    fn open(&mut self, id: String, total: usize) -> StoreResult<()> {
        if uuid::Uuid::parse_str(&id).is_err() || total == 0 || total > MAX_BYTES {
            return Err(invalid("invalid Android commit size or ID"));
        }
        if self.transfer.is_some() || self.finishing.is_some() {
            return Err(invalid("Android commit already active"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| invalid("Android commit allocation failed"))?;
        self.transfer = Some(Transfer { id, total, bytes });
        Ok(())
    }

    fn append(&mut self, id: &str, offset: usize, chunk: &str) -> StoreResult<usize> {
        let transfer = self
            .transfer
            .as_mut()
            .ok_or_else(|| invalid("no active Android commit"))?;
        if transfer.id != id
            || offset != transfer.bytes.len()
            || chunk.is_empty()
            || chunk.len() > CAPACITY
            || chunk.len() > transfer.total - transfer.bytes.len()
        {
            return Err(invalid("invalid Android commit chunk"));
        }
        transfer.bytes.extend_from_slice(chunk.as_bytes());
        Ok(transfer.bytes.len())
    }

    fn take(&mut self, id: &str) -> StoreResult<Vec<u8>> {
        let transfer = self
            .transfer
            .as_ref()
            .ok_or_else(|| invalid("no active Android commit"))?;
        if transfer.id != id || transfer.bytes.len() != transfer.total {
            return Err(invalid("incomplete or stale Android commit"));
        }
        self.finishing = Some(id.to_owned());
        Ok(self.transfer.take().unwrap().bytes)
    }

    fn cancel(&mut self, id: &str) {
        if self
            .transfer
            .as_ref()
            .is_some_and(|transfer| transfer.id == id)
        {
            self.transfer = None;
        }
        // A submitted transaction remains authoritative even if its caller disappears.
    }
    fn complete(&mut self, id: &str) {
        if self.finishing.as_deref() == Some(id) {
            self.finishing = None;
        }
    }
    fn reset(&mut self) {
        self.transfer = None;
    }
}

#[derive(Default)]
pub(crate) struct AndroidCommitState(Mutex<Pool>);
impl AndroidCommitState {
    fn lock(&self) -> StoreResult<MutexGuard<'_, Pool>> {
        self.0
            .lock()
            .map_err(|_| invalid("Android commit mutex poisoned"))
    }
    pub(crate) fn reset(&self) {
        if let Ok(mut pool) = self.lock() {
            pool.reset();
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Opened {
    capacity: usize,
}

#[tauri::command(async)]
pub(crate) fn pds_commit_android_open(
    window: WebviewWindow,
    state: State<'_, AndroidCommitState>,
    id: String,
    total_bytes: usize,
) -> StoreResult<Opened> {
    guard(&window)?;
    state.lock()?.open(id, total_bytes)?;
    Ok(Opened { capacity: CAPACITY })
}

#[tauri::command(async)]
pub(crate) fn pds_commit_android_chunk(
    window: WebviewWindow,
    state: State<'_, AndroidCommitState>,
    id: String,
    offset: usize,
    chunk: String,
) -> StoreResult<usize> {
    guard(&window)?;
    state.lock()?.append(&id, offset, &chunk)
}

// Keep the single allocation budget occupied through parsing and the store transaction.
struct FinishGuard {
    app: AppHandle,
    id: String,
}
impl Drop for FinishGuard {
    fn drop(&mut self) {
        if let Ok(mut pool) = self.app.state::<AndroidCommitState>().lock() {
            pool.complete(&self.id);
        }
    }
}

#[tauri::command]
pub(crate) async fn pds_commit_android_finish(
    app: AppHandle,
    window: WebviewWindow,
    state: State<'_, AndroidCommitState>,
    id: String,
) -> StoreResult<RevisionResult> {
    guard(&window)?;
    let bytes = state.lock()?.take(&id)?;
    let lease = FinishGuard { app, id };
    tauri::async_runtime::spawn_blocking(move || {
        let envelope = decode(&bytes)?;
        with_store_mut(lease.app.state(), |store| {
            store.commit_with_asset_aliases(&envelope.commit, &envelope.asset_aliases)
        })
    })
    .await
    .map_err(|_| invalid("Android commit task failed"))?
}

#[tauri::command(async)]
pub(crate) fn pds_commit_android_cancel(
    window: WebviewWindow,
    state: State<'_, AndroidCommitState>,
    id: String,
) -> StoreResult<()> {
    guard(&window)?;
    state.lock()?.cancel(&id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::PersistentStore;
    use serde_json::json;

    fn id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn rejects_bad_sizes_ids_ranges_replays_and_overlapping_producers() {
        let mut pool = Pool::default();
        for total in [0, MAX_BYTES + 1, usize::MAX] {
            assert!(pool.open(id(), total).is_err());
        }
        assert!(pool.open("bad".into(), 1).is_err());
        let token = id();
        pool.open(token.clone(), 6).unwrap();
        assert!(pool.open(id(), 1).is_err());
        for (key, offset, chunk) in [
            ("stale", 0, "a"),
            (&*token, 1, "a"),
            (&*token, usize::MAX, "a"),
            (&*token, 0, ""),
            (&*token, 0, "1234567"),
        ] {
            assert!(pool.append(key, offset, chunk).is_err());
        }
        assert!(pool.append(&token, 0, &"x".repeat(CAPACITY + 1)).is_err());
        assert!(pool.take(&token).is_err());
        assert_eq!(pool.append(&token, 0, "한글").unwrap(), 6);
        assert!(pool.append(&token, 0, "a").is_err());
        assert!(pool.take("stale").is_err());
        assert_eq!(pool.take(&token).unwrap(), "한글".as_bytes());
        assert!(pool.take(&token).is_err());
        pool.cancel(&token);
        pool.reset();
        assert!(pool.open(id(), 1).is_err()); // finish still owns the budget
        pool.complete("stale");
        assert!(pool.open(id(), 1).is_err());
        pool.complete(&token);
        pool.open(id(), 1).unwrap();
    }

    #[test]
    fn cancel_and_navigation_release_partial_payloads_only() {
        let mut pool = Pool::default();
        let token = id();
        pool.open(token.clone(), 3).unwrap();
        pool.append(&token, 0, "a").unwrap();
        pool.cancel("stale");
        assert!(pool.open(id(), 1).is_err());
        pool.cancel(&token);
        pool.open(id(), 3).unwrap();
        pool.reset();
        assert!(pool.append(&token, 1, "bc").is_err());
        pool.open(id(), 1).unwrap();
    }

    #[test]
    fn assembled_commit_keeps_exact_data_atomicity_revision_fence_and_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = store.replace_begin().unwrap();
        store
            .replace_put_root(&stage.staging_id, &json!({"username":"before"}))
            .unwrap();
        store.replace_commit(&stage.staging_id, Some(0)).unwrap();
        let snapshot = store.acquire_revision(1).unwrap();
        let original = store.read_root(None).unwrap();
        let text = "한글 🐿️\\\"\n".repeat(10_000);
        let data = serde_json::to_vec(&json!({"commit": {"expectedRevision":1,
            "rootMutations":[{"type":"set","key":"username","value":text}]},"assetAliases":[]}))
        .unwrap();
        let mut pool = Pool::default();
        let token = id();
        pool.open(token.clone(), data.len()).unwrap();
        let data = String::from_utf8(data).unwrap();
        let mut offset = 0;
        while offset < data.len() {
            let mut end = (offset + CAPACITY).min(data.len());
            while !data.is_char_boundary(end) {
                end -= 1;
            }
            offset = pool.append(&token, offset, &data[offset..end]).unwrap();
            assert_eq!(store.read_root(None).unwrap(), original);
        }
        let envelope = decode(&pool.take(&token).unwrap()).unwrap();
        assert_eq!(
            store
                .commit_with_asset_aliases(&envelope.commit, &envelope.asset_aliases)
                .unwrap()
                .revision,
            2
        );
        pool.complete(&token);
        assert_eq!(
            store.read_root(None).unwrap().value["username"],
            json!(text)
        );
        assert_eq!(store.read_root(Some(&snapshot.lease)).unwrap(), original);
        assert!(matches!(
            store.commit_with_asset_aliases(&envelope.commit, &envelope.asset_aliases),
            Err(StoreError::RevisionConflict { .. })
        ));
        assert_eq!(store.read_root(None).unwrap().revision, 2);
        assert!(decode(b"{bad").is_err());
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        assert_eq!(
            store.read_root(None).unwrap().value["username"],
            json!(text)
        );
    }
}
