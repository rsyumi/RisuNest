use super::{
    protocol::{
        sha256_hex, CloneDatabase, CloneManifest, CloneObjectKind, ClonePayload, ObjectDescriptor,
        VerifiedChunk, CLONE_CHUNK_SIZE, CLONE_DATABASE_FORMAT, CLONE_LOSSLESS_DATABASE_FORMAT,
        CLONE_MANIFEST_SCHEMA, MAX_MANIFEST_BYTES,
    },
    PeerSyncError,
};
use crate::asset_repository::PayloadCas;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub trait CloneSource {
    type Lease: PinnedCloneRevision;

    fn pin(&self) -> Result<Self::Lease, PeerSyncError>;
}

pub trait PinnedCloneRevision {
    fn source_revision(&self) -> u64;
    fn objects(&self) -> Result<Vec<PinnedSourceObject>, PeerSyncError>;
}

#[derive(Debug, Clone)]
pub struct PinnedSourceObject {
    pub kind: CloneObjectKind,
    pub logical_key: String,
    pub metadata: Value,
    pub path: PathBuf,
    database_format: Option<String>,
}

impl PinnedSourceObject {
    pub fn database(path: impl Into<PathBuf>) -> Self {
        Self {
            kind: CloneObjectKind::Database,
            logical_key: "database".to_owned(),
            metadata: Value::Null,
            path: path.into(),
            database_format: Some(CLONE_DATABASE_FORMAT.to_owned()),
        }
    }

    pub fn database_with_format(path: impl Into<PathBuf>, format: impl Into<String>) -> Self {
        Self {
            kind: CloneObjectKind::Database,
            logical_key: "database".to_owned(),
            metadata: Value::Null,
            path: path.into(),
            database_format: Some(format.into()),
        }
    }

    pub fn payload(
        kind: CloneObjectKind,
        logical_key: impl Into<String>,
        metadata: Value,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            kind,
            logical_key: logical_key.into(),
            metadata,
            path: path.into(),
            database_format: None,
        }
    }
}

#[derive(Debug)]
pub struct PreparedCloneSession {
    root: PathBuf,
    manifest: CloneManifest,
    manifest_bytes: Vec<u8>,
    manifest_id: String,
    served_objects: BTreeMap<String, String>,
}

