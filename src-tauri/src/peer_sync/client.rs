use super::device_registry::{
    load_or_create_device_id, prepare_incoming_completion_delivery,
    snapshot_incoming_completion_delivery, CompletionDeliveryPrepareStatus, CompletionLane,
    CompletionLeaseId, PendingCompletionDelivery,
};
use super::lan::{
    parse_completion_lease_headers, LanCloneClient, PeerCompletionCapability,
    PeerCompletionDelivery, PEER_COMPLETION_CAPABILITY_HEADER, PEER_COMPLETION_CAPABILITY_V1,
    PEER_COMPLETION_RESUME_HEADER,
};
use super::{
    http_stream::HttpRangeStream,
    protocol::{sha256_hex, CloneManifest, CloneObjectKind, MAX_MANIFEST_BYTES},
    PeerSyncError,
};
use crate::asset_repository::PayloadCas;
use reqwest::{
    blocking::{Client, Response},
    header::{ACCEPT_RANGES, CONTENT_LENGTH, ETAG},
    StatusCode, Url,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::sync::{Barrier, Mutex};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

const TRANSFER_BUFFER_BYTES: usize = 64 * 1024;
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PERSISTED_MANIFEST_FILE: &str = "manifest.json";

pub(crate) fn prepare_and_deliver_registered_clone_completion(
    app_root: &Path,
    credential_path: &Path,
    delivery: &PendingCompletionDelivery,
) -> Result<bool, PeerSyncError> {
    match prepare_incoming_completion_delivery(app_root, delivery.clone())? {
        CompletionDeliveryPrepareStatus::AlreadyDurable => return Ok(false),
        CompletionDeliveryPrepareStatus::Pending => {}
    }
    let snapshot = snapshot_incoming_completion_delivery(
        app_root,
        &delivery.source_device_id,
        CompletionLane::Clone,
    )?
    .ok_or_else(|| {
        PeerSyncError::Validation("pending clone completion delivery is missing".to_owned())
    })?;
    if snapshot.delivery != *delivery {
        return Err(PeerSyncError::Validation(
            "pending clone completion delivery changed".to_owned(),
        ));
    }
    let lan = LanCloneClient::open_persisted(credential_path)?;
    let (endpoint, session_id, manifest_id) = lan.target_identity()?;
    let target_device_id = load_or_create_device_id(app_root)?;
    if delivery.lane != CompletionLane::Clone.as_str()
        || delivery.manifest_id != manifest_id
        || lan.registered_source_device_id() != Some(delivery.source_device_id.as_str())
        || snapshot.source.device_id != delivery.source_device_id
        || snapshot.source.endpoint != endpoint
        || !snapshot.source.permissions.allows_read()
        || !lan.matches_registered_credential(
            endpoint,
            session_id,
            manifest_id,
            &target_device_id,
            &delivery.source_device_id,
            &snapshot.source.bearer,
        )?
    {
        return Err(PeerSyncError::Validation(
            "registered clone completion credential changed".to_owned(),
        ));
    }
    let hello = lan.hello_with_capabilities()?;
    if hello.hello.device_id != delivery.source_device_id
        || !hello.hello.permissions.allows_read()
        || hello.completion != PeerCompletionCapability::V1
    {
        return Err(PeerSyncError::Protocol(
            "registered clone completion capability changed".to_owned(),
        ));
    }
    if lan.deliver_completion(
        PeerCompletionCapability::V1,
        &delivery.completion_lease_id,
        delivery.useful_bytes,
    )? != PeerCompletionDelivery::Delivered
    {
        return Err(PeerSyncError::Protocol(
            "registered clone completion is unsupported".to_owned(),
        ));
    }
    Ok(true)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadReport {
    pub verified_objects: usize,
    pub transferred_bytes: u64,
    pub maximum_buffer_bytes: usize,
    pub maximum_response_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TransferCancellation {
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    pause_after_hash_read: Arc<Mutex<Option<Arc<Barrier>>>>,
    #[cfg(test)]
    pause_after_cas_read: Arc<Mutex<Option<Arc<Barrier>>>>,
}

impl TransferCancellation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn pause_after_hash_read_for_test(&self, pause: Arc<Barrier>) {
        *self.pause_after_hash_read.lock().unwrap() = Some(pause);
    }

    #[cfg(test)]
    fn pause_after_hash_read_for_test_if_requested(&self) {
        if let Some(pause) = self.pause_after_hash_read.lock().unwrap().take() {
            pause.wait();
            pause.wait();
        }
    }

    #[cfg(test)]
    pub(crate) fn pause_after_cas_read_for_test(&self, pause: Arc<Barrier>) {
        *self.pause_after_cas_read.lock().unwrap() = Some(pause);
    }

    #[cfg(test)]
    fn pause_after_cas_read_for_test_if_requested(&self) {
        if let Some(pause) = self.pause_after_cas_read.lock().unwrap().take() {
            pause.wait();
            pause.wait();
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ObjectProgress {
    next_chunk: usize,
    verified: bool,
}

#[derive(Debug, Default)]
struct LedgerState {
    manifest_id: Option<String>,
    objects: BTreeMap<String, ObjectProgress>,
    activated: bool,
    terminal_progress_lease_id: Option<CompletionLeaseId>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum LedgerEvent {
    Manifest {
        manifest_id: String,
    },
    Object {
        object: String,
        next_chunk: usize,
        verified: bool,
    },
    Activated {
        manifest_id: String,
    },
    TerminalProgress {
        manifest_id: String,
        completion_lease_id: String,
    },
}

pub struct LoopbackCloneClient {
    root: PathBuf,
    transport: HttpCloneTransport,
    required_manifest_id: Option<String>,
    source_device_id: Option<String>,
    manifest: Option<CloneManifest>,
    manifest_id: Option<String>,
    ledger: LedgerState,
    completion_v1: bool,
    completion_lease_id: Option<CompletionLeaseId>,
    #[cfg(test)]
    fail_after_cas_promotion: bool,
    #[cfg(test)]
    pause_after_cas_promotion: Option<PathBuf>,
    #[cfg(test)]
    pause_after_verified_chunk: Option<Arc<Barrier>>,
    #[cfg(test)]
    fail_record_activation: bool,
}

struct HttpCloneTransport {
    session_url: Url,
    http: Client,
    ranges: HttpRangeStream,
    bearer: Option<String>,
}

struct CancellableCasReader<'a, R> {
    reader: &'a mut R,
    cancellation: &'a TransferCancellation,
}

impl<R: Read> Read for CancellableCasReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "peer clone transfer cancelled",
            ));
        }
        let read = self.reader.read(buffer)?;
        #[cfg(test)]
        if read > 0 {
            self.cancellation
                .pause_after_cas_read_for_test_if_requested();
        }
        Ok(read)
    }
}

impl HttpCloneTransport {
    fn authenticated_request(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        match &self.bearer {
            Some(bearer) => request.bearer_auth(bearer),
            None => request,
        }
    }

    fn control_request(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        self.authenticated_request(request)
            .timeout(CONTROL_REQUEST_TIMEOUT)
    }

