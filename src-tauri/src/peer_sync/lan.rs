use super::{
    device_registry::{
        accept_outgoing_completion_offer, issue_outgoing_measured_completion_offer,
        load_or_create_device_id, outgoing_bidirectional_completion_lease_allows_remote_apply,
        outgoing_completion_lease_ready_bytes, outgoing_completion_offer_active,
        outgoing_device_is_registered, record_outgoing_seen, register_outgoing_claim,
        revoke_outgoing_device, seal_outgoing_completion_lease, CompletionAcceptance,
        CompletionLane, CompletionLeaseId, CompletionSealStatus, DevicePermissions, IncomingSource,
        OutgoingDevice, OutgoingDeviceRegistry,
    },
    http_stream::HttpRangeStream,
    logical_completion::{
        begin_outgoing_logical_object_issue, freeze_outgoing_logical_completion_proof,
        invalidate_outgoing_logical_completion_state, issue_outgoing_logical_completion_lease,
        record_outgoing_logical_progress, seal_outgoing_bidirectional_logical_completion,
        seal_outgoing_delta_logical_completion, LogicalCompletionManifest,
        OutgoingLogicalIssuedObjects,
    },
    protocol::{sha256_hex, CLONE_CHUNK_SIZE, MAX_MANIFEST_BYTES},
    registry_commands::{
        ensure_no_active_registered_source_work, lock_registered_source_lifecycle,
        register_incoming_source_if_compatible_locked,
    },
    PeerSyncError,
};
#[cfg(any(desktop, target_os = "android"))]
use super::{LogicalDeltaObject, LogicalDeltaObjectSource, PreparedCloneSession};
#[cfg(any(desktop, target_os = "android"))]
use crate::{
    asset_repository::PayloadCas,
    local_backup::{AtomicCancellation, CancellationProbe},
};
use serde::{Deserialize, Serialize};
#[cfg(any(desktop, target_os = "android"))]
use sha2::{Digest, Sha256};
#[cfg(any(desktop, target_os = "android"))]
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::{self, Seek, SeekFrom},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    thread::{self, JoinHandle},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(any(desktop, target_os = "android"))]
const CLAIM_TTL: Duration = Duration::from_secs(10 * 60);

// The LAN host mutexes guard plain data (claim state, tunnel-probe state, the
// device map, and the active connection handle) with no cross-field
// invariants, so a poisoned lock is recovered instead of propagated. stop()
// also runs from Drop, where a poisoning panic would abort the process while
// unwinding.
#[cfg(any(desktop, target_os = "android"))]
fn recovered_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
/// Stable code the interface maps to its own wording; never shown as native
/// text. The claim paths that produce it land with the peer version check.
#[cfg(any(desktop, target_os = "android"))]
pub(crate) const PEER_OUTDATED: &str = "peer-outdated";
/// The only claim protocol the source serves and every client sends.
const CLAIM_PROTOCOL_VERSION: u8 = 2;
#[cfg(desktop)]
pub(crate) const NAMED_TUNNEL_ORIGIN_PORT: u16 = 32145;
#[cfg(desktop)]
pub(crate) const NAMED_TUNNEL_ORIGIN_UNAVAILABLE: &str =
    "Named Tunnel cannot bind loopback port 32145. Stop the app using that port, or use Quick Tunnel / Trusted LAN.";
#[cfg(all(test, desktop))]
pub(crate) static NAMED_TUNNEL_TEST_LOCK: Mutex<()> = Mutex::new(());
// Tests override the Named Tunnel origin port with an ephemeral one so a full
// suite run never contends on the real fixed port (a machine-global resource
// any other process may hold).
#[cfg(all(test, desktop))]
static NAMED_TUNNEL_ORIGIN_PORT_OVERRIDE: std::sync::atomic::AtomicU16 =
    std::sync::atomic::AtomicU16::new(0);

#[cfg(desktop)]
pub(crate) fn named_tunnel_origin_port() -> u16 {
    #[cfg(test)]
    {
        let overridden = NAMED_TUNNEL_ORIGIN_PORT_OVERRIDE.load(Ordering::SeqCst);
        if overridden != 0 {
            return overridden;
        }
    }
    NAMED_TUNNEL_ORIGIN_PORT
}

#[cfg(all(test, desktop))]
pub(crate) struct NamedTunnelPortOverrideGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(all(test, desktop))]
impl Drop for NamedTunnelPortOverrideGuard {
    fn drop(&mut self) {
        NAMED_TUNNEL_ORIGIN_PORT_OVERRIDE.store(0, Ordering::SeqCst);
    }
}

#[cfg(all(test, desktop))]
pub(crate) fn override_named_tunnel_origin_port_for_test(
    port: u16,
) -> NamedTunnelPortOverrideGuard {
    let lock = NAMED_TUNNEL_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    NAMED_TUNNEL_ORIGIN_PORT_OVERRIDE.store(port, Ordering::SeqCst);
    NamedTunnelPortOverrideGuard { _lock: lock }
}
const MAX_URL_BYTES: usize = 512;
#[cfg(any(desktop, target_os = "android"))]
const MAX_HEADER_BYTES: usize = 8 * 1024;
#[cfg(any(desktop, target_os = "android"))]
const MAX_BODY_BYTES: usize = 1024;
const MAX_CLAIM_RESPONSE_BYTES: usize = 1024;
#[cfg(any(desktop, target_os = "android"))]
const MAX_REQUEST_LINE_BYTES: usize = MAX_URL_BYTES + 32;
#[cfg(any(desktop, target_os = "android"))]
const MAX_REQUEST_HEAD_BYTES: usize = MAX_REQUEST_LINE_BYTES + 2 + MAX_HEADER_BYTES + 4;
#[cfg(any(desktop, target_os = "android"))]
const CONNECTION_READ_POLL_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(any(desktop, target_os = "android"))]
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(any(desktop, target_os = "android"))]
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(any(desktop, target_os = "android"))]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(any(desktop, target_os = "android"))]
const RESPONSE_COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PERSISTED_CREDENTIAL_BYTES: u64 = 4096;
pub(crate) const PERSISTED_CREDENTIAL_SCHEMA: &str = "risunest.peer-clone-credential/v1";
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const PEER_COMPLETION_SCHEMA: &str = "risunest.peer-completion/v1";
const PEER_COMPLETION_PREPARED_SCHEMA: &str = "risunest.peer-completion-prepared/v1";
const MAX_COMPLETION_PREPARED_RESPONSE_BYTES: usize = 256;
pub(crate) const PEER_COMPLETION_CAPABILITY_HEADER: &str = "RisuNest-Peer-Completion";
pub(crate) const PEER_COMPLETION_CAPABILITY_V1: &str = "v1";
pub(crate) const PEER_COMPLETION_LEASE_HEADER: &str = "RisuNest-Peer-Completion-Lease";
pub(crate) const PEER_COMPLETION_RESUME_HEADER: &str = "RisuNest-Peer-Completion-Resume";
#[cfg(any(desktop, target_os = "android"))]
const LOGICAL_OBJECT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(any(desktop, target_os = "android"))]
pub struct LanPairing {
    pub session_id: String,
    pub manifest_id: String,
    pub claim: String,
    pub(crate) permissions: Option<Vec<String>>,
}

pub struct LanCloneClient {
    client: reqwest::blocking::Client,
    ranges: HttpRangeStream,
    #[cfg_attr(not(test), allow(dead_code))]
    control_timeout: Duration,
    endpoint: String,
    session_id: String,
    session_url: String,
    pub device_id: String,
    bearer: String,
    source_device_id: String,
    manifest_id: Option<String>,
}

pub(crate) struct LanCompletionManifestResponse {
    pub(crate) bytes: Vec<u8>,
    pub(crate) completion_lease_id: Option<CompletionLeaseId>,
}

