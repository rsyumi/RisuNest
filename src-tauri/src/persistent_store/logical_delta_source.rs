use super::{PersistentStore, StoreError};
use crate::{
    asset_repository::PayloadCas,
    peer_sync::{LogicalDeltaObject, LogicalDeltaObjectSource, PeerSyncError},
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    path::Path,
    sync::Arc,
};

pub(crate) struct LogicalDeltaSourceSession {
    store: PersistentStore,
    cas: PayloadCas,
    library_id: String,
    generation_id: String,
    session_id: String,
    active: bool,
    manifest_hash: String,
    manifest_bytes: Arc<[u8]>,
    objects: Vec<LogicalDeltaObject>,
    object_sizes: BTreeMap<String, u64>,
}

impl LogicalDeltaSourceSession {
    pub(crate) fn open(
        app_data_dir: &Path,
        repository_root: &Path,
        library_id: &str,
        generation_id: &str,
    ) -> Result<Self, PeerSyncError> {
        let cas = PayloadCas::new(repository_root)?;
        let mut store = PersistentStore::open(app_data_dir).map_err(map_store_error)?;
        let session_id = store
            .pin_logical_generation(library_id, generation_id)
            .map_err(map_store_error)?;
        Self::from_pinned(store, cas, library_id, generation_id, session_id)
    }

    pub(crate) fn resume(
        app_data_dir: &Path,
        repository_root: &Path,
        library_id: &str,
        generation_id: &str,
        session_id: &str,
    ) -> Result<Self, PeerSyncError> {
        let cas = PayloadCas::new(repository_root)?;
        let mut store = PersistentStore::open(app_data_dir).map_err(map_store_error)?;
        store
            .resume_logical_generation_pin(session_id, library_id, generation_id)
            .map_err(map_store_error)?;
        Self::from_pinned(store, cas, library_id, generation_id, session_id.to_owned())
    }

    fn from_pinned(
        mut store: PersistentStore,
        cas: PayloadCas,
        library_id: &str,
        generation_id: &str,
        session_id: String,
    ) -> Result<Self, PeerSyncError> {
        let built = match store.build_indexed_logical_manifest(library_id, generation_id) {
            Ok(built) => built,
            Err(error) => {
                let primary = map_store_error(error);
                return match store.release_logical_generation_pin(&session_id) {
                    Ok(()) => Err(primary),
                    Err(release) => Err(PeerSyncError::Storage(format!(
                        "{primary}; logical delta source pin release failed: {release}"
                    ))),
                };
            }
        };
        let objects = built
            .manifest
            .objects
            .iter()
            .map(|object| LogicalDeltaObject {
                hash: object.hash.clone(),
                size: object.size,
            })
            .collect::<Vec<_>>();
        let object_sizes = objects
            .iter()
            .map(|object| (object.hash.clone(), object.size))
            .collect();

        Ok(Self {
            store,
            cas,
            library_id: library_id.to_owned(),
            generation_id: generation_id.to_owned(),
            session_id,
            active: true,
            manifest_hash: built.manifest_hash,
            manifest_bytes: Arc::from(built.manifest_bytes),
            objects,
            object_sizes,
        })
    }

    pub(crate) fn manifest_hash(&self) -> &str {
        &self.manifest_hash
    }

    pub(crate) fn manifest_size(&self) -> u64 {
        self.manifest_bytes.len() as u64
    }

    pub(crate) fn objects(&self) -> &[LogicalDeltaObject] {
        &self.objects
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn open_manifest(&self) -> Result<Box<dyn Read>, PeerSyncError> {
        self.require_active()?;
        Ok(Box::new(Cursor::new(Arc::clone(&self.manifest_bytes))))
    }

    pub(crate) fn release(&mut self) -> Result<(), PeerSyncError> {
        if !self.active {
            return Ok(());
        }
        self.store
            .release_logical_generation_pin(&self.session_id)
            .map_err(map_store_error)?;
        self.active = false;
        Ok(())
    }

    fn require_active(&self) -> Result<(), PeerSyncError> {
        if self.active {
            Ok(())
        } else {
            Err(PeerSyncError::Validation(
                "logical delta source session has been released".to_owned(),
            ))
        }
    }
}

impl LogicalDeltaObjectSource for LogicalDeltaSourceSession {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError> {
        self.require_active()?;
        let expected_size = self.object_sizes.get(&object.hash).ok_or_else(|| {
            PeerSyncError::Validation(format!(
                "logical delta object {} is not present in the pinned manifest",
                object.hash
            ))
        })?;
        if *expected_size != object.size {
            return Err(PeerSyncError::Validation(format!(
                "logical delta object {} size does not match the pinned manifest",
                object.hash
            )));
        }

        if let Some(file) = self.cas.open_object(&object.hash)? {
            let actual_size = file.metadata()?.len();
            if actual_size != object.size {
                return Err(PeerSyncError::Validation(format!(
                    "logical delta CAS object {} size does not match the pinned manifest",
                    object.hash
                )));
            }
            return Ok(Box::new(file));
        }

        let bytes = self
            .store
            .reconstruct_logical_object(
                &self.cas,
                &self.library_id,
                &self.generation_id,
                &object.hash,
            )
            .map_err(map_store_error)?;
        Ok(Box::new(Cursor::new(bytes)))
    }
}

impl Drop for LogicalDeltaSourceSession {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn map_store_error(error: StoreError) -> PeerSyncError {
    match error {
        StoreError::Validation { message } => PeerSyncError::Validation(message),
        StoreError::SnapshotReleased => PeerSyncError::Validation(
            "logical delta source session is no longer available".to_owned(),
        ),
        other => PeerSyncError::Storage(other.to_string()),
    }
}
