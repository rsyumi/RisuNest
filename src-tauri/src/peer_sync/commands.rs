use super::{
    activate_downloaded_clone, prepare_lossless_clone_session, LanCloneClient, LanCloneHost,
    LoopbackCloneClient, LosslessCloneTargetAdapter, PeerSyncError, TransferCancellation,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::NeverCancelled,
    persistent_store::{self, PersistentStore, StoreError},
};
use serde::Serialize;
use std::{
    fs,
    net::{Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread::{self, JoinHandle},
};
use tauri::{AppHandle, Manager, State};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
    production_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeerCloneGateState {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    lossless_backup_ready: bool,
    http_transport_ready: bool,
    large_fixture_passed: bool,
}

impl PeerCloneCapabilities {
    fn current() -> Self {
        Self::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: false,
        })
    }

    fn from_gates(gates: PeerCloneGateState) -> Self {
        Self {
            desktop: gates.desktop,
            source_ready: gates.source_ready,
            atomic_activation_ready: gates.atomic_activation_ready,
            lossless_backup_ready: gates.lossless_backup_ready,
            http_transport_ready: gates.http_transport_ready,
            large_fixture_passed: gates.large_fixture_passed,
            production_enabled: gates.desktop
                && gates.source_ready
                && gates.atomic_activation_ready
                && gates.lossless_backup_ready
                && gates.http_transport_ready,
        }
    }
}