pub(super) fn parse_completion_lease_headers(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<CompletionLeaseId>, PeerSyncError> {
    let completion_versions = headers
        .get_all(PEER_COMPLETION_CAPABILITY_HEADER)
        .iter()
        .collect::<Vec<_>>();
    let completion_leases = headers
        .get_all(PEER_COMPLETION_LEASE_HEADER)
        .iter()
        .collect::<Vec<_>>();
    match (completion_versions.as_slice(), completion_leases.as_slice()) {
        ([], []) => Ok(None),
        ([version], [lease]) if version.as_bytes() == PEER_COMPLETION_CAPABILITY_V1.as_bytes() => {
            let lease = lease.to_str().map_err(|_| {
                PeerSyncError::Protocol("invalid peer completion lease header".to_owned())
            })?;
            CompletionLeaseId::parse(lease).map(Some)
        }
        _ => Err(PeerSyncError::Protocol(
            "invalid peer completion lease headers".to_owned(),
        )),
    }
}

impl LanCloneClient {
    pub(crate) fn claim_v2_and_persist_and_register(
        app_root: &Path,
        target_name: &str,
        credential_path: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        validate_object_hash(manifest_id)?;
        validate_device_name(target_name)?;
        let target_device_id = load_or_create_device_id(app_root)?;
        // One lifecycle hold covers the refusal, the claim round trip that makes the
        // source replace its authorization, and this device's own registration, so a
        // lane cannot start in the middle and lose the authorization it is using.
        let lifecycle = lock_registered_source_lifecycle()?;
        ensure_no_active_registered_source_work(&lifecycle, app_root)?;
        let mut client = Self::claim_v2_with_device(
            endpoint,
            session_id,
            claim,
            &target_device_id,
            target_name,
        )?;
        client.manifest_id = Some(manifest_id.to_owned());
        let registration = client.incoming_source()?;
        let previous_credential = snapshot_credential(credential_path)?;
        client.persist(credential_path)?;
        if let Err(error) =
            register_incoming_source_if_compatible_locked(&lifecycle, app_root, registration)
        {
            restore_credential(credential_path, previous_credential.as_deref()).map_err(
                |rollback| {
                    PeerSyncError::Storage(format!(
                        "registered clone credential rollback failed: {rollback}"
                    ))
                },
            )?;
            return Err(error);
        }
        Ok(client)
    }

    pub(crate) fn from_registered_and_persist(
        credential_path: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
    ) -> Result<Self, PeerSyncError> {
        let endpoint = validate_lan_endpoint(endpoint)?;
        if !is_canonical_uuid(session_id)
            || !is_lower_hex_256(manifest_id)
            || !is_canonical_uuid(target_device_id)
            || !is_canonical_uuid(source_device_id)
            || !is_lower_hex_256(bearer)
        {
            return Err(PeerSyncError::Protocol(
                "invalid registered clone credential".to_owned(),
            ));
        }
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "LAN session URL is too long".to_owned(),
            ));
        }
        let client = Self {
            client: build_clone_http_client(CONTROL_REQUEST_TIMEOUT)?,
            ranges: HttpRangeStream::new(Duration::from_secs(3))?,
            control_timeout: CONTROL_REQUEST_TIMEOUT,
            endpoint,
            session_id: session_id.to_owned(),
            session_url,
            device_id: target_device_id.to_owned(),
            bearer: bearer.to_owned(),
            source_device_id: source_device_id.to_owned(),
            manifest_id: Some(manifest_id.to_owned()),
        };
        client.persist(credential_path)?;
        Ok(client)
    }

    pub fn open_persisted(credential_path: &Path) -> Result<Self, PeerSyncError> {
        let Some(bytes) = Self::read_persisted_credential_bytes(credential_path)? else {
            return Err(PeerSyncError::Storage(
                "persisted LAN clone credential is missing".to_owned(),
            ));
        };
        let persisted: PersistedLanCredential = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        persisted.validate()?;
        Self::from_persisted(persisted)
    }

    /// Reads a persisted clone credential within its size bound, rejecting
    /// anything that is not a plain file. `Ok(None)` means the credential does
    /// not exist; every other I/O failure is reported as an error.
    fn read_persisted_credential_bytes(
        credential_path: &Path,
    ) -> Result<Option<Vec<u8>>, PeerSyncError> {
        let metadata = match fs::symlink_metadata(credential_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_PERSISTED_CREDENTIAL_BYTES
        {
            return Err(PeerSyncError::Storage(
                "invalid persisted LAN clone credential file".to_owned(),
            ));
        }
        let file = match File::open(credential_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_PERSISTED_CREDENTIAL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PERSISTED_CREDENTIAL_BYTES {
            return Err(PeerSyncError::Storage(
                "persisted LAN clone credential is too large".to_owned(),
            ));
        }
        Ok(Some(bytes))
    }

    /// Reports how the persisted clone credential at `credential_path` is
    /// corrupt, or `None` when its content is intact. Only content decides the
    /// verdict: a credential that is gone counts as corrupt, while every other
    /// I/O failure propagates so a locked, permission-denied, or otherwise
    /// temporarily unreadable file is never mistaken for corruption. Unlike
    /// `open_persisted` the verdict does not depend on the HTTP clients this
    /// process can build right now.
    #[cfg(any(target_os = "android", test))]
    pub(crate) fn persisted_credential_corruption(
        credential_path: &Path,
    ) -> Result<Option<PersistedCredentialCorruption>, PeerSyncError> {
        let Some(bytes) = Self::read_persisted_credential_bytes(credential_path)? else {
            return Ok(Some(PersistedCredentialCorruption::Missing));
        };
        let Ok(persisted) = serde_json::from_slice::<PersistedLanCredential>(&bytes) else {
            return Ok(Some(PersistedCredentialCorruption::Unparsable));
        };
        if persisted.validate().is_err() {
            return Ok(Some(PersistedCredentialCorruption::Invalid));
        }
        Ok(None)
    }

    fn claim_v2_with_device(
        endpoint: &str,
        session_id: &str,
        claim: &str,
        device_id: &str,
        device_name: &str,
    ) -> Result<Self, PeerSyncError> {
        if endpoint.len() > MAX_URL_BYTES
            || !is_canonical_uuid(session_id)
            || !is_lower_hex_256(claim)
            || !is_canonical_uuid(device_id)
        {
            return Err(PeerSyncError::Protocol(
                "invalid LAN pairing data".to_owned(),
            ));
        }
        validate_device_name(device_name)?;
        let endpoint = validate_lan_endpoint(endpoint)?;
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "LAN session URL is too long".to_owned(),
            ));
        }
        let client = build_clone_http_client(CONTROL_REQUEST_TIMEOUT)?;
        let response = client
            .post(format!("{session_url}/claim"))
            .json(&ClaimRequest {
                claim: claim.to_owned(),
                device_id: Some(device_id.to_owned()),
                protocol_version: CLAIM_PROTOCOL_VERSION,
                device_name: Some(device_name.to_owned()),
                permissions: None,
            })
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .send()
            .map_err(transport)?;
        // A source that predates the registered claim refuses the request outright.
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        let response = read_claim_response(response, "LAN")?;
        let (Some(source_device_id), Some(permissions)) =
            (response.source_device_id, response.permissions)
        else {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        };
        let source_name = response.source_device_name.ok_or_else(|| {
            PeerSyncError::Protocol("v2 source device name is missing".to_owned())
        })?;
        // A permission this build cannot name reads the same as a response that
        // omits the registered identity: the peer grants something this claim
        // does not understand, so it is reported as outdated rather than failed.
        if DevicePermissions::from_values(permissions).is_err() {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        if response.device_id != device_id
            || !is_canonical_uuid(&source_device_id)
            || validate_device_name(&source_name).is_err()
            || !is_lower_hex_256(&response.bearer)
            || response.permission != "clone-read"
        {
            return Err(PeerSyncError::Protocol(
                "invalid LAN v2 claim response".to_owned(),
            ));
        }
        Ok(Self {
            client,
            ranges: HttpRangeStream::new(Duration::from_secs(3))?,
            control_timeout: CONTROL_REQUEST_TIMEOUT,
            endpoint,
            session_id: session_id.to_owned(),
            session_url,
            device_id: response.device_id,
            bearer: response.bearer,
            source_device_id,
            manifest_id: None,
        })
    }

    fn incoming_source(&self) -> Result<IncomingSource, PeerSyncError> {
        let hello = self.hello()?;
        self.incoming_source_from_hello(hello)
    }

    fn incoming_source_from_hello(
        &self,
        hello: PeerHello,
    ) -> Result<IncomingSource, PeerSyncError> {
        if self.source_device_id != hello.device_id {
            return Err(PeerSyncError::Protocol(
                "v2 claim source device identity differs from authenticated hello".to_owned(),
            ));
        }
        Ok(IncomingSource {
            device_id: hello.device_id,
            name: hello.name,
            endpoint: self.endpoint.clone(),
            bearer: self.bearer.clone(),
            permissions: hello.permissions,
            last_seen_ms: now_ms() as u64,
            total_bytes: 0,
        })
    }

    pub(crate) fn hello(&self) -> Result<PeerHello, PeerSyncError> {
        authenticated_peer_hello(&self.endpoint, &self.bearer)
    }

    pub(crate) fn hello_with_capabilities(
        &self,
    ) -> Result<PeerHelloWithCapabilities, PeerSyncError> {
        authenticated_peer_hello_with_capabilities(&self.endpoint, &self.bearer)
    }

    pub(crate) fn deliver_completion(
        &self,
        capability: PeerCompletionCapability,
        operation_id: &str,
        transferred_bytes: u64,
    ) -> Result<PeerCompletionDelivery, PeerSyncError> {
        let manifest_id = self.manifest_id.as_deref().ok_or_else(|| {
            PeerSyncError::Protocol("LAN clone manifest identity is missing".to_owned())
        })?;
        deliver_peer_completion(
            &self.endpoint,
            &self.bearer,
            capability,
            CompletionLane::Clone,
            operation_id,
            manifest_id,
            transferred_bytes,
        )
    }

    pub(super) fn into_resumable_parts(
        self,
    ) -> Result<
        (
            reqwest::blocking::Client,
            HttpRangeStream,
            reqwest::Url,
            String,
            Option<String>,
        ),
        PeerSyncError,
    > {
        let session_url = reqwest::Url::parse(&self.session_url)
            .map_err(|_| PeerSyncError::Protocol("invalid persisted LAN session URL".to_owned()))?;
        Ok((
            self.client,
            self.ranges,
            session_url,
            self.bearer,
            self.manifest_id,
        ))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn session_url(&self) -> &str {
        &self.session_url
    }

    // Android clone jobs read the pinned target identity.
    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    pub(crate) fn target_identity(&self) -> Result<(&str, &str, &str), PeerSyncError> {
        let manifest_id = self.manifest_id.as_deref().ok_or_else(|| {
            PeerSyncError::Protocol("LAN clone manifest identity is missing".to_owned())
        })?;
        Ok((&self.endpoint, &self.session_id, manifest_id))
    }

    pub(crate) fn registered_source_device_id(&self) -> &str {
        &self.source_device_id
    }

    pub(crate) fn matches_registered_credential(
        &self,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
    ) -> Result<bool, PeerSyncError> {
        Ok(self.endpoint == validate_lan_endpoint(endpoint)?
            && self.session_id == session_id
            && self.manifest_id.as_deref() == Some(manifest_id)
            && self.device_id == target_device_id
            && self.source_device_id == source_device_id
            && constant_time_eq(&digest(self.bearer.as_bytes()), &digest(bearer.as_bytes())))
    }

    // Desktop resume validation compares persisted targets.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub(crate) fn matches_target(
        &self,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
    ) -> Result<bool, PeerSyncError> {
        Ok(self.endpoint == validate_lan_endpoint(endpoint)?
            && self.session_id == session_id
            && self.manifest_id.as_deref() == Some(manifest_id))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fetch_manifest(&self, expected_manifest_id: &str) -> Result<Vec<u8>, PeerSyncError> {
        Ok(self
            .fetch_manifest_request(expected_manifest_id, false, None)?
            .bytes)
    }

    fn fetch_manifest_request(
        &self,
        expected_manifest_id: &str,
        completion_v1: bool,
        resume_lease_id: Option<&CompletionLeaseId>,
    ) -> Result<LanCompletionManifestResponse, PeerSyncError> {
        if !is_lower_hex_256(expected_manifest_id) {
            return Err(PeerSyncError::Protocol(
                "invalid LAN manifest identity".to_owned(),
            ));
        }
        let mut request = self.client.get(format!("{}/manifest", self.session_url));
        if completion_v1 {
            request = request.header(
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
            );
            if let Some(resume_lease_id) = resume_lease_id {
                request = request.header(PEER_COMPLETION_RESUME_HEADER, resume_lease_id.as_str());
            }
        }
        let response = self.control_request(request).send().map_err(transport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        if response.content_length().unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES as u64 {
            return Err(PeerSyncError::Protocol(
                "LAN manifest is too large".to_owned(),
            ));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| PeerSyncError::Protocol("LAN manifest ETag is missing".to_owned()))?;
        let completion_lease_id = if completion_v1 {
            parse_completion_lease_headers(response.headers())?
        } else {
            None
        };
        let mut bytes = Vec::new();
        response
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(transport)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PeerSyncError::Protocol(
                "LAN manifest is too large".to_owned(),
            ));
        }
        let received = sha256_hex(&bytes);
        if received != expected_manifest_id || etag != quoted(expected_manifest_id) {
            return Err(PeerSyncError::StaleManifest {
                expected: expected_manifest_id.to_owned(),
                received,
            });
        }
        Ok(LanCompletionManifestResponse {
            bytes,
            completion_lease_id,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn head_object(&self, object: &str) -> Result<u64, PeerSyncError> {
        validate_object_hash(object)?;
        let response = self
            .control_request(
                self.client
                    .head(format!("{}/objects/{object}", self.session_url)),
            )
            .send()
            .map_err(transport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        if response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            != Some(quoted(object).as_str())
        {
            return Err(PeerSyncError::Protocol(
                "LAN object ETag does not match its identity".to_owned(),
            ));
        }
        if response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            != Some("bytes")
        {
            return Err(PeerSyncError::Protocol(
                "LAN object does not advertise byte ranges".to_owned(),
            ));
        }
        response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                PeerSyncError::Protocol("LAN object size header is missing or invalid".to_owned())
            })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn fetch_chunk(
        &self,
        object: &str,
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>, PeerSyncError> {
        validate_object_hash(object)?;
        let expected_size = end
            .checked_sub(start)
            .and_then(|size| size.checked_add(1))
            .filter(|size| *size <= CLONE_CHUNK_SIZE)
            .ok_or_else(|| PeerSyncError::Protocol("invalid LAN range".to_owned()))?;
        let total = self.head_object(object)?;
        let url = reqwest::Url::parse(&format!("{}/objects/{object}", self.session_url))
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        let mut bytes = Vec::with_capacity(expected_size as usize);
        self.ranges.read(
            url,
            Some(&self.bearer),
            object,
            start,
            end,
            Some(total),
            &|| false,
            &mut |chunk| {
                bytes.extend_from_slice(chunk);
                Ok(())
            },
        )?;
        Ok(bytes)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn report_progress(
        &self,
        verified_bytes: u64,
        current_object: Option<&str>,
    ) -> Result<(), PeerSyncError> {
        if let Some(object) = current_object {
            validate_object_hash(object)?
        }
        self.report_progress_for_operation(verified_bytes, current_object, None)
    }

    pub(crate) fn report_progress_for_operation(
        &self,
        verified_bytes: u64,
        current_object: Option<&str>,
        operation_id: Option<&str>,
    ) -> Result<(), PeerSyncError> {
        if let Some(operation_id) = operation_id {
            if !is_canonical_v4_uuid(operation_id) {
                return Err(PeerSyncError::Protocol(
                    "invalid clone completion operation identifier".to_owned(),
                ));
            }
        }
        if let Some(object) = current_object {
            validate_object_hash(object)?
        }
        let response = self
            .control_request(
                self.client
                    .post(format!("{}/progress", self.session_url))
                    .json(&ProgressRequest {
                        verified_bytes,
                        current_object: current_object.map(str::to_owned),
                        operation_id: operation_id.map(str::to_owned),
                        manifest_id: None,
                        verified_object: None,
                    }),
            )
            .send()
            .map_err(transport)?;
        if response.status().as_u16() != 204 {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }

    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn control_request(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request
            .bearer_auth(&self.bearer)
            .timeout(self.control_timeout)
    }

    fn persist(&self, credential_path: &Path) -> Result<(), PeerSyncError> {
        let persisted = PersistedLanCredential {
            schema: PERSISTED_CREDENTIAL_SCHEMA.to_owned(),
            endpoint: self.endpoint.clone(),
            session_id: self.session_id.clone(),
            manifest_id: self.manifest_id.clone().ok_or_else(|| {
                PeerSyncError::Protocol("LAN clone manifest identity is missing".to_owned())
            })?,
            device_id: self.device_id.clone(),
            bearer: self.bearer.clone(),
            source_device_id: self.source_device_id.clone(),
            permission: "clone-read".to_owned(),
        };
        persisted.validate()?;
        let bytes = serde_json::to_vec(&persisted)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        if bytes.len() as u64 > MAX_PERSISTED_CREDENTIAL_BYTES {
            return Err(PeerSyncError::Storage(
                "persisted LAN clone credential is too large".to_owned(),
            ));
        }
        write_credential_bytes_atomic(credential_path, &bytes)
    }

    fn from_persisted(persisted: PersistedLanCredential) -> Result<Self, PeerSyncError> {
        let endpoint = validate_lan_endpoint(&persisted.endpoint)?;
        let session_url = format!("{endpoint}/v1/sessions/{}", persisted.session_id);
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "LAN session URL is too long".to_owned(),
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(transport)?;
        let ranges = HttpRangeStream::new(Duration::from_secs(3))?;
        Ok(Self {
            client,
            ranges,
            control_timeout: CONTROL_REQUEST_TIMEOUT,
            endpoint,
            session_id: persisted.session_id,
            session_url,
            device_id: persisted.device_id,
            bearer: persisted.bearer,
            source_device_id: persisted.source_device_id,
            manifest_id: Some(persisted.manifest_id),
        })
    }
}

/// Why a persisted clone credential is unusable on its content alone.
#[cfg(any(target_os = "android", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PersistedCredentialCorruption {
    /// The credential file is gone.
    Missing,
    /// The bytes are not a persisted clone credential.
    Unparsable,
    /// The credential parses but fails its own validation.
    Invalid,
}

#[cfg(any(target_os = "android", test))]
impl PersistedCredentialCorruption {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unparsable => "unparsable",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedLanCredential {
    schema: String,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    device_id: String,
    bearer: String,
    source_device_id: String,
    permission: String,
}

impl PersistedLanCredential {
    fn validate(&self) -> Result<(), PeerSyncError> {
        if self.schema != PERSISTED_CREDENTIAL_SCHEMA
            || !is_canonical_uuid(&self.session_id)
            || !is_canonical_uuid(&self.device_id)
            || !is_lower_hex_256(&self.manifest_id)
            || !is_lower_hex_256(&self.bearer)
            || !is_canonical_uuid(&self.source_device_id)
            || self.permission != "clone-read"
        {
            return Err(PeerSyncError::Protocol(
                "invalid persisted LAN clone credential".to_owned(),
            ));
        }
        validate_lan_endpoint(&self.endpoint)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
#[cfg(any(desktop, target_os = "android"))]
pub struct LanDevice {
    pub device_id: String,
    pub verified_bytes: u64,
    pub current_object: Option<String>,
    pub last_seen_unix_ms: u128,
    pub revoked: bool,
}

#[cfg(any(desktop, target_os = "android"))]
struct DeviceState {
    info: LanDevice,
    bearer_digest: [u8; 32],
    permissions: DevicePermissions,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone)]
struct HostV2Registration {
    app_root: PathBuf,
    device_id: String,
    name: String,
    permissions: DevicePermissions,
}

#[cfg(any(desktop, target_os = "android"))]
impl HostV2Registration {
    fn new(
        app_root: &Path,
        name: &str,
        permissions: DevicePermissions,
    ) -> Result<Self, PeerSyncError> {
        validate_device_name(name)?;
        Ok(Self {
            app_root: app_root.to_path_buf(),
            device_id: load_or_create_device_id(app_root)?,
            name: name.to_owned(),
            permissions: permissions.clone(),
        })
    }
}

#[cfg(any(desktop, target_os = "android"))]
struct ClaimState {
    digest: [u8; 32],
    expires_at: Instant,
    expires_at_ms: u128,
    consumed: bool,
    v2_permissions: Option<DevicePermissions>,
}

#[cfg(desktop)]
struct TunnelProbeState {
    digest: [u8; 32],
    expected_body: [u8; 32],
}

#[cfg(desktop)]
pub(crate) struct TunnelOriginProbe {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) path_prefix: String,
    pub(crate) path: String,
    pub(crate) expected_body: [u8; 32],
}

#[cfg(any(desktop, target_os = "android"))]
struct LanShared {
    sessions: Vec<LanSession>,
    claim: Mutex<Option<ClaimState>>,
    #[cfg(desktop)]
    tunnel_probe: Mutex<Option<TunnelProbeState>>,
    devices: Mutex<BTreeMap<String, DeviceState>>,
    v2_registration: Mutex<Option<HostV2Registration>>,
    issued_logical_objects: OutgoingLogicalIssuedObjects,
    #[cfg(test)]
    after_v2_registry_claim: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[cfg(any(desktop, target_os = "android"))]
pub struct PreparedLogicalLanSession {
    session_id: String,
    source_device_id: String,
    manifest_id: String,
    manifest_bytes: Arc<[u8]>,
    objects: LogicalCompletionManifest,
    source: Mutex<Box<dyn LogicalDeltaObjectSource + Send>>,
}

#[cfg(any(desktop, target_os = "android"))]
pub(crate) struct PreparedBidirectionalLogicalLanSession {
    logical: PreparedLogicalLanSession,
    control: Arc<dyn LanBidirectionalControl>,
}

#[cfg(any(desktop, target_os = "android"))]
impl PreparedBidirectionalLogicalLanSession {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: &str,
        source_device_id: &str,
        manifest_id: String,
        manifest_bytes: impl Into<Arc<[u8]>>,
        objects: Vec<LogicalDeltaObject>,
        source: Box<dyn LogicalDeltaObjectSource + Send>,
        control: Arc<dyn LanBidirectionalControl>,
    ) -> Result<Self, PeerSyncError> {
        Ok(Self {
            logical: PreparedLogicalLanSession::new(
                session_id,
                source_device_id,
                manifest_id,
                manifest_bytes,
                objects,
                source,
            )?,
            control,
        })
    }
}

#[cfg(any(desktop, target_os = "android"))]
impl PreparedLogicalLanSession {
    pub fn new(
        session_id: &str,
        source_device_id: &str,
        manifest_id: String,
        manifest_bytes: impl Into<Arc<[u8]>>,
        objects: Vec<LogicalDeltaObject>,
        source: Box<dyn LogicalDeltaObjectSource + Send>,
    ) -> Result<Self, PeerSyncError> {
        if !is_canonical_uuid(session_id)
            || !is_canonical_uuid(source_device_id)
            || !is_lower_hex_256(&manifest_id)
        {
            return Err(PeerSyncError::Validation(
                "logical LAN session identity is invalid".to_owned(),
            ));
        }
        let manifest_bytes = manifest_bytes.into();
        if manifest_bytes.len() > MAX_MANIFEST_BYTES || sha256_hex(&manifest_bytes) != manifest_id {
            return Err(PeerSyncError::Validation(
                "logical LAN manifest identity is invalid".to_owned(),
            ));
        }
        let mut object_sizes = BTreeMap::new();
        for object in objects {
            validate_object_hash(&object.hash)?;
            if object_sizes.insert(object.hash, object.size).is_some() {
                return Err(PeerSyncError::Validation(
                    "logical LAN manifest contains duplicate objects".to_owned(),
                ));
            }
        }
        let manifest = super::logical_delta::decode_logical_manifest(&manifest_bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        let manifest_objects = manifest
            .objects
            .into_iter()
            .map(|object| (object.hash, object.size))
            .collect::<BTreeMap<_, _>>();
        if object_sizes != manifest_objects {
            return Err(PeerSyncError::Validation(
                "logical LAN objects differ from the canonical manifest".to_owned(),
            ));
        }
        Ok(Self {
            session_id: session_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            manifest_id,
            manifest_bytes,
            objects: LogicalCompletionManifest::new(object_sizes)?,
            source: Mutex::new(source),
        })
    }
}

#[cfg(any(desktop, target_os = "android"))]
enum LanSession {
    Clone(PreparedCloneSession),
    Logical(PreparedLogicalLanSession),
    BidirectionalLogical(PreparedBidirectionalLogicalLanSession),
}

#[cfg(any(desktop, target_os = "android"))]
impl LanSession {
    fn session_id(&self) -> &str {
        match self {
            Self::Clone(session) => &session.manifest().session_id,
            Self::Logical(session) => &session.session_id,
            Self::BidirectionalLogical(session) => &session.logical.session_id,
        }
    }

    fn manifest_id(&self) -> &str {
        match self {
            Self::Clone(session) => session.manifest_id(),
            Self::Logical(session) => &session.manifest_id,
            Self::BidirectionalLogical(session) => &session.logical.manifest_id,
        }
    }

    fn permission(&self) -> &'static str {
        match self {
            Self::Clone(_) => "clone-read",
            Self::Logical(_) => "logical-read",
            Self::BidirectionalLogical(_) => "logical-bidirectional",
        }
    }

    /// The grant a session hands out when its pairing link did not pin one, which
    /// is the case only for the sessions a lane opens for a single operation.
    fn lane_permissions(&self) -> DevicePermissions {
        match self {
            Self::BidirectionalLogical(_) => DevicePermissions::read_and_bidirectional(),
            Self::Clone(_) | Self::Logical(_) => DevicePermissions::read(),
        }
    }

    fn source_device_id(&self) -> Option<&str> {
        match self {
            Self::Clone(_) => None,
            Self::Logical(session) => Some(&session.source_device_id),
            Self::BidirectionalLogical(session) => Some(&session.logical.source_device_id),
        }
    }

    fn object_size(&self, hash: &str) -> Option<u64> {
        match self {
            Self::Clone(session) => session.manifest().objects.get(hash).map(|value| value.size),
            Self::Logical(session) => session.objects.object_size(hash),
            Self::BidirectionalLogical(session) => session.logical.objects.object_size(hash),
        }
    }

    fn logical_completion(&self) -> Option<(CompletionLane, &LogicalCompletionManifest)> {
        match self {
            Self::Clone(_) => None,
            Self::Logical(session) => Some((CompletionLane::Delta, &session.objects)),
            Self::BidirectionalLogical(session) => {
                Some((CompletionLane::Bidirectional, &session.logical.objects))
            }
        }
    }

    fn bidirectional_control(&self) -> Option<&Arc<dyn LanBidirectionalControl>> {
        match self {
            Self::BidirectionalLogical(session) => Some(&session.control),
            Self::Clone(_) | Self::Logical(_) => None,
        }
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn session_by_id<'a>(shared: &'a LanShared, session_id: &str) -> Option<&'a LanSession> {
    shared
        .sessions
        .iter()
        .find(|session| session.session_id() == session_id)
}

#[cfg(any(desktop, target_os = "android"))]
fn primary_session(shared: &LanShared) -> &LanSession {
    // Every host is prepared with at least one session, and a shared host lists the
    // clone session first, so the pairing link always names this session.
    &shared.sessions[0]
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone)]
pub(crate) struct LanCloneHostControl {
    shared: Weak<LanShared>,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanCloneHostControl {
    pub(crate) fn devices(&self) -> Vec<LanDevice> {
        let Some(shared) = self.shared.upgrade() else {
            return Vec::new();
        };
        let devices = recovered_lock(&shared.devices)
            .values()
            .map(|device| device.info.clone())
            .collect();
        devices
    }

    pub(crate) fn revoke(&self, device_id: &str) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        if let Some(registration) = recovered_lock(&shared.v2_registration).clone() {
            let removed = recovered_lock(&shared.devices).remove(device_id).is_some();
            if let Err(error) = revoke_outgoing_device(&registration.app_root, device_id, |_| {}) {
                crate::nlog!(
                    "warn",
                    "redundant live peer registry revoke failed: {error}"
                );
            }
            if let Err(error) = invalidate_outgoing_logical_completion_state(
                &registration.app_root,
                device_id,
                &shared.issued_logical_objects,
            ) {
                crate::nlog!("warn", "logical completion proof revoke failed: {error}");
            }
            return removed;
        }
        let mut devices = recovered_lock(&shared.devices);
        let Some(device) = devices.get_mut(device_id) else {
            return false;
        };
        device.info.revoked = true;
        true
    }

    #[cfg(test)]
    pub(crate) fn is_attached_for_test(&self) -> bool {
        self.shared.strong_count() != 0
    }
}

#[cfg(any(desktop, target_os = "android"))]
pub struct LanCloneHost {
    shared: Arc<LanShared>,
    address: Option<SocketAddr>,
    stopped: Option<Arc<AtomicBool>>,
    active_connection: Option<Arc<Mutex<Option<TcpStream>>>>,
    thread: Option<JoinHandle<Result<(), PeerSyncError>>>,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanCloneHost {
    pub fn prepare(session: PreparedCloneSession) -> Self {
        Self {
            shared: Arc::new(LanShared {
                sessions: vec![LanSession::Clone(session)],
                claim: Mutex::new(None),
                #[cfg(desktop)]
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
                v2_registration: Mutex::new(None),
                issued_logical_objects: OutgoingLogicalIssuedObjects::default(),
                #[cfg(test)]
                after_v2_registry_claim: Mutex::new(None),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        }
    }

    pub fn prepare_logical(session: PreparedLogicalLanSession) -> Self {
        Self {
            shared: Arc::new(LanShared {
                sessions: vec![LanSession::Logical(session)],
                claim: Mutex::new(None),
                #[cfg(desktop)]
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
                v2_registration: Mutex::new(None),
                issued_logical_objects: OutgoingLogicalIssuedObjects::default(),
                #[cfg(test)]
                after_v2_registry_claim: Mutex::new(None),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        }
    }

    pub(crate) fn prepare_bidirectional_logical(
        session: PreparedBidirectionalLogicalLanSession,
    ) -> Self {
        Self {
            shared: Arc::new(LanShared {
                sessions: vec![LanSession::BidirectionalLogical(session)],
                claim: Mutex::new(None),
                #[cfg(desktop)]
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
                v2_registration: Mutex::new(None),
                issued_logical_objects: OutgoingLogicalIssuedObjects::default(),
                #[cfg(test)]
                after_v2_registry_claim: Mutex::new(None),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        }
    }

    // One raw listener can serve the existing three session types.  Their
    // transfer engines remain owned by the prepared sessions themselves.
    pub(crate) fn prepare_shared(
        clone: PreparedCloneSession,
        delta: PreparedLogicalLanSession,
        bidirectional: PreparedBidirectionalLogicalLanSession,
    ) -> Result<Self, PeerSyncError> {
        let sessions = vec![
            LanSession::Clone(clone),
            LanSession::Logical(delta),
            LanSession::BidirectionalLogical(bidirectional),
        ];
        let mut ids = std::collections::BTreeSet::new();
        if sessions
            .iter()
            .any(|session| !ids.insert(session.session_id()))
        {
            return Err(PeerSyncError::Validation(
                "shared LAN sessions must have distinct IDs".to_owned(),
            ));
        }
        Ok(Self {
            shared: Arc::new(LanShared {
                sessions,
                claim: Mutex::new(None),
                #[cfg(desktop)]
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
                v2_registration: Mutex::new(None),
                issued_logical_objects: OutgoingLogicalIssuedObjects::default(),
                #[cfg(test)]
                after_v2_registry_claim: Mutex::new(None),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        })
    }

    // Decides what a claim on this host does: a registered clone or shared host
    // registers the caller as an outgoing device, while a host that only opens a
    // lane for a single operation issues a bearer for that lane and nothing else.
    pub(crate) fn enable_v2_registry(
        &mut self,
        app_root: &Path,
        source_name: &str,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        if self.thread.is_some() {
            return Err(PeerSyncError::Protocol(
                "LAN clone host is already running".to_owned(),
            ));
        }
        *recovered_lock(&self.shared.v2_registration) =
            Some(HostV2Registration::new(app_root, source_name, permissions)?);
        Ok(())
    }

    pub(crate) fn set_v2_pairing_permissions(
        &self,
        permissions: DevicePermissions,
    ) -> Result<(), PeerSyncError> {
        let mut registration = recovered_lock(&self.shared.v2_registration);
        let registration = registration
            .as_mut()
            .ok_or_else(|| PeerSyncError::Protocol("LAN v2 registry is not enabled".to_owned()))?;
        registration.permissions = permissions;
        Ok(())
    }

    fn rehydrate_registered_devices(&self) -> Result<(), PeerSyncError> {
        let Some(registration) = recovered_lock(&self.shared.v2_registration).clone() else {
            return Ok(());
        };
        let registry = OutgoingDeviceRegistry::load(&registration.app_root)?;
        let mut devices = recovered_lock(&self.shared.devices);
        devices.clear();
        for device in registry.devices() {
            let digest: [u8; 32] = hex::decode(&device.bearer_digest)
                .map_err(|_| PeerSyncError::Validation("invalid peer bearer digest".to_owned()))?
                .try_into()
                .map_err(|_| PeerSyncError::Validation("invalid peer bearer digest".to_owned()))?;
            devices.insert(
                device.device_id.clone(),
                DeviceState {
                    info: LanDevice {
                        device_id: device.device_id.clone(),
                        verified_bytes: 0,
                        current_object: None,
                        last_seen_unix_ms: now_ms(),
                        revoked: false,
                    },
                    bearer_digest: digest,
                    permissions: device.permissions.clone(),
                },
            );
        }
        Ok(())
    }

    // Desktop trusted-LAN hosting; Android hosts bind explicit interfaces.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub fn start(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::UNSPECIFIED, 0)
    }

    pub(crate) fn start_fixed_lan(&mut self, port: u16) -> Result<LanPairing, PeerSyncError> {
        if port == 0 {
            return Err(PeerSyncError::Validation(
                "shared LAN port must be nonzero".to_owned(),
            ));
        }
        self.start_on(Ipv4Addr::UNSPECIFIED, port)
    }

    pub(crate) fn start_fixed_on(
        &mut self,
        address: Ipv4Addr,
        port: u16,
    ) -> Result<LanPairing, PeerSyncError> {
        if port == 0 {
            return Err(PeerSyncError::Validation(
                "shared LAN port must be nonzero".to_owned(),
            ));
        }
        self.start_on(address, port)
    }

    // Android source hosting binds the selected private interface.
    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    pub(crate) fn start_private_lan(
        &mut self,
        address: Ipv4Addr,
    ) -> Result<LanPairing, PeerSyncError> {
        if !address.is_private() && !address.is_link_local() {
            return Err(PeerSyncError::Validation(
                "LAN source address must be private or link-local IPv4".to_owned(),
            ));
        }
        self.start_on(address, 0)
    }

    #[cfg(desktop)]
    pub(crate) fn start_quick_tunnel_origin(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::LOCALHOST, 0)
    }

    #[cfg(desktop)]
    pub(crate) fn start_fixed_loopback(&mut self, port: u16) -> Result<LanPairing, PeerSyncError> {
        if port == 0 {
            return Err(PeerSyncError::Validation(
                "shared loopback port must be nonzero".to_owned(),
            ));
        }
        self.start_on(Ipv4Addr::LOCALHOST, port)
    }

    #[cfg(desktop)]
    pub(crate) fn start_named_tunnel_origin(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::LOCALHOST, named_tunnel_origin_port())
    }

    fn start_on(&mut self, bind_address: Ipv4Addr, port: u16) -> Result<LanPairing, PeerSyncError> {
        if self.thread.is_some() {
            return Err(PeerSyncError::Protocol(
                "LAN clone host is already running".to_owned(),
            ));
        }
        self.rehydrate_registered_devices()?;
        let v2_permissions = recovered_lock(&self.shared.v2_registration)
            .as_ref()
            .map(|registration| registration.permissions.clone());
        let claim = random_secret()?;
        let expires_at_ms = now_ms().saturating_add(CLAIM_TTL.as_millis());
        *recovered_lock(&self.shared.claim) = Some(ClaimState {
            digest: digest(&claim),
            expires_at: Instant::now() + CLAIM_TTL,
            expires_at_ms,
            consumed: false,
            v2_permissions: v2_permissions.clone(),
        });
        let listener = TcpListener::bind((bind_address, port)).map_err(|error| {
            #[cfg(desktop)]
            if bind_address == Ipv4Addr::LOCALHOST
                && port == named_tunnel_origin_port()
                && port != 0
                && error.kind() == io::ErrorKind::AddrInUse
            {
                return PeerSyncError::Transport(NAMED_TUNNEL_ORIGIN_UNAVAILABLE.to_owned());
            }
            transport(error)
        })?;
        listener.set_nonblocking(true).map_err(transport)?;
        let address = listener.local_addr().map_err(transport)?;
        if address.ip() != std::net::IpAddr::V4(bind_address) {
            return Err(PeerSyncError::Protocol(
                "LAN clone server did not bind the requested IPv4 interface".to_owned(),
            ));
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let active_connection = Arc::new(Mutex::new(None));
        let shared = Arc::clone(&self.shared);
        let thread_stopped = Arc::clone(&stopped);
        let thread_active_connection = Arc::clone(&active_connection);
        self.thread = Some(thread::spawn(move || {
            serve(listener, shared, thread_stopped, thread_active_connection)
        }));
        self.address = Some(address);
        self.stopped = Some(stopped);
        self.active_connection = Some(active_connection);
        Ok(LanPairing {
            session_id: primary_session(&self.shared).session_id().to_owned(),
            manifest_id: primary_session(&self.shared).manifest_id().to_owned(),
            claim: hex::encode(claim),
            permissions: v2_permissions.map(|permissions| permissions.values().to_vec()),
        })
    }

    pub fn address(&self) -> Option<SocketAddr> {
        self.address
    }

    pub(crate) fn pairing_expires_at_ms(&self) -> Option<u128> {
        recovered_lock(&self.shared.claim)
            .as_ref()
            .map(|claim| claim.expires_at_ms)
    }

    pub(crate) fn rotate_pairing_link(&self) -> Result<LanPairing, PeerSyncError> {
        if self.thread.is_none() {
            return Err(PeerSyncError::Protocol(
                "LAN clone host is not running".to_owned(),
            ));
        }
        let claim = random_secret()?;
        let expires_at_ms = now_ms().saturating_add(CLAIM_TTL.as_millis());
        let permissions = recovered_lock(&self.shared.v2_registration)
            .as_ref()
            .map(|registration| registration.permissions.clone());
        *recovered_lock(&self.shared.claim) = Some(ClaimState {
            digest: digest(&claim),
            expires_at: Instant::now() + CLAIM_TTL,
            expires_at_ms,
            consumed: false,
            v2_permissions: permissions.clone(),
        });
        Ok(LanPairing {
            session_id: primary_session(&self.shared).session_id().to_owned(),
            manifest_id: primary_session(&self.shared).manifest_id().to_owned(),
            claim: hex::encode(claim),
            permissions: permissions.map(|value| value.values().to_vec()),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn manifest(&self) -> &super::CloneManifest {
        match primary_session(&self.shared) {
            LanSession::Clone(session) => session.manifest(),
            LanSession::Logical(_) | LanSession::BidirectionalLogical(_) => {
                panic!("logical LAN session has no clone manifest")
            }
        }
    }

    pub(crate) fn control(&self) -> LanCloneHostControl {
        LanCloneHostControl {
            shared: Arc::downgrade(&self.shared),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_after_v2_registry_claim_hook_for_test(
        &self,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) {
        *recovered_lock(&self.shared.after_v2_registry_claim) = Some(hook);
    }

    #[cfg(desktop)]
    pub(crate) fn issue_tunnel_probe(&self) -> Result<TunnelOriginProbe, PeerSyncError> {
        let Some(address) = self.address else {
            return Err(PeerSyncError::Protocol(
                "tunnel origin is not running".to_owned(),
            ));
        };
        if address.ip() != std::net::IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
            return Err(PeerSyncError::Protocol(
                "tunnel origin is not bound to IPv4 loopback".to_owned(),
            ));
        }
        let secret = random_secret()?;
        let expected_body = random_secret()?;
        *recovered_lock(&self.shared.tunnel_probe) = Some(TunnelProbeState {
            digest: digest(&secret),
            expected_body,
        });
        let path_prefix = format!(
            "/v1/sessions/{}/tunnel-check",
            primary_session(&self.shared).session_id()
        );
        let path = format!("{path_prefix}/{}", hex::encode(secret));
        Ok(TunnelOriginProbe {
            path_prefix,
            path,
            expected_body,
        })
    }

    #[cfg(desktop)]
    pub(crate) fn clear_tunnel_probe(&self) {
        *recovered_lock(&self.shared.tunnel_probe) = None;
    }

    // Android source status surfaces the device list and revocation.
    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    pub fn devices(&self) -> Vec<LanDevice> {
        self.control().devices()
    }

    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    pub fn revoke(&self, device_id: &str) -> bool {
        self.control().revoke(device_id)
    }

    // Whether the serve loop has registered a connection it is currently
    // handling; the stalled-peer Stop tests wait on this instead of sleeping.
    #[cfg(test)]
    pub(crate) fn has_active_connection_for_test(&self) -> bool {
        self.active_connection
            .as_ref()
            .is_some_and(|active| recovered_lock(active).is_some())
    }

    pub fn stop(&mut self) -> Result<(), PeerSyncError> {
        if let Some(stopped) = &self.stopped {
            stopped.store(true, Ordering::SeqCst);
        }
        if let Some(active_connection) = &self.active_connection {
            if let Some(connection) = recovered_lock(active_connection).take() {
                let _ = connection.shutdown(Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| PeerSyncError::Transport("LAN server thread panicked".to_owned()))??;
        }
        *recovered_lock(&self.shared.claim) = None;
        #[cfg(desktop)]
        self.clear_tunnel_probe();
        recovered_lock(&self.shared.devices).clear();
        self.address = None;
        self.stopped = None;
        self.active_connection = None;
        Ok(())
    }

    #[cfg(test)]
    pub fn expire_claim_for_test(&self) {
        if let Some(claim) = self.shared.claim.lock().unwrap().as_mut() {
            claim.expires_at = Instant::now() - Duration::from_secs(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_claim_for_test(&self) -> bool {
        self.shared.claim.lock().unwrap().is_some()
    }
}

#[cfg(any(desktop, target_os = "android"))]
impl Drop for LanCloneHost {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn serve(
    listener: TcpListener,
    shared: Arc<LanShared>,
    stopped: Arc<AtomicBool>,
    active_connection: Arc<Mutex<Option<TcpStream>>>,
) -> Result<(), PeerSyncError> {
    while !stopped.load(Ordering::SeqCst) {
        let (stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(transport(error)),
        };
        configure_connection(&stream)?;
        let shutdown_handle = stream.try_clone().map_err(transport)?;
        {
            let mut active = recovered_lock(&active_connection);
            if stopped.load(Ordering::SeqCst) {
                let _ = shutdown_handle.shutdown(Shutdown::Both);
                break;
            }
            *active = Some(shutdown_handle);
        }
        let _ = handle_connection(stream, &shared, &stopped);
        recovered_lock(&active_connection).take();
    }
    Ok(())
}

#[cfg(any(desktop, target_os = "android"))]
fn configure_connection(stream: &TcpStream) -> Result<(), PeerSyncError> {
    stream.set_nonblocking(false).map_err(transport)?;
    stream
        .set_read_timeout(Some(CONNECTION_READ_POLL_TIMEOUT))
        .map_err(transport)?;
    stream
        .set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT))
        .map_err(transport)?;
    Ok(())
}

#[cfg(any(desktop, target_os = "android"))]
struct HttpRequest {
    method: String,
    url: String,
    authorization: Option<String>,
    content_type: Option<String>,
    peer_completion: Option<String>,
    peer_completion_lease: Option<String>,
    peer_completion_resume: Option<String>,
    range: Option<String>,
    range_count: usize,
    body: Vec<u8>,
}

#[cfg(any(desktop, target_os = "android"))]
enum RequestReadError {
    Http(u16),
    Io(io::Error),
    Stopped,
}

#[cfg(any(desktop, target_os = "android"))]
fn handle_connection(
    mut stream: TcpStream,
    shared: &LanShared,
    stopped: &Arc<AtomicBool>,
) -> Result<(), PeerSyncError> {
    let request = match read_request(&mut stream, stopped) {
        Ok(request) => request,
        Err(RequestReadError::Http(status)) => return respond_empty(&mut stream, status),
        Err(RequestReadError::Io(error)) => return Err(transport(error)),
        Err(RequestReadError::Stopped) => return Ok(()),
    };
    if stopped.load(Ordering::SeqCst) {
        return Ok(());
    }
    handle_request(&mut stream, request, shared, stopped)
}

#[cfg(any(desktop, target_os = "android"))]
fn read_request(
    stream: &mut TcpStream,
    stopped: &AtomicBool,
) -> Result<HttpRequest, RequestReadError> {
    read_request_started(stream, stopped, Instant::now())
}

#[cfg(any(desktop, target_os = "android"))]
fn read_request_started(
    stream: &mut TcpStream,
    stopped: &AtomicBool,
    request_started: Instant,
) -> Result<HttpRequest, RequestReadError> {
    read_request_with_elapsed(stream, stopped, || request_started.elapsed())
}

#[cfg(any(desktop, target_os = "android"))]
fn read_request_with_elapsed(
    stream: &mut impl Read,
    stopped: &AtomicBool,
    elapsed: impl Fn() -> Duration,
) -> Result<HttpRequest, RequestReadError> {
    let mut head = [0_u8; MAX_REQUEST_HEAD_BYTES];
    let mut head_len = 0_usize;
    let head_end = loop {
        if stopped.load(Ordering::SeqCst) {
            return Err(RequestReadError::Stopped);
        }
        if elapsed() >= REQUEST_READ_DEADLINE {
            return Err(RequestReadError::Http(408));
        }
        if let Some(position) = find_bytes(&head[..head_len], b"\r\n\r\n") {
            break position;
        }
        if let Some(line_end) = find_bytes(&head[..head_len], b"\r\n") {
            if line_end > MAX_REQUEST_LINE_BYTES
                || head_len.saturating_sub(line_end + 2) > MAX_HEADER_BYTES
            {
                return Err(RequestReadError::Http(413));
            }
        } else if head_len > MAX_REQUEST_LINE_BYTES {
            return Err(RequestReadError::Http(413));
        }
        if head_len == head.len() {
            return Err(RequestReadError::Http(413));
        }
        match stream.read(&mut head[head_len..]) {
            Ok(0) => return Err(RequestReadError::Http(400)),
            Ok(read) => head_len += read,
            Err(error) if is_timeout(&error) => {
                if stopped.load(Ordering::SeqCst) {
                    return Err(RequestReadError::Stopped);
                }
                if elapsed() >= REQUEST_READ_DEADLINE {
                    return Err(RequestReadError::Http(408));
                }
            }
            Err(error) => return Err(RequestReadError::Io(error)),
        }
    };

    let line_end = find_bytes(&head[..head_end], b"\r\n").unwrap_or(head_end);
    if line_end > MAX_REQUEST_LINE_BYTES
        || head_end.saturating_sub(line_end.saturating_add(2)) > MAX_HEADER_BYTES
    {
        return Err(RequestReadError::Http(413));
    }
    let request_line =
        std::str::from_utf8(&head[..line_end]).map_err(|_| RequestReadError::Http(400))?;
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let url = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || url.is_empty() || version != "HTTP/1.1" || parts.next().is_some() {
        return Err(RequestReadError::Http(400));
    }
    if url.len() > MAX_URL_BYTES {
        return Err(RequestReadError::Http(413));
    }

    let mut authorization = None;
    let mut content_type = None;
    let mut peer_completion = None;
    let mut peer_completion_lease = None;
    let mut peer_completion_resume = None;
    let mut range = None;
    let mut range_count = 0_usize;
    let mut content_length = None;
    let header_start = (line_end + 2).min(head_end);
    let headers = std::str::from_utf8(&head[header_start..head_end])
        .map_err(|_| RequestReadError::Http(400))?;
    for line in headers.split("\r\n").filter(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return Err(RequestReadError::Http(400));
        };
        if !valid_header_name(name) {
            return Err(RequestReadError::Http(400));
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            if authorization.replace(value.to_owned()).is_some() {
                return Err(RequestReadError::Http(400));
            }
        } else if name.eq_ignore_ascii_case("content-type") {
            if content_type.replace(value.to_owned()).is_some() {
                return Err(RequestReadError::Http(400));
            }
        } else if name.eq_ignore_ascii_case(PEER_COMPLETION_CAPABILITY_HEADER) {
            if peer_completion.replace(value.to_owned()).is_some() {
                return Err(RequestReadError::Http(400));
            }
        } else if name.eq_ignore_ascii_case(PEER_COMPLETION_LEASE_HEADER) {
            if peer_completion_lease.replace(value.to_owned()).is_some() {
                return Err(RequestReadError::Http(400));
            }
        } else if name.eq_ignore_ascii_case(PEER_COMPLETION_RESUME_HEADER) {
            if peer_completion_resume.replace(value.to_owned()).is_some() {
                return Err(RequestReadError::Http(400));
            }
        } else if name.eq_ignore_ascii_case("range") {
            range_count += 1;
            if range.is_none() {
                range = Some(value.to_owned());
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(RequestReadError::Http(400));
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| RequestReadError::Http(400))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(RequestReadError::Http(400));
        }
    }

    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(RequestReadError::Http(413));
    }
    let body_start = head_end + 4;
    let available = head_len.saturating_sub(body_start).min(content_length);
    let mut body = Vec::with_capacity(content_length);
    body.extend_from_slice(&head[body_start..body_start + available]);
    while body.len() < content_length {
        if stopped.load(Ordering::SeqCst) {
            return Err(RequestReadError::Stopped);
        }
        if elapsed() >= REQUEST_READ_DEADLINE {
            return Err(RequestReadError::Http(408));
        }
        let remaining = content_length - body.len();
        let mut buffer = [0_u8; MAX_BODY_BYTES];
        match stream.read(&mut buffer[..remaining]) {
            Ok(0) => return Err(RequestReadError::Http(400)),
            Ok(read) => {
                body.extend_from_slice(&buffer[..read]);
                if elapsed() >= REQUEST_READ_DEADLINE {
                    return Err(RequestReadError::Http(408));
                }
            }
            Err(error) if is_timeout(&error) => {
                if stopped.load(Ordering::SeqCst) {
                    return Err(RequestReadError::Stopped);
                }
                if elapsed() >= REQUEST_READ_DEADLINE {
                    return Err(RequestReadError::Http(408));
                }
            }
            Err(error) => return Err(RequestReadError::Io(error)),
        }
    }
    Ok(HttpRequest {
        method: method.to_owned(),
        url: url.to_owned(),
        authorization,
        content_type,
        peer_completion,
        peer_completion_lease,
        peer_completion_resume,
        range,
        range_count,
        body,
    })
}

#[cfg(all(test, desktop))]
pub(super) fn read_request_with_elapsed_for_test(
    stream: &mut impl Read,
    stopped: &AtomicBool,
    elapsed: impl Fn() -> Duration,
) -> Result<(), u16> {
    match read_request_with_elapsed(stream, stopped, elapsed) {
        Ok(_) => Ok(()),
        Err(RequestReadError::Http(status)) => Err(status),
        Err(RequestReadError::Io(_) | RequestReadError::Stopped) => Err(0),
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn handle_request(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    stopped: &Arc<AtomicBool>,
) -> Result<(), PeerSyncError> {
    // Route before constructing a session path. This endpoint stays stable
    // across source restarts, unlike the per-session endpoints below.
    if request.url == "/v1/peer/hello" {
        return hello(stream, request, shared);
    }
    if request.url == "/v1/peer/completion" {
        return completion(stream, request, shared);
    }
    let Some(session_id) = request
        .url
        .strip_prefix("/v1/sessions/")
        .and_then(|path| path.split('/').next())
    else {
        return respond_empty(stream, 404);
    };
    let Some(session) = session_by_id(shared, session_id) else {
        return respond_empty(stream, 404);
    };
    let prefix = format!("/v1/sessions/{session_id}");
    #[cfg(desktop)]
    {
        let tunnel_probe_prefix = format!("{prefix}/tunnel-check/");
        if let Some(candidate) = request
            .url
            .strip_prefix(&tunnel_probe_prefix)
            .map(str::to_owned)
        {
            return tunnel_probe(stream, request, shared, &candidate);
        }
    }
    if request.url == format!("{prefix}/claim") {
        return claim(stream, request, shared, session);
    }
    let device = match authorize(&request, shared) {
        Ok(device) => device,
        Err(status) => return respond_empty(stream, status),
    };
    if matches!(session, LanSession::BidirectionalLogical(_))
        && !device.permissions.allows_bidirectional()
    {
        return respond_empty(stream, 403);
    }
    if !device.permissions.allows_read() {
        return respond_empty(stream, 403);
    }
    if request.url == format!("{prefix}/manifest") {
        if request.method != "GET" {
            return respond_empty(stream, 405);
        }
        if let Some(completion_version) = request.peer_completion.as_deref() {
            if completion_version != PEER_COMPLETION_CAPABILITY_V1 {
                return respond_empty(stream, 400);
            }
            if let Some(registration) = recovered_lock(&shared.v2_registration).clone() {
                let issued = match session {
                    LanSession::Clone(clone) => {
                        let transferred_bytes = clone
                            .manifest()
                            .objects
                            .values()
                            .try_fold(0_u64, |total, object| total.checked_add(object.size))
                            .ok_or_else(|| {
                                PeerSyncError::Validation(
                                    "clone manifest byte count overflow".to_owned(),
                                )
                            })?;
                        issue_outgoing_measured_completion_offer(
                            &registration.app_root,
                            &device.device_id,
                            CompletionLane::Clone,
                            session.manifest_id(),
                            transferred_bytes,
                            request.peer_completion_resume.as_deref(),
                        )
                    }
                    LanSession::Logical(session) => issue_outgoing_logical_completion_lease(
                        &registration.app_root,
                        &device.device_id,
                        CompletionLane::Delta,
                        &session.manifest_id,
                        request.peer_completion_resume.as_deref(),
                        &session.objects,
                        &shared.issued_logical_objects,
                    ),
                    LanSession::BidirectionalLogical(session) => {
                        issue_outgoing_logical_completion_lease(
                            &registration.app_root,
                            &device.device_id,
                            CompletionLane::Bidirectional,
                            &session.logical.manifest_id,
                            request.peer_completion_resume.as_deref(),
                            &session.logical.objects,
                            &shared.issued_logical_objects,
                        )
                    }
                };
                let lease = match issued {
                    Ok(lease) => lease,
                    Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                        return respond_empty(stream, 409);
                    }
                    Err(_) => return respond_empty(stream, 500),
                };
                let etag = quoted(session.manifest_id());
                return respond_bytes(
                    stream,
                    200,
                    &[
                        ("Content-Type", "application/json"),
                        ("ETag", &etag),
                        (
                            PEER_COMPLETION_CAPABILITY_HEADER,
                            PEER_COMPLETION_CAPABILITY_V1,
                        ),
                        (PEER_COMPLETION_LEASE_HEADER, lease.as_str()),
                    ],
                    match session {
                        LanSession::Clone(session) => session.manifest_bytes(),
                        LanSession::Logical(session) => &session.manifest_bytes,
                        LanSession::BidirectionalLogical(session) => {
                            &session.logical.manifest_bytes
                        }
                    },
                );
            }
        } else if request.peer_completion_resume.is_some() {
            return respond_empty(stream, 400);
        }
        return respond_bytes(
            stream,
            200,
            &[
                ("Content-Type", "application/json"),
                ("ETag", &quoted(session.manifest_id())),
            ],
            match session {
                LanSession::Clone(session) => session.manifest_bytes(),
                LanSession::Logical(session) => &session.manifest_bytes,
                LanSession::BidirectionalLogical(session) => &session.logical.manifest_bytes,
            },
        );
    }
    if request.url == format!("{prefix}/progress") {
        if request.method != "POST" {
            return respond_empty(stream, 405);
        }
        return progress(stream, request, shared, session, &device.device_id);
    }
    if request.url == format!("{prefix}/registration") {
        return bidirectional_registration(stream, request, session, &device);
    }
    if request.url == format!("{prefix}/remote-apply") {
        return bidirectional_remote_apply(stream, request, shared, session, &device, stopped);
    }
    let object_prefix = format!("{prefix}/objects/");
    let Some(object) = request.url.strip_prefix(&object_prefix).map(str::to_owned) else {
        return respond_empty(stream, 404);
    };
    if object.contains('/') || session.object_size(&object).is_none() {
        return respond_empty(stream, 404);
    }
    match (session, request.method.as_str()) {
        (LanSession::Clone(_), "HEAD") => head(stream, session, &object),
        (LanSession::Clone(_), "GET") => range(stream, &request, session, &object, stopped),
        (LanSession::Logical(_) | LanSession::BidirectionalLogical(_), "HEAD") => {
            head(stream, session, &object)
        }
        (LanSession::Logical(_) | LanSession::BidirectionalLogical(_), "GET") => {
            if request.range_count != 0 || request.range.is_some() || !request.body.is_empty() {
                return respond_empty(stream, 400);
            }
            let completion_issue_guard = match request.peer_completion_lease.as_deref() {
                None => None,
                Some(lease) => {
                    let Ok(lease) = CompletionLeaseId::parse(lease) else {
                        return respond_empty(stream, 400);
                    };
                    let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
                        return respond_empty(stream, 409);
                    };
                    let Some((lane, manifest)) = session.logical_completion() else {
                        return respond_empty(stream, 409);
                    };
                    match begin_outgoing_logical_object_issue(
                        &registration.app_root,
                        &device.device_id,
                        lane,
                        lease.as_str(),
                        session.manifest_id(),
                        &object,
                        manifest,
                        &shared.issued_logical_objects,
                    ) {
                        Ok(guard) => Some(guard),
                        Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                            return respond_empty(stream, 409);
                        }
                        Err(_) => return respond_empty(stream, 500),
                    }
                }
            };
            logical_object(
                stream,
                shared,
                session,
                &device.device_id,
                &object,
                completion_issue_guard,
                stopped,
            )
        }
        _ => respond_empty(stream, 405),
    }
}

#[cfg(desktop)]
fn tunnel_probe(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    candidate: &str,
) -> Result<(), PeerSyncError> {
    if request.method != "GET"
        || request.authorization.is_some()
        || request.range.is_some()
        || request.range_count != 0
        || !request.body.is_empty()
        || candidate.len() != 64
        || candidate.contains('/')
    {
        return respond_empty(stream, 404);
    }
    let Ok(secret) = hex::decode(candidate) else {
        return respond_empty(stream, 404);
    };
    if secret.len() != 32 {
        return respond_empty(stream, 404);
    }
    let mut probe = recovered_lock(&shared.tunnel_probe);
    let Some(current) = probe.as_ref() else {
        return respond_empty(stream, 404);
    };
    if !constant_time_eq(&current.digest, &digest(&secret)) {
        return respond_empty(stream, 404);
    }
    let expected_body = probe.take().expect("checked tunnel probe").expected_body;
    respond_bytes(
        stream,
        200,
        &[("Content-Type", "application/octet-stream")],
        &expected_body,
    )
}

#[cfg(any(desktop, target_os = "android"))]
fn claim(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    session: &LanSession,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    let Ok(request_body) = serde_json::from_slice::<ClaimRequest>(&request.body) else {
        return respond_empty(stream, 400);
    };
    if request_body.claim.len() != 64 {
        return respond_empty(stream, 403);
    }
    let Ok(secret) = hex::decode(request_body.claim) else {
        return respond_empty(stream, 403);
    };
    if secret.len() != 32 {
        return respond_empty(stream, 403);
    }
    if request_body.protocol_version != CLAIM_PROTOCOL_VERSION || request_body.permissions.is_some()
    {
        return respond_empty(stream, 400);
    }
    let Some(device_id) = request_body.device_id.as_deref() else {
        return respond_empty(stream, 400);
    };
    if !is_canonical_uuid(device_id) {
        return respond_empty(stream, 400);
    }
    let device_id = device_id.to_owned();
    let registration = recovered_lock(&shared.v2_registration).clone();
    // A registering claim names the device it registers; a session a lane opens for
    // a single operation registers nothing and answers with its own identity.
    let registered_device_name = match &registration {
        Some(_) => {
            let Some(device_name) = request_body.device_name.as_deref() else {
                return respond_empty(stream, 400);
            };
            if validate_device_name(device_name).is_err() {
                return respond_empty(stream, 400);
            }
            Some(device_name.to_owned())
        }
        None => None,
    };
    let (source_device_id, source_device_name) = match (&registration, session.source_device_id()) {
        (Some(registration), _) => (
            registration.device_id.clone(),
            Some(registration.name.clone()),
        ),
        (None, Some(source_device_id)) => (source_device_id.to_owned(), None),
        (None, None) => return respond_empty(stream, 400),
    };
    let mut claim = recovered_lock(&shared.claim);
    let Some(claim) = claim.as_mut() else {
        return respond_empty(stream, 410);
    };
    if claim.consumed || Instant::now() >= claim.expires_at {
        return respond_empty(stream, 410);
    }
    if !constant_time_eq(&claim.digest, &digest(&secret)) {
        return respond_empty(stream, 403);
    }
    let bearer = hex::encode(random_secret()?);
    let permissions = claim
        .v2_permissions
        .clone()
        .unwrap_or_else(|| session.lane_permissions());
    let registered_app_root = registration
        .as_ref()
        .map(|registration| registration.app_root.clone());
    if let (Some(registration), Some(name)) = (&registration, &registered_device_name) {
        if register_outgoing_claim(
            &registration.app_root,
            OutgoingDevice {
                device_id: device_id.clone(),
                name: name.clone(),
                bearer_digest: hex::encode(digest(bearer.as_bytes())),
                permissions: permissions.clone(),
                created_at_ms: now_ms() as u64,
                last_seen_ms: 0,
                total_bytes: 0,
            },
        )
        .is_err()
        {
            return respond_empty(stream, 500);
        }
        #[cfg(test)]
        if let Some(hook) = recovered_lock(&shared.after_v2_registry_claim).clone() {
            hook();
        }
    }
    claim.consumed = true;
    let response = ClaimResponse {
        device_id: device_id.clone(),
        bearer,
        permission: session.permission(),
        source_device_id: &source_device_id,
        source_device_name: source_device_name.as_deref(),
        permissions: permissions.values(),
    };
    recovered_lock(&shared.devices).insert(
        device_id,
        DeviceState {
            info: LanDevice {
                device_id: response.device_id.clone(),
                verified_bytes: 0,
                current_object: None,
                last_seen_unix_ms: now_ms(),
                revoked: false,
            },
            bearer_digest: digest(response.bearer.as_bytes()),
            permissions: permissions.clone(),
        },
    );
    if let Some(app_root) = registered_app_root {
        if !outgoing_device_is_registered(&app_root, &response.device_id).unwrap_or(false) {
            recovered_lock(&shared.devices).remove(&response.device_id);
        }
    }
    respond_json(stream, 200, &response)
}

#[cfg(any(desktop, target_os = "android"))]
struct AuthorizedDevice {
    device_id: String,
    permissions: DevicePermissions,
}

#[cfg(any(desktop, target_os = "android"))]
fn authorize(request: &HttpRequest, shared: &LanShared) -> Result<AuthorizedDevice, u16> {
    let value = request.authorization.as_deref().ok_or(401_u16)?;
    let Some(bearer) = value.strip_prefix("Bearer ") else {
        return Err(401);
    };
    if bearer.len() != 64 {
        return Err(401);
    }
    let candidate = digest(bearer.as_bytes());
    let mut devices = recovered_lock(&shared.devices);
    let mut authorized = None;
    for (id, device) in devices.iter_mut() {
        if constant_time_eq(&device.bearer_digest, &candidate) {
            if device.info.revoked {
                return Err(403);
            }
            device.info.last_seen_unix_ms = now_ms();
            authorized = Some(AuthorizedDevice {
                device_id: id.clone(),
                permissions: device.permissions.clone(),
            });
            break;
        }
    }
    drop(devices);
    let authorized = authorized.ok_or(401_u16)?;
    if let Some(registration) = recovered_lock(&shared.v2_registration).clone() {
        record_outgoing_seen(&registration.app_root, &authorized.device_id).map_err(|_| 500_u16)?;
    }
    Ok(authorized)
}

#[cfg(any(desktop, target_os = "android"))]
fn progress(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    session: &LanSession,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    let Ok(progress) = serde_json::from_slice::<ProgressRequest>(&request.body) else {
        return respond_empty(stream, 400);
    };
    if progress
        .operation_id
        .as_deref()
        .is_some_and(|operation_id| !is_canonical_v4_uuid(operation_id))
    {
        return respond_empty(stream, 400);
    }
    if matches!(session, LanSession::Clone(_))
        && (progress.manifest_id.is_some() || progress.verified_object.is_some())
    {
        return respond_empty(stream, 400);
    }
    let strict_logical = !matches!(session, LanSession::Clone(_))
        && (progress.operation_id.is_some()
            || progress.manifest_id.is_some()
            || progress.verified_object.is_some());
    if strict_logical {
        if progress.current_object.is_some()
            || progress.verified_object.is_none()
            || progress
                .verified_object
                .as_deref()
                .is_some_and(|object| validate_object_hash(object).is_err())
        {
            return respond_empty(stream, 400);
        }
        let (Some(lease_id), Some(manifest_id)) = (
            progress.operation_id.as_deref(),
            progress.manifest_id.as_deref(),
        ) else {
            return respond_empty(stream, 409);
        };
        if manifest_id != session.manifest_id() {
            return respond_empty(stream, 409);
        }
        let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
            return respond_empty(stream, 409);
        };
        let Some((lane, manifest)) = session.logical_completion() else {
            return respond_empty(stream, 409);
        };
        let verified_bytes = match record_outgoing_logical_progress(
            &registration.app_root,
            device_id,
            lane,
            lease_id,
            manifest_id,
            progress.verified_object.as_deref(),
            manifest,
            &shared.issued_logical_objects,
        ) {
            Ok(result) => result.verified_bytes(),
            Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                return respond_empty(stream, 409);
            }
            Err(_) => return respond_empty(stream, 500),
        };
        if let Some(device) = recovered_lock(&shared.devices).get_mut(device_id) {
            device.info.verified_bytes = verified_bytes;
            device.info.current_object = None;
            device.info.last_seen_unix_ms = now_ms();
        }
        return respond_empty(stream, 204);
    }
    if progress
        .current_object
        .as_deref()
        .is_some_and(|object| session.object_size(object).is_none())
    {
        return respond_empty(stream, 400);
    }
    let clone_total = match session {
        LanSession::Clone(session) => Some(
            session
                .manifest()
                .objects
                .values()
                .try_fold(0_u64, |total, object| total.checked_add(object.size))
                .ok_or_else(|| {
                    PeerSyncError::Validation("clone manifest byte count overflow".to_owned())
                })?,
        ),
        LanSession::Logical(_) | LanSession::BidirectionalLogical(_) => None,
    };
    if clone_total.is_some_and(|total| progress.verified_bytes > total) {
        return respond_empty(stream, 400);
    }
    let clone_completed = clone_total
        .is_some_and(|total| progress.current_object.is_none() && progress.verified_bytes == total);
    if let Some(device) = recovered_lock(&shared.devices).get_mut(device_id) {
        device.info.verified_bytes = progress.verified_bytes;
        device.info.current_object = progress.current_object;
        device.info.last_seen_unix_ms = now_ms();
    }
    if clone_completed {
        if let Some(completion_lease_id) = progress.operation_id.as_deref() {
            let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
                return respond_empty(stream, 409);
            };
            match seal_outgoing_completion_lease(
                &registration.app_root,
                device_id,
                CompletionLane::Clone,
                completion_lease_id,
                session.manifest_id(),
                progress.verified_bytes,
            ) {
                Ok(CompletionSealStatus::Sealed | CompletionSealStatus::AlreadyCompleted) => {}
                Ok(CompletionSealStatus::Conflict) => return respond_empty(stream, 409),
                Err(_) => return respond_empty(stream, 500),
            }
        } else if let Some(registration) = recovered_lock(&shared.v2_registration).clone() {
            match outgoing_completion_offer_active(
                &registration.app_root,
                device_id,
                CompletionLane::Clone,
            ) {
                Ok(true) => return respond_empty(stream, 409),
                Ok(false) => {}
                Err(_) => return respond_empty(stream, 500),
            }
        }
    }
    respond_empty(stream, 204)
}

#[cfg(any(desktop, target_os = "android"))]
fn authenticated_bidirectional_session(
    session: &LanSession,
    target_device_id: &str,
) -> Option<LanBidirectionalSession> {
    session
        .bidirectional_control()
        .and_then(|_| session.source_device_id())
        .map(|source_device_id| LanBidirectionalSession {
            session_id: session.session_id().to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        })
}

#[cfg(any(desktop, target_os = "android"))]
fn bidirectional_registration(
    stream: &mut TcpStream,
    request: HttpRequest,
    session: &LanSession,
    device: &AuthorizedDevice,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    if !device.permissions.allows_bidirectional() {
        return respond_empty(stream, 403);
    }
    let Some(bidirectional_session) =
        authenticated_bidirectional_session(session, &device.device_id)
    else {
        return respond_empty(stream, 404);
    };
    let Ok(request) = serde_json::from_slice::<LanBidirectionalRegistrationRequest>(&request.body)
    else {
        return respond_empty(stream, 400);
    };
    if !request.is_valid() {
        return respond_empty(stream, 400);
    }
    let control = session
        .bidirectional_control()
        .expect("checked bidirectional session");
    match control.register(bidirectional_session, request) {
        Ok(()) => respond_empty(stream, 204),
        Err(_) => respond_empty(stream, 409),
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn bidirectional_remote_apply(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    session: &LanSession,
    device: &AuthorizedDevice,
    stopped: &Arc<AtomicBool>,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    if !device.permissions.allows_bidirectional() {
        return respond_empty(stream, 403);
    }
    let Some(bidirectional_session) =
        authenticated_bidirectional_session(session, &device.device_id)
    else {
        return respond_empty(stream, 404);
    };
    let Ok(request) = serde_json::from_slice::<LanBidirectionalRemoteApplyRequest>(&request.body)
    else {
        return respond_empty(stream, 400);
    };
    if !request.is_valid() {
        return respond_empty(stream, 400);
    }
    let operation_id = request.operation_id.clone();
    let completion_manifest_id = request.expected_source_generation.manifest_hash.clone();
    let completion_deferred = request.completion_deferred_v1 == Some(true);
    if let Some(registration) = recovered_lock(&shared.v2_registration).clone() {
        let valid_completion_mode = if completion_deferred {
            outgoing_bidirectional_completion_lease_allows_remote_apply(
                &registration.app_root,
                &device.device_id,
                &operation_id,
                &completion_manifest_id,
            )
        } else {
            outgoing_completion_offer_active(
                &registration.app_root,
                &device.device_id,
                CompletionLane::Bidirectional,
            )
            .map(|active| !active)
        };
        match valid_completion_mode {
            Ok(true) => {}
            Ok(false) => return respond_empty(stream, 409),
            Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                return respond_empty(stream, 409);
            }
            Err(_) => return respond_empty(stream, 500),
        }
    } else if completion_deferred {
        return respond_empty(stream, 409);
    }
    let control = session
        .bidirectional_control()
        .expect("checked bidirectional session");
    let cancellation = AtomicCancellation::new(Arc::clone(stopped));
    match control.remote_apply(bidirectional_session, request, &cancellation) {
        Ok(receipt) if receipt.is_valid() => {
            if completion_deferred {
                let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
                    return respond_empty(stream, 409);
                };
                match seal_outgoing_bidirectional_logical_completion(
                    &registration.app_root,
                    &device.device_id,
                    &operation_id,
                    &completion_manifest_id,
                    receipt.transferred_bytes,
                    &shared.issued_logical_objects,
                ) {
                    Ok(CompletionSealStatus::Sealed | CompletionSealStatus::AlreadyCompleted) => {}
                    Ok(CompletionSealStatus::Conflict)
                    | Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                        return respond_empty(stream, 409);
                    }
                    Err(_) => return respond_empty(stream, 500),
                }
            }
            respond_json(stream, 200, &receipt)
        }
        Err(PeerSyncError::ActivationConflict { .. } | PeerSyncError::StaleManifest { .. }) => {
            respond_empty(stream, 409)
        }
        Ok(_) | Err(_) => respond_empty(stream, 500),
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn head(stream: &mut TcpStream, session: &LanSession, object: &str) -> Result<(), PeerSyncError> {
    let size = session
        .object_size(object)
        .ok_or_else(|| PeerSyncError::Storage("session object descriptor is missing".to_owned()))?;
    let range_header = match session {
        LanSession::Clone(_) => Some(("Accept-Ranges", "bytes")),
        LanSession::Logical(_) | LanSession::BidirectionalLogical(_) => None,
    };
    let etag = quoted(object);
    let mut headers = vec![("ETag", etag.as_str())];
    if let Some(header) = range_header {
        headers.insert(0, header);
    }
    write_response_head(stream, 200, &headers, size)
}

#[cfg(any(desktop, target_os = "android"))]
fn range(
    stream: &mut TcpStream,
    request: &HttpRequest,
    session: &LanSession,
    object: &str,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    if request.range_count != 1 {
        return respond_empty(stream, 416);
    }
    let Some((start, end)) = request.range.as_deref().and_then(parse_range) else {
        return respond_empty(stream, 416);
    };
    let LanSession::Clone(session) = session else {
        return respond_empty(stream, 405);
    };
    let descriptor = &session.manifest().objects[object];
    let Some(chunk) = descriptor
        .chunks
        .iter()
        .find(|chunk| chunk.offset == start && chunk.offset + chunk.size - 1 == end)
    else {
        return respond_empty(stream, 416);
    };
    let physical = session
        .physical_hash(object)
        .ok_or_else(|| PeerSyncError::Storage("session object mapping is missing".to_owned()))?;
    let cas = PayloadCas::new(session.root())?;
    let mut file = cas
        .open_object(physical)?
        .ok_or_else(|| PeerSyncError::Storage("session object file is missing".to_owned()))?;
    file.seek(SeekFrom::Start(start))?;
    write_response_head(
        stream,
        206,
        &[
            ("Accept-Ranges", "bytes"),
            ("ETag", &quoted(object)),
            (
                "Content-Range",
                &format!("bytes {start}-{end}/{}", descriptor.size),
            ),
        ],
        chunk.size,
    )?;
    copy_exact_response(stream, &mut file, chunk.size, stopped)
}

#[cfg(any(desktop, target_os = "android"))]
fn logical_object(
    stream: &mut TcpStream,
    shared: &LanShared,
    selected: &LanSession,
    device_id: &str,
    object: &str,
    completion_issue_guard: Option<super::logical_completion::OutgoingLogicalObjectIssueGuard>,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    let session = match selected {
        LanSession::Logical(session) => session,
        LanSession::BidirectionalLogical(session) => &session.logical,
        LanSession::Clone(_) => return respond_empty(stream, 405),
    };
    let size = session.objects.object_size(object).ok_or_else(|| {
        PeerSyncError::Storage("logical session object descriptor is missing".to_owned())
    })?;
    let mut source = session.source.lock().map_err(|error| {
        PeerSyncError::Storage(format!("logical source mutex poisoned: {error}"))
    })?;
    let mut reader = source.open_object(&LogicalDeltaObject {
        hash: object.to_owned(),
        size,
    })?;
    set_current_object(shared, device_id, Some(object));
    let result = write_response_head(stream, 200, &[("ETag", &quoted(object))], size)
        .and_then(|_| copy_exact_response(stream, reader.as_mut(), size, stopped));
    set_current_object(shared, device_id, None);
    result?;
    drop(reader);
    drop(source);
    if !stopped.load(Ordering::SeqCst) {
        if let Some(guard) = completion_issue_guard {
            guard.mark_after_full_response()?;
        }
    }
    Ok(())
}

#[cfg(any(desktop, target_os = "android"))]
fn set_current_object(shared: &LanShared, device_id: &str, object: Option<&str>) {
    if let Some(device) = recovered_lock(&shared.devices).get_mut(device_id) {
        device.info.current_object = object.map(str::to_owned);
        device.info.last_seen_unix_ms = now_ms();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimRequest {
    claim: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
    protocol_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permissions: Option<Vec<String>>,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerCompletionRequest {
    schema: String,
    lane: String,
    operation_id: String,
    manifest_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transferred_bytes: Option<u64>,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerCompletionPreparedResponse {
    schema: String,
    transferred_bytes: u64,
}

#[cfg(any(desktop, target_os = "android"))]
fn completion(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    if request.range.is_some() || request.range_count != 0 {
        return respond_empty(stream, 400);
    }
    let device = match authorize(&request, shared) {
        Ok(device) => device,
        Err(status) => return respond_empty(stream, status),
    };
    if !request
        .content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return respond_empty(stream, 415);
    }
    let Ok(completion) = serde_json::from_slice::<PeerCompletionRequest>(&request.body) else {
        return respond_empty(stream, 400);
    };
    let Ok(lane) = CompletionLane::parse(&completion.lane) else {
        return respond_empty(stream, 400);
    };
    if completion.schema != PEER_COMPLETION_SCHEMA
        || !is_canonical_v4_uuid(&completion.operation_id)
        || validate_object_hash(&completion.manifest_id).is_err()
    {
        return respond_empty(stream, 400);
    }
    let permitted = match lane {
        CompletionLane::Clone | CompletionLane::Delta => device.permissions.allows_read(),
        CompletionLane::Bidirectional => device.permissions.allows_bidirectional(),
    };
    if !permitted {
        return respond_empty(stream, 403);
    }
    let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
        return respond_empty(stream, 404);
    };
    if completion.transferred_bytes.is_none() {
        if lane == CompletionLane::Clone {
            return respond_empty(stream, 400);
        }
        let transferred_bytes = match lane {
            CompletionLane::Delta => {
                let bytes = match freeze_outgoing_logical_completion_proof(
                    &registration.app_root,
                    &device.device_id,
                    lane,
                    &completion.operation_id,
                    &completion.manifest_id,
                    &shared.issued_logical_objects,
                ) {
                    Ok(bytes) => bytes,
                    Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                        return respond_empty(stream, 409);
                    }
                    Err(_) => return respond_empty(stream, 500),
                };
                match seal_outgoing_delta_logical_completion(
                    &registration.app_root,
                    &device.device_id,
                    &completion.operation_id,
                    &completion.manifest_id,
                    bytes,
                ) {
                    Ok(CompletionSealStatus::Sealed | CompletionSealStatus::AlreadyCompleted) => {
                        bytes
                    }
                    Ok(CompletionSealStatus::Conflict)
                    | Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                        return respond_empty(stream, 409);
                    }
                    Err(_) => return respond_empty(stream, 500),
                }
            }
            CompletionLane::Bidirectional => {
                match outgoing_completion_lease_ready_bytes(
                    &registration.app_root,
                    &device.device_id,
                    lane,
                    &completion.operation_id,
                    &completion.manifest_id,
                ) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) | Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                        return respond_empty(stream, 409);
                    }
                    Err(_) => return respond_empty(stream, 500),
                }
            }
            CompletionLane::Clone => unreachable!("clone preparation rejected above"),
        };
        return respond_json(
            stream,
            200,
            &PeerCompletionPreparedResponse {
                schema: PEER_COMPLETION_PREPARED_SCHEMA.to_owned(),
                transferred_bytes,
            },
        );
    }
    let transferred_bytes = completion
        .transferred_bytes
        .expect("checked completion byte count");
    if lane == CompletionLane::Delta {
        match seal_outgoing_delta_logical_completion(
            &registration.app_root,
            &device.device_id,
            &completion.operation_id,
            &completion.manifest_id,
            transferred_bytes,
        ) {
            Ok(CompletionSealStatus::Sealed | CompletionSealStatus::AlreadyCompleted) => {}
            Ok(CompletionSealStatus::Conflict)
            | Err(PeerSyncError::Validation(_) | PeerSyncError::Protocol(_)) => {
                return respond_empty(stream, 409);
            }
            Err(_) => return respond_empty(stream, 500),
        }
    }
    match accept_outgoing_completion_offer(
        &registration.app_root,
        &device.device_id,
        lane,
        &completion.operation_id,
        &completion.manifest_id,
        transferred_bytes,
    ) {
        Ok(CompletionAcceptance::Recorded | CompletionAcceptance::AlreadyRecorded) => {
            respond_empty(stream, 204)
        }
        Ok(CompletionAcceptance::Rejected) => respond_empty(stream, 409),
        Err(_) => respond_empty(stream, 500),
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn hello(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
) -> Result<(), PeerSyncError> {
    if request.method != "GET"
        || request.range.is_some()
        || request.range_count != 0
        || !request.body.is_empty()
    {
        return respond_empty(stream, 404);
    }
    let device = match authorize(&request, shared) {
        Ok(device) => device,
        Err(status) => return respond_empty(stream, status),
    };
    let Some(registration) = recovered_lock(&shared.v2_registration).clone() else {
        return respond_empty(stream, 404);
    };
    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct LaneDescriptor<'a> {
        session_id: &'a str,
        manifest_id: &'a str,
    }
    #[derive(Serialize)]
    struct Lanes<'a> {
        clone: Option<LaneDescriptor<'a>>,
        delta: Option<LaneDescriptor<'a>>,
        bidirectional: Option<LaneDescriptor<'a>>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Hello<'a> {
        device_id: &'a str,
        name: &'a str,
        permissions: &'a [String],
        lanes: Lanes<'a>,
    }
    let clone = shared
        .sessions
        .iter()
        .find(|session| matches!(session, LanSession::Clone(_)));
    let delta = shared
        .sessions
        .iter()
        .find(|session| matches!(session, LanSession::Logical(_)));
    let bidirectional = shared
        .sessions
        .iter()
        .find(|session| matches!(session, LanSession::BidirectionalLogical(_)));
    let lanes = Lanes {
        clone: clone.map(|session| LaneDescriptor {
            session_id: session.session_id(),
            manifest_id: session.manifest_id(),
        }),
        delta: delta.map(|session| LaneDescriptor {
            session_id: session.session_id(),
            manifest_id: session.manifest_id(),
        }),
        bidirectional: bidirectional.map(|session| LaneDescriptor {
            session_id: session.session_id(),
            manifest_id: session.manifest_id(),
        }),
    };
    respond_json_with_headers(
        stream,
        200,
        &[(
            PEER_COMPLETION_CAPABILITY_HEADER,
            PEER_COMPLETION_CAPABILITY_V1,
        )],
        &Hello {
            device_id: &registration.device_id,
            name: &registration.name,
            permissions: device.permissions.values(),
            lanes,
        },
    )
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanBidirectionalSession {
    pub(crate) session_id: String,
    pub(crate) source_device_id: String,
    pub(crate) target_device_id: String,
}

#[cfg(any(desktop, target_os = "android"))]
pub(crate) trait LanBidirectionalControl: Send + Sync {
    fn register(
        &self,
        session: LanBidirectionalSession,
        request: LanBidirectionalRegistrationRequest,
    ) -> Result<(), PeerSyncError>;

    fn remote_apply(
        &self,
        session: LanBidirectionalSession,
        request: LanBidirectionalRemoteApplyRequest,
        cancellation: &dyn CancellationProbe,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError>;
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalGeneration {
    pub(crate) generation_id: String,
    pub(crate) manifest_hash: String,
    pub(crate) generation_sequence: String,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalGeneration {
    fn is_valid(&self) -> bool {
        !self.generation_id.is_empty()
            && self.generation_id.len() <= MAX_BODY_BYTES
            && is_lower_hex_256(&self.manifest_hash)
            && is_canonical_decimal(&self.generation_sequence)
    }
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalRegistrationRequest {
    pub(crate) library_id: String,
    pub(crate) generation: LanBidirectionalGeneration,
    pub(crate) expected_revision: i64,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalRegistrationRequest {
    fn is_valid(&self) -> bool {
        !self.library_id.is_empty()
            && self.library_id.len() <= MAX_BODY_BYTES
            && self.expected_revision >= 0
            && self.generation.is_valid()
    }
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalRemoteApplyRequest {
    pub(crate) operation_id: String,
    pub(crate) source_endpoint: String,
    pub(crate) source_session_id: String,
    pub(crate) source_manifest_id: String,
    pub(crate) source_claim: String,
    pub(crate) expected_source_revision: i64,
    pub(crate) expected_source_generation: LanBidirectionalGeneration,
    pub(crate) expected_common_base_manifest_hash: String,
    pub(crate) backup_losing_side: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_deferred_v1: Option<bool>,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalRemoteApplyRequest {
    fn is_valid(&self) -> bool {
        is_canonical_uuid(&self.operation_id)
            && validate_p5_desktop_endpoint(&self.source_endpoint).is_ok()
            && is_canonical_uuid(&self.source_session_id)
            && is_lower_hex_256(&self.source_manifest_id)
            && is_lower_hex_256(&self.source_claim)
            && self.expected_source_revision >= 0
            && self.expected_source_generation.is_valid()
            && is_lower_hex_256(&self.expected_common_base_manifest_hash)
    }
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalBackupReceipt {
    pub(crate) package_id: String,
    pub(crate) path: String,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalBackupReceipt {
    fn is_valid(&self) -> bool {
        !self.package_id.is_empty()
            && self.package_id.len() <= MAX_BODY_BYTES
            && !self.path.is_empty()
            && self.path.len() <= MAX_BODY_BYTES
    }
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalRemoteApplyReceipt {
    pub(crate) committed_revision: i64,
    pub(crate) committed_generation: LanBidirectionalGeneration,
    pub(crate) transferred_objects: u64,
    pub(crate) transferred_bytes: u64,
    pub(crate) backup: Option<LanBidirectionalBackupReceipt>,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalRemoteApplyReceipt {
    fn is_valid(&self) -> bool {
        self.committed_revision >= 0
            && self.committed_generation.is_valid()
            && match &self.backup {
                Some(backup) => backup.is_valid(),
                None => true,
            }
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(any(desktop, target_os = "android"))]
struct ClaimResponse<'a> {
    device_id: String,
    bearer: String,
    permission: &'static str,
    source_device_id: &'a str,
    // A session a lane opens for a single operation answers the peer it was handed
    // to and registers nothing, so it carries no source device name.
    #[serde(skip_serializing_if = "Option::is_none")]
    source_device_name: Option<&'a str>,
    permissions: &'a [String],
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponseOwned {
    device_id: String,
    bearer: String,
    permission: String,
    source_device_id: Option<String>,
    source_device_name: Option<String>,
    permissions: Option<Vec<String>>,
}

fn build_clone_http_client(timeout: Duration) -> Result<reqwest::blocking::Client, PeerSyncError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(transport)
}

fn read_claim_response(
    response: reqwest::blocking::Response,
    lane: &str,
) -> Result<ClaimResponseOwned, PeerSyncError> {
    if response.status() != reqwest::StatusCode::OK {
        return Err(PeerSyncError::Transport(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let mut body = Vec::new();
    response
        .take(MAX_CLAIM_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(transport)?;
    if body.len() > MAX_CLAIM_RESPONSE_BYTES {
        return Err(PeerSyncError::Protocol(format!(
            "{lane} claim response is too large"
        )));
    }
    serde_json::from_slice(&body).map_err(|error| PeerSyncError::Protocol(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerHello {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) lanes: PeerHelloLanes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthenticatedPeerHelloOutcome {
    Hello(PeerHello),
    AuthorizationExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerCompletionCapability {
    Unsupported,
    V1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerHelloWithCapabilities {
    pub(crate) hello: PeerHello,
    pub(crate) completion: PeerCompletionCapability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerCompletionDelivery {
    Unsupported,
    Delivered,
}

enum AuthenticatedPeerHelloCapabilitiesOutcome {
    Hello(PeerHelloWithCapabilities),
    AuthorizationExpired,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerHelloLane {
    pub(crate) session_id: String,
    pub(crate) manifest_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PeerHelloLanes {
    pub(crate) clone: Option<PeerHelloLane>,
    pub(crate) delta: Option<PeerHelloLane>,
    pub(crate) bidirectional: Option<PeerHelloLane>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PeerHelloResponse {
    device_id: String,
    name: String,
    permissions: Vec<String>,
    lanes: PeerHelloLanes,
}

pub(crate) fn authenticated_peer_hello(
    endpoint: &str,
    bearer: &str,
) -> Result<PeerHello, PeerSyncError> {
    match authenticated_peer_hello_status(endpoint, bearer)? {
        AuthenticatedPeerHelloOutcome::Hello(hello) => Ok(hello),
        AuthenticatedPeerHelloOutcome::AuthorizationExpired => {
            Err(PeerSyncError::Transport("HTTP 401 Unauthorized".to_owned()))
        }
    }
}

pub(crate) fn authenticated_peer_hello_status(
    endpoint: &str,
    bearer: &str,
) -> Result<AuthenticatedPeerHelloOutcome, PeerSyncError> {
    match authenticated_peer_hello_capabilities_status(endpoint, bearer)? {
        AuthenticatedPeerHelloCapabilitiesOutcome::Hello(observed) => {
            Ok(AuthenticatedPeerHelloOutcome::Hello(observed.hello))
        }
        AuthenticatedPeerHelloCapabilitiesOutcome::AuthorizationExpired => {
            Ok(AuthenticatedPeerHelloOutcome::AuthorizationExpired)
        }
    }
}

pub(crate) fn authenticated_peer_hello_with_capabilities(
    endpoint: &str,
    bearer: &str,
) -> Result<PeerHelloWithCapabilities, PeerSyncError> {
    match authenticated_peer_hello_capabilities_status(endpoint, bearer)? {
        AuthenticatedPeerHelloCapabilitiesOutcome::Hello(observed) => Ok(observed),
        AuthenticatedPeerHelloCapabilitiesOutcome::AuthorizationExpired => {
            Err(PeerSyncError::Transport("HTTP 401 Unauthorized".to_owned()))
        }
    }
}

fn authenticated_peer_hello_capabilities_status(
    endpoint: &str,
    bearer: &str,
) -> Result<AuthenticatedPeerHelloCapabilitiesOutcome, PeerSyncError> {
    let endpoint = validate_lan_endpoint(endpoint)?;
    if !is_lower_hex_256(bearer) {
        return Err(PeerSyncError::Protocol(
            "invalid peer hello credential".to_owned(),
        ));
    }
    let response = build_clone_http_client(CONTROL_REQUEST_TIMEOUT)?
        .get(format!("{endpoint}/v1/peer/hello"))
        .bearer_auth(bearer)
        .send()
        .map_err(transport)?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Ok(AuthenticatedPeerHelloCapabilitiesOutcome::AuthorizationExpired);
    }
    if response.status() != reqwest::StatusCode::OK {
        return Err(PeerSyncError::Transport(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let completion = match response.headers().get(PEER_COMPLETION_CAPABILITY_HEADER) {
        None => PeerCompletionCapability::Unsupported,
        Some(value) if value.as_bytes() == PEER_COMPLETION_CAPABILITY_V1.as_bytes() => {
            PeerCompletionCapability::V1
        }
        Some(_) => {
            return Err(PeerSyncError::Protocol(
                "invalid peer completion capability".to_owned(),
            ));
        }
    };
    let mut body = Vec::new();
    response
        .take(MAX_CLAIM_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(transport)?;
    if body.len() > MAX_CLAIM_RESPONSE_BYTES {
        return Err(PeerSyncError::Protocol(
            "peer hello response is too large".to_owned(),
        ));
    }
    let response: PeerHelloResponse = serde_json::from_slice(&body)
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    let permissions = DevicePermissions::from_values(response.permissions)?;
    if !is_canonical_uuid(&response.device_id)
        || validate_device_name(&response.name).is_err()
        || !valid_hello_lanes(&response.lanes)
    {
        return Err(PeerSyncError::Protocol(
            "invalid peer hello response".to_owned(),
        ));
    }
    Ok(AuthenticatedPeerHelloCapabilitiesOutcome::Hello(
        PeerHelloWithCapabilities {
            hello: PeerHello {
                device_id: response.device_id,
                name: response.name,
                permissions,
                lanes: response.lanes,
            },
            completion,
        },
    ))
}

pub(crate) fn deliver_peer_completion(
    endpoint: &str,
    bearer: &str,
    capability: PeerCompletionCapability,
    lane: CompletionLane,
    operation_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> Result<PeerCompletionDelivery, PeerSyncError> {
    let endpoint = validate_lan_endpoint(endpoint)?;
    if !is_lower_hex_256(bearer)
        || !is_canonical_v4_uuid(operation_id)
        || validate_object_hash(manifest_id).is_err()
    {
        return Err(PeerSyncError::Protocol(
            "invalid peer completion request".to_owned(),
        ));
    }
    if capability == PeerCompletionCapability::Unsupported {
        return Ok(PeerCompletionDelivery::Unsupported);
    }
    let request = PeerCompletionRequest {
        schema: PEER_COMPLETION_SCHEMA.to_owned(),
        lane: lane.as_str().to_owned(),
        operation_id: operation_id.to_owned(),
        manifest_id: manifest_id.to_owned(),
        transferred_bytes: Some(transferred_bytes),
    };
    let response = build_clone_http_client(CONTROL_REQUEST_TIMEOUT)?
        .post(format!("{endpoint}/v1/peer/completion"))
        .bearer_auth(bearer)
        .json(&request)
        .send()
        .map_err(transport)?;
    if response.status() != reqwest::StatusCode::NO_CONTENT {
        return Err(PeerSyncError::Transport(format!(
            "HTTP {}",
            response.status()
        )));
    }
    let mut body = Vec::new();
    response
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(transport)?;
    if !body.is_empty() {
        return Err(PeerSyncError::Protocol(
            "peer completion response must be empty".to_owned(),
        ));
    }
    Ok(PeerCompletionDelivery::Delivered)
}

pub(crate) fn prepare_peer_logical_completion(
    endpoint: &str,
    bearer: &str,
    lane: CompletionLane,
    operation_id: &str,
    manifest_id: &str,
) -> Result<u64, PeerSyncError> {
    let endpoint = validate_lan_endpoint(endpoint)?;
    if !is_lower_hex_256(bearer)
        || !matches!(lane, CompletionLane::Delta | CompletionLane::Bidirectional)
        || !is_canonical_v4_uuid(operation_id)
        || validate_object_hash(manifest_id).is_err()
    {
        return Err(PeerSyncError::Protocol(
            "invalid peer completion preparation request".to_owned(),
        ));
    }
    let request = PeerCompletionRequest {
        schema: PEER_COMPLETION_SCHEMA.to_owned(),
        lane: lane.as_str().to_owned(),
        operation_id: operation_id.to_owned(),
        manifest_id: manifest_id.to_owned(),
        transferred_bytes: None,
    };
    let response = build_clone_http_client(CONTROL_REQUEST_TIMEOUT)?
        .post(format!("{endpoint}/v1/peer/completion"))
        .bearer_auth(bearer)
        .json(&request)
        .send()
        .map_err(transport)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(PeerSyncError::Transport(format!(
            "HTTP {}",
            response.status()
        )));
    }
    if response.content_length().unwrap_or(u64::MAX) > MAX_COMPLETION_PREPARED_RESPONSE_BYTES as u64
    {
        return Err(PeerSyncError::Protocol(
            "peer completion preparation response is too large".to_owned(),
        ));
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        != Some("application/json")
    {
        return Err(PeerSyncError::Protocol(
            "peer completion preparation response is not JSON".to_owned(),
        ));
    }
    let mut body = Vec::new();
    response
        .take(MAX_COMPLETION_PREPARED_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(transport)?;
    if body.len() > MAX_COMPLETION_PREPARED_RESPONSE_BYTES {
        return Err(PeerSyncError::Protocol(
            "peer completion preparation response is too large".to_owned(),
        ));
    }
    let prepared: PeerCompletionPreparedResponse = serde_json::from_slice(&body)
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    if prepared.schema != PEER_COMPLETION_PREPARED_SCHEMA {
        return Err(PeerSyncError::Protocol(
            "invalid peer completion preparation response".to_owned(),
        ));
    }
    Ok(prepared.transferred_bytes)
}

fn valid_hello_lanes(lanes: &PeerHelloLanes) -> bool {
    [&lanes.clone, &lanes.delta, &lanes.bidirectional]
        .into_iter()
        .flatten()
        .all(|lane| is_canonical_uuid(&lane.session_id) && is_lower_hex_256(&lane.manifest_id))
}

#[cfg(any(desktop, target_os = "android"))]
fn validate_device_name(value: &str) -> Result<(), PeerSyncError> {
    if value.is_empty() || value.len() > 256 {
        return Err(PeerSyncError::Validation(
            "invalid peer device name".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressRequest {
    verified_bytes: u64,
    current_object: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verified_object: Option<String>,
}

fn validate_object_hash(object: &str) -> Result<(), PeerSyncError> {
    if is_lower_hex_256(object) {
        Ok(())
    } else {
        Err(PeerSyncError::Protocol(
            "invalid LAN object hash".to_owned(),
        ))
    }
}

#[cfg(all(test, any(desktop, target_os = "android")))]
thread_local! {
    pub(crate) static DISCOVER_LAN_IPV4_OVERRIDE: std::cell::Cell<Option<Ipv4Addr>> =
        const { std::cell::Cell::new(None) };
}

// Discovers this device's private or link-local IPv4 by opening a UDP socket
// toward a TEST-NET address; nothing is sent. Shared by every peer sync lane.
// Android sources bind explicitly selected interfaces instead.
#[cfg(any(desktop, target_os = "android"))]
#[cfg_attr(target_os = "android", allow(dead_code))]
pub(crate) fn discover_lan_ipv4() -> Result<Ipv4Addr, PeerSyncError> {
    #[cfg(test)]
    if let Some(address) = DISCOVER_LAN_IPV4_OVERRIDE.with(std::cell::Cell::take) {
        return Ok(address);
    }
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
    match socket.local_addr()?.ip() {
        std::net::IpAddr::V4(address) if address.is_private() || address.is_link_local() => {
            Ok(address)
        }
        _ => Err(PeerSyncError::Validation(
            "no private IPv4 LAN address is available".to_owned(),
        )),
    }
}

pub(crate) fn validate_lan_endpoint(value: &str) -> Result<String, PeerSyncError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| PeerSyncError::Protocol("invalid LAN endpoint".to_owned()))?;
    let valid_lan_ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => {
            ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback()
        }
        _ => false,
    };
    let valid_public_domain = matches!(
        url.host(),
        Some(url::Host::Domain(host))
            if host.contains('.')
                && !host.starts_with('.')
                && !host.ends_with('.')
                && !host.eq_ignore_ascii_case("localhost")
    );
    let invalid_shape = !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some();
    if invalid_shape {
        return Err(PeerSyncError::Protocol("invalid LAN endpoint".to_owned()));
    }
    if url.scheme() == "http" && valid_lan_ip && has_explicit_authority_port(value) {
        let port = url
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or_else(|| PeerSyncError::Protocol("invalid LAN endpoint".to_owned()))?;
        let host = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
            _ => unreachable!(),
        };
        return Ok(format!("http://{host}:{port}"));
    }
    if url.scheme() == "https"
        && valid_public_domain
        && !has_explicit_authority_port(value)
        && has_bare_authority_path(value)
    {
        return Ok(format!("https://{}", url.host_str().unwrap()));
    }
    Err(PeerSyncError::Protocol("invalid LAN endpoint".to_owned()))
}

pub(crate) fn validate_device_sync_endpoint(value: &str) -> Result<String, PeerSyncError> {
    let endpoint = validate_lan_endpoint(value)?;
    let url = reqwest::Url::parse(&endpoint)
        .map_err(|_| PeerSyncError::Protocol("invalid device sync endpoint".to_owned()))?;
    if matches!(
        url.host(),
        Some(url::Host::Ipv4(ip)) if ip.is_loopback()
    ) || matches!(
        url.host(),
        Some(url::Host::Ipv6(ip)) if ip.is_loopback()
    ) {
        return Err(PeerSyncError::Protocol(
            "invalid device sync endpoint".to_owned(),
        ));
    }
    Ok(endpoint)
}

pub(crate) fn validate_private_lan_endpoint(value: &str) -> Result<String, PeerSyncError> {
    let endpoint = validate_lan_endpoint(value)?;
    if endpoint.starts_with("http://") {
        Ok(endpoint)
    } else {
        Err(PeerSyncError::Protocol(
            "invalid private LAN endpoint".to_owned(),
        ))
    }
}

pub(crate) fn validate_p4_logical_delta_endpoint(value: &str) -> Result<String, PeerSyncError> {
    validate_lan_endpoint(value)
}

#[cfg(any(desktop, target_os = "android"))]
pub(crate) fn validate_p5_desktop_endpoint(value: &str) -> Result<String, PeerSyncError> {
    validate_lan_endpoint(value)
}

fn has_explicit_authority_port(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    let authority = remainder
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if let Some(bracket) = authority.rfind(']') {
        return authority[bracket + 1..].starts_with(':');
    }
    authority.contains(':')
}

fn has_bare_authority_path(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    let Some(path_start) = remainder.find('/') else {
        return true;
    };
    let path = &remainder[path_start..];
    let path_end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..path_end] == "/"
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn clone_endpoint_accepts_private_http_and_bare_public_https() {
        assert_eq!(
            validate_lan_endpoint("http://192.168.1.8:32145").unwrap(),
            "http://192.168.1.8:32145"
        );
        assert_eq!(
            validate_lan_endpoint("http://127.0.0.1:32145/").unwrap(),
            "http://127.0.0.1:32145"
        );
        assert_eq!(
            validate_lan_endpoint("https://sync.example.com/").unwrap(),
            "https://sync.example.com"
        );
        assert_eq!(
            validate_lan_endpoint("https://quick-id.trycloudflare.com").unwrap(),
            "https://quick-id.trycloudflare.com"
        );
    }

    #[test]
    fn canonical_device_sync_endpoint_rejects_loopback_and_accepts_private_ipv6() {
        for endpoint in [
            "http://127.0.0.1:32145",
            "http://127.1:32145",
            "http://127.255.255.254:32145",
            "http://[::1]:32145",
        ] {
            assert!(
                validate_device_sync_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
        }
        for endpoint in [
            "http://10.1.2.3:32145",
            "http://169.254.1.2:32145",
            "http://[fd12:3456::1]:32145",
            "http://[fe80::1234]:32145",
            "https://sync.example.com",
        ] {
            assert!(
                validate_device_sync_endpoint(endpoint).is_ok(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn clone_endpoint_rejects_non_bare_or_non_public_https() {
        for endpoint in [
            "http://sync.example.com:80",
            "https://127.0.0.1",
            "https://localhost",
            "https://sync.example.com:443",
            "https://sync.example.com:8443",
            "https://user@sync.example.com",
            "https://sync.example.com/path",
            "https://sync.example.com/.",
            "https://sync.example.com/a/..",
            "https://sync.example.com/%2e",
            "https://sync.example.com/?query=value",
            "https://sync.example.com/#fragment",
        ] {
            assert!(validate_lan_endpoint(endpoint).is_err(), "{endpoint}");
        }
    }

    #[test]
    fn logical_credentials_reject_public_https_while_clone_keeps_it() {
        let endpoint = "https://sync.example.com";
        assert!(validate_lan_endpoint(endpoint).is_ok());

        let credential = LanBidirectionalLogicalCredential {
            endpoint: endpoint.to_owned(),
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            manifest_id: "a".repeat(64),
            device_id: "00000000-0000-4000-8000-000000000002".to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000003".to_owned(),
            bearer: "b".repeat(64),
        };
        assert!(credential.validate().is_err());
    }

    #[test]
    fn persisted_clone_credentials_require_their_registered_source_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("credential.json");
        let mut credential = serde_json::json!({
            "schema": PERSISTED_CREDENTIAL_SCHEMA,
            "endpoint": "http://127.0.0.1:32145",
            "sessionId": "00000000-0000-4000-8000-000000000001",
            "manifestId": "a".repeat(64),
            "deviceId": "00000000-0000-4000-8000-000000000002",
            "bearer": "b".repeat(64),
            "sourceDeviceId": "00000000-0000-4000-8000-000000000003",
            "permission": "clone-read",
        });
        fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
        LanCloneClient::open_persisted(&path).unwrap();

        credential
            .as_object_mut()
            .unwrap()
            .remove("sourceDeviceId")
            .unwrap();
        fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
        assert!(LanCloneClient::open_persisted(&path).is_err());
    }

    #[test]
    fn persisted_credential_corruption_reads_content_and_never_a_read_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("credential.json");
        let credential = serde_json::json!({
            "schema": PERSISTED_CREDENTIAL_SCHEMA,
            "endpoint": "http://127.0.0.1:32145",
            "sessionId": "00000000-0000-4000-8000-000000000001",
            "manifestId": "a".repeat(64),
            "deviceId": "00000000-0000-4000-8000-000000000002",
            "bearer": "b".repeat(64),
            "sourceDeviceId": "00000000-0000-4000-8000-000000000003",
            "permission": "clone-read",
        });

        assert_eq!(
            LanCloneClient::persisted_credential_corruption(&path).unwrap(),
            Some(PersistedCredentialCorruption::Missing)
        );

        fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
        assert_eq!(
            LanCloneClient::persisted_credential_corruption(&path).unwrap(),
            None
        );

        fs::write(&path, b"not a credential").unwrap();
        assert_eq!(
            LanCloneClient::persisted_credential_corruption(&path).unwrap(),
            Some(PersistedCredentialCorruption::Unparsable)
        );

        let mut without_source = credential.clone();
        without_source
            .as_object_mut()
            .unwrap()
            .remove("sourceDeviceId")
            .unwrap();
        fs::write(&path, serde_json::to_vec(&without_source).unwrap()).unwrap();
        assert_eq!(
            LanCloneClient::persisted_credential_corruption(&path).unwrap(),
            Some(PersistedCredentialCorruption::Unparsable)
        );

        let mut foreign_endpoint = credential.clone();
        foreign_endpoint["endpoint"] = serde_json::json!("http://8.8.8.8:32145");
        fs::write(&path, serde_json::to_vec(&foreign_endpoint).unwrap()).unwrap();
        assert_eq!(
            LanCloneClient::persisted_credential_corruption(&path).unwrap(),
            Some(PersistedCredentialCorruption::Invalid)
        );

        // A directory in the credential's place fails every read deterministically
        // on both Windows and Unix without being a corruption verdict.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(LanCloneClient::persisted_credential_corruption(&path).is_err());
    }

    #[test]
    fn p4_delta_policy_accepts_private_lan_and_only_canonical_public_https() {
        assert_eq!(
            validate_p4_logical_delta_endpoint("http://192.168.1.2:8080").unwrap(),
            "http://192.168.1.2:8080"
        );
        assert_eq!(
            validate_p4_logical_delta_endpoint("https://sync.example.com/").unwrap(),
            "https://sync.example.com"
        );
        for endpoint in [
            "https://sync.example.com:443",
            "https://sync.example.com/path",
            "https://localhost",
            "https://127.0.0.1",
        ] {
            assert!(
                validate_p4_logical_delta_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn p5_claim_rejects_public_https_before_transport() {
        let failure = match LanBidirectionalLogicalClient::claim(
            "https://sync.example.invalid",
            "00000000-0000-4000-8000-000000000001",
            &"a".repeat(64),
            &"b".repeat(64),
            "00000000-0000-4000-8000-000000000002",
        ) {
            Ok(_) => panic!("P5 accepted a public HTTPS endpoint"),
            Err(failure) => failure,
        };
        assert!(matches!(failure, PeerSyncError::Protocol(_)), "{failure}");
    }

    #[test]
    fn desktop_p5_policy_accepts_private_lan_and_strict_public_https() {
        assert_eq!(
            validate_p5_desktop_endpoint("http://192.168.1.2:8080").unwrap(),
            "http://192.168.1.2:8080"
        );
        assert_eq!(
            validate_p5_desktop_endpoint("https://sync.example.com/").unwrap(),
            "https://sync.example.com"
        );
        for endpoint in [
            "https://sync.example.com:443",
            "https://sync.example.com/path",
            "https://localhost",
            "https://127.0.0.1",
        ] {
            assert!(
                validate_p5_desktop_endpoint(endpoint).is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn bidirectional_remote_apply_accepts_strict_public_https_source() {
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: "00000000-0000-4000-8000-000000000004".to_owned(),
            source_endpoint: "https://sync.example.com".to_owned(),
            source_session_id: "00000000-0000-4000-8000-000000000005".to_owned(),
            source_manifest_id: "c".repeat(64),
            source_claim: "d".repeat(64),
            expected_source_revision: 1,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: "generation-1".to_owned(),
                manifest_hash: "e".repeat(64),
                generation_sequence: "1".to_owned(),
            },
            expected_common_base_manifest_hash: "f".repeat(64),
            backup_losing_side: false,
            completion_deferred_v1: None,
        };
        assert!(request.is_valid());
    }
}

fn is_lower_hex_256(value: &str) -> bool {
    crate::trust_boundary::is_lower_hex_256(value)
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn is_canonical_v4_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| {
            parsed.get_version_num() == 4
                && parsed.get_variant() == uuid::Variant::RFC4122
                && parsed.to_string() == value
        })
        .unwrap_or(false)
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn ensure_credential_parent(path: &Path) -> Result<(), PeerSyncError> {
    #[cfg(unix)]
    let created = !path.exists();
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(PeerSyncError::Storage(
            "invalid LAN clone credential directory".to_owned(),
        ));
    }
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn snapshot_credential(path: &Path) -> Result<Option<Vec<u8>>, PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_PERSISTED_CREDENTIAL_BYTES
    {
        return Err(PeerSyncError::Storage(
            "invalid existing LAN clone credential".to_owned(),
        ));
    }
    Ok(Some(fs::read(path)?))
}

fn restore_credential(path: &Path, previous: Option<&[u8]>) -> Result<(), PeerSyncError> {
    match previous {
        Some(bytes) => write_credential_bytes_atomic(path, bytes),
        None => match fs::remove_file(path) {
            Ok(()) => path
                .parent()
                .ok_or_else(|| {
                    PeerSyncError::Storage("LAN clone credential path has no parent".to_owned())
                })
                .and_then(sync_parent_directory),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        },
    }
}

fn write_credential_bytes_atomic(
    credential_path: &Path,
    bytes: &[u8],
) -> Result<(), PeerSyncError> {
    if bytes.len() as u64 > MAX_PERSISTED_CREDENTIAL_BYTES {
        return Err(PeerSyncError::Storage(
            "persisted LAN clone credential is too large".to_owned(),
        ));
    }
    let parent = credential_path.parent().ok_or_else(|| {
        PeerSyncError::Storage("LAN clone credential path has no parent".to_owned())
    })?;
    ensure_credential_parent(parent)?;
    let temporary = parent.join(format!(".peer-credential-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = create_owner_only_credential_file(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace_credential_atomic(&temporary, credential_path)?;
        sync_parent_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_owner_only_credential_file(path: &Path) -> Result<File, PeerSyncError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(windows)]
fn replace_credential_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_credential_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
}

#[cfg(any(desktop, target_os = "android"))]
fn random_secret() -> Result<[u8; 32], PeerSyncError> {
    let mut secret = [0; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
    Ok(secret)
}
#[cfg(any(desktop, target_os = "android"))]
fn digest(bytes: impl AsRef<[u8]>) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
#[cfg(any(desktop, target_os = "android"))]
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}
#[cfg(any(desktop, target_os = "android"))]
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
#[cfg(any(desktop, target_os = "android"))]
fn parse_range(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || value.contains(char::is_whitespace) {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?)).filter(|(start, end)| start <= end)
}
fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}
fn transport(error: impl std::fmt::Display) -> PeerSyncError {
    PeerSyncError::Transport(error.to_string())
}

#[cfg(any(desktop, target_os = "android"))]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
#[cfg(any(desktop, target_os = "android"))]
fn valid_header_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}
#[cfg(any(desktop, target_os = "android"))]
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}
#[cfg(any(desktop, target_os = "android"))]
fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        410 => "Gone",
        413 => "Content Too Large",
        416 => "Range Not Satisfiable",
        _ => "Error",
    }
}
#[cfg(any(desktop, target_os = "android"))]
fn write_response_head(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    content_length: u64,
) -> Result<(), PeerSyncError> {
    let mut response = String::with_capacity(512);
    write!(
        response,
        "HTTP/1.1 {status} {}\r\nContent-Length: {content_length}\r\nConnection: close\r\n",
        status_reason(status)
    )
    .map_err(transport)?;
    for (name, value) in headers {
        if !valid_header_name(name) || value.contains(['\r', '\n']) {
            return Err(PeerSyncError::Protocol(
                "invalid HTTP response header".to_owned(),
            ));
        }
        write!(response, "{name}: {value}\r\n").map_err(transport)?;
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).map_err(transport)
}
#[cfg(any(desktop, target_os = "android"))]
fn respond_empty(stream: &mut TcpStream, status: u16) -> Result<(), PeerSyncError> {
    write_response_head(stream, status, &[], 0)
}
#[cfg(any(desktop, target_os = "android"))]
fn respond_bytes(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(), PeerSyncError> {
    write_response_head(stream, status, headers, body.len() as u64)?;
    stream.write_all(body).map_err(transport)
}
#[cfg(any(desktop, target_os = "android"))]
fn respond_json<T: Serialize>(
    stream: &mut TcpStream,
    status: u16,
    value: &T,
) -> Result<(), PeerSyncError> {
    respond_json_with_headers(stream, status, &[], value)
}

#[cfg(any(desktop, target_os = "android"))]
fn respond_json_with_headers<T: Serialize>(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    value: &T,
) -> Result<(), PeerSyncError> {
    let body =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    let mut response_headers = Vec::with_capacity(headers.len() + 1);
    response_headers.push(("Content-Type", "application/json"));
    response_headers.extend_from_slice(headers);
    respond_bytes(stream, status, &response_headers, &body)
}
#[cfg(any(desktop, target_os = "android"))]
fn copy_exact_response(
    stream: &mut TcpStream,
    reader: &mut dyn Read,
    size: u64,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    let mut remaining = size;
    let mut buffer = [0_u8; RESPONSE_COPY_BUFFER_BYTES];
    while remaining != 0 {
        if stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        let expected = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..expected])?;
        if read == 0 {
            return Err(PeerSyncError::Storage(
                "LAN object ended before its manifest size".to_owned(),
            ));
        }
        if let Err(error) = stream.write_all(&buffer[..read]) {
            return if stopped.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(transport(error))
            };
        }
        remaining -= read as u64;
    }
    Ok(())
}

#[cfg(all(test, desktop))]
mod timeout_tests {
    use super::*;
    use crate::peer_sync::device_registry::{
        issue_outgoing_measured_completion_offer, seal_outgoing_completion_lease, CompletionLane,
        CompletionSealStatus, DevicePermissions, IncomingSourceRegistry, OutgoingDeviceRegistry,
    };
    use crate::peer_sync::logical_completion::fail_next_logical_completion_proof_write_for_test;
    use crate::peer_sync::{
        logical_delta::{
            build_logical_manifest, LogicalManifestBuilderInput, LogicalRecordEnvelope,
            LogicalRecordLocator, ProjectedLogicalRecord,
        },
        LogicalDeltaObject, LogicalDeltaObjectSource,
    };
    use std::{collections::BTreeMap, io::Cursor, sync::mpsc};

    static LOGICAL_LAN_TEST_LOCK: Mutex<()> = Mutex::new(());

    const TEST_SESSION_ID: &str = "00000000-0000-4000-8000-000000000000";
    const TEST_BEARER: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const TEST_P5_CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
    const TEST_P5_OBJECT_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn completion_operation_ids_require_the_rfc4122_variant() {
        assert!(!is_canonical_v4_uuid(
            "00000000-0000-4000-0000-000000000001"
        ));
    }

    fn read_request_head(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0);
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(request).unwrap()
    }

    fn direct_client(address: SocketAddr) -> LanCloneClient {
        let endpoint = format!("http://{address}");
        let session_url = format!("{endpoint}/v1/sessions/{TEST_SESSION_ID}");
        LanCloneClient {
            client: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(1))
                .timeout(Duration::from_secs(1))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .unwrap(),
            ranges: HttpRangeStream::new(Duration::from_secs(1)).unwrap(),
            control_timeout: Duration::from_secs(1),
            endpoint,
            session_id: TEST_SESSION_ID.to_owned(),
            session_url,
            device_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            bearer: TEST_BEARER.to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000002".to_owned(),
            manifest_id: None,
        }
    }

    fn direct_logical_client(
        address: SocketAddr,
        control_timeout: Duration,
        object_idle_timeout: Duration,
    ) -> LanLogicalDeltaClient {
        let session_url = format!("http://{address}/v1/sessions/{TEST_SESSION_ID}");
        let timeouts = LogicalClientTimeouts {
            control_request: control_timeout,
            object_idle: object_idle_timeout,
        };
        LanLogicalDeltaClient {
            control_client: build_logical_http_client(LogicalRequestKind::Control, timeouts)
                .unwrap(),
            object_client: build_logical_http_client(LogicalRequestKind::Object, timeouts).unwrap(),
            control_timeout,
            session_url,
            device_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            bearer: TEST_BEARER.to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000002".to_owned(),
            manifest_id: "a".repeat(64),
            progress: Arc::new(Mutex::new(LogicalClientProgress::default())),
        }
    }

    fn direct_bidirectional_client(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        device_id: &str,
    ) -> Result<LanBidirectionalLogicalClient, PeerSyncError> {
        let inner = LanLogicalDeltaClient::claim_with_timeouts_and_device(
            endpoint,
            session_id,
            manifest_id,
            claim,
            device_id,
            "logical-bidirectional",
            LogicalClientTimeouts {
                control_request: TEST_P5_CONTROL_TIMEOUT,
                object_idle: TEST_P5_OBJECT_IDLE_TIMEOUT,
            },
        )?;
        Ok(LanBidirectionalLogicalClient {
            remote_apply_client: build_bidirectional_remote_apply_client()?,
            endpoint: validate_private_lan_endpoint(endpoint)?,
            session_id: session_id.to_owned(),
            inner,
        })
    }

    /// Answers one claim request with a fixed body, so a client can be pointed at a
    /// response shape the source itself would never produce.
    fn canned_claim_response_server(
        response: serde_json::Value,
    ) -> (String, mpsc::Receiver<usize>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (calls_tx, calls_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("POST "));
            calls_tx.send(1).unwrap();
            let body = serde_json::to_vec(&response).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
            finish_response(&mut stream);
        });
        (endpoint, calls_rx, server)
    }

    fn rejecting_v2_claim_server() -> (String, mpsc::Receiver<usize>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (calls_tx, calls_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut rejected, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut rejected).starts_with("POST "));
            calls_tx.send(1).unwrap();
            rejected
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            rejected.flush().unwrap();
            finish_response(&mut rejected);

            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut unexpected, _)) => {
                        assert!(read_request_head(&mut unexpected).starts_with("POST "));
                        calls_tx.send(2).unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("claim server accept failed: {error}"),
                }
            }
        });
        (endpoint, calls_rx, server)
    }

    fn mismatched_clone_hello_server(
        target_device_id: String,
        claimed_source_device_id: String,
        hello_source_device_id: String,
        permission: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut claim, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut claim).starts_with("POST "));
            let body = serde_json::to_vec(&serde_json::json!({
                "deviceId": target_device_id,
                "bearer": "c".repeat(64),
                "permission": permission,
                "sourceDeviceId": claimed_source_device_id,
                "sourceDeviceName": "Claimed source",
                "permissions": ["read"]
            }))
            .unwrap();
            write!(
                claim,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            claim.write_all(&body).unwrap();
            claim.flush().unwrap();
            finish_response(&mut claim);

            let (mut hello, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut hello).starts_with("GET /v1/peer/hello "));
            let body = serde_json::to_vec(&serde_json::json!({
                "deviceId": hello_source_device_id,
                "name": "Different source",
                "permissions": ["read"],
                "lanes": {
                    "clone": {
                        "sessionId": TEST_SESSION_ID,
                        "manifestId": "a".repeat(64)
                    },
                    "delta": null,
                    "bidirectional": null
                }
            }))
            .unwrap();
            write!(
                hello,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            hello.write_all(&body).unwrap();
            hello.flush().unwrap();
            finish_response(&mut hello);
        });
        (endpoint, server)
    }

    #[test]
    fn clone_v2_registration_rejects_hello_with_a_different_claimed_source_identity() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let target_device_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let claimed_source_device_id = "00000000-0000-4000-8000-000000000110";
        let hello_source_device_id = "00000000-0000-4000-8000-000000000111";
        let (endpoint, server) = mismatched_clone_hello_server(
            target_device_id,
            claimed_source_device_id.to_owned(),
            hello_source_device_id.to_owned(),
            "clone-read",
        );
        let credential = target_root.path().join("clone-credential.json");

        let result = LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Android",
            &credential,
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );

        assert!(matches!(
            result,
            Err(PeerSyncError::Protocol(message))
                if message.contains("source device identity")
        ));
        assert!(!credential.exists());
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
        server.join().unwrap();
    }

    #[test]
    fn logical_v2_registration_rejects_hello_with_a_different_claimed_source_identity() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let target_device_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let (endpoint, server) = mismatched_clone_hello_server(
            target_device_id,
            "00000000-0000-4000-8000-000000000112".to_owned(),
            "00000000-0000-4000-8000-000000000113".to_owned(),
            "logical-read",
        );

        let result = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );

        assert!(matches!(
            result,
            Err(PeerSyncError::Protocol(message))
                if message.contains("source device identity")
        ));
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
        server.join().unwrap();
    }

    #[test]
    fn clone_claim_reports_peer_outdated_on_a_bad_request_without_persisting() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let credential = target_root.path().join("strict-v2-credential.json");
        let (endpoint, calls, server) = rejecting_v2_claim_server();

        let result = LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Windows target",
            &credential,
            &endpoint,
            "00000000-0000-4000-8000-000000000097",
            &"a".repeat(64),
            &"b".repeat(64),
        );

        assert!(matches!(
            result.err(),
            Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
        ));
        assert_eq!(calls.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        server.join().unwrap();
        assert!(calls.try_recv().is_err());
        assert!(!credential.exists());
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
    }

    #[test]
    fn logical_claim_reports_peer_outdated_on_a_bad_request_without_registering() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let (endpoint, calls, server) = rejecting_v2_claim_server();

        let result = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Windows target",
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );

        assert!(matches!(
            result.err(),
            Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
        ));
        assert_eq!(calls.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
        server.join().unwrap();
        assert!(calls.try_recv().is_err());
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
    }

    #[test]
    fn logical_claim_reports_peer_outdated_when_the_response_omits_registered_identity() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let device_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();

        let (endpoint, _calls, server) = canned_claim_response_server(serde_json::json!({
            "deviceId": device_id,
            "bearer": "c".repeat(64),
            "permission": "logical-read",
            "sourceDeviceName": "Source",
            "permissions": ["read"],
        }));
        let missing_source = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Windows target",
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );
        server.join().unwrap();

        assert!(matches!(
            missing_source.err(),
            Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
        ));
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
    }

    #[test]
    fn single_operation_lane_claim_reports_peer_outdated_without_registered_identity() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let device_id = "00000000-0000-4000-8000-000000000078";

        for response in [
            serde_json::json!({
                "deviceId": device_id,
                "bearer": "c".repeat(64),
                "permission": "logical-read",
                "permissions": ["read"],
            }),
            serde_json::json!({
                "deviceId": device_id,
                "bearer": "c".repeat(64),
                "permission": "logical-read",
                "sourceDeviceId": "00000000-0000-4000-8000-000000000079",
            }),
            serde_json::json!({
                "deviceId": device_id,
                "bearer": "c".repeat(64),
                "permission": "logical-read",
                "sourceDeviceId": "00000000-0000-4000-8000-000000000079",
                "permissions": ["write"],
            }),
        ] {
            let (endpoint, _calls, server) = canned_claim_response_server(response);
            let claimed = LanLogicalDeltaClient::claim_with_timeouts_and_device(
                &endpoint,
                TEST_SESSION_ID,
                &"a".repeat(64),
                &"b".repeat(64),
                device_id,
                "logical-read",
                LogicalClientTimeouts {
                    control_request: CONTROL_REQUEST_TIMEOUT,
                    object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
                },
            );
            server.join().unwrap();

            assert!(matches!(
                claimed.err(),
                Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
            ));
        }
    }

    #[test]
    fn clone_claim_reports_peer_outdated_when_the_response_omits_registered_identity() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let credential = target_root.path().join("clone-credential.json");
        let device_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();

        let (endpoint, _calls, server) = canned_claim_response_server(serde_json::json!({
            "deviceId": device_id,
            "bearer": "c".repeat(64),
            "permission": "clone-read",
            "sourceDeviceName": "Source",
            "permissions": ["read"],
        }));
        let missing_source = LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Windows target",
            &credential,
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );
        server.join().unwrap();

        assert!(matches!(
            missing_source.err(),
            Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
        ));

        let (endpoint, _calls, server) = canned_claim_response_server(serde_json::json!({
            "deviceId": device_id,
            "bearer": "c".repeat(64),
            "permission": "clone-read",
            "sourceDeviceId": "00000000-0000-4000-8000-000000000131",
            "sourceDeviceName": "Source",
        }));
        let missing_permissions = LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Windows target",
            &credential,
            &endpoint,
            TEST_SESSION_ID,
            &"a".repeat(64),
            &"b".repeat(64),
        );
        server.join().unwrap();

        assert!(matches!(
            missing_permissions.err(),
            Some(PeerSyncError::Validation(code)) if code == PEER_OUTDATED
        ));
        assert!(!credential.exists());
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
    }

    fn finish_response(stream: &mut TcpStream) {
        let _ = stream.shutdown(Shutdown::Write);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut drain = [0_u8; 64];
        while stream.read(&mut drain).is_ok_and(|read| read != 0) {}
    }

    fn is_reqwest_timeout(error: &io::Error) -> bool {
        error
            .get_ref()
            .and_then(|source| source.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
    }

    #[test]
    fn socket_abort_is_not_a_logical_object_idle_timeout() {
        let socket_abort = io::Error::other(io::Error::from_raw_os_error(10053));

        assert!(!is_reqwest_timeout(&socket_abort));
    }

    #[test]
    fn accepted_connection_keeps_short_read_polls_and_allows_slow_response_progress() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();

        configure_connection(&server).unwrap();

        assert_eq!(
            server.read_timeout().unwrap(),
            Some(CONNECTION_READ_POLL_TIMEOUT)
        );
        assert_eq!(
            server.write_timeout().unwrap(),
            Some(RESPONSE_WRITE_TIMEOUT)
        );
        assert!(RESPONSE_WRITE_TIMEOUT > CONNECTION_READ_POLL_TIMEOUT);
        drop(client);
    }

    #[test]
    fn stalled_claim_uses_the_bounded_control_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        // The server holds the claim response until the client has already failed, so
        // the error can only come from the bounded control timeout (the socket provably
        // never responded or closed), independent of scheduling.
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            configure_connection(&stream).unwrap();
            // Best effort: an already-timed-out client may abort mid-request.
            let _ = read_request(&mut stream, &AtomicBool::new(false));
            let _ = hold_rx.recv();
        });
        let started = Instant::now();

        let error = LanCloneClient::claim_v2_with_device(
            &endpoint,
            "00000000-0000-4000-8000-000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "00000000-0000-4000-8000-000000000001",
            "Target",
        )
        .err()
        .unwrap();
        let elapsed = started.elapsed();
        let _ = hold_tx.send(());
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Transport(_)));
        // Generous hang guard only; the held socket proves a bounded timeout fired.
        assert!(elapsed < CONTROL_REQUEST_TIMEOUT * 4);
    }

    #[test]
    fn logical_object_body_stall_uses_a_bounded_idle_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bytes = b"stalled logical object".to_vec();
        let object_hash = sha256_hex(&bytes);
        let object_size = bytes.len() as u64;
        let response_etag = quoted(&object_hash);
        let address = listener.local_addr().unwrap();
        // The server holds the body back until the client has already failed, so the
        // error can only come from the bounded object idle timeout (the socket provably
        // never progressed or closed), independent of scheduling.
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("GET "));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            stream.flush().unwrap();
            let _ = hold_rx.recv();
            let _ = stream.write_all(&bytes);
        });
        let mut client =
            direct_logical_client(address, Duration::from_millis(500), Duration::from_secs(2));
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object_hash,
                size: object_size,
            })
            .unwrap();
        let started = Instant::now();
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        let elapsed = started.elapsed();
        let _ = hold_tx.send(());
        server.join().unwrap();

        assert!(
            is_reqwest_timeout(&error),
            "unexpected read error: {error:?}"
        );
        // Generous hang guard only; the held body proves the idle timeout fired.
        assert!(
            elapsed < Duration::from_secs(10),
            "stall lasted {elapsed:?}"
        );
    }

    #[test]
    fn logical_object_can_progress_longer_than_the_control_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bytes = b"logical object with paced progress".to_vec();
        let object_hash = sha256_hex(&bytes);
        let response_etag = quoted(&object_hash);
        let server_bytes = bytes.clone();
        let address = listener.local_addr().unwrap();
        let control_timeout = Duration::from_millis(100);
        // The server holds the second body half until this thread has provably let more
        // than the control timeout elapse, so the successful read demonstrates that
        // object streams outlive the short control timeout. Sleeping before the read
        // only ever extends the gap, so scheduling can not break the lower bound, and
        // the object timeout stays far above any plausible scheduler stall.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("GET "));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n",
                server_bytes.len()
            )
            .unwrap();
            stream.flush().unwrap();
            let (first_half, second_half) = server_bytes.split_at(server_bytes.len() / 2);
            stream.write_all(first_half).unwrap();
            stream.flush().unwrap();
            let _ = release_rx.recv();
            stream.write_all(second_half).unwrap();
            stream.flush().unwrap();
            finish_response(&mut stream);
        });
        let mut client = direct_logical_client(address, control_timeout, Duration::from_secs(10));
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object_hash,
                size: bytes.len() as u64,
            })
            .unwrap();
        let started = Instant::now();
        thread::sleep(control_timeout + Duration::from_millis(100));
        release_tx.send(()).unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert_eq!(received, bytes);
        assert!(elapsed > control_timeout);
    }

    #[test]
    fn manifest_requires_the_exact_ok_status() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = direct_client(listener.local_addr().unwrap());
        let body = b"manifest";
        let manifest_id = sha256_hex(body);
        let response_etag = quoted(&manifest_id);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("GET "));
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Length: {}\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
            finish_response(&mut stream);
        });

        let error = client.fetch_manifest(&manifest_id).unwrap_err();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Transport(_)));
    }

    #[test]
    fn clone_manifest_completion_opt_in_safely_falls_back_when_legacy_server_omits_headers() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let session_id = "00000000-0000-4000-8000-000000000120";
        let manifest_bytes = br#"{"objects":{}}"#.to_vec();
        let manifest_id = sha256_hex(&manifest_bytes);
        let response_manifest_id = manifest_id.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(
                request.starts_with(&format!("GET /v1/sessions/{session_id}/manifest HTTP/1.1"))
            );
            assert!(request
                .to_ascii_lowercase()
                .contains("risunest-peer-completion: v1\r\n"));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"{response_manifest_id}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                manifest_bytes.len()
            )
            .unwrap();
            stream.write_all(&manifest_bytes).unwrap();
            stream.flush().unwrap();
            finish_response(&mut stream);
        });
        let client = LanCloneClient {
            client: build_clone_http_client(CONTROL_REQUEST_TIMEOUT).unwrap(),
            ranges: HttpRangeStream::new(Duration::from_secs(3)).unwrap(),
            control_timeout: CONTROL_REQUEST_TIMEOUT,
            endpoint: endpoint.clone(),
            session_id: session_id.to_owned(),
            session_url: format!("{endpoint}/v1/sessions/{session_id}"),
            device_id: "00000000-0000-4000-8000-000000000121".to_owned(),
            bearer: TEST_BEARER.to_owned(),
            source_device_id: "00000000-0000-4000-8000-000000000122".to_owned(),
            manifest_id: Some(manifest_id.clone()),
        };

        let observed = client
            .fetch_manifest_request(&manifest_id, true, None)
            .unwrap();
        server.join().unwrap();
        assert!(observed.completion_lease_id.is_none());
    }

    #[test]
    fn object_head_requires_the_exact_ok_status() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = direct_client(listener.local_addr().unwrap());
        let object = "a".repeat(64);
        let response_etag = quoted(&object);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("HEAD "));
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 1\r\nAccept-Ranges: bytes\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            finish_response(&mut stream);
        });

        let error = client.head_object(&object).unwrap_err();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Transport(_)));
    }

    #[test]
    fn object_head_requires_the_byte_range_contract() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = direct_client(listener.local_addr().unwrap());
        let object = "a".repeat(64);
        let response_etag = quoted(&object);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut stream).starts_with("HEAD "));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            finish_response(&mut stream);
        });

        let error = client.head_object(&object).unwrap_err();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Protocol(_)), "{error:?}");
    }

    #[test]
    fn direct_range_requires_the_total_from_object_head() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = direct_client(listener.local_addr().unwrap());
        let object = "a".repeat(64);
        let response_etag = quoted(&object);
        let server = thread::spawn(move || loop {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            if request.starts_with("HEAD ") {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nAccept-Ranges: bytes\r\nETag: {response_etag}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                finish_response(&mut stream);
                continue;
            }
            assert!(request.starts_with("GET "));
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/3\r\nETag: {response_etag}\r\nConnection: close\r\n\r\nx"
            )
            .unwrap();
            finish_response(&mut stream);
            break;
        });

        let error = client.fetch_chunk(&object, 0, 0).unwrap_err();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Protocol(_)), "{error:?}");
    }

    struct LogicalFixtureSource(BTreeMap<String, Vec<u8>>);

    impl LogicalDeltaObjectSource for LogicalFixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            let bytes = self.0.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Storage("logical fixture object is absent".to_owned())
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    // Holds the object body until the test releases it, so an open that waited
    // for body production would provably hang instead of racing a sleep.
    struct HeldLogicalFixtureSource {
        object_hash: String,
        bytes: Vec<u8>,
        releases: std::collections::VecDeque<mpsc::Receiver<()>>,
    }

    impl LogicalDeltaObjectSource for HeldLogicalFixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            if object.hash != self.object_hash {
                return Err(PeerSyncError::Storage(
                    "held logical fixture object is absent".to_owned(),
                ));
            }
            Ok(Box::new(HeldFirstRead {
                inner: Cursor::new(self.bytes.clone()),
                release: self.releases.pop_front(),
            }))
        }
    }

    struct HeldFirstRead {
        inner: Cursor<Vec<u8>>,
        release: Option<mpsc::Receiver<()>>,
    }

    impl Read for HeldFirstRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if let Some(release) = self.release.take() {
                let _ = release.recv();
            }
            self.inner.read(output)
        }
    }

    #[derive(Default)]
    struct BidirectionalControlFixture {
        registrations: Mutex<Vec<LanBidirectionalSession>>,
        remote_apply_delay: Mutex<Option<Duration>>,
        remote_apply_error: Mutex<Option<PeerSyncError>>,
        remote_apply_started: Mutex<Option<mpsc::Sender<()>>>,
        remote_apply_bytes: Mutex<u64>,
        remote_apply_calls: Mutex<usize>,
        remote_apply_commits: Mutex<usize>,
        remote_apply_receipts: Mutex<BTreeMap<String, LanBidirectionalRemoteApplyReceipt>>,
    }

    impl LanBidirectionalControl for BidirectionalControlFixture {
        fn register(
            &self,
            session: LanBidirectionalSession,
            _request: LanBidirectionalRegistrationRequest,
        ) -> Result<(), PeerSyncError> {
            self.registrations.lock().unwrap().push(session);
            Ok(())
        }

        fn remote_apply(
            &self,
            _session: LanBidirectionalSession,
            request: LanBidirectionalRemoteApplyRequest,
            cancellation: &dyn CancellationProbe,
        ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
            *self.remote_apply_calls.lock().unwrap() += 1;
            if let Some(receipt) = self
                .remote_apply_receipts
                .lock()
                .unwrap()
                .get(&request.operation_id)
                .cloned()
            {
                return Ok(receipt);
            }
            if let Some(started) = self.remote_apply_started.lock().unwrap().take() {
                started.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while !cancellation.is_cancelled() {
                    if Instant::now() >= deadline {
                        return Err(PeerSyncError::Storage(
                            "fixture did not receive source cancellation".to_owned(),
                        ));
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                return Err(PeerSyncError::Cancelled);
            }
            if let Some(delay) = self.remote_apply_delay.lock().unwrap().take() {
                thread::sleep(delay);
            }
            if let Some(error) = self.remote_apply_error.lock().unwrap().take() {
                return Err(error);
            }
            let receipt = LanBidirectionalRemoteApplyReceipt {
                committed_revision: request.expected_source_revision,
                committed_generation: request.expected_source_generation,
                transferred_objects: 0,
                transferred_bytes: *self.remote_apply_bytes.lock().unwrap(),
                backup: None,
            };
            *self.remote_apply_commits.lock().unwrap() += 1;
            self.remote_apply_receipts
                .lock()
                .unwrap()
                .insert(request.operation_id, receipt.clone());
            Ok(receipt)
        }
    }

    #[test]
    fn p5_source_stop_cancels_remote_apply_and_joins_the_host() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = Arc::new(BidirectionalControlFixture::default());
        let (started_tx, started_rx) = mpsc::channel();
        *control.remote_apply_started.lock().unwrap() = Some(started_tx);
        let session_id = "00000000-0000-4000-8000-000000000082";
        let source_device_id = "00000000-0000-4000-8000-000000000083";
        let target_device_id = "00000000-0000-4000-8000-000000000084";
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                session_id,
                source_device_id,
                Arc::clone(&control),
            ));
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanBidirectionalLogicalClient::claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            target_device_id,
        )
        .unwrap();
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: "00000000-0000-4000-8000-000000000085".to_owned(),
            source_endpoint: endpoint,
            source_session_id: pairing.session_id,
            source_manifest_id: pairing.manifest_id.clone(),
            source_claim: pairing.claim,
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: "generation-1".to_owned(),
                manifest_hash: pairing.manifest_id.clone(),
                generation_sequence: "1".to_owned(),
            },
            expected_common_base_manifest_hash: pairing.manifest_id,
            backup_losing_side: false,
            completion_deferred_v1: None,
        };
        let requester = thread::spawn(move || client.request_remote_apply(request));
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let stop_started = Instant::now();
        host.stop().unwrap();

        assert!(stop_started.elapsed() < Duration::from_secs(1));
        assert!(requester.join().unwrap().is_err());
    }

    fn prepared_bidirectional_logical_session(
        session_id: &str,
        source_device_id: &str,
        control: Arc<BidirectionalControlFixture>,
    ) -> PreparedBidirectionalLogicalLanSession {
        prepared_bidirectional_logical_session_with_generation(
            session_id,
            source_device_id,
            "generation-1",
            control,
        )
    }

    fn prepared_bidirectional_logical_session_with_generation(
        session_id: &str,
        source_device_id: &str,
        generation: &str,
        control: Arc<BidirectionalControlFixture>,
    ) -> PreparedBidirectionalLogicalLanSession {
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: generation.to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
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
        PreparedBidirectionalLogicalLanSession::new(
            session_id,
            source_device_id,
            built.manifest_hash,
            built.manifest_bytes,
            built
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(LogicalFixtureSource(BTreeMap::new())),
            control,
        )
        .unwrap()
    }

    fn prepared_bidirectional_logical_session_with_object(
        session_id: &str,
        source_device_id: &str,
        control: Arc<BidirectionalControlFixture>,
    ) -> (PreparedBidirectionalLogicalLanSession, String) {
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "permission-check".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let object = built.manifest.objects[0].clone();
        let object_bytes = built.record_objects[0].object.bytes.clone();
        let object_hash = object.hash.clone();
        let session = PreparedBidirectionalLogicalLanSession::new(
            session_id,
            source_device_id,
            built.manifest_hash,
            built.manifest_bytes,
            vec![LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            }],
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                object.hash,
                object_bytes,
            )]))),
            control,
        )
        .unwrap();
        (session, object_hash)
    }

    fn prepared_logical_session(
        session_id: &str,
        source_device_id: &str,
    ) -> PreparedLogicalLanSession {
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
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
        PreparedLogicalLanSession::new(
            session_id,
            source_device_id,
            built.manifest_hash,
            built.manifest_bytes,
            built
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(LogicalFixtureSource(BTreeMap::new())),
        )
        .unwrap()
    }

    #[test]
    fn p5_remote_apply_waits_past_the_short_control_timeout_for_a_terminal_response() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = Arc::new(BidirectionalControlFixture::default());
        *control.remote_apply_delay.lock().unwrap() = Some(Duration::from_millis(1_500));
        let session_id = "00000000-0000-4000-8000-000000000078";
        let source_device_id = "00000000-0000-4000-8000-000000000079";
        let target_device_id = "00000000-0000-4000-8000-000000000080";
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                session_id,
                source_device_id,
                Arc::clone(&control),
            ));
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = direct_bidirectional_client(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            target_device_id,
        )
        .unwrap();
        let generation = LanBidirectionalGeneration {
            generation_id: "generation-1".to_owned(),
            manifest_hash: pairing.manifest_id.clone(),
            generation_sequence: "1".to_owned(),
        };

        let started = Instant::now();
        let receipt = client
            .request_remote_apply(LanBidirectionalRemoteApplyRequest {
                operation_id: "00000000-0000-4000-8000-000000000081".to_owned(),
                source_endpoint: endpoint,
                source_session_id: pairing.session_id,
                source_manifest_id: pairing.manifest_id,
                source_claim: pairing.claim,
                expected_source_revision: 0,
                expected_source_generation: generation.clone(),
                expected_common_base_manifest_hash: generation.manifest_hash.clone(),
                backup_losing_side: false,
                completion_deferred_v1: None,
            })
            .unwrap();

        assert!(started.elapsed() > TEST_P5_CONTROL_TIMEOUT);
        assert_eq!(receipt.committed_generation, generation);
        host.stop().unwrap();
    }

    #[test]
    fn v2_bidirectional_legacy_remote_apply_preserves_http_compatibility_without_early_accounting()
    {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        *control.remote_apply_bytes.lock().unwrap() = 17;
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000086",
                &source_device_id,
                Arc::clone(&control),
            ));
        host.enable_v2_registry(
            source_root.path(),
            "Windows",
            DevicePermissions::read_and_bidirectional(),
        )
        .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: "00000000-0000-4000-8000-000000000087".to_owned(),
            source_endpoint: endpoint,
            source_session_id: pairing.session_id,
            source_manifest_id: pairing.manifest_id.clone(),
            source_claim: pairing.claim,
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: "generation-1".to_owned(),
                manifest_hash: pairing.manifest_id.clone(),
                generation_sequence: "1".to_owned(),
            },
            expected_common_base_manifest_hash: pairing.manifest_id,
            backup_losing_side: false,
            completion_deferred_v1: None,
        };

        client.request_remote_apply(request.clone()).unwrap();
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 1);
        client.request_remote_apply(request).unwrap();

        let registry = OutgoingDeviceRegistry::load(source_root.path()).unwrap();
        assert_eq!(registry.devices()[0].total_bytes, 0);
        assert!(registry.devices()[0].last_seen_ms > 0);
        host.stop().unwrap();
    }

    #[test]
    fn p5_claim_binds_the_stable_target_and_authenticates_control_callbacks() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = Arc::new(BidirectionalControlFixture::default());
        let session_id = "00000000-0000-4000-8000-000000000070";
        let source_device_id = "00000000-0000-4000-8000-000000000071";
        let target_device_id = "00000000-0000-4000-8000-000000000072";
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                session_id,
                source_device_id,
                Arc::clone(&control),
            ));
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        assert_eq!(
            reqwest::blocking::Client::new()
                .post(format!(
                    "{endpoint}/v1/sessions/{}/registration",
                    pairing.session_id
                ))
                .body("{}")
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let client = LanBidirectionalLogicalClient::claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            target_device_id,
        )
        .unwrap();
        let generation = LanBidirectionalGeneration {
            generation_id: "generation-1".to_owned(),
            manifest_hash: pairing.manifest_id.clone(),
            generation_sequence: "1".to_owned(),
        };

        assert_eq!(client.device_id(), target_device_id);
        assert_eq!(client.source_device_id(), source_device_id);
        assert_eq!(host.devices()[0].device_id, target_device_id);

        client
            .register(LanBidirectionalRegistrationRequest {
                library_id: "library".to_owned(),
                generation: generation.clone(),
                expected_revision: 0,
            })
            .unwrap();
        assert_eq!(control.registrations.lock().unwrap().len(), 1);
        assert_eq!(
            control.registrations.lock().unwrap()[0]
                .target_device_id
                .as_str(),
            target_device_id
        );

        let accepted = client
            .request_remote_apply(LanBidirectionalRemoteApplyRequest {
                operation_id: "00000000-0000-4000-8000-000000000075".to_owned(),
                source_endpoint: endpoint.clone(),
                source_session_id: pairing.session_id.clone(),
                source_manifest_id: pairing.manifest_id.clone(),
                source_claim: pairing.claim.clone(),
                expected_source_revision: 0,
                expected_source_generation: generation.clone(),
                expected_common_base_manifest_hash: pairing.manifest_id.clone(),
                backup_losing_side: false,
                completion_deferred_v1: None,
            })
            .unwrap();
        assert_eq!(accepted.committed_generation, generation);

        *control.remote_apply_error.lock().unwrap() = Some(PeerSyncError::ActivationConflict {
            expected: Some("before".to_owned()),
            actual: Some("after".to_owned()),
        });
        assert!(matches!(
            client.request_remote_apply(LanBidirectionalRemoteApplyRequest {
                operation_id: "00000000-0000-4000-8000-000000000076".to_owned(),
                source_endpoint: endpoint.clone(),
                source_session_id: pairing.session_id.clone(),
                source_manifest_id: pairing.manifest_id.clone(),
                source_claim: pairing.claim.clone(),
                expected_source_revision: 0,
                expected_source_generation: generation.clone(),
                expected_common_base_manifest_hash: pairing.manifest_id.clone(),
                backup_losing_side: false,
                completion_deferred_v1: None,
            }),
            Err(PeerSyncError::ActivationConflict { .. })
        ));

        *control.remote_apply_error.lock().unwrap() =
            Some(PeerSyncError::Storage("fixture failure".to_owned()));
        assert!(matches!(
            client.request_remote_apply(LanBidirectionalRemoteApplyRequest {
                operation_id: "00000000-0000-4000-8000-000000000077".to_owned(),
                source_endpoint: endpoint.clone(),
                source_session_id: pairing.session_id.clone(),
                source_manifest_id: pairing.manifest_id.clone(),
                source_claim: pairing.claim.clone(),
                expected_source_revision: 0,
                expected_source_generation: generation.clone(),
                expected_common_base_manifest_hash: pairing.manifest_id.clone(),
                backup_losing_side: false,
                completion_deferred_v1: None,
            }),
            Err(PeerSyncError::Transport(message)) if message.contains("500")
        ));

        let resumed = LanBidirectionalLogicalClient::resume(client.credential()).unwrap();
        assert_eq!(resumed.device_id(), target_device_id);
        assert_eq!(resumed.source_device_id(), source_device_id);

        assert!(host.revoke(target_device_id));
        assert!(matches!(
            client.register(LanBidirectionalRegistrationRequest {
                library_id: "library".to_owned(),
                generation: LanBidirectionalGeneration {
                    generation_id: "generation-1".to_owned(),
                    manifest_hash: pairing.manifest_id,
                    generation_sequence: "1".to_owned(),
                },
                expected_revision: 0,
            }),
            Err(PeerSyncError::Transport(_))
        ));
        host.stop().unwrap();
    }

    #[test]
    fn v2_claim_persists_both_registries_and_hello_rehydrates_after_restart() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_name = "Windows desktop";
        let target_name = "Android";
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let target_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000090",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), source_name, DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());

        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            target_name,
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        assert_eq!(client.device_id, target_id);
        assert_eq!(client.source_device_id(), source_id);
        assert_eq!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .device_id,
            target_id
        );
        let incoming =
            super::super::device_registry::IncomingSourceRegistry::load(target_root.path())
                .unwrap();
        let source = &incoming.sources()[0];
        assert_eq!(source.device_id, source_id);
        assert_eq!(source.name, source_name);

        let hello = reqwest::blocking::Client::new()
            .get(format!("{endpoint}/v1/peer/hello"))
            .bearer_auth(&client.bearer)
            .send()
            .unwrap();
        assert_eq!(hello.status(), reqwest::StatusCode::OK);
        let hello: serde_json::Value = hello.json().unwrap();
        assert_eq!(hello["deviceId"], source_id);
        assert_eq!(hello["name"], source_name);
        assert_eq!(hello["lanes"]["delta"]["sessionId"], pairing.session_id);
        assert!(hello["lanes"]["clone"].is_null());
        assert!(hello["lanes"]["bidirectional"].is_null());
        let unknown_bearer = if client.bearer == "f".repeat(64) {
            "e".repeat(64)
        } else {
            "f".repeat(64)
        };
        assert_eq!(
            authenticated_peer_hello_status(&endpoint, &unknown_bearer).unwrap(),
            AuthenticatedPeerHelloOutcome::AuthorizationExpired
        );

        let bearer = client.bearer.clone();
        let pending_manifest = pairing.manifest_id.clone();
        let completion_lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        assert_eq!(
            client
                .prepare_delta_completion(completion_lease.as_str())
                .unwrap(),
            0
        );
        assert!(client.fetch_manifest_with_completion_lease(None).is_err());
        assert_eq!(
            client
                .fetch_manifest_with_completion_lease(Some(&completion_lease))
                .unwrap()
                .completion_lease_id
                .unwrap(),
            completion_lease
        );
        host.stop().unwrap();
        let mut restarted = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000091",
            &source_id,
        ));
        restarted
            .enable_v2_registry(source_root.path(), source_name, DevicePermissions::read())
            .unwrap();
        restarted.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let restarted_endpoint = format!("http://{}", restarted.address().unwrap());
        assert_eq!(
            reqwest::blocking::Client::new()
                .get(format!("{restarted_endpoint}/v1/peer/hello"))
                .bearer_auth(&bearer)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            deliver_peer_completion(
                &restarted_endpoint,
                &bearer,
                PeerCompletionCapability::V1,
                CompletionLane::Delta,
                completion_lease.as_str(),
                &pending_manifest,
                0,
            )
            .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            0
        );
        restarted.stop().unwrap();
    }

    #[test]
    fn authenticated_hello_advertises_completion_v1_without_changing_json() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000101",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();

        let raw = reqwest::blocking::Client::new()
            .get(format!("{endpoint}/v1/peer/hello"))
            .bearer_auth(&client.bearer)
            .send()
            .unwrap();
        assert_eq!(
            raw.headers()
                .get(PEER_COMPLETION_CAPABILITY_HEADER)
                .unwrap(),
            PEER_COMPLETION_CAPABILITY_V1
        );
        let json: serde_json::Value = raw.json().unwrap();
        let json = json.as_object().unwrap();
        assert_eq!(json.len(), 4);
        for key in ["deviceId", "name", "permissions", "lanes"] {
            assert!(json.contains_key(key));
        }
        let observed = client.hello_with_capabilities().unwrap();
        assert_eq!(observed.hello.device_id, source_id);
        assert_eq!(observed.completion, PeerCompletionCapability::V1);
        assert_eq!(client.hello().unwrap(), observed.hello);
        host.stop().unwrap();
    }

    #[test]
    fn v2_bidirectional_completion_deferred_flag_prevents_legacy_early_accounting() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        *control.remote_apply_bytes.lock().unwrap() = 17;
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000109",
                &source_device_id,
                Arc::clone(&control),
            ));
        host.enable_v2_registry(
            source_root.path(),
            "Windows",
            DevicePermissions::read_and_bidirectional(),
        )
        .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let completion_lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: completion_lease.as_str().to_owned(),
            source_endpoint: endpoint,
            source_session_id: pairing.session_id,
            source_manifest_id: pairing.manifest_id.clone(),
            source_claim: pairing.claim,
            expected_source_revision: 0,
            expected_source_generation: LanBidirectionalGeneration {
                generation_id: "generation-1".to_owned(),
                manifest_hash: pairing.manifest_id.clone(),
                generation_sequence: "1".to_owned(),
            },
            expected_common_base_manifest_hash: pairing.manifest_id,
            backup_losing_side: false,
            completion_deferred_v1: Some(true),
        };

        assert!(client
            .prepare_bidirectional_completion(completion_lease.as_str())
            .is_err());
        client.request_remote_apply(request.clone()).unwrap();
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 1);
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            0
        );
        assert_eq!(
            client
                .prepare_bidirectional_completion(completion_lease.as_str())
                .unwrap(),
            17
        );
        let capability = client.hello_with_capabilities().unwrap().completion;
        assert_eq!(
            client
                .deliver_completion(capability, completion_lease.as_str(), 17)
                .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert!(client.request_remote_apply(request.clone()).is_err());
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 1);
        let mut legacy = request;
        legacy.operation_id = "00000000-0000-4000-8000-000000000111".to_owned();
        legacy.completion_deferred_v1 = None;
        assert!(client.request_remote_apply(legacy).is_err());
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 1);
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            17
        );
        host.stop().unwrap();
    }

    #[test]
    fn v2_zero_byte_bidirectional_completion_retains_the_source_receipt() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000112",
                &source_device_id,
                Arc::clone(&control),
            ));
        host.enable_v2_registry(
            source_root.path(),
            "Windows",
            DevicePermissions::read_and_bidirectional(),
        )
        .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        client
            .request_remote_apply(LanBidirectionalRemoteApplyRequest {
                operation_id: lease.as_str().to_owned(),
                source_endpoint: endpoint,
                source_session_id: pairing.session_id,
                source_manifest_id: pairing.manifest_id.clone(),
                source_claim: pairing.claim,
                expected_source_revision: 0,
                expected_source_generation: LanBidirectionalGeneration {
                    generation_id: "generation-1".to_owned(),
                    manifest_hash: pairing.manifest_id.clone(),
                    generation_sequence: "1".to_owned(),
                },
                expected_common_base_manifest_hash: pairing.manifest_id.clone(),
                backup_losing_side: false,
                completion_deferred_v1: Some(true),
            })
            .unwrap();
        assert_eq!(
            client
                .prepare_bidirectional_completion(lease.as_str())
                .unwrap(),
            0
        );
        let capability = client.hello_with_capabilities().unwrap().completion;
        client
            .deliver_completion(capability, lease.as_str(), 0)
            .unwrap();

        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            0
        );
        assert!(
            super::super::device_registry::outgoing_completion_receipt_matches(
                source_root.path(),
                &client.inner.device_id,
                CompletionLane::Bidirectional,
                lease.as_str(),
                &pairing.manifest_id,
                0,
            )
            .unwrap()
        );
        host.stop().unwrap();
    }

    #[test]
    fn bidirectional_deferred_lease_rejects_a_stale_manifest_before_remote_apply() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let first_control = Arc::new(BidirectionalControlFixture::default());
        let mut first_host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000114",
                &source_device_id,
                first_control,
            ));
        first_host
            .enable_v2_registry(
                source_root.path(),
                "Windows",
                DevicePermissions::read_and_bidirectional(),
            )
            .unwrap();
        let first_pairing = first_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let first_endpoint = format!("http://{}", first_host.address().unwrap());
        let first_client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &first_endpoint,
            &first_pairing.session_id,
            &first_pairing.manifest_id,
            &first_pairing.claim,
        )
        .unwrap();
        let stale_lease = first_client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        let target_device_id = first_client.inner.device_id.clone();
        let bearer = first_client.inner.bearer.clone();
        let stale_manifest_id = first_pairing.manifest_id;
        first_host.stop().unwrap();

        let restarted_control = Arc::new(BidirectionalControlFixture::default());
        let mut restarted_host = LanCloneHost::prepare_bidirectional_logical(
            prepared_bidirectional_logical_session_with_generation(
                "00000000-0000-4000-8000-000000000115",
                &source_device_id,
                "generation-2",
                Arc::clone(&restarted_control),
            ),
        );
        restarted_host
            .enable_v2_registry(
                source_root.path(),
                "Windows",
                DevicePermissions::read_and_bidirectional(),
            )
            .unwrap();
        let restarted_pairing = restarted_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        assert_ne!(restarted_pairing.manifest_id, stale_manifest_id);
        let restarted_endpoint = format!("http://{}", restarted_host.address().unwrap());
        let restarted_client = LanBidirectionalLogicalClient::from_registered(
            &restarted_endpoint,
            &restarted_pairing.session_id,
            &restarted_pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &bearer,
        )
        .unwrap();
        let generation = LanBidirectionalGeneration {
            generation_id: "generation-2".to_owned(),
            manifest_hash: restarted_pairing.manifest_id.clone(),
            generation_sequence: "1".to_owned(),
        };
        let request = LanBidirectionalRemoteApplyRequest {
            operation_id: stale_lease.as_str().to_owned(),
            source_endpoint: restarted_endpoint,
            source_session_id: restarted_pairing.session_id,
            source_manifest_id: restarted_pairing.manifest_id,
            source_claim: restarted_pairing.claim,
            expected_source_revision: 0,
            expected_source_generation: generation.clone(),
            expected_common_base_manifest_hash: generation.manifest_hash,
            backup_losing_side: false,
            completion_deferred_v1: Some(true),
        };

        assert!(restarted_client.request_remote_apply(request).is_err());
        assert_eq!(*restarted_control.remote_apply_calls.lock().unwrap(), 0);
        restarted_host.stop().unwrap();
    }

    #[test]
    fn bidirectional_deferred_receipt_replay_seals_old_manifest_after_restart() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        *control.remote_apply_bytes.lock().unwrap() = 17;
        let mut first_host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000116",
                &source_device_id,
                Arc::clone(&control),
            ));
        first_host
            .enable_v2_registry(
                source_root.path(),
                "Windows",
                DevicePermissions::read_and_bidirectional(),
            )
            .unwrap();
        let first_pairing = first_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let first_endpoint = format!("http://{}", first_host.address().unwrap());
        let first_client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &first_endpoint,
            &first_pairing.session_id,
            &first_pairing.manifest_id,
            &first_pairing.claim,
        )
        .unwrap();
        let lease = first_client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        let target_device_id = first_client.inner.device_id.clone();
        let bearer = first_client.inner.bearer.clone();
        let old_manifest_id = first_pairing.manifest_id.clone();
        let old_generation = LanBidirectionalGeneration {
            generation_id: "generation-1".to_owned(),
            manifest_hash: old_manifest_id.clone(),
            generation_sequence: "1".to_owned(),
        };
        let mut request = LanBidirectionalRemoteApplyRequest {
            operation_id: lease.as_str().to_owned(),
            source_endpoint: first_endpoint,
            source_session_id: first_pairing.session_id,
            source_manifest_id: old_manifest_id.clone(),
            source_claim: first_pairing.claim,
            expected_source_revision: 0,
            expected_source_generation: old_generation.clone(),
            expected_common_base_manifest_hash: old_manifest_id.clone(),
            backup_losing_side: false,
            completion_deferred_v1: Some(true),
        };
        fail_next_logical_completion_proof_write_for_test(
            source_root.path(),
            &target_device_id,
            CompletionLane::Bidirectional,
        )
        .unwrap();
        assert!(first_client.request_remote_apply(request.clone()).is_err());
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 1);
        assert_eq!(*control.remote_apply_commits.lock().unwrap(), 1);
        first_host.stop().unwrap();

        let mut restarted_host = LanCloneHost::prepare_bidirectional_logical(
            prepared_bidirectional_logical_session_with_generation(
                "00000000-0000-4000-8000-000000000117",
                &source_device_id,
                "generation-2",
                Arc::clone(&control),
            ),
        );
        restarted_host
            .enable_v2_registry(
                source_root.path(),
                "Windows",
                DevicePermissions::read_and_bidirectional(),
            )
            .unwrap();
        let restarted_pairing = restarted_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        assert_ne!(restarted_pairing.manifest_id, old_manifest_id);
        let restarted_endpoint = format!("http://{}", restarted_host.address().unwrap());
        let restarted_client = LanBidirectionalLogicalClient::from_registered(
            &restarted_endpoint,
            &restarted_pairing.session_id,
            &restarted_pairing.manifest_id,
            &target_device_id,
            &source_device_id,
            &bearer,
        )
        .unwrap();
        request.source_endpoint = restarted_endpoint.clone();
        request.source_session_id = restarted_pairing.session_id;
        request.source_manifest_id = restarted_pairing.manifest_id;
        request.source_claim = restarted_pairing.claim;

        restarted_client.request_remote_apply(request).unwrap();
        assert_eq!(*control.remote_apply_calls.lock().unwrap(), 2);
        assert_eq!(*control.remote_apply_commits.lock().unwrap(), 1);
        assert_eq!(
            prepare_peer_logical_completion(
                &restarted_endpoint,
                &bearer,
                CompletionLane::Bidirectional,
                lease.as_str(),
                &old_manifest_id,
            )
            .unwrap(),
            17
        );
        restarted_host.stop().unwrap();
    }

    #[test]
    fn completion_endpoint_authenticates_validates_permissions_and_counts_exact_retry_once() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000102",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let url = format!("{endpoint}/v1/peer/completion");
        let operation_id = "00000000-0000-4000-8000-000000000103";
        let valid = serde_json::json!({
            "schema": PEER_COMPLETION_SCHEMA,
            "lane": "clone",
            "operationId": operation_id,
            "manifestId": pairing.manifest_id,
            "transferredBytes": 23
        });
        let raw = reqwest::blocking::Client::new();

        assert_eq!(
            raw.post(&url).json(&valid).send().unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            raw.get(&url)
                .bearer_auth(&client.bearer)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED
        );
        for invalid in [
            serde_json::json!({"schema":"wrong","lane":"delta","operationId":operation_id,"manifestId":pairing.manifest_id,"transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"unknown","operationId":operation_id,"manifestId":pairing.manifest_id,"transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"delta","operationId":"not-a-uuid","manifestId":pairing.manifest_id,"transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"delta","operationId":"00000000-0000-1000-8000-000000000103","manifestId":pairing.manifest_id,"transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"delta","operationId":operation_id,"manifestId":"BAD","transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"delta","operationId":operation_id,"manifestId":pairing.manifest_id,"transferredBytes":23,"extra":true}),
        ] {
            assert_eq!(
                raw.post(&url)
                    .bearer_auth(&client.bearer)
                    .json(&invalid)
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .header(reqwest::header::CONTENT_TYPE, "text/plain")
                .body(serde_json::to_vec(&valid).unwrap())
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .body(vec![b'x'; MAX_BODY_BYTES + 1])
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::PAYLOAD_TOO_LARGE
        );
        let forbidden = serde_json::json!({
            "schema": PEER_COMPLETION_SCHEMA,
            "lane": "bidirectional",
            "operationId": operation_id,
            "manifestId": pairing.manifest_id,
            "transferredBytes": 23
        });
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&forbidden)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );

        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&valid)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );
        let completion_lease = issue_outgoing_measured_completion_offer(
            source_root.path(),
            &client.device_id,
            CompletionLane::Clone,
            &pairing.manifest_id,
            23,
            None,
        )
        .unwrap();
        let pending_retry = issue_outgoing_measured_completion_offer(
            source_root.path(),
            &client.device_id,
            CompletionLane::Clone,
            &pairing.manifest_id,
            23,
            None,
        )
        .unwrap();
        assert_eq!(pending_retry, completion_lease);
        let operation_id = completion_lease.as_str();
        let valid = serde_json::json!({
            "schema": PEER_COMPLETION_SCHEMA,
            "lane": "clone",
            "operationId": operation_id,
            "manifestId": pairing.manifest_id,
            "transferredBytes": 23
        });
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&valid)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );
        assert_eq!(
            seal_outgoing_completion_lease(
                source_root.path(),
                &client.device_id,
                CompletionLane::Clone,
                operation_id,
                &pairing.manifest_id,
                23,
            )
            .unwrap(),
            CompletionSealStatus::Sealed
        );
        for forged in [
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"clone","operationId":operation_id,"manifestId":pairing.manifest_id,"transferredBytes":24}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"clone","operationId":"00000000-0000-4000-8000-000000000113","manifestId":pairing.manifest_id,"transferredBytes":23}),
            serde_json::json!({"schema":PEER_COMPLETION_SCHEMA,"lane":"clone","operationId":operation_id,"manifestId":"f".repeat(64),"transferredBytes":23}),
        ] {
            assert_eq!(
                raw.post(&url)
                    .bearer_auth(&client.bearer)
                    .json(&forged)
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::CONFLICT
            );
        }
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&valid)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
        let capability = client.hello_with_capabilities().unwrap().completion;
        assert_eq!(
            deliver_peer_completion(
                &endpoint,
                &client.bearer,
                capability,
                CompletionLane::Clone,
                operation_id,
                &pairing.manifest_id,
                23,
            )
            .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        let mismatched_retry = serde_json::json!({
            "schema": PEER_COMPLETION_SCHEMA,
            "lane": "delta",
            "operationId": operation_id,
            "manifestId": pairing.manifest_id,
            "transferredBytes": 24
        });
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&mismatched_retry)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );

        let next_completion_lease = issue_outgoing_measured_completion_offer(
            source_root.path(),
            &client.device_id,
            CompletionLane::Clone,
            &pairing.manifest_id,
            7,
            None,
        )
        .unwrap();
        let next_operation_id = next_completion_lease.as_str();
        assert_eq!(
            seal_outgoing_completion_lease(
                source_root.path(),
                &client.device_id,
                CompletionLane::Clone,
                next_operation_id,
                &pairing.manifest_id,
                7,
            )
            .unwrap(),
            CompletionSealStatus::Sealed
        );
        let next = serde_json::json!({
            "schema": PEER_COMPLETION_SCHEMA,
            "lane": "clone",
            "operationId": next_operation_id,
            "manifestId": pairing.manifest_id,
            "transferredBytes": 7
        });
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&next)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(
            raw.post(&url)
                .bearer_auth(&client.bearer)
                .json(&valid)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );
        let registry = OutgoingDeviceRegistry::load(source_root.path()).unwrap();
        assert_eq!(registry.devices()[0].total_bytes, 30);
        host.stop().unwrap();
    }

    #[test]
    fn completion_endpoint_persistence_failure_is_atomic_and_retryable() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000104",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let mut registry = OutgoingDeviceRegistry::load(source_root.path()).unwrap();
        let mut device = registry.devices()[0].clone();
        device.total_bytes = u64::MAX;
        registry.upsert(device).unwrap();
        registry.save().unwrap();
        let completion_lease = issue_outgoing_measured_completion_offer(
            source_root.path(),
            &client.device_id,
            CompletionLane::Clone,
            &pairing.manifest_id,
            1,
            None,
        )
        .unwrap();
        assert_eq!(
            seal_outgoing_completion_lease(
                source_root.path(),
                &client.device_id,
                CompletionLane::Clone,
                completion_lease.as_str(),
                &pairing.manifest_id,
                1,
            )
            .unwrap(),
            CompletionSealStatus::Sealed
        );

        let result = deliver_peer_completion(
            &endpoint,
            &client.bearer,
            PeerCompletionCapability::V1,
            CompletionLane::Clone,
            completion_lease.as_str(),
            &pairing.manifest_id,
            1,
        );
        assert!(result.is_err());
        let mut registry = OutgoingDeviceRegistry::load(source_root.path()).unwrap();
        assert_eq!(registry.devices()[0].total_bytes, u64::MAX);
        let mut device = registry.devices()[0].clone();
        device.total_bytes = 0;
        registry.upsert(device).unwrap();
        registry.save().unwrap();
        assert_eq!(
            deliver_peer_completion(
                &endpoint,
                &client.bearer,
                PeerCompletionCapability::V1,
                CompletionLane::Clone,
                completion_lease.as_str(),
                &pairing.manifest_id,
                1,
            )
            .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            1
        );
        host.stop().unwrap();
    }

    #[test]
    fn completion_client_skips_legacy_servers_and_retries_the_same_request_after_response_loss() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut hello, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut hello).starts_with("GET /v1/peer/hello "));
            let body = serde_json::to_vec(&serde_json::json!({
                "deviceId":"00000000-0000-4000-8000-000000000106",
                "name":"Legacy",
                "permissions":["read"],
                "lanes":{"clone":null,"delta":null,"bidirectional":null}
            }))
            .unwrap();
            write!(hello, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            hello.write_all(&body).unwrap();
            hello.flush().unwrap();
            finish_response(&mut hello);
        });
        let observed = authenticated_peer_hello_with_capabilities(&endpoint, TEST_BEARER).unwrap();
        server.join().unwrap();
        assert_eq!(observed.completion, PeerCompletionCapability::Unsupported);
        assert_eq!(
            deliver_peer_completion(
                &endpoint,
                TEST_BEARER,
                observed.completion,
                CompletionLane::Clone,
                "00000000-0000-4000-8000-000000000107",
                &"a".repeat(64),
                9,
            )
            .unwrap(),
            PeerCompletionDelivery::Unsupported
        );

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let mut bodies = Vec::new();
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let head = read_request_head(&mut stream);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then_some(value.trim())
                    })
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                let body_start = head.find("\r\n\r\n").unwrap() + 4;
                let mut body = head.as_bytes()[body_start..].to_vec();
                while body.len() < length {
                    let mut chunk = [0_u8; 1024];
                    let read = stream.read(&mut chunk).unwrap();
                    body.extend_from_slice(&chunk[..read]);
                }
                bodies.push(body[..length].to_vec());
                if attempt == 1 {
                    stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    stream.flush().unwrap();
                    finish_response(&mut stream);
                }
            }
            assert_eq!(bodies[0], bodies[1]);
        });
        let deliver = || {
            deliver_peer_completion(
                &endpoint,
                TEST_BEARER,
                PeerCompletionCapability::V1,
                CompletionLane::Clone,
                "00000000-0000-4000-8000-000000000108",
                &"b".repeat(64),
                11,
            )
        };
        assert!(deliver().is_err());
        assert_eq!(deliver().unwrap(), PeerCompletionDelivery::Delivered);
        server.join().unwrap();
    }

    #[test]
    fn concurrent_revoke_removes_a_claim_bearer_persisted_before_live_insertion() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let target_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000092",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let registered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        host.set_after_v2_registry_claim_hook_for_test({
            let registered = Arc::clone(&registered);
            let release = Arc::clone(&release);
            Arc::new(move || {
                registered.wait();
                release.wait();
            })
        });

        let target_root_path = target_root.path().to_owned();
        let session_id = pairing.session_id.clone();
        let manifest_id = pairing.manifest_id.clone();
        let claim_value = pairing.claim.clone();
        let endpoint_for_claim = endpoint.clone();
        let claim = std::thread::spawn(move || {
            LanLogicalDeltaClient::claim_v2_and_register(
                &target_root_path,
                "Android",
                &endpoint_for_claim,
                &session_id,
                &manifest_id,
                &claim_value,
            )
        });

        registered.wait();
        assert!(!host.revoke(&target_id));
        release.wait();
        assert!(matches!(
            claim.join().unwrap(),
            Err(PeerSyncError::Transport(message)) if message.contains("401")
        ));

        assert!(host.devices().is_empty());
        assert!(
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()
                .is_empty()
        );
        host.stop().unwrap();
    }

    fn retained_delta_completion(
        source_device_id: &str,
        manifest_id: &str,
    ) -> super::super::delta_completion::DeltaCompletionContext {
        let base = |sequence: &str| crate::persistent_store::SyncGenerationIdentity {
            generation_id: format!("generation-{sequence}"),
            manifest_hash: manifest_id.to_owned(),
            generation_sequence: sequence.to_owned(),
        };
        super::super::delta_completion::DeltaCompletionContext {
            operation_id: uuid::Uuid::new_v4().to_string(),
            source_device_id: source_device_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            mode: super::super::delta_completion::DeltaCompletionMode::CompletionV1,
            pre_revision: 4,
            pre_common_base: Some(base("1")),
            post_revision: 5,
            post_common_base: base("2"),
            transferred_objects: 1,
            transferred_bytes: 17,
        }
    }

    #[test]
    fn a_new_logical_registration_is_refused_while_a_delta_completion_is_retained() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000097",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        super::super::delta_completion::PeerDeltaCompletionJournal::new(target_root.path())
            .store_activation_intent(&retained_delta_completion(&source_id, &pairing.manifest_id))
            .unwrap();
        let outgoing_path = source_root.path().join("peer-sync/devices.json");
        let incoming_path = target_root.path().join("peer-sync/sources.json");
        let outgoing_before = fs::read(&outgoing_path).unwrap();
        let incoming_before = fs::read(&incoming_path).unwrap();
        let rotated = host.rotate_pairing_link().unwrap();

        let blocked = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &rotated.session_id,
            &rotated.manifest_id,
            &rotated.claim,
        )
        .err();

        assert_eq!(
            blocked,
            Some(PeerSyncError::Validation(
                "peer-registration-blocked-by-active-work".to_owned()
            ))
        );
        assert_eq!(fs::read(&outgoing_path).unwrap(), outgoing_before);
        assert_eq!(fs::read(&incoming_path).unwrap(), incoming_before);
        // The retained operation keeps authenticating with the authorization it has.
        assert_eq!(client.hello().unwrap().device_id, source_id);
        host.stop().unwrap();
    }

    #[test]
    fn v2_bidirectional_claim_registers_the_stable_source_and_granted_permissions() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000095",
                &source_id,
                control,
            ));
        host.enable_v2_registry(
            source_root.path(),
            "Windows",
            DevicePermissions::read_and_bidirectional(),
        )
        .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());

        let client = LanBidirectionalLogicalClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();

        assert_eq!(client.source_device_id(), source_id);
        assert!(!client.fetch_manifest().unwrap().is_empty());
        let hello = client.hello().unwrap();
        assert_eq!(hello.device_id, source_id);
        assert!(hello.permissions.allows_bidirectional());
        assert_eq!(
            hello.lanes.bidirectional.as_ref().unwrap().session_id,
            pairing.session_id
        );
        assert!(!format!("{hello:?}").contains(&client.credential().bearer));
        let incoming = IncomingSourceRegistry::load(target_root.path()).unwrap();
        assert_eq!(incoming.sources().len(), 1);
        assert_eq!(incoming.sources()[0].name, "Windows");
        assert!(incoming.sources()[0].permissions.allows_bidirectional());
        host.stop().unwrap();
    }

    #[test]
    fn strict_v2_clone_does_not_persist_when_post_claim_hello_disconnects() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target_root = tempfile::tempdir().unwrap();
        let target_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let source_id = "00000000-0000-4000-8000-000000000106";
        let session_id = "00000000-0000-4000-8000-000000000107";
        let manifest_id = "a".repeat(64);
        let claim = "b".repeat(64);
        let bearer = "c".repeat(64);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server_target_id = target_id.clone();
        let server = std::thread::spawn(move || {
            let (mut claim_stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut claim_stream).starts_with("POST "));
            let body = serde_json::to_vec(&serde_json::json!({
                "deviceId": server_target_id,
                "bearer": bearer,
                "permission": "clone-read",
                "sourceDeviceId": source_id,
                "sourceDeviceName": "Windows source",
                "permissions": ["read"]
            }))
            .unwrap();
            write!(
                claim_stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            claim_stream.write_all(&body).unwrap();
            claim_stream.flush().unwrap();
            finish_response(&mut claim_stream);

            let (mut hello_stream, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut hello_stream).starts_with("GET "));
        });
        let credential = target_root.path().join("strict-v2-credential.json");

        assert!(LanCloneClient::claim_v2_and_persist_and_register(
            target_root.path(),
            "Android target",
            &credential,
            &endpoint,
            session_id,
            &manifest_id,
            &claim,
        )
        .is_err());
        server.join().unwrap();
        assert!(!credential.exists());
        assert!(IncomingSourceRegistry::load(target_root.path())
            .unwrap()
            .sources()
            .is_empty());
    }

    #[test]
    fn invalid_v2_claim_fields_do_not_consume_the_one_use_claim() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000092",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let url = format!("{endpoint}/v1/sessions/{}/claim", pairing.session_id);
        let target_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let raw = reqwest::blocking::Client::new();

        for body in [
            // The shape a build that predates the registered claim sends.
            serde_json::json!({"claim": pairing.claim}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceName": "Android"}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id}),
            serde_json::json!({"claim": pairing.claim, "version": 2, "deviceId": target_id, "deviceName": "Android"}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 3, "deviceId": target_id, "deviceName": "Android"}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": "bad", "deviceName": "Android"}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id, "deviceName": ""}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id, "deviceName": "Android", "permissions": ["bidirectional"]}),
        ] {
            assert_eq!(
                raw.post(&url).json(&body).send().unwrap().status(),
                reqwest::StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            raw.post(&url)
                .json(&serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id, "deviceName": "Android"}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        host.stop().unwrap();
    }

    #[test]
    fn invalid_claim_fields_do_not_consume_an_unregistered_lane_claim() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        // No `enable_v2_registry`: this host only opens a lane for one operation.
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000094",
            &source_id,
        ));
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let url = format!("{endpoint}/v1/sessions/{}/claim", pairing.session_id);
        let target_id =
            super::super::device_registry::load_or_create_device_id(target_root.path()).unwrap();
        let raw = reqwest::blocking::Client::new();

        for body in [
            // The shape a build that predates the registered claim sends.
            serde_json::json!({"claim": pairing.claim}),
            serde_json::json!({"claim": pairing.claim, "protocolVersion": 2}),
            serde_json::json!({"claim": pairing.claim, "deviceId": target_id}),
        ] {
            assert_eq!(
                raw.post(&url).json(&body).send().unwrap().status(),
                reqwest::StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            raw.post(&url)
                .json(&serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id}))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        host.stop().unwrap();
    }

    #[test]
    fn changed_bearer_claim_requires_revoke_and_preserves_completion_replay() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared_logical_session(
            "00000000-0000-4000-8000-000000000118",
            &source_id,
        ));
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let first_pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &first_pairing.session_id,
            &first_pairing.manifest_id,
            &first_pairing.claim,
        )
        .unwrap();
        let lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        let old_bearer = client.bearer.clone();
        let target_id = client.device_id.clone();
        let next_pairing = host.rotate_pairing_link().unwrap();
        let claim_url = format!("{endpoint}/v1/sessions/{}/claim", next_pairing.session_id);
        let body = serde_json::json!({
            "claim": next_pairing.claim,
            "protocolVersion": 2,
            "deviceId": target_id,
            "deviceName": "Android"
        });
        let raw = reqwest::blocking::Client::new();

        assert_eq!(
            raw.post(&claim_url).json(&body).send().unwrap().status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(client.hello().unwrap().device_id, source_id);
        assert_eq!(client.prepare_delta_completion(lease.as_str()).unwrap(), 0);
        assert_eq!(
            client
                .deliver_completion(PeerCompletionCapability::V1, lease.as_str(), 0)
                .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert_eq!(
            raw.post(&claim_url).json(&body).send().unwrap().status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            client
                .deliver_completion(PeerCompletionCapability::V1, lease.as_str(), 0)
                .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert!(host.revoke(&target_id));
        let response = raw.post(&claim_url).json(&body).send().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let response: serde_json::Value = response.json().unwrap();
        assert_ne!(response["bearer"].as_str().unwrap(), old_bearer);
        assert!(!OutgoingDeviceRegistry::load(source_root.path())
            .unwrap()
            .has_completion_lease(&target_id, CompletionLane::Delta)
            .unwrap());
        host.stop().unwrap();
    }

    #[test]
    fn v2_read_only_claim_cannot_access_bidirectional_session_routes() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let source_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let target_id = "00000000-0000-4000-8000-000000000093";
        let control = Arc::new(BidirectionalControlFixture::default());
        let (session, object_hash) = prepared_bidirectional_logical_session_with_object(
            "00000000-0000-4000-8000-000000000094",
            &source_id,
            Arc::clone(&control),
        );
        let mut host = LanCloneHost::prepare_bidirectional_logical(session);
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let session_url = format!("{endpoint}/v1/sessions/{}", pairing.session_id);
        let claim: serde_json::Value = reqwest::blocking::Client::new()
            .post(format!("{session_url}/claim"))
            .json(&serde_json::json!({"claim": pairing.claim, "protocolVersion": 2, "deviceId": target_id, "deviceName": "Android"}))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(claim["permissions"], serde_json::json!(["read"]));
        let bearer = claim["bearer"].as_str().unwrap();
        let raw = reqwest::blocking::Client::new();
        let statuses = [
            raw.get(format!("{session_url}/manifest"))
                .bearer_auth(bearer)
                .send()
                .unwrap()
                .status(),
            raw.post(format!("{session_url}/progress"))
                .bearer_auth(bearer)
                .json(&serde_json::json!({"verifiedBytes": 0, "currentObject": null}))
                .send()
                .unwrap()
                .status(),
            raw.head(format!("{session_url}/objects/{object_hash}"))
                .bearer_auth(bearer)
                .send()
                .unwrap()
                .status(),
            raw.get(format!("{session_url}/objects/{object_hash}"))
                .bearer_auth(bearer)
                .send()
                .unwrap()
                .status(),
        ];
        assert_eq!(statuses, [reqwest::StatusCode::FORBIDDEN; 4]);
        assert_eq!(
            raw
                .post(format!("{session_url}/registration"))
                .bearer_auth(bearer)
                .json(&serde_json::json!({
                    "libraryId": "library",
                    "generation": {"generationId": "generation-1", "manifestHash": pairing.manifest_id, "generationSequence": "1"},
                    "expectedRevision": 0
                }))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );
        assert!(control.registrations.lock().unwrap().is_empty());
        host.stop().unwrap();
    }

    #[test]
    fn p5_rejects_malformed_claimants_and_wrong_logical_permissions() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = Arc::new(BidirectionalControlFixture::default());
        let mut host =
            LanCloneHost::prepare_bidirectional_logical(prepared_bidirectional_logical_session(
                "00000000-0000-4000-8000-000000000073",
                "00000000-0000-4000-8000-000000000074",
                control,
            ));
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let session_url = format!("{endpoint}/v1/sessions/{}", pairing.session_id);
        let raw_client = reqwest::blocking::Client::new();

        assert_eq!(
            raw_client
                .post(format!("{session_url}/claim"))
                .json(
                    &serde_json::json!({"claim": pairing.claim.clone(), "deviceId": "not-a-uuid"})
                )
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        assert!(matches!(
            LanLogicalDeltaClient::claim_with_timeouts_and_device(
                &endpoint,
                &pairing.session_id,
                &pairing.manifest_id,
                &pairing.claim,
                "00000000-0000-4000-8000-000000000076",
                "logical-read",
                LogicalClientTimeouts {
                    control_request: CONTROL_REQUEST_TIMEOUT,
                    object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
                },
            ),
            Err(PeerSyncError::Protocol(_))
        ));
        host.stop().unwrap();
    }

    #[test]
    fn logical_session_reuses_claim_bearer_revoke_and_bounded_object_routes() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let accounting_root = tempfile::tempdir().unwrap();
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "plugin-key".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let record_hash = built.record_objects[0].object.hash.clone();
        let record_bytes = built.record_objects[0].object.bytes.clone();
        let session_id = "00000000-0000-4000-8000-000000000042";
        let source_device_id = "00000000-0000-4000-8000-000000000043";
        let logical = PreparedLogicalLanSession::new(
            session_id,
            source_device_id,
            built.manifest_hash.clone(),
            built.manifest_bytes.clone(),
            built
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                record_hash.clone(),
                record_bytes.clone(),
            )]))),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let mut client = LanLogicalDeltaClient::claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            "00000000-0000-4000-8000-000000000078",
        )
        .unwrap();
        register_outgoing_claim(
            accounting_root.path(),
            OutgoingDevice {
                device_id: client.device_id.clone(),
                name: "legacy logical target".to_owned(),
                bearer_digest: hex::encode(digest(client.bearer.as_bytes())),
                permissions: DevicePermissions::read(),
                created_at_ms: 1,
                last_seen_ms: 1,
                total_bytes: 11,
            },
        )
        .unwrap();

        assert_eq!(client.source_device_id(), source_device_id);
        assert_eq!(client.fetch_manifest().unwrap(), built.manifest_bytes);
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: record_hash.clone(),
                size: record_bytes.len() as u64,
            })
            .unwrap();
        let devices = host.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].verified_bytes, 0);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, record_bytes);
        let devices = host.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].verified_bytes, received.len() as u64);
        assert_eq!(devices[0].current_object, None);
        assert_eq!(
            OutgoingDeviceRegistry::load(accounting_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            11
        );

        assert!(host.revoke(&client.device_id));
        assert!(matches!(
            client.fetch_manifest(),
            Err(PeerSyncError::Transport(_))
        ));
        host.stop().unwrap();
    }

    #[test]
    fn logical_progress_never_marks_a_corrupt_object_as_verified() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "plugin-key".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let record_hash = built.record_objects[0].object.hash.clone();
        let record_size = built.record_objects[0].object.size;
        let mut corrupt = built.record_objects[0].object.bytes.clone();
        corrupt[0] ^= 0xff;
        let logical = PreparedLogicalLanSession::new(
            "00000000-0000-4000-8000-000000000052",
            "00000000-0000-4000-8000-000000000053",
            built.manifest_hash.clone(),
            built.manifest_bytes,
            built
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                record_hash.clone(),
                corrupt,
            )]))),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let mut client = LanLogicalDeltaClient::claim(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            "00000000-0000-4000-8000-000000000078",
        )
        .unwrap();
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: record_hash,
                size: record_size,
            })
            .unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();

        let devices = host.devices();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].verified_bytes, 0);
        assert_eq!(devices[0].current_object, None);
        host.stop().unwrap();
    }

    #[test]
    fn strict_logical_progress_requires_an_issued_object_and_seals_source_derived_bytes() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "strict-proof".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let object = built.manifest.objects[0].clone();
        let object_bytes = built.record_objects[0].object.bytes.clone();
        let logical = PreparedLogicalLanSession::new(
            "00000000-0000-4000-8000-000000000205",
            &source_device_id,
            built.manifest_hash.clone(),
            built.manifest_bytes.clone(),
            vec![LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            }],
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                object.hash.clone(),
                object_bytes.clone(),
            )]))),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let mut client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let manifest = client.fetch_manifest_with_completion_lease(None).unwrap();
        let lease = manifest.completion_lease_id.unwrap();
        let completion_url = format!("{endpoint}/v1/peer/completion");
        for wrong in [
            serde_json::json!({
                "schema": PEER_COMPLETION_SCHEMA,
                "lane": "delta",
                "operationId": "00000000-0000-4000-8000-000000000211",
                "manifestId": pairing.manifest_id,
            }),
            serde_json::json!({
                "schema": PEER_COMPLETION_SCHEMA,
                "lane": "delta",
                "operationId": lease.as_str(),
                "manifestId": "f".repeat(64),
            }),
        ] {
            assert_eq!(
                reqwest::blocking::Client::new()
                    .post(&completion_url)
                    .bearer_auth(&client.bearer)
                    .json(&wrong)
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::CONFLICT
            );
        }
        let progress_url = format!("{endpoint}/v1/sessions/{}/progress", pairing.session_id);
        let strict_progress = ProgressRequest {
            verified_bytes: u64::MAX,
            current_object: None,
            operation_id: Some(lease.as_str().to_owned()),
            manifest_id: Some(pairing.manifest_id.clone()),
            verified_object: Some(object.hash.clone()),
        };

        assert_eq!(
            reqwest::blocking::Client::new()
                .post(&progress_url)
                .bearer_auth(&client.bearer)
                .json(&strict_progress)
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );

        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object.hash,
                size: object.size,
            })
            .unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, object_bytes);
        assert_eq!(
            client.prepare_delta_completion(lease.as_str()).unwrap(),
            object.size
        );
        assert_eq!(
            client.prepare_delta_completion(lease.as_str()).unwrap(),
            object.size
        );
        assert_eq!(host.devices()[0].verified_bytes, object.size);
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            0
        );
        let capability = client.hello_with_capabilities().unwrap().completion;
        assert!(client
            .deliver_completion(capability, lease.as_str(), object.size + 1)
            .is_err());
        assert_eq!(
            client
                .deliver_completion(capability, lease.as_str(), object.size)
                .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        assert_eq!(
            OutgoingDeviceRegistry::load(source_root.path())
                .unwrap()
                .devices()[0]
                .total_bytes,
            object.size
        );
        host.stop().unwrap();
    }

    #[test]
    fn logical_completion_prepare_omits_target_bytes_and_rejects_extra_response_fields() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(request.starts_with("POST /v1/peer/completion "));
            assert!(!request.contains("transferredBytes"));
            let body = serde_json::to_vec(&serde_json::json!({
                "schema": PEER_COMPLETION_PREPARED_SCHEMA,
                "transferredBytes": 9,
                "extra": true,
            }))
            .unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            finish_response(&mut stream);
        });

        assert!(prepare_peer_logical_completion(
            &endpoint,
            TEST_BEARER,
            CompletionLane::Delta,
            "00000000-0000-4000-8000-000000000210",
            &"a".repeat(64),
        )
        .is_err());
        server.join().unwrap();
    }

    #[test]
    fn global_delta_prepare_recovers_after_source_restart_and_manifest_change() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-before-restart".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: None,
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "restart-proof".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"before"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let old_manifest_id = built.manifest_hash.clone();
        let object = built.manifest.objects[0].clone();
        let object_bytes = built.record_objects[0].object.bytes.clone();
        let old_session = PreparedLogicalLanSession::new(
            "00000000-0000-4000-8000-000000000208",
            &source_device_id,
            old_manifest_id.clone(),
            built.manifest_bytes,
            vec![LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            }],
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                object.hash.clone(),
                object_bytes,
            )]))),
        )
        .unwrap();
        let mut old_host = LanCloneHost::prepare_logical(old_session);
        old_host
            .enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let old_pairing = old_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let old_endpoint = format!("http://{}", old_host.address().unwrap());
        let mut client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &old_endpoint,
            &old_pairing.session_id,
            &old_pairing.manifest_id,
            &old_pairing.claim,
        )
        .unwrap();
        let lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();
        client
            .open_object(&LogicalDeltaObject {
                hash: object.hash,
                size: object.size,
            })
            .unwrap()
            .read_to_end(&mut Vec::new())
            .unwrap();
        let bearer = client.bearer.clone();
        let target_device_id = client.device_id.clone();
        old_host.stop().unwrap();

        let changed_session =
            prepared_logical_session("00000000-0000-4000-8000-000000000209", &source_device_id);
        assert_ne!(changed_session.manifest_id, old_manifest_id);
        let mut restarted_host = LanCloneHost::prepare_logical(changed_session);
        restarted_host
            .enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        restarted_host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let restarted_endpoint = format!("http://{}", restarted_host.address().unwrap());

        for _ in 0..2 {
            assert_eq!(
                prepare_peer_logical_completion(
                    &restarted_endpoint,
                    &bearer,
                    CompletionLane::Delta,
                    lease.as_str(),
                    &old_manifest_id,
                )
                .unwrap(),
                object.size
            );
        }
        assert_eq!(
            deliver_peer_completion(
                &restarted_endpoint,
                &bearer,
                PeerCompletionCapability::V1,
                CompletionLane::Delta,
                lease.as_str(),
                &old_manifest_id,
                object.size,
            )
            .unwrap(),
            PeerCompletionDelivery::Delivered
        );
        let registry = OutgoingDeviceRegistry::load(source_root.path()).unwrap();
        let device = registry
            .devices()
            .iter()
            .find(|device| device.device_id == target_device_id)
            .unwrap();
        assert_eq!(device.total_bytes, object.size);
        restarted_host.stop().unwrap();
    }

    #[test]
    fn strict_partial_object_stream_is_never_issued_for_progress() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id =
            super::super::device_registry::load_or_create_device_id(source_root.path()).unwrap();
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "partial-proof".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let object = built.manifest.objects[0].clone();
        let mut partial = built.record_objects[0].object.bytes.clone();
        partial.pop();
        let logical = PreparedLogicalLanSession::new(
            "00000000-0000-4000-8000-000000000207",
            &source_device_id,
            built.manifest_hash.clone(),
            built.manifest_bytes,
            vec![LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            }],
            Box::new(LogicalFixtureSource(BTreeMap::from([(
                object.hash.clone(),
                partial,
            )]))),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        host.enable_v2_registry(source_root.path(), "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let mut client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let lease = client
            .fetch_manifest_with_completion_lease(None)
            .unwrap()
            .completion_lease_id
            .unwrap();

        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            })
            .unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        assert_eq!(
            reqwest::blocking::Client::new()
                .post(format!(
                    "{endpoint}/v1/sessions/{}/progress",
                    pairing.session_id
                ))
                .bearer_auth(&client.bearer)
                .json(&ProgressRequest {
                    verified_bytes: object.size,
                    current_object: None,
                    operation_id: Some(lease.as_str().to_owned()),
                    manifest_id: Some(pairing.manifest_id),
                    verified_object: Some(object.hash),
                })
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::CONFLICT
        );
        host.stop().unwrap();
    }

    #[test]
    fn strict_logical_progress_failure_is_a_read_error_while_legacy_stays_best_effort() {
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "strict-failure".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value":"remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let manifest_id = built.manifest_hash.clone();
        let manifest_bytes = built.manifest_bytes;
        let object = built.manifest.objects[0].clone();
        let object_bytes = built.record_objects[0].object.bytes.clone();
        let lease = "00000000-0000-4000-8000-000000000206";
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let response_manifest = manifest_id.clone();
        let response_object = object.hash.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(request.starts_with("GET "));
            assert!(request.to_ascii_lowercase().contains(
                &format!("{PEER_COMPLETION_CAPABILITY_HEADER}: {PEER_COMPLETION_CAPABILITY_V1}")
                    .to_ascii_lowercase()
            ));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\n{}: {}\r\n{}: {}\r\nConnection: close\r\n\r\n",
                manifest_bytes.len(),
                quoted(&response_manifest),
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
                PEER_COMPLETION_LEASE_HEADER,
                lease,
            )
            .unwrap();
            stream.write_all(&manifest_bytes).unwrap();
            finish_response(&mut stream);

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(request.to_ascii_lowercase().contains(
                &format!("{PEER_COMPLETION_LEASE_HEADER}: {lease}").to_ascii_lowercase()
            ));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\nConnection: close\r\n\r\n",
                object_bytes.len(),
                quoted(&response_object),
            )
            .unwrap();
            stream.write_all(&object_bytes).unwrap();
            finish_response(&mut stream);

            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(request.starts_with("POST "));
            stream
                .write_all(
                    b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            finish_response(&mut stream);
        });
        let mut client =
            direct_logical_client(address, Duration::from_secs(2), Duration::from_secs(2));
        client.manifest_id = manifest_id;
        client.fetch_manifest_with_completion_lease(None).unwrap();
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object.hash,
                size: object.size,
            })
            .unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        server.join().unwrap();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let legacy_bytes = b"legacy".to_vec();
        let legacy_hash = sha256_hex(&legacy_bytes);
        let legacy_etag = quoted(&legacy_hash);
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request_head(&mut stream);
            assert!(!request.contains(PEER_COMPLETION_LEASE_HEADER));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {}\r\nConnection: close\r\n\r\n",
                legacy_bytes.len(),
                legacy_etag,
            )
            .unwrap();
            stream.write_all(&legacy_bytes).unwrap();
            finish_response(&mut stream);
            let (mut progress, _) = listener.accept().unwrap();
            assert!(read_request_head(&mut progress).starts_with("POST "));
            progress
                .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
            finish_response(&mut progress);
        });
        let mut legacy =
            direct_logical_client(address, Duration::from_secs(2), Duration::from_secs(2));
        let mut reader = legacy
            .open_object(&LogicalDeltaObject {
                hash: legacy_hash,
                size: 6,
            })
            .unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"legacy");
        server.join().unwrap();
    }

    #[test]
    fn backpressured_logical_object_opens_without_waiting_for_control_progress() {
        let _guard = LOGICAL_LAN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let (second_hold_tx, second_hold_rx) = mpsc::channel::<()>();
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some("generation-0".to_owned()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Plugin {
                    storage_key: "backpressured-plugin".to_owned(),
                },
                LogicalRecordEnvelope::Plugin {
                    ordinal: 0,
                    value: serde_json::json!({"value": "remote"}),
                },
                vec![],
            )],
        })
        .unwrap();
        let record_hash = built.record_objects[0].object.hash.clone();
        let record_bytes = built.record_objects[0].object.bytes.clone();
        let logical = PreparedLogicalLanSession::new(
            "00000000-0000-4000-8000-000000000062",
            "00000000-0000-4000-8000-000000000063",
            built.manifest_hash,
            built.manifest_bytes,
            built
                .manifest
                .objects
                .iter()
                .map(|object| LogicalDeltaObject {
                    hash: object.hash.clone(),
                    size: object.size,
                })
                .collect(),
            Box::new(HeldLogicalFixtureSource {
                object_hash: record_hash.clone(),
                bytes: record_bytes.clone(),
                releases: std::collections::VecDeque::from([hold_rx, second_hold_rx]),
            }),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let control_timeout = Duration::from_millis(1_000);
        let mut client = LanLogicalDeltaClient::claim_with_timeouts_and_device(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            "00000000-0000-4000-8000-000000000077",
            "logical-read",
            LogicalClientTimeouts {
                control_request: control_timeout,
                object_idle: Duration::from_secs(10),
            },
        )
        .unwrap();

        let started = Instant::now();
        // The body is held until after open returns, so an open that waited for
        // body production would exhaust the object timeout and fail; a generous
        // hang guard replaces the scheduling-sensitive control-timeout bound.
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: record_hash.clone(),
                size: record_bytes.len() as u64,
            })
            .unwrap();
        let opened_in = started.elapsed();
        let current_while_streaming = host.devices()[0].current_object.clone();
        hold_tx.send(()).unwrap();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();

        assert!(
            opened_in < Duration::from_secs(5),
            "logical object open waited {opened_in:?} for control progress"
        );
        assert_eq!(
            current_while_streaming.as_deref(),
            Some(record_hash.as_str())
        );
        assert_eq!(received, record_bytes);
        assert_eq!(host.devices()[0].verified_bytes, received.len() as u64);
        assert_eq!(host.devices()[0].current_object, None);

        let dropped_reader = client
            .open_object(&LogicalDeltaObject {
                hash: record_hash.clone(),
                size: record_bytes.len() as u64,
            })
            .unwrap();
        assert_eq!(
            host.devices()[0].current_object.as_deref(),
            Some(record_hash.as_str())
        );
        drop(dropped_reader);
        // Unblock the held second body so the server observes the dropped
        // client and clears its progress.
        drop(second_hold_tx);
        let clear_deadline = Instant::now() + Duration::from_secs(2);
        while host.devices()[0].current_object.is_some() && Instant::now() < clear_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(host.devices()[0].current_object, None);
        assert_eq!(host.devices()[0].verified_bytes, received.len() as u64);
        host.stop().unwrap();
    }

    #[test]
    fn logical_object_stream_is_not_bound_by_the_short_control_timeout() {
        let timeouts = LogicalClientTimeouts {
            control_request: Duration::from_secs(5),
            object_idle: Duration::from_secs(30),
        };
        assert_eq!(
            logical_request_timeout(LogicalRequestKind::Control, timeouts),
            Duration::from_secs(5),
        );
        assert_eq!(
            logical_request_timeout(LogicalRequestKind::Object, timeouts),
            Duration::from_secs(30),
        );
    }
}

