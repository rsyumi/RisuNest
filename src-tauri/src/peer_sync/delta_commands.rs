use super::{
    execute_logical_delta_pull,
    lan::{LanCloneHostControl, LanLogicalDeltaClient, PreparedLogicalLanSession},
    logical_delta::decode_logical_manifest,
    LanCloneHost, LogicalDeltaActivation, LogicalDeltaObject, LogicalDeltaObjectSource,
    PeerSyncError, ReadyLogicalDeltaPlan,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    persistent_store::{
        self, establish_logical_common_base, logical_delta_source::LogicalDeltaSourceSession,
        PersistentLogicalDeltaTarget, PersistentStore, StoreError, PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
use serde::Serialize;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, UdpSocket},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};

const SOURCE_DEVICE_ID_FILE: &str = "source-device-id";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaCapabilities {
    desktop: bool,
    source_ready: bool,
    atomic_activation_ready: bool,
    authenticated_transport_ready: bool,
    production_enabled: bool,
}

#[tauri::command]
pub fn peer_delta_capabilities() -> PeerDeltaCapabilities {
    PeerDeltaCapabilities {
        desktop: true,
        source_ready: true,
        atomic_activation_ready: true,
        authenticated_transport_ready: true,
        production_enabled: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum PeerDeltaSourcePhase {
    Idle,
    Prepared,
    Running,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PeerDeltaSourceDevice {
    device_id: String,
    transferred_bytes: u64,
    current_object: Option<String>,
    last_seen_at: u64,
    revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaSourceStatus {
    session_id: Option<String>,
    manifest_id: Option<String>,
    pairing_uri: Option<String>,
    phase: PeerDeltaSourcePhase,
    devices: Vec<PeerDeltaSourceDevice>,
}

impl PeerDeltaSourceStatus {
    fn idle(phase: PeerDeltaSourcePhase) -> Self {
        Self {
            session_id: None,
            manifest_id: None,
            pairing_uri: None,
            phase,
            devices: Vec::new(),
        }
    }
}

struct DeltaSourceRuntime {
    session_id: String,
    manifest_id: String,
    pairing_uri: Option<String>,
    host: LanCloneHost,
    control: LanCloneHostControl,
    phase: PeerDeltaSourcePhase,
}

#[derive(Default)]
struct PeerDeltaRuntime {
    source_preparing: bool,
    source: Option<DeltaSourceRuntime>,
    stopped: bool,
    pull_in_progress: bool,
}

#[derive(Clone, Default)]
pub struct PeerDeltaCommandState {
    runtime: Arc<Mutex<PeerDeltaRuntime>>,
}

impl PeerDeltaCommandState {
    fn lock(&self) -> Result<MutexGuard<'_, PeerDeltaRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer delta command state mutex poisoned: {error}"))
        })
    }

    fn install_source(
        &self,
        session: LogicalDeltaSourceSession,
        source_device_id: &str,
        manifest_bytes: Vec<u8>,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let transport_session_id = uuid::Uuid::new_v4().to_string();
        let manifest_id = session.manifest_hash().to_owned();
        let objects = session.objects().to_vec();
        let prepared = PreparedLogicalLanSession::new(
            &transport_session_id,
            source_device_id,
            manifest_id.clone(),
            manifest_bytes,
            objects,
            Box::new(session),
        )?;
        let host = LanCloneHost::prepare_logical(prepared);
        let control = host.control();
        let mut runtime = self.lock()?;
        runtime.source_preparing = false;
        runtime.stopped = false;
        runtime.source = Some(DeltaSourceRuntime {
            session_id: transport_session_id,
            manifest_id,
            pairing_uri: None,
            host,
            control,
            phase: PeerDeltaSourcePhase::Prepared,
        });
        source_status(&runtime)
    }

    fn start_source(
        &self,
        session_id: &str,
        advertised_ip: Ipv4Addr,
    ) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        if source.phase != PeerDeltaSourcePhase::Prepared {
            return Err(PeerSyncError::Protocol(
                "peer delta source is not prepared".to_owned(),
            ));
        }
        let pairing = source.host.start()?;
        let address = source.host.address().ok_or_else(|| {
            PeerSyncError::Transport("peer delta source address is unavailable".to_owned())
        })?;
        let endpoint = format!("http://{advertised_ip}:{}", address.port());
        source.pairing_uri = Some(build_pairing_uri(&endpoint, &pairing)?);
        source.phase = PeerDeltaSourcePhase::Running;
        source_status(&runtime)
    }

    fn stop_source(&self, session_id: &str) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        source.host.stop()?;
        runtime.source = None;
        runtime.stopped = true;
        Ok(())
    }

    fn revoke(&self, session_id: &str, device_id: &str) -> Result<(), PeerSyncError> {
        let mut runtime = self.lock()?;
        let source = require_source(&mut runtime, session_id)?;
        if !source.control.revoke(device_id) {
            return Err(PeerSyncError::Validation(
                "peer delta source device is absent".to_owned(),
            ));
        }
        Ok(())
    }

    fn status(&self) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
        let runtime = self.lock()?;
        if runtime.source.is_none() {
            return Ok(PeerDeltaSourceStatus::idle(if runtime.stopped {
                PeerDeltaSourcePhase::Stopped
            } else {
                PeerDeltaSourcePhase::Idle
            }));
        }
        source_status(&runtime)
    }

    fn begin_pull(&self) -> Result<PullGuard, PeerSyncError> {
        let mut runtime = self.lock()?;
        if runtime.pull_in_progress {
            return Err(PeerSyncError::Protocol(
                "peer delta pull is already running".to_owned(),
            ));
        }
        runtime.pull_in_progress = true;
        Ok(PullGuard {
            state: self.clone(),
        })
    }
}

