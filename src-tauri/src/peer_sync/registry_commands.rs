#[cfg(target_os = "android")]
use super::android_source_commands::AndroidPeerCloneSourceState;
#[cfg(desktop)]
use super::commands::PeerCloneCommandState;
use super::{
    bidirectional_commands::PeerBidirectionalCommandState,
    delta_commands::PeerDeltaCommandState,
    device_registry::{
        incoming_source_summaries, outgoing_device_summaries, remove_incoming_source,
        revoke_outgoing_device, IncomingSourceSummary, OutgoingDeviceSummary,
    },
};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_sync_outgoing_devices(app: AppHandle) -> Result<Vec<OutgoingDeviceSummary>, String> {
    outgoing_device_summaries(&app_root(&app)?).map_err(|error| error.to_string())
}
#[tauri::command]
pub fn peer_sync_incoming_sources(app: AppHandle) -> Result<Vec<IncomingSourceSummary>, String> {
    incoming_source_summaries(&app_root(&app)?).map_err(|error| error.to_string())
}
#[tauri::command]
pub fn peer_sync_remove_incoming_source(app: AppHandle, device_id: String) -> Result<(), String> {
    remove_incoming_source(&app_root(&app)?, &device_id).map_err(|error| error.to_string())
}

#[cfg(desktop)]
#[tauri::command]
pub fn peer_sync_revoke_outgoing_device(
    app: AppHandle,
    clone: State<'_, PeerCloneCommandState>,
    delta: State<'_, PeerDeltaCommandState>,
    bidirectional: State<'_, PeerBidirectionalCommandState>,
    device_id: String,
) -> Result<(), String> {
    let clone = clone.inner().clone();
    let delta = delta.inner().clone();
    let bidirectional = bidirectional.inner().clone();
    revoke_outgoing_device(&app_root(&app)?, &device_id, move |id| {
        clone.revoke_registered_device(id);
        delta.revoke_registered_device(id);
        bidirectional.revoke_registered_device(id);
    })
    .map_err(|e| e.to_string())
}
#[cfg(target_os = "android")]
#[tauri::command]
pub fn peer_sync_revoke_outgoing_device(
    app: AppHandle,
    clone: State<'_, AndroidPeerCloneSourceState>,
    delta: State<'_, PeerDeltaCommandState>,
    bidirectional: State<'_, PeerBidirectionalCommandState>,
    device_id: String,
) -> Result<(), String> {
    let clone = clone.inner().clone();
    let delta = delta.inner().clone();
    let bidirectional = bidirectional.inner().clone();
    revoke_outgoing_device(&app_root(&app)?, &device_id, move |id| {
        clone.revoke_registered_device(id);
        delta.revoke_registered_device(id);
        bidirectional.revoke_registered_device(id);
    })
    .map_err(|e| e.to_string())
}