    fn endpoint(&self, suffix: &str) -> Result<Url, PeerSyncError> {
        let mut url = self.session_url.clone();
        let path = format!(
            "{}/{}",
            url.path().trim_end_matches('/'),
            suffix.trim_matches('/')
        );
        url.set_path(&path);
        Ok(url)
    }
}

impl LoopbackCloneClient {
    // The loopback clone client pairs with the test-only LoopbackCloneHost;
    // production transports construct clients through the LAN paths.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        staging_root: impl AsRef<Path>,
        session_url: impl AsRef<str>,
    ) -> Result<Self, PeerSyncError> {
        fs::create_dir_all(staging_root.as_ref())?;
        let root = fs::canonicalize(staging_root.as_ref())?;
        let session_url = validate_loopback_url(session_url.as_ref())?;
        let ledger = load_ledger(&root.join("ledger.jsonl"))?;
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(CONTROL_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(transport_error)?;
        let ranges = HttpRangeStream::new(Duration::from_secs(2))?;
        Ok(Self {
            root,
            transport: HttpCloneTransport {
                session_url,
                http,
                ranges,
                bearer: None,
            },
            required_manifest_id: None,
            source_device_id: None,
            manifest: None,
            manifest_id: None,
            ledger,
            completion_v1: false,
            completion_lease_id: None,
            #[cfg(test)]
            fail_after_cas_promotion: false,
            #[cfg(test)]
            pause_after_cas_promotion: None,
            #[cfg(test)]
            pause_after_verified_chunk: None,
            #[cfg(test)]
            fail_record_activation: false,
        })
    }

    pub(crate) fn from_lan_with_completion(
        staging_root: impl AsRef<Path>,
        lan: LanCloneClient,
        expected_manifest_id: &str,
        capability: PeerCompletionCapability,
        resume_lease_id: Option<&str>,
    ) -> Result<Self, PeerSyncError> {
        super::protocol::validate_hash(expected_manifest_id)?;
        let completion_lease_id = match (capability, resume_lease_id) {
            (PeerCompletionCapability::Unsupported, None) => None,
            (PeerCompletionCapability::V1, lease_id) => {
                lease_id.map(CompletionLeaseId::parse).transpose()?
            }
            (PeerCompletionCapability::Unsupported, Some(_)) => {
                return Err(PeerSyncError::Protocol(
                    "unsupported peer completion cannot resume a lease".to_owned(),
                ))
            }
        };
        let (http, ranges, session_url, bearer, persisted_manifest_id, source_device_id) =
            lan.into_resumable_parts()?;
        if persisted_manifest_id
            .as_deref()
            .is_some_and(|persisted| persisted != expected_manifest_id)
        {
            return Err(PeerSyncError::StaleManifest {
                expected: expected_manifest_id.to_owned(),
                received: persisted_manifest_id.unwrap(),
            });
        }
        fs::create_dir_all(staging_root.as_ref())?;
        let root = fs::canonicalize(staging_root.as_ref())?;
        let ledger = load_ledger(&root.join("ledger.jsonl"))?;
        Ok(Self {
            root,
            transport: HttpCloneTransport {
                session_url,
                http,
                ranges,
                bearer: Some(bearer),
            },
            required_manifest_id: Some(expected_manifest_id.to_owned()),
            source_device_id,
            manifest: None,
            manifest_id: None,
            ledger,
            completion_v1: capability == PeerCompletionCapability::V1,
            completion_lease_id,
            #[cfg(test)]
            fail_after_cas_promotion: false,
            #[cfg(test)]
            pause_after_cas_promotion: None,
            #[cfg(test)]
            pause_after_verified_chunk: None,
            #[cfg(test)]
            fail_record_activation: false,
        })
    }

    pub(crate) fn source_device_id(&self) -> Option<&str> {
        self.source_device_id.as_deref()
    }

    pub(crate) fn prepare_completion_manifest(&mut self) -> Result<Option<String>, PeerSyncError> {
        if !self.completion_v1 {
            return Ok(None);
        }
        self.fetch_manifest()?;
        Ok(self
            .completion_lease_id
            .as_ref()
            .map(|lease_id| lease_id.as_str().to_owned()))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn download(
        &mut self,
        cancellation: &TransferCancellation,
    ) -> Result<DownloadReport, PeerSyncError> {
        self.download_with_progress(cancellation, |_| {})
    }

    pub fn download_with_progress(
        &mut self,
        cancellation: &TransferCancellation,
        mut progress: impl FnMut(u64),
    ) -> Result<DownloadReport, PeerSyncError> {
        if cancellation.is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        self.fetch_manifest()?;
        let manifest = self.manifest.as_ref().unwrap().clone();
        let mut report = DownloadReport {
            maximum_buffer_bytes: TRANSFER_BUFFER_BYTES,
            ..DownloadReport::default()
        };
        for object_hash in transfer_order(&manifest) {
            self.download_object(
                &manifest,
                &object_hash,
                cancellation,
                &mut report,
                &mut progress,
            )?;
        }
        self.report_verified_progress(&manifest, None)?;
        report.verified_objects = manifest.objects.len();
        Ok(report)
    }

    pub(crate) fn transfer_progress(&mut self) -> Result<(u64, u64), PeerSyncError> {
        self.fetch_manifest()?;
        let manifest = self.manifest.as_ref().unwrap();
        let total_bytes = manifest
            .objects
            .values()
            .try_fold(0_u64, |total, object| total.checked_add(object.size))
            .ok_or_else(|| PeerSyncError::Protocol("clone byte count overflow".to_owned()))?;
        Ok((self.verified_bytes(manifest)?, total_bytes))
    }

    pub(crate) fn all_objects_verified(
        &mut self,
        cancellation: &TransferCancellation,
    ) -> Result<bool, PeerSyncError> {
        self.fetch_manifest()?;
        let manifest = self.manifest.as_ref().unwrap().clone();
        let mut all_verified = true;
        for (hash, object) in &manifest.objects {
            if cancellation.is_cancelled() {
                return Err(PeerSyncError::Cancelled);
            }
            if !matches!(self.ledger.objects.get(hash), Some(progress) if progress.verified) {
                all_verified = false;
                continue;
            }
            if !self.verify_local_object(hash, object.size, Some(cancellation))? {
                self.discard_local_object(hash)?;
                self.record_object_progress(hash, 0, false)?;
                all_verified = false;
            }
        }
        Ok(all_verified)
    }

    pub(crate) fn report_terminal_progress(&mut self) -> Result<(), PeerSyncError> {
        self.fetch_manifest()?;
        let manifest = self.manifest.as_ref().unwrap().clone();
        self.report_verified_progress(&manifest, None)
    }

    #[cfg(test)]
    pub fn verified_chunk_count(&self, object: &str) -> usize {
        self.ledger
            .objects
            .get(object)
            .map(|progress| progress.next_chunk)
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub fn verified_object_path(&self, object: &str) -> Option<PathBuf> {
        self.ledger
            .objects
            .get(object)
            .filter(|progress| progress.verified)
            .map(|_| self.object_path(object))
    }

    #[cfg(test)]
    pub fn fail_after_cas_promotion_once_for_test(&mut self) {
        self.fail_after_cas_promotion = true;
    }

    #[cfg(test)]
    pub fn pause_after_cas_promotion_for_test(&mut self, marker: impl Into<PathBuf>) {
        self.pause_after_cas_promotion = Some(marker.into());
    }

    #[cfg(test)]
    pub(crate) fn pause_after_verified_chunk_for_test(&mut self, pause: Arc<Barrier>) {
        self.pause_after_verified_chunk = Some(pause);
    }

    #[cfg(test)]
    pub(crate) fn fail_record_activation_once_for_test(&mut self) {
        self.fail_record_activation = true;
    }

    fn fetch_manifest(&mut self) -> Result<(), PeerSyncError> {
        if self.manifest.is_some() {
            return Ok(());
        }
        let requested_session_id = self
            .transport
            .session_url
            .path_segments()
            .and_then(|segments| segments.last());
        if let Some((manifest, manifest_id)) = load_persisted_manifest(
            &self.root.join(PERSISTED_MANIFEST_FILE),
            self.required_manifest_id.as_deref(),
            self.ledger.manifest_id.as_deref(),
            requested_session_id,
        )? {
            if !self.completion_v1 || self.completion_lease_id.is_some() {
                if self.ledger.manifest_id.is_none() {
                    self.append_event(&LedgerEvent::Manifest {
                        manifest_id: manifest_id.clone(),
                    })?;
                    self.ledger.manifest_id = Some(manifest_id.clone());
                }
                self.manifest = Some(manifest);
                self.manifest_id = Some(manifest_id);
                return Ok(());
            }
        }
        let mut request = self
            .transport
            .http
            .get(self.transport.endpoint("manifest")?);
        if self.completion_v1 {
            request = request.header(
                PEER_COMPLETION_CAPABILITY_HEADER,
                PEER_COMPLETION_CAPABILITY_V1,
            );
            if let Some(lease_id) = self.completion_lease_id.as_ref() {
                request = request.header(PEER_COMPLETION_RESUME_HEADER, lease_id.as_str());
            }
        }
        let response = self
            .transport
            .control_request(request)
            .send()
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "manifest request returned {}",
                response.status()
            )));
        }
        if response.content_length().unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES as u64 {
            return Err(PeerSyncError::Protocol(
                "clone manifest exceeds the bounded v1 size".to_owned(),
            ));
        }
        let etag = required_header(&response, ETAG)?.to_owned();
        let completion_lease_id = if self.completion_v1 {
            parse_completion_lease_headers(response.headers())?
        } else {
            None
        };
        if let Some(persisted) = self.completion_lease_id.as_ref() {
            if completion_lease_id.as_ref() != Some(persisted) {
                return Err(PeerSyncError::Protocol(
                    "clone completion resume lease changed".to_owned(),
                ));
            }
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(transport_error)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PeerSyncError::Protocol(
                "clone manifest exceeds the bounded v1 size".to_owned(),
            ));
        }
        let manifest: CloneManifest = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        manifest.validate()?;
        if manifest.canonical_bytes()? != bytes {
            return Err(PeerSyncError::Protocol(
                "clone manifest bytes are not canonical".to_owned(),
            ));
        }
        let manifest_id = sha256_hex(&bytes);
        if let Some(expected) = &self.required_manifest_id {
            if expected != &manifest_id {
                return Err(PeerSyncError::StaleManifest {
                    expected: expected.clone(),
                    received: manifest_id,
                });
            }
        }
        if etag != quoted(&manifest_id) {
            return Err(PeerSyncError::Protocol(
                "clone manifest ETag does not match its bytes".to_owned(),
            ));
        }
        if let Some(expected) = &self.ledger.manifest_id {
            if expected != &manifest_id {
                return Err(PeerSyncError::StaleManifest {
                    expected: expected.clone(),
                    received: manifest_id,
                });
            }
        } else {
            self.append_event(&LedgerEvent::Manifest {
                manifest_id: manifest_id.clone(),
            })?;
            self.ledger.manifest_id = Some(manifest_id.clone());
        }
        persist_manifest(&self.root.join(PERSISTED_MANIFEST_FILE), &bytes)?;
        self.completion_lease_id = completion_lease_id;
        self.manifest = Some(manifest);
        self.manifest_id = Some(manifest_id);
        Ok(())
    }

    fn download_object(
        &mut self,
        manifest: &CloneManifest,
        object_hash: &str,
        cancellation: &TransferCancellation,
        report: &mut DownloadReport,
        progress_callback: &mut impl FnMut(u64),
    ) -> Result<(), PeerSyncError> {
        let descriptor = manifest.object(object_hash)?;
        let current = self
            .ledger
            .objects
            .get(object_hash)
            .cloned()
            .unwrap_or_default();
        let download_directory = self.root.join("downloads");
        let part_path = download_directory.join(format!("{object_hash}.part"));
        if self.verify_local_object(object_hash, descriptor.size, Some(cancellation))? {
            if !current.verified {
                self.record_object_progress(object_hash, descriptor.chunks.len(), true)?;
            }
            remove_file_if_exists(&part_path)?;
            self.report_verified_progress(manifest, Some(object_hash))?;
            return Ok(());
        }
        self.discard_local_object(object_hash)?;
        if current.verified {
            self.record_object_progress(object_hash, 0, false)?;
        }
        if cancellation.is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        self.verify_remote_object(object_hash, descriptor.size)?;
        fs::create_dir_all(&download_directory)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&part_path)?;
        let mut next_chunk = self
            .ledger
            .objects
            .get(object_hash)
            .map(|progress| progress.next_chunk)
            .unwrap_or(0);
        if next_chunk > descriptor.chunks.len() {
            next_chunk = 0;
            self.record_object_progress(object_hash, 0, false)?;
        }
        let expected_offset = descriptor
            .chunks
            .get(next_chunk)
            .map(|chunk| chunk.offset)
            .unwrap_or(descriptor.size);
        if file.metadata()?.len() < expected_offset {
            next_chunk = 0;
            file.set_len(0)?;
            file.sync_all()?;
            self.record_object_progress(object_hash, 0, false)?;
        } else {
            file.set_len(expected_offset)?;
        }

        for (index, chunk) in descriptor.chunks.iter().enumerate().skip(next_chunk) {
            if cancellation.is_cancelled() {
                return Err(PeerSyncError::Cancelled);
            }
            file.seek(SeekFrom::Start(chunk.offset))?;
            let range_end = chunk.offset + chunk.size - 1;
            let range_url = self.transport.endpoint(&format!("objects/{object_hash}"))?;
            report.maximum_response_bytes = report.maximum_response_bytes.max(chunk.size);
            let mut hasher = Sha256::new();
            let mut received = 0_u64;
            let transfer = self.transport.ranges.read(
                range_url,
                self.transport.bearer.as_deref(),
                object_hash,
                chunk.offset,
                range_end,
                Some(descriptor.size),
                &|| cancellation.is_cancelled(),
                &mut |bytes| {
                    file.write_all(bytes)?;
                    hasher.update(bytes);
                    received += bytes.len() as u64;
                    report.transferred_bytes += bytes.len() as u64;
                    progress_callback(report.transferred_bytes);
                    Ok(())
                },
            );
            if let Err(error) = transfer {
                file.set_len(chunk.offset)?;
                file.sync_all()?;
                return Err(error);
            }
            if cancellation.is_cancelled() {
                file.set_len(chunk.offset)?;
                file.sync_all()?;
                return Err(PeerSyncError::Cancelled);
            }
            if received != chunk.size {
                file.set_len(chunk.offset)?;
                file.sync_all()?;
                return Err(PeerSyncError::Transport(
                    "range response ended before the requested clone chunk".to_owned(),
                ));
            }
            if hex::encode(hasher.finalize()) != chunk.sha256 {
                file.set_len(chunk.offset)?;
                file.sync_all()?;
                return Err(PeerSyncError::ChunkHashMismatch {
                    object: object_hash.to_owned(),
                    offset: chunk.offset,
                });
            }
            file.flush()?;
            file.sync_all()?;
            self.record_object_progress(object_hash, index + 1, false)?;
            self.report_verified_progress(manifest, Some(object_hash))?;
            #[cfg(test)]
            if let Some(pause) = self.pause_after_verified_chunk.take() {
                pause.wait();
                pause.wait();
            }
        }

        file.seek(SeekFrom::Start(0))?;
        if hash_reader(&mut file, Some(cancellation))? != object_hash {
            file.set_len(0)?;
            file.sync_all()?;
            self.record_object_progress(object_hash, 0, false)?;
            return Err(PeerSyncError::WholeObjectHashMismatch {
                object: object_hash.to_owned(),
            });
        }
        file.seek(SeekFrom::Start(0))?;
        let cas = PayloadCas::new(&self.root)?;
        let mut reader = CancellableCasReader {
            reader: &mut file,
            cancellation,
        };
        let prepared = cas.prepare_reader(&mut reader).map_err(|error| {
            if cancellation.is_cancelled() {
                PeerSyncError::Cancelled
            } else {
                error.into()
            }
        })?;
        if prepared.content_hash != object_hash || prepared.byte_size != descriptor.size {
            return Err(PeerSyncError::WholeObjectHashMismatch {
                object: object_hash.to_owned(),
            });
        }
        #[cfg(test)]
        if let Some(marker_path) = self.pause_after_cas_promotion.take() {
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(marker_path)?;
            marker.write_all(b"ready")?;
            marker.sync_all()?;
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_after_cas_promotion) {
            return Err(PeerSyncError::Storage(
                "injected crash after CAS promotion".to_owned(),
            ));
        }
        drop(file);
        remove_file_if_exists(&part_path)?;
        self.record_object_progress(object_hash, descriptor.chunks.len(), true)?;
        self.report_verified_progress(manifest, Some(object_hash))
    }

    fn verify_remote_object(&self, object_hash: &str, size: u64) -> Result<(), PeerSyncError> {
        let response = self
            .transport
            .control_request(
                self.transport
                    .http
                    .head(self.transport.endpoint(&format!("objects/{object_hash}"))?),
            )
            .send()
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            return Err(PeerSyncError::Transport(format!(
                "object HEAD returned {}",
                response.status()
            )));
        }
        if parse_u64_header(&response, CONTENT_LENGTH)? != size {
            return Err(PeerSyncError::Protocol(
                "object HEAD size does not match the manifest".to_owned(),
            ));
        }
        if required_header(&response, ACCEPT_RANGES)? != "bytes"
            || required_header(&response, ETAG)? != quoted(object_hash)
        {
            return Err(PeerSyncError::Protocol(
                "object HEAD does not advertise the immutable range contract".to_owned(),
            ));
        }
        Ok(())
    }

    fn report_verified_progress(
        &mut self,
        manifest: &CloneManifest,
        current_object: Option<&str>,
    ) -> Result<(), PeerSyncError> {
        if self.transport.bearer.is_none() {
            return Ok(());
        }
        let verified_bytes = self.verified_bytes(manifest)?;
        let total_bytes = manifest
            .objects
            .values()
            .try_fold(0_u64, |total, object| total.checked_add(object.size))
            .ok_or_else(|| PeerSyncError::Protocol("clone byte count overflow".to_owned()))?;
        let terminal_lease_id = if current_object.is_none() && verified_bytes == total_bytes {
            self.completion_lease_id.clone()
        } else {
            None
        };
        if terminal_lease_id.as_ref() == self.ledger.terminal_progress_lease_id.as_ref()
            && terminal_lease_id.is_some()
        {
            return Ok(());
        }
        let mut progress = serde_json::json!({
            "verifiedBytes": verified_bytes,
            "currentObject": current_object,
        });
        if let Some(lease_id) = self.completion_lease_id.as_ref() {
            progress["operationId"] = serde_json::Value::String(lease_id.as_str().to_owned());
        }
        let response = self
            .transport
            .control_request(
                self.transport
                    .http
                    .post(self.transport.endpoint("progress")?)
                    .json(&progress),
            )
            .send()
            .map_err(transport_error)?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(PeerSyncError::Transport(format!(
                "progress request returned {}",
                response.status()
            )));
        }
        if let Some(lease_id) = terminal_lease_id {
            let manifest_id = self.ledger.manifest_id.clone().ok_or_else(|| {
                PeerSyncError::Storage("clone terminal progress ledger has no manifest".to_owned())
            })?;
            self.append_event(&LedgerEvent::TerminalProgress {
                manifest_id,
                completion_lease_id: lease_id.as_str().to_owned(),
            })?;
            self.ledger.terminal_progress_lease_id = Some(lease_id);
        }
        Ok(())
    }

    fn verified_bytes(&self, manifest: &CloneManifest) -> Result<u64, PeerSyncError> {
        manifest
            .objects
            .iter()
            .try_fold(0_u64, |total, (hash, object)| {
                let next_chunk = self
                    .ledger
                    .objects
                    .get(hash)
                    .map(|progress| progress.next_chunk)
                    .unwrap_or(0)
                    .min(object.chunks.len());
                let object_bytes = object.chunks[..next_chunk]
                    .iter()
                    .try_fold(0_u64, |subtotal, chunk| subtotal.checked_add(chunk.size))?;
                total.checked_add(object_bytes)
            })
            .ok_or_else(|| PeerSyncError::Protocol("verified clone byte count overflow".to_owned()))
    }

    fn verify_local_object(
        &self,
        object_hash: &str,
        size: u64,
        cancellation: Option<&TransferCancellation>,
    ) -> Result<bool, PeerSyncError> {
        let cas = PayloadCas::new(&self.root)?;
        let Some(mut file) = cas.open_object(object_hash)? else {
            return Ok(false);
        };
        if file.metadata()?.len() != size {
            return Ok(false);
        }
        Ok(hash_reader(&mut file, cancellation)? == object_hash)
    }

    fn object_path(&self, object_hash: &str) -> PathBuf {
        self.root
            .join("assets-v2")
            .join("objects")
            .join(&object_hash[..2])
            .join(&object_hash[2..])
    }

    fn discard_local_object(&self, object_hash: &str) -> Result<(), PeerSyncError> {
        remove_file_if_exists(&self.object_path(object_hash))
    }

    fn record_object_progress(
        &mut self,
        object: &str,
        next_chunk: usize,
        verified: bool,
    ) -> Result<(), PeerSyncError> {
        self.append_event(&LedgerEvent::Object {
            object: object.to_owned(),
            next_chunk,
            verified,
        })?;
        self.ledger.objects.insert(
            object.to_owned(),
            ObjectProgress {
                next_chunk,
                verified,
            },
        );
        Ok(())
    }

    fn record_activation(&mut self) -> Result<(), PeerSyncError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_record_activation) {
            return Err(PeerSyncError::Storage(
                "injected clone activation ledger failure".to_owned(),
            ));
        }
        let manifest_id = self
            .manifest_id
            .clone()
            .ok_or_else(|| PeerSyncError::Protocol("clone manifest is not loaded".to_owned()))?;
        self.append_event(&LedgerEvent::Activated { manifest_id })?;
        self.ledger.activated = true;
        Ok(())
    }

    fn append_event(&self, event: &LedgerEvent) -> Result<(), PeerSyncError> {
        let mut bytes =
            serde_json::to_vec(event).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(self.root.join("ledger.jsonl"))?;
        let original_len = repair_ledger_tail_for_append(&mut file)?;
        if let Err(error) = (|| {
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()
        })() {
            let rollback = file.set_len(original_len).and_then(|()| file.sync_all());
            return match rollback {
                Ok(()) => Err(error.into()),
                Err(rollback) => Err(PeerSyncError::Storage(format!(
                    "clone ledger append failed: {error}; rollback failed: {rollback}"
                ))),
            };
        }
        Ok(())
    }
}

