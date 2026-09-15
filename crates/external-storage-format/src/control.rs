//! Canonical encrypted head and backup-point payloads.
use super::{
    snapshot::{ObjectRole, StoredObject},
    FormatError, Result,
};
use serde::{Deserialize, Serialize};

const HEAD_SCHEMA: &str = "risunest.external-head/v1";
const POINT_SCHEMA: &str = "risunest.external-backup-point/v1";
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeadDocument {
    pub schema: String,
    pub repository_id: String,
    pub library_id: String,
    pub commit_id: String,
    pub parent_commit_id: Option<String>,
    pub scope_id: [u8; 32],
    pub fingerprint: [u8; 32],
    pub snapshot: StoredObject,
}

impl HeadDocument {
    pub fn new(
        repository_id: String,
        library_id: String,
        commit_id: String,
        parent_commit_id: Option<String>,
        scope_id: [u8; 32],
        fingerprint: [u8; 32],
        snapshot: StoredObject,
    ) -> Result<Self> {
        let value = Self {
            schema: HEAD_SCHEMA.into(),
            repository_id,
            library_id,
            commit_id,
            parent_commit_id,
            scope_id,
            fingerprint,
            snapshot,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != HEAD_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.library_id.is_empty()
            || self.library_id.len() > 1024
            || self.commit_id.is_empty()
            || self.commit_id.len() > 1024
            || self
                .parent_commit_id
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 1024)
        {
            return Err(FormatError("invalid-head"));
        }
        self.snapshot.validate()?;
        if self.snapshot.header.role != ObjectRole::Snapshot
            || self.snapshot.header.repository_id != self.repository_id
        {
            return Err(FormatError("invalid-head-snapshot"));
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-head")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-head")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-head"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BackupPointKind {
    Backup,
    History,
    Manual,
    Conflict,
}

impl BackupPointKind {
    pub fn validate_snapshot_ids<'a>(
        self,
        snapshot_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut snapshot_ids = snapshot_ids.into_iter();
        let first = snapshot_ids.next();
        let second = snapshot_ids.next();
        let third = snapshot_ids.next();
        match self {
            Self::Conflict if first.is_none() || second.is_none() || third.is_some() => {
                return Err(FormatError("invalid-backup-point"));
            }
            Self::Conflict if first == second => {
                return Err(FormatError("duplicate-conflict-snapshot"));
            }
            Self::Conflict => {}
            _ if first.is_none() || second.is_some() => {
                return Err(FormatError("invalid-backup-point"));
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupPointDocument {
    pub schema: String,
    pub repository_id: String,
    pub point_id: String,
    pub kind: BackupPointKind,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub scope_id: [u8; 32],
    pub snapshots: Vec<StoredObject>,
}

impl BackupPointDocument {
    pub fn new(
        repository_id: String,
        point_id: String,
        kind: BackupPointKind,
        created_at_ms: u64,
        logical_revision: u64,
        scope_id: [u8; 32],
        snapshots: Vec<StoredObject>,
    ) -> Result<Self> {
        let value = Self {
            schema: POINT_SCHEMA.into(),
            repository_id,
            point_id,
            kind,
            created_at_ms,
            logical_revision,
            scope_id,
            snapshots,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != POINT_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.point_id.is_empty()
            || self.point_id.len() > 1024
            || self.logical_revision > i64::MAX as u64
        {
            return Err(FormatError("invalid-backup-point"));
        }
        self.kind
            .validate_snapshot_ids(
                self.snapshots
                    .iter()
                    .map(|snapshot| snapshot.header.object_id.as_str()),
            )?;
        for snapshot in &self.snapshots {
            snapshot.validate()?;
            if snapshot.header.role != ObjectRole::Snapshot
                || snapshot.header.repository_id != self.repository_id
            {
                return Err(FormatError("invalid-backup-point-snapshot"));
            }
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-backup-point")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-backup-point")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-backup-point"));
        }
        Ok(value)
    }
}

fn encode(value: &impl Serialize, max_bytes: usize, error: &'static str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|_| FormatError(error))?;
    if bytes.is_empty() || bytes.len() > max_bytes.min(MAX_CONTROL_BYTES) {
        return Err(FormatError("control-limit-exceeded"));
    }
    Ok(bytes)
}
fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    max_bytes: usize,
    error: &'static str,
) -> Result<T> {
    if bytes.is_empty() || bytes.len() > max_bytes.min(MAX_CONTROL_BYTES) {
        return Err(FormatError("control-limit-exceeded"));
    }
    serde_json::from_slice(bytes).map_err(|_| FormatError(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{PublicObjectHeader, WireLocator};
    fn stored(id: &str) -> StoredObject {
        let header =
            PublicObjectHeader::new("repository".into(), id.into(), ObjectRole::Snapshot, 10)
                .unwrap();
        StoredObject {
            ciphertext_length: crate::snapshot::envelope_length(&header).unwrap(),
            ciphertext_sha256: [2; 32],
            plaintext_length: 10,
            plaintext_sha256: [3; 32],
            locator: WireLocator {
                connection_identity: "account/root".into(),
                collection: None,
                object: format!("opaque-{id}"),
            },
            header,
        }
    }
    #[test]
    fn control_documents_are_canonical_and_conflicts_require_two_distinct_snapshots() {
        let head = HeadDocument::new(
            "repository".into(),
            "library".into(),
            "commit".into(),
            None,
            [4; 32],
            [5; 32],
            stored("snapshot-a"),
        )
        .unwrap();
        let encoded = head.encode(4096).unwrap();
        assert_eq!(HeadDocument::decode(&encoded, 4096).unwrap(), head);
        assert!(BackupPointDocument::new(
            "repository".into(),
            "point".into(),
            BackupPointKind::Conflict,
            1,
            7,
            [4; 32],
            vec![stored("snapshot-a")]
        )
        .is_err());
        let point = BackupPointDocument::new(
            "repository".into(),
            "point".into(),
            BackupPointKind::Conflict,
            1,
            7,
            [4; 32],
            vec![stored("snapshot-a"), stored("snapshot-b")],
        )
        .unwrap();
        assert_eq!(
            BackupPointDocument::decode(&point.encode(8192).unwrap(), 8192).unwrap(),
            point
        );
    }
}
