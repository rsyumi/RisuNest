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
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

const UNIFIED_ACTIVE_SOURCE_MARKER_SCHEMA: &str = "risunest.peer-clone-active-source/v1";
const UNIFIED_ACTIVE_SOURCE_MARKER_FILE: &str = "active-source.json";
const MAX_UNIFIED_ACTIVE_SOURCE_MARKER_BYTES: u64 = 4 * 1024;
static UNIFIED_SOURCE_FILES: Mutex<()> = Mutex::new(());

fn lock_unified_source_files() -> Result<MutexGuard<'static, ()>, PeerSyncError> {
    UNIFIED_SOURCE_FILES.lock().map_err(|error| {
        PeerSyncError::Storage(format!(
            "shared clone source filesystem mutex is unavailable: {error}"
        ))
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnifiedActiveSourceMarker {
    schema: String,
    directory_id: String,
    session_id: String,
    manifest_id: String,
}

impl UnifiedActiveSourceMarker {
    fn validate(&self) -> Result<(), PeerSyncError> {
        if self.schema != UNIFIED_ACTIVE_SOURCE_MARKER_SCHEMA
            || uuid::Uuid::parse_str(&self.directory_id)
                .map(|parsed| parsed.to_string() != self.directory_id)
                .unwrap_or(true)
            || uuid::Uuid::parse_str(&self.session_id)
                .map(|parsed| parsed.to_string() != self.session_id)
                .unwrap_or(true)
            || super::protocol::validate_hash(&self.manifest_id).is_err()
        {
            return Err(PeerSyncError::Validation(
                "invalid prepared clone source marker".to_owned(),
            ));
        }
        Ok(())
    }
}

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

/// Hostless clone preparation shared by the unified desktop and Android
/// listeners. The listener takes the session, while this owner retains the
/// exact generated directory for deterministic lane cleanup.
pub(crate) struct PreparedUnifiedCloneSource {
    prepared: Option<PreparedCloneSession>,
    session_root: PathBuf,
    marker_path: PathBuf,
    marker: UnifiedActiveSourceMarker,
}

impl PreparedUnifiedCloneSource {
    pub(crate) fn directory_id(&self) -> Result<&str, PeerSyncError> {
        self.session_root
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                PeerSyncError::Storage(
                    "shared clone source directory identity is unavailable".to_owned(),
                )
            })
    }

    pub(crate) fn session_root(&self) -> &Path {
        &self.session_root
    }

    pub(crate) fn session_id(&self) -> Result<&str, PeerSyncError> {
        self.prepared
            .as_ref()
            .map(|prepared| prepared.manifest().session_id.as_str())
            .ok_or_else(|| {
                PeerSyncError::Protocol("shared clone source session is unavailable".to_owned())
            })
    }

    pub(crate) fn manifest_id(&self) -> Result<&str, PeerSyncError> {
        self.prepared
            .as_ref()
            .map(PreparedCloneSession::manifest_id)
            .ok_or_else(|| {
                PeerSyncError::Protocol("shared clone source session is unavailable".to_owned())
            })
    }

    pub(crate) fn take_session(&mut self) -> Result<PreparedCloneSession, PeerSyncError> {
        self.prepared.take().ok_or_else(|| {
            PeerSyncError::Protocol("shared clone source session is unavailable".to_owned())
        })
    }

    pub(crate) fn cleanup(&mut self) -> Result<(), PeerSyncError> {
        let _files = lock_unified_source_files()?;
        self.prepared.take();
        let marker = read_unified_source_marker(&self.marker_path)?;
        if marker.as_ref().is_some_and(|marker| marker != &self.marker) {
            return Err(PeerSyncError::Validation(
                "prepared clone source marker belongs to another session".to_owned(),
            ));
        }
        remove_unified_directory(&self.session_root)?;
        if marker.is_some() {
            fs::remove_file(&self.marker_path)?;
        }
        sweep_unified_source_directories(self.session_root.parent().ok_or_else(|| {
            PeerSyncError::Storage("shared clone source session has no owned parent".to_owned())
        })?)
    }
}

