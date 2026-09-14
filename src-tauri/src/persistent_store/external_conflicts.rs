//! Durable conflict choices first pin the immutable local snapshot, then bind
//! the authenticated remote snapshot before a destructive choice is allowed.
//!
//! The encrypted remote documents remain in provider storage, while the PDS
//! keeps their authenticated references and pins the local capture. A choice
//! cannot become destructive until preservation is complete or after either
//! the local preview or remote head has changed.
use super::{sync_selection, PersistentStore, StoreError, StoreResult};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const MAX_REFERENCE_BYTES: usize = 256 * 1024;
const MAX_OBSERVATION_BYTES: usize = 16 * 1024;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.contains('\0')
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConflictRecord {
    pub id: String,
    pub connection_id: String,
    pub repository_id: String,
    pub local_capture_id: String,
    /// Canonical JSON for the authenticated, complete RemoteObject once its
    /// resumable upload has finished. The capture remains pinned before this
    /// reference exists.
    pub local_snapshot: Option<String>,
    pub local_identity: sync_selection::CaptureIdentity,
    /// Canonical JSON for the other authenticated RemoteObject once observed.
    pub remote_snapshot: Option<String>,
    pub remote_logical_revision: Option<i64>,
    pub remote_commit_id: Option<String>,
    /// Canonical JSON produced from control::ObservedHead::observation.
    pub remote_head_observation: Option<String>,
    pub created_at_ms: i64,
    pub preservation: ConflictPreservation,
    pub phase: ConflictPhase,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictPreservation {
    LocalOnly,
    RemoteComplete,
}
impl ConflictPreservation {
    fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "localOnly",
            Self::RemoteComplete => "remoteComplete",
        }
    }
    fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "localOnly" => Ok(Self::LocalOnly),
            "remoteComplete" => Ok(Self::RemoteComplete),
            _ => Err(invalid("Invalid external conflict preservation")),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ConflictPhase {
    Pending,
    Resolving,
    PublicationUnknown,
    Resolved,
}
impl ConflictPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolving => "resolving",
            Self::PublicationUnknown => "publicationUnknown",
            Self::Resolved => "resolved",
        }
    }
    fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "resolving" => Ok(Self::Resolving),
            "publicationUnknown" => Ok(Self::PublicationUnknown),
            "resolved" => Ok(Self::Resolved),
            _ => Err(invalid("Invalid external conflict phase")),
        }
    }
}

fn validate(record: &ConflictRecord) -> StoreResult<()> {
    let local_complete = record
        .local_snapshot
        .as_deref()
        .is_some_and(|value| bounded(value, MAX_REFERENCE_BYTES));
    let remote_absent = record.remote_snapshot.is_none()
        && record.remote_logical_revision.is_none()
        && record.remote_commit_id.is_none()
        && record.remote_head_observation.is_none();
    let remote_complete = record
        .remote_snapshot
        .as_deref()
        .is_some_and(|value| {
            bounded(value, MAX_REFERENCE_BYTES)
                && record.local_snapshot.as_deref() != Some(value)
        })
        && record.remote_logical_revision.is_some_and(|value| value >= 0)
        && record
            .remote_commit_id
            .as_deref()
            .is_some_and(|value| bounded(value, 1024))
        && record
            .remote_head_observation
            .as_deref()
            .is_some_and(|value| bounded(value, MAX_OBSERVATION_BYTES));
    let remote_bound = record
        .remote_snapshot
        .as_deref()
        .is_some_and(|value| {
            bounded(value, MAX_REFERENCE_BYTES)
                && record.local_snapshot.as_deref() != Some(value)
        })
        && record.remote_logical_revision.is_none()
        && record
            .remote_commit_id
            .as_deref()
            .is_some_and(|value| bounded(value, 1024))
        && record
            .remote_head_observation
            .as_deref()
            .is_some_and(|value| bounded(value, MAX_OBSERVATION_BYTES));
    if !bounded(&record.id, 1024)
        || !bounded(&record.connection_id, 1024)
        || !bounded(&record.repository_id, 1024)
        || !bounded(&record.local_capture_id, 1024)
        || record.created_at_ms < 0
        || match record.preservation {
            ConflictPreservation::LocalOnly => {
                (record.local_snapshot.is_some() && !local_complete)
                    || (!remote_absent && !remote_bound && !remote_complete)
            }
            ConflictPreservation::RemoteComplete => !local_complete || !remote_complete,
        }
    {
        return Err(invalid("Incomplete external conflict preservation"));
    }
    Ok(())
}

