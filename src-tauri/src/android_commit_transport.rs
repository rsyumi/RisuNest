//! Bounded strings or ArrayBuffer packets. No partial payload changes the store.
use crate::persistent_store::{
    commands::with_store_mut, AssetAlias, RevisionResult, StoreError, StoreResult, WorkingSetCommit,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, MutexGuard};
use tauri::{AppHandle, Manager, State, WebviewWindow};

const CAPACITY: usize = 32 * 1024;
const BINARY_CAPACITY: usize = 256 * 1024;
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
    binary: bool,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Pool {
    transfer: Option<Transfer>,
    finishing: Option<String>,
}
impl Pool {
    fn append_packet(&mut self, packet: &[u8]) -> StoreResult<usize> {
        if packet.len() <= 40 || packet.len() > 40 + BINARY_CAPACITY {
            return Err(invalid("invalid Android binary packet size"));
        }
        let id = std::str::from_utf8(&packet[..36]).map_err(|_| invalid("invalid binary ID"))?;
        let offset = u32::from_le_bytes(packet[36..40].try_into().unwrap()) as usize;
        self.append_bytes(id, offset, &packet[40..], true)
    }
    fn open(&mut self, id: String, total: usize, binary: bool) -> StoreResult<()> {
        if uuid::Uuid::parse_str(&id)
            .map(|uuid| uuid.to_string() != id)
            .unwrap_or(true)
            || total == 0
            || total > MAX_BYTES
        {
            return Err(invalid("invalid Android commit size or ID"));
        }
        if self.transfer.is_some() || self.finishing.is_some() {
            return Err(invalid("Android commit already active"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| invalid("Android commit allocation failed"))?;
        self.transfer = Some(Transfer {
            id,
            total,
            binary,
            bytes,
        });
        Ok(())
    }

    fn append(&mut self, id: &str, offset: usize, chunk: &str) -> StoreResult<usize> {
        self.append_bytes(id, offset, chunk.as_bytes(), false)
    }
    fn append_bytes(
        &mut self,
        id: &str,
        offset: usize,
        chunk: &[u8],
        binary: bool,
    ) -> StoreResult<usize> {
        let transfer = self
            .transfer
            .as_mut()
            .ok_or_else(|| invalid("no active Android commit"))?;
        if transfer.binary != binary
            || transfer.id != id
            || offset != transfer.bytes.len()
            || chunk.is_empty()
            || chunk.len() > if binary { BINARY_CAPACITY } else { CAPACITY }
            || chunk.len() > transfer.total - transfer.bytes.len()
        {
            return Err(invalid("invalid Android commit chunk"));
        }
        transfer.bytes.extend_from_slice(chunk);
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

#[derive(Clone, Default)]
pub(crate) struct AndroidCommitState(Arc<Mutex<Pool>>);

// Tauri commands and the native WebView listener share the same allocation budget.
#[cfg(target_os = "android")]
pub(crate) fn native_state() -> &'static AndroidCommitState {
    static STATE: std::sync::OnceLock<AndroidCommitState> = std::sync::OnceLock::new();
    STATE.get_or_init(AndroidCommitState::default)
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_rsyumi_risunest_AndroidCommitNative_append(
    env: jni::JNIEnv,
    _class: jni::objects::JClass,
    packet: jni::objects::JByteArray,
) -> jni::sys::jint {
    let result = (|| {
        let length = env.get_array_length(&packet).ok()? as usize;
        if length <= 40 || length > 40 + BINARY_CAPACITY {
            return None;
        }
        // Bounded owned packet; no Java pointer survives this call.
        let bytes = env.convert_byte_array(&packet).ok()?;
        native_state().lock().ok()?.append_packet(&bytes).ok()
    })();
    result.map(|offset| offset as i32).unwrap_or(-1)
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_rsyumi_risunest_AndroidCommitNative_reset(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
) {
    native_state().reset();
}
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
    binary: bool,
) -> StoreResult<Opened> {
    guard(&window)?;
    state.lock()?.open(id, total_bytes, binary)?;
    Ok(Opened {
        capacity: if binary { BINARY_CAPACITY } else { CAPACITY },
    })
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

    fn packet(id: &str, offset: u32, bytes: &[u8]) -> Vec<u8> {
        let mut packet = id.as_bytes().to_vec();
        packet.extend_from_slice(&offset.to_le_bytes());
        packet.extend_from_slice(bytes);
        packet
    }

    #[test]
    fn binary_packets_preserve_bytes_and_enforce_mode_bounds_owner_and_order() {
        let mut pool = Pool::default();
        let token = id();
        let bytes = "a".repeat(BINARY_CAPACITY - 1) + &"🐿️한글".repeat(100);
        pool.open(token.clone(), bytes.len(), true).unwrap();
        assert!(pool.append(&token, 0, "a").is_err());
        for bad in [
            vec![],
            vec![0; 40],
            vec![0; 41],
            packet(&token, 0, &vec![0; BINARY_CAPACITY + 1]),
            packet(&id(), 0, b"a"),
            packet(&token, 1, b"a"),
        ] {
            assert!(pool.append_packet(&bad).is_err());
        }
        for (i, chunk) in bytes.as_bytes().chunks(BINARY_CAPACITY).enumerate() {
            let offset = (i * BINARY_CAPACITY) as u32;
            assert_eq!(
                pool.append_packet(&packet(&token, offset, chunk)).unwrap(),
                offset as usize + chunk.len()
            );
            assert!(pool.append_packet(&packet(&token, offset, chunk)).is_err());
        }
        assert_eq!(pool.take(&token).unwrap(), bytes.as_bytes());
        pool.reset();
        assert!(pool.open(id(), 1, true).is_err());
        pool.complete(&token);
        pool.open(token.clone(), 1, false).unwrap();
        assert!(pool.append_packet(&packet(&token, 0, b"a")).is_err());
        pool.reset();
        pool.open(token.clone(), 2, true).unwrap();
        pool.append_packet(&packet(&token, 0, b"a")).unwrap();
        assert!(pool.take(&token).is_err());
        pool.cancel(&token);
        assert!(pool.append_packet(&packet(&token, 1, b"b")).is_err());
    }

    #[test]
    fn rejects_bad_sizes_ids_ranges_replays_and_overlapping_producers() {
        let mut pool = Pool::default();
        for total in [0, MAX_BYTES + 1, usize::MAX] {
            assert!(pool.open(id(), total, false).is_err());
        }
        assert!(pool.open("bad".into(), 1, false).is_err());
        let token = id();
        pool.open(token.clone(), 6, false).unwrap();
        assert!(pool.open(id(), 1, false).is_err());
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
        assert!(pool.open(id(), 1, false).is_err()); // finish still owns the budget
        pool.complete("stale");
        assert!(pool.open(id(), 1, false).is_err());
        pool.complete(&token);
        pool.open(id(), 1, false).unwrap();
    }

    #[test]
    fn cancel_and_navigation_release_partial_payloads_only() {
        let mut pool = Pool::default();
        let token = id();
        pool.open(token.clone(), 3, false).unwrap();
        pool.append(&token, 0, "a").unwrap();
        pool.cancel("stale");
        assert!(pool.open(id(), 1, false).is_err());
        pool.cancel(&token);
        pool.open(id(), 3, false).unwrap();
        pool.reset();
        assert!(pool.append(&token, 1, "bc").is_err());
        pool.open(id(), 1, false).unwrap();
    }

    #[test]
    fn assembled_commit_keeps_exact_data_atomicity_revision_fence_and_snapshot() {
        check_durable_commit(false);
        check_durable_commit(true);
    }

    fn check_durable_commit(binary: bool) {
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
        pool.open(token.clone(), data.len(), binary).unwrap();
        let data = String::from_utf8(data).unwrap();
        let mut offset = 0;
        while offset < data.len() {
            let mut end =
                (offset + if binary { BINARY_CAPACITY } else { CAPACITY }).min(data.len());
            while !binary && !data.is_char_boundary(end) {
                end -= 1;
            }
            offset = if binary {
                pool.append_packet(&packet(
                    &token,
                    offset as u32,
                    &data.as_bytes()[offset..end],
                ))
                .unwrap()
            } else {
                pool.append(&token, offset, &data[offset..end]).unwrap()
            };
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
