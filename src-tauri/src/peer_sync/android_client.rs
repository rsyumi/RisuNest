use super::{
    activate_downloaded_clone,
    lan::{validate_lan_endpoint, PeerCompletionCapability},
    CloneTargetAdapter, CloneValidator, DownloadReport, LanCloneClient, LoopbackCloneClient,
    LosslessCloneTargetAdapter, PeerSyncError, TransferCancellation,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::CancellationProbe,
    persistent_store::{PersistentStore, StoreError},
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex, MutexGuard,
    },
};

#[cfg(test)]
use std::sync::{atomic::AtomicBool, Arc, Barrier};

const JOB_SCHEMA: &str = "risunest.android-peer-clone-job/v1";
const JOB_OWNERSHIP_SCHEMA: &str = "risunest.android-peer-clone-ownership/v1";
const JOB_STATUS_SCHEMA: &str = "risunest.android-peer-clone-status/v1";
const REGISTRY_SCHEMA: &str = "risunest.android-peer-clone-registry/v1";
const VERIFIED_SCHEMA: &str = "risunest.android-peer-clone-verified/v1";
const CANCEL_REQUESTED_SCHEMA: &str = "risunest.android-peer-clone-cancel/v1";
const ACTIVE_OPERATION_KEY: &str = "peerCloneAndroidActiveOperation";
const DELETING_JOB_PREFIX: &str = ".deleting-";
const MAX_JOB_RECORD_BYTES: u64 = 16 * 1024;
const MAX_ERROR_BYTES: usize = 2 * 1024;
const STATUS_PROGRESS_STEP_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum AndroidCloneStopReason {
    Running = 0,
    Pause = 1,
    Cancel = 2,
}

pub(super) struct AndroidCloneStopState(AtomicU8);

impl AndroidCloneStopState {
    pub(super) fn new() -> Self {
        Self(AtomicU8::new(AndroidCloneStopReason::Running as u8))
    }

    pub(super) fn request_pause(&self) {
        self.0
            .fetch_max(AndroidCloneStopReason::Pause as u8, Ordering::SeqCst);
    }

    pub(super) fn request_cancel(&self) {
        self.0
            .store(AndroidCloneStopReason::Cancel as u8, Ordering::SeqCst);
    }