#[tauri::command]
pub fn peer_clone_capabilities() -> PeerCloneCapabilities {
    PeerCloneCapabilities::current()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneSourcePhase {
    Idle,
    Prepared,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneSourceDevice {
    device_id: String,
    verified_bytes: u64,
    current_object: Option<String>,
    last_seen_at: u64,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneSourceStatus {
    session_id: Option<String>,
    manifest_id: Option<String>,
    pairing_uri: Option<String>,
    phase: PeerCloneSourcePhase,
    devices: Vec<PeerCloneSourceDevice>,
}

impl PeerCloneSourceStatus {
    fn idle() -> Self {
        Self {
            session_id: None,
            manifest_id: None,
            pairing_uri: None,
            phase: PeerCloneSourcePhase::Idle,
            devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerCloneTargetRequest {
    endpoint: String,
    session_id: String,
    manifest_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerCloneTargetPhase {
    Idle,
    Downloading,
    AwaitingActivation,
    Activating,
    Cancelled,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneTargetStatus {
    phase: PeerCloneTargetPhase,
    completed_bytes: u64,
    total_bytes: Option<u64>,
    error: Option<String>,
}

impl PeerCloneTargetStatus {
    fn idle() -> Self {
        Self {
            phase: PeerCloneTargetPhase::Idle,
            completed_bytes: 0,
            total_bytes: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCloneFinalizeResult {
    revision: i64,
}

struct VerifiedCloneValidator;

impl<S> super::CloneValidator<S> for VerifiedCloneValidator {
    fn validate(
        &mut self,
        _manifest: &super::CloneManifest,
        _stage: &S,
    ) -> Result<(), PeerSyncError> {
        Ok(())
    }
}

struct SourceRuntime {
    session_id: String,
    manifest_id: String,
    session_root: PathBuf,
    host: Option<LanCloneHost>,
    pairing_uri: Option<String>,
    phase: PeerCloneSourcePhase,
}

struct TargetRuntime {
    request: PeerCloneTargetRequest,
    job_root: PathBuf,
    client: Option<LoopbackCloneClient>,
    cancellation: Option<TransferCancellation>,
    worker: Option<JoinHandle<()>>,
    status: PeerCloneTargetStatus,
}

#[derive(Default)]
struct PeerCloneRuntime {
    source_preparing: bool,
    source: Option<SourceRuntime>,
    target_claiming: bool,
    target: Option<TargetRuntime>,
}

#[derive(Clone, Default)]
pub struct PeerCloneCommandState {
    runtime: Arc<Mutex<PeerCloneRuntime>>,
}

impl PeerCloneCommandState {
    fn lock_runtime(&self) -> Result<MutexGuard<'_, PeerCloneRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer clone command state mutex poisoned: {error}"))
        })
    }

    pub fn prepare_source(
        &self,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        peer_root: &Path,
        cancellation: &dyn crate::local_backup::CancellationProbe,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        {
            let mut runtime = self.lock_runtime()?;
            if runtime.source_preparing
                || runtime
                    .source
                    .as_ref()
                    .is_some_and(|source| source.phase != PeerCloneSourcePhase::Stopped)
            {
                return Err(PeerSyncError::Protocol(
                    "peer clone source is already prepared or running".to_owned(),
                ));
            }
            runtime.source_preparing = true;
        }

        let operation_id = uuid::Uuid::new_v4().to_string();
        let preparation_parent = peer_root.join("source-preparation");
        let sessions_parent = peer_root.join("source-sessions");
        let preparation_root = preparation_parent.join(&operation_id);
        let session_root = sessions_parent.join(&operation_id);
        let prepared = (|| {
            fs::create_dir_all(&preparation_parent)?;
            fs::create_dir_all(&sessions_parent)?;
            let expected_revision = store.revision().map_err(store_error)?;
            prepare_lossless_clone_session(
                store,
                cas,
                expected_revision,
                &preparation_root,
                &session_root,
                cancellation,
            )
        })();

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let cleanup = remove_directory_if_exists(&session_root);
                let mut runtime = self.lock_runtime()?;
                runtime.source_preparing = false;
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(PeerSyncError::Storage(format!(
                        "{error}; peer clone source session cleanup failed: {cleanup}"
                    ))),
                };
            }
        };
        let session_id = prepared.manifest().session_id.clone();
        let manifest_id = prepared.manifest_id().to_owned();
        let mut runtime = self.lock_runtime()?;
        runtime.source_preparing = false;
        runtime.source = Some(SourceRuntime {
            session_id,
            manifest_id,
            session_root,
            host: Some(LanCloneHost::prepare(prepared)),
            pairing_uri: None,
            phase: PeerCloneSourcePhase::Prepared,
        });
        source_status(&runtime)
    }

    pub fn start_source(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        if !(advertised_ip.is_private() || advertised_ip.is_link_local()) {
            return Err(PeerSyncError::Validation(
                "peer clone source requires a private or link-local IPv4 address".to_owned(),
            ));
        }
        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        if source.phase != PeerCloneSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer clone source is not prepared".to_owned(),
            ));
        }
        let host = source.host.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source host is unavailable".to_owned())
        })?;
        let pairing = host.start()?;
        let address = host.address().ok_or_else(|| {
            PeerSyncError::Transport("peer clone source address is unavailable".to_owned())
        })?;
        let pairing_uri = match build_pairing_uri(advertised_ip, address.port(), &pairing) {
            Ok(pairing_uri) => pairing_uri,
            Err(error) => {
                let _ = host.stop();
                return Err(error);
            }
        };
        source.pairing_uri = Some(pairing_uri);
        source.phase = PeerCloneSourcePhase::Running;
        source_status(&runtime)
    }

    pub fn source_status(&self) -> Result<PeerCloneSourceStatus, PeerSyncError> {
        source_status(&self.lock_runtime()?)
    }

    pub fn source_bind_address(&self) -> Result<Option<SocketAddr>, PeerSyncError> {
        Ok(self
            .lock_runtime()?
            .source
            .as_ref()
            .and_then(|source| source.host.as_ref())
            .and_then(LanCloneHost::address))
    }

    pub fn revoke_source_device(
        &self,
        session_id: &str,
        device_id: &str,
    ) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock_runtime()?;
        let source = require_source_mut(&mut runtime, session_id)?;
        if source.phase != PeerCloneSourcePhase::Running
            || !source
                .host
                .as_ref()
                .is_some_and(|host| host.revoke(device_id))
        {
            return Err(PeerSyncError::Protocol(
                "peer clone source device is unavailable".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn stop_source(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let (host, session_root) = {
            let mut runtime = self.lock_runtime()?;
            let source = require_source_mut(&mut runtime, session_id)?;
            if source.phase == PeerCloneSourcePhase::Stopped {
                return Ok(());
            }
            let host = source.host.take();
            source.pairing_uri = None;
            source.phase = PeerCloneSourcePhase::Stopped;
            (host, source.session_root.clone())
        };
        let stop = host.map(|mut host| host.stop()).transpose().map(|_| ());
        let cleanup = remove_directory_if_exists(&session_root);
        match (stop, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(stop), Err(cleanup)) => Err(PeerSyncError::Storage(format!(
                "{stop}; peer clone source session cleanup failed: {cleanup}"
            ))),
        }
    }

    pub fn claim_target(
        &self,
        peer_root: &Path,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<(), PeerSyncError> {
        let request = PeerCloneTargetRequest {
            endpoint: endpoint.to_owned(),
            session_id: session_id.to_owned(),
            manifest_id: manifest_id.to_owned(),
        };
        let paths = target_paths(peer_root, &request)?;
        {
            let mut runtime = self.lock_runtime()?;
            if let Some(target) = &runtime.target {
                return if target.request == request && target.job_root == paths.job_root {
                    Ok(())
                } else {
                    Err(PeerSyncError::Protocol(
                        "another peer clone target job is already owned".to_owned(),
                    ))
                };
            }
            if runtime.target_claiming {
                return Err(PeerSyncError::Protocol(
                    "peer clone target claim is already in progress".to_owned(),
                ));
            }
            runtime.target_claiming = true;
        }

        let claimed = (|| {
            let lan = match fs::symlink_metadata(&paths.credential) {
                Ok(_) => LanCloneClient::open_persisted(&paths.credential)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    LanCloneClient::claim_and_persist(
                        &paths.credential,
                        endpoint,
                        session_id,
                        manifest_id,
                        claim,
                    )?
                }
                Err(error) => return Err(error.into()),
            };
            if !lan.matches_target(endpoint, session_id, manifest_id)? {
                return Err(PeerSyncError::Protocol(
                    "persisted peer clone target identity does not match the request".to_owned(),
                ));
            }
            LoopbackCloneClient::from_lan(&paths.transfer, lan, manifest_id)
        })();

        let mut runtime = self.lock_runtime()?;
        runtime.target_claiming = false;
        let client = claimed?;
        runtime.target = Some(TargetRuntime {
            request,
            job_root: paths.job_root,
            client: Some(client),
            cancellation: None,
            worker: None,
            status: PeerCloneTargetStatus::idle(),
        });
        Ok(())
    }

    pub fn start_target_download(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
    ) -> Result<(), PeerSyncError> {
        self.start_target_worker(peer_root, request, false)
    }

    pub fn resume_target_download(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
    ) -> Result<(), PeerSyncError> {
        self.reap_finished_target_worker()?;
        let paths = target_paths(peer_root, &request)?;
        let needs_restore = {
            let mut runtime = self.lock_runtime()?;
            if let Some(target) = &runtime.target {
                if target.request != request || target.job_root != paths.job_root {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target request does not own the active job".to_owned(),
                    ));
                }
                false
            } else {
                if runtime.target_claiming {
                    return Err(PeerSyncError::Protocol(
                        "peer clone target ownership is already being restored".to_owned(),
                    ));
                }
                runtime.target_claiming = true;
                true
            }
        };
        if needs_restore {
            let restored = (|| {
                let lan = LanCloneClient::open_persisted(&paths.credential)?;
                if !lan.matches_target(
                    &request.endpoint,
                    &request.session_id,
                    &request.manifest_id,
                )? {
                    return Err(PeerSyncError::Protocol(
                        "persisted peer clone target identity does not match the request"
                            .to_owned(),
                    ));
                }
                LoopbackCloneClient::from_lan(&paths.transfer, lan, &request.manifest_id)
            })();
            let mut runtime = self.lock_runtime()?;
            runtime.target_claiming = false;
            let client = restored?;
            if runtime.target.is_some() {
                return Err(PeerSyncError::Protocol(
                    "another peer clone target job acquired ownership".to_owned(),
                ));
            }
            runtime.target = Some(TargetRuntime {
                request: request.clone(),
                job_root: paths.job_root,
                client: Some(client),
                cancellation: None,
                worker: None,
                status: PeerCloneTargetStatus::idle(),
            });
        }
        self.start_target_worker(peer_root, request, true)
    }

    fn start_target_worker(
        &self,
        peer_root: &Path,
        request: PeerCloneTargetRequest,
        resume: bool,
    ) -> Result<(), PeerSyncError> {
        self.reap_finished_target_worker()?;
        let paths = target_paths(peer_root, &request)?;
        let (mut client, cancellation) = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, &request, &paths.job_root)?;
            let allowed = target.status.phase == PeerCloneTargetPhase::Idle
                || (resume
                    && matches!(
                        target.status.phase,
                        PeerCloneTargetPhase::Cancelled | PeerCloneTargetPhase::Failed
                    ));
            if !allowed {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not ready to download".to_owned(),
                ));
            }
            let client = target.client.take().ok_or_else(|| {
                PeerSyncError::Protocol("peer clone target client is unavailable".to_owned())
            })?;
            let cancellation = TransferCancellation::new();
            target.cancellation = Some(cancellation.clone());
            target.status = PeerCloneTargetStatus {
                phase: PeerCloneTargetPhase::Downloading,
                completed_bytes: target.status.completed_bytes,
                total_bytes: target.status.total_bytes,
                error: None,
            };
            (client, cancellation)
        };

        let runtime = Arc::clone(&self.runtime);
        let worker_request = request.clone();
        let worker_cancellation = cancellation.clone();
        let handle = thread::spawn(move || {
            let initial = client.transfer_progress();
            let (verified_bytes, total_bytes) = match initial {
                Ok(progress) => progress,
                Err(error) => {
                    finish_target_worker(&runtime, &worker_request, client, Err(error), None);
                    return;
                }
            };
            update_target_progress(&runtime, &worker_request, verified_bytes, total_bytes);
            let progress_runtime = Arc::clone(&runtime);
            let progress_request = worker_request.clone();
            let result = client.download_with_progress(&worker_cancellation, move |transferred| {
                update_target_progress(
                    &progress_runtime,
                    &progress_request,
                    verified_bytes.saturating_add(transferred).min(total_bytes),
                    total_bytes,
                );
            });
            let final_progress = client.transfer_progress().ok();
            let result = if worker_cancellation.is_cancelled() {
                Err(PeerSyncError::Cancelled)
            } else {
                result.map(|_| ())
            };
            finish_target_worker(&runtime, &worker_request, client, result, final_progress);
        });
        let mut command_runtime = self.lock_runtime()?;
        let target = require_target_mut(&mut command_runtime, &request, &paths.job_root)?;
        target.worker = Some(handle);
        Ok(())
    }

    pub fn cancel_target(&self, request: &PeerCloneTargetRequest) -> Result<(), PeerSyncError> {
        let worker = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_request_mut(&mut runtime, request)?;
            if target.status.phase != PeerCloneTargetPhase::Downloading {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not downloading".to_owned(),
                ));
            }
            target
                .cancellation
                .as_ref()
                .ok_or_else(|| {
                    PeerSyncError::Protocol(
                        "peer clone target cancellation is unavailable".to_owned(),
                    )
                })?
                .cancel();
            target.worker.take()
        };
        join_target_worker(worker)?;
        let mut runtime = self.lock_runtime()?;
        let target = require_target_request_mut(&mut runtime, request)?;
        target.cancellation = None;
        target.status.phase = PeerCloneTargetPhase::Cancelled;
        target.status.error = None;
        Ok(())
    }

    pub fn target_status_current(&self) -> Result<PeerCloneTargetStatus, PeerSyncError> {
        Ok(self
            .lock_runtime()?
            .target
            .as_ref()
            .map(|target| target.status.clone())
            .unwrap_or_else(PeerCloneTargetStatus::idle))
    }

    pub fn target_status(
        &self,
        request: &PeerCloneTargetRequest,
    ) -> Result<PeerCloneTargetStatus, PeerSyncError> {
        let runtime = self.lock_runtime()?;
        let target = runtime.target.as_ref().ok_or_else(|| {
            PeerSyncError::Protocol("peer clone target job is unavailable".to_owned())
        })?;
        if &target.request != request {
            return Err(PeerSyncError::Protocol(
                "peer clone target request does not own the active job".to_owned(),
            ));
        }
        Ok(target.status.clone())
    }

    pub fn finalize_target(
        &self,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        peer_root: &Path,
        request: &PeerCloneTargetRequest,
    ) -> Result<PeerCloneFinalizeResult, PeerSyncError> {
        let paths = target_paths(peer_root, request)?;
        let worker = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            if target.status.phase != PeerCloneTargetPhase::AwaitingActivation {
                return Err(PeerSyncError::Protocol(
                    "peer clone target is not awaiting activation".to_owned(),
                ));
            }
            target.status.phase = PeerCloneTargetPhase::Activating;
            target.worker.take()
        };
        if let Err(error) = join_target_worker(worker) {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            target.status.phase = PeerCloneTargetPhase::Failed;
            target.status.error = Some(error.to_string());
            return Err(error);
        }
        let mut client = {
            let mut runtime = self.lock_runtime()?;
            let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
            match target.client.take() {
                Some(client) => client,
                None => {
                    let error = PeerSyncError::Protocol(
                        "peer clone target client is unavailable".to_owned(),
                    );
                    target.status.phase = PeerCloneTargetPhase::Failed;
                    target.status.error = Some(error.to_string());
                    return Err(error);
                }
            }
        };
        let activation_root = peer_root.join("activation");
        let result = (|| {
            let expected_revision = store.revision().map_err(store_error)?;
            let mut target = LosslessCloneTargetAdapter::new(
                store,
                cas,
                &activation_root,
                expected_revision,
                &NeverCancelled,
            )?;
            let mut validator = VerifiedCloneValidator;
            let activation = activate_downloaded_clone(&mut client, &mut target, &mut validator);
            drop(target);
            match activation {
                Ok(()) | Err(PeerSyncError::AlreadyActivated) => Ok(PeerCloneFinalizeResult {
                    revision: store.revision().map_err(store_error)?,
                }),
                Err(error) => Err(error),
            }
        })();
        let mut runtime = self.lock_runtime()?;
        let target = require_target_mut(&mut runtime, request, &paths.job_root)?;
        target.client = Some(client);
        target.cancellation = None;
        match &result {
            Ok(_) => {
                target.status.phase = PeerCloneTargetPhase::Completed;
                target.status.error = None;
            }
            Err(error) => {
                target.status.phase = if matches!(error, PeerSyncError::Cancelled) {
                    PeerCloneTargetPhase::Cancelled
                } else {
                    PeerCloneTargetPhase::Failed
                };
                target.status.error = Some(error.to_string());
            }
        }
        result
    }

    fn reap_finished_target_worker(&self) -> Result<(), PeerSyncError> {
        let worker = {
            let mut runtime = self.lock_runtime()?;
            runtime.target.as_mut().and_then(|target| {
                if target.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                    target.worker.take()
                } else {
                    None
                }
            })
        };
        join_target_worker(worker)
    }
}

