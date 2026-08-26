use super::PeerSyncError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const CLONE_CHUNK_SIZE: u64 = 8 * 1024 * 1024;
pub const CLONE_MANIFEST_SCHEMA: &str = "risunest.peer-clone/v1";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_OBJECTS: usize = 100_000;
const MAX_LOGICAL_KEY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloneObjectKind {
    Database,
    Asset,
    Inlay,
    Cold,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedChunk {
    pub offset: u64,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectDescriptor {
    pub size: u64,
    pub sha256: String,
    pub chunks: Vec<VerifiedChunk>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClonePayload {
    pub kind: CloneObjectKind,
    pub logical_key: String,
    pub metadata: serde_json::Value,
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneManifest {
    pub schema: String,
    pub session_id: String,
    pub source_revision: u64,
    pub chunk_size: u64,
    pub database: String,
    pub payloads: Vec<ClonePayload>,
    pub objects: BTreeMap<String, ObjectDescriptor>,
}

impl CloneManifest {
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, PeerSyncError> {
        let bytes =
            serde_json::to_vec(self).map_err(|error| PeerSyncError::Protocol(error.to_string()))?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PeerSyncError::Protocol(
                "clone manifest exceeds the bounded v1 size".to_owned(),
            ));
        }
        Ok(bytes)
    }

    pub fn identity(&self) -> Result<String, PeerSyncError> {
        Ok(sha256_hex(&self.canonical_bytes()?))
    }

    pub fn validate(&self) -> Result<(), PeerSyncError> {
        if self.schema != CLONE_MANIFEST_SCHEMA {
            return protocol_error("unsupported clone manifest schema");
        }
        if self.chunk_size != CLONE_CHUNK_SIZE {
            return protocol_error("clone manifest chunk size is not 8 MiB");
        }
        if self.session_id.is_empty() || self.session_id.len() > 128 {
            return protocol_error("invalid clone session identifier");
        }
        if self.objects.is_empty() || self.objects.len() > MAX_OBJECTS {
            return protocol_error("invalid clone object count");
        }
        if !self.objects.contains_key(&self.database) {
            return protocol_error("database object is missing from clone manifest");
        }

        let mut logical_keys = BTreeSet::new();
        for payload in &self.payloads {
            if payload.kind == CloneObjectKind::Database {
                return protocol_error("database cannot be listed as a payload alias");
            }
            if payload.logical_key.as_bytes().len() > MAX_LOGICAL_KEY_BYTES {
                return protocol_error("payload logical key exceeds the v1 limit");
            }
            if !logical_keys.insert((payload.kind, payload.logical_key.as_str())) {
                return protocol_error("duplicate payload logical key");
            }
            if !self.objects.contains_key(&payload.object) {
                return protocol_error("payload object is missing from clone manifest");
            }
        }

        for (object_hash, object) in &self.objects {
            validate_hash(object_hash)?;
            validate_hash(&object.sha256)?;
            if object_hash != &object.sha256 {
                return protocol_error("object map key and whole-object hash differ");
            }
            let expected_chunks = if object.size == 0 {
                0
            } else {
                object.size.div_ceil(CLONE_CHUNK_SIZE) as usize
            };
            if object.chunks.len() != expected_chunks {
                return protocol_error("object chunk count does not cover its exact size");
            }
            let mut expected_offset = 0_u64;
            for (index, chunk) in object.chunks.iter().enumerate() {
                validate_hash(&chunk.sha256)?;
                if chunk.offset != expected_offset {
                    return protocol_error("object chunks are not contiguous");
                }
                let remaining = object.size - expected_offset;
                let expected_size = remaining.min(CLONE_CHUNK_SIZE);
                if chunk.size != expected_size || chunk.size == 0 {
                    return protocol_error("object chunk has a noncanonical size");
                }
                if index + 1 != object.chunks.len() && chunk.size != CLONE_CHUNK_SIZE {
                    return protocol_error("nonfinal clone chunk is not 8 MiB");
                }
                expected_offset = expected_offset.checked_add(chunk.size).ok_or_else(|| {
                    PeerSyncError::Protocol("clone chunk offset overflow".to_owned())
                })?;
            }
            if expected_offset != object.size {
                return protocol_error("object chunks do not cover the object size");
            }
        }
        self.canonical_bytes()?;
        Ok(())
    }

    pub fn object(&self, hash: &str) -> Result<&ObjectDescriptor, PeerSyncError> {
        self.objects
            .get(hash)
            .ok_or_else(|| PeerSyncError::Protocol("object is absent from manifest".to_owned()))
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn validate_hash(hash: &str) -> Result<(), PeerSyncError> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    protocol_error("SHA-256 must be 64 lowercase hexadecimal characters")
}

fn protocol_error<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Protocol(message.to_owned()))
}