fn repair_ledger_tail_for_append(file: &mut File) -> Result<u64, PeerSyncError> {
    const SCAN_BYTES: usize = 4 * 1024;

    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        file.seek(SeekFrom::End(0))?;
        return Ok(length);
    }

    let mut end = length;
    let mut buffer = [0_u8; SCAN_BYTES];
    while end > 0 {
        let read = end.min(SCAN_BYTES as u64) as usize;
        let start = end - read as u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..read])?;
        if let Some(index) = buffer[..read].iter().rposition(|byte| *byte == b'\n') {
            let valid_len = start + index as u64 + 1;
            file.set_len(valid_len)?;
            file.sync_all()?;
            file.seek(SeekFrom::End(0))?;
            return Ok(valid_len);
        }
        end = start;
    }
    file.set_len(0)?;
    file.sync_all()?;
    file.seek(SeekFrom::End(0))?;
    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneActivation {
    Activated,
    AlreadyActive,
    Conflict { actual: Option<String> },
}

pub trait CloneTargetAdapter {
    type Stage;

    fn active_manifest_id(&self) -> Result<Option<String>, PeerSyncError>;
    fn begin(&mut self, manifest_id: &str) -> Result<Self::Stage, PeerSyncError>;
    fn stage_object(
        &mut self,
        stage: &mut Self::Stage,
        kind: CloneObjectKind,
        logical_key: &str,
        metadata: &serde_json::Value,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError>;
    fn abort(&mut self, stage: Self::Stage) -> Result<(), PeerSyncError>;
    /// The comparison and activation must be one atomic target transaction.
    fn activate_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_manifest_id: Option<&str>,
        new_manifest_id: &str,
    ) -> Result<CloneActivation, PeerSyncError>;
}