fn decode_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
    i64,
    String,
    String,
)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}
fn from_row(
    row: (
        String,
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        i64,
        String,
        String,
    ),
) -> StoreResult<ConflictRecord> {
    let (
        id,
        connection_id,
        repository_id,
        local_capture_id,
        local_snapshot,
        local_identity,
        remote_snapshot,
        remote_logical_revision,
        remote_commit_id,
        remote_head_observation,
        created_at_ms,
        preservation,
        phase,
    ) = row;
    let record = ConflictRecord {
        id,
        connection_id,
        repository_id,
        local_capture_id,
        local_snapshot,
        local_identity: serde_json::from_str(&local_identity)?,
        remote_snapshot,
        remote_logical_revision,
        remote_commit_id,
        remote_head_observation,
        created_at_ms,
        preservation: ConflictPreservation::parse(&preservation)?,
        phase: ConflictPhase::parse(&phase)?,
    };
    validate(&record)?;
    Ok(record)
}

impl PersistentStore {
    /// Both snapshot references must already be authenticated and remotely
    /// complete. This transaction pins the local capture before exposing the
    /// conflict to a renderer.
    pub(crate) fn external_record_conflict(&mut self, record: &ConflictRecord) -> StoreResult<()> {
        self.external_record_conflict_with_pause(record, false, true)
    }
    pub(crate) fn external_record_conflict_exit_drain(
        &mut self,
        record: &ConflictRecord,
    ) -> StoreResult<()> {
        self.external_record_conflict_with_pause(record, true, true)
    }
    pub(crate) fn external_record_local_conflict(
        &mut self,
        record: &ConflictRecord,
    ) -> StoreResult<()> {
        self.external_record_conflict_with_pause(record, false, false)
    }
    pub(crate) fn external_record_local_conflict_exit_drain(
        &mut self,
        record: &ConflictRecord,
    ) -> StoreResult<()> {
        self.external_record_conflict_with_pause(record, true, false)
    }
    /// Settles a provider response that already proved the prepared head could
    /// not publish. A pause arriving after that response must not discard the
    /// capture that now needs conflict preservation.
    pub(crate) fn external_record_local_conflict_after_rejection(
        &mut self,
        record: &ConflictRecord,
    ) -> StoreResult<()> {
        self.external_record_conflict_with_pause(record, true, false)
    }
    fn external_record_conflict_with_pause(
        &mut self,
        record: &ConflictRecord,
        allow_paused: bool,
        remotely_complete: bool,
    ) -> StoreResult<()> {
        validate(record)?;
        if record.phase != ConflictPhase::Pending
            || (record.preservation == ConflictPreservation::RemoteComplete) != remotely_complete
        {
            return Err(invalid("A new external conflict must be pending"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if allow_paused {
            sync_selection::require_publish_exit_drain(
                &tx,
                &record.local_identity,
                &record.connection_id,
            )?;
        } else {
            sync_selection::require_publish(&tx, &record.local_identity, &record.connection_id)?;
        }
        let identity: Option<String> = tx
            .query_row(
                "SELECT identity FROM external_storage_captures WHERE id=?1",
                [&record.local_capture_id],
                |row| row.get(0),
            )
            .optional()?;
        if identity.as_deref() != Some(serde_json::to_string(&record.local_identity)?.as_str()) {
            return Err(invalid("Conflict capture identity differs"));
        }
        let prepared: bool = tx.query_row(
            if remotely_complete {
                "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE id=?1 AND connection_id=?2 AND capture_id=?3 AND role='sync' AND phase='ready')"
            } else {
                "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE id=?1 AND connection_id=?2 AND capture_id=?3 AND role='sync' AND phase IN ('ready','publishing'))"
            },
            params![record.id,record.connection_id,record.local_capture_id], |row| row.get(0))?;
        if !prepared {
            return Err(invalid(
                "Conflict preservation has no prepared local snapshot",
            ));
        }
        tx.execute(
            "INSERT INTO external_storage_conflicts VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                record.id,
                record.connection_id,
                record.repository_id,
                record.local_capture_id,
                record.local_snapshot,
                serde_json::to_string(&record.local_identity)?,
                record.remote_snapshot,
                record.remote_logical_revision,
                record.remote_commit_id,
                record.remote_head_observation,
                record.created_at_ms,
                record.preservation.as_str(),
                record.phase.as_str()
            ],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO external_storage_capture_refs VALUES(?1,?2)",
            params![record.local_capture_id, record.id],
        )?;
        tx.execute(
            if remotely_complete {
                "UPDATE external_storage_jobs SET phase='stale' WHERE id=?1"
            } else {
                "UPDATE external_storage_jobs SET phase='conflictPreserving' WHERE id=?1"
            },
            [&record.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn external_conflict(&self, id: &str) -> StoreResult<Option<ConflictRecord>> {
        let row=self.connection.query_row(
            "SELECT id,connection_id,repository_id,local_capture_id,local_snapshot,local_identity,remote_snapshot,remote_logical_revision,remote_commit_id,remote_head_observation,created_at_ms,preservation,phase FROM external_storage_conflicts WHERE id=?1",
            [id], decode_row).optional()?;
        row.map(from_row).transpose()
    }

    pub(crate) fn external_conflicts(&self, connection: &str) -> StoreResult<Vec<ConflictRecord>> {
        let mut statement=self.connection.prepare(
            "SELECT id,connection_id,repository_id,local_capture_id,local_snapshot,local_identity,remote_snapshot,remote_logical_revision,remote_commit_id,remote_head_observation,created_at_ms,preservation,phase FROM external_storage_conflicts WHERE connection_id=?1 AND phase!='resolved' ORDER BY created_at_ms,id")?;
        let rows = statement.query_map([connection], decode_row)?;
        rows.map(|row| from_row(row?)).collect()
    }

    /// The caller has freshly authenticated `remote_observation`. The selected
    /// preview remains valid only while the exact local capture identity is
    /// still current.
    pub(crate) fn external_begin_conflict_resolution(
        &mut self,
        id: &str,
        remote_observation: &str,
    ) -> StoreResult<ConflictRecord> {
        if !bounded(remote_observation, MAX_OBSERVATION_BYTES) {
            return Err(invalid("Missing authenticated remote observation"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row=tx.query_row(
            "SELECT id,connection_id,repository_id,local_capture_id,local_snapshot,local_identity,remote_snapshot,remote_logical_revision,remote_commit_id,remote_head_observation,created_at_ms,preservation,phase FROM external_storage_conflicts WHERE id=?1",
            [id],decode_row).optional()?.ok_or_else(||invalid("External conflict does not exist"))?;
        let mut record = from_row(row)?;
        if record.phase != ConflictPhase::Pending
            || record.preservation != ConflictPreservation::RemoteComplete
            || record.remote_head_observation.as_deref() != Some(remote_observation)
            || sync_selection::identity(&tx)? != record.local_identity
        {
            return Err(invalid("External conflict preview became stale"));
        }
        sync_selection::require_publish(&tx, &record.local_identity, &record.connection_id)?;
        tx.execute("UPDATE external_storage_conflicts SET phase='resolving' WHERE id=?1 AND phase='pending'",[id])?;
        tx.commit()?;
        record.phase = ConflictPhase::Resolving;
        Ok(record)
    }

    pub(crate) fn external_complete_conflict_preservation(
        &mut self,
        id: &str,
        remote_snapshot: &str,
        remote_logical_revision: i64,
        remote_commit_id: &str,
        remote_head_observation: &str,
    ) -> StoreResult<ConflictRecord> {
        self.external_complete_conflict_preservation_with_pause(
            id,
            remote_snapshot,
            remote_logical_revision,
            remote_commit_id,
            remote_head_observation,
            false,
        )
    }

    pub(crate) fn external_bind_local_conflict_snapshot(
        &mut self,
        id: &str,
        local_snapshot: &str,
    ) -> StoreResult<ConflictRecord> {
        self.external_bind_local_conflict_snapshot_with_pause(id, local_snapshot, false)
    }

    pub(crate) fn external_bind_local_conflict_snapshot_exit_drain(
        &mut self,
        id: &str,
        local_snapshot: &str,
    ) -> StoreResult<ConflictRecord> {
        self.external_bind_local_conflict_snapshot_with_pause(id, local_snapshot, true)
    }

    fn external_bind_local_conflict_snapshot_with_pause(
        &mut self,
        id: &str,
        local_snapshot: &str,
        allow_paused: bool,
    ) -> StoreResult<ConflictRecord> {
        if !bounded(local_snapshot, MAX_REFERENCE_BYTES) {
            return Err(invalid("Incomplete local conflict snapshot"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT id,connection_id,repository_id,local_capture_id,local_snapshot,local_identity,remote_snapshot,remote_logical_revision,remote_commit_id,remote_head_observation,created_at_ms,preservation,phase FROM external_storage_conflicts WHERE id=?1",
                [id],
                decode_row,
            )
            .optional()?
            .ok_or_else(|| invalid("External conflict does not exist"))?;
        let mut record = from_row(row)?;
        if record.phase != ConflictPhase::Pending
            || record.preservation != ConflictPreservation::LocalOnly
            || record
                .remote_snapshot
                .as_deref()
                .is_some_and(|remote| remote == local_snapshot)
        {
            return Err(invalid("No local conflict upload to bind"));
        }
        if record.local_snapshot.as_deref() == Some(local_snapshot) {
            return Ok(record);
        }
        if record.local_snapshot.is_some() {
            return Err(invalid("Local conflict upload changed before binding"));
        }
        if allow_paused {
            sync_selection::require_publish_exit_drain(
                &tx,
                &record.local_identity,
                &record.connection_id,
            )?;
        } else {
            sync_selection::require_publish(&tx, &record.local_identity, &record.connection_id)?;
        }
        if tx.execute(
            "UPDATE external_storage_conflicts SET local_snapshot=?2 WHERE id=?1 AND local_snapshot IS NULL AND preservation='localOnly' AND phase='pending'",
            params![id, local_snapshot],
        )? != 1
        {
            return Err(invalid("Local conflict upload changed before binding"));
        }
        tx.commit()?;
        record.local_snapshot = Some(local_snapshot.into());
        validate(&record)?;
        Ok(record)
    }

    pub(crate) fn external_complete_conflict_preservation_exit_drain(
        &mut self,
        id: &str,
        remote_snapshot: &str,
        remote_logical_revision: i64,
        remote_commit_id: &str,
        remote_head_observation: &str,
    ) -> StoreResult<ConflictRecord> {
        self.external_complete_conflict_preservation_with_pause(
            id,
            remote_snapshot,
            remote_logical_revision,
            remote_commit_id,
            remote_head_observation,
            true,
        )
    }

    fn external_complete_conflict_preservation_with_pause(
        &mut self,
        id: &str,
        remote_snapshot: &str,
        remote_logical_revision: i64,
        remote_commit_id: &str,
        remote_head_observation: &str,
        allow_paused: bool,
    ) -> StoreResult<ConflictRecord> {
        if !bounded(remote_snapshot, MAX_REFERENCE_BYTES)
            || remote_logical_revision < 0
            || !bounded(remote_commit_id, 1024)
            || !bounded(remote_head_observation, MAX_OBSERVATION_BYTES)
        {
            return Err(invalid("Incomplete remote conflict preservation"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT id,connection_id,repository_id,local_capture_id,local_snapshot,local_identity,remote_snapshot,remote_logical_revision,remote_commit_id,remote_head_observation,created_at_ms,preservation,phase FROM external_storage_conflicts WHERE id=?1",
                [id],
                decode_row,
            )
            .optional()?
            .ok_or_else(|| invalid("External conflict does not exist"))?;
        let mut record = from_row(row)?;
        if record.phase != ConflictPhase::Pending
            || record.preservation != ConflictPreservation::LocalOnly
            || record.local_snapshot.is_none()
            || record.local_snapshot.as_deref() == Some(remote_snapshot)
            || record
                .remote_snapshot
                .as_deref()
                .is_some_and(|value| value != remote_snapshot)
            || record
                .remote_commit_id
                .as_deref()
                .is_some_and(|value| value != remote_commit_id)
            || record
                .remote_head_observation
                .as_deref()
                .is_some_and(|value| value != remote_head_observation)
        {
            return Err(invalid("No local-only conflict preservation to complete"));
        }
        if allow_paused {
            sync_selection::require_publish_exit_drain(
                &tx,
                &record.local_identity,
                &record.connection_id,
            )?;
        } else {
            sync_selection::require_publish(&tx, &record.local_identity, &record.connection_id)?;
        }
        if tx.execute(
            "UPDATE external_storage_conflicts SET remote_snapshot=?2,remote_logical_revision=?3,remote_commit_id=?4,remote_head_observation=?5,preservation='remoteComplete' WHERE id=?1 AND preservation='localOnly' AND phase='pending'",
            params![id, remote_snapshot, remote_logical_revision, remote_commit_id, remote_head_observation],
        )? != 1
        {
            return Err(invalid("Conflict preservation changed before completion"));
        }
        if tx.execute(
            "UPDATE external_storage_jobs SET phase='stale' WHERE id=?1 AND role='sync' AND phase='conflictPreserving'",
            [id],
        )? != 1
        {
            return Err(invalid("Conflict preservation job changed before completion"));
        }
        tx.commit()?;
        record.remote_snapshot = Some(remote_snapshot.into());
        record.remote_logical_revision = Some(remote_logical_revision);
        record.remote_commit_id = Some(remote_commit_id.into());
        record.remote_head_observation = Some(remote_head_observation.into());
        record.preservation = ConflictPreservation::RemoteComplete;
        validate(&record)?;
        Ok(record)
    }

    pub(crate) fn external_conflict_publication_unknown(&mut self, id: &str) -> StoreResult<()> {
        if self.connection.execute("UPDATE external_storage_conflicts SET phase='publicationUnknown' WHERE id=?1 AND phase='resolving'",[id])?!=1 {
            return Err(invalid("No resolving external conflict"));
        }
        Ok(())
    }

    /// A verified pre-write rejection leaves both preserved snapshots intact.
    /// The old preview remains listed, while a later normal sync may preserve
    /// a new remote side if the head changed.
    pub(crate) fn external_reject_conflict_resolution(&mut self, id: &str) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        if tx.execute("UPDATE external_storage_conflicts SET phase='pending' WHERE id=?1 AND phase='resolving'",[id])?!=1 {
            return Err(invalid("No resolving external conflict"));
        }
        if tx.execute("UPDATE external_storage_jobs SET phase='cancelled' WHERE id=?1 AND phase IN ('ready','stale')",[id])?!=1 {
            return Err(invalid("Conflict resolution was not rejected before publication"));
        }
        tx.commit()?;
        Ok(())
    }

    /// Call only after the chosen remote publication or local activation is
    /// confirmed in PDS. No automatic retention or physical remote deletion is
    /// performed here.
    pub(crate) fn external_finish_conflict(&mut self, id: &str) -> StoreResult<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let capture:Option<String>=tx.query_row(
            "SELECT local_capture_id FROM external_storage_conflicts WHERE id=?1 AND phase IN ('resolving','publicationUnknown')",
            [id],|row|row.get(0)).optional()?;
        let capture = capture.ok_or_else(|| invalid("No active external conflict resolution"))?;
        tx.execute(
            "UPDATE external_storage_conflicts SET phase='resolved' WHERE id=?1",
            [id],
        )?;
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE capture_id=?1 AND job_id=?2",
            params![capture, id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn phase_parser_rejects_unversioned_or_unknown_state() {
        assert_eq!(
            ConflictPhase::parse("pending").unwrap(),
            ConflictPhase::Pending
        );
        assert!(ConflictPhase::parse("finished").is_err());
    }
}