struct TargetPaths {
    job_root: PathBuf,
    credential: PathBuf,
    transfer: PathBuf,
}

fn target_paths(
    peer_root: &Path,
    request: &PeerCloneTargetRequest,
) -> Result<TargetPaths, PeerSyncError> {
    let session = uuid::Uuid::parse_str(&request.session_id).map_err(|_| {
        PeerSyncError::Validation("peer clone target session identity is invalid".to_owned())
    })?;
    if session.to_string() != request.session_id {
        return Err(PeerSyncError::Validation(
            "peer clone target session identity is not canonical".to_owned(),
        ));
    }
    super::protocol::validate_hash(&request.manifest_id)?;
    let job_root = peer_root.join("targets").join(&request.session_id);
    Ok(TargetPaths {
        credential: job_root.join("credential.json"),
        transfer: job_root.join("transfer"),
        job_root,
    })
}

fn finish_target_worker(
    runtime: &Arc<Mutex<PeerCloneRuntime>>,
    request: &PeerCloneTargetRequest,
    client: LoopbackCloneClient,
    result: Result<(), PeerSyncError>,
    progress: Option<(u64, u64)>,
) {
    let Ok(mut runtime) = runtime.lock() else {
        return;
    };
    let Some(target) = runtime
        .target
        .as_mut()
        .filter(|target| target.request == *request)
    else {
        return;
    };
    target.client = Some(client);
    target.cancellation = None;
    if let Some((completed_bytes, total_bytes)) = progress {
        target.status.completed_bytes = completed_bytes;
        target.status.total_bytes = Some(total_bytes);
    }
    match result {
        Ok(()) => {
            target.status.phase = PeerCloneTargetPhase::AwaitingActivation;
            if let Some(total_bytes) = target.status.total_bytes {
                target.status.completed_bytes = total_bytes;
            }
            target.status.error = None;
        }
        Err(PeerSyncError::Cancelled) => {
            target.status.phase = PeerCloneTargetPhase::Cancelled;
            target.status.error = None;
        }
        Err(error) => {
            target.status.phase = PeerCloneTargetPhase::Failed;
            target.status.error = Some(error.to_string());
        }
    }
}