pub trait CloneValidator<S> {
    fn validate(&mut self, manifest: &CloneManifest, stage: &S) -> Result<(), PeerSyncError>;
}

impl<S, F> CloneValidator<S> for F
where
    F: FnMut(&CloneManifest, &S) -> Result<(), PeerSyncError>,
{
    fn validate(&mut self, manifest: &CloneManifest, stage: &S) -> Result<(), PeerSyncError> {
        self(manifest, stage)
    }
}

pub fn activate_downloaded_clone<T, V>(
    client: &mut LoopbackCloneClient,
    target: &mut T,
    validator: &mut V,
) -> Result<(), PeerSyncError>
where
    T: CloneTargetAdapter,
    V: CloneValidator<T::Stage>,
{
    client.fetch_manifest()?;
    let manifest = client.manifest.as_ref().unwrap().clone();
    let manifest_id = client.manifest_id.as_ref().unwrap().clone();
    let expected_manifest_id = target.active_manifest_id()?;
    if expected_manifest_id.as_deref() == Some(&manifest_id) {
        if !client.ledger.activated {
            client.record_activation()?;
        }
        return Err(PeerSyncError::AlreadyActivated);
    }
    if client.ledger.activated {
        return Err(PeerSyncError::Validation(
            "clone ledger says activated but target does not expose that manifest".to_owned(),
        ));
    }
    for hash in manifest.objects.keys() {
        let progress = client.ledger.objects.get(hash);
        if !matches!(progress, Some(progress) if progress.verified)
            || !client.verify_local_object(hash, manifest.objects[hash].size, None)?
        {
            return Err(PeerSyncError::Validation(
                "clone activation requires every object to be locally verified".to_owned(),
            ));
        }
    }

    let mut stage = target.begin(&manifest_id)?;
    let staging = (|| {
        for payload in &manifest.payloads {
            let mut reader = client.open_verified_object(&payload.object)?;
            target.stage_object(
                &mut stage,
                payload.kind,
                &payload.logical_key,
                &payload.metadata,
                &mut reader,
            )?;
        }
        let mut database = client.open_verified_object(&manifest.database.object)?;
        target.stage_object(
            &mut stage,
            CloneObjectKind::Database,
            "database",
            &serde_json::Value::Null,
            &mut database,
        )?;
        validator.validate(&manifest, &stage)
    })();
    if let Err(error) = staging {
        let _ = target.abort(stage);
        return Err(error);
    }

    let activation =
        match target.activate_if_current(&mut stage, expected_manifest_id.as_deref(), &manifest_id)
        {
            Ok(activation) => activation,
            Err(error) => {
                let _ = target.abort(stage);
                return Err(error);
            }
        };
    match activation {
        CloneActivation::Activated => {
            drop(stage);
            client.record_activation()
        }
        CloneActivation::AlreadyActive => {
            let _ = target.abort(stage);
            if !client.ledger.activated {
                client.record_activation()?;
            }
            Err(PeerSyncError::AlreadyActivated)
        }
        CloneActivation::Conflict { actual } => {
            let _ = target.abort(stage);
            Err(PeerSyncError::ActivationConflict {
                expected: expected_manifest_id,
                actual,
            })
        }
    }
}