#[cfg(any(desktop, target_os = "android"))]
pub struct LanLogicalDeltaClient {
    control_client: reqwest::blocking::Client,
    object_client: reqwest::blocking::Client,
    control_timeout: Duration,
    session_url: String,
    pub device_id: String,
    bearer: String,
    source_device_id: String,
    manifest_id: String,
    progress: Arc<Mutex<LogicalClientProgress>>,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Default)]
struct LogicalClientProgress {
    verified_bytes: u64,
    verified_objects: BTreeSet<String>,
    completion_lease_id: Option<CompletionLeaseId>,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalRequestKind {
    Control,
    Object,
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Copy)]
struct LogicalClientTimeouts {
    control_request: Duration,
    object_idle: Duration,
}

#[cfg(any(desktop, target_os = "android"))]
fn logical_request_timeout(kind: LogicalRequestKind, timeouts: LogicalClientTimeouts) -> Duration {
    match kind {
        LogicalRequestKind::Control => timeouts.control_request,
        LogicalRequestKind::Object => timeouts.object_idle,
    }
}

#[cfg(any(desktop, target_os = "android"))]
fn build_logical_http_client(
    kind: LogicalRequestKind,
    timeouts: LogicalClientTimeouts,
) -> Result<reqwest::blocking::Client, PeerSyncError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(logical_request_timeout(kind, timeouts))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(transport)
}