fn update_target_progress(
    runtime: &Arc<Mutex<PeerCloneRuntime>>,
    request: &PeerCloneTargetRequest,
    completed_bytes: u64,
    total_bytes: u64,
) {
    if let Ok(mut runtime) = runtime.lock() {
        if let Some(target) = runtime
            .target
            .as_mut()
            .filter(|target| target.request == *request)
        {
            target.status.completed_bytes = completed_bytes;
            target.status.total_bytes = Some(total_bytes);
        }
    }
}

fn join_target_worker(worker: Option<JoinHandle<()>>) -> Result<(), PeerSyncError> {
    worker
        .map(|worker| {
            worker.join().map_err(|_| {
                PeerSyncError::Transport("peer clone target worker panicked".to_owned())
            })
        })
        .transpose()
        .map(|_| ())
}

fn source_status(runtime: &PeerCloneRuntime) -> Result<PeerCloneSourceStatus, PeerSyncError> {
    let Some(source) = &runtime.source else {
        return Ok(PeerCloneSourceStatus::idle());
    };
    let devices = source
        .host
        .as_ref()
        .map(|host| {
            host.devices()
                .into_iter()
                .map(|device| PeerCloneSourceDevice {
                    device_id: device.device_id,
                    verified_bytes: device.verified_bytes,
                    current_object: device.current_object,
                    last_seen_at: u64::try_from(device.last_seen_unix_ms).unwrap_or(u64::MAX),
                    revoked: device.revoked,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(PeerCloneSourceStatus {
        session_id: Some(source.session_id.clone()),
        manifest_id: Some(source.manifest_id.clone()),
        pairing_uri: source.pairing_uri.clone(),
        phase: source.phase,
        devices,
    })
}

fn require_source_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    session_id: &str,
) -> Result<&'a mut SourceRuntime, PeerSyncError> {
    runtime
        .source
        .as_mut()
        .filter(|source| source.session_id == session_id)
        .ok_or_else(|| {
            PeerSyncError::Protocol("peer clone source session is unavailable".to_owned())
        })
}

fn require_target_request_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    request: &PeerCloneTargetRequest,
) -> Result<&'a mut TargetRuntime, PeerSyncError> {
    runtime
        .target
        .as_mut()
        .filter(|target| target.request == *request)
        .ok_or_else(|| {
            PeerSyncError::Protocol(
                "peer clone target request does not own the active job".to_owned(),
            )
        })
}

