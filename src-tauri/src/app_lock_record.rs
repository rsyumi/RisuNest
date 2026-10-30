use std::path::Path;
use serde_json::Value;
use tauri::{AppHandle, Manager};
use crate::persistent_store::{device_store::{DeviceStore, DEVICE_DATABASE_FILE}, PersistentStoreState, StoreError};

const KEY: &str = "risuNestAppLock";

fn read_record(root: &Path) -> Result<Option<String>, StoreError> {
    let directory = root.join("persistent");
    if !directory.join(DEVICE_DATABASE_FILE).try_exists()? { return Ok(None); }
    let store = DeviceStore::open(&directory)?;
    Ok(store.read_setting(KEY)?.map(|value| match value {
        Value::String(record) => record,
        other => other.to_string(),
    }))
}

fn write_record(root: &Path, record: &str) -> Result<(), StoreError> {
    let directory = root.join("persistent");
    std::fs::create_dir_all(&directory)?;
    DeviceStore::open(&directory)?.write_setting(KEY, &Value::String(record.to_owned()))
}

async fn record_operation<T: Send + 'static>(operation: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(operation).await.map_err(|error| error.to_string())?
}

#[tauri::command]
pub(crate) async fn app_lock_read_record(app: AppHandle) -> Result<Option<String>, String> {
    record_operation(move || {
        let state = app.state::<PersistentStoreState>();
        let _guard = state.admit_renderer_operation().map_err(|error| error.to_string())?;
        read_record(&crate::app_paths::data_root(&app)?).map_err(|error| error.to_string())
    }).await
}

#[tauri::command]
pub(crate) async fn app_lock_write_record(app: AppHandle, record: String) -> Result<(), String> {
    record_operation(move || {
        let state = app.state::<PersistentStoreState>();
        let _guard = state.admit_renderer_operation().map_err(|error| error.to_string())?;
        let root = crate::app_paths::data_root(&app)?;
        state.outside_store_open(|| write_record(&root, &record)).and_then(|written| written).map_err(|error| error.to_string())
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_device_store_yields_without_blocking_the_command_thread() {
        use std::{future::Future, sync::Arc, task::{Context, Poll, Wake, Waker}};
        struct Noop;
        impl Wake for Noop { fn wake(self: Arc<Self>) {} }
        let root = tempfile::tempdir().unwrap();
        write_record(root.path(), "before").unwrap();
        let connection = rusqlite::Connection::open(root.path().join("persistent").join(DEVICE_DATABASE_FILE)).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        let path = root.path().to_owned();
        let mut operation = Box::pin(record_operation(move || write_record(&path, "after").map_err(|error| error.to_string())));
        let waker = Waker::from(Arc::new(Noop));
        assert!(matches!(operation.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Pending));
        connection.execute_batch("ROLLBACK").unwrap();
        tauri::async_runtime::block_on(operation).unwrap();
        assert_eq!(read_record(root.path()).unwrap().as_deref(), Some("after"));
    }

    #[test]
    fn recovery_record_survives_reopen_without_opening_a_library() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_record(root.path()).unwrap(), None);
        assert!(!root.path().join("persistent").exists());
        let record = r#"{"schema":"risunest.app-lock/v1","enabled":false}"#;
        write_record(root.path(), record).unwrap();
        assert_eq!(read_record(root.path()).unwrap().as_deref(), Some(record));
        assert!(!root.path().join("persistent").join(crate::persistent_store::DATABASE_FILE).exists());
    }

    #[test]
    fn a_broken_device_store_is_an_error_and_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("persistent");
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join(DEVICE_DATABASE_FILE);
        std::fs::write(&file, b"synthetic damaged database").unwrap();
        assert!(read_record(root.path()).is_err());
        assert!(write_record(root.path(), "{}").is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"synthetic damaged database");
    }
}