#[cfg(any(desktop, target_os = "android"))]
fn build_bidirectional_remote_apply_client() -> Result<reqwest::Client, PeerSyncError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(transport)
}

#[cfg(any(desktop, target_os = "android"))]
impl LanLogicalDeltaClient {
    pub(crate) fn from_registered(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
    ) -> Result<Self, PeerSyncError> {
        let endpoint = validate_p4_logical_delta_endpoint(endpoint)?;
        if !is_canonical_uuid(session_id)
            || !is_lower_hex_256(manifest_id)
            || !is_canonical_uuid(target_device_id)
            || !is_canonical_uuid(source_device_id)
            || !is_lower_hex_256(bearer)
        {
            return Err(PeerSyncError::Protocol(
                "invalid registered logical credential".to_owned(),
            ));
        }
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "logical delta session URL is too long".to_owned(),
            ));
        }
        let timeouts = LogicalClientTimeouts {
            control_request: CONTROL_REQUEST_TIMEOUT,
            object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
        };
        Ok(Self {
            control_client: build_logical_http_client(LogicalRequestKind::Control, timeouts)?,
            object_client: build_logical_http_client(LogicalRequestKind::Object, timeouts)?,
            control_timeout: timeouts.control_request,
            session_url,
            device_id: target_device_id.to_owned(),
            bearer: bearer.to_owned(),
            source_device_id: source_device_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            progress: Arc::new(Mutex::new(LogicalClientProgress::default())),
        })
    }

    #[cfg(test)]
    pub(crate) fn claim_v2_and_register(
        app_root: &Path,
        target_name: &str,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        Self::claim_v2_and_register_with_permission(
            app_root,
            target_name,
            endpoint,
            session_id,
            manifest_id,
            claim,
            "logical-read",
        )
    }

    pub(crate) fn hello(&self) -> Result<PeerHello, PeerSyncError> {
        let endpoint = self
            .session_url
            .split("/v1/sessions/")
            .next()
            .ok_or_else(|| PeerSyncError::Protocol("invalid logical session URL".to_owned()))?;
        authenticated_peer_hello(endpoint, &self.bearer)
    }

    pub(crate) fn hello_with_capabilities(
        &self,
    ) -> Result<PeerHelloWithCapabilities, PeerSyncError> {
        let endpoint = self
            .session_url
            .split("/v1/sessions/")
            .next()
            .ok_or_else(|| PeerSyncError::Protocol("invalid logical session URL".to_owned()))?;
        authenticated_peer_hello_with_capabilities(endpoint, &self.bearer)
    }

    pub(crate) fn deliver_completion(
        &self,
        capability: PeerCompletionCapability,
        operation_id: &str,
        transferred_bytes: u64,
    ) -> Result<PeerCompletionDelivery, PeerSyncError> {
        let endpoint = self
            .session_url
            .split("/v1/sessions/")
            .next()
            .ok_or_else(|| PeerSyncError::Protocol("invalid logical session URL".to_owned()))?;
        deliver_peer_completion(
            endpoint,
            &self.bearer,
            capability,
            CompletionLane::Delta,
            operation_id,
            &self.manifest_id,
            transferred_bytes,
        )
    }

    pub(crate) fn prepare_delta_completion(
        &self,
        operation_id: &str,
    ) -> Result<u64, PeerSyncError> {
        let endpoint = self
            .session_url
            .split("/v1/sessions/")
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PeerSyncError::Protocol("invalid logical session URL".to_owned()))?;
        prepare_peer_logical_completion(
            endpoint,
            &self.bearer,
            CompletionLane::Delta,
            operation_id,
            &self.manifest_id,
        )
    }

    #[cfg(test)]
    fn claim_v2_and_register_with_permission(
        app_root: &Path,
        target_name: &str,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        expected_permission: &str,
    ) -> Result<Self, PeerSyncError> {
        if endpoint.len() > MAX_URL_BYTES
            || !is_canonical_uuid(session_id)
            || !is_lower_hex_256(manifest_id)
            || !is_lower_hex_256(claim)
        {
            return Err(PeerSyncError::Protocol(
                "invalid logical delta pairing data".to_owned(),
            ));
        }
        validate_device_name(target_name)?;
        let endpoint = validate_p4_logical_delta_endpoint(endpoint)?;
        let target_device_id = load_or_create_device_id(app_root)?;
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "logical delta session URL is too long".to_owned(),
            ));
        }
        let timeouts = LogicalClientTimeouts {
            control_request: CONTROL_REQUEST_TIMEOUT,
            object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
        };
        let control_client = build_logical_http_client(LogicalRequestKind::Control, timeouts)?;
        let object_client = build_logical_http_client(LogicalRequestKind::Object, timeouts)?;
        // One lifecycle hold covers the refusal, the claim round trip that makes the
        // source replace its authorization, and this device's own registration, so a
        // lane cannot start in the middle and lose the authorization it is using.
        let lifecycle = lock_registered_source_lifecycle()?;
        ensure_no_active_registered_source_work(&lifecycle, app_root)?;
        let response = control_client
            .post(format!("{session_url}/claim"))
            .json(&ClaimRequest {
                claim: claim.to_owned(),
                device_id: Some(target_device_id.clone()),
                protocol_version: CLAIM_PROTOCOL_VERSION,
                device_name: Some(target_name.to_owned()),
                permissions: None,
            })
            .timeout(timeouts.control_request)
            .send()
            .map_err(transport)?;
        // A source that predates the registered claim refuses the request outright.
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        let response = read_claim_response(response, "logical delta")?;
        let (Some(source_device_id), Some(granted)) =
            (response.source_device_id, response.permissions)
        else {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        };
        let source_name = response.source_device_name.ok_or_else(|| {
            PeerSyncError::Protocol("v2 source device name is missing".to_owned())
        })?;
        // A permission this build cannot name reads the same as a response that
        // omits the registered identity: the peer grants something this claim
        // does not understand, so it is reported as outdated rather than failed.
        if DevicePermissions::from_values(granted).is_err() {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        if response.device_id != target_device_id
            || !is_canonical_uuid(&source_device_id)
            || validate_device_name(&source_name).is_err()
            || !is_lower_hex_256(&response.bearer)
            || response.permission != expected_permission
        {
            return Err(PeerSyncError::Protocol(
                "invalid logical delta v2 claim response".to_owned(),
            ));
        }
        let hello = authenticated_peer_hello(endpoint.as_str(), &response.bearer)?;
        if hello.device_id != source_device_id {
            return Err(PeerSyncError::Protocol(
                "v2 claim source device identity differs from authenticated hello".to_owned(),
            ));
        }
        register_incoming_source_if_compatible_locked(
            &lifecycle,
            app_root,
            IncomingSource {
                device_id: source_device_id.clone(),
                name: hello.name,
                endpoint: endpoint.clone(),
                bearer: response.bearer.clone(),
                permissions: hello.permissions,
                last_seen_ms: now_ms() as u64,
                total_bytes: 0,
            },
        )?;
        Ok(Self {
            control_client,
            object_client,
            control_timeout: timeouts.control_request,
            session_url,
            device_id: response.device_id,
            bearer: response.bearer,
            source_device_id,
            manifest_id: manifest_id.to_owned(),
            progress: Arc::new(Mutex::new(LogicalClientProgress::default())),
        })
    }

    /// Claims the session the peer opened for a single operation. That session
    /// registers nothing on either side, so the answer carries the peer's own
    /// identity instead of a registered source.
    pub fn claim(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        device_id: &str,
    ) -> Result<Self, PeerSyncError> {
        Self::claim_with_timeouts_and_device(
            endpoint,
            session_id,
            manifest_id,
            claim,
            device_id,
            "logical-read",
            LogicalClientTimeouts {
                control_request: CONTROL_REQUEST_TIMEOUT,
                object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
            },
        )
    }

    fn claim_with_timeouts_and_device(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        device_id: &str,
        permission: &str,
        timeouts: LogicalClientTimeouts,
    ) -> Result<Self, PeerSyncError> {
        if endpoint.len() > MAX_URL_BYTES
            || !is_canonical_uuid(session_id)
            || !is_lower_hex_256(manifest_id)
            || !is_lower_hex_256(claim)
            || !is_canonical_uuid(device_id)
        {
            return Err(PeerSyncError::Protocol(
                "invalid logical delta pairing data".to_owned(),
            ));
        }
        let endpoint = validate_private_lan_endpoint(endpoint)?;
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "logical delta session URL is too long".to_owned(),
            ));
        }
        let control_timeout = timeouts.control_request;
        let control_client = build_logical_http_client(LogicalRequestKind::Control, timeouts)?;
        let object_client = build_logical_http_client(LogicalRequestKind::Object, timeouts)?;
        let response = control_client
            .post(format!("{session_url}/claim"))
            .json(&ClaimRequest {
                claim: claim.to_owned(),
                device_id: Some(device_id.to_owned()),
                protocol_version: CLAIM_PROTOCOL_VERSION,
                device_name: None,
                permissions: None,
            })
            .timeout(control_timeout)
            .send()
            .map_err(transport)?;
        // A peer that predates the single-operation claim refuses it outright.
        if response.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let mut body = Vec::new();
        response
            .take(MAX_CLAIM_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(transport)?;
        if body.len() > MAX_CLAIM_RESPONSE_BYTES {
            return Err(PeerSyncError::Protocol(
                "logical delta claim response is too large".to_owned(),
            ));
        }
        let response: ClaimResponseOwned = serde_json::from_slice(&body)
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        // A single-operation lane registers nothing, so the granted permissions only
        // prove that the source answers the registered claim; the lane itself is
        // authorized by the bearer this response carries.
        let (Some(source_device_id), Some(granted)) =
            (response.source_device_id, response.permissions)
        else {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        };
        // A permission this build cannot name reads the same as a response that
        // omits the registered identity: the peer grants something this claim
        // does not understand, so it is reported as outdated rather than failed.
        if DevicePermissions::from_values(granted).is_err() {
            return Err(PeerSyncError::Validation(PEER_OUTDATED.to_owned()));
        }
        if response.device_id != device_id
            || !is_canonical_uuid(&source_device_id)
            || !is_lower_hex_256(&response.bearer)
            || response.permission != permission
        {
            return Err(PeerSyncError::Protocol(
                "invalid logical delta claim response".to_owned(),
            ));
        }
        Ok(Self {
            control_client,
            object_client,
            control_timeout,
            session_url,
            device_id: response.device_id,
            bearer: response.bearer,
            source_device_id,
            manifest_id: manifest_id.to_owned(),
            progress: Arc::new(Mutex::new(LogicalClientProgress::default())),
        })
    }

    pub fn source_device_id(&self) -> &str {
        &self.source_device_id
    }

    pub(crate) fn registered_source_bearer(&self) -> &str {
        &self.bearer
    }

    pub fn fetch_manifest(&self) -> Result<Vec<u8>, PeerSyncError> {
        Ok(self.fetch_manifest_request(false, None)?.bytes)
    }

    pub(crate) fn fetch_manifest_with_completion_lease(
        &self,
        resume_lease_id: Option<&CompletionLeaseId>,
    ) -> Result<LanCompletionManifestResponse, PeerSyncError> {
        self.fetch_manifest_request(true, resume_lease_id)
    }

    fn fetch_manifest_request(
        &self,
        completion_v1: bool,
        resume_lease_id: Option<&CompletionLeaseId>,
    ) -> Result<LanCompletionManifestResponse, PeerSyncError> {
        let mut request = self
            .control_client
            .get(format!("{}/manifest", self.session_url));
        if completion_v1 {
            request = request.header(
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
            );
            if let Some(resume_lease_id) = resume_lease_id {
                request = request.header(PEER_COMPLETION_RESUME_HEADER, resume_lease_id.as_str());
            }
        }
        let response = self.authorized_control(request).send().map_err(transport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        if response.content_length().unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES as u64 {
            return Err(PeerSyncError::Protocol(
                "logical delta manifest is too large".to_owned(),
            ));
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| {
                PeerSyncError::Protocol("logical delta manifest ETag is missing".to_owned())
            })?;
        let completion_lease_id = if completion_v1 {
            parse_completion_lease_headers(response.headers())?
        } else {
            None
        };
        let mut bytes = Vec::new();
        response
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(transport)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PeerSyncError::Protocol(
                "logical delta manifest is too large".to_owned(),
            ));
        }
        let received = sha256_hex(&bytes);
        if received != self.manifest_id || etag != quoted(&self.manifest_id) {
            return Err(PeerSyncError::StaleManifest {
                expected: self.manifest_id.clone(),
                received,
            });
        }
        if completion_v1 {
            let mut progress = self.progress.lock().map_err(|error| {
                PeerSyncError::Storage(format!("logical progress mutex poisoned: {error}"))
            })?;
            if progress.completion_lease_id != completion_lease_id {
                progress.verified_bytes = 0;
                progress.verified_objects.clear();
            }
            progress.completion_lease_id = completion_lease_id.clone();
        }
        Ok(LanCompletionManifestResponse {
            bytes,
            completion_lease_id,
        })
    }

    fn authorized_control(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request
            .bearer_auth(&self.bearer)
            .timeout(self.control_timeout)
    }

    fn progress_reporter(&self) -> LogicalProgressReporter {
        LogicalProgressReporter {
            client: self.control_client.clone(),
            control_timeout: self.control_timeout,
            session_url: self.session_url.clone(),
            bearer: self.bearer.clone(),
            manifest_id: self.manifest_id.clone(),
            progress: Arc::clone(&self.progress),
        }
    }
}

