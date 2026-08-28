use super::{
    http_stream::HttpRangeStream,
    protocol::{sha256_hex, CLONE_CHUNK_SIZE, MAX_MANIFEST_BYTES},
    PeerSyncError,
};
#[cfg(desktop)]
use super::{LogicalDeltaObject, LogicalDeltaObjectSource, PreparedCloneSession};
#[cfg(desktop)]
use crate::asset_repository::PayloadCas;
use serde::{Deserialize, Serialize};
#[cfg(desktop)]
use sha2::{Digest, Sha256};
#[cfg(desktop)]
use std::{
    collections::BTreeMap,
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
    path::Path,
    time::Duration,
};

#[cfg(desktop)]
const CLAIM_TTL: Duration = Duration::from_secs(10 * 60);
#[cfg(desktop)]
pub(crate) const NAMED_TUNNEL_ORIGIN_PORT: u16 = 32145;
#[cfg(desktop)]
pub(crate) const NAMED_TUNNEL_ORIGIN_UNAVAILABLE: &str =
    "Named Tunnel cannot bind loopback port 32145. Stop the app using that port, or use Quick Tunnel / Trusted LAN.";
#[cfg(all(test, desktop))]
pub(crate) static NAMED_TUNNEL_TEST_LOCK: Mutex<()> = Mutex::new(());
const MAX_URL_BYTES: usize = 512;
#[cfg(desktop)]
const MAX_HEADER_BYTES: usize = 8 * 1024;
#[cfg(desktop)]
const MAX_BODY_BYTES: usize = 1024;
const MAX_CLAIM_RESPONSE_BYTES: usize = 1024;
#[cfg(desktop)]
const MAX_REQUEST_LINE_BYTES: usize = MAX_URL_BYTES + 32;
#[cfg(desktop)]
const MAX_REQUEST_HEAD_BYTES: usize = MAX_REQUEST_LINE_BYTES + 2 + MAX_HEADER_BYTES + 4;
#[cfg(desktop)]
const CONNECTION_READ_POLL_TIMEOUT: Duration = Duration::from_millis(250);
#[cfg(desktop)]
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(desktop)]
const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(2);
#[cfg(desktop)]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(desktop)]
const RESPONSE_COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PERSISTED_CREDENTIAL_BYTES: u64 = 4096;
const PERSISTED_CREDENTIAL_SCHEMA: &str = "risunest.peer-clone-credential/v1";
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(desktop)]
const LOGICAL_OBJECT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(desktop)]
pub struct LanPairing {
    pub session_id: String,
    pub manifest_id: String,
    pub claim: String,
}

pub struct LanCloneClient {
    client: reqwest::blocking::Client,
    ranges: HttpRangeStream,
    control_timeout: Duration,
    endpoint: String,
    session_id: String,
    session_url: String,
    pub device_id: String,
    bearer: String,
    manifest_id: Option<String>,
}

impl LanCloneClient {
    pub fn claim(endpoint: &str, session_id: &str, claim: &str) -> Result<Self, PeerSyncError> {
        Self::claim_with_timeout(endpoint, session_id, claim, CONTROL_REQUEST_TIMEOUT)
    }