struct PullGuard {
    state: PeerDeltaCommandState,
}

struct AbortUnsealedJobOnDrop<'a>(&'a RefCell<DurableCasJob>);

impl Drop for AbortUnsealedJobOnDrop<'_> {
    fn drop(&mut self) {
        let mut job = self.0.borrow_mut();
        if !job.is_sealed() && !job.is_released() {
            let _ = job.release(CasReleaseOutcome::Aborted);
        }
    }
}

impl Drop for PullGuard {
    fn drop(&mut self) {
        if let Ok(mut runtime) = self.state.runtime.lock() {
            runtime.pull_in_progress = false;
        }
    }
}

fn require_source<'a>(
    runtime: &'a mut PeerDeltaRuntime,
    session_id: &str,
) -> Result<&'a mut DeltaSourceRuntime, PeerSyncError> {
    runtime
        .source
        .as_mut()
        .filter(|source| source.session_id == session_id)
        .ok_or_else(|| PeerSyncError::Validation("peer delta source session is absent".to_owned()))
}

fn source_status(runtime: &PeerDeltaRuntime) -> Result<PeerDeltaSourceStatus, PeerSyncError> {
    let Some(source) = &runtime.source else {
        return Ok(PeerDeltaSourceStatus::idle(PeerDeltaSourcePhase::Idle));
    };
    Ok(PeerDeltaSourceStatus {
        session_id: Some(source.session_id.clone()),
        manifest_id: Some(source.manifest_id.clone()),
        pairing_uri: source.pairing_uri.clone(),
        phase: source.phase,
        devices: source
            .control
            .devices()
            .into_iter()
            .map(|device| PeerDeltaSourceDevice {
                device_id: device.device_id,
                transferred_bytes: device.verified_bytes,
                current_object: device.current_object,
                last_seen_at: u64::try_from(device.last_seen_unix_ms).unwrap_or(u64::MAX),
                revoked: device.revoked,
            })
            .collect(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PeerDeltaPullResult {
    NoChanges {
        revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
    },
    Updated {
        revision: i64,
        transferred_objects: u64,
        transferred_bytes: u64,
    },
    FullCloneRequired {
        reason: &'static str,
    },
    Conflict {
        reason: &'static str,
    },
}

#[derive(Default)]
struct MeasuredTransferTotals {
    objects: Cell<u64>,
    bytes: Cell<u64>,
}

struct MeasuredLogicalDeltaSource<'a, S: LogicalDeltaObjectSource + ?Sized> {
    inner: &'a mut S,
    totals: Rc<MeasuredTransferTotals>,
}

impl<'a, S: LogicalDeltaObjectSource + ?Sized> MeasuredLogicalDeltaSource<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            totals: Rc::new(MeasuredTransferTotals::default()),
        }
    }

    fn totals(&self) -> (u64, u64) {
        (self.totals.objects.get(), self.totals.bytes.get())
    }
}