pub(crate) fn prepare_unified_clone_source(
    store: &mut PersistentStore,
    cas: &PayloadCas,
    peer_root: &Path,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedUnifiedCloneSource, PeerSyncError> {
    let _files = lock_unified_source_files()?;
    let operation_id = uuid::Uuid::new_v4().to_string();
    let preparation_parent = peer_root.join("source-preparation");
    let sessions_parent = peer_root.join("source-sessions");
    let preparation_root = preparation_parent.join(&operation_id);
    let session_root = sessions_parent.join(&operation_id);
    fs::create_dir_all(&preparation_parent)?;
    fs::create_dir_all(&sessions_parent)?;
    let expected_revision = store.revision().map_err(store_error)?;
    let prepared = prepare_lossless_clone_session(
        store,
        cas,
        expected_revision,
        &preparation_root,
        &session_root,
        cancellation,
    )
    .map_err(|error| match fs::remove_dir_all(&session_root) {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => error,
        Err(cleanup) => PeerSyncError::Storage(format!(
            "{error}; peer clone source session cleanup failed: {cleanup}"
        )),
    })?;
    let marker = UnifiedActiveSourceMarker {
        schema: UNIFIED_ACTIVE_SOURCE_MARKER_SCHEMA.to_owned(),
        directory_id: operation_id,
        session_id: prepared.manifest().session_id.clone(),
        manifest_id: prepared.manifest_id().to_owned(),
    };
    let marker_path = peer_root.join(UNIFIED_ACTIVE_SOURCE_MARKER_FILE);
    if let Err(error) = write_unified_source_marker(&marker_path, &marker) {
        let cleanup = remove_unified_directory(&session_root);
        return match cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(PeerSyncError::Storage(format!(
                "{error}; peer clone source session cleanup failed: {cleanup}"
            ))),
        };
    }
    Ok(PreparedUnifiedCloneSource {
        prepared: Some(prepared),
        session_root,
        marker_path,
        marker,
    })
}

fn unified_metadata_is_link(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn remove_unified_directory(path: &Path) -> Result<(), PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || unified_metadata_is_link(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone cleanup target is not an ordinary directory".to_owned(),
        ));
    }
    fs::remove_dir_all(path)?;
    Ok(())
}

fn sweep_unified_source_directories(parent: &Path) -> Result<(), PeerSyncError> {
    let metadata = match fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() || unified_metadata_is_link(&metadata) {
        return Err(PeerSyncError::Validation(
            "peer clone cleanup root is not an ordinary directory".to_owned(),
        ));
    }
    let canonical_parent = fs::canonicalize(parent)?;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if uuid::Uuid::parse_str(&name)
            .map(|value| value.to_string() != name)
            .unwrap_or(true)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() || unified_metadata_is_link(&metadata) {
            continue;
        }
        let canonical = fs::canonicalize(entry.path())?;
        if canonical.parent() != Some(canonical_parent.as_path()) {
            return Err(PeerSyncError::Validation(
                "peer clone cleanup target escaped its owned root".to_owned(),
            ));
        }
        fs::remove_dir_all(canonical)?;
    }
    Ok(())
}

