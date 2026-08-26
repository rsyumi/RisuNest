use super::lan::LanCloneClient;
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
use std::sync::Barrier;
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
}

pub struct LoopbackCloneClient {
    root: PathBuf,
    transport: HttpCloneTransport,
    required_manifest_id: Option<String>,
    manifest: Option<CloneManifest>,
    manifest_id: Option<String>,
    ledger: LedgerState,
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
            manifest: None,
            manifest_id: None,
            ledger,
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

    pub fn from_lan(
        staging_root: impl AsRef<Path>,
        lan: LanCloneClient,
        expected_manifest_id: &str,
    ) -> Result<Self, PeerSyncError> {
        super::protocol::validate_hash(expected_manifest_id)?;
        let (http, ranges, session_url, bearer, persisted_manifest_id) =
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
            manifest: None,
            manifest_id: None,
            ledger,
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

    pub(crate) fn all_objects_verified(&mut self) -> Result<bool, PeerSyncError> {
        self.fetch_manifest()?;
        let manifest = self.manifest.as_ref().unwrap();
        if manifest.objects.keys().any(
            |hash| !matches!(self.ledger.objects.get(hash), Some(progress) if progress.verified),
        ) {
            return Ok(false);
        }
        for (hash, object) in &manifest.objects {
            if !self.verify_local_object(hash, object.size)? {
                return Ok(false);
            }
        }
        Ok(true)
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
        let response = self
            .transport
            .control_request(
                self.transport
                    .http
                    .get(self.transport.endpoint("manifest")?),
            )
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
        if self.verify_local_object(object_hash, descriptor.size)? {
            if !current.verified {
                self.record_object_progress(object_hash, descriptor.chunks.len(), true)?;
            }
            remove_file_if_exists(&part_path)?;
            self.report_verified_progress(manifest, Some(object_hash))?;
            return Ok(());
        }
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
        if hash_reader(&mut file)? != object_hash {
            file.set_len(0)?;
            file.sync_all()?;
            self.record_object_progress(object_hash, 0, false)?;
            return Err(PeerSyncError::WholeObjectHashMismatch {
                object: object_hash.to_owned(),
            });
        }
        file.seek(SeekFrom::Start(0))?;
        let cas = PayloadCas::new(&self.root)?;
        let prepared = cas.prepare_reader(&mut file)?;
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
        &self,
        manifest: &CloneManifest,
        current_object: Option<&str>,
    ) -> Result<(), PeerSyncError> {
        if self.transport.bearer.is_none() {
            return Ok(());
        }
        let verified_bytes = self.verified_bytes(manifest)?;
        let response = self
            .transport
            .control_request(
                self.transport
                    .http
                    .post(self.transport.endpoint("progress")?)
                    .json(&serde_json::json!({
                        "verifiedBytes": verified_bytes,
                        "currentObject": current_object,
                    })),
            )
            .send()
            .map_err(transport_error)?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(PeerSyncError::Transport(format!(
                "progress request returned {}",
                response.status()
            )));
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

    fn verify_local_object(&self, object_hash: &str, size: u64) -> Result<bool, PeerSyncError> {
        let cas = PayloadCas::new(&self.root)?;
        let Some(mut file) = cas.open_object(object_hash)? else {
            return Ok(false);
        };
        if file.metadata()?.len() != size {
            return Ok(false);
        }
        Ok(hash_reader(&mut file)? == object_hash)
    }

    fn object_path(&self, object_hash: &str) -> PathBuf {
        self.root
            .join("assets-v2")
            .join("objects")
            .join(&object_hash[..2])
            .join(&object_hash[2..])
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
            .append(true)
            .open(self.root.join("ledger.jsonl"))?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }
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
            || !client.verify_local_object(hash, manifest.objects[hash].size)?
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
    file.by_ref()
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
            file.by_ref()
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

fn hash_reader(reader: &mut impl Read) -> Result<String, PeerSyncError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; TRANSFER_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
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
}