impl<S: LogicalDeltaObjectSource + ?Sized> LogicalDeltaObjectSource
    for MeasuredLogicalDeltaSource<'_, S>
{
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        let reader = self.inner.open_object(object)?;
        let objects = self.totals.objects.get().checked_add(1).ok_or_else(|| {
            PeerSyncError::Validation("logical transfer object count overflow".to_owned())
        })?;
        self.totals.objects.set(objects);
        Ok(Box::new(MeasuredLogicalDeltaReader {
            inner: reader,
            totals: Rc::clone(&self.totals),
        }))
    }
}

struct MeasuredLogicalDeltaReader {
    inner: Box<dyn Read>,
    totals: Rc<MeasuredTransferTotals>,
}

impl Read for MeasuredLogicalDeltaReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(output)?;
        let bytes = self
            .totals
            .bytes
            .get()
            .checked_add(read as u64)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "logical transfer byte count overflow",
                )
            })?;
        self.totals.bytes.set(bytes);
        Ok(read)
    }
}

#[allow(clippy::too_many_arguments)]
fn pull_logical_delta<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    source_device_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    let actual_revision = store.revision().map_err(store_error)?;
    if actual_revision != expected_revision {
        return Ok(PeerDeltaPullResult::Conflict {
            reason: "staleRevision",
        });
    }
    let local = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    if local.manifest.library_id != PRODUCT_LOGICAL_LIBRARY_ID {
        return Err(PeerSyncError::Validation(
            "active logical library identity is unsupported".to_owned(),
        ));
    }
    let remote_manifest = decode_logical_manifest(remote_manifest_bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    if remote_manifest.library_id != local.manifest.library_id {
        return Ok(PeerDeltaPullResult::FullCloneRequired {
            reason: "noExactCommonBase",
        });
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let job = RefCell::new(DurableCasJob::begin(
        app_root,
        &job_id,
        CasJobKind::LogicalDeltaTarget,
        now_millis()?,
    )?);
    let _abort_unsealed = AbortUnsealedJobOnDrop(&job);

    let staging_root = app_root.join("peer-delta").join("staging");
    let mut target = PersistentLogicalDeltaTarget::new_with_durable_job(
        store,
        cas,
        source_device_id,
        &local.manifest.library_id,
        &local.manifest.generation,
        remote_manifest_bytes,
        &staging_root,
        &job,
    )?;
    if !target.has_common_base()? {
        drop(target);
        job.borrow_mut().seal(store, now_millis()?)?;
        let bootstrap = establish_logical_common_base(
            store,
            cas,
            source_device_id,
            &local.manifest.library_id,
            &local.manifest.generation,
            expected_revision,
            remote_manifest_bytes,
        );
        return finish_bootstrap(&job, expected_revision, bootstrap);
    }

    let plan = match target.build_ready_plan(expected_revision) {
        Ok(plan) => plan,
        Err(error) => {
            drop(target);
            return classify_plan_error(error);
        }
    };
    let local_hashes = local
        .manifest
        .objects
        .iter()
        .map(|object| object.hash.clone())
        .collect::<BTreeSet<_>>();
    let remote_sizes = remote_manifest
        .objects
        .iter()
        .map(|object| (object.hash.clone(), object.size))
        .collect::<BTreeMap<_, _>>();
    let mut measured_source = MeasuredLogicalDeltaSource::new(remote_source);
    let activation = execute_logical_delta_pull(
        &plan,
        &local_hashes,
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    drop(target);
    match activation {
        Ok(LogicalDeltaActivation::Activated { revision }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(if plan.apply.is_empty() {
                PeerDeltaPullResult::NoChanges {
                    revision,
                    transferred_objects,
                    transferred_bytes,
                }
            } else {
                PeerDeltaPullResult::Updated {
                    revision,
                    transferred_objects,
                    transferred_bytes,
                }
            })
        }
        Ok(LogicalDeltaActivation::AlreadyActive { revision }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(PeerDeltaPullResult::NoChanges {
                revision,
                transferred_objects: 0,
                transferred_bytes: 0,
            })
        }
        Ok(LogicalDeltaActivation::Conflict {
            actual_revision,
            actual_base_manifest_hash,
        }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Ok(classify_activation_conflict(
                &plan,
                actual_revision,
                &actual_base_manifest_hash,
            ))
        }
        Err(error) => {
            if !job.borrow().is_sealed() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            classify_plan_error(error)
        }
    }
}

fn finish_bootstrap(
    job: &RefCell<DurableCasJob>,
    expected_revision: i64,
    bootstrap: Result<(), PeerSyncError>,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    match bootstrap {
        Ok(()) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Committed);
            Ok(PeerDeltaPullResult::NoChanges {
                revision: expected_revision,
                transferred_objects: 0,
                transferred_bytes: 0,
            })
        }
        Err(PeerSyncError::Validation(message)) if message.contains("remote content differs") => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            Ok(PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            })
        }
        Err(error @ PeerSyncError::ActivationConflict { .. }) => {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            classify_plan_error(error)
        }
        Err(error) => {
            if !job.borrow().is_sealed() {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            Err(error)
        }
    }
}