#[cfg(any(desktop, target_os = "android"))]
impl LogicalDeltaObjectSource for LanLogicalDeltaClient {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        validate_object_hash(&object.hash)?;
        let completion_lease_id = self
            .progress
            .lock()
            .map_err(|error| {
                PeerSyncError::Storage(format!("logical progress mutex poisoned: {error}"))
            })?
            .completion_lease_id
            .clone();
        let mut request = self
            .object_client
            .get(format!("{}/objects/{}", self.session_url, object.hash))
            .bearer_auth(&self.bearer);
        if let Some(lease_id) = completion_lease_id {
            request = request.header(PEER_COMPLETION_LEASE_HEADER, lease_id.as_str());
        }
        let response = request.send().map_err(transport)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let content_length = response.content_length().ok_or_else(|| {
            PeerSyncError::Protocol("logical delta object size header is missing".to_owned())
        })?;
        if content_length != object.size
            || response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                != Some(quoted(&object.hash).as_str())
        {
            return Err(PeerSyncError::Protocol(
                "logical delta object response identity is invalid".to_owned(),
            ));
        }
        Ok(Box::new(LogicalProgressReader {
            inner: response,
            reporter: self.progress_reporter(),
            expected_hash: object.hash.clone(),
            expected_size: object.size,
            received_size: 0,
            hasher: Sha256::new(),
            completed: false,
        }))
    }
}

