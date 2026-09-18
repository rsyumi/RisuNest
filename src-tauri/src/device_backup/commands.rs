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
    release: impl FnOnce(&Session, &std::path::Path, CasReleaseOutcome) -> Result<()>,
) -> Result<()> {
    let session = state.session(session_id)?;
    require(
        session.profile == "native-portable",
        "Device recovery requires a native portable session",
    )?;
    if session.includes_library {
        require(
            matches!(session.phase.as_str(), "committed" | "rolled-back"),
            "Native portable restore pins can only be released after recovery",
        )?;
        release(
            &session,
            state.repository_root(),
            if session.phase == "committed" {
                CasReleaseOutcome::Committed
            } else {
                CasReleaseOutcome::Aborted
            },
        )?;
    }
    state.recovery_complete(session_id)
}

fn complete_native_recovery(state: &DeviceBackupState, session_id: &str) -> Result<()> {
    complete_native_recovery_with(state, session_id, |session, root, outcome| {
        release_native_restore_pins(session, root, outcome)
    })
}

#[cfg(test)]
pub(super) fn complete_native_recovery_for_test(
    state: &DeviceBackupState,
    session_id: &str,
    release: impl FnOnce(&Session, &std::path::Path, CasReleaseOutcome) -> Result<()>,
) -> Result<()> {
    complete_native_recovery_with(state, session_id, release)
}

#[tauri::command(async)]
pub(crate) fn native_device_backup_bootstrap(
    app: AppHandle,
    state: State<'_, DeviceBackupState>,
) -> Result<BootstrapDecision> {
    if state.is_blocking()? {
        require(
            app.webview_windows().len() == 1,
            "Device maintenance requires one WebView with all previous plugin contexts closed",
        )?;
    }
    let decision = state.bootstrap_for_entry()?;
    let Some(session) = decision.session.as_ref() else {
        return Ok(decision);
    };
    require(
        session.profile == "native-portable",
        "Device recovery requires a native portable session",
    )?;
    if session.phase == "committed" {
        return Ok(decision);
    }
    if matches!(
        session.phase.as_str(),
        "loading-source" | "preparing"
    ) {
        release_native_restore_pins(
            session,
            state.repository_root(),
            CasReleaseOutcome::Aborted,
        )?;
        state.fail(&session.session_id, "interrupted-before-native-apply")?;
        state.recovery_complete(&session.session_id)?;
        return state.bootstrap_for_entry();
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
        let decision = state.bootstrap_for_entry()?;
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
pub(crate) fn native_device_backup_recovery_complete(
    state: State<'_, DeviceBackupState>,
    session_id: String,
) -> Result<()> {
    complete_native_recovery(&state, &session_id)
}
