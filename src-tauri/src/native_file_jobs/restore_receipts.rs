use super::*;
use crate::persistent_store::{PersistentStore, StoreError, StoreResult};

const KEY: &str = "native-file-restore-receipts.v1";

// This is an operation-to-request mapping. The LWW intent and activation receipt
// remain the only evidence that replacement was accepted or committed.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Receipt {
    job_id: String,
    staging_id: String,
    authority: String,
    archive: bool,
    source_bytes: u64,
    source_sha256: String,
    character_count: u64,
    preset_count: u64,
    warning_codes: Vec<String>,
}

fn read(store: &PersistentStore) -> StoreResult<Vec<Receipt>> {
    store.device_store()?.read_setting(KEY)?.map(serde_json::from_value).transpose().map(|rows| rows.unwrap_or_default()).map_err(StoreError::from)
}

pub(super) fn prepare(store: &PersistentStore, job: &JobControl, staging_id: &str, result: &JobResultSummary) -> StoreResult<Option<String>> {
    let kind = job.status().kind;
    if !matches!(kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup) { return Ok(None); }
    let authority = store.lww_binding_authority()?.0.to_string();
    let mut rows = read(store)?;
    rows.retain(|row| row.job_id != job.id());
    rows.push(Receipt {
        job_id: job.id(), staging_id: staging_id.to_owned(), authority: authority.clone(),
        archive: kind == JobKind::RestoreLegacyLocalBackup,
        source_bytes: result.source_bytes, source_sha256: result.source_sha256.clone(),
        character_count: result.character_count, preset_count: result.preset_count, warning_codes: result.warning_codes.clone(),
    });
    store.device_store()?.write_setting(KEY, &serde_json::to_value(rows)?)?;
    Ok(Some(authority))
}

pub(super) fn forget(store: &PersistentStore, registry: &JobRegistry, id: &str) -> StoreResult<()> {
    let mut rows = read(store)?;
    // A later intent replay still needs this mapping to recover the sync hold and acknowledgement.
    if rows.iter().filter(|row| row.job_id == id).any(|row| store.lww_file_restore_receipt(&row.staging_id, &row.staging_id, &row.authority).is_err()) {
        registry.hidden_restore_receipts.lock().map_err(|error| StoreError::Store { message: error.to_string() })?.insert(id.to_owned());
        return Ok(());
    }
    let before = rows.len();
    rows.retain(|row| row.job_id != id);
    if before == rows.len() { return Ok(()); }
    if rows.is_empty() { store.device_store()?.remove_setting(KEY) }
    else { store.device_store()?.write_setting(KEY, &serde_json::to_value(rows)?) }
}

pub(super) fn reconcile(store: &PersistentStore, registry: &JobRegistry) -> StoreResult<()> {
    reconcile_matching(store, registry, None)
}

pub(super) fn reconcile_job(store: &PersistentStore, registry: &JobRegistry, job_id: &str) -> StoreResult<()> {
    reconcile_matching(store, registry, Some(job_id))
}