impl PreparedCloneSession {
    pub(crate) fn reopen_lossless(
        session_root: impl AsRef<Path>,
        expected_session_id: &str,
        expected_manifest_id: &str,
    ) -> Result<Self, PeerSyncError> {
        if uuid::Uuid::parse_str(expected_session_id)
            .map(|value| value.to_string() != expected_session_id)
            .unwrap_or(true)
            || super::protocol::validate_hash(expected_manifest_id).is_err()
        {
            return recovery_validation("invalid prepared clone recovery identity");
        }
        let session_root = session_root.as_ref();
        let root_metadata = match fs::symlink_metadata(session_root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return recovery_validation("prepared clone session directory is missing")
            }
            Err(error) => return Err(error.into()),
        };
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return recovery_validation("prepared clone session root is not a real directory");
        }
        let root = fs::canonicalize(session_root)?;
        let manifest_path = root.join("manifest.json");
        let manifest_metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return recovery_validation("prepared clone manifest is missing")
            }
            Err(error) => return Err(error.into()),
        };
        if !manifest_metadata.is_file()
            || manifest_metadata.file_type().is_symlink()
            || manifest_metadata.len() > MAX_MANIFEST_BYTES as u64
        {
            return recovery_validation("prepared clone manifest file is invalid");
        }
        let mut manifest_bytes = Vec::with_capacity(manifest_metadata.len() as usize);
        File::open(&manifest_path)?
            .take(MAX_MANIFEST_BYTES as u64 + 1)
            .read_to_end(&mut manifest_bytes)?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return recovery_validation("prepared clone manifest exceeds its bound");
        }
        let manifest: CloneManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| PeerSyncError::Validation(error.to_string()))?;
        manifest.validate().map_err(recovery_contract_error)?;
        let canonical = manifest
            .canonical_bytes()
            .map_err(recovery_contract_error)?;
        if canonical != manifest_bytes {
            return recovery_validation("prepared clone manifest is not canonical JSON");
        }
        let manifest_id = sha256_hex(&manifest_bytes);
        if manifest.session_id != expected_session_id || manifest_id != expected_manifest_id {
            return recovery_validation(
                "prepared clone manifest identity does not match its marker",
            );
        }
        if manifest.database.format != CLONE_LOSSLESS_DATABASE_FORMAT
            || !manifest.payloads.is_empty()
            || manifest.objects.len() != 1
        {
            return recovery_validation(
                "prepared clone recovery requires exactly one lossless package",
            );
        }
        let descriptor = manifest
            .objects
            .get(&manifest.database.object)
            .ok_or_else(|| {
                PeerSyncError::Validation(
                    "prepared clone lossless package is absent from its manifest".to_owned(),
                )
            })?;
        let cas = PayloadCas::new(&root).map_err(recovery_io_error)?;
        let object_path = cas
            .object_path(&manifest.database.object)
            .map_err(recovery_io_error)?
            .ok_or_else(|| {
                PeerSyncError::Validation("prepared clone lossless package is missing".to_owned())
            })?;
        let object_size = fs::symlink_metadata(object_path)
            .map_err(recovery_io_error)?
            .len();
        if object_size != descriptor.size {
            return recovery_validation("prepared clone lossless package size changed");
        }
        let object_hash = descriptor.sha256.clone();
        Ok(Self {
            root,
            manifest,
            manifest_bytes,
            manifest_id,
            served_objects: BTreeMap::from([(object_hash.clone(), object_hash)]),
        })
    }

    pub fn manifest(&self) -> &CloneManifest {
        &self.manifest
    }

    pub fn manifest_bytes(&self) -> &[u8] {
        &self.manifest_bytes
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn physical_hash(&self, advertised_hash: &str) -> Option<&str> {
        self.served_objects.get(advertised_hash).map(String::as_str)
    }

    #[cfg(test)]
    pub fn force_whole_hash_mismatch_for_test(
        &mut self,
        payload_index: usize,
    ) -> Result<String, PeerSyncError> {
        let original = self
            .manifest
            .payloads
            .get(payload_index)
            .ok_or_else(|| PeerSyncError::Protocol("test payload index is invalid".to_owned()))?
            .object
            .clone();
        let advertised = sha256_hex(format!("whole-mismatch:{original}").as_bytes());
        let mut descriptor = self
            .manifest
            .objects
            .remove(&original)
            .ok_or_else(|| PeerSyncError::Protocol("test object is absent".to_owned()))?;
        descriptor.sha256 = advertised.clone();
        self.manifest.objects.insert(advertised.clone(), descriptor);
        for payload in &mut self.manifest.payloads {
            if payload.object == original {
                payload.object = advertised.clone();
            }
        }
        if self.manifest.database.object == original {
            self.manifest.database.object = advertised.clone();
        }
        let physical = self
            .served_objects
            .remove(&original)
            .ok_or_else(|| PeerSyncError::Protocol("test physical object is absent".to_owned()))?;
        self.served_objects.insert(advertised.clone(), physical);
        self.refresh_manifest_for_test()?;
        Ok(advertised)
    }

    #[cfg(test)]
    fn refresh_manifest_for_test(&mut self) -> Result<(), PeerSyncError> {
        self.manifest.validate()?;
        self.manifest_bytes = self.manifest.canonical_bytes()?;
        self.manifest_id = sha256_hex(&self.manifest_bytes);
        fs::write(self.root.join("manifest.json"), &self.manifest_bytes)?;
        Ok(())
    }
}

fn recovery_validation<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.to_owned()))
}

fn recovery_contract_error(error: PeerSyncError) -> PeerSyncError {
    PeerSyncError::Validation(error.to_string())
}

fn recovery_io_error(error: std::io::Error) -> PeerSyncError {
    if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
    ) {
        PeerSyncError::Validation(error.to_string())
    } else {
        PeerSyncError::Storage(error.to_string())
    }
}

pub fn prepare_clone_session<S: CloneSource>(
    source: &S,
    session_root: impl AsRef<Path>,
) -> Result<PreparedCloneSession, PeerSyncError> {
    let session_root = session_root.as_ref();
    let lease = source.pin()?;
    let result = prepare_pinned_revision(&lease, session_root);
    drop(lease);
    result
}

