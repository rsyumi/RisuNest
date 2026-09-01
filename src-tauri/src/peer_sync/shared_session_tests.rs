use super::lan::validate_lan_endpoint;
#[cfg(desktop)]
use super::shared_session::{SharedSourceEngines, SharedSourcePreparationContext};
use super::{
    device_registry::DevicePermissions,
    lan::{
        LanBidirectionalControl, LanBidirectionalRegistrationRequest,
        LanBidirectionalRemoteApplyReceipt, LanBidirectionalRemoteApplyRequest,
        LanBidirectionalSession, PreparedBidirectionalLogicalLanSession, PreparedLogicalLanSession,
    },
    logical_delta::{
        build_logical_manifest, LogicalManifestBuilderInput, LogicalManifestObject,
        LogicalRecordEnvelope, LogicalRecordLocator, ProjectedLogicalRecord,
    },
    prepare_clone_session,
    shared_session::{
        SharedPairingData, SharedSessionHost, SharedSessionLifecycle, SharedSessionPhase,
        SharedSourceLane, SharedSourceOwnership, SharedSourcePreparation,
    },
    CloneSource, LogicalDeltaObject, LogicalDeltaObjectSource, PeerSyncError, PinnedCloneRevision,
    PinnedSourceObject,
};
use crate::local_backup::CancellationProbe;
#[cfg(desktop)]
use crate::{
    asset_repository::PayloadCas, local_backup::NeverCancelled, persistent_store::PersistentStore,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    path::PathBuf,
    sync::Arc,
};

#[derive(Default)]
struct LifecycleFixture {
    events: Vec<&'static str>,
    fail_prepare: Option<SharedSourceLane>,
    fail_build: bool,
    cleanup_failures: Vec<SharedSourceLane>,
}

impl SharedSourceOwnership for LifecycleFixture {
    type Host = ();

    fn build_host(&mut self) -> Result<Self::Host, PeerSyncError> {
        self.events.push("build-host");
        if self.fail_build {
            return Err(PeerSyncError::Storage(
                "host construction failed after move".to_owned(),
            ));
        }
        Ok(())
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
        };
        lifecycle.prepare(&mut context).unwrap();
    }
    assert!(root
        .path()
        .join("peer-clone")
        .join("active-source.json")
        .exists());

    lifecycle.stop().unwrap();

    assert_eq!(lifecycle.phase().unwrap(), SharedSessionPhase::Idle);
    assert!(!root
        .path()
        .join("peer-clone")
        .join("active-source.json")
        .exists());
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
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        Ok(Box::new(Cursor::new(
            self.0.get(&object.hash).cloned().unwrap_or_default(),
        )))
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
    )
    .unwrap();
    let mut host = SharedSessionHost::new(clone, delta, bidi).unwrap();
    host.enable_v2_registry(root.path(), "test", DevicePermissions::read())
        .unwrap();
    (root, host)
}
fn claim(pairing: &SharedPairingData, id: &str) -> reqwest::blocking::Response {
    reqwest::blocking::Client::new().post(format!("{}/v1/sessions/{}/claim", pairing.endpoint, pairing.session_id)).json(&serde_json::json!({"claim": pairing.claim, "protocolVersion":2, "deviceId":id, "deviceName":"target"})).send().unwrap()
}

#[test]
fn canonical_registration_uri_has_exact_clone_fields_and_no_bearer() {
    let pairing = SharedPairingData {
        endpoint: "http://127.0.0.1:32145".to_owned(),
        session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        manifest_id: "a".repeat(64),
        claim: "b".repeat(64),
        expires_at_ms: 1_000,
    };

    let uri = pairing.canonical_uri();

    assert!(uri.starts_with("risuailocal://peer-clone/v2?"));
    assert!(uri.contains("endpoint=http%3A%2F%2F127.0.0.1%3A32145"));
    assert!(uri.contains("session=00000000-0000-4000-8000-000000000001"));
    assert!(uri.contains(&format!("manifest={}", "a".repeat(64))));
    assert!(uri.ends_with(&format!("#claim={}", "b".repeat(64))));
    assert!(!uri.contains("bearer"));
}

#[test]
fn one_listener_routes_all_lanes_and_hello_returns_exact_descriptors() {
    let (_root, mut host) = host();
    let pairing = host.start_fixed_loopback(32145).unwrap();
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
    for lane in [
        pairing.session_id.as_str(),
        "00000000-0000-4000-8000-000000000011",
        "00000000-0000-4000-8000-000000000012",
    ] {
        assert_eq!(
            reqwest::blocking::Client::new()
                .get(format!("{}/v1/sessions/{lane}/manifest", pairing.endpoint))
                .bearer_auth(&bearer)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
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
    let pairing = shared
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, 32146)
        .unwrap();
    let advertised = validate_lan_endpoint(&pairing.endpoint).unwrap();
    assert_eq!(advertised, pairing.endpoint);
    assert!(!advertised.contains("0.0.0.0"));
    let (_other_root, mut other) = host();
    assert!(other
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, 32146)
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
    let pairing = host.start_fixed_loopback(32147).unwrap();
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
        .start_fixed_lan(std::net::Ipv4Addr::LOCALHOST, 32149)
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
    let pairing = host.start_fixed_loopback(32150).unwrap();
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
    let pairing = host.start_fixed_loopback(32151).unwrap();
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
    let pairing = host.start_fixed_loopback(32148).unwrap();
    let bearer = claim(&pairing, "00000000-0000-4000-8000-000000000024")
        .json::<serde_json::Value>()
        .unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    host.stop().unwrap();
    let restarted = host.start_fixed_loopback(32148).unwrap();
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