impl LoopbackCloneClient {
    fn open_verified_object(&self, hash: &str) -> Result<File, PeerSyncError> {
        PayloadCas::new(&self.root)?
            .open_object(hash)?
            .ok_or_else(|| PeerSyncError::Storage("verified clone object is missing".to_owned()))
    }
}

fn transfer_order(manifest: &CloneManifest) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for payload in &manifest.payloads {
        if seen.insert(payload.object.clone()) {
            ordered.push(payload.object.clone());
        }
    }
    if seen.insert(manifest.database.object.clone()) {
        ordered.push(manifest.database.object.clone());
    }
    ordered
}

pub(crate) fn persisted_clone_manifest_total(
    transfer_root: &Path,
    expected_session_id: &str,
    expected_manifest_id: &str,
) -> Result<u64, PeerSyncError> {
    let (manifest, _) = load_persisted_manifest(
        &transfer_root.join(PERSISTED_MANIFEST_FILE),
        Some(expected_manifest_id),
        None,
        Some(expected_session_id),
    )?
    .ok_or_else(|| PeerSyncError::Storage("persisted clone manifest is missing".to_owned()))?;
    manifest
        .objects
        .values()
        .try_fold(0_u64, |total, object| total.checked_add(object.size))
        .ok_or_else(|| PeerSyncError::Storage("persisted clone byte count overflow".to_owned()))
}

