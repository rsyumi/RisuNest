use super::*;
use tauri::{AppHandle, Manager, State};

fn require_renderer_maintenance(state: &DeviceBackupState, session_id: &str) -> Result<()> {
    if !state.maintenance_entered(session_id)? {
        return Err(error(
            "device-maintenance-not-entered",
            "The native maintenance document must finish loading before renderer changes",
        ));
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_bootstrap(
    app: AppHandle,
    state: State<'_, DeviceBackupState>,
    fresh_bootstrap: Option<bool>,
) -> Result<BootstrapDecision> {
    if state.is_blocking()? {
        require(
            app.webview_windows().len() == 1,
            "Device maintenance requires one WebView with all previous plugin contexts closed",
        )?;
    }
    state.bootstrap_for_entry(fresh_bootstrap.unwrap_or(false))
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_section_begin(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
    metadata_json: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.section_begin(&session_id, spool, &section_id, &metadata_json)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_row_append(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
    ordinal: u64,
    payload_json: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.row_append(&session_id, spool, &section_id, ordinal, &payload_json)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_row_append_from_blob(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
    ordinal: u64,
    sha256: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.row_append_from_blob(&session_id, spool, &section_id, ordinal, &sha256)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_section_finish(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
) -> Result<SectionManifest> {
    require_renderer_maintenance(&state, &session_id)?;
    state.section_finish(&session_id, spool, &section_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_section_list(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    after_section_id: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<SectionManifest>> {
    state.section_page(
        &session_id,
        spool,
        after_section_id.as_deref(),
        limit.unwrap_or(128),
    )
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_row_read(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
    after_ordinal: Option<u64>,
    limit: u32,
) -> Result<RowPage> {
    state.row_read(&session_id, spool, &section_id, after_ordinal, limit)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_row_read_bytes(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    section_id: String,
    ordinal: u64,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    state.row_read_bytes(&session_id, spool, &section_id, ordinal, offset, length)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_blob_begin(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    object_id: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.blob_begin(&session_id, spool, &object_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_blob_append(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    object_id: String,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.blob_append(&session_id, spool, &object_id, offset, &bytes)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_blob_finish(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    object_id: String,
) -> Result<BlobManifest> {
    require_renderer_maintenance(&state, &session_id)?;
    state.blob_finish(&session_id, spool, &object_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_blob_read(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    spool: Spool,
    object_id: String,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    state.blob_read(&session_id, spool, &object_id, offset, length)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_prepared(
    state: State<'_, DeviceBackupState>,
    session_id: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.prepared(&session_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_section_intent(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    section_id: String,
    rollback: bool,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.section_intent(&session_id, &section_id, rollback)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_section_complete(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    section_id: String,
    rollback: bool,
    digest: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.section_complete(&session_id, &section_id, rollback, &digest)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_finish_device(
    state: State<'_, DeviceBackupState>,
    session_id: String,
) -> Result<Session> {
    require_renderer_maintenance(&state, &session_id)?;
    state.finish_device(&session_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_recovery_complete(
    state: State<'_, DeviceBackupState>,
    session_id: String,
) -> Result<()> {
    require_renderer_maintenance(&state, &session_id)?;
    state.recovery_complete(&session_id)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_fail(
    state: State<'_, DeviceBackupState>,
    session_id: String,
    code: String,
    detail: Option<FailureDetail>,
) -> Result<Session> {
    require_renderer_maintenance(&state, &session_id)?;
    state.fail_with_detail(&session_id, &code, detail)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_retry_recovery(
    state: State<'_, DeviceBackupState>,
    session_id: String,
) -> Result<Session> {
    require_renderer_maintenance(&state, &session_id)?;
    state.retry_recovery(&session_id)
}
