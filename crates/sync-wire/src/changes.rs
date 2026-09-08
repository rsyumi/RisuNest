use crate::{canonical, hash, validate_hash, validate_id, RemoteHead, Result, Sequence, WireError};
use serde::{Deserialize, Serialize};

pub const MAX_KEY_BYTES: usize = 64 * 1024;
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
pub const MAX_PAGE_RECORDS: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum RecordVersion {
    Absent,
    Live {
        #[serde(rename = "objectHash")]
        object_hash: String,
        dependencies: Vec<String>,
    },
    Tombstone {
        #[serde(rename = "deletionId")]
        deletion_id: String,
    },
}
// Serde's internally tagged *unit* variant ignores surplus fields even when the
// enum denies unknown fields. Deserialize absent through an empty struct variant.
impl<'de> Deserialize<'de> for RecordVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
        enum Input {
            Absent {},
            Live {
                #[serde(rename = "objectHash")]
                object_hash: String,
                dependencies: Vec<String>,
            },
            Tombstone {
                #[serde(rename = "deletionId")]
                deletion_id: String,
            },
        }
        Ok(match Input::deserialize(d)? {
            Input::Absent {} => Self::Absent,
            Input::Live {
                object_hash,
                dependencies,
            } => Self::Live {
                object_hash,
                dependencies,
            },
            Input::Tombstone { deletion_id } => Self::Tombstone { deletion_id },
        })
    }
}
impl RecordVersion {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Absent => Ok(()),
            Self::Tombstone { deletion_id } => validate_id(deletion_id),
            Self::Live {
                object_hash,
                dependencies,
            } => {
                validate_hash(object_hash)?;
                for hash in dependencies {
                    validate_hash(hash)?;
                }
                if dependencies.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(WireError("unordered-dependencies"));
                }
                Ok(())
            }
        }
    }
    pub fn object_hashes(&self) -> Vec<&str> {
        match self {
            Self::Live {
                object_hash,
                dependencies,
            } => std::iter::once(object_hash.as_str())
                .chain(dependencies.iter().map(String::as_str))
                .collect(),
            _ => Vec::new(),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordChange {
    pub key: String,
    pub before: RecordVersion,
    pub after: RecordVersion,
}

/// Exact record observations include parent/owner reads even when not written.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFence {
    pub key: String,
    pub version: RecordVersion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeSet {
    pub changes: Vec<RecordChange>,
    pub read_fences: Vec<ReadFence>,
}
impl ChangeSet {
    pub fn validate(&self) -> Result<()> {
        if self.changes.is_empty()
            || self.changes.len() > MAX_PAGE_RECORDS
            || self.read_fences.len() > MAX_PAGE_RECORDS
        {
            return Err(WireError("invalid-change-count"));
        }
        let valid_key = |key: &str| !key.is_empty() && key.len() <= MAX_KEY_BYTES;
        for change in &self.changes {
            if !valid_key(&change.key)
                || change.after == RecordVersion::Absent
                || change.before == change.after
            {
                return Err(WireError("invalid-change"));
            }
            change.before.validate()?;
            change.after.validate()?;
        }
        for fence in &self.read_fences {
            if !valid_key(&fence.key) {
                return Err(WireError("invalid-key"));
            }
            fence.version.validate()?;
        }
        if self.changes.windows(2).any(|w| w[0].key >= w[1].key)
            || self.read_fences.windows(2).any(|w| w[0].key >= w[1].key)
        {
            return Err(WireError("unordered-keys"));
        }
        if canonical::encode(self)?.len() > MAX_METADATA_BYTES {
            return Err(WireError("metadata-too-large"));
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(hash(&canonical::encode(self)?))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitIntent {
    pub device_operation_seq: Sequence,
    pub expected_head: RemoteHead,
    pub changes_digest: String,
    pub staged_changes_id: String,
}
impl CommitIntent {
    pub fn validate(&self) -> Result<()> {
        self.expected_head.validate()?;
        validate_hash(&self.changes_digest)?;
        validate_id(&self.staged_changes_id)?;
        if self.device_operation_seq == Sequence::from(0) {
            return Err(WireError("invalid-operation-sequence"));
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        // Locator and transport choices are deliberately excluded from the logical intent.
        Ok(hash(&canonical::encode(&serde_json::json!({
            "domain": "risunest-sync-intent-v1", "expectedHead": self.expected_head,
            "changesDigest": self.changes_digest
        }))?))
    }
}
pub fn operation_id(library: &str, device: &str, seq: &Sequence) -> Result<String> {
    validate_id(library)?;
    validate_id(device)?;
    Ok(hash(&canonical::encode(&[
        "risunest-sync-operation-v1",
        library,
        device,
        seq.as_str(),
    ])?))
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: String,
    pub device_operation_seq: Sequence,
    pub intent_digest: String,
    pub status: TerminalStatus,
    pub head: RemoteHead,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalStatus {
    Committed,
    Stale,
    Failed,
}
