#[cfg(desktop)]
use super::lan::authenticated_peer_clone_session;
use super::lan::{
    validate_lan_endpoint, PEER_COMPLETION_CAPABILITY_HEADER, PEER_COMPLETION_CAPABILITY_V1,
    PEER_COMPLETION_LEASE_HEADER, PEER_COMPLETION_RESUME_HEADER, PEER_COMPLETION_SCHEMA,
};
use super::{
    android_foreground::{
        registry as android_foreground_registry, test_registry_guard, AndroidForegroundLane,
    },
    device_registry::{DevicePermissions, OutgoingDeviceRegistry},
    shared_session::{
        reserve_device_sync_source, AndroidDeviceSyncHost, AndroidDeviceSyncSourceState,
        DeviceSyncLinkPermissions, DeviceSyncListenMethod, DeviceSyncPrepareRequest,
        DeviceSyncSourcePhase, SharedPairingData, SharedRemoteCommit, SharedSessionLifecycle,
        SharedSessionPhase, SharedSourceLane, SharedSourceOwnership, SharedSourcePreparation,
    },
    PeerSyncError,
};
#[cfg(desktop)]
use super::{
    bidirectional_commands::{
        PeerBidirectionalDurableOperation, PeerBidirectionalOperationJournal,
        OPERATION_SCHEMA as PEER_BIDIRECTIONAL_OPERATION_SCHEMA,
    },
    lan::{LanBidirectionalGeneration, SharedSessionKind, MAX_RETIRED_SESSIONS_PER_LANE},
};
#[cfg(desktop)]
use super::{
    lan::{
        LanBidirectionalControl, LanBidirectionalRegistrationRequest,
        LanBidirectionalRemoteApplyReceipt, LanBidirectionalRemoteApplyRequest,
        LanBidirectionalSession, PreparedBidirectionalLogicalLanSession, PreparedLogicalLanSession,
        SharedSessionResealer, SharedSessionSeal,
    },
    logical_delta::{
        build_logical_manifest, LogicalManifestBuilderInput, LogicalManifestObject,
        LogicalRecordEnvelope, LogicalRecordLocator, ProjectedLogicalRecord,
    },
    prepare_clone_session,
    protocol::CloneManifest,
    shared_session::{
        run_device_sync_rotate_link, DeviceSyncSourceState, SharedPeerTunnel,
        SharedPeerTunnelCleanup, SharedPeerTunnelLauncher, SharedPeerTunnelLifecycle,
        SharedPublicOriginVerifier, SharedRemoteCommitSlot, SharedSessionHost, SharedSourceEngines,
        SharedSourcePreparationContext,
    },
    CloneSource, LogicalDeltaObject, LogicalDeltaObjectSource, PinnedCloneRevision,
    PinnedSourceObject, PreparedCloneSession,
};
#[cfg(desktop)]
use crate::persistent_store::SyncGenerationIdentity;
#[cfg(desktop)]
use crate::{
    asset_repository::PayloadCas,
    local_backup::{AtomicCancellation, CancellationProbe, NeverCancelled},
    persistent_store::PersistentStore,
};
#[cfg(desktop)]
use sha2::{Digest, Sha256};
#[cfg(desktop)]
use std::sync::atomic::AtomicBool;
#[cfg(desktop)]
use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Read},
    net::TcpListener,
    path::PathBuf,
};
use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct AndroidHostEvents {
    starts: usize,
    rotations: usize,
    stops: usize,
    cleanups: Vec<SharedSourceLane>,
    fail_start_once: bool,
    fail_port_once: bool,
    fail_stop_once: bool,
    fail_prepare: Option<SharedSourceLane>,
}

struct AndroidHostFixture {
    events: Arc<Mutex<AndroidHostEvents>>,
}

#[test]
fn android_source_reserve_logs_the_underlying_failure_and_preserves_its_string_contract() {
    let _guard = test_registry_guard();
    let occupied = android_foreground_registry()
        .reserve(AndroidForegroundLane::P4Target)
        .unwrap();

    let error = reserve_device_sync_source().unwrap_err();

    assert_eq!(error, "Android foreground service is already reserved");
    let entry = crate::native_log::global_state()
        .tail(None)
        .into_iter()
        .rev()
        .find(|entry| {
            entry.target.ends_with("peer_sync::shared_session")
                && entry.message.contains("device sync source reserve failed")
        })
        .expect("Android source reservation failure log");
    assert!(entry
        .message
        .contains("Android foreground service is already reserved"));
    assert!(android_foreground_registry().detach_if_generation(&occupied));
}

impl AndroidDeviceSyncHost for AndroidHostFixture {
    fn enable_registry(
        &mut self,
        _: &std::path::Path,
        _: &str,
        _: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        Ok(())
    }

    fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        let mut events = self.events.lock().unwrap();
        events.starts += 1;
        if std::mem::take(&mut events.fail_port_once) {
            return Err(PeerSyncError::Transport(
                "shared fixed port is unavailable".to_owned(),
            ));
        }
        if std::mem::take(&mut events.fail_start_once) {
            return Err(PeerSyncError::Transport(
                "injected listener failure".to_owned(),
            ));
        }
        Ok(SharedPairingData {
            endpoint: format!("http://{address}:{port}"),
            session_id: "00000000-0000-4000-8000-000000000061".to_owned(),
            manifest_id: "61".repeat(32),
            claim: "claim".to_owned(),
            expires_at_ms: 9_999,
        })
    }

    fn stop_listener(&mut self) -> Result<(), PeerSyncError> {
        let mut events = self.events.lock().unwrap();
        events.stops += 1;
        if std::mem::take(&mut events.fail_stop_once) {
            return Err(PeerSyncError::Transport(
                "injected listener cleanup failure".to_owned(),
            ));
        }
        Ok(())
    }

    fn rotate_link(&mut self, _: DevicePermissions) -> Result<SharedPairingData, PeerSyncError> {
        let mut events = self.events.lock().unwrap();
        events.rotations += 1;
        Ok(SharedPairingData {
            endpoint: "http://192.168.4.8:32145".to_owned(),
            session_id: "00000000-0000-4000-8000-000000000062".to_owned(),
            manifest_id: "62".repeat(32),
            claim: "rotated-claim".to_owned(),
            expires_at_ms: 10_999,
        })
    }
}

struct AndroidPreparationFixture {
    events: Arc<Mutex<AndroidHostEvents>>,
}

impl SharedSourceOwnership for AndroidPreparationFixture {
    type Host = AndroidHostFixture;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        Ok(AndroidHostFixture {
            events: Arc::clone(&self.events),
        })
    }

    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError> {
        self.events.lock().unwrap().cleanups.push(lane);
        Ok(())
    }

    fn stop_host(&mut self, host: &mut Self::Host) -> Result<(), PeerSyncError> {
        host.stop_listener()
    }
}

impl SharedSourcePreparation<()> for AndroidPreparationFixture {
    fn prepare(&mut self, lane: SharedSourceLane, _: &mut ()) -> Result<(), PeerSyncError> {
        let mut events = self.events.lock().unwrap();
        if events.fail_prepare == Some(lane) {
            events.fail_prepare = None;
            return Err(PeerSyncError::Storage(format!(
                "injected {lane:?} preparation failure"
            )));
        }
        Ok(())
    }
}

fn android_state(
    events: Arc<Mutex<AndroidHostEvents>>,
) -> AndroidDeviceSyncSourceState<AndroidPreparationFixture> {
    AndroidDeviceSyncSourceState::new_for_test(
        AndroidPreparationFixture { events },
        Ipv4Addr::new(192, 168, 4, 8),
    )
}

fn android_lan_request(port: u16) -> DeviceSyncPrepareRequest {
    DeviceSyncPrepareRequest {
        method: DeviceSyncListenMethod::Lan,
        fixed_port: port,
        public_base_url: None,
    }
}

#[test]
fn android_unified_source_accepts_only_explicit_nonzero_lan_ports() {
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(events);
    let root = tempfile::tempdir().unwrap();

    assert!(state
        .prepare_for_test(&mut (), root.path(), android_lan_request(0))
        .is_err());
    assert!(state
        .prepare_for_test(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: 32145,
                public_base_url: None,
            },
        )
        .is_err());
    assert!(state
        .prepare_for_test(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::FixedUrl,
                fixed_port: 32145,
                public_base_url: Some("https://sync.example.com".to_owned()),
            },
        )
        .is_err());
    assert_eq!(
        state
            .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
            .unwrap()
            .phase,
        DeviceSyncSourcePhase::Prepared
    );
}

#[test]
fn android_lane_prepare_failures_leave_a_terminal_retryable_phase() {
    for lane in [
        SharedSourceLane::Clone,
        SharedSourceLane::Delta,
        SharedSourceLane::Bidirectional,
    ] {
        let events = Arc::new(Mutex::new(AndroidHostEvents {
            fail_prepare: Some(lane),
            ..Default::default()
        }));
        let state = android_state(events);
        let root = tempfile::tempdir().unwrap();

        assert!(state
            .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
            .is_err());
        let failed = state.status().unwrap();
        assert_eq!(failed.phase, DeviceSyncSourcePhase::Error);
        assert_eq!(
            failed.latest_error,
            Some(super::shared_session::DeviceSyncErrorCategory::PreparationFailed)
        );
        assert_eq!(
            state
                .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
                .unwrap()
                .phase,
            DeviceSyncSourcePhase::Prepared
        );
        assert_eq!(state.stop_for_test().unwrap(), None);
    }
}

#[test]
fn android_notification_stop_keeps_all_prepared_lanes_restartable() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let first = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&first));

    let running = state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: true,
            },
            first.clone(),
        )
        .unwrap();
    assert_eq!(running.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(
        running.endpoint.as_deref(),
        Some("http://192.168.4.8:32145")
    );
    assert!(android_foreground_registry().cancel_exact(&first));
    assert!(android_foreground_registry().detach_if_generation(&first));
    assert_eq!(
        state.status().unwrap().phase,
        DeviceSyncSourcePhase::Prepared
    );
    assert!(events.lock().unwrap().cleanups.is_empty());

    let restarted = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&restarted));
    assert_eq!(
        state
            .start_attached_for_test(
                DeviceSyncLinkPermissions {
                    read: true,
                    bidirectional: false,
                },
                restarted.clone(),
            )
            .unwrap()
            .phase,
        DeviceSyncSourcePhase::Running
    );
    assert_eq!(events.lock().unwrap().starts, 2);

    let released = state.stop_for_test().unwrap();
    assert_eq!(released, Some(restarted));
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(
        events.lock().unwrap().cleanups,
        [
            SharedSourceLane::Bidirectional,
            SharedSourceLane::Delta,
            SharedSourceLane::Clone,
        ]
    );
}

#[test]
fn android_start_failure_releases_only_its_exact_foreground_generation() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents {
        fail_start_once: true,
        ..Default::default()
    }));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let failed = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&failed));
    assert!(state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            failed.clone(),
        )
        .is_err());
    assert_eq!(
        state.status().unwrap().phase,
        DeviceSyncSourcePhase::Prepared
    );

    let current = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&current));
    assert!(!android_foreground_registry().cancel_exact(&failed));
    assert_eq!(
        state
            .start_attached_for_test(
                DeviceSyncLinkPermissions {
                    read: true,
                    bidirectional: false,
                },
                current.clone(),
            )
            .unwrap()
            .phase,
        DeviceSyncSourcePhase::Running
    );
    assert_eq!(state.stop_for_test().unwrap(), Some(current));
}

#[test]
fn android_fixed_port_collision_reports_port_unavailable() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents {
        fail_port_once: true,
        ..Default::default()
    }));
    let state = android_state(events);
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let foreground = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&foreground));

    assert!(state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            foreground,
        )
        .is_err());
    assert_eq!(
        state.status().unwrap().latest_error,
        Some(super::shared_session::DeviceSyncErrorCategory::PortUnavailable)
    );
}

#[test]
fn android_running_source_rotates_pairing_without_restarting_or_replacing_foreground() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let foreground = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&foreground));
    let initial = state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            foreground.clone(),
        )
        .unwrap();

    let rotated = state
        .rotate_link_for_test(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: true,
        })
        .unwrap();

    assert_eq!(rotated.phase, DeviceSyncSourcePhase::Running);
    assert_ne!(rotated.pairing_uri, initial.pairing_uri);
    assert_eq!(events.lock().unwrap().starts, 1);
    assert_eq!(events.lock().unwrap().rotations, 1);
    assert_eq!(state.stop_for_test().unwrap(), Some(foreground));
}

#[test]
fn android_duplicate_start_is_idempotent_and_keeps_notification_stop_restartable() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let foreground = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&foreground));
    let permissions = DeviceSyncLinkPermissions {
        read: true,
        bidirectional: false,
    };
    let first = state
        .start_attached_for_test(permissions, foreground.clone())
        .unwrap();
    let duplicate = state
        .start_attached_for_test(permissions, foreground.clone())
        .unwrap();

    assert_eq!(duplicate, first);
    assert_eq!(events.lock().unwrap().starts, 1);
    assert!(android_foreground_registry().cancel_exact(&foreground));
    assert_eq!(
        state.status().unwrap().phase,
        DeviceSyncSourcePhase::Prepared
    );
    assert!(android_foreground_registry().detach_if_generation(&foreground));
}

