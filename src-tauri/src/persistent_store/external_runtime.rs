//! Short authoritative operations called between native network stages.
use super::{
    external_storage_state as jobs, sync_selection, PersistentStore, StoreError, StoreResult,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use crate::external_storage::publication::PublicationPermit;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExternalBase {
    pub repository_id: String,
    pub snapshot_id: String,
    pub commit_id: String,
    pub head_observation: String,
    pub identity: sync_selection::CaptureIdentity,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ExternalJob {
    pub id: String,
    pub connection_id: String,
    pub repository_id: String,
    pub capture_id: String,
    pub identity: sync_selection::CaptureIdentity,
    pub role: String,
    pub strategy: Option<String>,
    pub expected_head: Option<String>,
    pub commit_id: String,
    pub phase: String,
}
pub(crate) struct ExternalReceiveCompletion {
    pub snapshot_id: String,
    pub expected_revision: i64,
    pub revision: i64,
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capture(store: &mut PersistentStore) -> sync_selection::CaptureIdentity {
        let identity = store.external_identity().unwrap();
        let tx = store.connection.transaction().unwrap();
        jobs::register_capture(
            &tx,
            "capture",
            &identity,
            "scope",
            "logical-v1",
            "",
            &"a".repeat(64),
            "destination",
        )
        .unwrap();
        tx.commit().unwrap();
        identity
    }
    #[test]
    fn backup_completion_never_advances_or_replaces_the_sync_head_base() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let identity = capture(&mut store);
        let encoded = serde_json::to_string(&identity).unwrap();
        store.connection.execute("INSERT INTO external_storage_bases VALUES('destination','repository','sync-snapshot','sync-commit','authenticated-head',?1)",[encoded]).unwrap();
        store
            .external_prepare_backup("backup", "destination", "repository", "capture", "point")
            .unwrap();
        store
            .external_finish_backup("backup", "point", "backup-snapshot", "point-observation")
            .unwrap();
        let job = store.external_job("backup").unwrap().unwrap();
        assert_eq!(job.connection_id, "destination");
        assert_eq!(job.phase, "complete");
        assert!(store.external_job("missing").unwrap().is_none());
        let base = store.external_base("destination").unwrap().unwrap();
        assert_eq!(base.snapshot_id, "sync-snapshot");
        assert_eq!(base.head_observation, "authenticated-head");
        assert_eq!(
            store.external_backup_result("backup").unwrap(),
            Some(("backup-snapshot".into(), identity))
        );
        assert!(store
            .external_finish_backup("backup", "point", "different-snapshot", "point-observation")
            .is_err());
    }
    #[test]
    fn cancelling_a_prepared_failure_releases_the_destination_and_capture_pin() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        capture(&mut store);
        store
            .external_prepare_backup("failed", "destination", "repository", "capture", "point")
            .unwrap();
        assert!(store
            .external_prepare_backup("next", "destination", "repository", "capture", "point2")
            .is_err());
        store.external_cancel_prepared("failed").unwrap();
        let pins: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM external_storage_capture_refs WHERE job_id='failed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pins, 0);
        store
            .external_prepare_backup("next", "destination", "repository", "capture", "point2")
            .unwrap();
    }
}