#[cfg(any(desktop, target_os = "android"))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalLogicalCredential {
    pub(crate) endpoint: String,
    pub(crate) session_id: String,
    pub(crate) manifest_id: String,
    pub(crate) device_id: String,
    pub(crate) source_device_id: String,
    pub(crate) bearer: String,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalLogicalCredential {
    // Android resume validates trusted-LAN credentials.
    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    fn validate(&self) -> Result<String, PeerSyncError> {
        self.validate_with(validate_private_lan_endpoint)
    }

    // Desktop resume additionally accepts tunnel endpoints.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    fn validate_p5_desktop(&self) -> Result<String, PeerSyncError> {
        self.validate_with(validate_p5_desktop_endpoint)
    }

    fn validate_with(
        &self,
        validate_endpoint: fn(&str) -> Result<String, PeerSyncError>,
    ) -> Result<String, PeerSyncError> {
        if !is_canonical_uuid(&self.session_id)
            || !is_lower_hex_256(&self.manifest_id)
            || !is_canonical_uuid(&self.device_id)
            || !is_canonical_uuid(&self.source_device_id)
            || !is_lower_hex_256(&self.bearer)
        {
            return Err(PeerSyncError::Protocol(
                "invalid bidirectional logical credential".to_owned(),
            ));
        }
        validate_endpoint(&self.endpoint)
    }
}

