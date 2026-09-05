#[cfg(test)]
use super::android_foreground::test_registry_guard;
#[cfg(any(target_os = "android", test))]
use super::android_foreground::{
    registry, AndroidCancellationProbe, AndroidForegroundKey, AndroidForegroundLane,
};
#[cfg(target_os = "android")]
use super::command_codes::finish_peer_command;
#[cfg(any(target_os = "android", test))]
use super::command_codes::is_bounded_code;
use super::logical_delta_transfer::execute_logical_delta_pull_with_commit_intent;
#[cfg(test)]
use super::target_foreground_transition::AndroidTargetForegroundTransitionPhase as AndroidTargetForegroundPhase;
#[cfg(any(target_os = "android", test))]
use super::target_foreground_transition::{
    AndroidTargetForegroundTransition, AndroidTargetForegroundTransitionError,
};
use super::{
    command_codes::{code_for, finish_peer_worker, PeerCommandCode},
    delta_completion::{
        abandon_retained_delta_completion, recover_delta_completion, retained_delta_completion,
        DeltaCompletionContext, LanDeltaCompletionTransport, PeerDeltaCompletionJournal,
        RecoveredDeltaCompletion, RetainedDeltaCompletion, RetainedDeltaWitness,
    },
    lan::{LanLogicalDeltaClient, PreparedLogicalLanSession},
    logical_delta::{decode_logical_manifest, hash_logical_manifest},
    LogicalDeltaActivation, LogicalDeltaObject, LogicalDeltaObjectSource, PeerSyncError,
    ReadyLogicalDeltaPlan,
};
use crate::{
    asset_repository::{
        job_pins::{
            reclaim_abandoned_durable_cas_jobs, CasJobKind, CasReleaseOutcome, DurableCasJob,
        },
        PayloadCas,
    },
    local_backup::CancellationProbe,
    persistent_store::{
        self, establish_logical_common_base_with_commit_intent,
        logical_delta_source::LogicalDeltaSourceSession, PersistentLogicalDeltaTarget,
        PersistentStore, StoreError, SyncGenerationIdentity, PRODUCT_LOGICAL_LIBRARY_ID,
    },
};
// The registered desktop pull is the one product caller that never cancels.
#[cfg(any(desktop, test))]
use crate::local_backup::NeverCancelled;
use serde::Serialize;
#[cfg(test)]
use std::fs;
#[cfg(any(target_os = "android", test))]
use std::future::Future;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, State};

const P4_DELTA_TARGET_JOB_PREFIX: &str = "p4-delta-target-";
pub(crate) const P4_SOURCE_PIN_PREFIX: &str = "logical-session-p4-source-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDeltaCapabilities {
    desktop: bool,
    atomic_activation_ready: bool,
    authenticated_transport_ready: bool,
    production_enabled: bool,
}

#[tauri::command]
pub fn peer_delta_capabilities() -> PeerDeltaCapabilities {
    PeerDeltaCapabilities {
        desktop: cfg!(desktop),
        atomic_activation_ready: true,
        authenticated_transport_ready: true,
        production_enabled: true,
    }
}

#[derive(Default)]
struct PeerDeltaRuntime {
    pull_in_progress: bool,
    #[cfg(any(target_os = "android", test))]
    target_foreground: Option<AndroidTargetForegroundStatus>,
}

/// Hostless P4 source preparation.  Keeping the logical session inside the
/// prepared transport makes its generation pin live exactly until shared
/// source cleanup drops the transport.
pub(crate) struct PreparedSharedDeltaSource {
    session: Option<PreparedLogicalLanSession>,
}

impl PreparedSharedDeltaSource {
    pub(crate) fn take_session(&mut self) -> Result<PreparedLogicalLanSession, PeerSyncError> {
        self.session.take().ok_or_else(|| {
            PeerSyncError::Protocol("shared delta source session is unavailable".to_owned())
        })
    }

    pub(crate) fn cleanup(&mut self) {
        self.session.take();
    }
}

pub(crate) fn prepare_shared_delta_source(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
) -> Result<PreparedSharedDeltaSource, PeerSyncError> {
    store
        .reclaim_logical_generation_pins(P4_SOURCE_PIN_PREFIX)
        .map_err(store_error)?;
    let built = store
        .seal_or_initialize_active_logical_generation(cas)
        .map_err(store_error)?;
    let session_store = store.open_native_job_store().map_err(store_error)?;
    let session = LogicalDeltaSourceSession::open_owned(
        session_store,
        app_root,
        &built.manifest.library_id,
        &built.manifest.generation,
        P4_SOURCE_PIN_PREFIX,
    )?;
    let source_device_id = canonical_source_device_id(app_root)?;
    let transport_session_id = uuid::Uuid::new_v4().to_string();
    let manifest_id = session.manifest_hash().to_owned();
    let objects = session.objects().to_vec();
    let prepared = PreparedLogicalLanSession::new(
        &transport_session_id,
        &source_device_id,
        manifest_id,
        built.manifest_bytes,
        objects,
        Box::new(session),
    )?;
    Ok(PreparedSharedDeltaSource {
        session: Some(prepared),
    })
}

#[cfg(any(target_os = "android", test))]
type AndroidTargetForegroundStatus = AndroidTargetForegroundTransition<PeerDeltaPullResult>;

#[derive(Clone)]
pub struct PeerDeltaCommandState {
    runtime: Arc<Mutex<PeerDeltaRuntime>>,
    // Serializes the Android target foreground lifecycle; the desktop lane has
    // no foreground service to keep in step with the runtime.
    #[cfg(any(target_os = "android", test))]
    lifecycle_operation: Arc<Mutex<()>>,
}

impl Default for PeerDeltaCommandState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(PeerDeltaRuntime::default())),
            #[cfg(any(target_os = "android", test))]
            lifecycle_operation: Arc::new(Mutex::new(())),
        }
    }
}

impl PeerDeltaCommandState {
    #[cfg(any(target_os = "android", test))]
    fn lock_lifecycle_operation(&self) -> Result<MutexGuard<'_, ()>, PeerSyncError> {
        self.lifecycle_operation.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer delta lifecycle mutex poisoned: {error}"))
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, PeerDeltaRuntime>, PeerSyncError> {
        self.runtime.lock().map_err(|error| {
            PeerSyncError::Storage(format!("peer delta command state mutex poisoned: {error}"))
        })
    }