    pub(super) fn current(&self) -> AndroidCloneStopReason {
        match self.0.load(Ordering::SeqCst) {
            value if value == AndroidCloneStopReason::Pause as u8 => AndroidCloneStopReason::Pause,
            value if value == AndroidCloneStopReason::Cancel as u8 => {
                AndroidCloneStopReason::Cancel
            }
            _ => AndroidCloneStopReason::Running,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AndroidCloneJobPhase {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "paused")]
    Paused,
    #[serde(rename = "downloading")]
    Downloading,
    #[serde(rename = "awaitingActivation")]
    VerifiedAwaitingActivation,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "failed")]
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidCloneJobStatus {
    pub(crate) job_id: String,
    pub(crate) endpoint: String,
    pub(crate) session_id: String,
    pub(crate) manifest_id: String,
    pub(crate) phase: AndroidCloneJobPhase,
    pub(crate) completed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) committed_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backup_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobDescriptor {
    schema: String,
    job_id: String,
    manifest_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_capability: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobOwnership {
    schema: String,
    job_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobMarker {
    schema: String,
    manifest_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneActivationWitness {
    job_id: String,
    manifest_id: String,
    revision: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidClonePersistedStatus {
    schema: String,
    phase: AndroidCloneJobPhase,
    completed_bytes: u64,
    total_bytes: Option<u64>,
    error: Option<String>,
    committed_revision: Option<u64>,
    #[serde(default)]
    backup_path: Option<PathBuf>,
    #[serde(default)]
    completion_acknowledged: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    completion_lease_id: Option<String>,
}

impl AndroidClonePersistedStatus {
    fn ready(completion_lease_id: Option<String>) -> Self {
        Self {
            schema: JOB_STATUS_SCHEMA.to_owned(),
            phase: AndroidCloneJobPhase::Ready,
            completed_bytes: 0,
            total_bytes: None,
            error: None,
            committed_revision: None,
            backup_path: None,
            completion_acknowledged: false,
            completion_lease_id,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        let invalid_progress = self
            .total_bytes
            .is_some_and(|total| self.completed_bytes > total);
        let invalid_error = self
            .error
            .as_deref()
            .is_some_and(|error| error.len() > MAX_ERROR_BYTES);
        let invalid_commit = self.committed_revision.is_some()
            && self.phase != AndroidCloneJobPhase::VerifiedAwaitingActivation;
        let invalid_acknowledgement =
            self.completion_acknowledged && self.committed_revision.is_none();
        if self.schema != JOB_STATUS_SCHEMA
            || invalid_progress
            || invalid_error
            || invalid_commit
            || invalid_acknowledgement
            || self
                .completion_lease_id
                .as_deref()
                .is_some_and(|lease_id| !is_canonical_v4_uuid(lease_id))
        {
            return Err(PeerSyncError::Storage(
                "Android clone job status is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

pub(crate) fn current_android_clone_job_references_backup(
    app_root: &Path,
    backup: &Path,
) -> Result<bool, PeerSyncError> {
    let Some(name) = backup.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    let Some(stem) = name
        .strip_prefix("pre-clone-")
        .and_then(|name| name.strip_suffix(".lossless"))
    else {
        return Ok(false);
    };
    if validate_job_id(stem).is_err() {
        return Ok(false);
    }
    let job_id = stem;

    let backup_root = fs::canonicalize(app_root.join("peer-clone-activation/backups"))?;
    let backup = fs::canonicalize(backup)?;
    if backup.parent() != Some(backup_root.as_path()) {
        return Ok(false);
    }

    let jobs_root = app_root.join("peer-clone-jobs");
    let current_path = jobs_root.join("current.json");
    let metadata = match fs::symlink_metadata(&current_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || super::maintenance::link_like(&metadata) {
        return Err(PeerSyncError::Validation(
            "Android clone current job record is not a plain file".to_owned(),
        ));
    }
    let current: AndroidCloneRegistryOwnership =
        read_bounded_json(&current_path, MAX_JOB_RECORD_BYTES)?;
    if current.schema != REGISTRY_SCHEMA {
        return Err(PeerSyncError::Validation(
            "Android clone current job record is invalid".to_owned(),
        ));
    }
    validate_job_id(&current.job_id)?;
    if current.job_id != job_id {
        return Ok(false);
    }

    let (job_root, descriptor) = validate_job(&jobs_root.join(job_id))?;
    if descriptor.job_id != job_id {
        return Err(PeerSyncError::Validation(
            "Android clone current job identity changed".to_owned(),
        ));
    }
    let status: AndroidClonePersistedStatus =
        read_bounded_json(&job_root.join("status.json"), MAX_JOB_RECORD_BYTES)?;
    status.validate()?;
    let Some(receipt) = status.backup_path else {
        return Ok(true);
    };
    if fs::canonicalize(receipt)? != backup {
        return Err(PeerSyncError::Validation(
            "Android clone current job backup receipt changed".to_owned(),
        ));
    }
    Ok(true)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneRegistryOwnership {
    schema: String,
    job_id: String,
}

pub struct AndroidResumableCloneJob {
    root: PathBuf,
    descriptor: AndroidCloneJobDescriptor,
    endpoint: String,
    session_id: String,
    client: LoopbackCloneClient,
}

impl AndroidResumableCloneJob {
    pub(crate) fn from_registered(
        job_root: impl AsRef<Path>,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
        completion_capability: PeerCompletionCapability,
    ) -> Result<Self, PeerSyncError> {
        let job_root = job_root.as_ref();
        let job_id = validate_job_id_from_path(job_root)?;
        fs::create_dir(job_root)?;
        let root = fs::canonicalize(job_root)?;
        let result = (|| {
            write_new_json(
                &root.join("ownership.json"),
                &AndroidCloneJobOwnership {
                    schema: JOB_OWNERSHIP_SCHEMA.to_owned(),
                    job_id: job_id.clone(),
                },
            )?;
            let descriptor = AndroidCloneJobDescriptor {
                schema: JOB_SCHEMA.to_owned(),
                job_id: job_id.clone(),
                manifest_id: manifest_id.to_owned(),
                completion_capability: (completion_capability == PeerCompletionCapability::V1)
                    .then(|| super::lan::PEER_COMPLETION_CAPABILITY_V1.to_owned()),
            };
            write_new_json(&root.join("job.json"), &descriptor)?;
            let lan = LanCloneClient::from_registered_and_persist(
                &root.join("credential.json"),
                endpoint,
                session_id,
                manifest_id,
                target_device_id,
                source_device_id,
                bearer,
            )?;
            let mut client = LoopbackCloneClient::from_lan_with_completion(
                &root,
                lan,
                manifest_id,
                completion_capability,
                None,
            )?;
            write_new_json(
                &root.join("status.json"),
                &AndroidClonePersistedStatus::ready(None),
            )?;
            let completion_lease_id = client.prepare_completion_manifest()?;
            write_json_atomic(
                &root.join("status.json"),
                &AndroidClonePersistedStatus::ready(completion_lease_id),
            )?;
            Ok(Self {
                root: root.clone(),
                descriptor,
                endpoint: endpoint.to_owned(),
                session_id: session_id.to_owned(),
                client,
            })
        })();
        if result.is_err() {
            let _ = cleanup_unpublished_job(&root, &job_id);
        }
        result
    }

    pub fn open(job_root: impl AsRef<Path>) -> Result<Self, PeerSyncError> {
        let (root, descriptor) = validate_job(job_root.as_ref())?;
        validate_marker_if_present(
            &root.join("verified.json"),
            VERIFIED_SCHEMA,
            &descriptor.manifest_id,
        )?;
        validate_marker_if_present(
            &root.join("cancel.requested"),
            CANCEL_REQUESTED_SCHEMA,
            &descriptor.manifest_id,
        )?;
        let lan = LanCloneClient::open_persisted(&root.join("credential.json"))?;
        let (endpoint, session_id, persisted_manifest_id) = lan.target_identity()?;
        if persisted_manifest_id != descriptor.manifest_id {
            return Err(PeerSyncError::Storage(
                "Android clone job target identity is inconsistent".to_owned(),
            ));
        }
        let endpoint = endpoint.to_owned();
        let session_id = session_id.to_owned();
        let status_path = root.join("status.json");
        let persisted_status = if status_path.exists() {
            Some(read_bounded_json::<AndroidClonePersistedStatus>(
                &status_path,
                MAX_JOB_RECORD_BYTES,
            )?)
        } else {
            None
        };
        let completion_capability = descriptor_completion_capability(&descriptor)?;
        let resume_lease_id = persisted_status
            .as_ref()
            .and_then(|status| status.completion_lease_id.as_deref());
        let client = LoopbackCloneClient::from_lan_with_completion(
            &root,
            lan,
            &descriptor.manifest_id,
            completion_capability,
            resume_lease_id,
        )?;
        let mut job = Self {
            root,
            descriptor,
            endpoint,
            session_id,
            client,
        };
        if persisted_status.is_none() {
            let completion_lease_id = job.client.prepare_completion_manifest()?;
            let mut status = AndroidClonePersistedStatus::ready(completion_lease_id);
            if job.root.join("verified.json").is_file() {
                let (completed_bytes, total_bytes) = job.client.transfer_progress()?;
                if completed_bytes != total_bytes {
                    return Err(PeerSyncError::Storage(
                        "Android verified clone transfer ledger is incomplete".to_owned(),
                    ));
                }
                status.phase = AndroidCloneJobPhase::VerifiedAwaitingActivation;
                status.completed_bytes = completed_bytes;
                status.total_bytes = Some(total_bytes);
            }
            write_new_json(&job.root.join("status.json"), &status)?;
        } else {
            let status = job.read_status()?;
            validate_android_completion_state(&job.descriptor, &status)?;
        }
        Ok(job)
    }

    pub fn phase(&self) -> Result<AndroidCloneJobPhase, PeerSyncError> {
        Ok(self.reconciled_status()?.phase)
    }

    pub(crate) fn status(&self) -> Result<AndroidCloneJobStatus, PeerSyncError> {
        let status = self.reconciled_status()?;
        Ok(AndroidCloneJobStatus {
            job_id: self.descriptor.job_id.clone(),
            endpoint: self.endpoint.clone(),
            session_id: self.session_id.clone(),
            manifest_id: self.descriptor.manifest_id.clone(),
            phase: status.phase,
            completed_bytes: status.completed_bytes,
            total_bytes: status.total_bytes,
            error: status.error,
            committed_revision: status.committed_revision,
            backup_path: status.backup_path,
        })
    }

    // Production Android transfers run through the foreground JNI path; this
    // progress-free wrapper serves the desktop test suite.
    #[cfg(test)]
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
        if self.cancel_requested()? || cancellation.is_cancelled() {
            return Err(PeerSyncError::Cancelled);
        }
        let initial_status = self.read_status()?;
        if initial_status.committed_revision.is_some() {
            return Err(PeerSyncError::AlreadyActivated);
        }
        let completion_lease_id = initial_status.completion_lease_id;

        let all_objects_verified = match self.client.all_objects_verified(cancellation) {
            Ok(verified) => verified,
            Err(error) => return self.finish_download_error(error),
        };
        let (completed_before, total_bytes) = match self.client.transfer_progress() {
            Ok(progress) => progress,
            Err(error) => return self.finish_download_error(error),
        };
        if all_objects_verified {
            if let Err(error) = self.client.report_terminal_progress() {
                return self.finish_download_error(error);
            }
            self.ensure_verified_marker()?;
            self.write_status(&AndroidClonePersistedStatus {
                schema: JOB_STATUS_SCHEMA.to_owned(),
                phase: AndroidCloneJobPhase::VerifiedAwaitingActivation,
                completed_bytes: total_bytes,
                total_bytes: Some(total_bytes),
                error: None,
                committed_revision: None,
                backup_path: None,
                completion_acknowledged: false,
                completion_lease_id: completion_lease_id.clone(),
            })?;
            return Ok(DownloadReport::default());
        }

        self.write_status(&AndroidClonePersistedStatus {
            schema: JOB_STATUS_SCHEMA.to_owned(),
            phase: AndroidCloneJobPhase::Downloading,
            completed_bytes: completed_before,
            total_bytes: Some(total_bytes),
            error: None,
            committed_revision: None,
            backup_path: None,
            completion_acknowledged: false,
            completion_lease_id: completion_lease_id.clone(),
        })?;
        let status_error = RefCell::new(None);
        let last_persisted = Cell::new(completed_before);
        let progress_cancellation = cancellation.clone();
        let status_path = self.root.join("status.json");
        let result = self
            .client
            .download_with_progress(cancellation, |transferred| {
                if status_error.borrow().is_none() {
                    let completed_bytes = completed_before
                        .saturating_add(transferred)
                        .min(total_bytes);
                    if completed_bytes != total_bytes
                        && completed_bytes.saturating_sub(last_persisted.get())
                            < STATUS_PROGRESS_STEP_BYTES
                    {
                        progress(transferred);
                        return;
                    }
                    let status = AndroidClonePersistedStatus {
                        schema: JOB_STATUS_SCHEMA.to_owned(),
                        phase: AndroidCloneJobPhase::Downloading,
                        completed_bytes,
                        total_bytes: Some(total_bytes),
                        error: None,
                        committed_revision: None,
                        backup_path: None,
                        completion_acknowledged: false,
                        completion_lease_id: completion_lease_id.clone(),
                    };
                    if let Err(error) = write_json_atomic(&status_path, &status) {
                        *status_error.borrow_mut() = Some(error);
                        progress_cancellation.cancel();
                    } else {
                        last_persisted.set(completed_bytes);
                    }
                }
                progress(transferred);
            });
        if let Some(error) = status_error.into_inner() {
            if let Err(pause_error) = self.mark_paused() {
                return Err(PeerSyncError::Storage(format!(
                    "Android clone progress status persistence failed: {error}; failed to persist paused Android clone status: {pause_error}"
                )));
            }
            return Err(error);
        }
        match result {
            Ok(report) => {
                self.ensure_verified_marker()?;
                self.write_status(&AndroidClonePersistedStatus {
                    schema: JOB_STATUS_SCHEMA.to_owned(),
                    phase: AndroidCloneJobPhase::VerifiedAwaitingActivation,
                    completed_bytes: total_bytes,
                    total_bytes: Some(total_bytes),
                    error: None,
                    committed_revision: None,
                    backup_path: None,
                    completion_acknowledged: false,
                    completion_lease_id,
                })?;
                Ok(report)
            }
            Err(error) => self.finish_download_error(error),
        }
    }

    pub fn request_cancel(&self) -> Result<(), PeerSyncError> {
        if self.reconciled_status()?.committed_revision.is_some() {
            return Err(PeerSyncError::AlreadyActivated);
        }
        write_cancel_marker(&self.root, &self.descriptor.manifest_id)
    }

    pub(crate) fn request_cancel_for_platform_stop(&self) -> Result<bool, PeerSyncError> {
        if self.cancel_requested()? {
            return Ok(true);
        }
        let status = self.reconciled_status()?;
        if status.phase == AndroidCloneJobPhase::VerifiedAwaitingActivation
            || status.committed_revision.is_some()
        {
            return Ok(false);
        }
        self.request_cancel()?;
        Ok(true)
    }

    pub fn request_cancel_at(job_root: impl AsRef<Path>) -> Result<(), PeerSyncError> {
        Self::open(job_root)?.request_cancel()
    }

    pub(crate) fn mark_paused(&self) -> Result<(), PeerSyncError> {
        let mut status = self.reconciled_status()?;
        if matches!(
            status.phase,
            AndroidCloneJobPhase::VerifiedAwaitingActivation | AndroidCloneJobPhase::Failed
        ) {
            return Ok(());
        }
        status.phase = AndroidCloneJobPhase::Paused;
        status.error = None;
        self.write_status(&status)
    }

    pub(crate) fn mark_failed(&self, error: &PeerSyncError) -> Result<(), PeerSyncError> {
        let mut status = self.reconciled_status()?;
        if status.committed_revision.is_some() {
            return Err(PeerSyncError::AlreadyActivated);
        }
        status.phase = AndroidCloneJobPhase::Failed;
        status.error = Some(truncate_utf8(&error.to_string(), MAX_ERROR_BYTES));
        self.write_status(&status)
    }

    pub(crate) fn into_activation_client(mut self) -> Result<LoopbackCloneClient, PeerSyncError> {
        if self.cancel_requested()? {
            return Err(PeerSyncError::Cancelled);
        }
        let status = self.reconciled_status()?;
        if status.phase != AndroidCloneJobPhase::VerifiedAwaitingActivation
            || !self
                .client
                .all_objects_verified(&TransferCancellation::new())?
        {
            return Err(PeerSyncError::Validation(
                "Android clone job is not verified for activation".to_owned(),
            ));
        }
        Ok(self.client)
    }

    pub(crate) fn mark_committed(
        &self,
        committed_revision: u64,
    ) -> Result<AndroidCloneJobStatus, PeerSyncError> {
        let mut status = self.reconciled_status()?;
        if status.phase != AndroidCloneJobPhase::VerifiedAwaitingActivation {
            return Err(PeerSyncError::Validation(
                "Android clone job is not awaiting activation".to_owned(),
            ));
        }
        if let Some(existing) = status.committed_revision {
            if existing != committed_revision {
                return Err(PeerSyncError::Validation(
                    "Android clone job committed revision changed".to_owned(),
                ));
            }
        } else {
            status.committed_revision = Some(committed_revision);
            status.error = None;
            self.write_status(&status)?;
        }
        self.status()
    }

    fn acknowledge_completion(&self) -> Result<(), PeerSyncError> {
        let mut status = self.reconciled_status()?;
        if status.committed_revision.is_none() {
            return Err(PeerSyncError::Validation(
                "Android clone completion cannot be acknowledged before commit".to_owned(),
            ));
        }
        if !status.completion_acknowledged {
            status.completion_acknowledged = true;
            self.write_status(&status)?;
        }
        Ok(())
    }

    pub(crate) fn record_backup_path(
        &self,
        backup_path: &Path,
    ) -> Result<AndroidCloneJobStatus, PeerSyncError> {
        super::maintenance::with_backup_reference_lifecycle(|| {
            let app_root = self.root.parent().and_then(Path::parent).ok_or_else(|| {
                PeerSyncError::Storage("Android clone job has no app root".to_owned())
            })?;
            if !backup_receipt_owns_activation(
                Some(backup_path),
                &app_root.join("peer-clone-activation"),
                &self.descriptor.job_id,
            )? {
                return Err(PeerSyncError::Storage(
                    "Android clone backup disappeared before receipt publication".to_owned(),
                ));
            }
            let mut status = self.reconciled_status()?;
            match status.backup_path.as_deref() {
                Some(existing) if existing == backup_path => return self.status(),
                Some(_) => {
                    return Err(PeerSyncError::Validation(
                        "Android clone job backup path changed".to_owned(),
                    ))
                }
                None => {}
            }
            status.backup_path = Some(backup_path.to_owned());
            self.write_status(&status)?;
            self.status()
        })
    }

    pub fn discard_at(job_root: impl AsRef<Path>) -> Result<(), PeerSyncError> {
        let job_root = job_root.as_ref();
        if !job_root.exists() {
            return cleanup_deleting_job_for_root(job_root);
        }
        let (root, descriptor) = validate_job(job_root)?;
        cleanup_owned_job(&root, &descriptor.job_id)
    }

    pub fn discard(self) -> Result<(), PeerSyncError> {
        cleanup_owned_job(&self.root, &self.descriptor.job_id)
    }

    pub fn cancel_requested(&self) -> Result<bool, PeerSyncError> {
        let path = self.root.join("cancel.requested");
        if !path.exists() {
            return Ok(false);
        }
        validate_marker_if_present(&path, CANCEL_REQUESTED_SCHEMA, &self.descriptor.manifest_id)?;
        Ok(true)
    }

    fn reconciled_status(&self) -> Result<AndroidClonePersistedStatus, PeerSyncError> {
        let mut status = self.read_status()?;
        if self.root.join("verified.json").is_file()
            && matches!(
                status.phase,
                AndroidCloneJobPhase::Ready
                    | AndroidCloneJobPhase::Paused
                    | AndroidCloneJobPhase::Downloading
            )
            && status.committed_revision.is_none()
        {
            status.phase = AndroidCloneJobPhase::VerifiedAwaitingActivation;
            status.completed_bytes = status.total_bytes.unwrap_or(status.completed_bytes);
            status.error = None;
            self.write_status(&status)?;
        }
        Ok(status)
    }

    fn read_status(&self) -> Result<AndroidClonePersistedStatus, PeerSyncError> {
        let status: AndroidClonePersistedStatus =
            read_bounded_json(&self.root.join("status.json"), MAX_JOB_RECORD_BYTES)?;
        validate_android_completion_state(&self.descriptor, &status)?;
        Ok(status)
    }

    fn write_status(&self, status: &AndroidClonePersistedStatus) -> Result<(), PeerSyncError> {
        validate_android_completion_state(&self.descriptor, status)?;
        write_json_atomic(&self.root.join("status.json"), status)
    }

    fn ensure_verified_marker(&self) -> Result<(), PeerSyncError> {
        if self.root.join("verified.json").exists() {
            return validate_marker_if_present(
                &self.root.join("verified.json"),
                VERIFIED_SCHEMA,
                &self.descriptor.manifest_id,
            );
        }
        write_new_json(
            &self.root.join("verified.json"),
            &AndroidCloneJobMarker {
                schema: VERIFIED_SCHEMA.to_owned(),
                manifest_id: self.descriptor.manifest_id.clone(),
            },
        )
    }

    fn finish_download_error<T>(&self, error: PeerSyncError) -> Result<T, PeerSyncError> {
        match &error {
            PeerSyncError::Cancelled => {}
            PeerSyncError::Transport(_) => self.mark_paused()?,
            _ => self.mark_failed(&error)?,
        }
        Err(error)
    }
}

pub(crate) struct AndroidCloneJobRegistry {
    jobs_root: PathBuf,
    state: Mutex<()>,
    #[cfg(test)]
    leave_activation_stage_after_commit: AtomicBool,
    #[cfg(test)]
    fail_backup_receipt_write: AtomicBool,
    #[cfg(test)]
    registered_publish_barriers: Mutex<Option<(Arc<Barrier>, Arc<Barrier>)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AndroidCloneFinalizeReceipt {
    pub(crate) revision: u64,
    pub(crate) backup_path: Option<PathBuf>,
}

impl AndroidCloneJobRegistry {
    pub(crate) fn initialize(app_data_root: impl AsRef<Path>) -> Result<Self, PeerSyncError> {
        fs::create_dir_all(app_data_root.as_ref())?;
        let app_data_root = fs::canonicalize(app_data_root.as_ref())?;
        let requested_jobs_root = app_data_root.join("peer-clone-jobs");
        fs::create_dir_all(&requested_jobs_root)?;
        let jobs_root = fs::canonicalize(&requested_jobs_root)?;
        if jobs_root.parent() != Some(app_data_root.as_path()) {
            return Err(PeerSyncError::Storage(
                "Android clone jobs root escaped app data".to_owned(),
            ));
        }
        let registry = Self {
            jobs_root,
            state: Mutex::new(()),
            #[cfg(test)]
            leave_activation_stage_after_commit: AtomicBool::new(false),
            #[cfg(test)]
            fail_backup_receipt_write: AtomicBool::new(false),
            #[cfg(test)]
            registered_publish_barriers: Mutex::new(None),
        };
        let state = registry.lock()?;
        registry.recover_locked(&state, true)?;
        drop(state);
        Ok(registry)
    }

    pub(crate) fn connect_registered(
        &self,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        target_device_id: &str,
        source_device_id: &str,
        bearer: &str,
        completion_capability: PeerCompletionCapability,
    ) -> Result<AndroidCloneJobStatus, PeerSyncError> {
        let endpoint = validate_lan_endpoint(endpoint)?;
        validate_session_id(session_id)?;
        if !is_sha256(manifest_id) {
            return Err(PeerSyncError::Protocol(
                "invalid LAN manifest identity".to_owned(),
            ));
        }
        let state = self.lock()?;
        self.recover_locked(&state, false)?;
        if let Some(job_id) = self.read_current_id()? {
            let current = AndroidResumableCloneJob::open(self.jobs_root.join(job_id))?;
            let status = current.status()?;
            let credential = LanCloneClient::open_persisted(&current.root.join("credential.json"))?;
            if credential.matches_registered_credential(
                &endpoint,
                session_id,
                manifest_id,
                target_device_id,
                source_device_id,
                bearer,
            )? {
                return Ok(status);
            }
            return Err(PeerSyncError::Validation(
                "Android clone target already owns a different job".to_owned(),
            ));
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let job_root = self.jobs_root.join(&job_id);
        let job = AndroidResumableCloneJob::from_registered(
            &job_root,
            &endpoint,
            session_id,
            manifest_id,
            target_device_id,
            source_device_id,
            bearer,
            completion_capability,
        )?;
        let published = (|| {
            let _source_lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
            if !registered_android_job_source_is_current(&self.jobs_root, &job)? {
                crate::nlog!(
                    "warn",
                    "registered Android clone job no longer binds the registered source"
                );
                return Err(PeerSyncError::Validation(
                    super::registry_commands::REGISTERED_SOURCE_CHANGED.to_owned(),
                ));
            }
            let app_root = self.jobs_root.parent().ok_or_else(|| {
                PeerSyncError::Storage("Android clone jobs root has no app root".to_owned())
            })?;
            let source = super::device_registry::incoming_source_by_id(app_root, source_device_id)?;
            if !source.as_ref().is_some_and(|source| {
                source.device_id == source_device_id
                    && source.endpoint == endpoint
                    && source.bearer == bearer
                    && source.permissions.allows_read()
            }) {
                crate::nlog!(
                    "warn",
                    "registered Android clone source changed before publication"
                );
                return Err(PeerSyncError::Validation(
                    super::registry_commands::REGISTERED_SOURCE_CHANGED.to_owned(),
                ));
            }
            #[cfg(test)]
            self.wait_before_registered_publish_for_test()?;
            self.write_current_id(&job_id)
        })();
        if let Err(error) = published {
            job.discard().map_err(|cleanup| {
                PeerSyncError::Storage(format!(
                    "{error}; failed to discard unavailable registered Android clone: {cleanup}"
                ))
            })?;
            return Err(error);
        }
        job.status()
    }

    pub(crate) fn current(&self) -> Result<Option<AndroidCloneJobStatus>, PeerSyncError> {
        let state = self.lock()?;
        self.recover_locked(&state, false)?;
        let Some(job_id) = self.read_current_id()? else {
            return Ok(None);
        };
        let job_root = self.jobs_root.join(&job_id);
        if !job_root.exists() {
            self.remove_current_id()?;
            return Ok(None);
        }
        let job = AndroidResumableCloneJob::open(job_root)?;
        if job.cancel_requested()? {
            let mut status = job.status()?;
            status.phase = AndroidCloneJobPhase::Cancelled;
            return Ok(Some(status));
        }
        Ok(Some(job.status()?))
    }

    #[cfg(any(target_os = "android", test))]
    pub(crate) fn current_for_command(
        &self,
    ) -> Result<
        Option<super::registered_target_commands::AndroidRegisteredCloneStatus>,
        PeerSyncError,
    > {
        let state = self.lock()?;
        self.recover_locked(&state, false)?;
        let Some(job_id) = self.read_current_id()? else {
            return Ok(None);
        };
        let job_root = self.jobs_root.join(&job_id);
        if !job_root.exists() {
            self.remove_current_id()?;
            return Ok(None);
        }
        let job = AndroidResumableCloneJob::open(job_root)?;
        let mut status = job.status()?;
        if job.cancel_requested()? {
            status.phase = AndroidCloneJobPhase::Cancelled;
        }
        // Every job is created from a registered source, so recovery has already
        // released any job whose credential lost that binding.
        let source_device_id = job.client.source_device_id().ok_or_else(|| {
            PeerSyncError::Storage("Android clone job has no registered source".to_owned())
        })?;
        Ok(Some(
            super::registered_target_commands::safe_android_clone_status(source_device_id, &status),
        ))
    }

    #[cfg(test)]
    pub(crate) fn download(
        &self,
        job_id: &str,
        cancellation: &TransferCancellation,
    ) -> Result<DownloadReport, PeerSyncError> {
        let root = self.owned_job_root(job_id)?;
        AndroidResumableCloneJob::open(root)?.download(cancellation)
    }

    pub(crate) fn request_cancel(&self, job_id: &str) -> Result<(), PeerSyncError> {
        let root = self.owned_job_root(job_id)?;
        AndroidResumableCloneJob::request_cancel_at(root)
    }

    pub(crate) fn finalize(
        &self,
        job_id: &str,
        store: &mut PersistentStore,
        cas: &PayloadCas,
        activation_root: &Path,
        expected_revision: i64,
        cancellation: &dyn CancellationProbe,
    ) -> Result<AndroidCloneFinalizeReceipt, PeerSyncError> {
        let state = self.lock()?;
        let root = self.owned_job_root_locked(job_id, &state)?;
        let job = AndroidResumableCloneJob::open(&root)?;
        let status = job.status()?;
        if let Some(committed_revision) = status.committed_revision {
            validate_committed_activation(store, &status.manifest_id, job_id, committed_revision)?;
            self.finish_durable_completion(&root, job_id, activation_root)?;
            let status = AndroidResumableCloneJob::open(&root)?.status()?;
            return Ok(AndroidCloneFinalizeReceipt {
                revision: committed_revision,
                backup_path: status.backup_path,
            });
        }
        if status.phase != AndroidCloneJobPhase::VerifiedAwaitingActivation {
            return Err(PeerSyncError::Validation(
                "Android clone job is not awaiting activation".to_owned(),
            ));
        }
        let manifest_id = status.manifest_id;
        let mut client = job.into_activation_client()?;
        let observer_root = root.clone();
        #[cfg(test)]
        let fail_backup_receipt_write =
            self.fail_backup_receipt_write.swap(false, Ordering::SeqCst);
        let mut backup_observer = |backup_path: &Path| {
            #[cfg(test)]
            if fail_backup_receipt_write {
                return Err(PeerSyncError::Storage(
                    "injected Android backup receipt persistence failure".to_owned(),
                ));
            }
            AndroidResumableCloneJob::open(&observer_root)?
                .record_backup_path(backup_path)
                .map(|_| ())
        };
        let mut target = LosslessCloneTargetAdapter::new_with_owned_backup_observer(
            store,
            cas,
            activation_root,
            expected_revision,
            cancellation,
            &manifest_id,
            job_id,
            &mut backup_observer,
        )?;
        #[cfg(test)]
        if self
            .leave_activation_stage_after_commit
            .swap(false, Ordering::SeqCst)
        {
            target.leave_durable_job_after_commit_once_for_test();
        }
        let mut validator = AndroidVerifiedCloneValidator;
        let activation = activate_downloaded_clone(&mut client, &mut target, &mut validator);
        match activation {
            Ok(()) | Err(PeerSyncError::AlreadyActivated) => {}
            Err(error) => match target.active_manifest_id() {
                Ok(Some(active))
                    if active == manifest_id
                        && AndroidResumableCloneJob::open(&root)?
                            .status()?
                            .backup_path
                            .is_some() => {}
                Ok(_) => return Err(error),
                Err(reconcile) => {
                    return Err(PeerSyncError::Storage(format!(
                        "{error}; failed to reconcile Android clone activation: {reconcile}"
                    )))
                }
            },
        }
        drop(target);
        let committed_revision =
            u64::try_from(store.revision().map_err(store_error)?).map_err(|_| {
                PeerSyncError::Storage("Android clone committed revision is negative".to_owned())
            })?;
        record_activation_witness(store, job_id, &manifest_id, committed_revision)?;
        AndroidResumableCloneJob::open(&root)?.mark_committed(committed_revision)?;
        validate_committed_activation(store, &manifest_id, job_id, committed_revision)?;
        self.finish_durable_completion(&root, job_id, activation_root)?;
        let status = AndroidResumableCloneJob::open(&root)?.status()?;
        Ok(AndroidCloneFinalizeReceipt {
            revision: committed_revision,
            backup_path: status.backup_path,
        })
    }

    fn finish_durable_completion(
        &self,
        root: &Path,
        job_id: &str,
        activation_root: &Path,
    ) -> Result<(), PeerSyncError> {
        let mut job = AndroidResumableCloneJob::open(root)?;
        let persisted = job.read_status()?;
        if persisted.committed_revision.is_none() {
            return Err(PeerSyncError::Validation(
                "Android clone completion is not committed".to_owned(),
            ));
        }
        let completion_capability = descriptor_completion_capability(&job.descriptor)?;
        if job.client.source_device_id().is_some()
            && !backup_receipt_owns_activation(
                persisted.backup_path.as_deref(),
                activation_root,
                job_id,
            )?
        {
            return Err(PeerSyncError::Validation(
                "registered Android clone completion backup receipt is missing".to_owned(),
            ));
        }
        let (ledger_completed, ledger_total) = job.client.transfer_progress()?;
        if ledger_completed != ledger_total
            || persisted.completed_bytes != ledger_completed
            || persisted.total_bytes != Some(ledger_total)
        {
            return Err(PeerSyncError::Storage(
                "Android clone completion bytes differ from its verified transfer ledger"
                    .to_owned(),
            ));
        }
        if let Some(source_device_id) = job.client.source_device_id() {
            let app_root = self.jobs_root.parent().ok_or_else(|| {
                PeerSyncError::Storage("Android clone jobs root has no app root".to_owned())
            })?;
            match completion_capability {
                PeerCompletionCapability::Unsupported => {
                    let receipt_id = super::device_registry::completion_receipt_id(
                        "clone",
                        job_id,
                        &job.descriptor.manifest_id,
                    );
                    if persisted.completion_acknowledged {
                        if !super::device_registry::incoming_completed_operation_recorded(
                            app_root,
                            source_device_id,
                            &receipt_id,
                        )? {
                            return Err(PeerSyncError::Validation(
                                "Android clone completion receipt is missing".to_owned(),
                            ));
                        }
                    } else {
                        super::device_registry::record_incoming_completed_operation_once(
                            app_root,
                            source_device_id,
                            &receipt_id,
                            ledger_total,
                        )?;
                    }
                }
                PeerCompletionCapability::V1 => {
                    let completion_lease_id =
                        persisted.completion_lease_id.as_deref().ok_or_else(|| {
                            PeerSyncError::Validation(
                                "Android clone completion lease is missing".to_owned(),
                            )
                        })?;
                    let delivery = super::device_registry::PendingCompletionDelivery {
                        source_device_id: source_device_id.to_owned(),
                        lane: super::device_registry::CompletionLane::Clone
                            .as_str()
                            .to_owned(),
                        completion_lease_id: completion_lease_id.to_owned(),
                        manifest_id: job.descriptor.manifest_id.clone(),
                        useful_bytes: ledger_total,
                        receipt_id: super::device_registry::completion_receipt_id(
                            "clone",
                            completion_lease_id,
                            &job.descriptor.manifest_id,
                        ),
                    };
                    if persisted.completion_acknowledged {
                        if !super::device_registry::incoming_completion_is_durable(
                            app_root, &delivery,
                        )? {
                            return Err(PeerSyncError::Validation(
                                "Android clone completion receipt is missing".to_owned(),
                            ));
                        }
                    } else {
                        let delivered =
                            super::client::prepare_and_deliver_registered_clone_completion(
                                app_root,
                                &root.join("credential.json"),
                                &delivery,
                            )?;
                        if delivered {
                            super::device_registry::finalize_incoming_completion_delivery(
                                app_root, &delivery,
                            )?;
                        }
                        if !super::device_registry::incoming_completion_is_durable(
                            app_root, &delivery,
                        )? {
                            return Err(PeerSyncError::Validation(
                                "Android clone completion is not durable".to_owned(),
                            ));
                        }
                    }
                }
            }
        }
        job.acknowledge_completion()
    }

    #[cfg(test)]
    pub(crate) fn leave_activation_stage_after_commit_once_for_test(&self) {
        self.leave_activation_stage_after_commit
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fail_backup_receipt_write_once_for_test(&self) {
        self.fail_backup_receipt_write.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn pause_registered_publish_once_for_test(
        &self,
        reached_publish: Arc<Barrier>,
        resume_publish: Arc<Barrier>,
    ) {
        *self.registered_publish_barriers.lock().unwrap() = Some((reached_publish, resume_publish));
    }

    #[cfg(test)]
    fn wait_before_registered_publish_for_test(&self) -> Result<(), PeerSyncError> {
        let barriers = self
            .registered_publish_barriers
            .lock()
            .map_err(|_| {
                PeerSyncError::Storage(
                    "Android clone registered publish test hook lock failed".to_owned(),
                )
            })?
            .take();
        if let Some((reached_publish, resume_publish)) = barriers {
            reached_publish.wait();
            resume_publish.wait();
        }
        Ok(())
    }

    pub(crate) fn release(
        &self,
        job_id: &str,
        store: &PersistentStore,
    ) -> Result<(), PeerSyncError> {
        let state = self.lock()?;
        let root = self.owned_job_root_locked(job_id, &state)?;
        let job = AndroidResumableCloneJob::open(root)?;
        let status = job.read_status()?;
        let Some(committed_revision) = status.committed_revision else {
            return Err(PeerSyncError::Validation(
                "Android clone job cannot be released before durable completion".to_owned(),
            ));
        };
        let activation_root = self
            .jobs_root
            .parent()
            .ok_or_else(|| {
                PeerSyncError::Storage("Android clone jobs root has no app root".to_owned())
            })?
            .join("peer-clone-activation");
        validate_committed_activation(
            store,
            &job.descriptor.manifest_id,
            job_id,
            committed_revision,
        )?;
        self.finish_durable_completion(&job.root, job_id, &activation_root)?;
        let completed = AndroidResumableCloneJob::open(&job.root)?;
        if !completed.read_status()?.completion_acknowledged {
            return Err(PeerSyncError::Validation(
                "Android clone job cannot be released before durable completion".to_owned(),
            ));
        }
        completed.discard()?;
        self.remove_current_id()
    }

    fn owned_job_root(&self, job_id: &str) -> Result<PathBuf, PeerSyncError> {
        let state = self.lock()?;
        self.owned_job_root_locked(job_id, &state)
    }

    fn owned_job_root_locked(
        &self,
        job_id: &str,
        _state: &MutexGuard<'_, ()>,
    ) -> Result<PathBuf, PeerSyncError> {
        validate_job_id(job_id)?;
        if self.read_current_id()?.as_deref() != Some(job_id) {
            return Err(PeerSyncError::Validation(
                "Android clone job is not the current owned job".to_owned(),
            ));
        }
        let root = self.jobs_root.join(job_id);
        if !root.exists() {
            self.remove_current_id()?;
            return Err(PeerSyncError::Validation(
                "Android clone job no longer exists".to_owned(),
            ));
        }
        Ok(root)
    }

    fn recover_locked(
        &self,
        _state: &MutexGuard<'_, ()>,
        pause_interrupted_downloads: bool,
    ) -> Result<(), PeerSyncError> {
        cleanup_deleting_jobs(&self.jobs_root)?;
        let _source_lifecycle = super::registry_commands::lock_registered_source_lifecycle()?;
        let pointer = self.read_current_id()?;
        let mut jobs = Vec::new();
        for entry in fs::read_dir(&self.jobs_root)? {
            let entry = entry?;
            let metadata = entry.file_type()?;
            if !metadata.is_dir() || metadata.is_symlink() {
                continue;
            }
            let Some(job_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_job_id(&job_id).is_err() {
                continue;
            }
            let job = match AndroidResumableCloneJob::open(entry.path()) {
                Ok(job) => job,
                Err(error) if pointer.as_deref() != Some(job_id.as_str()) => {
                    if cleanup_unpublished_job(&entry.path(), &job_id).is_ok() {
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            // Every job is created from a registered source, so a job directory
            // whose credential has no source identity is corrupt: release it the
            // same way an orphaned job is released.
            if job.client.source_device_id().is_none() {
                job.discard()?;
                continue;
            }
            let status = job.read_status()?;
            if status.committed_revision.is_none() && pointer.as_deref() != Some(job_id.as_str()) {
                job.discard()?;
                continue;
            }
            if status.committed_revision.is_none()
                && !registered_android_job_source_is_current(&self.jobs_root, &job)?
            {
                job.discard()?;
                continue;
            }
            if pause_interrupted_downloads && job.cancel_requested()? {
                job.discard()?;
                continue;
            }
            if pause_interrupted_downloads && job.phase()? == AndroidCloneJobPhase::Downloading {
                job.mark_paused()?;
            }
            jobs.push(job_id);
        }
        if jobs.len() > 1 {
            return Err(PeerSyncError::Validation(
                "Android clone registry contains more than one owned job".to_owned(),
            ));
        }
        let recovered = jobs.pop();
        if pointer != recovered {
            match recovered {
                Some(job_id) => self.write_current_id(&job_id)?,
                None => self.remove_current_id()?,
            }
        }
        Ok(())
    }

    fn read_current_id(&self) -> Result<Option<String>, PeerSyncError> {
        let path = self.jobs_root.join("current.json");
        if !path.exists() {
            return Ok(None);
        }
        let current: AndroidCloneRegistryOwnership =
            read_bounded_json(&path, MAX_JOB_RECORD_BYTES)?;
        if current.schema != REGISTRY_SCHEMA {
            return Err(PeerSyncError::Storage(
                "Android clone registry ownership is invalid".to_owned(),
            ));
        }
        validate_job_id(&current.job_id)?;
        Ok(Some(current.job_id))
    }

    fn write_current_id(&self, job_id: &str) -> Result<(), PeerSyncError> {
        validate_job_id(job_id)?;
        super::maintenance::with_backup_reference_lifecycle(|| {
            write_json_atomic(
                &self.jobs_root.join("current.json"),
                &AndroidCloneRegistryOwnership {
                    schema: REGISTRY_SCHEMA.to_owned(),
                    job_id: job_id.to_owned(),
                },
            )
        })
    }

    fn remove_current_id(&self) -> Result<(), PeerSyncError> {
        match fs::remove_file(self.jobs_root.join("current.json")) {
            Ok(()) => sync_parent_directory(&self.jobs_root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, PeerSyncError> {
        self.state
            .lock()
            .map_err(|_| PeerSyncError::Storage("Android clone registry lock failed".to_owned()))
    }
}

fn registered_android_job_source_is_current(
    jobs_root: &Path,
    job: &AndroidResumableCloneJob,
) -> Result<bool, PeerSyncError> {
    let app_root = jobs_root.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone jobs root has no app root".to_owned())
    })?;
    let credential = LanCloneClient::open_persisted(&job.root.join("credential.json"))?;
    let source_device_id = credential.registered_source_device_id().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job has no registered source".to_owned())
    })?;
    let Some(source) = super::device_registry::incoming_source_by_id(app_root, source_device_id)?
    else {
        return Ok(false);
    };
    let (endpoint, session_id, manifest_id) = credential.target_identity()?;
    let local_device_id = super::device_registry::load_or_create_device_id(app_root)?;
    Ok(source.permissions.allows_read()
        && credential.matches_registered_credential(
            endpoint,
            session_id,
            manifest_id,
            &local_device_id,
            source_device_id,
            &source.bearer,
        )?
        && source.endpoint == endpoint)
}

fn record_activation_witness(
    store: &PersistentStore,
    job_id: &str,
    manifest_id: &str,
    committed_revision: u64,
) -> Result<(), PeerSyncError> {
    let revision = i64::try_from(committed_revision).map_err(|_| {
        PeerSyncError::Validation(
            "Android clone committed activation evidence is invalid".to_owned(),
        )
    })?;
    store
        .set_app_kv(
            ACTIVE_OPERATION_KEY,
            &serde_json::to_value(AndroidCloneActivationWitness {
                job_id: job_id.to_owned(),
                manifest_id: manifest_id.to_owned(),
                revision,
            })
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?,
        )
        .map_err(store_error)
}

fn validate_committed_activation(
    store: &PersistentStore,
    manifest_id: &str,
    job_id: &str,
    committed_revision: u64,
) -> Result<(), PeerSyncError> {
    let committed_revision = i64::try_from(committed_revision).map_err(|_| {
        PeerSyncError::Validation(
            "Android clone committed activation evidence is invalid".to_owned(),
        )
    })?;
    let revision = store.revision().map_err(store_error)?;
    let marker = store
        .get_app_kv("peerCloneActiveManifest")
        .map_err(store_error)?
        .and_then(|value| value.as_object().cloned());
    let marker_valid = marker.as_ref().is_some_and(|marker| {
        marker.len() == 2
            && revision >= committed_revision
            && marker.get("revision").and_then(serde_json::Value::as_i64)
                == Some(committed_revision)
            && marker.get("manifestId").and_then(serde_json::Value::as_str) == Some(manifest_id)
    });
    let witness_valid = store
        .get_app_kv(ACTIVE_OPERATION_KEY)
        .map_err(store_error)?
        .and_then(|value| serde_json::from_value::<AndroidCloneActivationWitness>(value).ok())
        .is_some_and(|witness| {
            witness.job_id == job_id
                && witness.manifest_id == manifest_id
                && witness.revision == committed_revision
        });
    if !marker_valid || !witness_valid {
        return Err(PeerSyncError::Validation(
            "Android clone committed activation evidence is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn backup_receipt_owns_activation(
    backup_path: Option<&Path>,
    activation_root: &Path,
    job_id: &str,
) -> Result<bool, PeerSyncError> {
    let Some(backup_path) = backup_path else {
        return Ok(false);
    };
    let metadata = match fs::symlink_metadata(backup_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || crate::trust_boundary::is_link_like(&metadata) {
        return Ok(false);
    }
    let expected = activation_root
        .join("backups")
        .join(format!("pre-clone-{job_id}.lossless"));
    let expected = match fs::canonicalize(expected) {
        Ok(expected) => expected,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(fs::canonicalize(backup_path)? == expected)
}

/// `source_device_id` of `None` asks the same question about any registered source.
pub(crate) fn registered_clone_source_is_active(
    app_root: &Path,
    source_device_id: Option<&str>,
) -> Result<bool, PeerSyncError> {
    let app_root = fs::canonicalize(app_root)?;
    let requested_jobs_root = app_root.join("peer-clone-jobs");
    if !requested_jobs_root.try_exists()? {
        return Ok(false);
    }
    let jobs_root = fs::canonicalize(requested_jobs_root)?;
    if jobs_root.parent() != Some(app_root.as_path()) {
        return Err(PeerSyncError::Storage(
            "Android clone jobs root escaped app data".to_owned(),
        ));
    }
    let current_path = jobs_root.join("current.json");
    if !current_path.try_exists()? {
        return Ok(false);
    }
    let current: AndroidCloneRegistryOwnership =
        read_bounded_json(&current_path, MAX_JOB_RECORD_BYTES)?;
    if current.schema != REGISTRY_SCHEMA {
        return Err(PeerSyncError::Storage(
            "Android clone registry ownership is invalid".to_owned(),
        ));
    }
    validate_job_id(&current.job_id)?;
    let (job_root, descriptor) = validate_job(&jobs_root.join(current.job_id))?;
    let credential = LanCloneClient::open_persisted(&job_root.join("credential.json"))?;
    let (_, _, credential_manifest_id) = credential.target_identity()?;
    if credential_manifest_id != descriptor.manifest_id {
        return Err(PeerSyncError::Storage(
            "Android clone job target identity is inconsistent".to_owned(),
        ));
    }
    Ok(credential
        .registered_source_device_id()
        .is_some_and(|registered| source_device_id.is_none_or(|wanted| registered == wanted)))
}

struct AndroidVerifiedCloneValidator;

impl<S> CloneValidator<S> for AndroidVerifiedCloneValidator {
    fn validate(
        &mut self,
        _manifest: &super::CloneManifest,
        _stage: &S,
    ) -> Result<(), PeerSyncError> {
        Ok(())
    }
}

fn store_error(error: StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn write_cancel_marker(root: &Path, manifest_id: &str) -> Result<(), PeerSyncError> {
    let path = root.join("cancel.requested");
    if path.exists() {
        return validate_marker_if_present(&path, CANCEL_REQUESTED_SCHEMA, manifest_id);
    }
    write_new_json(
        &path,
        &AndroidCloneJobMarker {
            schema: CANCEL_REQUESTED_SCHEMA.to_owned(),
            manifest_id: manifest_id.to_owned(),
        },
    )
}

fn validate_job(job_root: &Path) -> Result<(PathBuf, AndroidCloneJobDescriptor), PeerSyncError> {
    let job_id = validate_job_id_from_path(job_root)?;
    let root = fs::canonicalize(job_root)?;
    validate_ownership(&root, &job_id)?;
    let descriptor: AndroidCloneJobDescriptor =
        read_bounded_json(&root.join("job.json"), MAX_JOB_RECORD_BYTES)?;
    if descriptor.schema != JOB_SCHEMA
        || descriptor.job_id != job_id
        || !is_sha256(&descriptor.manifest_id)
        || !matches!(
            descriptor.completion_capability.as_deref(),
            None | Some(super::lan::PEER_COMPLETION_CAPABILITY_V1)
        )
    {
        return Err(PeerSyncError::Storage(
            "Android clone job descriptor is invalid".to_owned(),
        ));
    }
    Ok((root, descriptor))
}

fn descriptor_completion_capability(
    descriptor: &AndroidCloneJobDescriptor,
) -> Result<PeerCompletionCapability, PeerSyncError> {
    match descriptor.completion_capability.as_deref() {
        None => Ok(PeerCompletionCapability::Unsupported),
        Some(super::lan::PEER_COMPLETION_CAPABILITY_V1) => Ok(PeerCompletionCapability::V1),
        Some(_) => Err(PeerSyncError::Storage(
            "Android clone completion capability is invalid".to_owned(),
        )),
    }
}

fn validate_android_completion_state(
    descriptor: &AndroidCloneJobDescriptor,
    status: &AndroidClonePersistedStatus,
) -> Result<(), PeerSyncError> {
    status.validate()?;
    let valid = match (
        descriptor.completion_capability.as_deref(),
        status.completion_lease_id.as_deref(),
    ) {
        (None, None) => true,
        (Some(super::lan::PEER_COMPLETION_CAPABILITY_V1), Some(lease_id)) => {
            is_canonical_v4_uuid(lease_id) && lease_id != descriptor.job_id
        }
        _ => false,
    };
    if !valid {
        return Err(PeerSyncError::Storage(
            "Android clone completion lease is inconsistent".to_owned(),
        ));
    }
    Ok(())
}

fn validate_job_id_from_path(path: &Path) -> Result<String, PeerSyncError> {
    let value = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            PeerSyncError::Storage("Android clone job path has no UTF-8 identity".to_owned())
        })?;
    validate_job_id(value)?;
    Ok(value.to_owned())
}

fn validate_job_id(value: &str) -> Result<(), PeerSyncError> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| PeerSyncError::Storage("Android clone job identity is invalid".to_owned()))?;
    if parsed.get_version_num() != 4 || parsed.to_string() != value {
        return Err(PeerSyncError::Storage(
            "Android clone job identity must be a canonical UUID v4".to_owned(),
        ));
    }
    Ok(())
}

fn is_canonical_v4_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.get_version_num() == 4 && parsed.to_string() == value)
        .unwrap_or(false)
}

fn validate_session_id(value: &str) -> Result<(), PeerSyncError> {
    if uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(PeerSyncError::Protocol(
            "invalid LAN pairing data".to_owned(),
        ))
    }
}

fn validate_ownership(root: &Path, expected_job_id: &str) -> Result<(), PeerSyncError> {
    if root.file_name().and_then(|value| value.to_str()) != Some(expected_job_id) {
        return Err(PeerSyncError::Storage(
            "Android clone job root changed identity".to_owned(),
        ));
    }
    let ownership: AndroidCloneJobOwnership =
        read_bounded_json(&root.join("ownership.json"), MAX_JOB_RECORD_BYTES)?;
    if ownership.schema != JOB_OWNERSHIP_SCHEMA || ownership.job_id != expected_job_id {
        return Err(PeerSyncError::Storage(
            "Android clone job ownership is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_marker_if_present(
    path: &Path,
    schema: &str,
    manifest_id: &str,
) -> Result<(), PeerSyncError> {
    if !path.exists() {
        return Ok(());
    }
    let marker: AndroidCloneJobMarker = read_bounded_json(path, MAX_JOB_RECORD_BYTES)?;
    if marker.schema != schema || marker.manifest_id != manifest_id {
        return Err(PeerSyncError::Storage(
            "Android clone job marker is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<(), PeerSyncError> {
    let bytes = bounded_json(value)?;
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job record has no parent".to_owned())
    })?;
    let temporary = temporary_path(path)?;
    let result = (|| {
        write_synced_file(&temporary, &bytes)?;
        match fs::rename(&temporary, path) {
            Ok(()) => sync_parent_directory(parent),
            Err(error) => {
                let existing = fs::symlink_metadata(path)
                    .ok()
                    .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                    .and_then(|_| fs::read(path).ok());
                if existing.as_deref() == Some(bytes.as_slice()) {
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), PeerSyncError> {
    let bytes = bounded_json(value)?;
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job record has no parent".to_owned())
    })?;
    let temporary = temporary_path(path)?;
    let result = (|| {
        write_synced_file(&temporary, &bytes)?;
        replace_file_atomic(&temporary, path)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn bounded_json(value: &impl Serialize) -> Result<Vec<u8>, PeerSyncError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() as u64 > MAX_JOB_RECORD_BYTES {
        return Err(PeerSyncError::Storage(
            "Android clone job record exceeds its bound".to_owned(),
        ));
    }
    Ok(bytes)
}

fn temporary_path(path: &Path) -> Result<PathBuf, PeerSyncError> {
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job record has no parent".to_owned())
    })?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            PeerSyncError::Storage("Android clone job record has no UTF-8 name".to_owned())
        })?;
    Ok(parent.join(format!(".{file_name}-{}.tmp", uuid::Uuid::new_v4())))
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum: u64,
) -> Result<T, PeerSyncError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(PeerSyncError::Storage(
            "invalid Android clone job record".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(PeerSyncError::Storage(
            "Android clone job record exceeds its bound".to_owned(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| PeerSyncError::Storage(error.to_string()))
}

fn cleanup_owned_job(root: &Path, expected_job_id: &str) -> Result<(), PeerSyncError> {
    cleanup_job_directory(root, expected_job_id, true)
}

fn cleanup_unpublished_job(root: &Path, expected_job_id: &str) -> Result<(), PeerSyncError> {
    cleanup_job_directory(root, expected_job_id, false)
}

fn cleanup_job_directory(
    root: &Path,
    expected_job_id: &str,
    require_ownership: bool,
) -> Result<(), PeerSyncError> {
    validate_job_id(expected_job_id)?;
    let parent = root.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job has no registry parent".to_owned())
    })?;
    let parent = fs::canonicalize(parent)?;
    cleanup_deleting_job(&parent, expected_job_id)?;
    let canonical = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if canonical.parent() != Some(parent.as_path())
        || canonical.file_name().and_then(|value| value.to_str()) != Some(expected_job_id)
    {
        return Err(PeerSyncError::Storage(
            "Android clone cleanup escaped its owned root".to_owned(),
        ));
    }
    if require_ownership {
        validate_ownership(&canonical, expected_job_id)?;
    }
    let deleting = deleting_job_path(&parent, expected_job_id);
    match fs::rename(&canonical, &deleting) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return cleanup_deleting_job(&parent, expected_job_id);
        }
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(&parent)?;
    remove_deleting_job(&parent, expected_job_id)
}

fn cleanup_deleting_jobs(jobs_root: &Path) -> Result<(), PeerSyncError> {
    for entry in fs::read_dir(jobs_root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(job_id) = name.strip_prefix(DELETING_JOB_PREFIX) else {
            continue;
        };
        if validate_job_id(job_id).is_ok() {
            remove_deleting_job(jobs_root, job_id)?;
        }
    }
    Ok(())
}

fn cleanup_deleting_job_for_root(root: &Path) -> Result<(), PeerSyncError> {
    let job_id = validate_job_id_from_path(root)?;
    let parent = root.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job has no registry parent".to_owned())
    })?;
    let parent = fs::canonicalize(parent)?;
    cleanup_deleting_job(&parent, &job_id)
}

fn cleanup_deleting_job(jobs_root: &Path, job_id: &str) -> Result<(), PeerSyncError> {
    let deleting = deleting_job_path(jobs_root, job_id);
    if !deleting.exists() {
        return Ok(());
    }
    remove_deleting_job(jobs_root, job_id)
}

fn remove_deleting_job(jobs_root: &Path, job_id: &str) -> Result<(), PeerSyncError> {
    validate_job_id(job_id)?;
    let deleting = deleting_job_path(jobs_root, job_id);
    let metadata = match fs::symlink_metadata(&deleting) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(PeerSyncError::Storage(
            "Android clone deletion tombstone is invalid".to_owned(),
        ));
    }
    let canonical = fs::canonicalize(&deleting)?;
    if canonical.parent() != Some(jobs_root)
        || canonical.file_name().and_then(|value| value.to_str())
            != deleting.file_name().and_then(|value| value.to_str())
    {
        return Err(PeerSyncError::Storage(
            "Android clone deletion tombstone escaped its registry".to_owned(),
        ));
    }
    match fs::remove_dir_all(&canonical) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(jobs_root)
}

fn deleting_job_path(jobs_root: &Path, job_id: &str) -> PathBuf {
    jobs_root.join(format!("{DELETING_JOB_PREFIX}{job_id}"))
}

fn truncate_utf8(value: &str, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value.to_owned();
    }
    let mut end = maximum_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
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
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
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

fn is_sha256(value: &str) -> bool {
    crate::trust_boundary::is_lower_hex_256(value)
}