fn load_persisted_manifest(
    path: &Path,
    required_manifest_id: Option<&str>,
    ledger_manifest_id: Option<&str>,
    requested_session_id: Option<&str>,
) -> Result<Option<(CloneManifest, String)>, PeerSyncError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(PeerSyncError::Storage(
            "persisted clone manifest exceeds the bounded v1 size".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(PeerSyncError::Storage(
            "persisted clone manifest exceeds the bounded v1 size".to_owned(),
        ));
    }
    let manifest: CloneManifest = serde_json::from_slice(&bytes).map_err(|error| {
        PeerSyncError::Storage(format!("invalid persisted clone manifest: {error}"))
    })?;
    manifest.validate().map_err(|error| {
        PeerSyncError::Storage(format!("invalid persisted clone manifest: {error}"))
    })?;
    if manifest.canonical_bytes().map_err(|error| {
        PeerSyncError::Storage(format!("invalid persisted clone manifest: {error}"))
    })? != bytes
    {
        return Err(PeerSyncError::Storage(
            "persisted clone manifest bytes are not canonical".to_owned(),
        ));
    }
    let manifest_id = sha256_hex(&bytes);
    for expected in [required_manifest_id, ledger_manifest_id]
        .into_iter()
        .flatten()
    {
        if expected != manifest_id {
            return Err(PeerSyncError::StaleManifest {
                expected: expected.to_owned(),
                received: manifest_id,
            });
        }
    }
    if requested_session_id.is_some_and(|session_id| session_id != manifest.session_id) {
        return Ok(None);
    }
    if required_manifest_id.is_none() && ledger_manifest_id.is_none() {
        return Err(PeerSyncError::Storage(
            "persisted clone manifest has no checkpoint identity".to_owned(),
        ));
    }
    Ok(Some((manifest, manifest_id)))
}

fn persist_manifest(path: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    match File::open(path) {
        Ok(mut file) => {
            if file.metadata()?.len() > MAX_MANIFEST_BYTES as u64 {
                return Err(PeerSyncError::Storage(
                    "persisted clone manifest exceeds the bounded v1 size".to_owned(),
                ));
            }
            let mut existing = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take(MAX_MANIFEST_BYTES as u64 + 1)
                .read_to_end(&mut existing)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(PeerSyncError::Storage(
                "persisted clone manifest changed within an immutable job".to_owned(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("persisted clone manifest path has no parent".to_owned())
    })?;
    let temporary = parent.join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_manifest_parent(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn sync_manifest_parent(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_manifest_parent(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
}

fn load_ledger(path: &Path) -> Result<LedgerState, PeerSyncError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LedgerState::default())
        }
        Err(error) => return Err(error.into()),
    };
    let mut ledger = LedgerState::default();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut valid_bytes = 0_u64;
    let mut truncated_tail = false;
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            truncated_tail = true;
            break;
        }
        valid_bytes += read as u64;
        line.pop();
        if line.ends_with(b"\r") {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        let event: LedgerEvent = serde_json::from_slice(&line)
            .map_err(|error| PeerSyncError::Storage(format!("invalid clone ledger: {error}")))?;
        match event {
            LedgerEvent::Manifest { manifest_id } => {
                if let Some(existing) = &ledger.manifest_id {
                    if existing != &manifest_id {
                        return Err(PeerSyncError::Storage(
                            "clone ledger contains multiple manifests".to_owned(),
                        ));
                    }
                }
                ledger.manifest_id = Some(manifest_id);
            }
            LedgerEvent::Object {
                object,
                next_chunk,
                verified,
            } => {
                ledger.objects.insert(
                    object,
                    ObjectProgress {
                        next_chunk,
                        verified,
                    },
                );
            }
            LedgerEvent::Activated { manifest_id } => {
                if ledger.manifest_id.as_deref() != Some(&manifest_id) {
                    return Err(PeerSyncError::Storage(
                        "clone activation ledger references another manifest".to_owned(),
                    ));
                }
                ledger.activated = true;
            }
            LedgerEvent::TerminalProgress {
                manifest_id,
                completion_lease_id,
            } => {
                if ledger.manifest_id.as_deref() != Some(&manifest_id) {
                    return Err(PeerSyncError::Storage(
                        "clone terminal progress ledger references another manifest".to_owned(),
                    ));
                }
                ledger.terminal_progress_lease_id =
                    Some(CompletionLeaseId::parse(&completion_lease_id).map_err(|_| {
                        PeerSyncError::Storage(
                            "clone terminal progress ledger has an invalid lease".to_owned(),
                        )
                    })?);
            }
        }
    }
    drop(reader);
    if truncated_tail {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(valid_bytes)?;
        file.sync_all()?;
    }
    Ok(ledger)
}

pub(crate) fn persisted_activation_matches(
    transfer_root: &Path,
    manifest_id: &str,
) -> Result<bool, PeerSyncError> {
    super::protocol::validate_hash(manifest_id)?;
    let ledger = load_ledger(&transfer_root.join("ledger.jsonl"))?;
    Ok(ledger.activated && ledger.manifest_id.as_deref() == Some(manifest_id))
}

#[cfg_attr(not(test), allow(dead_code))]
fn validate_loopback_url(value: &str) -> Result<Url, PeerSyncError> {
    let url = Url::parse(value).map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(PeerSyncError::Protocol(
            "P0 clone client accepts only explicit 127.0.0.1 HTTP session URLs".to_owned(),
        ));
    }
    Ok(url)
}

fn required_header(
    response: &Response,
    name: reqwest::header::HeaderName,
) -> Result<&str, PeerSyncError> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            PeerSyncError::Protocol("required HTTP response header is missing".to_owned())
        })
}