#[test]
fn android_start_cleanup_failure_requires_explicit_stop_before_reprepare() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents {
        fail_start_once: true,
        fail_stop_once: true,
        ..Default::default()
    }));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let failed = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&failed));

    assert!(state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            failed,
        )
        .is_err());
    let failed_status = state.status().unwrap();
    assert_eq!(failed_status.phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        failed_status.latest_error,
        Some(super::shared_session::DeviceSyncErrorCategory::CleanupFailed)
    );

    assert_eq!(state.stop_for_test().unwrap(), None);
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(events.lock().unwrap().stops, 2);
}

#[test]
fn android_explicit_stop_returns_exact_foreground_when_native_cleanup_needs_retry() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(Arc::clone(&events));
    let root = tempfile::tempdir().unwrap();
    state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    let foreground = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&foreground));
    state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            foreground.clone(),
        )
        .unwrap();
    events.lock().unwrap().fail_stop_once = true;

    assert!(state.stop_for_test().is_err());
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        android_foreground_registry().source_status(AndroidForegroundLane::DeviceSyncSource),
        Some(foreground.clone())
    );
    assert!(state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .is_err());
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        android_foreground_registry().source_status(AndroidForegroundLane::DeviceSyncSource),
        Some(foreground.clone())
    );
    assert!(android_foreground_registry().cancel_exact(&foreground));
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Error);

    assert_eq!(state.stop_for_test().unwrap(), Some(foreground.clone()));
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert!(android_foreground_registry()
        .source_status(AndroidForegroundLane::DeviceSyncSource)
        .is_none());
    let next = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(next.generation > foreground.generation);
    assert!(android_foreground_registry().detach_if_generation(&next));
}

/// The production shared host, reachable over IPv4 loopback.
///
/// `SharedSessionHost::start_private_lan` insists on a private or link-local
/// interface, and no test machine can be relied on to own one, so the bind
/// entry point is the single substitution this fixture makes.  Every other
/// call, socket, thread, and cleanup below is the production host's.
#[cfg(desktop)]
struct LoopbackAndroidHost(SharedSessionHost);

#[cfg(desktop)]
impl AndroidDeviceSyncHost for LoopbackAndroidHost {
    fn enable_registry(
        &mut self,
        app_root: &std::path::Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        AndroidDeviceSyncHost::enable_registry(&mut self.0, app_root, source_name, permissions)
    }

    fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<SharedPairingData, PeerSyncError> {
        assert!(
            address.is_loopback(),
            "the loopback source fixture never binds a routable interface"
        );
        self.0.start_fixed_loopback(port)
    }

    fn stop_listener(&mut self) -> Result<(), PeerSyncError> {
        AndroidDeviceSyncHost::stop_listener(&mut self.0)
    }

    fn rotate_link(
        &mut self,
        permissions: DevicePermissions,
    ) -> Result<SharedPairingData, PeerSyncError> {
        AndroidDeviceSyncHost::rotate_link(&mut self.0, permissions)
    }

    fn revoke_registered_device(&mut self, device_id: &str) {
        AndroidDeviceSyncHost::revoke_registered_device(&mut self.0, device_id);
    }
}

/// The real preparation engines, rewrapped so the Android state receives the
/// loopback host.  Lane preparation and cleanup stay the production ones, so
/// the prepared clone session on disk is created and removed for real.
#[cfg(desktop)]
struct LoopbackAndroidSourcePreparation {
    engines: SharedSourceEngines,
}

#[cfg(desktop)]
impl SharedSourceOwnership for LoopbackAndroidSourcePreparation {
    type Host = LoopbackAndroidHost;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        self.engines.build_host().map(LoopbackAndroidHost)
    }

    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError> {
        self.engines.cleanup(lane)
    }

    fn stop_host(&mut self, host: &mut Self::Host) -> Result<(), PeerSyncError> {
        self.engines.stop_host(&mut host.0)
    }
}

#[cfg(desktop)]
impl<'a> SharedSourcePreparation<SharedSourcePreparationContext<'a>>
    for LoopbackAndroidSourcePreparation
{
    fn prepare(
        &mut self,
        lane: SharedSourceLane,
        context: &mut SharedSourcePreparationContext<'a>,
    ) -> Result<(), PeerSyncError> {
        self.engines.prepare(lane, context)
    }
}

/// Resolves the prepared clone session directory from the source marker the
/// clone lane writes, so the release assertion names the real owned root.
#[cfg(desktop)]
fn prepared_source_root(app_root: &std::path::Path) -> PathBuf {
    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(app_root.join("peer-clone").join("active-source.json")).unwrap(),
    )
    .unwrap();
    app_root
        .join("peer-clone")
        .join("source-sessions")
        .join(marker["directoryId"].as_str().unwrap())
}

#[cfg(desktop)]
#[test]
fn android_notification_stop_refuses_real_connections_and_release_removes_the_source_root() {
    let _guard = test_registry_guard();
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    seed_shared_source_store(&mut store);
    let expected_revision = store.revision().unwrap();
    let state = AndroidDeviceSyncSourceState::new_for_test(
        LoopbackAndroidSourcePreparation {
            engines: SharedSourceEngines::new(),
        },
        Ipv4Addr::LOCALHOST,
    );
    // A port the kernel just handed back is free without hard-coding one.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    {
        let mut context = SharedSourcePreparationContext {
            store: &mut store,
            cas: &cas,
            app_root: root.path(),
            cancellation: &NeverCancelled,
            expected_bidirectional_revision: expected_revision,
            remote_commit: Arc::new(SharedRemoteCommitSlot::default()),
        };
        state
            .prepare_for_test(&mut context, root.path(), android_lan_request(port))
            .unwrap();
    }
    let source_root = prepared_source_root(root.path());
    let marker = root.path().join("peer-clone").join("active-source.json");
    assert!(source_root.is_dir());
    let foreground = android_foreground_registry()
        .reserve(AndroidForegroundLane::DeviceSyncSource)
        .unwrap();
    assert!(android_foreground_registry().attach_exact(&foreground));
    let address = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let connect_timeout = std::time::Duration::from_secs(5);

    let running = state
        .start_attached_for_test(
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            },
            foreground.clone(),
        )
        .unwrap();

    assert_eq!(running.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(
        running.endpoint.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    // Positive control for the refusal below: the running source answers a
    // real TCP connect on the advertised endpoint.
    drop(
        std::net::TcpStream::connect_timeout(&address, connect_timeout)
            .expect("running source listener accepts a loopback connection"),
    );

    assert!(android_foreground_registry().cancel_exact(&foreground));

    let stopped = state.status().unwrap();
    assert_eq!(stopped.phase, DeviceSyncSourcePhase::Prepared);
    assert_eq!(stopped.endpoint, None);
    assert_eq!(stopped.pairing_uri, None);
    assert_eq!(stopped.latest_error, None);
    let refusal = std::net::TcpStream::connect_timeout(&address, connect_timeout)
        .expect_err("stopped source listener still accepted a loopback connection");
    #[cfg(windows)]
    assert_eq!(refusal.kind(), std::io::ErrorKind::ConnectionRefused);
    // Loopback RST is not guaranteed everywhere, so a bounded connect that
    // never completes counts as refused off Windows.
    #[cfg(not(windows))]
    assert!(
        matches!(
            refusal.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
        ),
        "unexpected connect failure after the notification Stop: {refusal:?}"
    );
    // The notification Stop only drops the listener; the prepared lanes and
    // their owned source root stay for a restart.
    assert!(source_root.is_dir());
    assert!(marker.is_file());
    assert!(android_foreground_registry().detach_if_generation(&foreground));

    assert_eq!(state.stop_for_test().unwrap(), None);

    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(state.status().unwrap().latest_error, None);
    assert!(!source_root.exists());
    assert!(!marker.exists());
}

#[derive(Default)]
struct LifecycleFixture {
    events: Vec<&'static str>,
    fail_prepare: Option<SharedSourceLane>,
    fail_build: bool,
    fail_stop_host_once: bool,
    cleanup_failures: Vec<SharedSourceLane>,
}

impl SharedSourceOwnership for LifecycleFixture {
    type Host = u32;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        self.events.push("build-host");
        if self.fail_build {
            return Err(PeerSyncError::Storage(
                "host construction failed after move".to_owned(),
            ));
        }
        Ok(0)
    }

    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError> {
        self.events.push(match lane {
            SharedSourceLane::Clone => "cleanup-clone",
            SharedSourceLane::Delta => "cleanup-delta",
            SharedSourceLane::Bidirectional => "cleanup-bidirectional",
        });
        if let Some(index) = self
            .cleanup_failures
            .iter()
            .position(|failed| *failed == lane)
        {
            self.cleanup_failures.remove(index);
            return Err(PeerSyncError::Storage(format!("{lane:?} cleanup failed")));
        }
        Ok(())
    }

    fn stop_host(&mut self, _: &mut Self::Host) -> Result<(), PeerSyncError> {
        self.events.push("stop-host");
        if std::mem::take(&mut self.fail_stop_host_once) {
            return Err(PeerSyncError::Transport("host stop failed".to_owned()));
        }
        Ok(())
    }
}

impl SharedSourcePreparation<()> for LifecycleFixture {
    fn prepare(&mut self, lane: SharedSourceLane, _: &mut ()) -> Result<(), PeerSyncError> {
        self.events.push(match lane {
            SharedSourceLane::Clone => "prepare-clone",
            SharedSourceLane::Delta => "prepare-delta",
            SharedSourceLane::Bidirectional => "prepare-bidirectional",
        });
        if self.fail_prepare == Some(lane) {
            return Err(PeerSyncError::Storage(format!(
                "{lane:?} preparation failed"
            )));
        }
        Ok(())
    }
}

#[test]
fn unified_preparation_builds_the_host_only_after_all_sources_prepare() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture::default());
    let mut context = ();

    lifecycle.prepare(&mut context).unwrap();

    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Prepared);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
        ]
    );
}

#[test]
fn prepared_lifecycle_exposes_its_host_without_transferring_ownership() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture::default());
    lifecycle.prepare(&mut ()).unwrap();

    lifecycle.with_host_mut(|host| *host = 7).unwrap();

    assert_eq!(lifecycle.with_host_mut(|host| *host).unwrap(), 7);
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Prepared);
}

#[test]
fn unified_preparation_rolls_back_every_acquired_source_in_reverse_order() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        fail_prepare: Some(SharedSourceLane::Bidirectional),
        cleanup_failures: vec![SharedSourceLane::Delta],
        ..Default::default()
    });

    let error = lifecycle.prepare(&mut ()).unwrap_err();

    assert!(error
        .to_string()
        .contains("Bidirectional preparation failed"));
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Stopping);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
}

#[test]
fn build_host_failure_cleans_every_prepared_lane_after_a_partial_move() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        fail_build: true,
        ..Default::default()
    });

    let error = lifecycle.prepare(&mut ()).unwrap_err();

    assert!(error
        .to_string()
        .contains("host construction failed after move"));
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Idle);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
}

#[test]
fn unified_stop_attempts_every_cleanup_after_a_failure() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        cleanup_failures: vec![SharedSourceLane::Bidirectional],
        ..Default::default()
    });
    lifecycle.prepare(&mut ()).unwrap();

    let error = lifecycle.stop().unwrap_err();

    assert!(error.to_string().contains("Bidirectional cleanup failed"));
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Stopping);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
}

#[test]
fn cleanup_failure_retains_the_lane_for_a_retry() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        cleanup_failures: vec![SharedSourceLane::Bidirectional],
        ..Default::default()
    });
    lifecycle.prepare(&mut ()).unwrap();

    assert!(lifecycle.stop().is_err());
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Stopping);
    lifecycle.stop().unwrap();

    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Idle);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
            "cleanup-bidirectional",
        ]
    );
}

#[test]
fn host_stop_failure_retains_the_host_for_a_retry_after_all_lane_cleanup() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        fail_stop_host_once: true,
        ..LifecycleFixture::default()
    });
    lifecycle.prepare(&mut ()).unwrap();

    assert!(lifecycle.stop().is_err());
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Stopping);
    lifecycle
        .with_preparation(|fixture| {
            assert_eq!(
                fixture.events,
                [
                    "prepare-clone",
                    "prepare-delta",
                    "prepare-bidirectional",
                    "build-host",
                    "stop-host",
                    "cleanup-bidirectional",
                    "cleanup-delta",
                    "cleanup-clone",
                ]
            );
        })
        .unwrap();

    lifecycle.stop().unwrap();
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Idle);
    lifecycle
        .with_preparation(|fixture| assert_eq!(fixture.events.last(), Some(&"stop-host")))
        .unwrap();
}

#[test]
fn prepare_does_not_replace_a_retryable_cleanup_owner() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture {
        cleanup_failures: vec![SharedSourceLane::Clone],
        ..Default::default()
    });
    lifecycle.prepare(&mut ()).unwrap();
    assert!(lifecycle.stop().is_err());

    let error = lifecycle.prepare(&mut ()).unwrap_err();

    assert!(error.to_string().contains("cleanup is pending"));
    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Stopping);
    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
    lifecycle.stop().unwrap();
}

