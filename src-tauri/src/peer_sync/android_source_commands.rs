#[cfg(test)]
use super::android_foreground::test_registry_guard;
use super::{
    android_foreground::{registry, AndroidForegroundKey, AndroidForegroundLane},
    device_registry::{platform_device_name, DevicePermissions},
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
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, State};

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

    pub(crate) fn revoke_registered_device(&self, device_id: &str) {
        if let Ok(source) = self.source.lock() {
            if let Some(source) = source.as_ref() {
                let _ = source.host.revoke(device_id);
            }
        }
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

    fn start_exact(
        &self,
        session_id: &str,
        foreground: AndroidForegroundKey,
        address: Ipv4Addr,
    ) -> Result<AndroidSourceStatus, String> {
        let mut source_slot = self
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?;
        let source = source_slot
            .as_mut()
            .ok_or_else(|| "Peer clone source is not prepared".to_owned())?;
        if source.session_id != session_id || source.phase != AndroidSourcePhase::Prepared {
            return Err("Peer clone source is not prepared".to_owned());
        }
        let pairing = source
            .host
            .start_private_lan(address)
            .map_err(|error| error.to_string())?;
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

        let callback_state = self.clone();
        let callback_key = foreground.clone();
        if !registry().set_source_stop_callback_exact(&foreground, move || {
            callback_state.pause_exact(&callback_key);
        }) {
            self.pause_exact(&foreground);
            return Err("Android foreground service detached before source start".to_owned());
        }
        self.status()
    }

    fn release(&self, session_id: &str) -> Result<Option<AndroidForegroundKey>, String> {
        let mut source_slot = self
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")?;
        let Some(source) = source_slot.as_ref() else {
            return Ok(None);
        };
        if source.session_id != session_id {
            return Err("Peer clone source session does not match".to_owned());
        }
        let foreground = source.foreground.clone();
        let mut source = source_slot.take().expect("checked source");
        drop(source_slot);
        source.foreground = None;
        if let Some(key) = foreground.as_ref() {
            let _ = registry().cancel_exact(key);
            let _ = registry().detach_if_generation(key);
        }
        source.host.stop().map_err(|error| error.to_string())?;
        fs::remove_dir_all(&source.root).map_err(|error| error.to_string())?;
        Ok(foreground)
    }
}

fn private_lan_address(candidates: impl IntoIterator<Item = IpAddr>) -> Option<Ipv4Addr> {
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates
        .iter()
        .find_map(|candidate| match candidate {
            IpAddr::V4(address) if address.is_private() => Some(*address),
            _ => None,
        })
        .or_else(|| {
            candidates.iter().find_map(|candidate| match candidate {
                IpAddr::V4(address) if address.is_link_local() => Some(*address),
                _ => None,
            })
        })
}