    fn claim_with_timeout(
        endpoint: &str,
        session_id: &str,
        claim: &str,
        control_timeout: Duration,
    ) -> Result<Self, PeerSyncError> {
        if endpoint.len() > MAX_URL_BYTES
            || uuid::Uuid::parse_str(session_id)
                .map(|value| value.to_string() != session_id)
                .unwrap_or(true)
            || !is_lower_hex_256(claim)
        {
            return Err(PeerSyncError::Protocol(
                "invalid LAN pairing data".to_owned(),
            ));
        }
        let endpoint = validate_lan_endpoint(endpoint)?;
        let session_url = format!("{endpoint}/v1/sessions/{session_id}");
        if session_url.len() > MAX_URL_BYTES {
            return Err(PeerSyncError::Protocol(
                "LAN session URL is too long".to_owned(),
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(control_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(transport)?;
        let ranges = HttpRangeStream::new(Duration::from_secs(3))?;
        let response = client
            .post(format!("{session_url}/claim"))
            .json(&ClaimRequest {
                claim: claim.to_owned(),
                device_id: None,
            })
            .timeout(control_timeout)
            .send()
            .map_err(transport)?;
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
                "LAN claim response is too large".to_owned(),
            ));
        }
        let response: ClaimResponseOwned = serde_json::from_slice(&body)
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        if !is_canonical_uuid(&response.device_id)
            || response.bearer.len() != 64
            || !is_lower_hex_256(&response.bearer)
            || response.permission != "clone-read"
        {
            return Err(PeerSyncError::Protocol(
                "invalid LAN claim response".to_owned(),
            ));
        }
        Ok(Self {
            client,
            ranges,
            control_timeout,
            endpoint,
            session_id: session_id.to_owned(),
            session_url,
            device_id: response.device_id,
            bearer: response.bearer,
            manifest_id: None,
        })
    }

    pub fn claim_and_persist(
        credential_path: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        validate_object_hash(manifest_id)?;
        let mut client = Self::claim(endpoint, session_id, claim)?;
        client.manifest_id = Some(manifest_id.to_owned());
        client.persist(credential_path)?;
        Ok(client)
    }

    pub fn open_persisted(credential_path: &Path) -> Result<Self, PeerSyncError> {
        let metadata = fs::symlink_metadata(credential_path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_PERSISTED_CREDENTIAL_BYTES
        {
            return Err(PeerSyncError::Storage(
                "invalid persisted LAN clone credential file".to_owned(),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(credential_path)?
            .take(MAX_PERSISTED_CREDENTIAL_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PERSISTED_CREDENTIAL_BYTES {
            return Err(PeerSyncError::Storage(
                "persisted LAN clone credential is too large".to_owned(),
            ));
        }
        let persisted: PersistedLanCredential = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        persisted.validate()?;
        Self::from_persisted(persisted)
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

    pub fn session_url(&self) -> &str {
        &self.session_url
    }

    pub(crate) fn target_identity(&self) -> Result<(&str, &str, &str), PeerSyncError> {
        let manifest_id = self.manifest_id.as_deref().ok_or_else(|| {
            PeerSyncError::Protocol("LAN clone manifest identity is missing".to_owned())
        })?;
        Ok((&self.endpoint, &self.session_id, manifest_id))
    }

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

    pub fn fetch_manifest(&self, expected_manifest_id: &str) -> Result<Vec<u8>, PeerSyncError> {
        if !is_lower_hex_256(expected_manifest_id) {
            return Err(PeerSyncError::Protocol(
                "invalid LAN manifest identity".to_owned(),
            ));
        }
        let response = self
            .control_request(self.client.get(format!("{}/manifest", self.session_url)))
            .send()
            .map_err(transport)?;
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
        Ok(bytes)
    }

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

    pub fn report_progress(
        &self,
        verified_bytes: u64,
        current_object: Option<&str>,
    ) -> Result<(), PeerSyncError> {
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
        let parent = credential_path.parent().ok_or_else(|| {
            PeerSyncError::Storage("LAN clone credential path has no parent".to_owned())
        })?;
        ensure_credential_parent(parent)?;
        let temporary = parent.join(format!(".peer-credential-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = create_owner_only_credential_file(&temporary)?;
            file.write_all(&bytes)?;
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
            manifest_id: Some(persisted.manifest_id),
        })
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
    permission: String,
}

impl PersistedLanCredential {
    fn validate(&self) -> Result<(), PeerSyncError> {
        if self.schema != PERSISTED_CREDENTIAL_SCHEMA
            || !is_canonical_uuid(&self.session_id)
            || !is_canonical_uuid(&self.device_id)
            || !is_lower_hex_256(&self.manifest_id)
            || !is_lower_hex_256(&self.bearer)
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
#[cfg(desktop)]
pub struct LanDevice {
    pub device_id: String,
    pub verified_bytes: u64,
    pub current_object: Option<String>,
    pub last_seen_unix_ms: u128,
    pub revoked: bool,
}

#[cfg(desktop)]
struct DeviceState {
    info: LanDevice,
    bearer_digest: [u8; 32],
}

#[cfg(desktop)]
struct ClaimState {
    digest: [u8; 32],
    expires_at: Instant,
    consumed: bool,
}

#[cfg(desktop)]
struct TunnelProbeState {
    digest: [u8; 32],
    expected_body: [u8; 32],
}

#[cfg(desktop)]
pub(crate) struct TunnelOriginProbe {
    pub(crate) path_prefix: String,
    pub(crate) path: String,
    pub(crate) expected_body: [u8; 32],
}

#[cfg(desktop)]
struct LanShared {
    session: LanSession,
    manifest_bytes: Arc<[u8]>,
    claim: Mutex<Option<ClaimState>>,
    tunnel_probe: Mutex<Option<TunnelProbeState>>,
    devices: Mutex<BTreeMap<String, DeviceState>>,
}

#[cfg(desktop)]
pub struct PreparedLogicalLanSession {
    session_id: String,
    source_device_id: String,
    manifest_id: String,
    manifest_bytes: Arc<[u8]>,
    objects: BTreeMap<String, u64>,
    source: Mutex<Box<dyn LogicalDeltaObjectSource + Send>>,
}

#[cfg(desktop)]
pub(crate) struct PreparedBidirectionalLogicalLanSession {
    logical: PreparedLogicalLanSession,
    control: Arc<dyn LanBidirectionalControl>,
}

#[cfg(desktop)]
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

#[cfg(desktop)]
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
            objects: object_sizes,
            source: Mutex::new(source),
        })
    }
}

#[cfg(desktop)]
enum LanSession {
    Clone(PreparedCloneSession),
    Logical(PreparedLogicalLanSession),
    BidirectionalLogical(PreparedBidirectionalLogicalLanSession),
}

#[cfg(desktop)]
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
            Self::Logical(session) => session.objects.get(hash).copied(),
            Self::BidirectionalLogical(session) => session.logical.objects.get(hash).copied(),
        }
    }

    fn bidirectional_control(&self) -> Option<&Arc<dyn LanBidirectionalControl>> {
        match self {
            Self::BidirectionalLogical(session) => Some(&session.control),
            Self::Clone(_) | Self::Logical(_) => None,
        }
    }
}

#[cfg(desktop)]
#[derive(Clone)]
pub(crate) struct LanCloneHostControl {
    shared: Weak<LanShared>,
}

#[cfg(desktop)]
impl LanCloneHostControl {
    pub(crate) fn devices(&self) -> Vec<LanDevice> {
        let Some(shared) = self.shared.upgrade() else {
            return Vec::new();
        };
        let devices = shared
            .devices
            .lock()
            .unwrap()
            .values()
            .map(|device| device.info.clone())
            .collect();
        devices
    }

    pub(crate) fn revoke(&self, device_id: &str) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        let mut devices = shared.devices.lock().unwrap();
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

#[cfg(desktop)]
pub struct LanCloneHost {
    shared: Arc<LanShared>,
    address: Option<SocketAddr>,
    stopped: Option<Arc<AtomicBool>>,
    active_connection: Option<Arc<Mutex<Option<TcpStream>>>>,
    thread: Option<JoinHandle<Result<(), PeerSyncError>>>,
}

#[cfg(desktop)]
impl LanCloneHost {
    pub fn prepare(session: PreparedCloneSession) -> Self {
        let manifest_bytes = Arc::from(session.manifest_bytes());
        Self {
            shared: Arc::new(LanShared {
                manifest_bytes,
                session: LanSession::Clone(session),
                claim: Mutex::new(None),
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
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
                manifest_bytes: Arc::clone(&session.manifest_bytes),
                session: LanSession::Logical(session),
                claim: Mutex::new(None),
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
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
                manifest_bytes: Arc::clone(&session.logical.manifest_bytes),
                session: LanSession::BidirectionalLogical(session),
                claim: Mutex::new(None),
                tunnel_probe: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        }
    }

    pub fn start(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::UNSPECIFIED, 0)
    }

    pub(crate) fn start_quick_tunnel_origin(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::LOCALHOST, 0)
    }

    pub(crate) fn start_named_tunnel_origin(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::LOCALHOST, NAMED_TUNNEL_ORIGIN_PORT)
    }

    fn start_on(&mut self, bind_address: Ipv4Addr, port: u16) -> Result<LanPairing, PeerSyncError> {
        if self.thread.is_some() {
            return Err(PeerSyncError::Protocol(
                "LAN clone host is already running".to_owned(),
            ));
        }
        let claim = random_secret()?;
        *self.shared.claim.lock().unwrap() = Some(ClaimState {
            digest: digest(&claim),
            expires_at: Instant::now() + CLAIM_TTL,
            consumed: false,
        });
        let listener = TcpListener::bind((bind_address, port)).map_err(|error| {
            if bind_address == Ipv4Addr::LOCALHOST
                && port == NAMED_TUNNEL_ORIGIN_PORT
                && error.kind() == io::ErrorKind::AddrInUse
            {
                PeerSyncError::Transport(NAMED_TUNNEL_ORIGIN_UNAVAILABLE.to_owned())
            } else {
                transport(error)
            }
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
            session_id: self.shared.session.session_id().to_owned(),
            manifest_id: self.shared.session.manifest_id().to_owned(),
            claim: hex::encode(claim),
        })
    }

    pub fn address(&self) -> Option<SocketAddr> {
        self.address
    }

    pub fn manifest(&self) -> &super::CloneManifest {
        match &self.shared.session {
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
        *self.shared.tunnel_probe.lock().unwrap() = Some(TunnelProbeState {
            digest: digest(&secret),
            expected_body,
        });
        let path_prefix = format!(
            "/v1/sessions/{}/tunnel-check",
            self.shared.session.session_id()
        );
        let path = format!("{path_prefix}/{}", hex::encode(secret));
        Ok(TunnelOriginProbe {
            path_prefix,
            path,
            expected_body,
        })
    }

    pub(crate) fn clear_tunnel_probe(&self) {
        *self.shared.tunnel_probe.lock().unwrap() = None;
    }

    pub fn devices(&self) -> Vec<LanDevice> {
        self.control().devices()
    }

    pub fn revoke(&self, device_id: &str) -> bool {
        self.control().revoke(device_id)
    }

    pub fn stop(&mut self) -> Result<(), PeerSyncError> {
        if let Some(stopped) = &self.stopped {
            stopped.store(true, Ordering::SeqCst);
        }
        if let Some(active_connection) = &self.active_connection {
            if let Some(connection) = active_connection.lock().unwrap().take() {
                let _ = connection.shutdown(Shutdown::Both);
            }
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| PeerSyncError::Transport("LAN server thread panicked".to_owned()))??;
        }
        *self.shared.claim.lock().unwrap() = None;
        self.clear_tunnel_probe();
        self.shared.devices.lock().unwrap().clear();
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
}

#[cfg(desktop)]
impl Drop for LanCloneHost {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(desktop)]
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
            let mut active = active_connection.lock().unwrap();
            if stopped.load(Ordering::SeqCst) {
                let _ = shutdown_handle.shutdown(Shutdown::Both);
                break;
            }
            *active = Some(shutdown_handle);
        }
        let _ = handle_connection(stream, &shared, &stopped);
        active_connection.lock().unwrap().take();
    }
    Ok(())
}

#[cfg(desktop)]
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

#[cfg(desktop)]
struct HttpRequest {
    method: String,
    url: String,
    authorization: Option<String>,
    range: Option<String>,
    range_count: usize,
    body: Vec<u8>,
}

#[cfg(desktop)]
enum RequestReadError {
    Http(u16),
    Io(io::Error),
    Stopped,
}

#[cfg(desktop)]
fn handle_connection(
    mut stream: TcpStream,
    shared: &LanShared,
    stopped: &AtomicBool,
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

#[cfg(desktop)]
fn read_request(
    stream: &mut TcpStream,
    stopped: &AtomicBool,
) -> Result<HttpRequest, RequestReadError> {
    read_request_started(stream, stopped, Instant::now())
}

#[cfg(desktop)]
fn read_request_started(
    stream: &mut TcpStream,
    stopped: &AtomicBool,
    request_started: Instant,
) -> Result<HttpRequest, RequestReadError> {
    read_request_with_elapsed(stream, stopped, || request_started.elapsed())
}

#[cfg(desktop)]
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

#[cfg(desktop)]
fn handle_request(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    let prefix = format!("/v1/sessions/{}", shared.session.session_id());
    let tunnel_probe_prefix = format!("{prefix}/tunnel-check/");
    if let Some(candidate) = request
        .url
        .strip_prefix(&tunnel_probe_prefix)
        .map(str::to_owned)
    {
        return tunnel_probe(stream, request, shared, &candidate);
    }
    if request.url == format!("{prefix}/claim") {
        return claim(stream, request, shared);
    }
    let device = match authorize(&request, shared) {
        Ok(device) => device,
        Err(status) => return respond_empty(stream, status),
    };
    if request.url == format!("{prefix}/manifest") {
        if request.method != "GET" {
            return respond_empty(stream, 405);
        }
        return respond_bytes(
            stream,
            200,
            &[
                ("Content-Type", "application/json"),
                ("ETag", &quoted(shared.session.manifest_id())),
            ],
            &shared.manifest_bytes,
        );
    }
    if request.url == format!("{prefix}/progress") {
        if request.method != "POST" {
            return respond_empty(stream, 405);
        }
        return progress(stream, request, shared, &device);
    }
    if request.url == format!("{prefix}/registration") {
        return bidirectional_registration(stream, request, shared, &device);
    }
    if request.url == format!("{prefix}/remote-apply") {
        return bidirectional_remote_apply(stream, request, shared, &device);
    }
    let object_prefix = format!("{prefix}/objects/");
    let Some(object) = request.url.strip_prefix(&object_prefix).map(str::to_owned) else {
        return respond_empty(stream, 404);
    };
    if object.contains('/') || shared.session.object_size(&object).is_none() {
        return respond_empty(stream, 404);
    }
    match (&shared.session, request.method.as_str()) {
        (LanSession::Clone(_), "HEAD") => head(stream, shared, &object),
        (LanSession::Clone(_), "GET") => range(stream, &request, shared, &object, stopped),
        (LanSession::Logical(_) | LanSession::BidirectionalLogical(_), "HEAD") => {
            head(stream, shared, &object)
        }
        (LanSession::Logical(_) | LanSession::BidirectionalLogical(_), "GET") => {
            if request.range_count != 0 || request.range.is_some() || !request.body.is_empty() {
                return respond_empty(stream, 400);
            }
            logical_object(stream, shared, &device, &object, stopped)
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
    let mut probe = shared.tunnel_probe.lock().unwrap();
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

#[cfg(desktop)]
fn claim(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
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
    let device_id = match &shared.session {
        LanSession::BidirectionalLogical(_) => match request_body.device_id.as_deref() {
            Some(device_id) if is_canonical_uuid(device_id) => device_id.to_owned(),
            _ => return respond_empty(stream, 400),
        },
        LanSession::Clone(_) | LanSession::Logical(_) => uuid::Uuid::new_v4().to_string(),
    };
    let mut claim = shared.claim.lock().unwrap();
    let Some(claim) = claim.as_mut() else {
        return respond_empty(stream, 410);
    };
    if claim.consumed || Instant::now() >= claim.expires_at {
        return respond_empty(stream, 410);
    }
    if !constant_time_eq(&claim.digest, &digest(&secret)) {
        return respond_empty(stream, 403);
    }
    claim.consumed = true;
    let bearer = random_secret()?;
    let response = ClaimResponse {
        device_id: device_id.clone(),
        bearer: hex::encode(bearer),
        permission: shared.session.permission(),
        source_device_id: shared.session.source_device_id(),
    };
    shared.devices.lock().unwrap().insert(
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
        },
    );
    respond_json(stream, 200, &response)
}

#[cfg(desktop)]
fn authorize(request: &HttpRequest, shared: &LanShared) -> Result<String, u16> {
    let value = request.authorization.as_deref().ok_or(401_u16)?;
    let Some(bearer) = value.strip_prefix("Bearer ") else {
        return Err(401);
    };
    if bearer.len() != 64 {
        return Err(401);
    }
    let candidate = digest(bearer.as_bytes());
    let mut devices = shared.devices.lock().unwrap();
    for (id, device) in devices.iter_mut() {
        if constant_time_eq(&device.bearer_digest, &candidate) {
            if device.info.revoked {
                return Err(403);
            }
            device.info.last_seen_unix_ms = now_ms();
            return Ok(id.clone());
        }
    }
    Err(401)
}

#[cfg(desktop)]
fn progress(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    let Ok(progress) = serde_json::from_slice::<ProgressRequest>(&request.body) else {
        return respond_empty(stream, 400);
    };
    if progress
        .current_object
        .as_deref()
        .is_some_and(|object| shared.session.object_size(object).is_none())
    {
        return respond_empty(stream, 400);
    }
    if let Some(device) = shared.devices.lock().unwrap().get_mut(device_id) {
        device.info.verified_bytes = progress.verified_bytes;
        device.info.current_object = progress.current_object;
        device.info.last_seen_unix_ms = now_ms();
    }
    respond_empty(stream, 204)
}

#[cfg(desktop)]
fn authenticated_bidirectional_session(
    shared: &LanShared,
    target_device_id: &str,
) -> Option<LanBidirectionalSession> {
    shared
        .session
        .bidirectional_control()
        .and_then(|_| shared.session.source_device_id())
        .map(|source_device_id| LanBidirectionalSession {
            session_id: shared.session.session_id().to_owned(),
            source_device_id: source_device_id.to_owned(),
            target_device_id: target_device_id.to_owned(),
        })
}

#[cfg(desktop)]
fn bidirectional_registration(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    let Some(session) = authenticated_bidirectional_session(shared, device_id) else {
        return respond_empty(stream, 404);
    };
    let Ok(request) = serde_json::from_slice::<LanBidirectionalRegistrationRequest>(&request.body)
    else {
        return respond_empty(stream, 400);
    };
    if !request.is_valid() {
        return respond_empty(stream, 400);
    }
    let control = shared
        .session
        .bidirectional_control()
        .expect("checked bidirectional session");
    match control.register(session, request) {
        Ok(()) => respond_empty(stream, 204),
        Err(_) => respond_empty(stream, 409),
    }
}

#[cfg(desktop)]
fn bidirectional_remote_apply(
    stream: &mut TcpStream,
    request: HttpRequest,
    shared: &LanShared,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    if request.method != "POST" {
        return respond_empty(stream, 405);
    }
    let Some(session) = authenticated_bidirectional_session(shared, device_id) else {
        return respond_empty(stream, 404);
    };
    let Ok(request) = serde_json::from_slice::<LanBidirectionalRemoteApplyRequest>(&request.body)
    else {
        return respond_empty(stream, 400);
    };
    if !request.is_valid() {
        return respond_empty(stream, 400);
    }
    let control = shared
        .session
        .bidirectional_control()
        .expect("checked bidirectional session");
    match control.remote_apply(session, request) {
        Ok(receipt) if receipt.is_valid() => respond_json(stream, 200, &receipt),
        Err(PeerSyncError::ActivationConflict { .. } | PeerSyncError::StaleManifest { .. }) => {
            respond_empty(stream, 409)
        }
        Ok(_) | Err(_) => respond_empty(stream, 500),
    }
}

#[cfg(desktop)]
fn head(stream: &mut TcpStream, shared: &LanShared, object: &str) -> Result<(), PeerSyncError> {
    let size = shared
        .session
        .object_size(object)
        .ok_or_else(|| PeerSyncError::Storage("session object descriptor is missing".to_owned()))?;
    let range_header = match &shared.session {
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

#[cfg(desktop)]
fn range(
    stream: &mut TcpStream,
    request: &HttpRequest,
    shared: &LanShared,
    object: &str,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    if request.range_count != 1 {
        return respond_empty(stream, 416);
    }
    let Some((start, end)) = request.range.as_deref().and_then(parse_range) else {
        return respond_empty(stream, 416);
    };
    let LanSession::Clone(session) = &shared.session else {
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

#[cfg(desktop)]
fn logical_object(
    stream: &mut TcpStream,
    shared: &LanShared,
    device_id: &str,
    object: &str,
    stopped: &AtomicBool,
) -> Result<(), PeerSyncError> {
    let session = match &shared.session {
        LanSession::Logical(session) => session,
        LanSession::BidirectionalLogical(session) => &session.logical,
        LanSession::Clone(_) => return respond_empty(stream, 405),
    };
    let size = *session.objects.get(object).ok_or_else(|| {
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
    result
}

#[cfg(desktop)]
fn set_current_object(shared: &LanShared, device_id: &str, object: Option<&str>) {
    if let Some(device) = shared.devices.lock().unwrap().get_mut(device_id) {
        device.info.current_object = object.map(str::to_owned);
        device.info.last_seen_unix_ms = now_ms();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRequest {
    claim: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
}

#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LanBidirectionalSession {
    pub(crate) session_id: String,
    pub(crate) source_device_id: String,
    pub(crate) target_device_id: String,
}

#[cfg(desktop)]
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
    ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError>;
}

#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalGeneration {
    pub(crate) generation_id: String,
    pub(crate) manifest_hash: String,
    pub(crate) generation_sequence: String,
}

#[cfg(desktop)]
impl LanBidirectionalGeneration {
    fn is_valid(&self) -> bool {
        !self.generation_id.is_empty()
            && self.generation_id.len() <= MAX_BODY_BYTES
            && is_lower_hex_256(&self.manifest_hash)
            && is_canonical_decimal(&self.generation_sequence)
    }
}

#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalRegistrationRequest {
    pub(crate) library_id: String,
    pub(crate) generation: LanBidirectionalGeneration,
    pub(crate) expected_revision: i64,
}

#[cfg(desktop)]
impl LanBidirectionalRegistrationRequest {
    fn is_valid(&self) -> bool {
        !self.library_id.is_empty()
            && self.library_id.len() <= MAX_BODY_BYTES
            && self.expected_revision >= 0
            && self.generation.is_valid()
    }
}

#[cfg(desktop)]
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
}

#[cfg(desktop)]
impl LanBidirectionalRemoteApplyRequest {
    fn is_valid(&self) -> bool {
        is_canonical_uuid(&self.operation_id)
            && validate_lan_endpoint(&self.source_endpoint).is_ok()
            && is_canonical_uuid(&self.source_session_id)
            && is_lower_hex_256(&self.source_manifest_id)
            && is_lower_hex_256(&self.source_claim)
            && self.expected_source_revision >= 0
            && self.expected_source_generation.is_valid()
            && is_lower_hex_256(&self.expected_common_base_manifest_hash)
    }
}

#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalBackupReceipt {
    pub(crate) package_id: String,
    pub(crate) path: String,
}

#[cfg(desktop)]
impl LanBidirectionalBackupReceipt {
    fn is_valid(&self) -> bool {
        !self.package_id.is_empty()
            && self.package_id.len() <= MAX_BODY_BYTES
            && !self.path.is_empty()
            && self.path.len() <= MAX_BODY_BYTES
    }
}

#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanBidirectionalRemoteApplyReceipt {
    pub(crate) committed_revision: i64,
    pub(crate) committed_generation: LanBidirectionalGeneration,
    pub(crate) transferred_objects: u64,
    pub(crate) transferred_bytes: u64,
    pub(crate) backup: Option<LanBidirectionalBackupReceipt>,
}

#[cfg(desktop)]
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
#[cfg(desktop)]
struct ClaimResponse<'a> {
    device_id: String,
    bearer: String,
    permission: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_device_id: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponseOwned {
    device_id: String,
    bearer: String,
    permission: String,
    source_device_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressRequest {
    verified_bytes: u64,
    current_object: Option<String>,
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

pub(crate) fn validate_lan_endpoint(value: &str) -> Result<String, PeerSyncError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| PeerSyncError::Protocol("invalid LAN endpoint".to_owned()))?;
    let valid_lan_ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
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
}

fn is_lower_hex_256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
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

#[cfg(desktop)]
fn random_secret() -> Result<[u8; 32], PeerSyncError> {
    let mut secret = [0; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
    Ok(secret)
}
#[cfg(desktop)]
fn digest(bytes: impl AsRef<[u8]>) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
#[cfg(desktop)]
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}
#[cfg(desktop)]
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
#[cfg(desktop)]
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
#[cfg(desktop)]
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
#[cfg(desktop)]
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
#[cfg(desktop)]
fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}
#[cfg(desktop)]
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
#[cfg(desktop)]
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
#[cfg(desktop)]
fn respond_empty(stream: &mut TcpStream, status: u16) -> Result<(), PeerSyncError> {
    write_response_head(stream, status, &[], 0)
}
#[cfg(desktop)]
fn respond_bytes(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<(), PeerSyncError> {
    write_response_head(stream, status, headers, body.len() as u64)?;
    stream.write_all(body).map_err(transport)
}
#[cfg(desktop)]
fn respond_json<T: Serialize>(
    stream: &mut TcpStream,
    status: u16,
    value: &T,
) -> Result<(), PeerSyncError> {
    let body =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    respond_bytes(
        stream,
        status,
        &[("Content-Type", "application/json")],
        &body,
    )
}
#[cfg(desktop)]
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
    use crate::peer_sync::{
        logical_delta::{
            build_logical_manifest, LogicalManifestBuilderInput, LogicalRecordEnvelope,
            LogicalRecordLocator, ProjectedLogicalRecord,
        },
        LogicalDeltaObject, LogicalDeltaObjectSource,
    };
    use std::collections::BTreeMap;
    use std::io::Cursor;

    static LOGICAL_LAN_TEST_LOCK: Mutex<()> = Mutex::new(());

    const TEST_SESSION_ID: &str = "00000000-0000-4000-8000-000000000000";
    const TEST_BEARER: &str = "0000000000000000000000000000000000000000000000000000000000000000";

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
            verified_bytes: Arc::new(Mutex::new(0)),
        }
    }

    fn finish_response(stream: &mut TcpStream) {
        let _ = stream.shutdown(Shutdown::Write);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut drain = [0_u8; 64];
        while stream.read(&mut drain).is_ok_and(|read| read != 0) {}
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
    fn stalled_claim_uses_the_short_control_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(800));
        });
        let started = Instant::now();