fn reconcile_matching(store: &PersistentStore, registry: &JobRegistry, job_id: Option<&str>) -> StoreResult<()> {
    for receipt in read(store)? {
        if registry.hidden_restore_receipts.lock().map_err(|error| StoreError::Store { message: error.to_string() })?.contains(&receipt.job_id) { continue; }
        if job_id.is_some_and(|id| id != receipt.job_id) { continue; }
        let existing = registry.lookup(&receipt.job_id).map_err(|message| StoreError::Store { message })?;
        if existing.as_ref().is_some_and(|job| {
            let status = job.status();
            !status.state.is_terminal() || matches!(status.state, JobState::Succeeded | JobState::Cancelled)
        }) { continue; }
        let outcome = store.lww_file_restore_receipt(&receipt.staging_id, &receipt.staging_id, &receipt.authority);
        let revision = outcome.as_ref().ok().copied().flatten();
        if outcome.is_ok() && revision.is_none() && existing.is_some() { continue; }
        let kind = if receipt.archive { JobKind::RestoreLegacyLocalBackup } else { JobKind::RestoreBlockRisuSave };
        let job = match existing {
            Some(job) => job,
            None => {
                let job = registry.create_internal(kind, None, vec![], false).map_err(|message| StoreError::Store { message })?;
                let temporary_id = job.id();
                job.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).job_id = receipt.job_id.clone();
                let mut jobs = registry.jobs.lock().map_err(|error| StoreError::Store { message: error.to_string() })?;
                jobs.remove(&temporary_id);
                jobs.insert(receipt.job_id.clone(), job.clone());
                job
            }
        };
        let mut status = job.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        status.phase = JobPhase::Complete;
        if let Err(error) = outcome {
            status.state = JobState::Failed;
            status.activation_revision = None;
            status.activation_authority = None;
            status.result = None;
            status.error = Some(JobFailure { code: "activation-outcome-unresolved".into(), message: error.to_string(), item_kind: None, owner: None });
        } else if let Some(revision) = revision {
            status.state = JobState::Succeeded;
            status.error = None;
            status.activation_revision = Some(revision);
            status.activation_authority = Some(receipt.authority);
            status.result = Some(JobResultSummary {
                revision, source_bytes: receipt.source_bytes, source_sha256: receipt.source_sha256,
                source_fingerprint_kind: SourceFingerprintKind::WholeFileSha256,
                character_count: receipt.character_count, preset_count: receipt.preset_count,
                warning_codes: receipt.warning_codes, export_exclusions: None, handoff_path: None, publication: None,
            });
        } else {
            status.state = JobState::Cancelled;
            status.error = Some(JobFailure { code: "cancelled".into(), message: "Backup preparation was interrupted before activation".into(), item_kind: None, owner: None });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stage(store: &mut PersistentStore) -> String {
        let id = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&id, &json!({"username":"Synthetic restore", "modules":[], "plugins":[], "loadouts":[], "pluginCustomStorage":{}})).unwrap();
        store.replace_put_presets(&id, &[]).unwrap();
        id
    }

    fn remember(store: &PersistentStore, id: &str) -> String {
        let job = JobRegistry::default().create(JobKind::RestoreBlockRisuSave).unwrap();
        prepare(store, &job, id, &JobResultSummary {
            revision: 0, source_bytes: 321, source_sha256: "ab".repeat(32),
            source_fingerprint_kind: SourceFingerprintKind::WholeFileSha256,
            character_count: 0, preset_count: 0, warning_codes: vec!["skipped-backup-data:2".into()],
            export_exclusions: None, handoff_path: None, publication: None,
        }).unwrap();
        job.id()
    }

    #[test]
    fn receipt_listing_defers_until_the_store_is_open_and_device_maintenance_finishes() {
        use crate::persistent_store::{commands::with_store_if_ready, PersistentStoreState};
        let unopened = PersistentStoreState::default();
        assert!(with_store_if_ready::<()>(&unopened, |_| panic!("Closed store was accessed")).unwrap().is_none());
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = stage(&mut store);
        let id = remember(&store, &stage);
        store.replace_commit(&stage, Some(0)).unwrap();
        let state = PersistentStoreState::with_test_store(store);
        let registry = JobRegistry::default();
        let maintenance = state.acquire_device_maintenance().unwrap();
        assert!(with_store_if_ready(&state, |store| reconcile(store, &registry)).unwrap().is_none());
        assert!(registry.list().unwrap().is_empty());
        drop(maintenance);
        assert!(with_store_if_ready(&state, |store| reconcile(store, &registry)).unwrap().is_none());
        let state = PersistentStoreState::with_test_store(PersistentStore::open(directory.path()).unwrap());
        assert_eq!(with_store_if_ready(&state, |store| reconcile(store, &registry)).unwrap(), Some(()));
        assert_eq!(registry.status(&id).unwrap().activation_revision, Some(1));
    }

    #[test]
    fn process_restart_recovers_the_exact_commit_without_reactivation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = stage(&mut store);
        let id = remember(&store, &stage);
        store.replace_commit(&stage, Some(0)).unwrap();
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        let registry = JobRegistry::default();
        for _ in 0..2 {
            reconcile(&store, &registry).unwrap();
            let status = registry.status(&id).unwrap();
            assert_eq!(status.state, JobState::Succeeded);
            assert_eq!(status.activation_revision, Some(1));
            assert_eq!(status.result.as_ref().unwrap().source_sha256, "ab".repeat(32));
            assert_eq!(status.result.as_ref().unwrap().warning_codes, vec!["skipped-backup-data:2"]);
            assert_eq!(store.revision().unwrap(), 1);
        }
        let mut rows = read(&store).unwrap();
        let authority = rows[0].authority.clone();
        rows[0].authority = "999".into();
        store.device_store().unwrap().write_setting(KEY, &serde_json::to_value(&rows).unwrap()).unwrap();
        let job = registry.lookup(&id).unwrap().unwrap();
        let succeeded = job.status();
        for state in [JobState::Succeeded, JobState::Cancelled] {
            let mut status = succeeded.clone();
            status.state = state;
            if state != JobState::Succeeded {
                status.activation_revision = None;
                status.activation_authority = None;
                status.result = None;
                status.error = Some(JobFailure { code: "cancelled".into(), message: "Confirmed before activation".into(), item_kind: None, owner: None });
            }
            *job.status.lock().unwrap() = status.clone();
            reconcile(&store, &registry).unwrap();
            assert_eq!(serde_json::to_value(job.status()).unwrap(), serde_json::to_value(status).unwrap(), "Confirmed {state:?} changed after receipt lookup failure");
        }
        rows[0].authority = authority;
        store.device_store().unwrap().write_setting(KEY, &serde_json::to_value(rows).unwrap()).unwrap();
        forget(&store, &registry, &id).unwrap();
        let restarted = JobRegistry::default();
        reconcile(&store, &restarted).unwrap();
        assert!(restarted.list().unwrap().is_empty());
    }

    #[test]
    fn an_unrelated_higher_revision_is_not_a_restore_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let abandoned = stage(&mut store);
        let id = remember(&store, &abandoned);
        let other = stage(&mut store);
        store.replace_commit(&other, Some(0)).unwrap();
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        let registry = JobRegistry::default();
        reconcile(&store, &registry).unwrap();
        let status = registry.status(&id).unwrap();
        assert_eq!(status.state, JobState::Cancelled);
        assert_eq!(status.activation_revision, None);
        assert!(status.result.is_none());
        assert_eq!(store.revision().unwrap(), 1);
        let job = registry.lookup(&id).unwrap().unwrap();
        let mut failed = status;
        failed.state = JobState::Failed;
        failed.error = Some(JobFailure { code: "store-error".into(), message: "Confirmed no activation".into(), item_kind: None, owner: None });
        *job.status.lock().unwrap() = failed.clone();
        reconcile_job(&store, &registry, &id).unwrap();
        assert_eq!(serde_json::to_value(job.status()).unwrap(), serde_json::to_value(failed).unwrap());
    }

    #[test]
    fn a_receipt_with_different_binding_authority_requires_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = stage(&mut store);
        remember(&store, &stage);
        store.replace_commit(&stage, Some(0)).unwrap();
        let mut rows = read(&store).unwrap();
        rows[0].authority = "999".into();
        store.device_store().unwrap().write_setting(KEY, &serde_json::to_value(rows).unwrap()).unwrap();
        let registry = JobRegistry::default();
        let valid_stage = self::stage(&mut store);
        let valid_id = remember(&store, &valid_stage);
        store.replace_commit(&valid_stage, Some(1)).unwrap();
        reconcile(&store, &registry).unwrap();
        let jobs = registry.list().unwrap();
        assert_eq!(jobs.len(), 2);
        let unresolved = jobs.iter().find(|job| job.job_id != valid_id).unwrap();
        assert_eq!(unresolved.state, JobState::Failed);
        assert_eq!(unresolved.error.as_ref().unwrap().code, "activation-outcome-unresolved");
        assert_eq!(registry.status(&valid_id).unwrap().state, JobState::Succeeded);
        let unresolved_id = unresolved.job_id.clone();
        registry.forget(&unresolved_id).unwrap();
        forget(&store, &registry, &unresolved_id).unwrap();
        reconcile(&store, &registry).unwrap();
        assert_eq!(registry.list().unwrap().len(), 1);
        assert_eq!(read(&store).unwrap().len(), 2);
        let restarted = JobRegistry::default();
        reconcile(&store, &restarted).unwrap();
        assert_eq!(restarted.list().unwrap().len(), 2);
        assert_eq!(store.revision().unwrap(), 2);
    }

    #[test]
    fn accepted_disk_failure_stays_unresolved_until_restart_replays_the_same_intent() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = stage(&mut store);
        let id = remember(&store, &stage);
        let connection = rusqlite::Connection::open(directory.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_restore BEFORE UPDATE ON generations BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;").unwrap();
        assert!(store.replace_commit(&stage, Some(0)).is_err());
        let unresolved_registry = JobRegistry::default();
        reconcile_job(&store, &unresolved_registry, &id).unwrap();
        assert_eq!(unresolved_registry.status(&id).unwrap().error.unwrap().code, "activation-outcome-unresolved");
        let registry = JobRegistry::default();
        let failed = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        let temporary_id = failed.id();
        failed.status.lock().unwrap().job_id = id.clone();
        registry.jobs.lock().unwrap().remove(&temporary_id);
        registry.jobs.lock().unwrap().insert(id.clone(), failed.clone());
        failed.finish_failure("store-error", "Synthetic accepted commit failure").unwrap();
        reconcile_job(&store, &registry, &id).unwrap();
        let unresolved = registry.status(&id).unwrap();
        assert_eq!(unresolved.state, JobState::Failed);
        assert_eq!(unresolved.error.unwrap().code, "activation-outcome-unresolved");
        assert!(unresolved.result.is_none());
        assert_eq!(unresolved.activation_revision, None);
        assert_eq!(store.revision().unwrap(), 0);
        connection.execute_batch("DROP TRIGGER reject_restore").unwrap();
        drop(connection);
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        reconcile_job(&store, &unresolved_registry, &id).unwrap();
        assert_eq!(unresolved_registry.status(&id).unwrap().state, JobState::Succeeded);
        assert_eq!(unresolved_registry.status(&id).unwrap().activation_revision, Some(1));
        reconcile_job(&store, &registry, &id).unwrap();
        assert_eq!(registry.status(&id).unwrap().state, JobState::Succeeded);
        assert_eq!(registry.status(&id).unwrap().activation_revision, Some(1));
        assert_eq!(store.revision().unwrap(), 1);
        reconcile_job(&store, &registry, &id).unwrap();
        assert_eq!(store.revision().unwrap(), 1);
    }

    #[test]
    fn a_restore_intent_closed_without_activation_stays_dismissed_after_restart() {
        use crate::persistent_store::lww::UnitMutation;
        use crate::persistent_store::WorkingSetCommit;
        use risunest_sync_wire::unit::UnitKey;
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = stage(&mut store);
        let id = remember(&store, &stage);
        let authority = read(&store).unwrap()[0].authority.clone();
        // The activation's library write fails after its intent is reserved, as in the disk failure above.
        let connection = rusqlite::Connection::open(directory.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_restore BEFORE UPDATE ON generations BEGIN SELECT RAISE(ABORT,'synthetic disk failure'); END;").unwrap();
        assert!(store.replace_commit(&stage, Some(0)).is_err());
        connection.execute_batch("DROP TRIGGER reject_restore").unwrap();
        // Earlier builds drained deferred receive rows without replaying intents first, which moved
        // the revision past the pending restore's base.
        connection.execute("UPDATE meta SET value='1' WHERE key='currentRevision'", []).unwrap();
        drop(connection);
        assert_eq!(store.revision().unwrap(), 1);
        // The next save replays the restore intent first; it now conflicts and is closed without changes.
        store.commit(&WorkingSetCommit {
            expected_revision: 1,
            unit_mutations: Some(vec![UnitMutation::Set { key: UnitKey::new(&["root", "language"]).unwrap(), value: json!("ja") }]),
            ..Default::default()
        }).unwrap();
        assert_eq!(store.revision().unwrap(), 2);
        let complete: bool = store.device_store().unwrap().connection()
            .query_row("SELECT complete FROM lww_intents WHERE request_id=?1", [&stage], |row| row.get(0)).unwrap();
        let activated: bool = store.library_rows()
            .query_row("SELECT EXISTS(SELECT 1 FROM lww_requests WHERE request_id=?1)", [&stage], |row| row.get(0)).unwrap();
        assert!(complete && !activated);
        assert_ne!(store.read_root(None).unwrap().value["username"], "Synthetic restore");
        // Nothing can activate this restore any more, so startup recovery reports it as interrupted.
        let registry = JobRegistry::default();
        reconcile(&store, &registry).unwrap();
        let interrupted = registry.status(&id).unwrap();
        assert!(matches!(interrupted.state, JobState::Failed | JobState::Cancelled) && interrupted.activation_revision.is_none());
        // Startup recovery dismisses the interrupted restore the way native_file_job_forget does.
        registry.forget(&id).unwrap();
        forget(&store, &registry, &id).unwrap();
        drop(store);
        let store = PersistentStore::open(directory.path()).unwrap();
        let restarted = JobRegistry::default();
        reconcile(&store, &restarted).unwrap();
        let jobs = restarted.list().unwrap();
        assert!(jobs.is_empty(), "A dismissed restore that can never activate came back after restart: {:?}, receipt lookup {:?}",
            jobs.iter().map(|job| (job.job_id.clone(), job.error.as_ref().map(|error| error.code.clone()))).collect::<Vec<_>>(),
            store.lww_file_restore_receipt(&stage, &stage, &authority).map_err(|error| error.to_string()));
        assert_eq!(read(&store).unwrap().len(), 0);
    }
}