#[cfg(any(desktop, target_os = "android"))]
pub(crate) struct LanBidirectionalLogicalClient {
    inner: LanLogicalDeltaClient,
    remote_apply_client: reqwest::Client,
    endpoint: String,
    session_id: String,
}

#[cfg(any(desktop, target_os = "android"))]
impl LanBidirectionalLogicalClient {
    pub(crate) fn from_registered(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
    ) -> Result<Self, PeerSyncError> {
        Self::resume(LanBidirectionalLogicalCredential {
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
            device_id: target_device_id.to_owned(),
            source_device_id: source_device_id.to_owned(),
            bearer: bearer.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn claim_v2_and_register(
        app_root: &Path,
        target_name: &str,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        let inner = LanLogicalDeltaClient::claim_v2_and_register_with_permission(
            app_root,
            target_name,
            endpoint,
            session_id,
            manifest_id,
            claim,
            "logical-bidirectional",
        )?;
        Ok(Self {
            remote_apply_client: build_bidirectional_remote_apply_client()?,
            endpoint: validate_p5_desktop_endpoint(endpoint)?,
            session_id: session_id.to_owned(),
            inner,
        })
    }

    pub(crate) fn hello(&self) -> Result<PeerHello, PeerSyncError> {
        authenticated_peer_hello(&self.endpoint, &self.inner.bearer)
    }

    pub(crate) fn hello_with_capabilities(
        &self,
    ) -> Result<PeerHelloWithCapabilities, PeerSyncError> {
        authenticated_peer_hello_with_capabilities(&self.endpoint, &self.inner.bearer)
    }

    pub(crate) fn deliver_completion(
        &self,
        capability: PeerCompletionCapability,
        operation_id: &str,
        transferred_bytes: u64,
    ) -> Result<PeerCompletionDelivery, PeerSyncError> {
        deliver_peer_completion(
            &self.endpoint,
            &self.inner.bearer,
            capability,
            CompletionLane::Bidirectional,
            operation_id,
            &self.inner.manifest_id,
            transferred_bytes,
        )
    }

    pub(crate) fn prepare_bidirectional_completion(
        &self,
        operation_id: &str,
    ) -> Result<u64, PeerSyncError> {
        prepare_peer_logical_completion(
            &self.endpoint,
            &self.inner.bearer,
            CompletionLane::Bidirectional,
            operation_id,
            &self.inner.manifest_id,
        )
    }

    // Only the engine tests reach a bidirectional session through a pairing link;
    // product flows resume a registered device instead.
    #[cfg(test)]
    pub(crate) fn claim(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        device_id: &str,
    ) -> Result<Self, PeerSyncError> {
        let inner = LanLogicalDeltaClient::claim_with_timeouts_and_device(
            endpoint,
            session_id,
            manifest_id,
            claim,
            device_id,
            "logical-bidirectional",
            LogicalClientTimeouts {
                control_request: CONTROL_REQUEST_TIMEOUT,
                object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
            },
        )?;
        Ok(Self {
            remote_apply_client: build_bidirectional_remote_apply_client()?,
            endpoint: validate_private_lan_endpoint(endpoint)?,
            session_id: session_id.to_owned(),
            inner,
        })
    }

    pub(crate) fn resume(
        credential: LanBidirectionalLogicalCredential,
    ) -> Result<Self, PeerSyncError> {
        #[cfg(desktop)]
        let endpoint = credential.validate_p5_desktop()?;
        #[cfg(target_os = "android")]
        let endpoint = credential.validate()?;
        let session_url = format!("{endpoint}/v1/sessions/{}", credential.session_id);
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "bidirectional logical session URL is too long".to_owned(),
            ));
        }
        let timeouts = LogicalClientTimeouts {
            control_request: CONTROL_REQUEST_TIMEOUT,
            object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
        };
        Ok(Self {
            remote_apply_client: build_bidirectional_remote_apply_client()?,
            inner: LanLogicalDeltaClient {
                control_client: build_logical_http_client(LogicalRequestKind::Control, timeouts)?,
                object_client: build_logical_http_client(LogicalRequestKind::Object, timeouts)?,
                control_timeout: timeouts.control_request,
                session_url,
                device_id: credential.device_id,
                bearer: credential.bearer,
                source_device_id: credential.source_device_id,
                manifest_id: credential.manifest_id,
                progress: Arc::new(Mutex::new(LogicalClientProgress::default())),
            },
            endpoint,
            session_id: credential.session_id,
        })
    }

