use super::{
    protocol::{
        sha256_hex, CloneDatabase, CloneManifest, CloneObjectKind, ClonePayload, ObjectDescriptor,
        VerifiedChunk, CLONE_CHUNK_SIZE, CLONE_DATABASE_FORMAT, CLONE_MANIFEST_SCHEMA,
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
    // Test fixtures pin objects with the default formats.
    #[cfg_attr(not(test), allow(dead_code))]
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

    #[cfg_attr(not(test), allow(dead_code))]
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
