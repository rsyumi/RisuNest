use super::{
    android_foreground::{registry, AndroidForegroundKey, AndroidForegroundLane},
    prepare_lossless_clone_session, LanCloneHost, PeerSyncError,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::NeverCancelled,
    persistent_store::{self, StoreError},
};
use serde::Serialize;
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, State};

const SERVICE_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidPeerCloneSourceCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
    production_enabled: bool,
    tunnel_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum AndroidSourcePhase {
    Idle,
    Prepared,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidSourceDevice {
    device_id: String,
    verified_bytes: u64,
    current_object: Option<String>,
    last_seen_at: u128,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidSourceStatus {
    session_id: Option<String>,
    manifest_id: Option<String>,
    pairing_uri: Option<String>,
    phase: AndroidSourcePhase,
    devices: Vec<AndroidSourceDevice>,
}

struct AndroidSource {
    session_id: String,
    manifest_id: String,
    root: PathBuf,
    host: LanCloneHost,
    pairing_uri: Option<String>,
    foreground: Option<AndroidForegroundKey>,
    phase: AndroidSourcePhase,
}

#[derive(Clone, Default)]
pub(crate) struct AndroidPeerCloneSourceState {
    source: Arc<Mutex<Option<AndroidSource>>>,
}

impl AndroidPeerCloneSourceState {
    pub(crate) fn initialize() -> Self {
        Self::default()
    }

    fn status(&self) -> Result<AndroidSourceStatus, String> {
        let source = self
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?;
        let Some(source) = source.as_ref() else {
            return Ok(AndroidSourceStatus {
                session_id: None,
                manifest_id: None,
                pairing_uri: None,
                phase: AndroidSourcePhase::Idle,
                devices: Vec::new(),
            });
        };
        Ok(AndroidSourceStatus {
            session_id: Some(source.session_id.clone()),
            manifest_id: Some(source.manifest_id.clone()),
            pairing_uri: source.pairing_uri.clone(),
            phase: source.phase,
            devices: source
                .host
                .devices()
                .into_iter()
                .map(|device| AndroidSourceDevice {
                    device_id: device.device_id,
                    verified_bytes: device.verified_bytes,
                    current_object: device.current_object,
                    last_seen_at: device.last_seen_unix_ms,
                    revoked: device.revoked,
                })
                .collect(),
        })
    }

    fn pause_exact(&self, key: &AndroidForegroundKey) {
        let Ok(mut source) = self.source.lock() else {
            return;
        };
        let Some(source) = source.as_mut() else {
            return;
        };
        if source.foreground.as_ref() != Some(key) {
            return;
        }
        let _ = source.host.stop();
        source.pairing_uri = None;
        source.foreground = None;
        source.phase = AndroidSourcePhase::Prepared;
    }

    fn release(&self, session_id: &str) -> Result<(), String> {
        let mut source_slot = self
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?;
        let Some(source) = source_slot.as_ref() else {
            return Ok(());
        };
        if source.session_id != session_id {
            return Err("Peer clone source session does not match".to_owned());
        }
        let mut source = source_slot.take().expect("checked source");
        drop(source_slot);
        if let Some(key) = source.foreground.take() {
            let _ = registry().cancel_exact(&key);
            let _ = registry().detach_if_generation(&key);
        }
        source.host.stop().map_err(|error| error.to_string())?;
        fs::remove_dir_all(&source.root).map_err(|error| error.to_string())?;
        Ok(())
    }
}

fn private_lan_address(candidates: impl IntoIterator<Item = IpAddr>) -> Option<Ipv4Addr> {
    candidates
        .into_iter()
        .find_map(|candidate| match candidate {
            IpAddr::V4(address) if address.is_private() || address.is_link_local() => Some(address),
            _ => None,
        })
}

fn discover_private_lan_address() -> Result<Ipv4Addr, String> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(|error| error.to_string())?;
    socket
        .connect((Ipv4Addr::new(192, 0, 2, 1), 9))
        .map_err(|error| error.to_string())?;
    private_lan_address([socket.local_addr().map_err(|error| error.to_string())?.ip()])
        .ok_or_else(|| "No private IPv4 LAN address is available".to_owned())
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, String> {
    let mut uri =
        url::Url::parse("risuailocal://peer-clone/v1").map_err(|error| error.to_string())?;
    uri.query_pairs_mut()
        .append_pair("endpoint", endpoint)
        .append_pair("session", &pairing.session_id)
        .append_pair("manifest", &pairing.manifest_id);
    uri.set_fragment(Some(&format!("claim={}", pairing.claim)));
    Ok(uri.to_string())
}

fn app_roots(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let app_root = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    Ok((
        app_root.clone(),
        app_root.join("peer-clone").join("android-source"),
    ))
}

fn as_store_error(error: PeerSyncError) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}

#[tauri::command]
pub(crate) fn peer_clone_android_source_capabilities() -> AndroidPeerCloneSourceCapabilities {
    AndroidPeerCloneSourceCapabilities {
        desktop: false,
        source_ready: true,
        atomic_activation_ready: false,
        lossless_backup_ready: true,
        http_transport_ready: true,
        large_fixture_passed: false,
        production_enabled: true,
        tunnel_ready: false,
    }
}

#[tauri::command]
pub(crate) fn peer_clone_android_source_reserve() -> Result<AndroidForegroundKey, String> {
    registry().reserve(AndroidForegroundLane::P1Source)
}

#[tauri::command(async)]
pub(crate) async fn peer_clone_android_source_prepare(
    app: AppHandle,
    state: State<'_, AndroidPeerCloneSourceState>,
) -> Result<AndroidSourceStatus, String> {
    let state = state.inner().clone();
    let (app_root, source_parent) = app_roots(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        if state
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?
            .is_some()
        {
            return Err("Peer clone source is already prepared".to_owned());
        }
        let operation_id = uuid::Uuid::new_v4().to_string();
        let preparation_root = source_parent.join("preparing").join(&operation_id);
        let source_root = source_parent.join("sessions").join(&operation_id);
        fs::create_dir_all(source_root.parent().expect("source parent"))
            .map_err(|error| error.to_string())?;
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        let prepared = persistent_store::commands::with_store_mut(app.state(), |store| {
            let revision = store.revision()?;
            prepare_lossless_clone_session(
                store,
                &cas,
                revision,
                &preparation_root,
                &source_root,
                &NeverCancelled,
            )
            .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())?;
        let session_id = prepared.manifest().session_id.clone();
        let manifest_id = prepared.manifest_id().to_owned();
        *state
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")? = Some(AndroidSource {
            session_id,
            manifest_id,
            root: source_root,
            host: LanCloneHost::prepare(prepared),
            pairing_uri: None,
            foreground: None,
            phase: AndroidSourcePhase::Prepared,
        });
        state.status()
    })
    .await
    .map_err(|error| format!("Peer clone source preparation failed: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn peer_clone_android_source_start(
    state: State<'_, AndroidPeerCloneSourceState>,
    session_id: String,
    foreground: AndroidForegroundKey,
) -> Result<AndroidSourceStatus, String> {
    if foreground.lane != AndroidForegroundLane::P1Source {
        return Err("Android foreground lane is not allowed".to_owned());
    }
    let deadline = Instant::now() + SERVICE_ATTACH_TIMEOUT;
    let cancellation = loop {
        if let Some(cancellation) = registry().acquire_exact(&foreground) {
            break cancellation;
        }
        if Instant::now() >= deadline {
            return Err("Android foreground service did not attach".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let address = discover_private_lan_address()?;
    let state = state.inner().clone();
    let callback_state = state.clone();
    let callback_key = foreground.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut source_slot = state
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?;
        let source = source_slot
            .as_mut()
            .ok_or_else(|| "Peer clone source is not prepared".to_owned())?;
        if source.session_id != session_id || source.phase != AndroidSourcePhase::Prepared {
            return Err("Peer clone source is not prepared".to_owned());
        }
        if cancellation.is_cancelled() {
            return Err("Android foreground service was cancelled".to_owned());
        }
        let pairing = source.host.start().map_err(|error| error.to_string())?;
        let port = source
            .host
            .address()
            .ok_or_else(|| "Peer clone listener is unavailable".to_owned())?
            .port();
        source.pairing_uri = Some(build_pairing_uri(
            &format!("http://{address}:{port}"),
            &pairing,
        )?);
        source.foreground = Some(foreground.clone());
        source.phase = AndroidSourcePhase::Running;
        drop(source_slot);
        if !registry().set_source_stop_callback_exact(&foreground, move || {
            callback_state.pause_exact(&callback_key);
        }) {
            state.pause_exact(&foreground);
            return Err("Android foreground service detached before source start".to_owned());
        }
        state.status()
    })
    .await
    .map_err(|error| format!("Peer clone source start failed: {error}"))?
}

#[tauri::command]
pub(crate) fn peer_clone_android_source_status(
    state: State<'_, AndroidPeerCloneSourceState>,
) -> Result<AndroidSourceStatus, String> {
    state.status()
}

#[tauri::command(async)]
pub(crate) async fn peer_clone_android_source_stop(
    state: State<'_, AndroidPeerCloneSourceState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.release(&session_id))
        .await
        .map_err(|error| format!("Peer clone source stop failed: {error}"))?
}

#[tauri::command]
pub(crate) fn peer_clone_android_source_revoke(
    state: State<'_, AndroidPeerCloneSourceState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    let source = state
        .source
        .lock()
        .map_err(|_| "Peer clone source is unavailable")?;
    let source = source
        .as_ref()
        .ok_or_else(|| "Peer clone source is not running".to_owned())?;
    if source.session_id != session_id || !source.host.revoke(&device_id) {
        return Err("Peer clone source device is unavailable".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_selection_accepts_only_private_or_link_local_ipv4() {
        assert_eq!(
            private_lan_address([
                "8.8.8.8".parse().unwrap(),
                "2001:4860:4860::8888".parse().unwrap(),
                "192.168.1.4".parse().unwrap(),
            ]),
            Some(Ipv4Addr::new(192, 168, 1, 4)),
        );
        assert_eq!(private_lan_address(["8.8.8.8".parse().unwrap()]), None);
    }

    #[test]
    fn android_source_capabilities_never_offer_tunnels() {
        let capabilities = peer_clone_android_source_capabilities();
        assert!(capabilities.production_enabled);
        assert!(capabilities.source_ready);
        assert!(!capabilities.desktop);
        assert!(!capabilities.tunnel_ready);
    }
}