#[test]
fn repeated_prepare_and_stop_are_idempotent() {
    let lifecycle = SharedSessionLifecycle::new(LifecycleFixture::default());

    lifecycle.prepare(&mut ()).unwrap();
    lifecycle.prepare(&mut ()).unwrap();
    lifecycle.stop().unwrap();
    lifecycle.stop().unwrap();

    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
}

#[test]
fn concurrent_prepare_calls_share_one_serialized_preparation() {
    let lifecycle = Arc::new(SharedSessionLifecycle::new(LifecycleFixture::default()));
    let first = {
        let lifecycle = Arc::clone(&lifecycle);
        std::thread::spawn(move || lifecycle.prepare(&mut ()))
    };
    let second = {
        let lifecycle = Arc::clone(&lifecycle);
        std::thread::spawn(move || lifecycle.prepare(&mut ()))
    };

    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();

    assert_eq!(
        lifecycle
            .with_preparation(|fixture| fixture.events.clone())
            .unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
        ]
    );
}

#[cfg(desktop)]
#[test]
fn real_shared_source_engines_accept_a_short_lived_store_and_remove_clone_marker() {
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    seed_shared_source_store(&mut store);
    let expected_revision = store.revision().unwrap();
    let lifecycle = SharedSessionLifecycle::new(SharedSourceEngines::new());

    {
        let mut context = SharedSourcePreparationContext {
            store: &mut store,
            cas: &cas,
            app_root: root.path(),
            cancellation: &NeverCancelled,
            expected_bidirectional_revision: expected_revision,
            remote_commit: Arc::new(SharedRemoteCommitSlot::default()),
        };
        lifecycle.prepare(&mut context).unwrap();
    }
    assert!(root
        .path()
        .join("peer-clone")
        .join("active-source.json")
        .exists());
    // Positive control for the reclaim assertion below: the delta lane holds a
    // P4 generation pin for as long as its prepared source lives.
    assert_eq!(
        store
            .count_logical_generation_pins(super::delta_commands::P4_SOURCE_PIN_PREFIX)
            .unwrap(),
        1
    );
    let sessions = root.path().join("peer-clone").join("source-sessions");
    let stale = sessions.join("323e4567-e89b-42d3-a456-426614174000");
    let noncanonical = sessions.join("423E4567-E89B-42D3-A456-426614174000");
    let canonical_file = sessions.join("523e4567-e89b-42d3-a456-426614174000");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&noncanonical).unwrap();
    std::fs::write(&canonical_file, b"preserve file").unwrap();
    // A canonically named symlink is the escape the sweep must never follow.
    let escaped = root.path().join("escaped-source-session");
    std::fs::create_dir_all(&escaped).unwrap();
    std::fs::write(escaped.join("sentinel"), b"preserve link target").unwrap();
    let canonical_link = sessions.join("723e4567-e89b-42d3-a456-426614174000");
    let linked = create_test_directory_symlink(&canonical_link, &escaped);

    lifecycle.stop().unwrap();

    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Idle);
    assert!(!root
        .path()
        .join("peer-clone")
        .join("active-source.json")
        .exists());
    assert!(!stale.exists());
    assert!(noncanonical.is_dir());
    assert!(canonical_file.is_file());
    if linked {
        // The sweep skips every link instead of deleting through it, so both
        // the link and everything behind it survive an owned-root cleanup.
        assert!(std::fs::symlink_metadata(&canonical_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(escaped.join("sentinel").is_file());
    }
    // Stop has to leave nothing for a later reclaim to find, and the release
    // has to be durable rather than only visible to the open handle.
    drop(store);
    let mut reopened = PersistentStore::open(root.path()).unwrap();
    assert_eq!(
        reopened
            .reclaim_logical_generation_pins(super::delta_commands::P4_SOURCE_PIN_PREFIX)
            .unwrap(),
        0
    );
}

/// Creates a directory symlink, reporting whether the platform allowed it.
/// Windows refuses without Developer Mode or elevation, so callers skip the
/// link-specific assertions instead of failing on an environment limit.
#[cfg(desktop)]
fn create_test_directory_symlink(link: &std::path::Path, target: &std::path::Path) -> bool {
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link);
    #[cfg(not(windows))]
    let created = std::os::unix::fs::symlink(target, link);
    match created {
        Ok(()) => true,
        Err(error) => {
            eprintln!("skipping the source sweep symlink defence: {error}");
            false
        }
    }
}

#[cfg(desktop)]
#[test]
fn real_shared_clone_cleanup_preserves_session_owned_by_a_different_active_marker() {
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    seed_shared_source_store(&mut store);
    let expected_revision = store.revision().unwrap();
    let lifecycle = SharedSessionLifecycle::new(SharedSourceEngines::new());
    let mut context = SharedSourcePreparationContext {
        store: &mut store,
        cas: &cas,
        app_root: root.path(),
        cancellation: &NeverCancelled,
        expected_bidirectional_revision: expected_revision,
        remote_commit: Arc::new(SharedRemoteCommitSlot::default()),
    };
    lifecycle.prepare(&mut context).unwrap();
    let marker_path = root.path().join("peer-clone").join("active-source.json");
    let original = std::fs::read(&marker_path).unwrap();
    let marker: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let active_directory = marker["directoryId"].as_str().unwrap().to_owned();
    let active_session = root
        .path()
        .join("peer-clone")
        .join("source-sessions")
        .join(&active_directory);
    let mut foreign = marker;
    foreign["directoryId"] =
        serde_json::Value::String("623e4567-e89b-42d3-a456-426614174000".to_owned());
    std::fs::write(&marker_path, serde_json::to_vec(&foreign).unwrap()).unwrap();

    assert!(lifecycle.stop().is_err());
    assert!(active_session.is_dir());
    std::fs::write(&marker_path, original).unwrap();
    lifecycle.stop().unwrap();
    assert!(!active_session.exists());
}

#[cfg(desktop)]
fn seed_shared_source_store(store: &mut PersistentStore) {
    let staging = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(
            &staging,
            &serde_json::json!({
                "username": "shared-source",
                "botPresetsId": 0,
                "personas": [{ "id": "persona" }],
                "selectedPersona": 0,
                "enabledModules": [],
                "characterOrder": [],
                "modules": [],
                "loadouts": [],
                "plugins": [],
                "pluginCustomStorage": {},
            }),
        )
        .unwrap();
    store
        .replace_put_presets(&staging, &[serde_json::json!({ "name": "preset" })])
        .unwrap();
    store
        .replace_put_asset_repository_authority(
            &staging,
            &crate::persistent_store::AssetRepositoryAuthorityState::V2 {
                migration_id: "shared-source-assets".to_owned(),
                compatibility_hash: "ab".repeat(32),
            },
        )
        .unwrap();
    store
        .replace_put_cold_payload_authority(
            &staging,
            &crate::persistent_store::ColdPayloadAuthorityState::V2 {
                migration_id: "shared-source-cold".to_owned(),
                compatibility_hash: "cd".repeat(32),
            },
        )
        .unwrap();
    store.replace_commit(&staging, Some(0)).unwrap();
}

/// The remote commit signal is process memory only: a recorded commit stays
/// readable through `stop` so the controller's final status read can still act
/// on it, and only a `prepare` that actually reseals clears it.
const REMOTE_COMMIT_OPERATION: &str = "00000000-0000-4000-8000-0000000000a1";

#[test]
fn android_shared_source_remote_commit_follows_the_same_lifecycle() {
    let _guard = test_registry_guard();
    let events = Arc::new(Mutex::new(AndroidHostEvents::default()));
    let state = android_state(events);
    let root = tempfile::tempdir().unwrap();
    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);

    let prepared = state
        .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
        .unwrap();
    assert_eq!(prepared.last_remote_commit, None);

    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);
    let expected = Some(SharedRemoteCommit {
        operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
        committed_revision: 12,
    });
    assert_eq!(state.status().unwrap().last_remote_commit, expected);
    assert_eq!(
        state
            .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
            .unwrap()
            .last_remote_commit,
        expected,
    );

    assert_eq!(state.stop_for_test().unwrap(), None);

    assert_eq!(state.status().unwrap().last_remote_commit, expected);
    assert_eq!(
        state
            .prepare_for_test(&mut (), root.path(), android_lan_request(32145))
            .unwrap()
            .last_remote_commit,
        None,
    );
}

#[cfg(desktop)]
#[rustfmt::skip]
mod desktop_transport {
    use super::*;

struct CloneFixture(PathBuf);
struct CloneLease(PathBuf);
impl CloneSource for CloneFixture {
    type Lease = CloneLease;
    fn pin(&self) -> Result<Self::Lease, PeerSyncError> {
        Ok(CloneLease(self.0.clone()))
    }
}
impl PinnedCloneRevision for CloneLease {
    fn source_revision(&self) -> u64 {
        1
    }
    fn objects(&self) -> Result<Vec<PinnedSourceObject>, PeerSyncError> {
        Ok(vec![PinnedSourceObject::database(&self.0)])
    }
}
struct Empty(BTreeMap<String, Vec<u8>>);
impl LogicalDeltaObjectSource for Empty {
    fn open_object(
        &mut self,
        object: &LogicalDeltaObject,
    ) -> Result<Box<dyn Read>, PeerSyncError> {
        Ok(Box::new(Cursor::new(
            self.0.get(&object.hash).cloned().unwrap_or_default(),
        )))
    }
}
/// The transport fixtures seal every lane at revision zero and this reports
/// the same, so hello never reseals inside them.
struct NoReseal;
impl SharedSessionResealer for NoReseal {
    fn store_revision(&self) -> Result<i64, PeerSyncError> {
        Ok(0)
    }
    fn reseal_delta(
        &self,
    ) -> Result<SharedSessionSeal<PreparedLogicalLanSession>, PeerSyncError> {
        Err(PeerSyncError::Protocol("fixture".to_owned()))
    }
    fn reseal_bidirectional(
        &self,
    ) -> Result<SharedSessionSeal<PreparedBidirectionalLogicalLanSession>, PeerSyncError> {
        Err(PeerSyncError::Protocol("fixture".to_owned()))
    }
    fn reseal_clone(
        &self,
        _: &dyn CancellationProbe,
    ) -> Result<SharedSessionSeal<PreparedCloneSession>, PeerSyncError> {
        Err(PeerSyncError::Protocol("fixture".to_owned()))
    }
}
struct Control;
impl LanBidirectionalControl for Control {
    fn register(
        &self,
        _: LanBidirectionalSession,
        _: LanBidirectionalRegistrationRequest,
    ) -> Result<(), PeerSyncError> {
        Ok(())
    }
    fn remote_apply(
        &self,
        _: LanBidirectionalSession,
        _: LanBidirectionalRemoteApplyRequest,
        _: &dyn CancellationProbe,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
        Err(PeerSyncError::Protocol("fixture".to_owned()))
    }
}
fn logical() -> (
    String,
    Vec<u8>,
    Vec<LogicalDeltaObject>,
    BTreeMap<String, Vec<u8>>,
) {
    let object_bytes = b"shared logical object".to_vec();
    let object_hash = hex::encode(Sha256::digest(&object_bytes));
    let built = build_logical_manifest(LogicalManifestBuilderInput {
        library_id: "library".to_owned(),
        generation: "g".to_owned(),
        generation_sequence: "1".to_owned(),
        parent_generation: None,
        source_revision: 1,
        records: vec![ProjectedLogicalRecord::live(
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root {
                value: serde_json::json!({}),
                owner_heads: vec![],
            },
            vec![LogicalManifestObject {
                hash: object_hash.clone(),
                size: object_bytes.len() as u64,
            }],
        )],
    })
    .unwrap();
    (
        built.manifest_hash,
        built.manifest_bytes,
        built
            .manifest
            .objects
            .iter()
            .map(|o| LogicalDeltaObject {
                hash: o.hash.clone(),
                size: o.size,
            })
            .collect(),
        BTreeMap::from([(object_hash, object_bytes)]),
    )
}
fn host() -> (tempfile::TempDir, SharedSessionHost) {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("db");
    std::fs::write(&db, b"{}").unwrap();
    let clone = prepare_clone_session(&CloneFixture(db), root.path().join("clone")).unwrap();
    let source = "00000000-0000-4000-8000-000000000010";
    let (hash, bytes, objects, delta_objects) = logical();
    let delta = PreparedLogicalLanSession::new(
        "00000000-0000-4000-8000-000000000011",
        source,
        hash,
        bytes,
        objects,
        Box::new(Empty(delta_objects)),
    )
    .unwrap();
    let (hash, bytes, objects, bidi_objects) = logical();
    let bidi = PreparedBidirectionalLogicalLanSession::new(
        "00000000-0000-4000-8000-000000000012",
        source,
        hash,
        bytes,
        objects,
        Box::new(Empty(bidi_objects)),
        Arc::new(Control),
        Arc::new(SharedRemoteCommitSlot::default()),
    )
    .unwrap();
    let mut host = SharedSessionHost::new(
        SharedSessionSeal {
            session: clone,
            sealed_revision: 0,
        },
        SharedSessionSeal {
            session: delta,
            sealed_revision: 0,
        },
        SharedSessionSeal {
            session: bidi,
            sealed_revision: 0,
        },
        Arc::new(NoReseal),
    )
    .unwrap();
    host.enable_v2_registry(root.path(), "test", DevicePermissions::read())
        .unwrap();
    (root, host)
}

struct TransportPreparation {
    host: Option<SharedSessionHost>,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl SharedSourceOwnership for TransportPreparation {
    type Host = SharedSessionHost;

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        self.events.lock().unwrap().push("build-host");
        self.host
            .take()
            .ok_or_else(|| PeerSyncError::Protocol("fixture host already moved".to_owned()))
    }