fn require_target_mut<'a>(
    runtime: &'a mut PeerCloneRuntime,
    request: &PeerCloneTargetRequest,
    job_root: &Path,
) -> Result<&'a mut TargetRuntime, PeerSyncError> {
    require_target_request_mut(runtime, request).and_then(|target| {
        if target.job_root == job_root {
            Ok(target)
        } else {
            Err(PeerSyncError::Protocol(
                "peer clone target storage root does not own the active job".to_owned(),
            ))
        }
    })
}

fn remove_directory_if_exists(path: &Path) -> Result<(), PeerSyncError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn build_pairing_uri(
    advertised_ip: Ipv4Addr,
    port: u16,
    pairing: &super::LanPairing,
) -> Result<String, PeerSyncError> {
    let mut uri = url::Url::parse("risuailocal://peer-clone/v1")
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    uri.query_pairs_mut()
        .append_pair("endpoint", &format!("http://{advertised_ip}:{port}"))
        .append_pair("session", &pairing.session_id)
        .append_pair("manifest", &pairing.manifest_id);
    uri.set_fragment(Some(&format!("claim={}", pairing.claim)));
    Ok(uri.to_string())
}

fn discover_lan_ipv4() -> Result<Ipv4Addr, PeerSyncError> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
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

fn store_error(error: StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn as_store_error(error: PeerSyncError) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}

