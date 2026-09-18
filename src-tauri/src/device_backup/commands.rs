use super::*;
use crate::asset_repository::job_pins::{CasReleaseOutcome, DurableCasJob};
use std::io::ErrorKind;
use tauri::{AppHandle, Manager, State};

fn release_native_restore_pins(
    session: &Session,
    root: &std::path::Path,
    outcome: CasReleaseOutcome,
) -> Result<()> {
    if !session.includes_library {
        return Ok(());
    }
    match DurableCasJob::open(root, &session.job_id) {
        Ok(mut pins) => pins.release(outcome).map_err(|_| {
            error(
                "device-storage-failed",
                "Native portable recovery could not release durable asset pins",
            )
        }),
        Err(failure) if failure.kind() == ErrorKind::NotFound => Ok(()),
        Err(_) => Err(error(
            "device-storage-failed",
            "Native portable recovery could not open durable asset pins",
        )),
    }
}

fn complete_native_recovery_with(
    state: &DeviceBackupState,
    session_id: &str,
    release: impl FnOnce(&Session, &std::path::Path) -> Result<()>,
) -> Result<()> {
    let session = state.session(session_id)?;
    if session.profile == "native-portable" && session.includes_library {
        if session.phase != "committed" {
            return Err(error(
                "device-invalid-state",
                "Native portable restore pins can only be released after commit",
            ));
        }
        release(&session, state.repository_root())?;
    }
    state.recovery_complete(session_id)
}

fn complete_native_recovery(state: &DeviceBackupState, session_id: &str) -> Result<()> {
    complete_native_recovery_with(state, session_id, |session, root| {
        release_native_restore_pins(session, root, CasReleaseOutcome::Committed)
    })
}

#[cfg(test)]
pub(super) fn complete_native_recovery_for_test(
    state: &DeviceBackupState,
    session_id: &str,
    release: impl FnOnce(&Session, &std::path::Path) -> Result<()>,
) -> Result<()> {
    complete_native_recovery_with(state, session_id, release)
}

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
    let decision = state.bootstrap_for_entry(fresh_bootstrap.unwrap_or(false))?;
    let Some(session) = decision.session.as_ref() else {
        return Ok(decision);
    };
    if session.profile != "native-portable" {
        return Ok(decision);
    }
    if session.phase == "committed" {
        return Ok(decision);
    }
    if matches!(
        session.phase.as_str(),
        "loading-source" | "preparing" | "awaiting-native-preparation"
    ) {
        release_native_restore_pins(
            session,
            state.repository_root(),
            CasReleaseOutcome::Aborted,
        )?;
        state.fail(&session.session_id, "interrupted-before-native-apply")?;
        state.recovery_complete(&session.session_id)?;
        return state.bootstrap_for_entry(false);
    }
    if matches!(
        session.phase.as_str(),
        "prepared" | "applying-device" | "committing-library"
    ) {
        let mut store = crate::persistent_store::PersistentStore::open(state.repository_root())
            .map_err(|_| {
                error(
                    "device-storage-failed",
                    "Native portable recovery could not open persistent storage",
                )
            })?;
        resume_journaled_native_restore(&state, &session.session_id, &mut store)?;
        let decision = state.bootstrap_for_entry(false)?;
        decision.session.as_ref().ok_or_else(|| {
            error(
                "device-invalid-state",
                "Native portable recovery lost its committed session",
            )
        })?;
        return Ok(decision);
    }
    Ok(decision)
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
    complete_native_recovery(&state, &session_id)
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