    #[cfg(any(target_os = "android", test))]
    fn reserve_target_foreground(&self) -> Result<AndroidForegroundKey, PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        if runtime.target_foreground.is_some() {
            return Err(PeerSyncError::Protocol(
                "Android peer delta target foreground cleanup is pending".to_owned(),
            ));
        }
        let foreground = registry()
            .reserve(AndroidForegroundLane::P4Target)
            .map_err(PeerSyncError::Protocol)?;
        runtime.target_foreground =
            Some(AndroidTargetForegroundStatus::reserved(foreground.clone()));
        Ok(foreground)
    }

    #[cfg(any(target_os = "android", test))]
    fn mark_target_running_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android peer delta target foreground is absent".to_owned())
        })?;
        target
            .require_exact_reserved(foreground)
            .map_err(|error| match error {
                AndroidTargetForegroundTransitionError::Stale => PeerSyncError::Protocol(
                    "Android peer delta target foreground identity is stale".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotReserved => PeerSyncError::Protocol(
                    "Android peer delta target foreground is not reserved".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotRunning => unreachable!(),
            })?;
        if !registry().retain_target_exact(foreground) {
            return Err(PeerSyncError::Protocol(
                "Android peer delta target foreground could not be retained".to_owned(),
            ));
        }
        target.mark_running();
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn publish_target_terminal_exact(
        &self,
        foreground: &AndroidForegroundKey,
        outcome: Result<PeerDeltaPullResult, String>,
    ) -> Result<(), PeerSyncError> {
        let _operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let target = runtime.target_foreground.as_mut().ok_or_else(|| {
            PeerSyncError::Protocol("Android peer delta target foreground is absent".to_owned())
        })?;
        target
            .require_exact_running(foreground)
            .map_err(|error| match error {
                AndroidTargetForegroundTransitionError::Stale => PeerSyncError::Protocol(
                    "Android peer delta target foreground identity is stale".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotRunning => PeerSyncError::Protocol(
                    "Android peer delta target foreground is not running".to_owned(),
                ),
                AndroidTargetForegroundTransitionError::NotReserved => unreachable!(),
            })?;
        target.publish_terminal(outcome);
        Ok(())
    }

    #[cfg(any(target_os = "android", test))]
    fn cancel_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        {
            let runtime = self.lock()?;
            let Some(target) = runtime.target_foreground.as_ref() else {
                return Ok(false);
            };
            if !target.matches(foreground) {
                return Ok(false);
            }
        }
        // The runtime guard is dropped before cancel_exact: a registered
        // source_stop callback runs synchronously on this thread and
        // re-locks this non-reentrant mutex. cancel_exact keeps the
        // exact-generation semantics through its own key equality check.
        Ok(registry().cancel_exact(foreground))
    }

    #[cfg(any(target_os = "android", test))]
    fn target_foreground_status(
        &self,
    ) -> Result<Option<AndroidTargetForegroundStatus>, PeerSyncError> {
        Ok(self.lock()?.target_foreground.clone())
    }

    #[cfg(any(target_os = "android", test))]
    fn release_target_foreground_exact(
        &self,
        foreground: &AndroidForegroundKey,
    ) -> Result<bool, PeerSyncError> {
        let operation = self.lock_lifecycle_operation()?;
        let mut runtime = self.lock()?;
        let Some(target) = runtime.target_foreground.as_ref() else {
            return Ok(registry().release_target_exact(foreground));
        };
        if !target.matches(foreground) {
            return Ok(false);
        }
        if target.is_running() {
            // Both guards are dropped before cancel_exact: a registered
            // source_stop callback runs synchronously on this thread and
            // re-locks these non-reentrant mutexes. cancel_exact keeps the
            // exact-generation semantics through its own key equality check.
            drop(runtime);
            drop(operation);
            let _ = registry().cancel_exact(foreground);
            return Ok(false);
        }
        if !registry().release_target_exact(foreground) {
            return Ok(false);
        }
        runtime.target_foreground = None;
        Ok(true)
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeltaCompletionAttempt {
    operation_id: String,
    source_bearer: String,
}

#[allow(clippy::too_many_arguments)]
fn delta_completion_context(
    attempt: &DeltaCompletionAttempt,
    source_device_id: &str,
    manifest_id: &str,
    pre_revision: i64,
    pre_common_base: Option<SyncGenerationIdentity>,
    post_revision: i64,
    post_common_base: SyncGenerationIdentity,
    transferred_objects: u64,
    transferred_bytes: u64,
) -> DeltaCompletionContext {
    DeltaCompletionContext {
        operation_id: attempt.operation_id.clone(),
        source_device_id: source_device_id.to_owned(),
        manifest_id: manifest_id.to_owned(),
        pre_revision,
        pre_common_base,
        post_revision,
        post_common_base,
        transferred_objects,
        transferred_bytes,
    }
}

fn publish_delta_activation_intent(
    app_root: &Path,
    context: &DeltaCompletionContext,
    expected_source_bearer: &str,
    intent_written: &Cell<bool>,
) -> Result<(), PeerSyncError> {
    intent_written.set(true);
    let _source_lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
    let source =
        super::device_registry::incoming_source_by_id(app_root, &context.source_device_id)?
            .ok_or_else(|| {
                PeerSyncError::Validation("registered incoming source is missing".to_owned())
            })?;
    if source.bearer != expected_source_bearer || !source.permissions.allows_read() {
        return Err(PeerSyncError::Validation(
            "registered delta source credential or permission changed".to_owned(),
        ));
    }
    PeerDeltaCompletionJournal::new(app_root).store_activation_intent(context)
}

fn selection_totals(
    selection: &super::LogicalDeltaTransferSelection,
) -> Result<(u64, u64), PeerSyncError> {
    let objects = u64::try_from(selection.missing_objects().len()).map_err(|_| {
        PeerSyncError::Validation("logical transfer object count overflow".to_owned())
    })?;
    let bytes = selection
        .missing_objects()
        .iter()
        .try_fold(0_u64, |total, object| {
            total.checked_add(object.size).ok_or_else(|| {
                PeerSyncError::Validation("logical transfer byte count overflow".to_owned())
            })
        })?;
    Ok((objects, bytes))
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

/// Every logical delta pull is a registered pull: it carries the completion
/// attempt whose activation intent the completion journal retains until the
/// recovery pass settles it.
#[allow(clippy::too_many_arguments)]
fn pull_logical_delta<S: LogicalDeltaObjectSource + ?Sized>(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    app_root: &Path,
    source_device_id: &str,
    expected_revision: i64,
    remote_manifest_bytes: &[u8],
    remote_source: &mut S,
    cancellation: &dyn CancellationProbe,
    completion: &DeltaCompletionAttempt,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
    if cancellation.is_cancelled() {
        return Err(PeerSyncError::Cancelled);
    }
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
    let manifest_id = hash_logical_manifest(&remote_manifest)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    let pre_common_base = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, source_device_id)
        .map_err(store_error)?;
    let post_common_base = SyncGenerationIdentity {
        generation_id: remote_manifest.generation.clone(),
        manifest_hash: manifest_id.clone(),
        generation_sequence: remote_manifest.generation_sequence.clone(),
    };

    let job_id = format!(
        "{P4_DELTA_TARGET_JOB_PREFIX}{}",
        completion.operation_id.as_str()
    );
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
        if cancellation.is_cancelled() {
            let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            return Err(PeerSyncError::Cancelled);
        }
        let intent_written = Cell::new(false);
        let bootstrap_context = delta_completion_context(
            completion,
            source_device_id,
            &manifest_id,
            expected_revision,
            pre_common_base.clone(),
            expected_revision,
            post_common_base.clone(),
            0,
            0,
        );
        let bootstrap = establish_logical_common_base_with_commit_intent(
            store,
            cas,
            source_device_id,
            &local.manifest.library_id,
            &local.manifest.generation,
            expected_revision,
            remote_manifest_bytes,
            || {
                if cancellation.is_cancelled() {
                    return Err(PeerSyncError::Cancelled);
                }
                publish_delta_activation_intent(
                    app_root,
                    &bootstrap_context,
                    &completion.source_bearer,
                    &intent_written,
                )
            },
        );
        return finish_bootstrap(&job, expected_revision, bootstrap, intent_written.get());
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
    let post_revision = if plan.apply.is_empty() {
        expected_revision
    } else {
        expected_revision
            .checked_add(1)
            .ok_or_else(|| PeerSyncError::Validation("logical revision overflow".to_owned()))?
    };
    let intent_written = Cell::new(false);
    let activation = execute_logical_delta_pull_with_commit_intent(
        &plan,
        &local_hashes,
        cas,
        &remote_sizes,
        &mut measured_source,
        &mut target,
        |_| Ok(()),
        |selection| {
            let (transferred_objects, transferred_bytes) = selection_totals(selection)?;
            let context = delta_completion_context(
                completion,
                source_device_id,
                &manifest_id,
                expected_revision,
                pre_common_base.clone(),
                post_revision,
                post_common_base.clone(),
                transferred_objects,
                transferred_bytes,
            );
            publish_delta_activation_intent(
                app_root,
                &context,
                &completion.source_bearer,
                &intent_written,
            )
        },
        cancellation,
    );
    let (transferred_objects, transferred_bytes) = measured_source.totals();
    let durable_abort_succeeded = target.durable_abort_succeeded();
    drop(target);
    finish_pull(
        &job,
        &plan,
        activation,
        transferred_objects,
        transferred_bytes,
        durable_abort_succeeded,
        intent_written.get(),
    )
}

fn finish_pull(
    job: &RefCell<DurableCasJob>,
    plan: &ReadyLogicalDeltaPlan,
    activation: Result<LogicalDeltaActivation, PeerSyncError>,
    transferred_objects: u64,
    transferred_bytes: u64,
    durable_abort_succeeded: bool,
    intent_written: bool,
) -> Result<PeerDeltaPullResult, PeerSyncError> {
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
            if !intent_written {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            Ok(classify_activation_conflict(
                &plan,
                actual_revision,
                &actual_base_manifest_hash,
            ))
        }
        Err(error) => {
            if !intent_written && (!job.borrow().is_sealed() || durable_abort_succeeded) {
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
    intent_written: bool,
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
            if !intent_written {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            Ok(PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            })
        }
        Err(error @ PeerSyncError::ActivationConflict { .. }) => {
            if !intent_written {
                let _ = job.borrow_mut().release(CasReleaseOutcome::Aborted);
            }
            classify_plan_error(error)
        }
        Err(error) => {
            if !intent_written {
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
#[cfg(target_os = "android")]
pub fn peer_delta_target_reserve(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<AndroidForegroundKey, String> {
    finish_peer_command(
        "peer delta target foreground reservation",
        state.reserve_target_foreground(),
    )
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_status(
    state: State<'_, PeerDeltaCommandState>,
) -> Result<Option<AndroidTargetForegroundStatus>, String> {
    finish_peer_command(
        "peer delta target foreground status",
        state.target_foreground_status(),
    )
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_release(
    state: State<'_, PeerDeltaCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    finish_peer_command(
        "peer delta target foreground release",
        state.release_target_foreground_exact(&foreground),
    )
}

#[tauri::command]
#[cfg(target_os = "android")]
pub fn peer_delta_target_foreground_cancel(
    state: State<'_, PeerDeltaCommandState>,
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    finish_peer_command(
        "peer delta target foreground cancellation",
        state.cancel_target_foreground_exact(&foreground),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerDeltaRetainedWitness {
    Committed,
    Uncommitted,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedDeltaCompletionStatus {
    operation_id: String,
    source_device_id: String,
    source_name: Option<String>,
    witness: PeerDeltaRetainedWitness,
    transferred_objects: u64,
    transferred_bytes: u64,
}

impl From<RetainedDeltaCompletion> for RetainedDeltaCompletionStatus {
    fn from(value: RetainedDeltaCompletion) -> Self {
        Self {
            operation_id: value.operation_id,
            source_device_id: value.source_device_id,
            source_name: value.source_name,
            witness: match value.witness {
                RetainedDeltaWitness::Committed => PeerDeltaRetainedWitness::Committed,
                RetainedDeltaWitness::Uncommitted => PeerDeltaRetainedWitness::Uncommitted,
                RetainedDeltaWitness::Ambiguous => PeerDeltaRetainedWitness::Ambiguous,
            },
            transferred_objects: value.transferred_objects,
            transferred_bytes: value.transferred_bytes,
        }
    }
}

/// The pull owns the same journal, so its guard is what keeps a status read and
/// a running pull apart. The store is opened under the guard and only once a
/// journal is actually there, so the usual empty answer opens nothing.
fn retained_completion_under_pull_guard(
    state: &PeerDeltaCommandState,
    app_root: &Path,
    open_store: impl FnOnce() -> Result<PersistentStore, PeerSyncError>,
) -> Result<Option<RetainedDeltaCompletionStatus>, PeerSyncError> {
    let _guard = state.begin_pull()?;
    retained_delta_completion(app_root, open_store)
        .map(|retained| retained.map(RetainedDeltaCompletionStatus::from))
}

fn abandon_retained_completion_under_pull_guard(
    state: &PeerDeltaCommandState,
    app_root: &Path,
    operation_id: &str,
) -> Result<(), PeerSyncError> {
    let _guard = state.begin_pull()?;
    abandon_retained_delta_completion(app_root, operation_id)
}

/// Names the retained delta completion this target still holds, so the
/// interface can offer resuming or cancelling it instead of only meeting the
/// refusal it causes. A local read, so it needs no Android foreground service.
#[tauri::command]
pub async fn peer_delta_target_retained(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
) -> Result<Option<RetainedDeltaCompletionStatus>, String> {
    let state = state.inner().clone();
    finish_peer_worker(
        "peer delta retained completion status",
        tauri::async_runtime::spawn_blocking(move || {
            let app_root = delta_app_data_root(&app)?;
            retained_completion_under_pull_guard(&state, &app_root, || {
                persistent_store::commands::with_store_mut(app.state(), |store| {
                    open_peer_delta_store(store)
                })
                .map_err(store_error)
            })
        })
        .await,
    )
}

/// Drops the retained delta completion named by `operation_id`. Local cleanup
/// only: this device's data is left exactly as it is and the source is never
/// asked for anything, so it needs no Android foreground service either.
#[tauri::command]
pub async fn peer_delta_target_abandon(
    app: AppHandle,
    state: State<'_, PeerDeltaCommandState>,
    operation_id: String,
) -> Result<(), String> {
    let state = state.inner().clone();
    finish_peer_worker(
        "peer delta retained completion abandonment",
        tauri::async_runtime::spawn_blocking(move || {
            abandon_retained_completion_under_pull_guard(
                &state,
                &delta_app_data_root(&app)?,
                &operation_id,
            )
        })
        .await,
    )
}

#[cfg(desktop)]
pub(crate) async fn peer_delta_pull_registered_client(
    app: AppHandle,
    state: PeerDeltaCommandState,
    client: LanLogicalDeltaClient,
    expected_revision: i64,
) -> Result<PeerDeltaPullResult, String> {
    peer_delta_pull_with_client_factory(app, state, expected_revision, NeverCancelled, move |_| {
        Ok(client)
    })
    .await
}

#[cfg(target_os = "android")]
pub(crate) async fn peer_delta_pull_registered_client(
    app: AppHandle,
    state: PeerDeltaCommandState,
    client: LanLogicalDeltaClient,
    expected_revision: i64,
    foreground: AndroidForegroundKey,
) -> Result<PeerDeltaPullResult, String> {
    let operation_state = state.clone();
    run_registered_delta_target_operation(state, foreground, move |cancellation| async move {
        peer_delta_pull_with_client_factory(
            app,
            operation_state,
            expected_revision,
            cancellation,
            move |_| Ok(client),
        )
        .await
    })
    .await
}

#[cfg(any(target_os = "android", test))]
async fn run_registered_delta_target_operation<F, Fut>(
    state: PeerDeltaCommandState,
    foreground: AndroidForegroundKey,
    operation: F,
) -> Result<PeerDeltaPullResult, String>
where
    F: FnOnce(AndroidCancellationProbe) -> Fut,
    Fut: Future<Output = Result<PeerDeltaPullResult, String>>,
{
    let cancellation = super::android_foreground::acquire_foreground_lane(
        &foreground,
        AndroidForegroundLane::P4Target,
    )
    .await
    .map_err(|error| registered_local_operation_failure("registered delta foreground", error))?;
    state
        .mark_target_running_exact(&foreground)
        .map_err(|error| {
            registered_local_operation_failure("registered delta foreground state", error)
        })?;
    let outcome = bound_registered_operation_outcome(operation(cancellation).await);
    state
        .publish_target_terminal_exact(&foreground, outcome.clone())
        .map_err(|error| {
            registered_local_operation_failure("registered delta terminal state", error)
        })?;
    outcome
}

/// A local failure carries no peer classification of its own, so it always
/// leaves the boundary as the generic code.
fn registered_local_operation_failure(context: &str, error: impl std::fmt::Display) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    PeerCommandCode::OperationFailed.code().to_owned()
}

fn peer_operation_failure(context: &str, error: PeerSyncError) -> String {
    crate::nlog!("warn", "{context} failed: {error}");
    code_for(&error).code().to_owned()
}

#[cfg(any(target_os = "android", test))]
fn bound_registered_operation_outcome<T>(outcome: Result<T, String>) -> Result<T, String> {
    outcome.map_err(|error| {
        if is_bounded_code(&error) {
            error
        } else {
            registered_local_operation_failure("registered delta operation", error)
        }
    })
}

fn fetch_delta_manifest_and_completion(
    client: &LanLogicalDeltaClient,
) -> Result<(Vec<u8>, DeltaCompletionAttempt), PeerSyncError> {
    let observed = client.hello()?;
    if observed.device_id != client.source_device_id() || !observed.permissions.allows_read() {
        return Err(PeerSyncError::Protocol(
            "registered delta source identity or permission changed".to_owned(),
        ));
    }
    let source_bearer = client.registered_source_bearer();
    let manifest = client.fetch_manifest_with_completion_lease(None)?;
    let Some(lease) = manifest.completion_lease_id else {
        return Err(PeerSyncError::Protocol(
            "registered delta completion capability changed during manifest fetch".to_owned(),
        ));
    };
    let attempt = DeltaCompletionAttempt {
        operation_id: lease.as_str().to_owned(),
        source_bearer: source_bearer.to_owned(),
    };
    Ok((manifest.bytes, attempt))
}

fn recovered_delta_result(completed: RecoveredDeltaCompletion) -> PeerDeltaPullResult {
    if completed.context.post_revision > completed.context.pre_revision {
        PeerDeltaPullResult::Updated {
            revision: completed.context.post_revision,
            transferred_objects: completed.context.transferred_objects,
            transferred_bytes: completed.useful_bytes,
        }
    } else {
        PeerDeltaPullResult::NoChanges {
            revision: completed.context.post_revision,
            transferred_objects: completed.context.transferred_objects,
            transferred_bytes: completed.useful_bytes,
        }
    }
}

fn is_successful_delta_result(result: &PeerDeltaPullResult) -> bool {
    matches!(
        result,
        PeerDeltaPullResult::NoChanges { .. } | PeerDeltaPullResult::Updated { .. }
    )
}

async fn peer_delta_pull_with_client_factory<
    C: CancellationProbe + Send + 'static,
    F: FnOnce(&Path) -> Result<LanLogicalDeltaClient, PeerSyncError> + Send + 'static,
>(
    app: AppHandle,
    state: PeerDeltaCommandState,
    expected_revision: i64,
    cancellation: C,
    client_factory: F,
) -> Result<PeerDeltaPullResult, String> {
    let app_root = app_root(&app)
        .map_err(|error| registered_local_operation_failure("peer delta app root", error))?;
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = state.begin_pull().map_err(|error| {
            registered_local_operation_failure("peer delta target state", error)
        })?;
        let mut store = persistent_store::commands::with_store_mut(app.state(), |store| {
            open_peer_delta_store(store)
        })
        .map_err(|error| registered_local_operation_failure("peer delta store", error))?;
        let mut completion_transport = LanDeltaCompletionTransport;
        let recovered = recover_delta_completion(&mut store, &app_root, &mut completion_transport)
            .map_err(|error| peer_operation_failure("peer delta completion recovery", error))?;
        reclaim_abandoned_durable_cas_jobs(
            &app_root,
            P4_DELTA_TARGET_JOB_PREFIX,
            CasJobKind::LogicalDeltaTarget,
        )
        .map_err(|error| registered_local_operation_failure("peer delta recovery", error))?;
        if let Some(completed) = recovered {
            return Ok(recovered_delta_result(completed));
        }
        let mut client = client_factory(&app_root)
            .map_err(|error| peer_operation_failure("peer delta client", error))?;
        let (manifest, completion) = fetch_delta_manifest_and_completion(&client)
            .map_err(|error| peer_operation_failure("peer delta manifest", error))?;
        let source_device_id = client.source_device_id().to_owned();
        let cas = PayloadCas::new(&app_root)
            .map_err(|error| registered_local_operation_failure("peer delta CAS", error))?;
        let pulled = pull_logical_delta(
            &mut store,
            &cas,
            &app_root,
            &source_device_id,
            expected_revision,
            &manifest,
            &mut client,
            &cancellation,
            &completion,
        );
        let recovered = recover_delta_completion(&mut store, &app_root, &mut completion_transport)
            .map_err(|error| peer_operation_failure("peer delta completion", error))?;
        reclaim_abandoned_durable_cas_jobs(
            &app_root,
            P4_DELTA_TARGET_JOB_PREFIX,
            CasJobKind::LogicalDeltaTarget,
        )
        .map_err(|error| registered_local_operation_failure("peer delta recovery", error))?;
        if let Some(completed) = recovered {
            return Ok(recovered_delta_result(completed));
        }
        match pulled {
            Ok(result) if is_successful_delta_result(&result) => {
                Err(registered_local_operation_failure(
                    "registered delta completion",
                    "durable completion journal is missing",
                ))
            }
            Ok(result) => Ok(result),
            Err(error) => Err(peer_operation_failure("peer delta pull", error)),
        }
    })
    .await
    .map_err(|error| registered_local_operation_failure("peer delta pull worker", error))?
}

fn app_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("failed to resolve application data directory: {error}"))
}

/// The same directory for the commands that finish through a bounded code, so
/// the resolver failure stays a `PeerSyncError` all the way to the boundary.
fn delta_app_data_root(app: &AppHandle) -> Result<PathBuf, PeerSyncError> {
    app.path()
        .app_data_dir()
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
}

pub(crate) fn canonical_source_device_id(app_root: &Path) -> Result<String, PeerSyncError> {
    super::device_registry::load_or_create_device_id(app_root)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
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
    use super::super::LanCloneHost;
    use super::*;
    use crate::{
        asset_repository::job_pins::{collect_durable_cas_job_roots, CasObjectRole},
        local_backup::AtomicCancellation,
        peer_sync::{
            delta_completion::{
                fail_next_delta_completion_store_after_replace, recover_delta_completion,
                DeltaCompletionContext, DeltaCompletionTransport, PeerDeltaCompletionJournal,
                PeerDeltaDurableCompletion,
            },
            device_registry::{
                incoming_source_by_id, DevicePermissions, IncomingSource, IncomingSourceRegistry,
            },
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
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc, Arc,
        },
        thread,
        time::Duration,
    };

    struct CancelAfterFirstReadSource {
        objects: BTreeMap<String, Vec<u8>>,
        cancelled: Arc<AtomicBool>,
        reads: usize,
    }

    struct NoCompletionTransport;

    impl DeltaCompletionTransport for NoCompletionTransport {
        fn prepare(
            &mut self,
            _source: &IncomingSource,
            _context: &DeltaCompletionContext,
        ) -> Result<u64, PeerSyncError> {
            panic!("an uncommitted completion must not prepare the source")
        }

        fn deliver(
            &mut self,
            _source: &IncomingSource,
            _delivery: &super::super::device_registry::PendingCompletionDelivery,
        ) -> Result<(), PeerSyncError> {
            panic!("an uncommitted completion must not deliver to the source")
        }
    }

    /// Stands in for the source a committed delta settles against. The engine
    /// tests never speak HTTP, so the prepared byte count mirrors what the
    /// transfer actually moved.
    #[derive(Default)]
    struct LocalCompletionTransport;

    impl DeltaCompletionTransport for LocalCompletionTransport {
        fn prepare(
            &mut self,
            _source: &IncomingSource,
            context: &DeltaCompletionContext,
        ) -> Result<u64, PeerSyncError> {
            Ok(context.transferred_bytes)
        }

        fn deliver(
            &mut self,
            _source: &IncomingSource,
            _delivery: &super::super::device_registry::PendingCompletionDelivery,
        ) -> Result<(), PeerSyncError> {
            Ok(())
        }
    }

    struct CancellingLanSource<'a> {
        client: &'a mut LanLogicalDeltaClient,
        cancelled: Arc<AtomicBool>,
    }

    impl LogicalDeltaObjectSource for CancellingLanSource<'_> {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            Ok(Box::new(CancelWhenCompleteReader {
                inner: self.client.open_object(object)?,
                remaining: object.size,
                cancelled: Arc::clone(&self.cancelled),
            }))
        }
    }

    struct CancelWhenCompleteReader {
        inner: Box<dyn Read>,
        remaining: u64,
        cancelled: Arc<AtomicBool>,
    }

    impl Read for CancelWhenCompleteReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(output)?;
            self.remaining = self.remaining.saturating_sub(read as u64);
            if self.remaining == 0 {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    impl LogicalDeltaObjectSource for CancelAfterFirstReadSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.reads += 1;
            let bytes = self.objects.get(&object.hash).cloned().unwrap();
            Ok(Box::new(CancelAfterFirstReadCursor {
                inner: Cursor::new(bytes),
                cancelled: Arc::clone(&self.cancelled),
                first: true,
            }))
        }
    }

    struct CancelAfterFirstReadCursor {
        inner: Cursor<Vec<u8>>,
        cancelled: Arc<AtomicBool>,
        first: bool,
    }

    const ACCOUNTING_SOURCE_ID: &str = "00000000-0000-4000-8000-000000000024";

    fn register_accounting_source(root: &Path, total_bytes: u64, last_seen_ms: u64) {
        let mut registry = IncomingSourceRegistry::load(root).unwrap();
        registry
            .upsert(IncomingSource {
                device_id: ACCOUNTING_SOURCE_ID.to_owned(),
                name: "accounting source".to_owned(),
                endpoint: "http://192.168.0.24:32145".to_owned(),
                bearer: "a".repeat(64),
                permissions: DevicePermissions::read(),
                last_seen_ms,
                total_bytes,
            })
            .unwrap();
        registry.save().unwrap();
    }

    fn accounting_source(root: &Path) -> IncomingSource {
        IncomingSourceRegistry::load(root).unwrap().sources()[0].clone()
    }

    /// The completion attempt a pull carries, against the registered incoming
    /// source the activation intent revalidates. An engine test that never
    /// claims a v2 source registers one here instead, exactly as the claim
    /// would have left it.
    fn registered_delta_attempt(root: &Path, source_device_id: &str) -> DeltaCompletionAttempt {
        let registered = match incoming_source_by_id(root, source_device_id).unwrap() {
            Some(source) => source,
            None => {
                let mut registry = IncomingSourceRegistry::load(root).unwrap();
                registry
                    .upsert(IncomingSource {
                        device_id: source_device_id.to_owned(),
                        name: "delta source".to_owned(),
                        endpoint: "http://192.168.0.24:32145".to_owned(),
                        bearer: "b".repeat(64),
                        permissions: DevicePermissions::read(),
                        last_seen_ms: 0,
                        total_bytes: 0,
                    })
                    .unwrap();
                registry.save().unwrap();
                incoming_source_by_id(root, source_device_id)
                    .unwrap()
                    .unwrap()
            }
        };
        DeltaCompletionAttempt {
            operation_id: uuid::Uuid::new_v4().to_string(),
            source_bearer: registered.bearer,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pull_registered_delta<S: LogicalDeltaObjectSource + ?Sized>(
        store: &mut PersistentStore,
        cas: &PayloadCas,
        app_root: &Path,
        source_device_id: &str,
        expected_revision: i64,
        remote_manifest_bytes: &[u8],
        remote_source: &mut S,
    ) -> Result<PeerDeltaPullResult, PeerSyncError> {
        pull_registered_delta_with_cancellation(
            store,
            cas,
            app_root,
            source_device_id,
            expected_revision,
            remote_manifest_bytes,
            remote_source,
            &NeverCancelled,
        )
    }

    /// Drives the engine the way the registered command does: a pull under a
    /// completion attempt, then the recovery pass that settles the journal
    /// entry it left behind. A pull that reports success without one is the
    /// failure the command itself reports.
    #[allow(clippy::too_many_arguments)]
    fn pull_registered_delta_with_cancellation<S: LogicalDeltaObjectSource + ?Sized>(
        store: &mut PersistentStore,
        cas: &PayloadCas,
        app_root: &Path,
        source_device_id: &str,
        expected_revision: i64,
        remote_manifest_bytes: &[u8],
        remote_source: &mut S,
        cancellation: &dyn CancellationProbe,
    ) -> Result<PeerDeltaPullResult, PeerSyncError> {
        let attempt = registered_delta_attempt(app_root, source_device_id);
        let pulled = pull_logical_delta(
            store,
            cas,
            app_root,
            source_device_id,
            expected_revision,
            remote_manifest_bytes,
            remote_source,
            cancellation,
            &attempt,
        );
        let completed = recover_delta_completion(store, app_root, &mut LocalCompletionTransport)
            .unwrap()
            .is_some();
        assert_eq!(
            completed,
            pulled.as_ref().is_ok_and(is_successful_delta_result),
            "a successful pull settles exactly one completion journal entry"
        );
        assert!(PeerDeltaCompletionJournal::new(app_root)
            .load()
            .unwrap()
            .is_none());
        pulled
    }

    impl Read for CancelAfterFirstReadCursor {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(output)?;
            if self.first && read > 0 {
                self.first = false;
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    /// Hosts a real P4 source through the same preparation engine functions the
    /// shared device sync session uses, now that the per-lane source runtime is
    /// gone.
    fn host_delta_source(root: &Path) -> (LanCloneHost, super::super::LanPairing) {
        let cas = PayloadCas::new(root).unwrap();
        let mut store = PersistentStore::open(root).unwrap();
        let mut prepared = prepare_shared_delta_source(&mut store, &cas, root).unwrap();
        let mut host = LanCloneHost::prepare_logical(prepared.take_session().unwrap());
        host.enable_v2_registry(root, "Windows", DevicePermissions::read())
            .unwrap();
        let pairing = host.start().unwrap();
        (host, pairing)
    }

    #[test]
    fn delta_prepare_stop_prepare_keeps_the_canonical_v2_source_identity() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let canonical = canonical_source_device_id(source_root.path()).unwrap();

        for _ in 0..2 {
            assert_eq!(
                canonical_source_device_id(source_root.path()).unwrap(),
                canonical
            );
            let (mut host, pairing) = host_delta_source(source_root.path());
            let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
            let client = LanLogicalDeltaClient::claim_v2_and_register(
                target_root.path(),
                "Android",
                &endpoint,
                &pairing.session_id,
                &pairing.manifest_id,
                &pairing.claim,
            )
            .unwrap();

            // A restarted source keeps the persisted identity on the wire, so a
            // registered target never sees it as a different peer.
            assert_eq!(client.hello().unwrap().device_id, canonical);
            host.stop().unwrap();
        }
    }

    #[test]
    fn android_target_foreground_release_is_native_owned_and_generation_exact() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        let mut stale = foreground.clone();
        stale.generation -= 1;
        assert_eq!(
            state
                .mark_target_running_exact(&stale)
                .unwrap_err()
                .to_string(),
            r#"Protocol("Android peer delta target foreground identity is stale")"#,
        );
        assert_eq!(
            state.target_foreground_status().unwrap().unwrap().phase,
            AndroidTargetForegroundPhase::Reserved,
        );
        state.mark_target_running_exact(&foreground).unwrap();
        assert!(!registry().detach_if_generation(&foreground));
        assert!(state.cancel_target_foreground_exact(&foreground).unwrap());
        assert!(!state.release_target_foreground_exact(&foreground).unwrap());
        state
            .publish_target_terminal_exact(
                &foreground,
                Ok(PeerDeltaPullResult::NoChanges {
                    revision: 7,
                    transferred_objects: 0,
                    transferred_bytes: 0,
                }),
            )
            .unwrap();
        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert_eq!(terminal.phase, AndroidTargetForegroundPhase::Terminal);
        assert!(matches!(
            terminal.result,
            Some(PeerDeltaPullResult::NoChanges { revision: 7, .. })
        ));
        assert!(!state.release_target_foreground_exact(&stale).unwrap());
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
        assert!(state.target_foreground_status().unwrap().is_none());

        let fresh = state.reserve_target_foreground().unwrap();
        assert!(fresh.generation > foreground.generation);
        assert!(!registry().detach_if_generation(&foreground));
        assert!(state.release_target_foreground_exact(&fresh).unwrap());
    }

    #[test]
    fn registered_delta_composes_exact_foreground_cancellation_and_terminal_publication() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        let mut stale = foreground.clone();
        stale.generation += 1;
        let operation_foreground = foreground.clone();

        let outcome = tauri::async_runtime::block_on(run_registered_delta_target_operation(
            state.clone(),
            foreground.clone(),
            move |cancellation| async move {
                assert!(!registry().cancel_exact(&stale));
                assert!(!cancellation.is_cancelled());
                assert!(registry().cancel_exact(&operation_foreground));
                assert!(cancellation.is_cancelled());
                Err("registered delta cancelled".to_owned())
            },
        ));

        assert_eq!(outcome.unwrap_err(), "operationFailed");
        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert_eq!(terminal.foreground, foreground);
        assert_eq!(terminal.phase, AndroidTargetForegroundPhase::Terminal);
        assert_eq!(terminal.result, None);
        assert_eq!(terminal.error.as_deref(), Some("operationFailed"));
        assert!(state
            .release_target_foreground_exact(&terminal.foreground)
            .unwrap());
    }

    #[test]
    fn registered_delta_bounds_transport_and_local_operation_errors() {
        assert_eq!(
            peer_operation_failure(
                "registered delta transport",
                PeerSyncError::Transport("http://192.168.1.7/session/secret".to_owned()),
            ),
            "transportUnavailable"
        );
        assert_eq!(
            peer_operation_failure(
                "registered delta local",
                PeerSyncError::Storage("C:\\private\\store".to_owned()),
            ),
            "operationFailed"
        );
        assert_eq!(
            registered_local_operation_failure("peer delta pull worker", "cancelled worker"),
            "operationFailed"
        );
    }

    #[test]
    fn re_bounding_a_delta_outcome_keeps_a_code_the_lane_already_produced() {
        for code in ["sourceInUse", "deltaCompletionRetained", "peerOutdated"] {
            assert_eq!(
                bound_registered_operation_outcome::<()>(Err(code.to_owned())).unwrap_err(),
                code
            );
        }
        assert_eq!(
            bound_registered_operation_outcome::<()>(Err(
                "C:\\private\\store and bearer secret".to_owned()
            ))
            .unwrap_err(),
            "operationFailed"
        );
        assert_eq!(
            bound_registered_operation_outcome(Ok::<_, String>(4)),
            Ok(4)
        );
    }

    #[test]
    fn committed_target_stays_running_during_terminal_publication_pause() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        state.mark_target_running_exact(&foreground).unwrap();
        let (committed_tx, committed_rx) = mpsc::channel();
        let (publish_tx, publish_rx) = mpsc::channel();
        let operation_state = state.clone();
        let operation_foreground = foreground.clone();

        let operation = thread::spawn(move || {
            let committed = PeerDeltaPullResult::Updated {
                revision: 12,
                transferred_objects: 1,
                transferred_bytes: 32,
            };
            committed_tx.send(()).unwrap();
            publish_rx.recv().unwrap();
            operation_state
                .publish_target_terminal_exact(&operation_foreground, Ok(committed))
                .unwrap();
        });

        committed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            state.target_foreground_status().unwrap().unwrap().phase,
            AndroidTargetForegroundPhase::Running,
        );
        assert!(state.cancel_target_foreground_exact(&foreground).unwrap());
        assert!(!state.release_target_foreground_exact(&foreground).unwrap());
        assert!(!registry().detach_if_generation(&foreground));
        publish_tx.send(()).unwrap();
        operation.join().unwrap();
        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert!(matches!(
            terminal.result,
            Some(PeerDeltaPullResult::Updated { revision: 12, .. })
        ));
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
    }

    #[test]
    fn target_precommit_failure_is_terminal_and_releases_without_a_result() {
        let _registry_guard = test_registry_guard();
        let state = PeerDeltaCommandState::default();
        let foreground = state.reserve_target_foreground().unwrap();
        assert!(registry().attach_exact(&foreground));
        state.mark_target_running_exact(&foreground).unwrap();
        state
            .publish_target_terminal_exact(
                &foreground,
                Err("cancelled before activation".to_owned()),
            )
            .unwrap();

        let terminal = state.target_foreground_status().unwrap().unwrap();
        assert_eq!(terminal.phase, AndroidTargetForegroundPhase::Terminal);
        assert_eq!(terminal.result, None);
        assert_eq!(
            terminal.error.as_deref(),
            Some("cancelled before activation")
        );
        assert!(state.release_target_foreground_exact(&foreground).unwrap());
    }

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

        let first = canonical_source_device_id(root.path()).unwrap();
        let second = canonical_source_device_id(root.path()).unwrap();

        assert_eq!(first, second);
        assert_eq!(uuid::Uuid::parse_str(&first).unwrap().to_string(), first);
        assert_eq!(
            fs::read_to_string(root.path().join("peer-sync").join("device-id")).unwrap(),
            first
        );
        assert!(!root.path().join("peer-delta").exists());
    }

    #[test]
    fn invalid_persisted_source_device_identity_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("peer-sync").join("device-id");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not-a-device-id\n").unwrap();

        assert!(matches!(
            canonical_source_device_id(root.path()),
            Err(PeerSyncError::Storage(_))
        ));
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
                false,
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

    #[test]
    fn sealed_target_error_releases_roots_after_a_successful_durable_abort() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000006",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"aborted target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        let error = finish_pull(
            &job,
            &activation_conflict_plan(),
            Err(PeerSyncError::Storage("activation failed".to_owned())),
            0,
            0,
            true,
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("activation failed"));
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn sealed_target_error_preserves_roots_when_durable_abort_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000007",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        let prepared = job
            .borrow_mut()
            .prepare_bytes(&cas, b"retained target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        assert!(finish_pull(
            &job,
            &activation_conflict_plan(),
            Err(PeerSyncError::Storage(
                "activation and abort failed".to_owned()
            )),
            0,
            0,
            false,
            false,
        )
        .is_err());

        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&prepared.content_hash));
    }

    #[test]
    fn activation_intent_revalidates_the_registered_source_credential() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let attempt = DeltaCompletionAttempt {
            operation_id: "00000000-0000-4000-8000-000000000027".to_owned(),
            source_bearer: "b".repeat(64),
        };
        let context = delta_completion_context(
            &attempt,
            ACCOUNTING_SOURCE_ID,
            &"b".repeat(64),
            0,
            None,
            1,
            SyncGenerationIdentity {
                generation_id: "remote".to_owned(),
                manifest_hash: "b".repeat(64),
                generation_sequence: "1".to_owned(),
            },
            1,
            17,
        );
        let intent_written = Cell::new(false);
        let before = fs::read(directory.path().join("peer-sync/sources.json")).unwrap();

        let error = publish_delta_activation_intent(
            directory.path(),
            &context,
            &attempt.source_bearer,
            &intent_written,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("credential or permission changed"));
        assert!(intent_written.get());
        assert_eq!(
            fs::read(directory.path().join("peer-sync/sources.json")).unwrap(),
            before
        );
        assert!(PeerDeltaCompletionJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
    }

    #[test]
    fn generic_bootstrap_error_releases_its_sealed_durable_roots() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let job = RefCell::new(
            DurableCasJob::begin(
                directory.path(),
                "p4-delta-target-00000000-0000-4000-8000-000000000008",
                CasJobKind::LogicalDeltaTarget,
                0,
            )
            .unwrap(),
        );
        job.borrow_mut()
            .prepare_bytes(&cas, b"bootstrap target", CasObjectRole::DirectObject)
            .unwrap();
        job.borrow_mut().seal(&mut store, 0).unwrap();

        assert!(finish_bootstrap(
            &job,
            0,
            Err(PeerSyncError::Storage("bootstrap failed".to_owned())),
            false,
        )
        .is_err());

        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
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
            pull_registered_delta(
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
            pull_registered_delta(
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
            pull_registered_delta(
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
            pull_registered_delta(
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
            pull_registered_delta(
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
    fn registered_conflict_and_full_clone_do_not_create_completion_or_count_bytes() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let mut source = empty_source();
        let conflict = DeltaCompletionAttempt {
            operation_id: "00000000-0000-4000-8000-000000000023".to_owned(),
            source_bearer: "a".repeat(64),
        };

        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                ACCOUNTING_SOURCE_ID,
                1,
                &local.manifest_bytes,
                &mut source,
                &NeverCancelled,
                &conflict,
            )
            .unwrap(),
            PeerDeltaPullResult::Conflict {
                reason: "staleRevision",
            }
        );

        let divergent =
            remote_root_manifest(&local.manifest, "remote-other", json!({"side":"remote"}));
        let full_clone = DeltaCompletionAttempt {
            operation_id: "00000000-0000-4000-8000-000000000024".to_owned(),
            source_bearer: "a".repeat(64),
        };
        assert_eq!(
            pull_logical_delta(
                &mut store,
                &cas,
                directory.path(),
                ACCOUNTING_SOURCE_ID,
                0,
                &divergent.manifest_bytes,
                &mut source,
                &NeverCancelled,
                &full_clone,
            )
            .unwrap(),
            PeerDeltaPullResult::FullCloneRequired {
                reason: "noExactCommonBase",
            }
        );

        assert_eq!(accounting_source(directory.path()).total_bytes, 40);
        assert!(PeerDeltaCompletionJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        for attempt in [conflict, full_clone] {
            assert!(DurableCasJob::open(
                directory.path(),
                &format!("{P4_DELTA_TARGET_JOB_PREFIX}{}", attempt.operation_id),
            )
            .is_err());
        }
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
        pull_registered_delta(
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
            pull_registered_delta(
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
    fn registered_activation_releases_the_job_before_durable_accounting_recovery() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        pull_registered_delta(
            &mut store,
            &cas,
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote = remote_root_manifest(
            &local.manifest,
            "registered-remote",
            json!({"side":"remote"}),
        );
        let mut source = FixtureSource {
            objects: remote
                .record_objects
                .iter()
                .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
                .collect(),
            reads: 0,
        };
        let attempt = DeltaCompletionAttempt {
            operation_id: "00000000-0000-4000-8000-000000000025".to_owned(),
            source_bearer: "a".repeat(64),
        };

        let result = pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            0,
            &remote.manifest_bytes,
            &mut source,
            &NeverCancelled,
            &attempt,
        )
        .unwrap();

        assert!(matches!(
            result,
            PeerDeltaPullResult::Updated { revision: 1, .. }
        ));
        assert_eq!(accounting_source(directory.path()).total_bytes, 40);
        let retained = PeerDeltaCompletionJournal::new(directory.path())
            .load()
            .unwrap()
            .unwrap();
        assert!(matches!(
            retained,
            PeerDeltaDurableCompletion::ActivationIntent { .. }
        ));
        assert!(DurableCasJob::open(
            directory.path(),
            &format!("{P4_DELTA_TARGET_JOB_PREFIX}{}", attempt.operation_id),
        )
        .is_err());

        let completed =
            recover_delta_completion(&mut store, directory.path(), &mut LocalCompletionTransport)
                .unwrap()
                .unwrap();
        assert_eq!(completed.useful_bytes, remote.record_objects[0].object.size);
        assert_eq!(
            accounting_source(directory.path()).total_bytes,
            40 + remote.record_objects[0].object.size
        );
        assert!(PeerDeltaCompletionJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
    }

    #[test]
    fn visible_journal_publication_error_retains_the_sealed_target_job() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        pull_registered_delta(
            &mut store,
            &cas,
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote = remote_root_manifest(
            &local.manifest,
            "registered-publication-failure",
            json!({"side":"remote"}),
        );
        let mut source = FixtureSource {
            objects: remote
                .record_objects
                .iter()
                .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
                .collect(),
            reads: 0,
        };
        let attempt = DeltaCompletionAttempt {
            operation_id: "00000000-0000-4000-8000-000000000026".to_owned(),
            source_bearer: "a".repeat(64),
        };
        fail_next_delta_completion_store_after_replace(directory.path());

        let error = pull_logical_delta(
            &mut store,
            &cas,
            directory.path(),
            ACCOUNTING_SOURCE_ID,
            0,
            &remote.manifest_bytes,
            &mut source,
            &NeverCancelled,
            &attempt,
        )
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("injected journal publication failure"));
        assert_eq!(store.revision().unwrap(), 0);
        assert_eq!(accounting_source(directory.path()).total_bytes, 40);
        assert!(matches!(
            PeerDeltaCompletionJournal::new(directory.path())
                .load()
                .unwrap(),
            Some(PeerDeltaDurableCompletion::ActivationIntent { .. })
        ));
        let retained = DurableCasJob::open(
            directory.path(),
            &format!("{P4_DELTA_TARGET_JOB_PREFIX}{}", attempt.operation_id),
        )
        .unwrap();
        assert!(retained.is_sealed());
        assert!(!retained.is_released());

        assert!(
            recover_delta_completion(&mut store, directory.path(), &mut NoCompletionTransport,)
                .unwrap()
                .is_none()
        );
        assert!(PeerDeltaCompletionJournal::new(directory.path())
            .load()
            .unwrap()
            .is_none());
        assert!(DurableCasJob::open(
            directory.path(),
            &format!("{P4_DELTA_TARGET_JOB_PREFIX}{}", attempt.operation_id),
        )
        .is_err());
    }

    #[test]
    fn registered_v1_zero_byte_completion_is_durable_on_both_peers_before_success() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id = canonical_source_device_id(source_root.path()).unwrap();
        let (mut host, pairing) = host_delta_source(source_root.path());
        let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
        let mut client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let (remote_manifest, attempt) = fetch_delta_manifest_and_completion(&client).unwrap();
        assert!(
            super::super::device_registry::CompletionLeaseId::parse(&attempt.operation_id).is_ok()
        );

        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let staged = pull_logical_delta(
            &mut target_store,
            &target_cas,
            target_root.path(),
            &source_device_id,
            0,
            &remote_manifest,
            &mut client,
            &NeverCancelled,
            &attempt,
        )
        .unwrap();
        assert_eq!(
            staged,
            PeerDeltaPullResult::NoChanges {
                revision: 0,
                transferred_objects: 0,
                transferred_bytes: 0,
            }
        );
        assert_eq!(accounting_source(target_root.path()).total_bytes, 0);

        let completed = recover_delta_completion(
            &mut target_store,
            target_root.path(),
            &mut LanDeltaCompletionTransport,
        )
        .unwrap()
        .unwrap();
        assert_eq!(completed.useful_bytes, 0);
        assert!(PeerDeltaCompletionJournal::new(target_root.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(accounting_source(target_root.path()).total_bytes, 0);
        let outgoing =
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap();
        assert_eq!(outgoing.devices()[0].total_bytes, 0);

        host.stop().unwrap();
    }

    #[test]
    fn registered_v1_retry_reuses_lease_and_counts_source_proof_once() {
        let source_root = tempfile::tempdir().unwrap();
        let target_root = tempfile::tempdir().unwrap();
        let source_device_id = canonical_source_device_id(source_root.path()).unwrap();

        let mut source_store = PersistentStore::open(source_root.path()).unwrap();
        source_store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side":"base"})),
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
        let source_cas = PayloadCas::new(source_root.path()).unwrap();
        let source_base = source_store
            .seal_or_initialize_active_logical_generation(&source_cas)
            .unwrap();

        let mut target_store = PersistentStore::open(target_root.path()).unwrap();
        target_store
            .commit(&WorkingSetCommit {
                expected_revision: 0,
                root: Some(json!({"side":"base"})),
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
        let target_cas = PayloadCas::new(target_root.path()).unwrap();
        let target_base = target_store
            .seal_or_initialize_active_logical_generation(&target_cas)
            .unwrap();
        crate::persistent_store::establish_logical_common_base(
            &mut target_store,
            &target_cas,
            &source_device_id,
            PRODUCT_LOGICAL_LIBRARY_ID,
            &target_base.manifest.generation,
            1,
            &source_base.manifest_bytes,
        )
        .unwrap();

        source_store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(json!({"side":"remote"})),
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
        drop(source_store);
        let (mut host, pairing) = host_delta_source(source_root.path());
        let endpoint = format!("http://127.0.0.1:{}", host.address().unwrap().port());
        let mut client = LanLogicalDeltaClient::claim_v2_and_register(
            target_root.path(),
            "Android",
            &endpoint,
            &pairing.session_id,
            &pairing.manifest_id,
            &pairing.claim,
        )
        .unwrap();
        let (remote_manifest, first_attempt) =
            fetch_delta_manifest_and_completion(&client).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut cancelling_source = CancellingLanSource {
            client: &mut client,
            cancelled,
        };

        assert_eq!(
            pull_logical_delta(
                &mut target_store,
                &target_cas,
                target_root.path(),
                &source_device_id,
                1,
                &remote_manifest,
                &mut cancelling_source,
                &cancellation,
                &first_attempt,
            )
            .unwrap_err(),
            PeerSyncError::Cancelled
        );
        assert!(PeerDeltaCompletionJournal::new(target_root.path())
            .load()
            .unwrap()
            .is_none());
        assert_eq!(accounting_source(target_root.path()).total_bytes, 0);

        let (retry_manifest, retry_attempt) = fetch_delta_manifest_and_completion(&client).unwrap();
        assert_eq!(retry_attempt.operation_id, first_attempt.operation_id);
        assert!(matches!(
            pull_logical_delta(
                &mut target_store,
                &target_cas,
                target_root.path(),
                &source_device_id,
                1,
                &retry_manifest,
                &mut client,
                &NeverCancelled,
                &retry_attempt,
            )
            .unwrap(),
            PeerDeltaPullResult::Updated { .. }
        ));

        let completed = recover_delta_completion(
            &mut target_store,
            target_root.path(),
            &mut LanDeltaCompletionTransport,
        )
        .unwrap()
        .unwrap();
        assert!(completed.useful_bytes > 0);
        assert_eq!(
            accounting_source(target_root.path()).total_bytes,
            completed.useful_bytes
        );
        let outgoing =
            super::super::device_registry::OutgoingDeviceRegistry::load(source_root.path())
                .unwrap();
        assert_eq!(outgoing.devices()[0].total_bytes, completed.useful_bytes);
        assert!(outgoing
            .has_exact_completion_receipt(
                &outgoing.devices()[0].device_id,
                super::super::device_registry::CompletionLane::Delta,
                &first_attempt.operation_id,
                &pairing.manifest_id,
                completed.useful_bytes,
            )
            .unwrap());
        assert!(recover_delta_completion(
            &mut target_store,
            target_root.path(),
            &mut LanDeltaCompletionTransport,
        )
        .unwrap()
        .is_none());
        assert_eq!(
            accounting_source(target_root.path()).total_bytes,
            completed.useful_bytes
        );

        host.stop().unwrap();
    }

    #[test]
    fn cancelled_target_cleans_only_its_unsealed_job_and_retries_with_a_fresh_job() {
        let directory = tempfile::tempdir().unwrap();
        register_accounting_source(directory.path(), 40, 7);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = store
            .seal_or_initialize_active_logical_generation(&cas)
            .unwrap();
        let peer = "00000000-0000-4000-8000-000000000023";
        pull_registered_delta(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &local.manifest_bytes,
            &mut empty_source(),
        )
        .unwrap();
        let remote =
            remote_root_manifest(&local.manifest, "remote-cancel", json!({"side":"remote"}));
        let unowned = directory
            .path()
            .join("peer-delta")
            .join("staging")
            .join("unowned");
        fs::create_dir_all(&unowned).unwrap();
        fs::write(unowned.join("keep"), b"unowned").unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let objects = remote
            .record_objects
            .iter()
            .map(|record| (record.object.hash.clone(), record.object.bytes.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut source = CancelAfterFirstReadSource {
            objects: objects.clone(),
            cancelled,
            reads: 0,
        };

        let error = pull_registered_delta_with_cancellation(
            &mut store,
            &cas,
            directory.path(),
            peer,
            0,
            &remote.manifest_bytes,
            &mut source,
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(accounting_source(directory.path()).total_bytes, 40);
        assert_eq!(accounting_source(directory.path()).last_seen_ms, 7);
        assert_eq!(source.reads, 1);
        assert_eq!(store.revision().unwrap(), 0);
        assert!(unowned.join("keep").is_file());
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );

        let mut retry = FixtureSource { objects, reads: 0 };
        assert!(matches!(
            pull_registered_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                0,
                &remote.manifest_bytes,
                &mut retry,
            )
            .unwrap(),
            PeerDeltaPullResult::Updated { revision: 1, .. }
        ));
        assert_eq!(retry.reads, 1);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(unowned.join("keep").is_file());

        assert!(matches!(
            pull_registered_delta(
                &mut store,
                &cas,
                directory.path(),
                peer,
                1,
                &remote.manifest_bytes,
                &mut empty_source(),
            )
            .unwrap(),
            PeerDeltaPullResult::NoChanges { revision: 1, .. }
        ));
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
        pull_registered_delta(
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
            pull_registered_delta(
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
        pull_registered_delta(
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
            pull_registered_delta(
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
            pull_registered_delta(
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

    #[test]
    fn a_running_pull_excludes_the_retained_status_and_abandon_commands() {
        let directory = tempfile::tempdir().unwrap();
        let operation_id = "00000000-0000-4000-8000-000000000031";
        let state = PeerDeltaCommandState::default();
        let held = state.begin_pull().unwrap();

        assert!(
            retained_completion_under_pull_guard(&state, directory.path(), || {
                panic!("the store must not be opened while a pull owns the target")
            })
            .is_err()
        );
        assert!(abandon_retained_completion_under_pull_guard(
            &state,
            directory.path(),
            operation_id
        )
        .is_err());

        drop(held);

        assert_eq!(
            retained_completion_under_pull_guard(&state, directory.path(), || {
                PersistentStore::open(directory.path()).map_err(store_error)
            })
            .unwrap(),
            None
        );
        abandon_retained_completion_under_pull_guard(&state, directory.path(), operation_id)
            .unwrap();
    }
}