fn open_peer_delta_store(store: &PersistentStore) -> Result<PersistentStore, StoreError> {
    store.open_native_job_store()
}

fn classify_plan_error(error: PeerSyncError) -> Result<PeerDeltaPullResult, PeerSyncError> {
    match error {
        PeerSyncError::LogicalMergeConflict { .. } => Ok(PeerDeltaPullResult::Conflict {
            reason: "localAndRemoteChanged",
        }),
        PeerSyncError::ActivationConflict { expected, actual } => {
            Ok(PeerDeltaPullResult::Conflict {
                reason: if activation_conflict_is_revision(&expected, &actual) {
                    "staleRevision"
                } else {
                    "localAndRemoteChanged"
                },
            })
        }
        error => Err(error),
    }
}

fn activation_conflict_is_revision(expected: &Option<String>, actual: &Option<String>) -> bool {
    expected
        .as_deref()
        .is_some_and(|value| value.parse::<i64>().is_ok())
        && actual
            .as_deref()
            .is_some_and(|value| value.parse::<i64>().is_ok())
}

fn classify_activation_conflict(
    plan: &ReadyLogicalDeltaPlan,
    actual_revision: i64,
    actual_base_manifest_hash: &str,
) -> PeerDeltaPullResult {
    PeerDeltaPullResult::Conflict {
        reason: if actual_revision != plan.expected_local_revision {
            "staleRevision"
        } else if actual_base_manifest_hash != plan.expected_base_manifest_hash {
            "localAndRemoteChanged"
        } else {
            // The target can also reject an otherwise identical revision/base when
            // the active logical head no longer matches the prepared stage.
            "staleRevision"
        },
    }
}

#[tauri::command]
pub async fn peer_delta_prepare(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
) -> Result<PeerDeltaSourceStatus, String> {
    let state = state.inner().clone();
    let app_root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        {
            let mut runtime = state.lock().map_err(|error| error.to_string())?;
            if runtime.source_preparing || runtime.source.is_some() {
                return Err("peer delta source is already prepared".to_owned());
            }
            runtime.source_preparing = true;
        }
        let prepared = (|| {
            let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
            let built = persistent_store::commands::with_store_mut(app.state(), |store| {
                store.seal_or_initialize_active_logical_generation(&cas)
            })
            .map_err(|error| error.to_string())?;
            let session = LogicalDeltaSourceSession::open(
                &app_root,
                &app_root,
                &built.manifest.library_id,
                &built.manifest.generation,
            )
            .map_err(|error| error.to_string())?;
            let source_device_id = load_or_create_source_device_id(
                &app_root.join("peer-delta").join(SOURCE_DEVICE_ID_FILE),
            )
            .map_err(|error| error.to_string())?;
            state
                .install_source(session, &source_device_id, built.manifest_bytes)
                .map_err(|error| error.to_string())
        })();
        if prepared.is_err() {
            if let Ok(mut runtime) = state.runtime.lock() {
                runtime.source_preparing = false;
            }
        }
        prepared
    })
    .await
    .map_err(|error| format!("peer delta source preparation worker failed: {error}"))?
}