fn app_peer_root(app: &AppHandle) -> Result<(PathBuf, PathBuf), String> {
    let app_root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))?;
    Ok((app_root.clone(), app_root.join("peer-clone")))
}

fn target_request(
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> PeerCloneTargetRequest {
    PeerCloneTargetRequest {
        endpoint,
        session_id,
        manifest_id,
    }
}

#[tauri::command(async)]
pub async fn peer_clone_prepare(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
) -> Result<PeerCloneSourceStatus, String> {
    let state = state.inner().clone();
    let (app_root, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            state
                .prepare_source(store, &cas, &peer_root, &NeverCancelled)
                .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer clone source preparation worker failed: {error}"))?
}

#[tauri::command(async)]
pub fn peer_clone_start(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
) -> Result<PeerCloneSourceStatus, String> {
    let advertised_ip = discover_lan_ipv4().map_err(|error| error.to_string())?;
    state
        .start_source(&session_id, advertised_ip)
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_status(
    state: State<'_, PeerCloneCommandState>,
) -> Result<PeerCloneSourceStatus, String> {
    state.source_status().map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_stop(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
) -> Result<(), String> {
    state
        .stop_source(&session_id)
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_revoke(
    state: State<'_, PeerCloneCommandState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    state
        .revoke_source_device(&session_id, &device_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_claim_client(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let (_, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        state.claim_target(&peer_root, &endpoint, &session_id, &manifest_id, &claim)
    })
    .await
    .map_err(|error| format!("peer clone target claim worker failed: {error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_download(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = app_peer_root(&app)?;
    state
        .start_target_download(
            &peer_root,
            target_request(endpoint, session_id, manifest_id),
        )
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_resume(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let (_, peer_root) = app_peer_root(&app)?;
    state
        .resume_target_download(
            &peer_root,
            target_request(endpoint, session_id, manifest_id),
        )
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_cancel(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    tauri::async_runtime::spawn_blocking(move || state.cancel_target(&request))
        .await
        .map_err(|error| format!("peer clone target cancellation worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command(async)]
pub fn peer_clone_target_status(
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<PeerCloneTargetStatus, String> {
    state
        .target_status(&target_request(endpoint, session_id, manifest_id))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_clone_finalize(
    app: AppHandle,
    state: State<'_, PeerCloneCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
) -> Result<PeerCloneFinalizeResult, String> {
    let state = state.inner().clone();
    let request = target_request(endpoint, session_id, manifest_id);
    let (app_root, peer_root) = app_peer_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        persistent_store::commands::with_store_mut(app.state(), |store| {
            state
                .finalize_target(store, &cas, &peer_root, &request)
                .map_err(as_store_error)
        })
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer clone target finalization worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{net::Ipv4Addr, thread, time::Duration};

    #[test]
    fn public_lan_clone_exposes_verified_production_gates() {
        assert_eq!(
            peer_clone_capabilities(),
            PeerCloneCapabilities {
                desktop: true,
                source_ready: true,
                atomic_activation_ready: true,
                lossless_backup_ready: true,
                http_transport_ready: true,
                large_fixture_passed: false,
                production_enabled: true,
            }
        );
    }

    #[test]
    fn production_gate_requires_lossless_backup_and_qualified_http_transport() {
        let otherwise_ready = PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: true,
        };

        assert!(PeerCloneCapabilities::from_gates(otherwise_ready).production_enabled);
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                lossless_backup_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
        assert!(
            !PeerCloneCapabilities::from_gates(PeerCloneGateState {
                http_transport_ready: false,
                ..otherwise_ready
            })
            .production_enabled
        );
    }

    #[test]
    fn large_fixture_is_release_evidence_not_a_runtime_gate() {
        let capabilities = PeerCloneCapabilities::from_gates(PeerCloneGateState {
            desktop: true,
            source_ready: true,
            atomic_activation_ready: true,
            lossless_backup_ready: true,
            http_transport_ready: true,
            large_fixture_passed: false,
        });

        assert!(capabilities.production_enabled);
        assert!(!capabilities.large_fixture_passed);
    }

    #[test]
    fn product_full_lossless_clone_waits_for_renderer_finalize() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 0);
        seed_product_store(&mut target_store, "Target", 0);
        let source = PeerCloneCommandState::default();

        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();

        assert_eq!(prepared.phase, PeerCloneSourcePhase::Prepared);
        assert!(prepared.pairing_uri.is_none());
        assert!(source.source_bind_address().unwrap().is_none());

        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let bound = source.source_bind_address().unwrap().unwrap();
        assert!(bound.ip().is_unspecified());
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        assert_eq!(
            pairing.advertised_endpoint,
            format!("http://192.168.1.4:{}", bound.port())
        );

        let request = PeerCloneTargetRequest {
            endpoint: format!("http://127.0.0.1:{}", bound.port()),
            session_id: pairing.session_id.clone(),
            manifest_id: pairing.manifest_id.clone(),
        };
        let target = PeerCloneCommandState::default();
        target
            .claim_target(
                &target_root.path().join("peer-sync"),
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        PeerCloneCommandState::default()
            .claim_target(
                &target_root.path().join("peer-sync"),
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        assert!(super::super::LanCloneClient::claim(
            &request.endpoint,
            &request.session_id,
            &pairing.claim,
        )
        .is_err());

        target
            .start_target_download(&target_root.path().join("peer-sync"), request.clone())
            .unwrap();
        let downloaded = wait_for_target_phase(&target, PeerCloneTargetPhase::AwaitingActivation);
        assert_eq!(downloaded.completed_bytes, downloaded.total_bytes.unwrap());
        let device_id = source.source_status().unwrap().devices[0].device_id.clone();
        source
            .revoke_source_device(&request.session_id, &device_id)
            .unwrap();
        assert!(source.source_status().unwrap().devices[0].revoked);
        assert_eq!(target_store.revision().unwrap(), 1);
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Target"
        );

        let finalized = target
            .finalize_target(
                &mut target_store,
                &target_cas,
                &target_root.path().join("peer-sync"),
                &request,
            )
            .unwrap();

        assert_eq!(finalized.revision, 2);
        assert_eq!(
            target.target_status(&request).unwrap().phase,
            PeerCloneTargetPhase::Completed
        );
        assert_eq!(
            target_store.read_root(None).unwrap().value["username"],
            "Source"
        );
        source.stop_source(pairing.session_id.as_str()).unwrap();
    }

    #[test]
    fn product_cancel_preserves_persisted_claim_and_verified_resume_state() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        seed_product_store(&mut source_store, "Source", 9 * 1024 * 1024);
        let source = PeerCloneCommandState::default();
        let prepared = source
            .prepare_source(
                &mut source_store,
                &source_cas,
                &source_root.path().join("peer-sync"),
                &NeverCancelled,
            )
            .unwrap();
        let running = source
            .start_source(
                prepared.session_id.as_deref().unwrap(),
                Ipv4Addr::new(192, 168, 1, 4),
            )
            .unwrap();
        let pairing = parse_product_pairing(running.pairing_uri.as_deref().unwrap());
        let bound = source.source_bind_address().unwrap().unwrap();
        let request = PeerCloneTargetRequest {
            endpoint: format!("http://127.0.0.1:{}", bound.port()),
            session_id: pairing.session_id,
            manifest_id: pairing.manifest_id,
        };
        let target = PeerCloneCommandState::default();
        let peer_root = target_root.path().join("peer-sync");
        target
            .claim_target(
                &peer_root,
                &request.endpoint,
                &request.session_id,
                &request.manifest_id,
                &pairing.claim,
            )
            .unwrap();
        target
            .start_target_download(&peer_root, request.clone())
            .unwrap();
        target.cancel_target(&request).unwrap();
        wait_for_target_phase(&target, PeerCloneTargetPhase::Cancelled);
        drop(target);

        let resumed = PeerCloneCommandState::default();
        resumed
            .resume_target_download(&peer_root, request.clone())
            .unwrap();
        wait_for_target_phase(&resumed, PeerCloneTargetPhase::AwaitingActivation);

        assert_eq!(source.source_status().unwrap().devices.len(), 1);
        source.stop_source(&request.session_id).unwrap();
    }

    fn wait_for_target_phase(
        state: &PeerCloneCommandState,
        expected: PeerCloneTargetPhase,
    ) -> PeerCloneTargetStatus {
        for _ in 0..2_000 {
            let status = state.target_status_current().unwrap();
            if status.phase == expected {
                return status;
            }
            if status.phase == PeerCloneTargetPhase::Failed {
                panic!("target failed before {expected:?}: {:?}", status.error);
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("target did not reach {expected:?}");
    }

    struct ProductPairing {
        advertised_endpoint: String,
        session_id: String,
        manifest_id: String,
        claim: String,
    }

    fn parse_product_pairing(value: &str) -> ProductPairing {
        let url = url::Url::parse(value).unwrap();
        ProductPairing {
            advertised_endpoint: url
                .query_pairs()
                .find(|(key, _)| key == "endpoint")
                .unwrap()
                .1
                .into_owned(),
            session_id: url
                .query_pairs()
                .find(|(key, _)| key == "session")
                .unwrap()
                .1
                .into_owned(),
            manifest_id: url
                .query_pairs()
                .find(|(key, _)| key == "manifest")
                .unwrap()
                .1
                .into_owned(),
            claim: url
                .fragment()
                .unwrap()
                .strip_prefix("claim=")
                .unwrap()
                .to_owned(),
        }
    }

    fn seed_product_store(store: &mut PersistentStore, username: &str, filler_bytes: usize) {
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
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
                    "productCloneFiller": "x".repeat(filler_bytes),
                }),
            )
            .unwrap();
        store
            .replace_put_presets(&staging, &[json!({ "name": "preset" })])
            .unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
    }
}