fn prepare_pinned_revision(
    lease: &impl PinnedCloneRevision,
    session_root: &Path,
) -> Result<PreparedCloneSession, PeerSyncError> {
    fs::create_dir_all(session_root)?;
    let root = fs::canonicalize(session_root)?;
    let cas = PayloadCas::new(&root)?;
    let source_objects = lease.objects()?;
    let mut database = None;
    let mut payloads = Vec::new();
    let mut objects = BTreeMap::new();
    let mut served_objects = BTreeMap::new();
    let mut aliases = BTreeSet::new();

    for source_object in source_objects {
        if source_object.kind != CloneObjectKind::Database
            && !aliases.insert((source_object.kind, source_object.logical_key.clone()))
        {
            return Err(PeerSyncError::Protocol(
                "pinned source contains a duplicate payload alias".to_owned(),
            ));
        }
        let file = File::open(&source_object.path)?;
        let mut chunked = ChunkHashingReader::new(file);
        let prepared = cas.prepare_reader(&mut chunked)?;
        let chunks = chunked.finish();
        let descriptor = ObjectDescriptor {
            size: prepared.byte_size,
            sha256: prepared.content_hash.clone(),
            chunks,
        };
        if let Some(existing) = objects.insert(prepared.content_hash.clone(), descriptor.clone()) {
            if existing != descriptor {
                return Err(PeerSyncError::Protocol(
                    "equal object hash produced unequal descriptors".to_owned(),
                ));
            }
        }
        served_objects.insert(prepared.content_hash.clone(), prepared.content_hash.clone());

        if source_object.kind == CloneObjectKind::Database {
            let format = source_object.database_format.ok_or_else(|| {
                PeerSyncError::Protocol("pinned database has no declared format".to_owned())
            })?;
            if database.replace((prepared.content_hash, format)).is_some() {
                return Err(PeerSyncError::Protocol(
                    "pinned source must contain exactly one database object".to_owned(),
                ));
            }
        } else {
            payloads.push(ClonePayload {
                kind: source_object.kind,
                logical_key: source_object.logical_key,
                metadata: source_object.metadata,
                object: prepared.content_hash,
            });
        }
    }

    let manifest = CloneManifest {
        schema: CLONE_MANIFEST_SCHEMA.to_owned(),
        session_id: uuid::Uuid::new_v4().to_string(),
        source_revision: lease.source_revision(),
        created_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| PeerSyncError::Protocol(error.to_string()))?,
        chunk_size: CLONE_CHUNK_SIZE,
        database: {
            let (object, format) = database.ok_or_else(|| {
                PeerSyncError::Protocol(
                    "pinned source must contain exactly one database object".to_owned(),
                )
            })?;
            CloneDatabase { format, object }
        },
        payloads,
        objects,
    };
    manifest.validate()?;
    let manifest_bytes = manifest.canonical_bytes()?;
    let manifest_id = sha256_hex(&manifest_bytes);
    write_immutable_manifest(&root, &manifest_bytes)?;
    Ok(PreparedCloneSession {
        root,
        manifest,
        manifest_bytes,
        manifest_id,
        served_objects,
    })
}

fn write_immutable_manifest(root: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    let temporary = root.join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let final_path = root.join("manifest.json");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    match fs::rename(&temporary, &final_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}

struct ChunkHashingReader<R> {
    inner: R,
    chunk_hasher: Sha256,
    chunks: Vec<VerifiedChunk>,
    chunk_offset: u64,
    chunk_size: u64,
    finished: bool,
}

impl<R> ChunkHashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            chunk_hasher: Sha256::new(),
            chunks: Vec::new(),
            chunk_offset: 0,
            chunk_size: 0,
            finished: false,
        }
    }

    fn finish(mut self) -> Vec<VerifiedChunk> {
        self.finish_current_chunk();
        self.chunks
    }

    fn finish_current_chunk(&mut self) {
        if self.chunk_size == 0 {
            return;
        }
        let hash = std::mem::take(&mut self.chunk_hasher).finalize();
        self.chunks.push(VerifiedChunk {
            offset: self.chunk_offset,
            size: self.chunk_size,
            sha256: hex::encode(hash),
        });
        self.chunk_offset += self.chunk_size;
        self.chunk_size = 0;
    }
}

impl<R: Read> Read for ChunkHashingReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.finished || output.is_empty() {
            return Ok(0);
        }
        let remaining = (CLONE_CHUNK_SIZE - self.chunk_size) as usize;
        let limit = output.len().min(remaining);
        let read = self.inner.read(&mut output[..limit])?;
        if read == 0 {
            self.finished = true;
            self.finish_current_chunk();
            return Ok(0);
        }
        self.chunk_hasher.update(&output[..read]);
        self.chunk_size += read as u64;
        if self.chunk_size == CLONE_CHUNK_SIZE {
            self.finish_current_chunk();
        }
        Ok(read)
    }
}