#[tauri::command]
pub fn peer_delta_start(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<PeerDeltaSourceStatus, String> {
    let address = discover_lan_ipv4().map_err(|error| error.to_string())?;
    state
        .start_source(&session_id, address)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_delta_status(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<PeerDeltaSourceStatus, String> {
    state.status().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn peer_delta_stop(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || state.stop_source(&session_id))
        .await
        .map_err(|error| format!("peer delta source stop worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn peer_delta_revoke(
    state: State<'_, PeerDeltaCommandState>,
    session_id: String,
    device_id: String,
) -> Result<(), String> {
    state
        .revoke(&session_id, &device_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn peer_delta_pull(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    endpoint: String,
    session_id: String,
    manifest_id: String,
    claim: String,
    expected_revision: i64,
) -> Result<PeerDeltaPullResult, String> {
    let state = state.inner().clone();
    let app_root = app_root(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_pull().map_err(|error| error.to_string())?;
        let mut client = LanLogicalDeltaClient::claim(&endpoint, &session_id, &manifest_id, &claim)
            .map_err(|error| error.to_string())?;
        let manifest = client.fetch_manifest().map_err(|error| error.to_string())?;
        let source_device_id = client.source_device_id().to_owned();
        let mut store = persistent_store::commands::with_store_mut(app.state(), |store| {
            open_peer_delta_store(store)
        })
        .map_err(|error| error.to_string())?;
        let cas = PayloadCas::new(&app_root).map_err(|error| error.to_string())?;
        pull_logical_delta(
            &mut store,
            &cas,
            &app_root,
            &source_device_id,
            expected_revision,
            &manifest,
            &mut client,
        )
        .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| format!("peer delta pull worker failed: {error}"))?
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

fn build_pairing_uri(endpoint: &str, pairing: &super::LanPairing) -> Result<String, PeerSyncError> {
    let mut uri = url::Url::parse("risuailocal://peer-delta/v1")
        .map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
    uri.query_pairs_mut()
        .append_pair("endpoint", endpoint)
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

pub(crate) fn load_or_create_source_device_id(path: &Path) -> Result<String, PeerSyncError> {
    if path.exists() {
        return read_source_device_id(path);
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("source device identity path has no parent".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".source-device-{}.tmp", uuid::Uuid::new_v4()));
    let identity = uuid::Uuid::new_v4().to_string();
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(identity.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(identity.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_source_device_id(path: &Path) -> Result<String, PeerSyncError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 64 {
        return Err(PeerSyncError::Storage(
            "source device identity file is invalid".to_owned(),
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(65).read_to_end(&mut bytes)?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| PeerSyncError::Storage("source device identity is not UTF-8".to_owned()))?
        .trim_end_matches(['\r', '\n']);
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| {
        PeerSyncError::Storage("source device identity is not a canonical UUID".to_owned())
    })?;
    if parsed.to_string() != value {
        return Err(PeerSyncError::Storage(
            "source device identity is not canonical".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn now_millis() -> Result<i64, PeerSyncError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| PeerSyncError::Storage("system time exceeds SQLite integer range".to_owned()))
}

fn store_error(error: StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asset_repository::job_pins::{collect_durable_cas_job_roots, CasObjectRole},
        peer_sync::{
            logical_delta::{
                build_logical_manifest, BuiltLogicalManifest, LogicalManifest,
                LogicalManifestBuilderInput, LogicalRecordEnvelope, LogicalRecordLocator,
                ProjectedLogicalRecord,
            },
            LogicalDeltaObjectSource,
        },
        persistent_store::WorkingSetCommit,
    };
    use serde_json::json;
    use std::{
        io::{Cursor, Read},
        sync::mpsc,
        thread,
    };

    struct FixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        reads: usize,
    }

    struct BlockingFixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        opened: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl LogicalDeltaObjectSource for BlockingFixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.opened
                .send(())
                .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
            self.release
                .recv()
                .map_err(|error| PeerSyncError::Transport(error.to_string()))?;
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    impl LogicalDeltaObjectSource for FixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.reads += 1;
            let bytes = self.objects.get(&object.hash).ok_or_else(|| {
                PeerSyncError::Transport(format!("fixture object {} is absent", object.hash))
            })?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    fn empty_source() -> FixtureSource {
        FixtureSource {
            objects: BTreeMap::new(),
            reads: 0,
        }
    }

    fn remote_root_manifest(
        base: &LogicalManifest,
        generation: &str,
        value: serde_json::Value,
    ) -> BuiltLogicalManifest {
        build_logical_manifest(LogicalManifestBuilderInput {
            library_id: base.library_id.clone(),
            generation: generation.to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: Some(base.generation.clone()),
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::live(
                LogicalRecordLocator::Root,
                LogicalRecordEnvelope::Root {
                    value,
                    owner_heads: vec![],
                },
                vec![],
            )],
        })
        .unwrap()
    }

    #[test]
    fn source_device_identity_is_stable_and_outside_the_persistent_store() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("peer-delta").join(SOURCE_DEVICE_ID_FILE);

        let first = load_or_create_source_device_id(&path).unwrap();
        let second = load_or_create_source_device_id(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().to_string(), first);
        assert_eq!(path.parent().unwrap(), root.path().join("peer-delta"));
    }

    #[test]
    fn invalid_persisted_source_device_identity_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(SOURCE_DEVICE_ID_FILE);
        fs::write(&path, b"not-a-device-id\n").unwrap();

        assert!(matches!(
            load_or_create_source_device_id(&path),
            Err(PeerSyncError::Storage(_))
        ));
    }

    #[test]
    fn delta_pairing_uses_a_dedicated_scheme_without_changing_clone_links() {
        let pairing = super::super::LanPairing {
            session_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            manifest_id: "1".repeat(64),
            claim: "2".repeat(64),
        };

        let uri = build_pairing_uri("http://192.168.1.8:1234", &pairing).unwrap();

        assert!(uri.starts_with("risuailocal://peer-delta/v1?"));
        assert!(uri.ends_with(&format!("#claim={}", pairing.claim)));
    }

    #[test]
    fn typed_merge_conflict_projects_to_the_product_conflict_result() {
        assert_eq!(
            classify_plan_error(PeerSyncError::LogicalMergeConflict {
                record: "plugin:shared".to_owned(),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn planning_revision_cas_conflict_projects_as_stale_revision() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("7".to_owned()),
                actual: Some("8".to_owned()),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn planning_base_cas_conflict_projects_as_local_and_remote_changed() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("a".repeat(64)),
                actual: Some("c".repeat(64)),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn changed_retry_revision_cas_conflict_projects_as_stale_revision() {
        assert_eq!(
            classify_plan_error(PeerSyncError::ActivationConflict {
                expected: Some("8".to_owned()),
                actual: Some("9".to_owned()),
            })
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn bootstrap_revision_conflict_releases_its_sealed_durable_roots() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "bootstrap-conflict",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"bootstrap manifest", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();
        assert!(!collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .is_empty());

        assert_eq!(
            finish_bootstrap(
                &job,
                7,
                Err(PeerSyncError::ActivationConflict {
                    expected: Some("7".to_owned()),
                    actual: Some("8".to_owned()),
                }),
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
        assert_eq!(store.revision().unwrap(), 0);
    }

    fn activation_conflict_plan() -> ReadyLogicalDeltaPlan {
        ReadyLogicalDeltaPlan {
            expected_local_revision: 7,
            expected_base_manifest_hash: "a".repeat(64),
            expected_remote_generation: "remote".to_owned(),
            apply: vec![],
            preserve_local_keys: vec![],
            candidate_object_hashes: vec![],
            next_base_manifest_hash: "b".repeat(64),
            next_base_generation_sequence: "8".to_owned(),
        }
    }

    #[test]
    fn coordinator_projects_revision_cas_conflict_as_stale_revision() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(&plan, 8, &plan.expected_base_manifest_hash),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn coordinator_projects_base_cas_conflict_as_local_and_remote_changed() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(&plan, 7, &"c".repeat(64)),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
    }

    #[test]
    fn coordinator_projects_matching_revision_and_base_conflict_as_stale_revision() {
        let plan = activation_conflict_plan();

        assert_eq!(
            classify_activation_conflict(
                &plan,
                plan.expected_local_revision,
                &plan.expected_base_manifest_hash,
            ),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
    }

    #[test]
    fn coordinator_bootstraps_only_an_exact_peer_and_preserves_stale_or_divergent_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let mut source = empty_source();

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                0,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges {
                revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                0,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges {
                revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000001",
                1,
                &local.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);

        let other = remote_root_manifest(&local.manifest, "remote-other", json!({"side":"remote"}));
        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000002",
                0,
                &other.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                "00000000-0000-4000-8000-000000000002",
                0,
                &other.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            }
        );
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(source.reads, 0);
    }

    #[test]
    fn coordinator_fetches_only_changed_objects_and_activates_the_exact_revision() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000003";
        let mut source = empty_source();
        pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut source,
        )
        .unwrap();
        let remote =
            remote_root_manifest(&local.manifest, "remote-updated", json!({"side":"remote"}));
        source.objects = remote
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect();

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                0,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Updated {
                revision: 1,
                transferred_objects: 1,
                transferred_bytes: remote.record_objects[0].object.size,
            }
        );
        assert_eq!(source.reads, 1);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value,
            json!({"side":"remote"})
        );
    }

    #[test]
    fn dedicated_pull_connection_does_not_hold_the_managed_store_during_object_io() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut setup = PersistentStore::open(directory.path()).unwrap();
        let local = setup
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000013";
        pull_logical_delta(
            &mut setup,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote = remote_root_manifest(&local.manifest, "remote-slow", json!({"side":"remote"}));
        let export_lease = setup.acquire_revision(0).unwrap();
        let exported = setup.export_risu_save(&export_lease.lease, true).unwrap();
        let exported_path = PathBuf::from(&exported.path);
        let ownership_path = exported_path.with_extension("lease");
        let managed_store = Arc::new(Mutex::new(setup));
        let mut pull_store = {
            let managed = managed_store.lock().unwrap();
            open_peer_delta_store(&managed).unwrap()
        };
        assert!(exported_path.is_file());
        assert!(ownership_path.is_file());
        let (opened_tx, opened_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let root = directory.path().to_path_buf();
        let remote_bytes = remote.manifest_bytes.clone();
        let source = BlockingFixtureSource {
            objects: remote
                .record_objects
                .iter()
                .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
                .collect(),
            opened: opened_tx,
            release: release_rx,
        };
        let pull = thread::spawn(move || {
            let mut source = source;
            let cas = PayloadCas::new(&root).unwrap();
            pull_logical_delta(
                &mut pull_store,
                &cas,
                &root,
                peer,
                0,
                &remote_bytes,
                &mut source,
            )
        });
        opened_rx.recv().unwrap();

        assert_eq!(
            managed_store
                .try_lock()
                .unwrap()
                .read_root(None)
                .unwrap()
                .value,
            json!({})
        );
        assert!(exported_path.is_file());
        assert!(ownership_path.is_file());
        release_tx.send(()).unwrap();
        assert_eq!(
            pull.join().unwrap().unwrap(),
            PeerDeltaPullResult::Updated {
                revision: 1,
                transferred_objects: 1,
                transferred_bytes: remote.record_objects[0].object.size,
            }
        );
    }

    #[test]
    fn coordinator_returns_a_structured_same_record_conflict_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let base = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000004";
        let mut source = empty_source();
        pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &base.manifest_bytes,
            &mut source,
        )
        .unwrap();
        store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side":"local"})),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();
        let remote =
            remote_root_manifest(&base.manifest, "remote-conflict", json!({"side":"remote"}));

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
        assert_eq!(source.reads, 0);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value,
            json!({"side":"local"})
        );
        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut source,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "localAndRemoteChanged",
            }
        );
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(source.reads, 0);
    }
}