    fn cleanup(&mut self, lane: SharedSourceLane) -> Result<(), PeerSyncError> {
        self.events.lock().unwrap().push(match lane {
            SharedSourceLane::Clone => "cleanup-clone",
            SharedSourceLane::Delta => "cleanup-delta",
            SharedSourceLane::Bidirectional => "cleanup-bidirectional",
        });
        Ok(())
    }

    fn stop_host(&mut self, host: &mut Self::Host) -> Result<(), PeerSyncError> {
        self.events.lock().unwrap().push("stop-host");
        host.stop()
    }
}

impl SharedSourcePreparation<()> for TransportPreparation {
    fn prepare(&mut self, lane: SharedSourceLane, _: &mut ()) -> Result<(), PeerSyncError> {
        self.events.lock().unwrap().push(match lane {
            SharedSourceLane::Clone => "prepare-clone",
            SharedSourceLane::Delta => "prepare-delta",
            SharedSourceLane::Bidirectional => "prepare-bidirectional",
        });
        Ok(())
    }
}

fn available_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[cfg(windows)]
#[test]
#[ignore = "requires an active private IPv4 LAN interface on this machine"]
fn windows_lan_sharing_starts_without_a_registered_peer() {
    // Only the adapter list is real. All source data and registration state are
    // synthetic, isolated in a temporary directory by host().
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        None,
    );
    let port = available_port();
    state.prepare(&mut (), root.path(), DeviceSyncPrepareRequest {
        method: DeviceSyncListenMethod::Lan,
        fixed_port: port,
        public_base_url: None,
    }).unwrap();
    let result = state.start(DeviceSyncLinkPermissions { read: true, bidirectional: false });
    // Always release the listener, including before reporting a failed assert.
    state.stop().unwrap();
    let status = result.unwrap();
    assert_eq!(status.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(status.latest_error, None);
    let endpoint = url::Url::parse(status.endpoint.as_deref().unwrap()).unwrap();
    let Some(url::Host::Ipv4(address)) = endpoint.host() else { panic!("expected IPv4 endpoint") };
    assert!(address.is_private() || address.is_link_local());
    assert_eq!(endpoint.port(), Some(port));
    assert!(status.pairing_uri.is_some());
    TcpListener::bind((address, port)).unwrap();
    eprintln!("Synthetic sharing started and stopped without a peer on {address}");
}

#[test]
fn local_lan_address_failure_has_a_distinct_wire_code() {
    assert_eq!(
        serde_json::to_value(super::super::shared_session::DeviceSyncErrorCategory::LanAddressUnavailable).unwrap(),
        "lan-address-unavailable",
    );
}

#[test]
fn unified_lan_transport_uses_the_requested_fixed_port_and_releases_it_on_stop() {
    let (root, host) = host();
    let events = Arc::new(Mutex::new(Vec::new()));
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::clone(&events),
        },
        Ipv4Addr::LOCALHOST,
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();

    let status = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();

    assert_eq!(status.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(
        status.endpoint.as_deref(),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    assert!(status
        .pairing_uri
        .as_deref()
        .unwrap()
        .starts_with("risunestlocal://peer-clone/v2?"));
    assert!(status.expires_at_ms.unwrap() > 0);
    assert_eq!(status.latest_error, None);
    state.stop().unwrap();
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Idle);
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );
}

#[test]
fn concurrent_unified_start_calls_preserve_one_running_listener() {
    let (root, host) = host();
    let state = Arc::new(DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    ));
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let starts = (0..2)
        .map(|_| {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                state
                    .start(DeviceSyncLinkPermissions {
                        read: true,
                        bidirectional: false,
                    })
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let statuses = starts
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(statuses[0], statuses[1]);
    assert_eq!(statuses[0].phase, DeviceSyncSourcePhase::Running);
    assert_eq!(
        reqwest::blocking::Client::new()
            .get(format!("http://127.0.0.1:{port}/v1/peer/hello"))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    state.stop().unwrap();
}

struct FakeSharedTunnelLauncher {
    public_url: url::Url,
    seen_origin: Arc<Mutex<Option<std::net::SocketAddr>>>,
}

impl SharedPeerTunnelLauncher for FakeSharedTunnelLauncher {
    fn start(
        &self,
        host: SharedSessionHost,
    ) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        *self.seen_origin.lock().unwrap() = host.address();
        Ok(Box::new(FakeSharedTunnel {
            _host: host,
            public_url: self.public_url.clone(),
        }))
    }
}

struct FakeSharedTunnel {
    _host: SharedSessionHost,
    public_url: url::Url,
}

impl SharedPeerTunnel for FakeSharedTunnel {
    fn transport_url(&self) -> url::Url {
        self.public_url.clone()
    }

    fn stop(&mut self) -> Result<(), PeerSyncError> {
        Ok(())
    }
}

#[test]
fn unified_quick_transport_uses_the_fixed_loopback_port_and_public_tunnel_url() {
    let (root, host) = host();
    let seen_origin = Arc::new(Mutex::new(None));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(FakeSharedTunnelLauncher {
            public_url: url::Url::parse("https://session-id.trycloudflare.com/").unwrap(),
            seen_origin: Arc::clone(&seen_origin),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();

    let status = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();

    assert_eq!(
        *seen_origin.lock().unwrap(),
        Some((Ipv4Addr::LOCALHOST, port).into())
    );
    assert_eq!(
        status.endpoint.as_deref(),
        Some("https://session-id.trycloudflare.com/")
    );
    assert!(status
        .pairing_uri
        .as_deref()
        .unwrap()
        .contains("endpoint=https%3A%2F%2Fsession-id.trycloudflare.com%2F"));
    state.stop().unwrap();
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
}

struct LifecycleTunnelLauncher {
    lifecycle: Arc<Mutex<SharedPeerTunnelLifecycle>>,
    starts: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedPeerTunnelLauncher for LifecycleTunnelLauncher {
    fn start(
        &self,
        host: SharedSessionHost,
    ) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(LifecycleTunnel {
            _host: host,
            lifecycle: Arc::clone(&self.lifecycle),
        }))
    }
}

struct LifecycleTunnel {
    _host: SharedSessionHost,
    lifecycle: Arc<Mutex<SharedPeerTunnelLifecycle>>,
}

impl SharedPeerTunnel for LifecycleTunnel {
    fn transport_url(&self) -> url::Url {
        url::Url::parse("https://lifecycle.trycloudflare.com/").unwrap()
    }

    fn lifecycle(&mut self) -> Result<SharedPeerTunnelLifecycle, PeerSyncError> {
        Ok(*self.lifecycle.lock().unwrap())
    }

    fn stop(&mut self) -> Result<(), PeerSyncError> {
        *self.lifecycle.lock().unwrap() = SharedPeerTunnelLifecycle::Stopped;
        Ok(())
    }
}

#[test]
fn unified_quick_status_reports_a_naturally_stopped_tunnel() {
    let (root, host) = host();
    let lifecycle = Arc::new(Mutex::new(SharedPeerTunnelLifecycle::Running));
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(LifecycleTunnelLauncher {
            lifecycle: Arc::clone(&lifecycle),
            starts: Arc::clone(&starts),
        }),
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();

    *lifecycle.lock().unwrap() = SharedPeerTunnelLifecycle::Stopped;
    assert!(state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .is_err());
    let status = state.status().unwrap();

    assert_eq!(status.phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        status.latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::TransportUnavailable)
    );
    assert_eq!(status.endpoint, None);
    assert_eq!(status.pairing_uri, None);
    assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    state.stop().unwrap();
}

#[test]
fn cleanup_pending_quick_tunnel_blocks_restart_until_stop_retries_it() {
    let (root, host) = host();
    let lifecycle = Arc::new(Mutex::new(SharedPeerTunnelLifecycle::Running));
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(LifecycleTunnelLauncher {
            lifecycle: Arc::clone(&lifecycle),
            starts: Arc::clone(&starts),
        }),
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();

    *lifecycle.lock().unwrap() = SharedPeerTunnelLifecycle::CleanupPending;
    assert_eq!(state.status().unwrap().phase, DeviceSyncSourcePhase::Error);
    assert!(state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .is_err());
    assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);

    assert_eq!(state.stop().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(
        *lifecycle.lock().unwrap(),
        SharedPeerTunnelLifecycle::Stopped
    );
}

struct FakePublicOriginVerifier {
    seen: Arc<Mutex<Vec<(std::net::SocketAddr, url::Url)>>>,
}

impl SharedPublicOriginVerifier for FakePublicOriginVerifier {
    fn verify(
        &self,
        host: &SharedSessionHost,
        public_url: &url::Url,
    ) -> Result<(), PeerSyncError> {
        self.seen
            .lock()
            .unwrap()
            .push((host.address().unwrap(), public_url.clone()));
        Ok(())
    }
}

#[test]
fn fixed_url_probes_the_external_origin_without_launching_cloudflared() {
    let (root, host) = host();
    let tunnel_origin = Arc::new(Mutex::new(None));
    let probes = Arc::new(Mutex::new(Vec::new()));
    let state = DeviceSyncSourceState::new_for_test_with_transports(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(FakeSharedTunnelLauncher {
            public_url: url::Url::parse("https://must-not-launch.example.com/").unwrap(),
            seen_origin: Arc::clone(&tunnel_origin),
        }),
        Arc::new(FakePublicOriginVerifier {
            seen: Arc::clone(&probes),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::FixedUrl,
                fixed_port: port,
                public_base_url: Some("https://sync.example.com/".to_owned()),
            },
        )
        .unwrap();

    let status = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: true,
        })
        .unwrap();

    assert_eq!(*tunnel_origin.lock().unwrap(), None);
    assert_eq!(
        *probes.lock().unwrap(),
        [(
            (Ipv4Addr::LOCALHOST, port).into(),
            url::Url::parse("https://sync.example.com/").unwrap(),
        )]
    );
    assert_eq!(
        status.endpoint.as_deref(),
        Some("https://sync.example.com/")
    );
    state.stop().unwrap();
}

#[test]
fn invalid_fixed_url_records_only_the_safe_configuration_category() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );

    assert!(state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::FixedUrl,
                fixed_port: available_port(),
                public_base_url: Some("http://localhost/private?token=secret".to_owned()),
            },
        )
        .is_err());

    let status = state.status().unwrap();
    assert_eq!(status.phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        status.latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::InvalidConfiguration)
    );
    assert_eq!(status.endpoint, None);
    assert_eq!(status.pairing_uri, None);
}

