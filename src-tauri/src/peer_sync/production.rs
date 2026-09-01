use super::{
    prepare_clone_session,
    protocol::{validate_hash, CloneObjectKind},
    CloneActivation, CloneSource, CloneTargetAdapter, PeerSyncError, PinnedCloneRevision,
    PinnedSourceObject, PreparedCloneSession,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    local_backup::CancellationProbe,
    lossless_backup::{
        create_and_verify_lossless_backup_v1,
        restore_verified_lossless_package_v1_durable_controlled,
        verify_lossless_package_v1_for_production, LosslessError, LosslessErrorCode,
    },
    persistent_store::PersistentStore,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const ACTIVE_MANIFEST_KEY: &str = "peerCloneActiveManifest";
pub(crate) const ACTIVATION_STAGE_SEPARATOR: char = '_';
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActiveManifestMarker {
    manifest_id: String,
    revision: i64,
}

#[derive(Clone)]
struct PreparedLosslessSource {
    revision: u64,
    package: PathBuf,
}

impl CloneSource for PreparedLosslessSource {
    type Lease = Self;

    fn pin(&self) -> Result<Self::Lease, PeerSyncError> {
        Ok(self.clone())
    }
}

impl PinnedCloneRevision for PreparedLosslessSource {
    fn source_revision(&self) -> u64 {
        self.revision
    }

    fn objects(&self) -> Result<Vec<PinnedSourceObject>, PeerSyncError> {
        Ok(vec![PinnedSourceObject::database_with_format(
            &self.package,
            super::CLONE_LOSSLESS_DATABASE_FORMAT,
        )])
    }
}

pub(crate) fn prepare_lossless_clone_session(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    expected_revision: i64,
    preparation_root: &Path,
    session_root: &Path,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedCloneSession, PeerSyncError> {
    let revision = u64::try_from(expected_revision).map_err(|_| {
        PeerSyncError::Validation("peer clone source revision must be nonnegative".to_owned())
    })?;
    fs::create_dir(preparation_root)?;
    let package = preparation_root.join("source.lossless");
    let prepared = (|| {
        create_and_verify_lossless_backup_v1(
            &package,
            preparation_root,
            cas,
            store,
            expected_revision,
            cancellation,
        )
        .map_err(lossless_error)?;
        prepare_clone_session(&PreparedLosslessSource { revision, package }, session_root)
    })();
    let cleanup = fs::remove_dir_all(preparation_root).map_err(PeerSyncError::from);
    match (prepared, cleanup) {
        (Ok(session), Ok(())) => Ok(session),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(PeerSyncError::Storage(format!(
            "{error}; lossless source preparation cleanup failed: {cleanup}"
        ))),
    }
}

pub(crate) struct LosslessCloneStage {
    directory: PathBuf,
    package: Option<PathBuf>,
    pre_replacement_backup: PathBuf,
    manifest_id: String,
    durable_job_id: String,
}

pub(crate) struct LosslessCloneTargetAdapter<'a> {
    store: &'a mut PersistentStore,
    cas: &'a PayloadCas,
    root: PathBuf,
    expected_revision: i64,
    cancellation: &'a dyn CancellationProbe,
    #[cfg(test)]
    fail_cleanup_after_commit: bool,
    #[cfg(test)]
    leave_durable_after_precommit_failure: bool,
    #[cfg(test)]
    leave_durable_after_commit: bool,
}

impl<'a> LosslessCloneTargetAdapter<'a> {
    pub(crate) fn new(
        store: &'a mut PersistentStore,
        cas: &'a PayloadCas,
        root: &Path,
        expected_revision: i64,
        cancellation: &'a dyn CancellationProbe,
    ) -> Result<Self, PeerSyncError> {
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        fs::create_dir_all(root.join("backups"))?;
        let mut adapter = Self {
            store,
            cas,
            root,
            expected_revision,
            cancellation,
            #[cfg(test)]
            fail_cleanup_after_commit: false,
            #[cfg(test)]
            leave_durable_after_precommit_failure: false,
            #[cfg(test)]
            leave_durable_after_commit: false,
        };
        adapter.reconcile_abandoned_jobs()?;
        Ok(adapter)
    }

    #[cfg(test)]
    pub(crate) fn fail_cleanup_after_commit_once_for_test(&mut self) {
        self.fail_cleanup_after_commit = true;
    }

    #[cfg(test)]
    pub(crate) fn leave_durable_job_after_precommit_failure_once_for_test(&mut self) {
        self.leave_durable_after_precommit_failure = true;
    }

    #[cfg(test)]
    pub(crate) fn leave_durable_job_after_commit_once_for_test(&mut self) {
        self.leave_durable_after_commit = true;
    }

    fn current_marker(&self) -> Result<Option<ActiveManifestMarker>, PeerSyncError> {
        let Some(value) = self
            .store
            .get_app_kv(ACTIVE_MANIFEST_KEY)
            .map_err(store_error)?
        else {
            return Ok(None);
        };
        let marker: ActiveManifestMarker = serde_json::from_value(value)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        validate_hash(&marker.manifest_id)?;
        Ok(Some(marker))
    }

    fn remove_owned_stage(&self, stage: &LosslessCloneStage) -> Result<(), PeerSyncError> {
        self.remove_owned_directory(&stage.directory)
    }

    fn remove_owned_directory(&self, directory: &Path) -> Result<(), PeerSyncError> {
        let directory = match fs::canonicalize(directory) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if directory.parent() != Some(self.root.as_path()) {
            return Err(PeerSyncError::Storage(
                "peer clone staging directory escaped its owned root".to_owned(),
            ));
        }
        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn reconcile_abandoned_jobs(&mut self) -> Result<(), PeerSyncError> {
        let mut stages = fs::read_dir(&self.root)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        stages.sort();
        for directory in stages {
            let Some((manifest_id, durable_job_id)) = activation_stage_identity(&directory) else {
                continue;
            };
            if !self.reconcile_job(&directory, &manifest_id, &durable_job_id)? {
                self.remove_owned_directory(&directory)?;
            }
        }
        Ok(())
    }

    fn reconcile_stage_job(&mut self, stage: &LosslessCloneStage) -> Result<(), PeerSyncError> {
        self.reconcile_job(&stage.directory, &stage.manifest_id, &stage.durable_job_id)
            .map(|_| ())
    }

    fn reconcile_job(
        &mut self,
        directory: &Path,
        manifest_id: &str,
        durable_job_id: &str,
    ) -> Result<bool, PeerSyncError> {
        let mut job = match DurableCasJob::open(self.cas.repository_root(), durable_job_id) {
            Ok(job) => job,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if job.kind() != CasJobKind::PeerClone {
            return Err(PeerSyncError::Storage(
                "peer clone activation stage refers to a different durable CAS job kind".to_owned(),
            ));
        }
        let outcome = if job.is_sealed() && self.marker_committed(manifest_id)? {
            CasReleaseOutcome::Committed
        } else {
            CasReleaseOutcome::Aborted
        };
        job.release(outcome)?;
        self.remove_owned_directory(directory)?;
        Ok(true)
    }

    fn marker_committed(&self, manifest_id: &str) -> Result<bool, PeerSyncError> {
        let revision = self.store.revision().map_err(store_error)?;
        Ok(self
            .current_marker()?
            .is_some_and(|marker| marker.manifest_id == manifest_id && marker.revision == revision))
    }
}

impl CloneTargetAdapter for LosslessCloneTargetAdapter<'_> {
    type Stage = LosslessCloneStage;

    fn active_manifest_id(&self) -> Result<Option<String>, PeerSyncError> {
        let revision = self.store.revision().map_err(store_error)?;
        Ok(self
            .current_marker()?
            .filter(|marker| marker.revision == revision)
            .map(|marker| marker.manifest_id))
    }

    fn begin(&mut self, manifest_id: &str) -> Result<Self::Stage, PeerSyncError> {
        validate_hash(manifest_id)?;
        let stage_id = uuid::Uuid::new_v4().to_string();
        let directory = self.root.join(format!(
            "{manifest_id}{ACTIVATION_STAGE_SEPARATOR}{stage_id}"
        ));
        fs::create_dir(&directory)?;
        crate::trust_boundary::sync_directory(&self.root)?;
        Ok(LosslessCloneStage {
            directory,
            package: None,
            pre_replacement_backup: self
                .root
                .join("backups")
                .join(format!("pre-clone-{manifest_id}-{stage_id}.lossless")),
            manifest_id: manifest_id.to_owned(),
            durable_job_id: stage_id,
        })
    }

    fn stage_object(
        &mut self,
        stage: &mut Self::Stage,
        kind: CloneObjectKind,
        logical_key: &str,
        metadata: &Value,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError> {
        if kind != CloneObjectKind::Database
            || logical_key != "database"
            || !metadata.is_null()
            || stage.package.is_some()
        {
            return Err(PeerSyncError::Validation(
                "lossless peer clone must contain exactly one database package".to_owned(),
            ));
        }
        let temporary = stage.directory.join("incoming.lossless.tmp");
        let package = stage.directory.join("incoming.lossless");
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
        }
        output.flush()?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, &package)?;
        stage.package = Some(package);
        Ok(())
    }

    fn abort(&mut self, stage: Self::Stage) -> Result<(), PeerSyncError> {
        self.reconcile_stage_job(&stage)?;
        self.remove_owned_stage(&stage)
    }

    fn activate_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_manifest_id: Option<&str>,
        new_manifest_id: &str,
    ) -> Result<CloneActivation, PeerSyncError> {
        validate_hash(new_manifest_id)?;
        if stage.manifest_id != new_manifest_id {
            return Err(PeerSyncError::Validation(
                "peer clone activation manifest does not own its staging directory".to_owned(),
            ));
        }
        self.reconcile_stage_job(stage)?;
        let actual = self.active_manifest_id()?;
        if actual.as_deref() == Some(new_manifest_id) {
            return Ok(CloneActivation::AlreadyActive);
        }
        if actual.as_deref() != expected_manifest_id {
            return Ok(CloneActivation::Conflict { actual });
        }
        let package = stage.package.as_ref().ok_or_else(|| {
            PeerSyncError::Validation("lossless peer clone package was not staged".to_owned())
        })?;
        let marker = ActiveManifestMarker {
            manifest_id: new_manifest_id.to_owned(),
            revision: self.expected_revision.checked_add(1).ok_or_else(|| {
                PeerSyncError::Validation("peer clone revision overflow".to_owned())
            })?,
        };
        let marker_value = serde_json::to_value(&marker)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        let restore_staging = stage.directory.join("restore-staging");
        fs::create_dir(&restore_staging)?;
        let mut verifier = File::open(package)?;
        let verified = verify_lossless_package_v1_for_production(&mut verifier, self.cancellation)
            .map_err(lossless_error)?;
        let mut reader = File::open(package)?;
        let now_ms = unix_time_ms()?;
        let mut durable_job = DurableCasJob::begin(
            self.cas.repository_root(),
            &stage.durable_job_id,
            CasJobKind::PeerClone,
            now_ms,
        )?;
        let commit_attempted = std::cell::Cell::new(false);
        #[cfg(test)]
        let leave_precommit = std::mem::take(&mut self.leave_durable_after_precommit_failure);
        #[cfg(not(test))]
        let leave_precommit = false;
        let before_activation = || {
            if leave_precommit {
                Err("injected process loss before peer clone commit".to_owned())
            } else {
                Ok(())
            }
        };
        let before_commit_attempt = || commit_attempted.set(true);
        let restore = restore_verified_lossless_package_v1_durable_controlled(
            &mut reader,
            &verified,
            &restore_staging,
            self.cas,
            self.store,
            self.expected_revision,
            &stage.pre_replacement_backup,
            Some((ACTIVE_MANIFEST_KEY, &marker_value)),
            &mut durable_job,
            now_ms,
            &before_activation,
            &before_commit_attempt,
            self.cancellation,
        );
        match restore {
            Ok(_) => {
                #[cfg(test)]
                if std::mem::take(&mut self.leave_durable_after_commit) {
                    return Err(PeerSyncError::Storage(
                        "injected process loss after peer clone commit".to_owned(),
                    ));
                }
                durable_job.release(CasReleaseOutcome::Committed)?;
                #[cfg(test)]
                if std::mem::take(&mut self.fail_cleanup_after_commit) {
                    return Err(PeerSyncError::Storage(
                        "injected stage cleanup failure after clone commit".to_owned(),
                    ));
                }
                self.remove_owned_stage(stage)?;
                Ok(CloneActivation::Activated)
            }
            Err(error) => {
                #[cfg(test)]
                if leave_precommit {
                    return Err(lossless_error(error));
                }
                let outcome = if commit_attempted.get() && self.marker_committed(new_manifest_id)? {
                    CasReleaseOutcome::Committed
                } else {
                    CasReleaseOutcome::Aborted
                };
                durable_job.release(outcome)?;
                if error.code != LosslessErrorCode::RevisionConflict {
                    return Err(lossless_error(error));
                }
                let actual = self.active_manifest_id()?;
                if actual.as_deref() == Some(new_manifest_id) {
                    Ok(CloneActivation::AlreadyActive)
                } else {
                    Ok(CloneActivation::Conflict { actual })
                }
            }
        }
    }
}

pub(crate) fn activation_stage_identity(path: &Path) -> Option<(String, String)> {
    let name = path.file_name()?.to_str()?;
    let (manifest_id, durable_job_id_text) = name.split_once(ACTIVATION_STAGE_SEPARATOR)?;
    let durable_job_id = uuid::Uuid::parse_str(durable_job_id_text).ok()?;
    if validate_hash(manifest_id).is_err()
        || durable_job_id.hyphenated().to_string() != durable_job_id_text
    {
        return None;
    }
    Some((manifest_id.to_owned(), durable_job_id.to_string()))
}

fn unix_time_ms() -> Result<i64, PeerSyncError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| PeerSyncError::Storage("system time exceeds durable job range".to_owned()))
}

fn store_error(error: crate::persistent_store::StoreError) -> PeerSyncError {
    PeerSyncError::Storage(error.to_string())
}

fn lossless_error(error: LosslessError) -> PeerSyncError {
    match error.code {
        LosslessErrorCode::Cancelled => PeerSyncError::Cancelled,
        LosslessErrorCode::Store | LosslessErrorCode::Io => PeerSyncError::Storage(error.message),
        _ => PeerSyncError::Validation(error.message),
    }
}
