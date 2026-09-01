use super::lan::validate_lan_endpoint;
use super::{
    device_registry::DevicePermissions,
    lan::{
        LanBidirectionalControl, LanBidirectionalRegistrationRequest,
        LanBidirectionalRemoteApplyReceipt, LanBidirectionalRemoteApplyRequest,
        LanBidirectionalSession, PreparedBidirectionalLogicalLanSession, PreparedLogicalLanSession,
    },
    logical_delta::{
        build_logical_manifest, LogicalManifestBuilderInput, LogicalRecordEnvelope,
        LogicalRecordLocator, ProjectedLogicalRecord,
    },
    prepare_clone_session,
    shared_session::{SharedPairingData, SharedSessionHost},
    CloneSource, LogicalDeltaObject, LogicalDeltaObjectSource, PeerSyncError, PinnedCloneRevision,
    PinnedSourceObject,
};
use crate::local_backup::CancellationProbe;
use std::{
    io::{Cursor, Read},
    path::PathBuf,
    sync::Arc,
};

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
struct Empty;
impl LogicalDeltaObjectSource for Empty {
    fn open_object(&mut self, _: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        Ok(Box::new(Cursor::new(Vec::new())))
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
fn logical() -> (String, Vec<u8>, Vec<LogicalDeltaObject>) {
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
            vec![],
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
    )
}
fn host() -> (tempfile::TempDir, SharedSessionHost) {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("db");
    std::fs::write(&db, b"{}").unwrap();
    let clone = prepare_clone_session(&CloneFixture(db), root.path().join("clone")).unwrap();
    let source = "00000000-0000-4000-8000-000000000010";
    let (hash, bytes, objects) = logical();
    let delta = PreparedLogicalLanSession::new(
        "00000000-0000-4000-8000-000000000011",
        source,
        hash,
        bytes,
        objects,
        Box::new(Empty),
    )
    .unwrap();
    let (hash, bytes, objects) = logical();
    let bidi = PreparedBidirectionalLogicalLanSession::new(
        "00000000-0000-4000-8000-000000000012",
        source,
        hash,
        bytes,
        objects,
        Box::new(Empty),
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
    let (_root, mut host) = host();
    assert!(host.start_fixed_loopback(0).is_err());
    let pairing = host.start_fixed_loopback(32146).unwrap();
    assert!(host.start_fixed_loopback(32146).is_err());
    let established = claim(&pairing, "00000000-0000-4000-8000-000000000021");
    let bearer = established.json::<serde_json::Value>().unwrap()["bearer"]
        .as_str()
        .unwrap()
        .to_owned();
    let old = pairing.claim.clone();
    let replacement = host.rotate_link().unwrap();
    assert_eq!(pairing.endpoint, replacement.endpoint);
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
    host.stop().unwrap();
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