        let error = LanCloneClient::claim_with_timeout(
            &endpoint,
            "00000000-0000-4000-8000-000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            Duration::from_millis(150),
        )
        .err()
        .unwrap();
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert!(matches!(error, PeerSyncError::Transport(_)));
        assert!(elapsed < Duration::from_millis(500));
    }

    #[test]
    fn logical_object_body_stall_uses_a_bounded_idle_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let bytes = b"stalled logical object".to_vec();
        let object_hash = sha256_hex(&bytes);
        let object_size = bytes.len() as u64;
        let response_etag = quoted(&object_hash);
        let address = listener.local_addr().unwrap();
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
            thread::sleep(Duration::from_millis(1_000));
            let _ = stream.write_all(&bytes);
        });
        let mut client = direct_logical_client(
            address,
            Duration::from_millis(100),
            Duration::from_millis(120),
        );
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object_hash,
                size: object_size,
            })
            .unwrap();
        let started = Instant::now();
        let error = reader.read_to_end(&mut Vec::new()).unwrap_err();
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::Other
        ));
        assert!(
            elapsed < Duration::from_millis(800),
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
            for chunk in server_bytes.chunks(8) {
                thread::sleep(Duration::from_millis(80));
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
            }
            finish_response(&mut stream);
        });
        let control_timeout = Duration::from_millis(100);
        let mut client =
            direct_logical_client(address, control_timeout, Duration::from_millis(200));
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: object_hash,
                size: bytes.len() as u64,
            })
            .unwrap();
        let started = Instant::now();
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

    struct DelayedLogicalFixtureSource {
        object_hash: String,
        bytes: Vec<u8>,
        delay: Duration,
    }

    impl LogicalDeltaObjectSource for DelayedLogicalFixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            if object.hash != self.object_hash {
                return Err(PeerSyncError::Storage(
                    "delayed logical fixture object is absent".to_owned(),
                ));
            }
            Ok(Box::new(DelayedFirstRead {
                inner: Cursor::new(self.bytes.clone()),
                delay: Some(self.delay),
            }))
        }
    }

    struct DelayedFirstRead {
        inner: Cursor<Vec<u8>>,
        delay: Option<Duration>,
    }

    impl Read for DelayedFirstRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if let Some(delay) = self.delay.take() {
                thread::sleep(delay);
            }
            self.inner.read(output)
        }
    }

    #[derive(Default)]
    struct BidirectionalControlFixture {
        registrations: Mutex<Vec<LanBidirectionalSession>>,
        remote_apply_delay: Mutex<Option<Duration>>,
        remote_apply_error: Mutex<Option<PeerSyncError>>,
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
        ) -> Result<LanBidirectionalRemoteApplyReceipt, PeerSyncError> {
            if let Some(delay) = self.remote_apply_delay.lock().unwrap().take() {
                thread::sleep(delay);
            }
            if let Some(error) = self.remote_apply_error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(LanBidirectionalRemoteApplyReceipt {
                committed_revision: request.expected_source_revision,
                committed_generation: request.expected_source_generation,
                transferred_objects: 0,
                transferred_bytes: 0,
                backup: None,
            })
        }
    }

    fn prepared_bidirectional_logical_session(
        session_id: &str,
        source_device_id: &str,
        control: Arc<BidirectionalControlFixture>,
    ) -> PreparedBidirectionalLogicalLanSession {
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

    #[test]
    fn p5_remote_apply_waits_past_the_short_control_timeout_for_a_terminal_response() {
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
        let control = Arc::new(BidirectionalControlFixture::default());
        *control.remote_apply_delay.lock().unwrap() = Some(Duration::from_millis(5_100));
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
            })
            .unwrap();

        assert!(started.elapsed() > CONTROL_REQUEST_TIMEOUT);
        assert_eq!(receipt.committed_generation, generation);
        host.stop().unwrap();
    }

    #[test]
    fn p5_claim_binds_the_stable_target_and_authenticates_control_callbacks() {
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
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
    fn p5_rejects_malformed_claimants_and_wrong_logical_permissions() {
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
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
                Some("00000000-0000-4000-8000-000000000076"),
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
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
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

        assert!(host.revoke(&client.device_id));
        assert!(matches!(
            client.fetch_manifest(),
            Err(PeerSyncError::Transport(_))
        ));
        host.stop().unwrap();
    }

    #[test]
    fn logical_progress_never_marks_a_corrupt_object_as_verified() {
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
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
    fn backpressured_logical_object_opens_without_waiting_for_control_progress() {
        let _guard = LOGICAL_LAN_TEST_LOCK.lock().unwrap();
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
            Box::new(DelayedLogicalFixtureSource {
                object_hash: record_hash.clone(),
                bytes: record_bytes.clone(),
                delay: Duration::from_millis(600),
            }),
        )
        .unwrap();
        let mut host = LanCloneHost::prepare_logical(logical);
        let pairing = host.start_on(Ipv4Addr::LOCALHOST, 0).unwrap();
        let endpoint = format!("http://{}", host.address().unwrap());
        let control_timeout = Duration::from_millis(400);
        let mut client = LanLogicalDeltaClient::claim_with_timeouts(
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
            LogicalClientTimeouts {
                control_request: control_timeout,
                object_idle: Duration::from_secs(2),
            },
        )
        .unwrap();

        let started = Instant::now();
        let mut reader = client
            .open_object(&LogicalDeltaObject {
                hash: record_hash.clone(),
                size: record_bytes.len() as u64,
            })
            .unwrap();
        let opened_in = started.elapsed();
        let current_while_streaming = host.devices()[0].current_object.clone();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();

        assert!(
            opened_in < control_timeout,
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

#[cfg(desktop)]
pub struct LanLogicalDeltaClient {
    control_client: reqwest::blocking::Client,
    object_client: reqwest::blocking::Client,
    control_timeout: Duration,
    session_url: String,
    pub device_id: String,
    bearer: String,
    source_device_id: String,
    manifest_id: String,
    verified_bytes: Arc<Mutex<u64>>,
}

#[cfg(desktop)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogicalRequestKind {
    Control,
    Object,
}

#[cfg(desktop)]
#[derive(Clone, Copy)]
struct LogicalClientTimeouts {
    control_request: Duration,
    object_idle: Duration,
}

#[cfg(desktop)]
fn logical_request_timeout(kind: LogicalRequestKind, timeouts: LogicalClientTimeouts) -> Duration {
    match kind {
        LogicalRequestKind::Control => timeouts.control_request,
        LogicalRequestKind::Object => timeouts.object_idle,
    }
}

#[cfg(desktop)]
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

#[cfg(desktop)]
fn build_bidirectional_remote_apply_client() -> Result<reqwest::blocking::Client, PeerSyncError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(transport)
}

#[cfg(desktop)]
impl LanLogicalDeltaClient {
    pub fn claim(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        Self::claim_with_timeouts(
            endpoint,
            session_id,
            manifest_id,
            claim,
            LogicalClientTimeouts {
                control_request: CONTROL_REQUEST_TIMEOUT,
                object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
            },
        )
    }

    fn claim_with_timeouts(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        timeouts: LogicalClientTimeouts,
    ) -> Result<Self, PeerSyncError> {
        Self::claim_with_timeouts_and_device(
            endpoint,
            session_id,
            manifest_id,
            claim,
            None,
            "logical-read",
            timeouts,
        )
    }

    fn claim_with_timeouts_and_device(
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
        device_id: Option<&str>,
        permission: &str,
        timeouts: LogicalClientTimeouts,
    ) -> Result<Self, PeerSyncError> {
        if endpoint.len() > MAX_URL_BYTES
            || !is_canonical_uuid(session_id)
            || !is_lower_hex_256(manifest_id)
            || !is_lower_hex_256(claim)
            || device_id.is_some_and(|value| !is_canonical_uuid(value))
        {
            return Err(PeerSyncError::Protocol(
                "invalid logical delta pairing data".to_owned(),
            ));
        }
        let endpoint = validate_lan_endpoint(endpoint)?;
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
                device_id: device_id.map(str::to_owned),
            })
            .timeout(control_timeout)
            .send()
            .map_err(transport)?;
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
        let source_device_id = response.source_device_id.ok_or_else(|| {
            PeerSyncError::Protocol("logical delta source device identity is missing".to_owned())
        })?;
        if !is_canonical_uuid(&response.device_id)
            || device_id.is_some_and(|expected| response.device_id.as_str() != expected)
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
            verified_bytes: Arc::new(Mutex::new(0)),
        })
    }

    pub fn source_device_id(&self) -> &str {
        &self.source_device_id
    }

    pub fn fetch_manifest(&self) -> Result<Vec<u8>, PeerSyncError> {
        let response = self
            .authorized_control(
                self.control_client
                    .get(format!("{}/manifest", self.session_url)),
            )
            .send()
            .map_err(transport)?;
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
        Ok(bytes)
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
            verified_bytes: Arc::clone(&self.verified_bytes),
        }
    }
}