impl PersistentStore {
    pub(crate) fn external_pin_history_record(
        &self,
        job: &str,
    ) -> StoreResult<Option<jobs::PinHistoryRecord>> {
        jobs::pin_history_record(&self.connection, job)
    }
    pub(crate) fn external_prepare_pin_history(
        &mut self,
        intent: &jobs::PinHistoryIntent<'_>,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::prepare_pin_history(&tx, intent)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_finish_pin_history(
        &mut self,
        job: &str,
        observation: &str,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::finish_pin_history(&tx, job, observation)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_backup_result(
        &self,
        job: &str,
    ) -> StoreResult<Option<(String, sync_selection::CaptureIdentity)>> {
        let row:Option<(String,String)>=self.connection.query_row("SELECT p.snapshot_id,p.identity FROM external_storage_backup_points p JOIN external_storage_jobs j ON j.id=p.job_id WHERE p.job_id=?1 AND j.phase='complete'",[job],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        row.map(|(snapshot, identity)| Ok((snapshot, serde_json::from_str(&identity)?)))
            .transpose()
    }
    pub(crate) fn external_receive_completion(
        &self,
        job: &str,
        connection: &str,
    ) -> StoreResult<Option<ExternalReceiveCompletion>> {
        let Some(completed) = self.external_job(job)?.filter(|item| {
            item.connection_id == connection && item.role == "restore" && item.phase == "complete"
        }) else {
            return Ok(None);
        };
        // The receive marker and the single revision increment share one transaction.
        // A later cycle may have replaced the connection's base already.
        let revision = completed.identity.revision.checked_add(1)
            .filter(|_| completed.identity.revision >= 0)
            .ok_or_else(|| invalid("Invalid completed receive revision"))?;
        Ok(Some(ExternalReceiveCompletion {
            snapshot_id: completed.capture_id,
            expected_revision: completed.identity.revision,
            revision,
        }))
    }

    pub(crate) fn external_validate_receive(
        &self,
        job: &str,
        connection: &str,
        expected: &sync_selection::CaptureIdentity,
        authenticated_head: &str,
    ) -> StoreResult<()> {
        let intent = self.external_job(job)?
            .ok_or_else(|| invalid("Missing prepared receive intent"))?;
        if intent.role != "restore" || intent.phase != "ready"
            || intent.connection_id != connection || intent.identity != *expected
            || intent.expected_head.as_deref() != Some(authenticated_head)
            || self.external_identity()? != *expected
        {
            return Err(invalid("Prepared receive identity changed"));
        }
        sync_selection::require_publish(&self.connection, expected, connection)?;
        sync_selection::require_no_pending_publication(&self.connection)
    }

    pub(crate) fn external_identity(&self) -> StoreResult<sync_selection::CaptureIdentity> {
        sync_selection::identity(&self.connection)
    }
    pub(crate) fn external_library_is_pristine(&self) -> StoreResult<bool> {
        let identity = sync_selection::identity(&self.connection)?;
        if identity.revision != 0 {
            return Ok(false);
        }
        Ok(self.connection.query_row(
            "SELECT value='{}'
             AND NOT EXISTS(SELECT 1 FROM bot_presets WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM characters WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM conversations WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM messages WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM plugin_storage WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM asset_aliases WHERE generation=?1)
             AND NOT EXISTS(SELECT 1 FROM asset_owner_heads WHERE generation=?1)
             FROM root WHERE generation=?1",
            [&identity.generation],
            |row| row.get(0),
        )?)
    }
    pub(crate) fn external_selection(&self) -> StoreResult<sync_selection::Selection> {
        sync_selection::read(&self.connection)
    }
    pub(crate) fn external_select(
        &mut self,
        epoch: &str,
        target: &sync_selection::SyncTarget,
    ) -> StoreResult<sync_selection::Selection> {
        let tx = self.connection.transaction()?;
        let selected = sync_selection::select(&tx, epoch, target)?;
        tx.commit()?;
        Ok(selected)
    }
    pub(crate) fn external_base(&self, connection: &str) -> StoreResult<Option<ExternalBase>> {
        let row: Option<(String,String,String,String,String)> = self.connection.query_row(
            "SELECT repository_id,snapshot_id,commit_id,head_observation,identity FROM external_storage_bases WHERE connection_id=?1", [connection],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
        row.map(
            |(repository_id, snapshot_id, commit_id, head_observation, identity)| {
                Ok(ExternalBase {
                    repository_id,
                    snapshot_id,
                    commit_id,
                    head_observation,
                    identity: serde_json::from_str(&identity)?,
                })
            },
        )
        .transpose()
    }
    pub(crate) fn external_jobs(&self, connection: &str) -> StoreResult<Vec<ExternalJob>> {
        let mut query = self.connection.prepare("SELECT id,repository_id,capture_id,identity,role,strategy,expected_head,commit_id,phase FROM external_storage_jobs WHERE connection_id=?1 ORDER BY rowid DESC")?;
        let mut rows = query.query([connection])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(ExternalJob {
                id: row.get(0)?,
                connection_id: connection.into(),
                repository_id: row.get(1)?,
                capture_id: row.get(2)?,
                identity: serde_json::from_str(&row.get::<_, String>(3)?)?,
                role: row.get(4)?,
                strategy: row.get(5)?,
                expected_head: row.get(6)?,
                commit_id: row.get(7)?,
                phase: row.get(8)?,
            });
        }
        Ok(result)
    }
    pub(crate) fn external_job(&self, id: &str) -> StoreResult<Option<ExternalJob>> {
        self.connection
            .query_row(
                "SELECT connection_id,repository_id,capture_id,identity,role,strategy,expected_head,commit_id,phase
                 FROM external_storage_jobs WHERE id=?1",
                [id],
                |row| {
                    let identity = row.get::<_, String>(3)?;
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        identity,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    connection_id,
                    repository_id,
                    capture_id,
                    identity,
                    role,
                    strategy,
                    expected_head,
                    commit_id,
                    phase,
                )| {
                    Ok(ExternalJob {
                        id: id.into(),
                        connection_id,
                        repository_id,
                        capture_id,
                        identity: serde_json::from_str(&identity)?,
                        role,
                        strategy,
                        expected_head,
                        commit_id,
                        phase,
                    })
                },
            )
            .transpose()
    }
    pub(crate) fn external_prepare_backup(
        &mut self,
        id: &str,
        connection: &str,
        repository: &str,
        capture: &str,
        point: &str,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::prepare_backup(&tx, id, connection, repository, capture, point)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_prepare_publication(
        &mut self,
        intent: &jobs::PublishIntent<'_>,
        permit: &PublicationPermit,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::prepare_publication(&tx, intent, permit)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_prepare_receive(
        &mut self,
        intent: &jobs::ReceiveIntent<'_>,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::prepare_receive(&tx, intent)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_begin_publication(
        &mut self,
        permit: &PublicationPermit,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::begin_publication(&tx, permit)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_publication_unknown(&mut self, job: &str) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::publication_unknown(&tx, job)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_confirm_publication(
        &mut self,
        permit: &PublicationPermit,
        commit: &str,
        snapshot: &str,
        observation: &str,
    ) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::confirm_publication(&tx, permit, commit, snapshot, observation)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_cancel_prepared(&mut self, job: &str) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        jobs::cancel_prepared(&tx, job)?;
        tx.commit()?;
        Ok(())
    }
    /// Called only while the file(true) admission permit is held and after the
    /// native runtime job for this connection has stopped.
    pub(crate) fn external_prepare_connection_removal(
        &mut self,
        connection: &str,
    ) -> StoreResult<()> {
        if connection.is_empty() {
            return Err(invalid("Missing external connection identity"));
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let unsettled:bool=tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE connection_id=?1 AND phase='applying')",
            [connection], |row| row.get(0))?;
        if unsettled {
            return Err(invalid(
                "Unsettled external operation blocks connection removal",
            ));
        }
        tx.execute(
            "UPDATE external_storage_jobs SET phase='publicationUnknown' WHERE connection_id=?1 AND phase='publishing'",
            [connection],
        )?;
        tx.execute(
            "UPDATE external_storage_jobs SET phase='cancelled' WHERE connection_id=?1 AND phase NOT IN ('complete','cancelled','publicationUnknown')",
            [connection],
        )?;
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE job_id IN (SELECT id FROM external_storage_jobs WHERE connection_id=?1 AND phase!='publicationUnknown')",
            [connection],
        )?;
        tx.execute(
            "DELETE FROM external_storage_bases WHERE connection_id=?1",
            [connection],
        )?;
        let selection = sync_selection::read(&tx)?;
        if selection.target == sync_selection::SyncTarget::External(connection.into()) {
            sync_selection::select(&tx, &selection.epoch, &sync_selection::SyncTarget::None)?;
        }
        super::content_change_index::remove_connection_consumer(&tx, connection)?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn external_finish_backup(
        &mut self,
        job: &str,
        point: &str,
        snapshot: &str,
        observation: &str,
    ) -> StoreResult<()> {
        if point.is_empty() || snapshot.is_empty() || observation.is_empty() {
            return Err(invalid("Missing confirmed backup point"));
        }
        let tx = self.connection.transaction()?;
        let (connection,repository,identity,expected,phase): (String,String,String,String,String) = tx.query_row(
            "SELECT connection_id,repository_id,identity,commit_id,phase FROM external_storage_jobs WHERE id=?1 AND role='backup'",[job],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
        if expected != point || phase != "ready" {
            return Err(invalid("Backup completion does not match its intent"));
        }
        tx.execute(
            "INSERT INTO external_storage_backup_points VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                job,
                connection,
                repository,
                snapshot,
                point,
                observation,
                identity
            ],
        )?;
        tx.execute(
            "UPDATE external_storage_jobs SET phase='complete' WHERE id=?1",
            [job],
        )?;
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE job_id=?1",
            [job],
        )?;
        tx.commit()?;
        Ok(())
    }
    /// A definite CAS rejection or pre-write sequential mismatch proves no
    /// remote mutation by this attempt.
    pub(crate) fn external_publication_rejected(&mut self, job: &str) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        if tx.execute("UPDATE external_storage_jobs SET phase='stale' WHERE id=?1 AND role='sync' AND phase IN ('ready','publishing')",[job])? != 1 {
            return Err(invalid("No definite publication rejection to settle"));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn external_accept_equivalent(
        &mut self,
        permit: &PublicationPermit,
        connection: &str,
        repository: &str,
        previous_commit: &str,
        previous_observation: &str,
        snapshot: &str,
        commit: &str,
        observation: &str,
        identity: &sync_selection::CaptureIdentity,
    ) -> StoreResult<()> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if permit.job_id().is_empty() || permit.selection_epoch() != identity.selection_epoch {
            return Err(invalid("Publication permit does not match equivalent head"));
        }
        match permit.mode() {
            crate::external_storage::publication::PublicationMode::Foreground => {
                sync_selection::require_publish(&tx, identity, connection)?;
            }
            crate::external_storage::publication::PublicationMode::ExitDrain => {
                sync_selection::require_publish_exit_drain(&tx, identity, connection)?;
            }
        }
        let current:Option<(String,String)>=tx.query_row(
            "SELECT commit_id,head_observation FROM external_storage_bases WHERE connection_id=?1 AND repository_id=?2",
            params![connection,repository],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if current != Some((previous_commit.into(), previous_observation.into())) {
            return Err(invalid("Equivalent sync base changed"));
        }
        tx.execute("UPDATE external_storage_bases SET snapshot_id=?2,commit_id=?3,head_observation=?4,identity=?5 WHERE connection_id=?1",
            params![connection,snapshot,commit,observation,serde_json::to_string(identity)?])?;
        tx.commit()?;
        Ok(())
    }
}