pub(crate) fn discover_private_lan_address() -> Result<Ipv4Addr, String> {
    let interfaces = if_addrs::get_if_addrs().map_err(|error| error.to_string())?;
    private_lan_address(interfaces.into_iter().map(|interface| interface.ip()))
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
        let mut host = LanCloneHost::prepare(prepared);
        host.enable_v2_registry(&app_root, platform_device_name(), DevicePermissions::read())
            .map_err(|error| error.to_string())?;
        *state
            .source
            .lock()
            .map_err(|_| "Peer clone source is unavailable")? = Some(AndroidSource {
            session_id,
            manifest_id,
            root: source_root,
            host,
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
    let cancellation = super::android_foreground::acquire_foreground_lane(
        &foreground,
        AndroidForegroundLane::P1Source,
    )
    .await?;
    let address = discover_private_lan_address()?;
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if cancellation.is_cancelled() {
            return Err("Android foreground service was cancelled".to_owned());
        }
        state.start_exact(&session_id, foreground, address)
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
) -> Result<Option<AndroidForegroundKey>, String> {
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
    use crate::peer_sync::{
        prepare_clone_session, CloneSource, PinnedCloneRevision, PinnedSourceObject,
    };
    use reqwest::blocking::Client;
    use serde_json::{json, Value};
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Duration,
    };

    struct FixtureSource {
        database: PathBuf,
    }

    struct FixtureLease {
        database: PathBuf,
    }

    impl CloneSource for FixtureSource {
        type Lease = FixtureLease;

        fn pin(&self) -> Result<Self::Lease, PeerSyncError> {
            Ok(FixtureLease {
                database: self.database.clone(),
            })
        }
    }

    impl PinnedCloneRevision for FixtureLease {
        fn source_revision(&self) -> u64 {
            7
        }

        fn objects(&self) -> Result<Vec<PinnedSourceObject>, PeerSyncError> {
            Ok(vec![PinnedSourceObject::database(&self.database)])
        }
    }

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
        assert_eq!(
            private_lan_address([
                "127.0.0.1".parse().unwrap(),
                "0.0.0.0".parse().unwrap(),
                "203.0.113.5".parse().unwrap(),
            ]),
            None,
        );
        assert_eq!(
            private_lan_address([
                "10.2.3.4".parse().unwrap(),
                "192.168.1.4".parse().unwrap(),
                "169.254.7.8".parse().unwrap(),
            ]),
            Some(Ipv4Addr::new(10, 2, 3, 4)),
        );
        assert_eq!(
            private_lan_address([
                "169.254.7.8".parse().unwrap(),
                "203.0.113.5".parse().unwrap(),
                "192.168.9.4".parse().unwrap(),
            ]),
            Some(Ipv4Addr::new(192, 168, 9, 4)),
        );
    }

    #[test]
    fn android_source_capabilities_never_offer_tunnels() {
        let capabilities = peer_clone_android_source_capabilities();
        assert!(capabilities.production_enabled);
        assert!(capabilities.source_ready);
        assert!(!capabilities.desktop);
        assert!(!capabilities.tunnel_ready);
    }

    #[test]
    fn android_prepared_host_enables_v2_registration_and_live_revoke() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("database.risusave");
        fs::write(&database, b"android-v2").unwrap();
        let prepared =
            prepare_clone_session(&FixtureSource { database }, &root.path().join("source"))
                .unwrap();
        let mut host = LanCloneHost::prepare(prepared);
        host.enable_v2_registry(root.path(), "Android", DevicePermissions::read())
            .unwrap();
        let pairing = host
            .start_private_lan(discover_private_lan_address().unwrap())
            .unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let target_root = tempfile::tempdir().unwrap();
        let client = super::super::lan::LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Android target",
            &target_root.path().join("credential.json"),
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        assert!(
            crate::peer_sync::device_registry::OutgoingDeviceRegistry::load(root.path())
                .unwrap()
                .devices()
                .len()
                == 1
        );
        assert!(host.revoke(&client.device_id));
        assert!(
            crate::peer_sync::device_registry::OutgoingDeviceRegistry::load(root.path())
                .unwrap()
                .devices()
                .is_empty()
        );
    }

    #[test]
    fn notification_stop_pauses_real_source_and_full_release_cleans_up() {
        let _registry_guard = test_registry_guard();
        let fixture_root = tempfile::tempdir().unwrap();
        let database = fixture_root.path().join("database.risusave");
        fs::write(&database, b"synthetic-android-source").unwrap();
        let source_root = fixture_root.path().join("prepared-source");
        let prepared = prepare_clone_session(&FixtureSource { database }, &source_root).unwrap();
        let session_id = prepared.manifest().session_id.clone();
        let manifest_id = prepared.manifest_id().to_owned();
        let host = LanCloneHost::prepare(prepared);
        let retained = host.control();
        let state = AndroidPeerCloneSourceState::default();
        *state.source.lock().unwrap() = Some(AndroidSource {
            session_id: session_id.clone(),
            manifest_id,
            root: source_root.clone(),
            host,
            pairing_uri: None,
            foreground: None,
            phase: AndroidSourcePhase::Prepared,
        });
        let address = discover_private_lan_address().unwrap();
        let foreground = registry().reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(registry().attach_exact(&foreground));

        let running = state
            .start_exact(&session_id, foreground.clone(), address)
            .unwrap();
        let pairing_uri = url::Url::parse(running.pairing_uri.as_ref().unwrap()).unwrap();
        assert!(state
            .source
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .host
            .has_claim_for_test());
        let endpoint = pairing_uri
            .query_pairs()
            .find_map(|(name, value)| (name == "endpoint").then(|| value.into_owned()))
            .unwrap();
        let claim = pairing_uri
            .fragment()
            .unwrap()
            .strip_prefix("claim=")
            .unwrap();
        let claimed: Value = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{endpoint}/v1/sessions/{session_id}/claim"))
            .json(&json!({ "claim": claim }))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert!(claimed["deviceId"].is_string());
        assert_eq!(state.status().unwrap().devices.len(), 1);

        let listener_address = state
            .source
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .host
            .address()
            .unwrap();
        let mut active = TcpStream::connect(listener_address).unwrap();
        active
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        active.write_all(b"GET /incomplete HTTP/1.1\r\n").unwrap();

        assert!(registry().cancel_exact(&foreground));
        let paused = state.status().unwrap();
        assert_eq!(paused.phase, AndroidSourcePhase::Prepared);
        assert_eq!(paused.session_id.as_deref(), Some(session_id.as_str()));
        assert!(paused.pairing_uri.is_none());
        assert!(paused.devices.is_empty());
        assert!(source_root.exists());
        assert!(retained.is_attached_for_test());
        assert!(TcpStream::connect(listener_address).is_err());
        let mut byte = [0_u8; 1];
        match active.read(&mut byte) {
            Ok(0) => {}
            Err(error)
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            result => {
                panic!("active source stream remained open after notification Stop: {result:?}")
            }
        }
        assert!(registry().detach_if_generation(&foreground));

        let restarted = registry().reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(registry().attach_exact(&restarted));
        let running_again = state
            .start_exact(&session_id, restarted.clone(), address)
            .unwrap();
        assert_eq!(running_again.phase, AndroidSourcePhase::Running);
        assert!(state
            .source
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .host
            .has_claim_for_test());
        assert!(registry().cancel_exact(&restarted));
        assert!(!state
            .source
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .host
            .has_claim_for_test());
        assert!(registry().detach_if_generation(&restarted));

        let released = registry().reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(registry().attach_exact(&released));
        assert_eq!(
            state
                .start_exact(&session_id, released.clone(), address)
                .unwrap()
                .phase,
            AndroidSourcePhase::Running
        );

        assert_eq!(state.release(&session_id).unwrap(), Some(released));
        assert_eq!(state.status().unwrap().phase, AndroidSourcePhase::Idle);
        assert!(!source_root.exists());
        assert!(!retained.is_attached_for_test());
    }
}