#[cfg(desktop)]
impl LogicalDeltaObjectSource for LanLogicalDeltaClient {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        validate_object_hash(&object.hash)?;
        let response = self
            .object_client
            .get(format!("{}/objects/{}", self.session_url, object.hash))
            .bearer_auth(&self.bearer)
            .send()
            .map_err(transport)?;
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

#[cfg(desktop)]
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

#[cfg(desktop)]
impl LanBidirectionalLogicalCredential {
    fn validate(&self) -> Result<String, PeerSyncError> {
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
        validate_lan_endpoint(&self.endpoint)
    }
}

#[cfg(desktop)]
pub(crate) struct LanBidirectionalLogicalClient {
    inner: LanLogicalDeltaClient,
    remote_apply_client: reqwest::blocking::Client,
    endpoint: String,
    session_id: String,
}

#[cfg(desktop)]
impl LanBidirectionalLogicalClient {
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
            Some(device_id),
            "logical-bidirectional",
            LogicalClientTimeouts {
                control_request: CONTROL_REQUEST_TIMEOUT,
                object_idle: LOGICAL_OBJECT_IDLE_TIMEOUT,
            },
        )?;
        Ok(Self {
            remote_apply_client: build_bidirectional_remote_apply_client()?,
            endpoint: validate_lan_endpoint(endpoint)?,
            session_id: session_id.to_owned(),
            inner,
        })
    }

    pub(crate) fn resume(
        credential: LanBidirectionalLogicalCredential,
    ) -> Result<Self, PeerSyncError> {
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
                verified_bytes: Arc::new(Mutex::new(0)),
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

    pub(crate) fn device_id(&self) -> &str {
        &self.inner.device_id
    }

    pub(crate) fn source_device_id(&self) -> &str {
        self.inner.source_device_id()
    }

    pub(crate) fn fetch_manifest(&self) -> Result<Vec<u8>, PeerSyncError> {
        self.inner.fetch_manifest()
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
        let response = self
            .remote_apply_client
            .post(format!("{}/remote-apply", self.inner.session_url))
            .bearer_auth(&self.inner.bearer)
            .json(&request)
            .send()
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
        response
            .take(MAX_BODY_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .map_err(transport)?;
        if body.len() > MAX_BODY_BYTES {
            return Err(PeerSyncError::Protocol(
                "bidirectional remote apply response is too large".to_owned(),
            ));
        }
        let receipt = serde_json::from_slice::<LanBidirectionalRemoteApplyReceipt>(&body)
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        if !receipt.is_valid() {
            return Err(PeerSyncError::Protocol(
                "invalid bidirectional remote apply response".to_owned(),
            ));
        }
        Ok(receipt)
    }
}

#[cfg(desktop)]
impl LogicalDeltaObjectSource for LanBidirectionalLogicalClient {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        self.inner.open_object(object)
    }
}