fn parse_u64_header(
    response: &Response,
    name: reqwest::header::HeaderName,
) -> Result<u64, PeerSyncError> {
    required_header(response, name)?
        .parse::<u64>()
        .map_err(|_| {
            PeerSyncError::Protocol("HTTP size header is not an unsigned integer".to_owned())
        })
}

fn hash_reader(
    reader: &mut impl Read,
    cancellation: Option<&TransferCancellation>,
) -> Result<String, PeerSyncError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; TRANSFER_BUFFER_BYTES];
    loop {
        if cancellation.is_some_and(TransferCancellation::is_cancelled) {
            return Err(PeerSyncError::Cancelled);
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        #[cfg(test)]
        if let Some(cancellation) = cancellation {
            cancellation.pause_after_hash_read_for_test_if_requested();
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

fn remove_file_if_exists(path: &Path) -> Result<(), PeerSyncError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

fn transport_error(error: impl std::fmt::Display) -> PeerSyncError {
    PeerSyncError::Transport(error.to_string())
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    fn terminal_progress_manifest() -> (CloneManifest, String) {
        let object_hash = sha256_hex(&[]);
        let manifest = CloneManifest {
            schema: super::super::protocol::CLONE_MANIFEST_SCHEMA.to_owned(),
            session_id: "terminal-progress".to_owned(),
            source_revision: 1,
            created_at: "2026-09-02T00:00:00Z".to_owned(),
            chunk_size: super::super::protocol::CLONE_CHUNK_SIZE,
            database: super::super::protocol::CloneDatabase {
                format: super::super::protocol::CLONE_DATABASE_FORMAT.to_owned(),
                object: object_hash.clone(),
            },
            payloads: Vec::new(),
            objects: BTreeMap::from([(
                object_hash.clone(),
                super::super::protocol::ObjectDescriptor {
                    size: 0,
                    sha256: object_hash,
                    chunks: Vec::new(),
                },
            )]),
        };
        let manifest_id = sha256_hex(&manifest.canonical_bytes().expect("manifest bytes"));
        (manifest, manifest_id)
    }

    fn terminal_progress_client(
        staging_root: &Path,
        address: std::net::SocketAddr,
        lease_id: &str,
    ) -> LoopbackCloneClient {
        fs::create_dir_all(staging_root).expect("create staging root");
        let root = fs::canonicalize(staging_root).expect("canonical staging root");
        let (manifest, manifest_id) = terminal_progress_manifest();
        let ledger = load_ledger(&root.join("ledger.jsonl")).expect("load ledger");
        let mut client = LoopbackCloneClient {
            root,
            transport: HttpCloneTransport {
                session_url: Url::parse(&format!("http://{address}/v1/sessions/terminal-progress"))
                    .expect("session URL"),
                http: Client::builder().no_proxy().build().expect("HTTP client"),
                ranges: HttpRangeStream::new(Duration::from_secs(1)).expect("range client"),
                bearer: Some("registered-bearer".to_owned()),
            },
            required_manifest_id: Some(manifest_id.clone()),
            source_device_id: Some("00000000-0000-4000-8000-000000000403".to_owned()),
            manifest: Some(manifest),
            manifest_id: Some(manifest_id.clone()),
            ledger,
            completion_v1: true,
            completion_lease_id: Some(CompletionLeaseId::parse(lease_id).expect("lease ID")),
            fail_after_cas_promotion: false,
            pause_after_cas_promotion: None,
            pause_after_verified_chunk: None,
            fail_record_activation: false,
        };
        if client.ledger.manifest_id.is_none() {
            client
                .append_event(&LedgerEvent::Manifest {
                    manifest_id: manifest_id.clone(),
                })
                .expect("record manifest");
            client.ledger.manifest_id = Some(manifest_id);
        }
        client
    }

    fn terminal_progress_server(
        response_count: usize,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<Vec<String>>) {
        use std::net::{Shutdown, TcpListener};
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind progress fixture");
        let address = listener.local_addr().expect("progress fixture address");
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..response_count {
                let (mut stream, _) = listener.accept().expect("accept progress request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone progress stream"));
                let mut request = String::new();
                let mut content_length = 0_usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read progress request");
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().expect("content length");
                    }
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0_u8; content_length];
                reader.read_exact(&mut body).expect("read progress body");
                request.push_str(std::str::from_utf8(&body).expect("progress body UTF-8"));
                requests.push(request);
                drop(reader);
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("write progress response");
                stream.flush().expect("flush progress response");
                stream
                    .shutdown(Shutdown::Write)
                    .expect("finish progress response");
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .expect("set progress drain timeout");
                let mut drain = [0_u8; 64];
                while stream.read(&mut drain).is_ok_and(|read| read != 0) {}
            }
            requests
        });
        (address, server)
    }

    #[test]
    fn clone_http_control_requests_keep_a_short_total_timeout() {
        let transport = HttpCloneTransport {
            session_url: Url::parse("http://127.0.0.1:1/v1/sessions/test").unwrap(),
            http: Client::builder().no_proxy().build().unwrap(),
            ranges: HttpRangeStream::new(Duration::from_secs(1)).unwrap(),
            bearer: None,
        };

        let control = transport
            .control_request(transport.http.get(transport.endpoint("manifest").unwrap()))
            .build()
            .unwrap();
        assert_eq!(control.timeout(), Some(&CONTROL_REQUEST_TIMEOUT));
    }

    #[test]
    fn successful_terminal_progress_is_not_reposted_for_the_same_lease_after_restart() {
        let staging = tempfile::tempdir().expect("staging parent");
        let transfer_root = staging.path().join("transfer");
        let lease_id = "00000000-0000-4000-8000-000000000411";
        let (address, server) = terminal_progress_server(1);
        let mut first = terminal_progress_client(&transfer_root, address, lease_id);

        first
            .report_terminal_progress()
            .expect("initial terminal progress");
        assert_eq!(server.join().expect("join progress fixture").len(), 1);
        drop(first);

        let mut restarted = terminal_progress_client(&transfer_root, address, lease_id);
        restarted
            .report_terminal_progress()
            .expect("durably acknowledged terminal progress must work offline");
    }

    #[test]
    fn terminal_progress_acknowledgement_is_exact_to_the_completion_lease() {
        let staging = tempfile::tempdir().expect("staging parent");
        let transfer_root = staging.path().join("transfer");
        let first_lease = "00000000-0000-4000-8000-000000000412";
        let second_lease = "00000000-0000-4000-8000-000000000413";
        let (first_address, first_server) = terminal_progress_server(1);
        let mut first = terminal_progress_client(&transfer_root, first_address, first_lease);
        first
            .report_terminal_progress()
            .expect("first terminal progress");
        assert_eq!(first_server.join().expect("join first fixture").len(), 1);
        drop(first);

        let (second_address, second_server) = terminal_progress_server(1);
        let mut second = terminal_progress_client(&transfer_root, second_address, second_lease);
        second
            .report_terminal_progress()
            .expect("different lease must be reported");
        let requests = second_server.join().expect("join second fixture");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains(second_lease));
        assert!(!requests[0].contains(first_lease));
        drop(second);

        let mut old_lease = terminal_progress_client(&transfer_root, second_address, first_lease);
        assert!(old_lease.report_terminal_progress().is_err());
    }

    #[test]
    fn terminal_progress_ack_write_failure_retries_after_remote_success() {
        let staging = tempfile::tempdir().expect("staging parent");
        let transfer_root = staging.path().join("transfer");
        let displaced_root = staging.path().join("displaced-transfer");
        let lease_id = "00000000-0000-4000-8000-000000000414";
        let (address, server) = terminal_progress_server(2);
        let mut client = terminal_progress_client(&transfer_root, address, lease_id);
        fs::rename(&transfer_root, &displaced_root).expect("displace transfer root");

        assert!(matches!(
            client.report_terminal_progress(),
            Err(PeerSyncError::Storage(_))
        ));
        fs::rename(&displaced_root, &transfer_root).expect("restore transfer root");
        client
            .report_terminal_progress()
            .expect("retry terminal progress after restoring ledger");
        assert_eq!(server.join().expect("join progress fixture").len(), 2);
        drop(client);

        let mut restarted = terminal_progress_client(&transfer_root, address, lease_id);
        restarted
            .report_terminal_progress()
            .expect("retried acknowledgement must survive restart");
    }

    #[test]
    fn terminal_progress_retry_repairs_a_partial_acknowledgement_append() {
        let staging = tempfile::tempdir().expect("staging parent");
        let transfer_root = staging.path().join("transfer");
        let lease_id = "00000000-0000-4000-8000-000000000415";
        let (address, server) = terminal_progress_server(1);
        let mut client = terminal_progress_client(&transfer_root, address, lease_id);
        OpenOptions::new()
            .append(true)
            .open(transfer_root.join("ledger.jsonl"))
            .expect("open ledger")
            .write_all(b"{\"kind\":\"terminal-progress\"")
            .expect("write failed append prefix");

        client
            .report_terminal_progress()
            .expect("retry terminal progress after partial append");
        assert_eq!(server.join().expect("join progress fixture").len(), 1);
        drop(client);

        let mut restarted = terminal_progress_client(&transfer_root, address, lease_id);
        restarted
            .report_terminal_progress()
            .expect("repaired acknowledgement must survive restart");
    }

    fn assert_resume_manifest_rejects_response_lease(
        include_capability: bool,
        response_lease: Option<&str>,
        expected_error: &str,
    ) {
        use std::net::{Shutdown, TcpListener};
        use std::thread;

        let persisted = CompletionLeaseId::parse("00000000-0000-4000-8000-000000000401")
            .expect("persisted lease");
        let object_hash = sha256_hex(&[]);
        let manifest = CloneManifest {
            schema: super::super::protocol::CLONE_MANIFEST_SCHEMA.to_owned(),
            session_id: "resume".to_owned(),
            source_revision: 1,
            created_at: "2026-09-02T00:00:00Z".to_owned(),
            chunk_size: super::super::protocol::CLONE_CHUNK_SIZE,
            database: super::super::protocol::CloneDatabase {
                format: super::super::protocol::CLONE_DATABASE_FORMAT.to_owned(),
                object: object_hash.clone(),
            },
            payloads: Vec::new(),
            objects: BTreeMap::from([(
                object_hash.clone(),
                super::super::protocol::ObjectDescriptor {
                    size: 0,
                    sha256: object_hash,
                    chunks: Vec::new(),
                },
            )]),
        };
        let manifest_bytes = manifest.canonical_bytes().expect("manifest bytes");
        let manifest_id = sha256_hex(&manifest_bytes);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let expected_error = expected_error.to_owned();
        let include_capability = include_capability;
        let response_lease = response_lease.map(str::to_owned);
        let response_manifest_id = manifest_id.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept manifest request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone manifest stream"));
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read manifest request");
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut completion_headers = String::new();
            if include_capability {
                completion_headers.push_str(&format!(
                    "{}: {}\r\n",
                    PEER_COMPLETION_CAPABILITY_HEADER, PEER_COMPLETION_CAPABILITY_V1
                ));
            }
            if let Some(lease) = response_lease {
                completion_headers.push_str(&format!(
                    "{}: {lease}\r\n",
                    super::super::lan::PEER_COMPLETION_LEASE_HEADER
                ));
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"{response_manifest_id}\"\r\n{completion_headers}Connection: close\r\n\r\n",
                manifest_bytes.len(),
            );
            let mut response_bytes = response.into_bytes();
            response_bytes.extend_from_slice(&manifest_bytes);
            stream
                .write_all(&response_bytes)
                .expect("write manifest response");
            stream.flush().expect("flush manifest response");
            stream
                .shutdown(Shutdown::Write)
                .expect("finish manifest response");
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("set fixture drain timeout");
            let mut drain = [0_u8; 64];
            while stream.read(&mut drain).is_ok_and(|read| read != 0) {}
            request
        });

        let staging = tempfile::tempdir().expect("staging root");
        let root = fs::canonicalize(staging.path()).expect("canonical staging root");
        let mut client = LoopbackCloneClient {
            root,
            transport: HttpCloneTransport {
                session_url: Url::parse(&format!("http://{address}/v1/sessions/resume"))
                    .expect("session URL"),
                http: Client::builder().no_proxy().build().expect("HTTP client"),
                ranges: HttpRangeStream::new(Duration::from_secs(1)).expect("range client"),
                bearer: Some("registered-bearer".to_owned()),
            },
            required_manifest_id: Some(manifest_id),
            source_device_id: Some("00000000-0000-4000-8000-000000000403".to_owned()),
            manifest: None,
            manifest_id: None,
            ledger: LedgerState::default(),
            completion_v1: true,
            completion_lease_id: Some(persisted.clone()),
            fail_after_cas_promotion: false,
            pause_after_cas_promotion: None,
            pause_after_verified_chunk: None,
            fail_record_activation: false,
        };

        let error = client
            .prepare_completion_manifest()
            .expect_err("resume response must fail closed");
        assert_eq!(error, PeerSyncError::Protocol(expected_error));
        assert_eq!(client.completion_lease_id.as_ref(), Some(&persisted));
        assert!(client.manifest.is_none());
        let request = server.join().expect("join fixture");
        assert!(request.contains("/manifest"));
        assert!(request.to_ascii_lowercase().contains(
            &format!("{}: {}", PEER_COMPLETION_RESUME_HEADER, persisted.as_str())
                .to_ascii_lowercase()
        ));
        assert!(!request.contains("/progress"));
    }

    #[test]
    fn resumed_completion_manifest_requires_the_exact_response_lease() {
        assert_resume_manifest_rejects_response_lease(
            false,
            None,
            "clone completion resume lease changed",
        );
        assert_resume_manifest_rejects_response_lease(
            true,
            None,
            "invalid peer completion lease headers",
        );
        assert_resume_manifest_rejects_response_lease(
            true,
            Some("00000000-0000-4000-8000-000000000402"),
            "clone completion resume lease changed",
        );
    }
}