#[test]
fn command_invalid_rotation_preserves_status_and_allows_corrected_rotation() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    let initial = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    let initial_pairing = state.pairing_for_test().unwrap();
    let bearer = claim(&initial_pairing, "00000000-0000-4000-8000-000000000032")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let client = reqwest::blocking::Client::new();

    let invalid_result = tauri::async_runtime::block_on(run_device_sync_rotate_link(
        state.clone(),
        DeviceSyncLinkPermissions {
            read: false,
            bidirectional: false,
        },
    ));
    assert_eq!(
        invalid_result,
        Err(super::super::shared_session::DeviceSyncErrorCategory::InvalidConfiguration)
    );
    let after_invalid = state.status().unwrap();
    assert_eq!(after_invalid.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(after_invalid.endpoint, initial.endpoint);
    assert_eq!(after_invalid.pairing_uri, initial.pairing_uri);
    assert_eq!(
        after_invalid.latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::InvalidConfiguration)
    );
    assert_eq!(
        client
            .get(format!("{}/v1/peer/hello", initial_pairing.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    let rotated = tauri::async_runtime::block_on(run_device_sync_rotate_link(
        state.clone(),
        DeviceSyncLinkPermissions {
            read: true,
            bidirectional: true,
        },
    ))
    .unwrap();

    assert_eq!(rotated.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(rotated.endpoint, initial.endpoint);
    assert_ne!(rotated.pairing_uri, initial.pairing_uri);
    assert!(rotated.expires_at_ms.unwrap() >= initial.expires_at_ms.unwrap());
    assert_eq!(
        client
            .get(format!("{}/v1/peer/hello", initial_pairing.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    state.stop().unwrap();
}

#[test]
fn overlapping_command_rotations_keep_results_bound_to_each_operation() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    let initial = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    let initial_pairing = state.pairing_for_test().unwrap();
    let bearer = claim(&initial_pairing, "00000000-0000-4000-8000-000000000033")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let barrier = Arc::new(std::sync::Barrier::new(3));

    let invalid_state = state.clone();
    let invalid_barrier = Arc::clone(&barrier);
    let invalid = std::thread::spawn(move || {
        invalid_barrier.wait();
        tauri::async_runtime::block_on(run_device_sync_rotate_link(
            invalid_state,
            DeviceSyncLinkPermissions {
                read: false,
                bidirectional: false,
            },
        ))
    });
    let valid_state = state.clone();
    let valid_barrier = Arc::clone(&barrier);
    let valid = std::thread::spawn(move || {
        valid_barrier.wait();
        tauri::async_runtime::block_on(run_device_sync_rotate_link(
            valid_state,
            DeviceSyncLinkPermissions {
                read: true,
                bidirectional: true,
            },
        ))
    });
    barrier.wait();

    assert_eq!(
        invalid.join().unwrap(),
        Err(super::super::shared_session::DeviceSyncErrorCategory::InvalidConfiguration)
    );
    let rotated = valid.join().unwrap().unwrap();
    assert_eq!(rotated.phase, DeviceSyncSourcePhase::Running);
    assert_ne!(rotated.pairing_uri, initial.pairing_uri);
    let final_status = state.status().unwrap();
    assert_eq!(final_status.phase, DeviceSyncSourcePhase::Running);
    assert_eq!(final_status.endpoint, initial.endpoint);
    assert_eq!(final_status.pairing_uri, rotated.pairing_uri);
    assert_eq!(
        reqwest::blocking::Client::new()
            .get(format!("{}/v1/peer/hello", initial_pairing.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    state.stop().unwrap();
}

#[test]
fn command_active_host_rotation_failure_enters_error_state() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    let initial = state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    state.remove_host_for_test().unwrap();

    let result = tauri::async_runtime::block_on(run_device_sync_rotate_link(
        state.clone(),
        DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        },
    ));

    assert_eq!(
        result,
        Err(super::super::shared_session::DeviceSyncErrorCategory::StateUnavailable)
    );
    let status = state.status().unwrap();
    assert_eq!(status.phase, DeviceSyncSourcePhase::Error);
    assert_eq!(
        status.latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::StateUnavailable)
    );
    assert_eq!(status.pairing_uri, initial.pairing_uri);
}

struct FailingSharedTunnelLauncher;

impl SharedPeerTunnelLauncher for FailingSharedTunnelLauncher {
    fn start(&self, _: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        Err(PeerSyncError::Transport(
            "raw quick-tunnel diagnostic".to_owned(),
        ))
    }
}

struct RetryableStartCleanupLauncher {
    cleanup_available: std::sync::atomic::AtomicBool,
    cleanup_attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedPeerTunnelLauncher for RetryableStartCleanupLauncher {
    fn start(&self, _: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        Err(PeerSyncError::Transport(
            "raw quick start failure with cleanup owner".to_owned(),
        ))
    }

    fn take_failed_cleanup(&self) -> Option<Box<dyn SharedPeerTunnelCleanup>> {
        self.cleanup_available
            .swap(false, std::sync::atomic::Ordering::SeqCst)
            .then(|| {
                Box::new(RetryableStartCleanup {
                    attempts: Arc::clone(&self.cleanup_attempts),
                }) as Box<dyn SharedPeerTunnelCleanup>
            })
    }
}

struct RetryableStartCleanup {
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedPeerTunnelCleanup for RetryableStartCleanup {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        let attempt = self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == 0 {
            Err(PeerSyncError::Transport(
                "raw quick start cleanup failure".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

/// Holds the start inside the launcher until the test releases it twice, so a
/// competing observer can look at the source while the start is in flight.
struct PausedSharedTunnelLauncher {
    pause: Arc<std::sync::Barrier>,
}

impl SharedPeerTunnelLauncher for PausedSharedTunnelLauncher {
    fn start(&self, host: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        self.pause.wait();
        self.pause.wait();
        Ok(Box::new(FakeSharedTunnel {
            _host: host,
            public_url: url::Url::parse("https://paused-id.trycloudflare.com/").unwrap(),
        }))
    }
}

#[test]
fn status_reports_starting_while_a_start_is_blocked_inside_the_launcher() {
    let (root, host) = host();
    let pause = Arc::new(std::sync::Barrier::new(2));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(PausedSharedTunnelLauncher {
            pause: Arc::clone(&pause),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();
    let starting = {
        let state = state.clone();
        std::thread::spawn(move || {
            state.start(DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            })
        })
    };
    pause.wait();

    // Status must not queue behind the operation the start owns.
    let status = state.status().unwrap();
    assert_eq!(status.phase, DeviceSyncSourcePhase::Starting);
    assert_eq!(status.endpoint, None);
    assert_eq!(status.pairing_uri, None);

    pause.wait();
    assert_eq!(
        starting.join().unwrap().unwrap().phase,
        DeviceSyncSourcePhase::Running
    );
    state.stop().unwrap();
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
}

/// Mirrors the system launcher when the tunnel process is already running but
/// the start still fails: the spawned process is handed over as the cleanup
/// owner instead of being leaked.
struct SpawnedStartCleanupLauncher {
    cleanup_available: std::sync::atomic::AtomicBool,
    stop_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedPeerTunnelLauncher for SpawnedStartCleanupLauncher {
    fn start(&self, _: SharedSessionHost) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        Err(PeerSyncError::Transport(
            "raw quick start failure after the process spawned".to_owned(),
        ))
    }

    fn take_failed_cleanup(&self) -> Option<Box<dyn SharedPeerTunnelCleanup>> {
        self.cleanup_available
            .swap(false, std::sync::atomic::Ordering::SeqCst)
            .then(|| {
                Box::new(SpawnedStartCleanup {
                    stop_calls: Arc::clone(&self.stop_calls),
                }) as Box<dyn SharedPeerTunnelCleanup>
            })
    }
}

struct SpawnedStartCleanup {
    stop_calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl SharedPeerTunnelCleanup for SpawnedStartCleanup {
    fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.stop_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

struct FailingPublicOriginVerifier;

impl SharedPublicOriginVerifier for FailingPublicOriginVerifier {
    fn verify(&self, _: &SharedSessionHost, _: &url::Url) -> Result<(), PeerSyncError> {
        Err(PeerSyncError::Transport(
            "raw fixed-origin diagnostic".to_owned(),
        ))
    }
}

#[test]
fn transport_failures_are_not_misreported_as_port_conflicts() {
    for (method, public_base_url, launcher, verifier) in [
        (
            DeviceSyncListenMethod::Quick,
            None,
            Arc::new(FailingSharedTunnelLauncher) as Arc<dyn SharedPeerTunnelLauncher>,
            Arc::new(FakePublicOriginVerifier {
                seen: Arc::new(Mutex::new(Vec::new())),
            }) as Arc<dyn SharedPublicOriginVerifier>,
        ),
        (
            DeviceSyncListenMethod::FixedUrl,
            Some("https://sync.example.com/".to_owned()),
            Arc::new(FakeSharedTunnelLauncher {
                public_url: url::Url::parse("https://unused.example.com/").unwrap(),
                seen_origin: Arc::new(Mutex::new(None)),
            }) as Arc<dyn SharedPeerTunnelLauncher>,
            Arc::new(FailingPublicOriginVerifier) as Arc<dyn SharedPublicOriginVerifier>,
        ),
    ] {
        let (root, host) = host();
        let state = DeviceSyncSourceState::new_for_test_with_transports(
            TransportPreparation {
                host: Some(host),
                events: Arc::new(Mutex::new(Vec::new())),
            },
            launcher,
            verifier,
        );
        let port = available_port();
        state
            .prepare(
                &mut (),
                root.path(),
                DeviceSyncPrepareRequest {
                    method,
                    fixed_port: port,
                    public_base_url,
                },
            )
            .unwrap();

        assert!(state
            .start(DeviceSyncLinkPermissions {
                read: true,
                bidirectional: false,
            })
            .is_err());
        let status = state.status().unwrap();
        assert_eq!(status.phase, DeviceSyncSourcePhase::Error);
        assert_eq!(
            status.latest_error,
            Some(super::super::shared_session::DeviceSyncErrorCategory::TransportUnavailable)
        );
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        state.stop().unwrap();
    }
}

#[test]
fn failed_quick_start_stops_the_spawned_tunnel_once_without_publishing_a_pairing() {
    let (root, host) = host();
    let stop_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(SpawnedStartCleanupLauncher {
            cleanup_available: std::sync::atomic::AtomicBool::new(true),
            stop_calls: Arc::clone(&stop_calls),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();

    assert!(state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .is_err());

    // The already-spawned process is stopped by the failing start itself.
    assert_eq!(stop_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let status = state.status().unwrap();
    assert_eq!(status.phase, DeviceSyncSourcePhase::Error);
    assert_eq!(status.endpoint, None);
    assert_eq!(status.pairing_uri, None);
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();

    // A cleanup that succeeded is not retained, so stop must not stop it twice.
    assert_eq!(state.stop().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(stop_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn quick_start_failure_retains_failed_cleanup_for_explicit_stop_retry() {
    let (root, host) = host();
    let cleanup_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Arc::new(RetryableStartCleanupLauncher {
            cleanup_available: std::sync::atomic::AtomicBool::new(true),
            cleanup_attempts: Arc::clone(&cleanup_attempts),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();

    assert!(state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .is_err());
    assert_eq!(
        cleanup_attempts.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();

    assert_eq!(state.stop().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(
        cleanup_attempts.load(std::sync::atomic::Ordering::SeqCst),
        2
    );
}

#[test]
fn fixed_port_conflict_has_its_own_safe_error_category() {
    let (root, host) = host();
    let occupied = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();

    assert!(state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .is_err());
    assert_eq!(
        state.status().unwrap().latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::PortUnavailable)
    );
    state.stop().unwrap();
}

struct RetryableTunnelLauncher {
    public_url: url::Url,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl SharedPeerTunnelLauncher for RetryableTunnelLauncher {
    fn start(
        &self,
        host: SharedSessionHost,
    ) -> Result<Box<dyn SharedPeerTunnel>, PeerSyncError> {
        Ok(Box::new(RetryableTunnel {
            _host: host,
            public_url: self.public_url.clone(),
            events: Arc::clone(&self.events),
            fail_once: true,
        }))
    }
}

struct RetryableTunnel {
    _host: SharedSessionHost,
    public_url: url::Url,
    events: Arc<Mutex<Vec<&'static str>>>,
    fail_once: bool,
}

impl SharedPeerTunnel for RetryableTunnel {
    fn transport_url(&self) -> url::Url {
        self.public_url.clone()
    }

    fn stop(&mut self) -> Result<(), PeerSyncError> {
        self.events.lock().unwrap().push("stop-tunnel");
        if std::mem::take(&mut self.fail_once) {
            return Err(PeerSyncError::Transport(
                "raw retryable tunnel cleanup failure".to_owned(),
            ));
        }
        Ok(())
    }
}

#[test]
fn unified_stop_retries_tunnel_after_attempting_host_and_all_prepared_cleanup() {
    let (root, host) = host();
    let events = Arc::new(Mutex::new(Vec::new()));
    let state = DeviceSyncSourceState::new_for_test_with_quick_tunnel(
        TransportPreparation {
            host: Some(host),
            events: Arc::clone(&events),
        },
        Arc::new(RetryableTunnelLauncher {
            public_url: url::Url::parse("https://retry.trycloudflare.com/").unwrap(),
            events: Arc::clone(&events),
        }),
    );
    let port = available_port();
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Quick,
                fixed_port: port,
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();

    assert!(state.stop().is_err());
    assert_eq!(
        state.status().unwrap().latest_error,
        Some(super::super::shared_session::DeviceSyncErrorCategory::CleanupFailed)
    );
    assert_eq!(state.status().unwrap().endpoint, None);
    assert_eq!(state.status().unwrap().pairing_uri, None);
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        [
            "prepare-clone",
            "prepare-delta",
            "prepare-bidirectional",
            "build-host",
            "stop-tunnel",
            "stop-host",
            "cleanup-bidirectional",
            "cleanup-delta",
            "cleanup-clone",
        ]
    );

    assert_eq!(state.stop().unwrap().phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(events.lock().unwrap().last(), Some(&"stop-tunnel"));
    assert_eq!(state.stop().unwrap().phase, DeviceSyncSourcePhase::Idle);
}

#[test]
fn device_sync_wire_names_match_the_settings_contract() {
    let port = available_port();
    let request: DeviceSyncPrepareRequest = serde_json::from_value(serde_json::json!({
        "method": "fixed-url",
        "fixedPort": port,
        "publicBaseUrl": "https://sync.example.com/"
    }))
    .unwrap();
    assert_eq!(request.method, DeviceSyncListenMethod::FixedUrl);
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::json!({
            "method": "fixed-url",
            "fixedPort": port,
            "publicBaseUrl": "https://sync.example.com/"
        })
    );
    assert_eq!(
        serde_json::to_value(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap(),
        serde_json::json!({"read": true, "bidirectional": false})
    );
    assert_eq!(
        serde_json::to_value(
            super::super::shared_session::DeviceSyncErrorCategory::TransportUnavailable
        )
        .unwrap(),
        "transport-unavailable"
    );
    let status = super::super::shared_session::DeviceSyncSourceStatus {
        phase: DeviceSyncSourcePhase::Error,
        endpoint: None,
        pairing_uri: None,
        expires_at_ms: None,
        latest_error: Some(
            super::super::shared_session::DeviceSyncErrorCategory::TransportUnavailable,
        ),
        last_remote_commit: None,
    };
    let serialized = serde_json::to_value(status).unwrap();
    assert_eq!(
        serialized,
        serde_json::json!({
            "phase": "error",
            "endpoint": null,
            "pairingUri": null,
            "expiresAtMs": null,
            "latestError": "transport-unavailable",
            "lastRemoteCommit": null
        })
    );
    assert!(!serialized.to_string().contains("raw"));
    assert!(!serialized.to_string().contains("bearer"));
    let recorded = super::super::shared_session::DeviceSyncSourceStatus {
        phase: DeviceSyncSourcePhase::Running,
        endpoint: None,
        pairing_uri: None,
        expires_at_ms: None,
        latest_error: None,
        last_remote_commit: Some(SharedRemoteCommit {
            operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
            committed_revision: 12,
        }),
    };
    let recorded = serde_json::to_value(recorded).unwrap();
    assert_eq!(
        recorded["lastRemoteCommit"],
        serde_json::json!({
            "operationId": REMOTE_COMMIT_OPERATION,
            "committedRevision": 12
        })
    );
    assert!(!recorded.to_string().contains("raw"));
    assert!(!recorded.to_string().contains("bearer"));
}

#[test]
fn shared_source_status_reports_a_recorded_remote_commit() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    assert_eq!(state.status().unwrap().last_remote_commit, None);

    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);

    assert_eq!(
        state.status().unwrap().last_remote_commit,
        Some(SharedRemoteCommit {
            operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
            committed_revision: 12,
        })
    );
}

#[test]
fn shared_source_stop_keeps_the_recorded_remote_commit() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);

    let stopped = state.stop().unwrap();

    assert_eq!(stopped.phase, DeviceSyncSourcePhase::Idle);
    assert_eq!(
        stopped.last_remote_commit,
        Some(SharedRemoteCommit {
            operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
            committed_revision: 12,
        })
    );
    assert_eq!(
        state.status().unwrap().last_remote_commit,
        Some(SharedRemoteCommit {
            operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
            committed_revision: 12,
        })
    );
}

#[test]
fn shared_source_prepare_clears_the_recorded_remote_commit() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);

    let prepared = state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();

    assert_eq!(prepared.phase, DeviceSyncSourcePhase::Prepared);
    assert_eq!(prepared.last_remote_commit, None);
    assert_eq!(state.status().unwrap().last_remote_commit, None);
}

#[test]
fn shared_source_idempotent_prepare_keeps_the_recorded_remote_commit() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    let request = DeviceSyncPrepareRequest {
        method: DeviceSyncListenMethod::Lan,
        fixed_port: available_port(),
        public_base_url: None,
    };
    state.prepare(&mut (), root.path(), request.clone()).unwrap();
    state
        .remote_commit_slot()
        .record(REMOTE_COMMIT_OPERATION, 12);

    let again = state.prepare(&mut (), root.path(), request).unwrap();

    assert_eq!(again.phase, DeviceSyncSourcePhase::Prepared);
    assert_eq!(
        again.last_remote_commit,
        Some(SharedRemoteCommit {
            operation_id: REMOTE_COMMIT_OPERATION.to_owned(),
            committed_revision: 12,
        })
    );
}

#[test]
fn shared_bidirectional_prepare_hands_the_remote_commit_slot_to_the_session() {
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    seed_shared_source_store(&mut store);
    let expected_revision = store.revision().unwrap();
    let slot = Arc::new(SharedRemoteCommitSlot::default());

    let mut prepared = super::super::bidirectional_commands::prepare_shared_bidirectional_source(
        &mut store,
        &cas,
        root.path(),
        expected_revision,
        Arc::clone(&slot),
    )
    .unwrap();

    let session = prepared.take_session().unwrap();
    assert!(Arc::ptr_eq(session.remote_commit_slot_for_test(), &slot));
    prepared.cleanup();
}
fn claim(pairing: &SharedPairingData, id: &str) -> reqwest::blocking::Response {
    reqwest::blocking::Client::new().post(format!("{}/v1/sessions/{}/claim", pairing.endpoint, pairing.session_id)).json(&serde_json::json!({"claim": pairing.claim, "protocolVersion":2, "deviceId":id, "deviceName":"target"})).send().unwrap()
}

#[test]
fn unified_source_revoke_hook_invalidates_an_established_live_bearer() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    let pairing = state.pairing_for_test().unwrap();
    let device_id = "00000000-0000-4000-8000-000000000031";
    let response = claim(&pairing, device_id);
    assert!(response.status().is_success());
    let bearer = response.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let client = reqwest::blocking::Client::new();
    assert_eq!(
        client
            .get(format!("{}/v1/peer/hello", pairing.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    let registry = root.path().join("peer-sync/devices.json");
    fs::remove_file(&registry).unwrap();
    fs::create_dir(&registry).unwrap();
    state.revoke_registered_device(device_id);

    assert_eq!(
        client
            .get(format!("{}/v1/peer/hello", pairing.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    fs::remove_dir(registry).unwrap();
    state.stop().unwrap();
}

#[test]
fn unified_source_revocation_is_scoped_to_the_revoked_device() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    let permissions = DeviceSyncLinkPermissions {
        read: true,
        bidirectional: false,
    };
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    let endpoint = state.start(permissions).unwrap().endpoint.unwrap();
    let established = |device_id: &str| {
        let pairing = state.pairing_for_test().unwrap();
        let response = claim(&pairing, device_id);
        assert!(response.status().is_success());
        response.json::<serde_json::Value>().unwrap()["bearer"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let kept_device = "00000000-0000-4000-8000-000000000034";
    let revoked_device = "00000000-0000-4000-8000-000000000035";
    let kept = established(kept_device);
    // A pairing link carries a single claim, so the second device registers
    // against a rotated link on the very same running source.
    tauri::async_runtime::block_on(run_device_sync_rotate_link(state.clone(), permissions)).unwrap();
    let revoked = established(revoked_device);
    let client = reqwest::blocking::Client::new();
    let hello = |bearer: &str| {
        client
            .get(format!("{endpoint}/v1/peer/hello"))
            .bearer_auth(bearer)
            .send()
            .unwrap()
            .status()
    };
    assert_eq!(hello(&kept), reqwest::StatusCode::OK);
    assert_eq!(hello(&revoked), reqwest::StatusCode::OK);

    state.revoke_registered_device(revoked_device);

    assert_eq!(hello(&revoked), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(hello(&kept), reqwest::StatusCode::OK);
    state.stop().unwrap();
}

#[test]
fn clone_terminal_progress_persists_last_seen_without_legacy_source_accounting() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    let pairing = state.pairing_for_test().unwrap();
    let device_id = "00000000-0000-4000-8000-000000000032";
    let response = claim(&pairing, device_id);
    let bearer = response.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let client = reqwest::blocking::Client::new();
    let manifest = client
        .get(format!(
            "{}/v1/sessions/{}/manifest",
            pairing.endpoint, pairing.session_id
        ))
        .bearer_auth(&bearer)
        .send()
        .unwrap()
        .json::<CloneManifest>()
        .unwrap();
    let total_bytes = manifest
        .objects
        .values()
        .map(|object| object.size)
        .sum::<u64>();

    for _ in 0..2 {
        assert_eq!(
            client
                .post(format!(
                    "{}/v1/sessions/{}/progress",
                    pairing.endpoint, pairing.session_id
                ))
                .bearer_auth(&bearer)
                .json(&serde_json::json!({
                    "verifiedBytes": total_bytes,
                    "currentObject": null
                }))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
    }

    let registry = OutgoingDeviceRegistry::load(root.path()).unwrap();
    assert_eq!(registry.devices()[0].total_bytes, 0);
    assert!(registry.devices()[0].last_seen_ms > 0);
    state.stop().unwrap();
}

#[test]
fn clone_terminal_progress_with_operation_id_defers_completion_accounting() {
    let (root, host) = host();
    let state = DeviceSyncSourceState::new_for_test(
        TransportPreparation {
            host: Some(host),
            events: Arc::new(Mutex::new(Vec::new())),
        },
        Ipv4Addr::LOCALHOST,
    );
    state
        .prepare(
            &mut (),
            root.path(),
            DeviceSyncPrepareRequest {
                method: DeviceSyncListenMethod::Lan,
                fixed_port: available_port(),
                public_base_url: None,
            },
        )
        .unwrap();
    state
        .start(DeviceSyncLinkPermissions {
            read: true,
            bidirectional: false,
        })
        .unwrap();
    let pairing = state.pairing_for_test().unwrap();
    let response = claim(&pairing, "00000000-0000-4000-8000-000000000033");
    let bearer = response.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let client = reqwest::blocking::Client::new();
    let manifest_url = format!(
        "{}/v1/sessions/{}/manifest",
        pairing.endpoint, pairing.session_id
    );
    let legacy_manifest = client
        .get(&manifest_url)
        .bearer_auth(&bearer)
        .send()
        .unwrap();
    assert!(legacy_manifest
        .headers()
        .get(PEER_COMPLETION_LEASE_HEADER)
        .is_none());
    let manifest = legacy_manifest.json::<CloneManifest>().unwrap();
    let total_bytes = manifest
        .objects
        .values()
        .map(|object| object.size)
        .sum::<u64>();
    let first_manifest = client
        .get(&manifest_url)
        .bearer_auth(&bearer)
        .header(
            PEER_COMPLETION_CAPABILITY_HEADER,
            PEER_COMPLETION_CAPABILITY_V1,
        )
        .send()
        .unwrap();
    assert_eq!(first_manifest.status(), reqwest::StatusCode::OK);
    assert_eq!(
        first_manifest
            .headers()
            .get(PEER_COMPLETION_CAPABILITY_HEADER)
            .unwrap(),
        PEER_COMPLETION_CAPABILITY_V1
    );
    let lease_a = first_manifest
        .headers()
        .get(PEER_COMPLETION_LEASE_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(uuid::Uuid::parse_str(&lease_a).unwrap().get_version_num(), 4);
    let retry_manifest = client
        .get(&manifest_url)
        .bearer_auth(&bearer)
        .header(
            PEER_COMPLETION_CAPABILITY_HEADER,
            PEER_COMPLETION_CAPABILITY_V1,
        )
        .send()
        .unwrap();
    assert_eq!(retry_manifest.status(), reqwest::StatusCode::OK);
    assert_eq!(
        retry_manifest
            .headers()
            .get(PEER_COMPLETION_LEASE_HEADER)
            .unwrap(),
        lease_a.as_str()
    );
    assert_eq!(
        client
            .get(&manifest_url)
            .bearer_auth(&bearer)
            .header(
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
            )
            .header(
                PEER_COMPLETION_RESUME_HEADER,
                "00000000-0000-4000-8000-000000000035",
            )
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    let progress_url = format!(
        "{}/v1/sessions/{}/progress",
        pairing.endpoint, pairing.session_id
    );

    for operation_id in [None, Some("00000000-0000-4000-8000-000000000034")] {
        let mut progress = serde_json::json!({
            "verifiedBytes": total_bytes,
            "currentObject": null
        });
        if let Some(operation_id) = operation_id {
            progress["operationId"] = operation_id.into();
        }
        assert_eq!(
            client
                .post(&progress_url)
                .bearer_auth(&bearer)
                .json(&progress)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );
    }
    assert_eq!(
        client
            .post(&progress_url)
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "verifiedBytes": total_bytes,
                "currentObject": null,
                "operationId": lease_a
            }))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
        0
    );
    assert_eq!(
        client
            .get(&manifest_url)
            .bearer_auth(&bearer)
            .header(
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
            )
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    let resumed = client
        .get(&manifest_url)
        .bearer_auth(&bearer)
        .header(
            PEER_COMPLETION_CAPABILITY_HEADER,
            PEER_COMPLETION_CAPABILITY_V1,
        )
        .header(PEER_COMPLETION_RESUME_HEADER, &lease_a)
        .send()
        .unwrap();
    assert_eq!(resumed.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resumed
            .headers()
            .get(PEER_COMPLETION_LEASE_HEADER)
            .unwrap(),
        lease_a.as_str()
    );
    let completion_url = format!("{}/v1/peer/completion", pairing.endpoint);
    let completion_a = serde_json::json!({
        "schema": PEER_COMPLETION_SCHEMA,
        "lane": "clone",
        "operationId": lease_a,
        "manifestId": pairing.manifest_id,
        "transferredBytes": total_bytes
    });
    for _ in 0..2 {
        assert_eq!(
            client
                .post(&completion_url)
                .bearer_auth(&bearer)
                .json(&completion_a)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
        total_bytes
    );
    let next_manifest = client
        .get(format!(
            "{}/v1/sessions/{}/manifest",
            pairing.endpoint, pairing.session_id
        ))
        .bearer_auth(&bearer)
        .header(
            PEER_COMPLETION_CAPABILITY_HEADER,
            PEER_COMPLETION_CAPABILITY_V1,
        )
        .send()
        .unwrap();
    assert_eq!(next_manifest.status(), reqwest::StatusCode::OK);
    let lease_b = next_manifest
        .headers()
        .get(PEER_COMPLETION_LEASE_HEADER)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_ne!(lease_b, lease_a);

    assert_eq!(
        client
            .post(&progress_url)
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "verifiedBytes": total_bytes,
                "currentObject": null,
                "operationId": lease_a
            }))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        client
            .post(&completion_url)
            .bearer_auth(&bearer)
            .json(&completion_a)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        client
            .post(&progress_url)
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "verifiedBytes": total_bytes,
                "currentObject": null,
                "operationId": lease_b
            }))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    let completion_b = serde_json::json!({
        "schema": PEER_COMPLETION_SCHEMA,
        "lane": "clone",
        "operationId": lease_b,
        "manifestId": pairing.manifest_id,
        "transferredBytes": total_bytes
    });
    assert_eq!(
        client
            .post(&completion_url)
            .bearer_auth(&bearer)
            .json(&completion_b)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        client
            .post(&progress_url)
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "verifiedBytes": total_bytes,
                "currentObject": null,
                "operationId": lease_a
            }))
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        OutgoingDeviceRegistry::load(root.path()).unwrap().devices()[0].total_bytes,
        total_bytes.checked_mul(2).unwrap()
    );
    state.stop().unwrap();
}

#[test]
fn canonical_registration_uri_has_exact_clone_fields_and_no_bearer() {
    let port = available_port();
    let pairing = SharedPairingData {
        endpoint: format!("http://127.0.0.1:{port}"),
        session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        manifest_id: "a".repeat(64),
        claim: "b".repeat(64),
        expires_at_ms: 1_000,
    };

    let uri = pairing.canonical_uri();

    assert!(uri.starts_with("risunestlocal://peer-clone/v2?"));
    assert!(uri.contains(&format!("endpoint=http%3A%2F%2F127.0.0.1%3A{port}")));
    assert!(uri.contains("session=00000000-0000-4000-8000-000000000001"));
    assert!(uri.contains(&format!("manifest={}", "a".repeat(64))));
    assert!(uri.ends_with(&format!("#claim={}", "b".repeat(64))));
    assert!(!uri.contains("bearer"));
}

#[test]
fn one_listener_routes_all_lanes_and_hello_returns_exact_descriptors() {
    let (_root, mut host) = host();
    let pairing = host.start_fixed_loopback(available_port()).unwrap();
    assert!(pairing.expires_at_ms > 0);
    let response = claim(&pairing, "00000000-0000-4000-8000-000000000020");
    assert!(response.status().is_success());
    let bearer = response.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let hello: serde_json::Value = reqwest::blocking::Client::new()
        .get(format!("{}/v1/peer/hello", pairing.endpoint))
        .bearer_auth(&bearer)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(hello["lanes"]["clone"]["sessionId"], pairing.session_id);
    assert_eq!(
        hello["lanes"]["delta"]["sessionId"],
        "00000000-0000-4000-8000-000000000011"
    );
    assert_eq!(
        hello["lanes"]["bidirectional"]["sessionId"],
        "00000000-0000-4000-8000-000000000012"
    );
    for (lane, expected_status) in [
        (pairing.session_id.as_str(), reqwest::StatusCode::OK),
        (
            "00000000-0000-4000-8000-000000000011",
            reqwest::StatusCode::OK,
        ),
        (
            "00000000-0000-4000-8000-000000000012",
            reqwest::StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            reqwest::blocking::Client::new()
                .get(format!("{}/v1/sessions/{lane}/manifest", pairing.endpoint))
                .bearer_auth(&bearer)
                .send()
                .unwrap()
                .status(),
            expected_status
        );
    }
    let manifest: serde_json::Value = reqwest::blocking::Client::new()
        .get(format!(
            "{}/v1/sessions/{}/manifest",
            pairing.endpoint, pairing.session_id
        ))
        .bearer_auth(&bearer)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let object = manifest["objects"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();
    assert_eq!(
        reqwest::blocking::Client::new()
            .head(format!(
                "{}/v1/sessions/{}/objects/{object}",
                pairing.endpoint, pairing.session_id
            ))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        reqwest::blocking::Client::new()
            .post(format!(
                "{}/v1/sessions/00000000-0000-4000-8000-000000000012/registration",
                pairing.endpoint
            ))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    host.stop().unwrap();
}

#[test]
fn fixed_port_rejects_zero_and_rotation_invalidates_only_pending_claim() {
    let (_root, mut shared) = host();
    assert!(shared
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, 0)
        .is_err());
    let port = available_port();
    let pairing = shared
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, port)
        .unwrap();
    let advertised = validate_lan_endpoint(&pairing.endpoint).unwrap();
    assert_eq!(advertised, pairing.endpoint);
    assert!(!advertised.contains("0.0.0.0"));
    let (_other_root, mut other) = host();
    assert!(other
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, port)
        .is_err());
    let established = claim(&pairing, "00000000-0000-4000-8000-000000000021");
    let bearer = established.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let old = pairing.claim.clone();
    let replacement = shared.rotate_link().unwrap();
    assert_eq!(pairing.endpoint, replacement.endpoint);
    assert_eq!(
        validate_lan_endpoint(&replacement.endpoint).unwrap(),
        advertised
    );
    assert!(!replacement.endpoint.contains("0.0.0.0"));
    assert_eq!(
        reqwest::blocking::Client::new()
            .get(format!(
                "{}/v1/sessions/{}/manifest",
                pairing.endpoint, pairing.session_id
            ))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        claim(
            &SharedPairingData {
                claim: old,
                ..pairing.clone()
            },
            "00000000-0000-4000-8000-000000000021"
        )
        .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    assert!(claim(&replacement, "00000000-0000-4000-8000-000000000022")
        .status()
        .is_success());
    shared.stop().unwrap();
}

#[test]
fn expired_link_is_rejected_by_monotonic_enforcement() {
    let (_root, mut host) = host();
    let pairing = host.start_fixed_loopback(available_port()).unwrap();
    host.expire_link_for_test();
    assert_eq!(
        claim(&pairing, "00000000-0000-4000-8000-000000000023").status(),
        reqwest::StatusCode::GONE
    );
    host.stop().unwrap();
}

#[test]
fn fixed_lan_advertises_the_reachable_address_not_wildcard() {
    let (_root, mut host) = host();
    let pairing = host
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, available_port())
        .unwrap();
    assert_eq!(
        validate_lan_endpoint(&pairing.endpoint).unwrap(),
        pairing.endpoint
    );
    assert!(claim(&pairing, "00000000-0000-4000-8000-000000000025")
        .status()
        .is_success());
    host.stop().unwrap();
}

#[test]
fn permitted_bidirectional_registration_reaches_existing_control() {
    let (root, mut host) = host();
    host.enable_v2_registry(
        root.path(),
        "test",
        DevicePermissions::read_and_bidirectional(),
    )
    .unwrap();
    let pairing = host.start_fixed_loopback(available_port()).unwrap();
    let bearer = claim(&pairing, "00000000-0000-4000-8000-000000000026")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let manifest = reqwest::blocking::Client::new()
        .get(format!("{}/v1/peer/hello", pairing.endpoint))
        .bearer_auth(&bearer)
        .send()
        .unwrap()
        .json::<serde_json::Value>()
        .unwrap()["lanes"]["bidirectional"]["manifestId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(reqwest::blocking::Client::new().post(format!("{}/v1/sessions/00000000-0000-4000-8000-000000000012/registration", pairing.endpoint)).bearer_auth(&bearer).json(&serde_json::json!({"libraryId":"library","generation":{"generationId":"g","manifestHash":manifest,"generationSequence":"1"},"expectedRevision":0})).send().unwrap().status(), reqwest::StatusCode::NO_CONTENT);
    host.stop().unwrap();
}

#[test]
fn delta_object_body_uses_the_shared_listener() {
    let (_root, mut host) = host();
    let pairing = host.start_fixed_loopback(available_port()).unwrap();
    let bearer = claim(&pairing, "00000000-0000-4000-8000-000000000027")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let delta = "00000000-0000-4000-8000-000000000011";
    let manifest: serde_json::Value = reqwest::blocking::Client::new()
        .get(format!("{}/v1/sessions/{delta}/manifest", pairing.endpoint))
        .bearer_auth(&bearer)
        .send()
        .unwrap()
        .json()
        .unwrap();
    let object = hex::encode(Sha256::digest(b"shared logical object"));
    assert!(manifest["objects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value["hash"] == object));
    let response = reqwest::blocking::Client::new()
        .get(format!(
            "{}/v1/sessions/{delta}/objects/{object}",
            pairing.endpoint
        ))
        .bearer_auth(&bearer)
        .send()
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.bytes().unwrap().as_ref(), b"shared logical object");
    host.stop().unwrap();
}

#[test]
fn stop_clears_runtime_and_restart_rehydrates_persisted_bearer() {
    let (_root, mut host) = host();
    let port = available_port();
    let pairing = host.start_fixed_loopback(port).unwrap();
    let bearer = claim(&pairing, "00000000-0000-4000-8000-000000000024")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    host.stop().unwrap();
    let restarted = host.start_fixed_loopback(port).unwrap();
    assert_eq!(
        reqwest::blocking::Client::new()
            .get(format!("{}/v1/peer/hello", restarted.endpoint))
            .bearer_auth(&bearer)
            .send()
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    host.stop().unwrap();
}
}

/// A prepared, running shared source over a real store, a real listener and the
/// real preparation engines.  The lifecycle owns the host, so the store handle
/// stays available for the local commits these scenarios make.
#[cfg(desktop)]
struct RealSharedSource {
    root: tempfile::TempDir,
    store: PersistentStore,
    remote_commit: Arc<SharedRemoteCommitSlot>,
    lifecycle: SharedSessionLifecycle<SharedSourceEngines>,
    endpoint: String,
    bearer: String,
}

#[cfg(desktop)]
const RESEAL_TARGET_DEVICE_ID: &str = "00000000-0000-4000-8000-0000000004a1";

#[cfg(desktop)]
fn started_real_shared_source() -> RealSharedSource {
    let root = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(root.path()).unwrap();
    let mut store = PersistentStore::open(root.path()).unwrap();
    seed_shared_source_store(&mut store);
    let expected_revision = store.revision().unwrap();
    let remote_commit = Arc::new(SharedRemoteCommitSlot::default());
    let lifecycle = SharedSessionLifecycle::new(SharedSourceEngines::new());
    {
        let mut context = SharedSourcePreparationContext {
            store: &mut store,
            cas: &cas,
            app_root: root.path(),
            cancellation: &NeverCancelled,
            expected_bidirectional_revision: expected_revision,
            remote_commit: Arc::clone(&remote_commit),
        };
        lifecycle.prepare(&mut context).unwrap();
    }
    // A port the kernel just handed back is free without hard-coding one.
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let pairing = lifecycle
        .with_host_mut(|host| {
            host.enable_v2_registry(
                root.path(),
                "Windows",
                DevicePermissions::read_and_bidirectional(),
            )?;
            host.start_fixed_loopback(port)
        })
        .unwrap()
        .unwrap();
    // The pairing link names the clone lane, so the claim goes through the raw
    // endpoint instead of one lane's client.
    let claimed = reqwest::blocking::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/claim",
            pairing.endpoint, pairing.session_id
        ))
        .json(&serde_json::json!({
            "claim": pairing.claim,
            "deviceId": RESEAL_TARGET_DEVICE_ID,
            "protocolVersion": 2,
            "deviceName": "Android",
        }))
        .send()
        .unwrap();
    assert_eq!(claimed.status(), reqwest::StatusCode::OK);
    let bearer = claimed.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    RealSharedSource {
        root,
        store,
        remote_commit,
        lifecycle,
        endpoint: pairing.endpoint,
        bearer,
    }
}

#[cfg(desktop)]
impl RealSharedSource {
    fn hello_lanes(&self) -> serde_json::Value {
        let response = reqwest::blocking::Client::new()
            .get(format!("{}/v1/peer/hello", self.endpoint))
            .bearer_auth(&self.bearer)
            .send()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.json::<serde_json::Value>().unwrap()["lanes"].clone()
    }

    fn commit_local_change(&mut self, username: &str) {
        let revision = self.store.revision().unwrap();
        let staging = self.store.replace_begin().unwrap().staging_id;
        self.store
            .replace_put_root(
                &staging,
                &serde_json::json!({
                    "username": username,
                    "botPresetsId": 0,
                    "personas": [{ "id": "persona" }],
                    "selectedPersona": 0,
                    "enabledModules": [],
                    "characterOrder": [],
                    "modules": [],
                    "loadouts": [],
                    "plugins": [],
                    "pluginCustomStorage": {},
                }),
            )
            .unwrap();
        self.store
            .replace_put_presets(&staging, &[serde_json::json!({ "name": "preset" })])
            .unwrap();
        self.store
            .replace_put_asset_repository_authority(
                &staging,
                &crate::persistent_store::AssetRepositoryAuthorityState::V2 {
                    migration_id: "shared-source-assets".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        self.store
            .replace_put_cold_payload_authority(
                &staging,
                &crate::persistent_store::ColdPayloadAuthorityState::V2 {
                    migration_id: "shared-source-cold".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        self.store.replace_commit(&staging, Some(revision)).unwrap();
    }

    fn lane_sessions(&self) -> Vec<(SharedSessionKind, String, String, i64)> {
        self.lifecycle
            .with_host_mut(|host| host.control_for_test().unwrap().lane_sessions_for_test())
            .unwrap()
    }

    fn retired_session_ids(&self) -> Vec<String> {
        self.lifecycle
            .with_host_mut(|host| {
                host.control_for_test()
                    .unwrap()
                    .retired_session_ids_for_test()
            })
            .unwrap()
    }
}

#[cfg(desktop)]
fn lane_session_id(lanes: &serde_json::Value, lane: &str) -> String {
    lanes[lane]["sessionId"].as_str().unwrap().to_owned()
}

#[cfg(desktop)]
fn lane_manifest_id(lanes: &serde_json::Value, lane: &str) -> String {
    lanes[lane]["manifestId"].as_str().unwrap().to_owned()
}

#[cfg(desktop)]
#[test]
fn shared_hello_reseals_the_logical_lanes_after_a_local_commit() {
    let mut source = started_real_shared_source();
    let before = source.hello_lanes();

    source.commit_local_change("after-the-first-hello");
    let after = source.hello_lanes();

    for lane in ["delta", "bidirectional"] {
        assert_ne!(
            lane_session_id(&before, lane),
            lane_session_id(&after, lane)
        );
        assert_ne!(
            lane_manifest_id(&before, lane),
            lane_manifest_id(&after, lane)
        );
    }
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_hello_keeps_the_same_sessions_when_nothing_changed() {
    let source = started_real_shared_source();

    let first = source.hello_lanes();
    let second = source.hello_lanes();
    let third = source.hello_lanes();

    assert_eq!(first, second);
    assert_eq!(second, third);
    assert!(source.retired_session_ids().is_empty());
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_hello_leaves_the_clone_lane_at_its_prepared_seal() {
    let mut source = started_real_shared_source();
    let before = source.hello_lanes();
    let prepared_clone = source
        .lane_sessions()
        .into_iter()
        .find(|(kind, ..)| *kind == SharedSessionKind::Clone)
        .unwrap();

    source.commit_local_change("after-the-clone-seal");
    let after = source.hello_lanes();

    assert_eq!(
        lane_session_id(&before, "clone"),
        lane_session_id(&after, "clone")
    );
    assert_eq!(
        lane_manifest_id(&before, "clone"),
        lane_manifest_id(&after, "clone")
    );
    assert_eq!(prepared_clone.1, lane_session_id(&after, "clone"));
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn a_retired_shared_delta_session_still_serves_its_manifest() {
    let mut source = started_real_shared_source();
    let before = source.hello_lanes();
    let retired = lane_session_id(&before, "delta");

    source.commit_local_change("after-the-retired-session");
    let after = source.hello_lanes();
    let response = reqwest::blocking::Client::new()
        .get(format!(
            "{}/v1/sessions/{retired}/manifest",
            source.endpoint
        ))
        .bearer_auth(&source.bearer)
        .send()
        .unwrap();

    assert_ne!(lane_session_id(&after, "delta"), retired);
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers().get(reqwest::header::ETAG).unwrap(),
        format!("\"{}\"", lane_manifest_id(&before, "delta")).as_str()
    );
    assert!(source.retired_session_ids().contains(&retired));
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_reseal_keeps_the_generation_pin_count_bounded() {
    let mut source = started_real_shared_source();

    for round in 0..8 {
        source.commit_local_change(&format!("pin-bound-{round}"));
        source.hello_lanes();
    }

    // One live session plus the retirement cap, for both logical prefixes.
    let bound = MAX_RETIRED_SESSIONS_PER_LANE + 1;
    assert!(
        source
            .store
            .count_logical_generation_pins(super::delta_commands::P4_SOURCE_PIN_PREFIX)
            .unwrap()
            <= bound
    );
    assert!(
        source
            .store
            .count_logical_generation_pins(super::bidirectional_commands::P5_SOURCE_PIN_PREFIX)
            .unwrap()
            <= bound
    );
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_stop_releases_every_session_and_pin() {
    let mut source = started_real_shared_source();
    source.commit_local_change("before-stop");
    source.hello_lanes();

    source.lifecycle.stop().unwrap();

    let root = source.root.path().to_path_buf();
    drop(source);
    let mut reopened = PersistentStore::open(&root).unwrap();
    assert_eq!(
        reopened
            .reclaim_logical_generation_pins(super::delta_commands::P4_SOURCE_PIN_PREFIX)
            .unwrap(),
        0
    );
    assert_eq!(
        reopened
            .reclaim_logical_generation_pins(super::bidirectional_commands::P5_SOURCE_PIN_PREFIX)
            .unwrap(),
        0
    );
}

#[cfg(desktop)]
#[test]
fn shared_hello_defers_the_bidirectional_lane_while_a_target_operation_is_retained() {
    let mut source = started_real_shared_source();
    let before = source.hello_lanes();
    retain_foreign_source_operation(source.root.path());

    source.commit_local_change("while-a-target-operation-is-retained");
    let after = source.hello_lanes();

    assert_ne!(
        lane_session_id(&before, "delta"),
        lane_session_id(&after, "delta")
    );
    assert_eq!(
        lane_session_id(&before, "bidirectional"),
        lane_session_id(&after, "bidirectional")
    );
    source.lifecycle.stop().unwrap();
}

/// Writes a retained operation prepared for a different source device, which is
/// exactly what `retained_allows_source_prepare` refuses.
#[cfg(desktop)]
fn retain_foreign_source_operation(app_root: &std::path::Path) {
    let generation = |id: &str, fill: char| SyncGenerationIdentity {
        generation_id: id.to_owned(),
        manifest_hash: fill.to_string().repeat(64),
        generation_sequence: "0".to_owned(),
    };
    PeerBidirectionalOperationJournal::new(app_root)
        .store(&PeerBidirectionalDurableOperation::SourcePrepared {
            schema: PEER_BIDIRECTIONAL_OPERATION_SCHEMA.to_owned(),
            operation_id: "123e4567-e89b-42d3-a456-426614174300".to_owned(),
            source_device_id: "123e4567-e89b-42d3-a456-426614174301".to_owned(),
            target_device_id: "123e4567-e89b-42d3-a456-426614174302".to_owned(),
            expected_source_revision: 0,
            previous_shared: generation("previous", 'a'),
            expected_source_generation: generation("source", 'b'),
            shared_generation: LanBidirectionalGeneration {
                generation_id: "shared".to_owned(),
                manifest_hash: "c".repeat(64),
                generation_sequence: "1".to_owned(),
            },
            incoming_revision: 1,
            transferred_objects: 0,
            transferred_bytes: 0,
            backup_required: false,
            backup: None,
            durable_job_id: "123e4567-e89b-42d3-a456-426614174303".to_owned(),
        })
        .unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_hello_reseal_keeps_the_recorded_remote_commit() {
    let mut source = started_real_shared_source();
    source
        .remote_commit
        .record("00000000-0000-4000-8000-0000000004b1", 12);
    let expected = Some(SharedRemoteCommit {
        operation_id: "00000000-0000-4000-8000-0000000004b1".to_owned(),
        committed_revision: 12,
    });

    source.commit_local_change("keeps-the-remote-commit");
    source.hello_lanes();

    assert_eq!(source.remote_commit.latest(), expected);
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
#[test]
fn shared_reseal_hands_the_same_remote_commit_slot_to_the_new_session() {
    let mut source = started_real_shared_source();
    let before = source.hello_lanes();

    source.commit_local_change("keeps-the-slot");
    let after = source.hello_lanes();
    let slot = source
        .lifecycle
        .with_host_mut(|host| {
            host.control_for_test()
                .unwrap()
                .bidirectional_remote_commit_slot_for_test()
        })
        .unwrap()
        .unwrap();

    assert_ne!(
        lane_session_id(&before, "bidirectional"),
        lane_session_id(&after, "bidirectional")
    );
    assert!(Arc::ptr_eq(&slot, &source.remote_commit));
    source.lifecycle.stop().unwrap();
}

#[cfg(desktop)]
impl RealSharedSource {
    fn clone_session(&self) -> super::lan::PeerHelloLane {
        authenticated_peer_clone_session(&self.endpoint, &self.bearer).unwrap()
    }

    fn active_clone_marker(&self) -> serde_json::Value {
        serde_json::from_slice(
            &fs::read(
                self.root
                    .path()
                    .join("peer-clone")
                    .join("active-source.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }
}

/// The full copy replaces the target's data wholesale, so a source that has
/// moved on rebuilds its package before it hands out a descriptor.
#[cfg(desktop)]
#[test]
fn clone_session_request_rebuilds_the_package_after_a_local_commit() {
    let mut source = started_real_shared_source();
    let prepared = source.hello_lanes();
    let prepared_directory = prepared_source_root(source.root.path());

    let unchanged = source.clone_session();
    source.commit_local_change("before-the-full-copy");
    let refreshed = source.clone_session();

    assert_eq!(
        unchanged.session_id,
        lane_session_id(&prepared, "clone"),
        "an unchanged store answers with the package it already sealed"
    );
    assert_ne!(refreshed.session_id, unchanged.session_id);
    assert_eq!(
        source.active_clone_marker()["sessionId"].as_str().unwrap(),
        refreshed.session_id
    );
    // Hello follows the refreshed lane, and the superseded package is gone so a
    // full lossless copy of the data cannot pile up per reseal.
    assert_eq!(
        lane_session_id(&source.hello_lanes(), "clone"),
        refreshed.session_id
    );
    assert!(!prepared_directory.exists());
    assert!(prepared_source_root(source.root.path()).is_dir());
    source.lifecycle.stop().unwrap();
}

/// Stop still owns every artifact the reseal produced, including the clone
/// package the reseal replaced the prepared one with.
#[cfg(desktop)]
#[test]
fn shared_stop_removes_the_resealed_clone_package_and_marker() {
    let mut source = started_real_shared_source();
    source.commit_local_change("before-stop-after-reseal");
    source.clone_session();
    let resealed = prepared_source_root(source.root.path());

    source.lifecycle.stop().unwrap();

    assert!(!resealed.exists());
    assert!(!source
        .root
        .path()
        .join("peer-clone")
        .join("active-source.json")
        .exists());
}

/// Every owned clone source directory, so a cancelled build is visible as an
/// extra one rather than only as a missing marker.
#[cfg(desktop)]
fn source_session_directories(app_root: &std::path::Path) -> Vec<PathBuf> {
    let sessions = app_root.join("peer-clone").join("source-sessions");
    let Ok(entries) = fs::read_dir(&sessions) else {
        return Vec::new();
    };
    let mut directories = entries
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

/// The pairing link names the clone lane, so a device that opens the link has
/// to find the session it names.  A registered device asking for a full copy in
/// that window clones the package this host already sealed.
#[cfg(desktop)]
#[test]
fn a_clone_session_request_holds_the_lane_while_a_pairing_link_is_claimable() {
    let mut source = started_real_shared_source();
    let prepared = source.hello_lanes();
    let prepared_directory = prepared_source_root(source.root.path());
    let link = source
        .lifecycle
        .with_host_mut(|host| host.rotate_link())
        .unwrap()
        .unwrap();

    source.commit_local_change("while-the-pairing-link-is-claimable");
    let deferred = source.clone_session();
    // The sealed package the deferred descriptor names is still on disk, so the
    // device that opens the link can still take it.
    let held_directories = source_session_directories(source.root.path());
    source
        .lifecycle
        .with_host_mut(|host| host.expire_link_for_test())
        .unwrap();
    let refreshed = source.clone_session();

    assert_eq!(link.session_id, lane_session_id(&prepared, "clone"));
    assert_eq!(deferred.session_id, link.session_id);
    assert_eq!(held_directories, [prepared_directory.clone()]);
    // The expired link no longer binds the lane, so the same request reseals.
    assert_ne!(refreshed.session_id, deferred.session_id);
    assert!(!prepared_directory.exists());
    source.lifecycle.stop().unwrap();
}

/// "Stop sharing" cancels the package build, and a cancelled build leaves the
/// lane, the marker and the owned directories exactly as they were.
#[cfg(desktop)]
#[test]
fn a_cancelled_clone_reseal_leaves_no_package_behind() {
    let mut source = started_real_shared_source();
    let sealed = lane_session_id(&source.hello_lanes(), "clone");
    let prepared_directory = prepared_source_root(source.root.path());
    let resealer = source
        .lifecycle
        .with_preparation(|engines| engines.reseal_engine_for_test())
        .unwrap()
        .unwrap();

    source.commit_local_change("before-the-cancelled-reseal");
    let cancelled =
        resealer.reseal_clone(&AtomicCancellation::new(Arc::new(AtomicBool::new(true))));

    assert!(cancelled.is_err());
    assert_eq!(lane_session_id(&source.hello_lanes(), "clone"), sealed);
    assert_eq!(
        source.active_clone_marker()["sessionId"].as_str().unwrap(),
        sealed
    );
    assert_eq!(
        source_session_directories(source.root.path()),
        [prepared_directory]
    );
    source.lifecycle.stop().unwrap();
}
