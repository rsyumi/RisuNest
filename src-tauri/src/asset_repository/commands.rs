use super::{PayloadCas, PreparedPayload};
use tauri::{AppHandle, Manager};

fn repository_root(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_prepare(
    app: AppHandle,
    data: Vec<u8>,
) -> Result<PreparedPayload, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        PayloadCas::new(&root)
            .and_then(|cas| cas.prepare_bytes(&data))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS prepare operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_read_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        PayloadCas::new(&root)
            .and_then(|cas| cas.read_object(&content_hash))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS read operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_read_object_range(
    app: AppHandle,
    content_hash: String,
    start: u64,
    end_exclusive: u64,
) -> Result<Option<Vec<u8>>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        PayloadCas::new(&root)
            .and_then(|cas| cas.read_object_range(&content_hash, start, end_exclusive))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS range operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn asset_cas_stat_object(
    app: AppHandle,
    content_hash: String,
) -> Result<Option<u64>, String> {
    let root = repository_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        PayloadCas::new(&root)
            .and_then(|cas| cas.stat_object(&content_hash))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("failed to join CAS stat operation: {error}"))?
}
