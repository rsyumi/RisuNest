use super::{
    prepare_clone_session,
    protocol::{validate_hash, CloneObjectKind},
    CloneActivation, CloneSource, CloneTargetAdapter, PeerSyncError, PinnedCloneRevision,
    PinnedSourceObject, PreparedCloneSession,
};
use crate::{
    asset_repository::PayloadCas,
    local_backup::CancellationProbe,
    lossless_backup::{
        create_and_verify_lossless_backup_v1, restore_lossless_package_v1_with_app_kv,
        LosslessError, LosslessErrorCode,
    },
    persistent_store::PersistentStore,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const ACTIVE_MANIFEST_KEY: &str = "peerCloneActiveManifest";
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
}

pub(crate) struct LosslessCloneTargetAdapter<'a> {
    store: &'a mut PersistentStore,
    cas: &'a PayloadCas,
    root: PathBuf,
    expected_revision: i64,
    cancellation: &'a dyn CancellationProbe,
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
        Ok(Self {
            store,
            cas,
            root,
            expected_revision,
            cancellation,
        })
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
        let directory = match fs::canonicalize(&stage.directory) {
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
        let directory = self.root.join(&stage_id);
        fs::create_dir(&directory)?;
        Ok(LosslessCloneStage {
            directory,
            package: None,
            pre_replacement_backup: self
                .root
                .join("backups")
                .join(format!("pre-clone-{manifest_id}-{stage_id}.lossless")),
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
        self.remove_owned_stage(&stage)
    }

    fn activate_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_manifest_id: Option<&str>,
        new_manifest_id: &str,
    ) -> Result<CloneActivation, PeerSyncError> {
        validate_hash(new_manifest_id)?;
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
        let mut reader = File::open(package)?;
        match restore_lossless_package_v1_with_app_kv(
            &mut reader,
            &restore_staging,
            self.cas,
            self.store,
            self.expected_revision,
            &stage.pre_replacement_backup,
            ACTIVE_MANIFEST_KEY,
            &marker_value,
            self.cancellation,
        ) {
            Ok(_) => {
                self.remove_owned_stage(stage)?;
                Ok(CloneActivation::Activated)
            }
            Err(error) if error.code == LosslessErrorCode::RevisionConflict => {
                Ok(CloneActivation::Conflict {
                    actual: self.active_manifest_id()?,
                })
            }
            Err(error) => Err(lossless_error(error)),
        }
    }
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