#[cfg(desktop)]
struct LogicalProgressReporter {
    client: reqwest::blocking::Client,
    control_timeout: Duration,
    session_url: String,
    bearer: String,
    verified_bytes: Arc<Mutex<u64>>,
}

#[cfg(desktop)]
impl LogicalProgressReporter {
    fn verified_bytes(&self) -> Result<u64, PeerSyncError> {
        self.verified_bytes
            .lock()
            .map(|value| *value)
            .map_err(|error| {
                PeerSyncError::Storage(format!("logical progress mutex poisoned: {error}"))
            })
    }

    fn report_current(&self, current_object: Option<&str>) -> Result<(), PeerSyncError> {
        let response = self
            .client
            .post(format!("{}/progress", self.session_url))
            .bearer_auth(&self.bearer)
            .json(&ProgressRequest {
                verified_bytes: self.verified_bytes()?,
                current_object: current_object.map(str::to_owned),
            })
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

    fn complete_object(&self, size: u64) -> Result<(), PeerSyncError> {
        {
            let mut verified = self.verified_bytes.lock().map_err(|error| {
                PeerSyncError::Storage(format!("logical progress mutex poisoned: {error}"))
            })?;
            *verified = verified.checked_add(size).ok_or_else(|| {
                PeerSyncError::Validation("logical progress byte count overflow".to_owned())
            })?;
        }
        self.report_current(None)
    }
}

#[cfg(desktop)]
struct LogicalProgressReader {
    inner: reqwest::blocking::Response,
    reporter: LogicalProgressReporter,
    expected_hash: String,
    expected_size: u64,
    received_size: u64,
    hasher: Sha256,
    completed: bool,
}

#[cfg(desktop)]
impl LogicalProgressReader {
    fn finish_if_verified(&mut self) {
        if self.completed {
            return;
        }
        let hash = hex::encode(self.hasher.clone().finalize());
        if self.received_size == self.expected_size && hash == self.expected_hash {
            self.completed = true;
            let _ = self.reporter.complete_object(self.expected_size);
        }
    }
}

#[cfg(desktop)]
impl Read for LogicalProgressReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return self.inner.read(output);
        }
        match self.inner.read(output) {
            Ok(0) => {
                self.finish_if_verified();
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
