use super::{
    protocol::{sha256_hex, CLONE_CHUNK_SIZE, MAX_MANIFEST_BYTES},
    PeerSyncError, PreparedCloneSession,
};
use crate::asset_repository::PayloadCas;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Read, Seek, SeekFrom},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const CLAIM_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_URL_BYTES: usize = 512;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 1024;

pub struct LanPairing {
    pub session_id: String,
    pub manifest_id: String,
    pub claim: String,
}

pub struct LanCloneClient {
    client: reqwest::blocking::Client,
    session_url: String,
    pub device_id: String,
    bearer: String,
}

impl LanCloneClient {
    pub fn claim(endpoint: &str, session_id: &str, claim: &str) -> Result<Self, PeerSyncError> {
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
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(transport)?;
        let response = client
            .post(format!("{session_url}/claim"))
            .json(&ClaimRequest {
                claim: claim.to_owned(),
            })
            .send()
            .map_err(transport)?;
        if !response.status().is_success() {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let response: ClaimResponseOwned = response.json().map_err(transport)?;
        if response.device_id.is_empty()
            || response.bearer.len() != 64
            || response.permission != "clone-read"
        {
            return Err(PeerSyncError::Protocol(
                "invalid LAN claim response".to_owned(),
            ));
        }
        Ok(Self {
            client,
            session_url,
            device_id: response.device_id,
            bearer: response.bearer,
        })
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
            .authenticated(self.client.get(format!("{}/manifest", self.session_url)))
            .send()
            .map_err(transport)?;
        if !response.status().is_success() {
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
            .authenticated(
                self.client
                    .head(format!("{}/objects/{object}", self.session_url)),
            )
            .send()
            .map_err(transport)?;
        if !response.status().is_success() {
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
        let response = self
            .authenticated(
                self.client
                    .get(format!("{}/objects/{object}", self.session_url))
                    .header(reqwest::header::RANGE, format!("bytes={start}-{end}")),
            )
            .send()
            .map_err(transport)?;
        if response.status().as_u16() != 206 {
            return Err(PeerSyncError::Transport(format!(
                "HTTP {}",
                response.status()
            )));
        }
        let expected_etag = quoted(object);
        let expected_range_prefix = format!("bytes {start}-{end}/");
        if response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            != Some(expected_etag.as_str())
            || !response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(&expected_range_prefix))
        {
            return Err(PeerSyncError::Protocol(
                "LAN range response headers are invalid".to_owned(),
            ));
        }
        let mut bytes = Vec::new();
        response
            .take(expected_size + 1)
            .read_to_end(&mut bytes)
            .map_err(transport)?;
        if bytes.len() as u64 != expected_size {
            return Err(PeerSyncError::Protocol(
                "LAN range body length is invalid".to_owned(),
            ));
        }
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
            .authenticated(
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

    fn authenticated(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        request.bearer_auth(&self.bearer)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LanDevice {
    pub device_id: String,
    pub verified_bytes: u64,
    pub current_object: Option<String>,
    pub last_seen_unix_ms: u128,
    pub revoked: bool,
}

struct DeviceState {
    info: LanDevice,
    bearer_digest: [u8; 32],
}

struct ClaimState {
    digest: [u8; 32],
    expires_at: Instant,
    consumed: bool,
}

struct LanShared {
    session: PreparedCloneSession,
    manifest_bytes: Arc<[u8]>,
    claim: Mutex<Option<ClaimState>>,
    devices: Mutex<BTreeMap<String, DeviceState>>,
}

pub struct LanCloneHost {
    shared: Arc<LanShared>,
    address: Option<SocketAddr>,
    stopped: Option<Arc<AtomicBool>>,
    thread: Option<JoinHandle<Result<(), PeerSyncError>>>,
}

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
            thread: None,
        }
    }

    pub fn start(&mut self) -> Result<LanPairing, PeerSyncError> {
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
        let server = Server::http((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| PeerSyncError::Transport("LAN server has no IP address".to_owned()))?;
        if !address.ip().is_unspecified() {
            return Err(PeerSyncError::Protocol(
                "LAN clone server must bind 0.0.0.0".to_owned(),
            ));
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(&self.shared);
        let thread_stopped = Arc::clone(&stopped);
        self.thread = Some(thread::spawn(move || serve(server, shared, thread_stopped)));
        self.address = Some(address);
        self.stopped = Some(stopped);
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
        if let Some(address) = self.address {
            let _ = TcpStream::connect_timeout(
                &(Ipv4Addr::LOCALHOST, address.port()).into(),
                Duration::from_millis(100),
            );
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
        Ok(())
    }

    #[cfg(test)]
    pub fn expire_claim_for_test(&self) {
        if let Some(claim) = self.shared.claim.lock().unwrap().as_mut() {
            claim.expires_at = Instant::now() - Duration::from_secs(1);
        }
    }
}

impl Drop for LanCloneHost {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn serve(
    server: Server,
    shared: Arc<LanShared>,
    stopped: Arc<AtomicBool>,
) -> Result<(), PeerSyncError> {
    while !stopped.load(Ordering::SeqCst) {
        let Some(request) = server
            .recv_timeout(Duration::from_millis(50))
            .map_err(|error| PeerSyncError::Transport(error.to_string()))?
        else {
            continue;
        };
        if !stopped.load(Ordering::SeqCst) {
            let _ = handle_request(request, &shared);
        }
    }
    Ok(())
}

fn handle_request(request: Request, shared: &LanShared) -> Result<(), PeerSyncError> {
    if request.url().len() > MAX_URL_BYTES
        || request
            .headers()
            .iter()
            .map(|header| header.field.as_str().len() + header.value.as_str().len())
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return respond_empty(request, 413);
    }
    let prefix = format!("/v1/sessions/{}", shared.session.manifest().session_id);
    if request.url() == format!("{prefix}/claim") {
        return claim(request, shared);
    }
    let device = match authorize(&request, shared) {
        Ok(device) => device,
        Err(status) => return respond_empty(request, status),
    };
    if request.url() == format!("{prefix}/manifest") {
        if request.method() != &Method::Get {
            return respond_empty(request, 405);
        }
        return request
            .respond(
                Response::new(
                    StatusCode(200),
                    vec![
                        header("content-type", "application/json")?,
                        header("etag", &quoted(shared.session.manifest_id()))?,
                    ],
                    io::Cursor::new(Arc::clone(&shared.manifest_bytes)),
                    Some(shared.session.manifest_bytes().len()),
                    None,
                )
                .with_chunked_threshold(usize::MAX),
            )
            .map_err(transport);
    }
    if request.url() == format!("{prefix}/progress") {
        if request.method() != &Method::Post {
            return respond_empty(request, 405);
        }
        return progress(request, shared, &device);
    }
    let object_prefix = format!("{prefix}/objects/");
    let Some(object) = request
        .url()
        .strip_prefix(&object_prefix)
        .map(str::to_owned)
    else {
        return respond_empty(request, 404);
    };
    if object.contains('/') || !shared.session.manifest().objects.contains_key(&object) {
        return respond_empty(request, 404);
    }
    match request.method() {
        Method::Head => head(request, shared, &object),
        Method::Get => range(request, shared, &object),
        _ => respond_empty(request, 405),
    }
}

fn claim(mut request: Request, shared: &LanShared) -> Result<(), PeerSyncError> {
    if request.method() != &Method::Post {
        return respond_empty(request, 405);
    }
    let Some(body) = read_small_body(request.as_reader()) else {
        return respond_empty(request, 413);
    };
    let Ok(request_body) = serde_json::from_slice::<ClaimRequest>(&body) else {
        return respond_empty(request, 400);
    };
    if request_body.claim.len() != 64 {
        return respond_empty(request, 403);
    }
    let Ok(secret) = hex::decode(request_body.claim) else {
        return respond_empty(request, 403);
    };
    if secret.len() != 32 {
        return respond_empty(request, 403);
    }
    let mut claim = shared.claim.lock().unwrap();
    let Some(claim) = claim.as_mut() else {
        return respond_empty(request, 410);
    };
    if claim.consumed || Instant::now() >= claim.expires_at {
        return respond_empty(request, 410);
    }
    if !constant_time_eq(&claim.digest, &digest(&secret)) {
        return respond_empty(request, 403);
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
    respond_json(request, 200, &response)
}

fn authorize(request: &Request, shared: &LanShared) -> Result<String, u16> {
    let value = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("authorization"))
        .map(|header| header.value.as_str())
        .ok_or(401_u16)?;
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

fn progress(
    mut request: Request,
    shared: &LanShared,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    let Some(body) = read_small_body(request.as_reader()) else {
        return respond_empty(request, 413);
    };
    let Ok(progress) = serde_json::from_slice::<ProgressRequest>(&body) else {
        return respond_empty(request, 400);
    };
    if progress
        .current_object
        .as_deref()
        .is_some_and(|object| !shared.session.manifest().objects.contains_key(object))
    {
        return respond_empty(request, 400);
    }
    if let Some(device) = shared.devices.lock().unwrap().get_mut(device_id) {
        device.info.verified_bytes = progress.verified_bytes;
        device.info.current_object = progress.current_object;
        device.info.last_seen_unix_ms = now_ms();
    }
    respond_empty(request, 204)
}

fn head(request: Request, shared: &LanShared, object: &str) -> Result<(), PeerSyncError> {
    let descriptor = &shared.session.manifest().objects[object];
    request
        .respond(
            Response::new(
                StatusCode(200),
                vec![
                    header("content-length", &descriptor.size.to_string())?,
                    header("accept-ranges", "bytes")?,
                    header("etag", &quoted(object))?,
                ],
                io::empty(),
                Some(descriptor.size as usize),
                None,
            )
            .with_chunked_threshold(usize::MAX),
        )
        .map_err(transport)
}

fn range(request: Request, shared: &LanShared, object: &str) -> Result<(), PeerSyncError> {
    let ranges: Vec<_> = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv("range"))
        .collect();
    if ranges.len() != 1 {
        return respond_empty(request, 416);
    }
    let Some((start, end)) = parse_range(ranges[0].value.as_str()) else {
        return respond_empty(request, 416);
    };
    let descriptor = &shared.session.manifest().objects[object];
    let Some(chunk) = descriptor
        .chunks
        .iter()
        .find(|chunk| chunk.offset == start && chunk.offset + chunk.size - 1 == end)
    else {
        return respond_empty(request, 416);
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
    request
        .respond(
            Response::new(
                StatusCode(206),
                vec![
                    header("accept-ranges", "bytes")?,
                    header("etag", &quoted(object))?,
                    header(
                        "content-range",
                        &format!("bytes {start}-{end}/{}", descriptor.size),
                    )?,
                ],
                file.take(chunk.size),
                Some(chunk.size as usize),
                None,
            )
            .with_chunked_threshold(usize::MAX),
        )
        .map_err(transport)
}

#[derive(Serialize, Deserialize)]
struct ClaimRequest {
    claim: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
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

fn random_secret() -> Result<[u8; 32], PeerSyncError> {
    let mut secret = [0; 32];
    getrandom::getrandom(&mut secret)
        .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
    Ok(secret)
}
fn digest(bytes: impl AsRef<[u8]>) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |different, (a, b)| different | (a ^ b))
        == 0
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
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
fn header(name: &str, value: &str) -> Result<Header, PeerSyncError> {
    Header::from_bytes(name, value)
        .map_err(|_| PeerSyncError::Protocol("invalid HTTP response header".to_owned()))
}
fn transport(error: impl std::fmt::Display) -> PeerSyncError {
    PeerSyncError::Transport(error.to_string())
}
fn respond_empty(request: Request, status: u16) -> Result<(), PeerSyncError> {
    request
        .respond(Response::empty(StatusCode(status)))
        .map_err(transport)
}
fn respond_json<T: Serialize>(
    request: Request,
    status: u16,
    value: &T,
) -> Result<(), PeerSyncError> {
    let body =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    request
        .respond(
            Response::new(
                StatusCode(status),
                vec![header("content-type", "application/json")?],
                io::Cursor::new(body.clone()),
                Some(body.len()),
                None,
            )
            .with_chunked_threshold(usize::MAX),
        )
        .map_err(transport)
}
fn read_small_body(reader: &mut dyn Read) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    reader
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .ok()?;
    (body.len() <= MAX_BODY_BYTES).then_some(body)
}