fn read_unified_source_marker(
    path: &Path,
) -> Result<Option<UnifiedActiveSourceMarker>, PeerSyncError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file()
        || unified_metadata_is_link(&metadata)
        || metadata.len() > MAX_UNIFIED_ACTIVE_SOURCE_MARKER_BYTES
    {
        return Err(PeerSyncError::Validation(
            "invalid prepared clone source marker file".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_UNIFIED_ACTIVE_SOURCE_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_UNIFIED_ACTIVE_SOURCE_MARKER_BYTES {
        return Err(PeerSyncError::Validation(
            "prepared clone source marker exceeds its bound".to_owned(),
        ));
    }
    let marker: UnifiedActiveSourceMarker = serde_json::from_slice(&bytes)
        .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
    marker.validate()?;
    Ok(Some(marker))
}

fn write_unified_source_marker(
    path: &Path,
    marker: &UnifiedActiveSourceMarker,
) -> Result<(), PeerSyncError> {
    marker.validate()?;
    let bytes =
        serde_json::to_vec(marker).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() as u64 > MAX_UNIFIED_ACTIVE_SOURCE_MARKER_BYTES {
        return Err(PeerSyncError::Storage(
            "prepared clone source marker exceeds its bound".to_owned(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("prepared clone source marker has no parent".to_owned())
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".active-source-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        replace_unified_marker(&temporary, path)?;
        sync_unified_marker_parent(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(windows)]
fn replace_unified_marker(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
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
fn replace_unified_marker(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(unix)]
fn sync_unified_marker_parent(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_unified_marker_parent(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
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
    backup_observer: Option<&'a mut dyn FnMut(&Path) -> Result<(), PeerSyncError>>,
    backup_observer_stage: Option<(String, String)>,
    committed_backup_path: Option<PathBuf>,
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
        Self::initialize(
            store,
            cas,
            root,
            expected_revision,
            cancellation,
            None,
            None,
        )
    }

    pub(crate) fn new_with_backup_observer(
        store: &'a mut PersistentStore,
        cas: &'a PayloadCas,
        root: &Path,
        expected_revision: i64,
        cancellation: &'a dyn CancellationProbe,
        backup_observer: &'a mut dyn FnMut(&Path) -> Result<(), PeerSyncError>,
    ) -> Result<Self, PeerSyncError> {
        Self::initialize(
            store,
            cas,
            root,
            expected_revision,
            cancellation,
            Some(backup_observer),
            None,
        )
    }

    pub(crate) fn new_with_owned_backup_observer(
        store: &'a mut PersistentStore,
        cas: &'a PayloadCas,
        root: &Path,
        expected_revision: i64,
        cancellation: &'a dyn CancellationProbe,
        manifest_id: &str,
        durable_job_id: &str,
        backup_observer: &'a mut dyn FnMut(&Path) -> Result<(), PeerSyncError>,
    ) -> Result<Self, PeerSyncError> {
        validate_hash(manifest_id)?;
        let durable_job_id = uuid::Uuid::parse_str(durable_job_id)
            .map_err(|_| PeerSyncError::Validation("invalid peer clone durable job id".to_owned()))?
            .to_string();
        Self::initialize(
            store,
            cas,
            root,
            expected_revision,
            cancellation,
            Some(backup_observer),
            Some((manifest_id.to_owned(), durable_job_id)),
        )
    }

    fn initialize(
        store: &'a mut PersistentStore,
        cas: &'a PayloadCas,
        root: &Path,
        expected_revision: i64,
        cancellation: &'a dyn CancellationProbe,
        backup_observer: Option<&'a mut dyn FnMut(&Path) -> Result<(), PeerSyncError>>,
        backup_observer_stage: Option<(String, String)>,
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
            backup_observer,
            backup_observer_stage,
            committed_backup_path: None,
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

    pub(crate) fn committed_backup_path(&self) -> Option<&Path> {
        self.committed_backup_path.as_deref()
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
            let backup_path = self.expected_backup_path(manifest_id, durable_job_id);
            if self
                .backup_observer_stage
                .as_ref()
                .is_none_or(|owned| owned.0 == manifest_id && owned.1 == durable_job_id)
            {
                self.observe_committed_backup(&backup_path)?;
            }
            CasReleaseOutcome::Committed
        } else {
            CasReleaseOutcome::Aborted
        };
        job.release(outcome)?;
        self.remove_owned_directory(directory)?;
        Ok(true)
    }

    fn expected_backup_path(&self, manifest_id: &str, durable_job_id: &str) -> PathBuf {
        self.root
            .join("backups")
            .join(format!("pre-clone-{manifest_id}-{durable_job_id}.lossless"))
    }

    fn observe_committed_backup(&mut self, path: &Path) -> Result<(), PeerSyncError> {
        if self.committed_backup_path.as_deref() == Some(path) {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(PeerSyncError::Storage(
                "peer clone pre-replacement backup is not a regular file".to_owned(),
            ));
        }
        if let Some(observer) = self.backup_observer.as_deref_mut() {
            observer(path)?;
        }
        self.committed_backup_path = Some(path.to_owned());
        Ok(())
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
        let stage_id = self
            .backup_observer_stage
            .as_ref()
            .filter(|owned| owned.0 == manifest_id)
            .map(|owned| owned.1.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
                self.observe_committed_backup(&stage.pre_replacement_backup)?;
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
