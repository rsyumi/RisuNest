#[cfg(desktop)]
use super::PreparedCloneSession;
use super::{
    http_stream::HttpRangeStream,
    protocol::{sha256_hex, CLONE_CHUNK_SIZE, MAX_MANIFEST_BYTES},
    PeerSyncError,
};
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
        Arc, Mutex,
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
struct LanShared {
    session: PreparedCloneSession,
    manifest_bytes: Arc<[u8]>,
    claim: Mutex<Option<ClaimState>>,
    devices: Mutex<BTreeMap<String, DeviceState>>,
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
        Self {
            shared: Arc::new(LanShared {
                manifest_bytes: Arc::from(session.manifest_bytes()),
                session,
                claim: Mutex::new(None),
                devices: Mutex::new(BTreeMap::new()),
            }),
            address: None,
            stopped: None,
            active_connection: None,
            thread: None,
        }
    }

    pub fn start(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::UNSPECIFIED)
    }

    pub(crate) fn start_quick_tunnel_origin(&mut self) -> Result<LanPairing, PeerSyncError> {
        self.start_on(Ipv4Addr::LOCALHOST)
    }

    fn start_on(&mut self, bind_address: Ipv4Addr) -> Result<LanPairing, PeerSyncError> {
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
        let listener = TcpListener::bind((bind_address, 0)).map_err(transport)?;
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
            session_id: self.shared.session.manifest().session_id.clone(),
            manifest_id: self.shared.session.manifest_id().to_owned(),
            claim: hex::encode(claim),
        })
    }

    pub fn address(&self) -> Option<SocketAddr> {
        self.address
    }

    pub fn manifest(&self) -> &super::CloneManifest {
        self.shared.session.manifest()
    }

    pub fn devices(&self) -> Vec<LanDevice> {
        self.shared
            .devices
            .lock()
            .unwrap()
            .values()
            .map(|device| device.info.clone())
            .collect()
    }

    pub fn revoke(&self, device_id: &str) -> bool {
        let mut devices = self.shared.devices.lock().unwrap();
        let Some(device) = devices.get_mut(device_id) else {
            return false;
        };
        device.info.revoked = true;
        true
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
    let prefix = format!("/v1/sessions/{}", shared.session.manifest().session_id);
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
    let object_prefix = format!("{prefix}/objects/");
    let Some(object) = request.url.strip_prefix(&object_prefix).map(str::to_owned) else {
        return respond_empty(stream, 404);
    };
    if object.contains('/') || !shared.session.manifest().objects.contains_key(&object) {
        return respond_empty(stream, 404);
    }
    match request.method.as_str() {
        "HEAD" => head(stream, shared, &object),
        "GET" => range(stream, &request, shared, &object, stopped),
        _ => respond_empty(stream, 405),
    }
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
    let device_id = uuid::Uuid::new_v4().to_string();
    let response = ClaimResponse {
        device_id: device_id.clone(),
        bearer: hex::encode(bearer),
        permission: "clone-read",
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
        .is_some_and(|object| !shared.session.manifest().objects.contains_key(object))
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
fn head(stream: &mut TcpStream, shared: &LanShared, object: &str) -> Result<(), PeerSyncError> {
    let descriptor = &shared.session.manifest().objects[object];
    write_response_head(
        stream,
        200,
        &[("Accept-Ranges", "bytes"), ("ETag", &quoted(object))],
        descriptor.size,
    )
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
    let descriptor = &shared.session.manifest().objects[object];
    let Some(chunk) = descriptor
        .chunks
        .iter()
        .find(|chunk| chunk.offset == start && chunk.offset + chunk.size - 1 == end)
    else {
        return respond_empty(stream, 416);
    };
    let physical = shared
        .session
        .physical_hash(object)
        .ok_or_else(|| PeerSyncError::Storage("session object mapping is missing".to_owned()))?;
    let cas = PayloadCas::new(shared.session.root())?;
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

#[derive(Serialize, Deserialize)]
struct ClaimRequest {
    claim: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg(desktop)]
struct ClaimResponse {
    device_id: String,
    bearer: String,
    permission: &'static str,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponseOwned {
    device_id: String,
    bearer: String,
    permission: String,
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

fn validate_lan_endpoint(value: &str) -> Result<String, PeerSyncError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| PeerSyncError::Protocol("invalid LAN endpoint".to_owned()))?;
    let valid_ip = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !valid_ip
    {
        return Err(PeerSyncError::Protocol("invalid LAN endpoint".to_owned()));
    }
    let host = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        _ => unreachable!(),
    };
    Ok(format!("http://{host}:{}", url.port().unwrap()))
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
}