    pub(crate) fn credential(&self) -> LanBidirectionalLogicalCredential {
        LanBidirectionalLogicalCredential {
            endpoint: self.endpoint.clone(),
            session_id: self.session_id.clone(),
            manifest_id: self.inner.manifest_id.clone(),
            device_id: self.inner.device_id.clone(),
            source_device_id: self.inner.source_device_id.clone(),
            bearer: self.inner.bearer.clone(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn device_id(&self) -> &str {
        &self.inner.device_id
    }

    pub(crate) fn source_device_id(&self) -> &str {
        self.inner.source_device_id()
    }

    pub(crate) fn fetch_manifest(&self) -> Result<Vec<u8>, PeerSyncError> {
        self.inner.fetch_manifest()
    }

    pub(crate) fn fetch_manifest_with_completion_lease(
        &self,
        resume_lease_id: Option<&CompletionLeaseId>,
    ) -> Result<LanCompletionManifestResponse, PeerSyncError> {
        self.inner
            .fetch_manifest_with_completion_lease(resume_lease_id)
    }

    pub(crate) fn register(
        &self,
        request: LanBidirectionalRegistrationRequest,
    ) -> Result<(), PeerSyncError> {
        if !request.is_valid() {
            return Err(PeerSyncError::Protocol(
                "invalid bidirectional registration request".to_owned(),
            ));
        }
        let response = self
            .inner
            .authorized_control(
                self.inner
                    .control_client
                    .post(format!("{}/registration", self.inner.session_url))
                    .json(&request),
            )
            .send()
            .map_err(transport)?;
        if response.status().as_u16() != 204 {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }

    pub(crate) fn request_remote_apply(
        &self,
        request: LanBidirectionalRemoteApplyRequest,
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
        if !request.is_valid() {
            return Err(PeerSyncError::Protocol(
                "invalid bidirectional remote apply request".to_owned(),
            ));
        }
        tauri::async_runtime::block_on(async {
            let mut response = self
                .remote_apply_client
                .post(format!("{}/remote-apply", self.inner.session_url))
                .bearer_auth(&self.inner.bearer)
                .json(&request)
                .send()
                .await
                .map_err(transport)?;
            if response.status() == reqwest::StatusCode::CONFLICT {
                return Err(PeerSyncError::ActivationConflict {
                    expected: None,
                    actual: None,
                });
            }
            if response.status() != reqwest::StatusCode::OK {
                return Err(PeerSyncError::Transport(format!(
                    "HTTP {}",
                    response.status()
                )));
            }
            if response.content_length().unwrap_or(u64::MAX) > MAX_BODY_BYTES as u64 {
                return Err(PeerSyncError::Protocol(
                    "bidirectional remote apply response is too large".to_owned(),
                ));
            }
            let mut body = Vec::new();
            while let Some(chunk) =
                tokio::time::timeout(LOGICAL_OBJECT_IDLE_TIMEOUT, response.chunk())
                    .await
                    .map_err(|_| {
                        PeerSyncError::Transport(
                            "bidirectional remote apply response read timed out".to_owned(),
                        )
                    })?
                    .map_err(transport)?
            {
                if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                    return Err(PeerSyncError::Protocol(
                        "bidirectional remote apply response is too large".to_owned(),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            let receipt = serde_json::from_slice::<LanBidirectionalRemoteApplyReceipt>(&body)
                .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
            if !receipt.is_valid() {
                return Err(PeerSyncError::Protocol(
                    "invalid bidirectional remote apply response".to_owned(),
                ));
            }
            Ok(receipt)
        })
    }
}

#[cfg(any(desktop, target_os = "android"))]
impl LogicalDeltaObjectSource for LanBidirectionalLogicalClient {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        self.inner.open_object(object)
    }
}

#[cfg(any(desktop, target_os = "android"))]
struct LogicalProgressReporter {
    client: reqwest::blocking::Client,
    control_timeout: Duration,
    session_url: String,
    bearer: String,
    manifest_id: String,
    progress: Arc<Mutex<LogicalClientProgress>>,
}

#[cfg(any(desktop, target_os = "android"))]
impl LogicalProgressReporter {
    fn post(&self, request: &ProgressRequest) -> Result<(), PeerSyncError> {
        let response = self
            .client
            .post(format!("{}/progress", self.session_url))
            .bearer_auth(&self.bearer)
            .json(request)
            .timeout(self.control_timeout)
            .send()
            .map_err(transport)?;
        if response.status().as_u16() != 204 {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }

    fn complete_object(&self, object: &str, size: u64) -> Result<(), PeerSyncError> {
        let mut progress = self.progress.lock().map_err(|error| {
            PeerSyncError::Storage(format!("logical progress mutex poisoned: {error}"))
        })?;
        if let Some(lease_id) = progress.completion_lease_id.clone() {
            let already_verified = progress.verified_objects.contains(object);
            let candidate = if already_verified {
                progress.verified_bytes
            } else {
                progress.verified_bytes.checked_add(size).ok_or_else(|| {
                    PeerSyncError::Validation("logical progress byte count overflow".to_owned())
                })?
            };
            self.post(&ProgressRequest {
                verified_bytes: candidate,
                current_object: None,
                operation_id: Some(lease_id.as_str().to_owned()),
                manifest_id: Some(self.manifest_id.clone()),
                verified_object: Some(object.to_owned()),
            })?;
            if !already_verified {
                progress.verified_bytes = candidate;
                progress.verified_objects.insert(object.to_owned());
            }
            return Ok(());
        }

        progress.verified_bytes = progress.verified_bytes.checked_add(size).ok_or_else(|| {
            PeerSyncError::Validation("logical progress byte count overflow".to_owned())
        })?;
        let verified_bytes = progress.verified_bytes;
        drop(progress);
        let _ = self.post(&ProgressRequest {
            verified_bytes,
            current_object: None,
            operation_id: None,
            manifest_id: None,
            verified_object: None,
        });
        Ok(())
    }
}

#[cfg(any(desktop, target_os = "android"))]
struct LogicalProgressReader {
    inner: reqwest::blocking::Response,
    reporter: LogicalProgressReporter,
    expected_hash: String,
    expected_size: u64,
    received_size: u64,
    hasher: Sha256,
    completed: bool,
}

#[cfg(any(desktop, target_os = "android"))]
impl LogicalProgressReader {
    fn finish_if_verified(&mut self) -> io::Result<()> {
        if self.completed {
            return Ok(());
        }
        let hash = hex::encode(self.hasher.clone().finalize());
        if self.received_size == self.expected_size && hash == self.expected_hash {
            self.reporter
                .complete_object(&self.expected_hash, self.expected_size)
                .map_err(|error| io::Error::other(error.to_string()))?;
            self.completed = true;
        }
        Ok(())
    }
}

#[cfg(any(desktop, target_os = "android"))]
impl Read for LogicalProgressReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return self.inner.read(output);
        }
        match self.inner.read(output) {
            Ok(0) => {
                self.finish_if_verified()?;
                Ok(0)
            }
            Ok(read) => {
                self.received_size =
                    self.received_size.checked_add(read as u64).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "logical object stream byte count overflow",
                        )
                    })?;
                self.hasher.update(&output[..read]);
                Ok(read)
            }
            Err(error) => Err(error),
        }
    }
}
