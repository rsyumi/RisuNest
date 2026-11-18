use super::journal_frame::{self, JournalFrameLabels};
use super::migration_gc::AssetRootSet;
use super::{PayloadCas, PreparedPayload};
use crate::persistent_store::asset_object_catalog::{
    AssetObjectRegistration, ASSET_OBJECT_CATALOG_MAX_PAGE,
};
use crate::persistent_store::PersistentStore;
use crate::trust_boundary::{is_link_like, is_lower_hex_256, sync_directory};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const DURABLE_CAS_JOB_VERSION: u32 = 1;
const MAX_DURABLE_CAS_JOB_JOURNALS: usize = 4_096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CasJobKind {
    DirectAssetOrInlayWrite,
    LocalBackupRestore,
    CardOrModuleContentImport,
    OfficialPublicationOrExportPreparation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CasObjectRole {
    DirectObject,
    OwnerManifest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CasReleaseOutcome {
    Committed,
    Aborted,
}

/// What started a journal, so a later sweep can tell whether it still runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CasJobOwnerKind {
    PageWrite,
    ContentImport,
    NativeFileJob,
    SnapshotExport,
    ExternalPublication,
    ExternalCompaction,
    ExternalHydration,
    ExternalRestore,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CasJobOwner {
    pub(crate) kind: CasJobOwnerKind,
    pub(crate) id: String,
    /// The process instance that started the journal.
    pub(crate) process: String,
}

impl CasJobOwner {
    fn current(kind: CasJobOwnerKind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_owned(),
            process: process_instance().to_owned(),
        }
    }

    pub(crate) fn page_write(id: &str) -> Self {
        Self::current(CasJobOwnerKind::PageWrite, id)
    }

    pub(crate) fn content_import(id: &str) -> Self {
        Self::current(CasJobOwnerKind::ContentImport, id)
    }

    pub(crate) fn native_file_job(id: &str) -> Self {
        Self::current(CasJobOwnerKind::NativeFileJob, id)
    }

    pub(crate) fn snapshot_export(id: &str) -> Self {
        Self::current(CasJobOwnerKind::SnapshotExport, id)
    }

    pub(crate) fn external_publication(id: &str) -> Self {
        Self::current(CasJobOwnerKind::ExternalPublication, id)
    }

    pub(crate) fn external_compaction(id: &str) -> Self {
        Self::current(CasJobOwnerKind::ExternalCompaction, id)
    }

    pub(crate) fn external_hydration(id: &str) -> Self {
        Self::current(CasJobOwnerKind::ExternalHydration, id)
    }

    pub(crate) fn external_restore(id: &str) -> Self {
        Self::current(CasJobOwnerKind::ExternalRestore, id)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::native_file_job("synthetic-test-owner")
    }

    #[cfg(test)]
    pub(crate) fn from_another_process(kind: CasJobOwnerKind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_owned(),
            process: "synthetic-earlier-process".to_owned(),
        }
    }

    pub(crate) fn started_in_this_process(&self) -> bool {
        self.process == process_instance()
    }

    fn validate(&self) -> io::Result<()> {
        validate_job_id(&self.id)?;
        validate_job_id(&self.process)
    }
}

/// Identifies this run of the app; journals carry it to tell a page reload
/// apart from a restart.
pub(crate) fn process_instance() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// How many handles of this process have each journal open.
fn open_handles() -> std::sync::MutexGuard<'static, HashMap<PathBuf, usize>> {
    static HANDLES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    HANDLES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn track_open_handle(journal_path: &Path) {
    *open_handles().entry(journal_path.to_owned()).or_default() += 1;
    #[cfg(test)]
    {
        *JOURNAL_OPENS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(journal_path.to_owned())
            .or_default() += 1;
    }
}

#[cfg(test)]
static JOURNAL_OPENS: Mutex<BTreeMap<PathBuf, usize>> = Mutex::new(BTreeMap::new());

/// How many times this process began or opened the journal of `job_id`.
#[cfg(test)]
pub(crate) fn journal_opens(repository_root: &Path, job_id: &str) -> usize {
    let Some((_, directory)) = job_pin_directory(repository_root, false).unwrap() else {
        return 0;
    };
    JOURNAL_OPENS
        .lock()
        .unwrap()
        .get(&directory.join(format!("job-{job_id}.journal")))
        .copied()
        .unwrap_or_default()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PinDescriptor {
    byte_size: u64,
    role: CasObjectRole,
    published_by_job: bool,
}

#[derive(Debug)]
pub(crate) struct DurableCasJob {
    repository_root: PathBuf,
    journal_path: PathBuf,
    state: JobState,
}

#[derive(Debug)]
struct JobState {
    job_id: String,
    kind: CasJobKind,
    owner: CasJobOwner,
    created_at_ms: i64,
    pins: BTreeMap<String, PinDescriptor>,
    sealed: bool,
    released: bool,
    next_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum JobJournalRecord {
    Begin {
        version: u32,
        sequence: u64,
        job_id: String,
        job_kind: CasJobKind,
        owner: CasJobOwner,
        created_at_ms: i64,
    },
    Pin {
        sequence: u64,
        job_id: String,
        object_hash: String,
        byte_size: u64,
        object_role: CasObjectRole,
        published_by_job: bool,
    },
    Seal {
        sequence: u64,
        job_id: String,
        pin_count: u64,
    },
    Release {
        sequence: u64,
        job_id: String,
        outcome: CasReleaseOutcome,
    },
}

impl DurableCasJob {
    pub(crate) fn begin(
        repository_root: &Path,
        job_id: &str,
        kind: CasJobKind,
        owner: CasJobOwner,
        created_at_ms: i64,
    ) -> io::Result<Self> {
        validate_job_id(job_id)?;
        owner.validate()?;
        if created_at_ms < 0 {
            return invalid_data("CAS job creation time must be nonnegative");
        }
        let _repository_guard = super::coordinator::lock_repository_mutation()?;
        let (repository_root, directory) = job_pin_directory(repository_root, true)?
            .expect("requested job-pin directory creation");
        let journal_path = directory.join(format!("job-{job_id}.journal"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&journal_path)?;
        write_record(
            &mut file,
            &JobJournalRecord::Begin {
                version: DURABLE_CAS_JOB_VERSION,
                sequence: 0,
                job_id: job_id.to_owned(),
                job_kind: kind,
                owner: owner.clone(),
                created_at_ms,
            },
            true,
        )?;
        let _ = sync_directory(&directory)?;
        track_open_handle(&journal_path);
        Ok(Self {
            repository_root,
            journal_path,
            state: JobState {
                job_id: job_id.to_owned(),
                kind,
                owner,
                created_at_ms,
                pins: BTreeMap::new(),
                sealed: false,
                released: false,
                next_sequence: 1,
            },
        })
    }

    pub(crate) fn open(repository_root: &Path, job_id: &str) -> io::Result<Self> {
        validate_job_id(job_id)?;
        let (repository_root, directory) = job_pin_directory(repository_root, false)?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "CAS job directory is missing"))?;
        let journal_path = directory.join(format!("job-{job_id}.journal"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal_path)?;
        let inspected = inspect_job_state(&mut file, Some(job_id))?;
        if let Some(valid_length) = inspected.incomplete_tail {
            let _repository_guard = super::coordinator::lock_repository_mutation()?;
            recover_incomplete_tail_already_guarded(&mut file, valid_length)?;
        }
        let state = inspected.state;
        track_open_handle(&journal_path);
        Ok(Self {
            repository_root,
            journal_path,
            state,
        })
    }

    /// Whether the journal of `job_id` may still be on disk. A check that fails counts as present.
    pub(crate) fn journal_exists(repository_root: &Path, job_id: &str) -> bool {
        match job_pin_directory(repository_root, false) {
            Ok(Some((_, directory))) => directory.join(format!("job-{job_id}.journal")).try_exists().unwrap_or(true),
            Ok(None) => false,
            Err(_) => true,
        }
    }

    pub(crate) fn owner(&self) -> &CasJobOwner {
        &self.state.owner
    }

    /// Whether another handle of this process, besides this one, has the
    /// journal open.
    pub(crate) fn held_elsewhere(&self) -> bool {
        open_handles()
            .get(&self.journal_path)
            .is_some_and(|count| *count > 1)
    }

    pub(crate) fn prepare_bytes(
        &mut self,
        cas: &PayloadCas,
        bytes: &[u8],
        role: CasObjectRole,
    ) -> io::Result<PreparedPayload> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let prepared = cas.prepare_bytes(bytes)?;
        self.record_prepared(cas, &prepared, role)?;
        Ok(prepared)
    }

    pub(crate) fn prepare_reader(
        &mut self,
        cas: &PayloadCas,
        reader: &mut impl Read,
        role: CasObjectRole,
    ) -> io::Result<PreparedPayload> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let prepared = cas.prepare_reader(reader)?;
        self.record_prepared(cas, &prepared, role)?;
        Ok(prepared)
    }

    pub(crate) fn prepare_owned_upload(
        &mut self,
        cas: &PayloadCas,
        upload: super::payload_cas::OwnedPayloadUpload,
        role: CasObjectRole,
    ) -> io::Result<PreparedPayload> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let prepared = upload.finish(cas)?;
        self.record_prepared(cas, &prepared, role)?;
        Ok(prepared)
    }

    pub(crate) fn prepare_reader_expected(
        &mut self,
        cas: &PayloadCas,
        reader: &mut impl Read,
        expected_content_hash: &str,
        expected_byte_size: u64,
        role: CasObjectRole,
    ) -> io::Result<PreparedPayload> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let prepared =
            cas.prepare_reader_expected(reader, expected_content_hash, expected_byte_size)?;
        self.record_prepared(cas, &prepared, role)?;
        Ok(prepared)
    }

    pub(crate) fn adopt_import_payload(
        &mut self,
        cas: &PayloadCas,
        path: &Path,
        hash: &str,
        size: u64,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<PreparedPayload> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let prepared = cas.adopt_import_payload(path, hash, size, cancelled)?;
        // Adoption just validated and published this object. The unsealed job
        // blocks GC, so reopening the whole CAS path here adds no new guarantee.
        self.record_pin(
            &prepared.content_hash,
            prepared.byte_size,
            CasObjectRole::DirectObject,
            !prepared.deduplicated,
        )?;
        Ok(prepared)
    }

    pub(crate) fn publish_import_batch(
        &mut self,
        cas: &PayloadCas,
        mut staged: Vec<super::StagedPayload>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Vec<PreparedPayload>> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        if !matches!(self.state.kind, CasJobKind::LocalBackupRestore | CasJobKind::CardOrModuleContentImport) {
            return invalid_data("CAS job is not an import");
        }
        if staged.iter().any(|payload| payload.repository_root != self.repository_root) {
            return invalid_data("staged CAS object belongs to another repository");
        }
        let _guard = self.lock_import_publication(cas, &mut staged, cancelled)?;
        let existing = cas.stat_objects(staged.iter().map(|payload| payload.content_hash.as_str()))?;
        let mut journal = OpenOptions::new().append(true).open(&self.journal_path)?;
        // Record the whole batch before publication, including a possible partial failure.
        for (payload, existing_size) in staged.iter().zip(existing) {
            self.record_pin_in(&mut journal, &payload.content_hash, payload.byte_size,
                CasObjectRole::DirectObject, existing_size.is_none())?;
        }
        journal.sync_data()?;
        cas.publish_verified_import_batch(staged)
    }

    pub(crate) fn publish_unmapped_content_batch(
        &mut self,
        cas: &PayloadCas,
        mut staged: Vec<super::StagedPayload>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Vec<PreparedPayload>> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        if self.state.kind != CasJobKind::CardOrModuleContentImport || !self.state.pins.is_empty() {
            return invalid_data("CAS content job is not awaiting asset mapping");
        }
        // The unsealed job blocks collection until mapping supplies the final roots.
        let _guard = self.lock_import_publication(cas, &mut staged, cancelled)?;
        cas.publish_verified_import_batch(staged)
    }

    fn lock_import_publication(
        &self,
        cas: &PayloadCas,
        staged: &mut [super::StagedPayload],
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<std::sync::MutexGuard<'static, ()>> {
        loop {
            if cancelled() { return Err(io::Error::other("import cancelled")); }
            let guard = super::coordinator::lock_repository_mutation()?;
            if cas.import_destinations_unchanged(staged)? { return Ok(guard); }
            drop(guard);
            // A previous batch or another writer may have supplied the object.
            // Rehash outside the lock, then check the held file identity again.
            cas.refresh_import_destinations(staged, cancelled)?;
        }
    }

    pub(crate) fn pin_existing(
        &mut self,
        cas: &PayloadCas,
        object_hash: &str,
        byte_size: u64,
        role: CasObjectRole,
    ) -> io::Result<()> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        validate_hash(object_hash)?;
        match cas.stat_object(object_hash)? {
            Some(actual_size) if actual_size == byte_size => {}
            Some(_) => return invalid_data("existing CAS object size does not match its pin"),
            None => return invalid_data("existing CAS object is missing"),
        }
        self.record_pin(object_hash, byte_size, role, false)
    }

    pub(crate) fn pin_existing_batch(
        &mut self,
        cas: &PayloadCas,
        pins: &[(String, u64, CasObjectRole)],
    ) -> io::Result<()> {
        self.ensure_preparable()?;
        self.ensure_cas(cas)?;
        let mut pending = BTreeMap::<String, PinDescriptor>::new();
        for (object_hash, byte_size, role) in pins {
            validate_hash(object_hash)?;
            let descriptor = PinDescriptor {
                byte_size: *byte_size,
                role: *role,
                published_by_job: false,
            };
            if let Some(existing) = self.state.pins.get(object_hash) {
                if existing.byte_size != descriptor.byte_size || existing.role != descriptor.role {
                    return invalid_data("CAS job contains a conflicting object pin");
                }
                continue;
            }
            if pending
                .insert(object_hash.clone(), descriptor)
                .is_some_and(|existing| existing != descriptor)
            {
                return invalid_data("CAS job batch contains a conflicting object pin");
            }
        }
        for (object_hash, pin) in &pending {
            match cas.stat_object(object_hash)? {
                Some(actual_size) if actual_size == pin.byte_size => {}
                Some(_) => return invalid_data("existing CAS object size does not match its pin"),
                None => return invalid_data("existing CAS object is missing"),
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        for (object_hash, pin) in pending {
            let record = JobJournalRecord::Pin {
                sequence: self.state.next_sequence,
                job_id: self.state.job_id.clone(),
                object_hash: object_hash.clone(),
                byte_size: pin.byte_size,
                object_role: pin.role,
                published_by_job: pin.published_by_job,
            };
            write_record(&mut file, &record, false)?;
            self.state.pins.insert(object_hash, pin);
            self.state.next_sequence += 1;
        }
        file.flush()?;
        file.sync_data()
    }

    pub(crate) fn seal(
        &mut self,
        store: &mut PersistentStore,
        created_at_ms: i64,
    ) -> io::Result<()> {
        if self.state.sealed && !self.state.released {
            return Ok(());
        }
        self.ensure_preparable()?;
        if created_at_ms < 0 {
            return invalid_data("CAS job seal time must be nonnegative");
        }
        let store_root = fs::canonicalize(store.repository_root())?;
        if store_root != self.repository_root {
            return invalid_data("CAS job and object catalog use different repositories");
        }
        let registrations = self
            .state
            .pins
            .iter()
            .map(|(object_hash, pin)| AssetObjectRegistration {
                object_hash: object_hash.clone(),
                byte_size: pin.byte_size,
            })
            .collect::<Vec<_>>();
        let _repository_guard = super::coordinator::lock_repository_mutation()?;
        let mut catalog = store.asset_object_catalog();
        for batch in registrations.chunks(ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
            catalog
                .register(batch, created_at_ms)
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        }
        self.record_seal()
    }

    fn record_seal(&mut self) -> io::Result<()> {
        let record = JobJournalRecord::Seal {
            sequence: self.state.next_sequence,
            job_id: self.state.job_id.clone(),
            pin_count: self.state.pins.len() as u64,
        };
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_record(&mut file, &record, true)?;
        self.state.sealed = true;
        self.state.next_sequence += 1;
        Ok(())
    }

    /// Persist the exact batch before any canonical file can appear. A partial
    /// publication is recovered from these hashes without enumerating the CAS.
    pub(crate) fn record_staged_publication(
        &mut self,
        staged: &[super::StagedPayload],
        _guard: &std::sync::MutexGuard<'_, ()>,
    ) -> io::Result<()> {
        self.ensure_preparable()?;
        if self.state.owner.kind != CasJobOwnerKind::ExternalHydration {
            return invalid_data("CAS publication journal has a different owner kind");
        }
        for payload in staged {
            if payload.repository_root != self.repository_root {
                return invalid_data("staged CAS object belongs to another repository");
            }
            self.record_pin(&payload.content_hash, payload.byte_size, CasObjectRole::DirectObject, true)?;
        }
        OpenOptions::new().append(true).open(&self.journal_path)?.sync_all()
    }

    /// The caller has registered every published body while holding exclusion.
    pub(crate) fn finish_catalog_registration(
        &mut self,
        _guard: &std::sync::MutexGuard<'_, ()>,
    ) -> io::Result<()> {
        self.ensure_preparable()?;
        if self.state.owner.kind != CasJobOwnerKind::ExternalHydration {
            return invalid_data("CAS publication journal has a different owner kind");
        }
        self.record_seal()?;
        self.release_already_guarded(CasReleaseOutcome::Committed)
    }

    pub(crate) fn root_set(&self) -> io::Result<AssetRootSet> {
        if !self.state.sealed || self.state.released {
            return invalid_data("only a sealed active CAS job has a direct root set");
        }
        Ok(root_set_from_state(&self.state))
    }

    pub(crate) fn release(&mut self, outcome: CasReleaseOutcome) -> io::Result<()> {
        if self.state.released {
            let _repository_guard = super::coordinator::lock_repository_mutation()?;
            return self.cleanup_released_journal();
        }
        if outcome == CasReleaseOutcome::Committed && !self.state.sealed {
            return invalid_data("unsealed CAS job cannot be released as committed");
        }
        let _repository_guard = super::coordinator::lock_repository_mutation()?;
        self.release_already_guarded(outcome)
    }

    fn release_already_guarded(&mut self, outcome: CasReleaseOutcome) -> io::Result<()> {
        if outcome == CasReleaseOutcome::Aborted && !self.state.sealed {
            self.register_abort_candidates()?;
        }
        let record = JobJournalRecord::Release {
            sequence: self.state.next_sequence,
            job_id: self.state.job_id.clone(),
            outcome,
        };
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_record(&mut file, &record, true)?;
        drop(file);
        self.state.released = true;
        self.state.next_sequence += 1;
        self.cleanup_released_journal()
    }

    fn cleanup_released_journal(&self) -> io::Result<()> {
        match fs::remove_file(&self.journal_path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        if let Some(parent) = self.journal_path.parent() {
            let _ = sync_directory(parent)?;
        }
        Ok(())
    }

    fn register_abort_candidates(&self) -> io::Result<()> {
        let cas = PayloadCas::new(&self.repository_root)?;
        let registrations = self
            .state
            .pins
            .iter()
            .filter(|(_, pin)| pin.published_by_job)
            .filter_map(|(object_hash, pin)| match cas.stat_object(object_hash) {
                Ok(None) => None,
                Ok(Some(size)) if size == pin.byte_size => Some(Ok(AssetObjectRegistration {
                    object_hash: object_hash.clone(), byte_size: pin.byte_size,
                })),
                Ok(Some(_)) => Some(invalid_data("published CAS object size does not match its journal")),
                Err(error) => Some(Err(error)),
            })
            .collect::<io::Result<Vec<_>>>()?;
        if registrations.is_empty() {
            return Ok(());
        }
        for batch in registrations.chunks(ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
            crate::persistent_store::register_asset_objects_at_root(
                &self.repository_root,
                batch,
                self.state.created_at_ms,
            )
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        }
        Ok(())
    }

    // Used by native_file_jobs tests behind the native-official-publication
    // feature; the default lib build cannot see that usage.
    #[allow(dead_code)]
    pub(crate) fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub(crate) fn is_sealed(&self) -> bool {
        self.state.sealed
    }

    pub(crate) fn is_released(&self) -> bool {
        self.state.released
    }

    pub(crate) fn pin_count(&self) -> usize {
        self.state.pins.len()
    }

    pub(crate) fn has_exact_pins(&self, pins: &[(String, u64, CasObjectRole)]) -> bool {
        let mut expected = BTreeMap::<String, (u64, CasObjectRole)>::new();
        for (object_hash, byte_size, role) in pins {
            if expected
                .insert(object_hash.clone(), (*byte_size, *role))
                .is_some_and(|existing| existing != (*byte_size, *role))
            {
                return false;
            }
        }
        self.state.pins.len() == expected.len()
            && expected.iter().all(|(object_hash, (byte_size, role))| {
                self.state
                    .pins
                    .get(object_hash)
                    .is_some_and(|pin| pin.byte_size == *byte_size && pin.role == *role)
            })
    }

    pub(crate) fn kind(&self) -> CasJobKind {
        self.state.kind
    }

    #[cfg(test)]
    pub(crate) fn leave_release_record_for_cleanup_retry(
        &mut self,
        outcome: CasReleaseOutcome,
    ) -> io::Result<()> {
        if self.state.released {
            return Ok(());
        }
        if outcome == CasReleaseOutcome::Committed && !self.state.sealed {
            return invalid_data("unsealed CAS job cannot be released as committed");
        }
        let record = JobJournalRecord::Release {
            sequence: self.state.next_sequence,
            job_id: self.state.job_id.clone(),
            outcome,
        };
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_record(&mut file, &record, true)?;
        self.state.released = true;
        self.state.next_sequence += 1;
        Ok(())
    }

    fn ensure_preparable(&self) -> io::Result<()> {
        if self.state.sealed || self.state.released {
            return invalid_data("sealed or released CAS job cannot accept more objects");
        }
        Ok(())
    }

    fn ensure_cas(&self, cas: &PayloadCas) -> io::Result<()> {
        if cas.repository_root() != self.repository_root {
            return invalid_data("CAS job and payload CAS use different repositories");
        }
        Ok(())
    }

    fn record_prepared(
        &mut self,
        cas: &PayloadCas,
        prepared: &PreparedPayload,
        role: CasObjectRole,
    ) -> io::Result<()> {
        match cas.stat_object(&prepared.content_hash)? {
            Some(actual_size) if actual_size == prepared.byte_size => {}
            _ => return invalid_data("prepared CAS object is not durable at its canonical path"),
        }
        self.record_pin(
            &prepared.content_hash,
            prepared.byte_size,
            role,
            !prepared.deduplicated,
        )
    }

    fn record_pin(
        &mut self,
        object_hash: &str,
        byte_size: u64,
        role: CasObjectRole,
        published_by_job: bool,
    ) -> io::Result<()> {
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        self.record_pin_in(&mut file, object_hash, byte_size, role, published_by_job)
    }

    fn record_pin_in(
        &mut self,
        file: &mut File,
        object_hash: &str,
        byte_size: u64,
        role: CasObjectRole,
        published_by_job: bool,
    ) -> io::Result<()> {
        validate_hash(object_hash)?;
        let descriptor = PinDescriptor {
            byte_size,
            role,
            published_by_job,
        };
        if let Some(existing) = self.state.pins.get(object_hash) {
            if existing.byte_size == byte_size && existing.role == role {
                return Ok(());
            }
            return invalid_data("CAS job contains a conflicting object pin");
        }
        let record = JobJournalRecord::Pin {
            sequence: self.state.next_sequence,
            job_id: self.state.job_id.clone(),
            object_hash: object_hash.to_owned(),
            byte_size,
            object_role: role,
            published_by_job,
        };
        write_record(file, &record, false)?;
        self.state.pins.insert(object_hash.to_owned(), descriptor);
        self.state.next_sequence += 1;
        Ok(())
    }
}

impl Drop for DurableCasJob {
    fn drop(&mut self) {
        let mut handles = open_handles();
        if let Some(count) = handles.get_mut(&self.journal_path) {
            *count -= 1;
            if *count == 0 {
                handles.remove(&self.journal_path);
            }
        }
    }
}

/// A journal's owner and progress, read without repairing it.
#[derive(Clone, Debug)]
pub(crate) struct DurableCasJobOwnership {
    pub(crate) owner: CasJobOwner,
    pub(crate) sealed: bool,
}

/// Reads a journal without truncating an incomplete tail, which may be a
/// record its running job is still writing. `None` means it is gone.
pub(crate) fn inspect_durable_cas_job(
    repository_root: &Path,
    job_id: &str,
) -> io::Result<Option<DurableCasJobOwnership>> {
    validate_job_id(job_id)?;
    let Some((_, directory)) = job_pin_directory(repository_root, false)? else {
        return Ok(None);
    };
    let mut file = match File::open(directory.join(format!("job-{job_id}.journal"))) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let state = inspect_job_state(&mut file, Some(job_id))?.state;
    if state.released {
        return Ok(None);
    }
    Ok(Some(DurableCasJobOwnership {
        owner: state.owner,
        sealed: state.sealed,
    }))
}

/// Whether a handle of this process has the journal open.
pub(crate) fn durable_cas_job_held(repository_root: &Path, job_id: &str) -> io::Result<bool> {
    validate_job_id(job_id)?;
    let Some((_, directory)) = job_pin_directory(repository_root, false)? else {
        return Ok(false);
    };
    Ok(open_handles().contains_key(&directory.join(format!("job-{job_id}.journal"))))
}

/// The IDs of the journals present now, without reading them.
pub(crate) fn durable_cas_job_ids(repository_root: &Path) -> io::Result<Vec<String>> {
    let Some((_, directory)) = job_pin_directory(repository_root, false)? else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for entry in fs::read_dir(directory)?.take(MAX_DURABLE_CAS_JOB_JOURNALS) {
        let name = entry?.file_name();
        let Some(job_id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("job-"))
            .and_then(|name| name.strip_suffix(".journal"))
        else {
            continue;
        };
        if validate_job_id(job_id).is_ok() {
            ids.push(job_id.to_owned());
        }
    }
    Ok(ids)
}

pub(crate) fn collect_durable_cas_job_roots(repository_root: &Path) -> AssetRootSet {
    collect_durable_cas_job_roots_inner(repository_root, false)
}

pub(crate) fn collect_durable_cas_job_roots_read_only(repository_root: &Path) -> AssetRootSet {
    collect_durable_cas_job_roots_inner_read_only(repository_root)
}

fn collect_durable_cas_job_roots_inner_read_only(repository_root: &Path) -> AssetRootSet {
    let mut roots = AssetRootSet::default();
    let directory = match job_pin_directory(repository_root, false) {
        Ok(Some((_, directory))) => directory,
        Ok(None) => return roots,
        Err(_) => {
            roots
                .blockers
                .insert("job-pin-directory-invalid".to_owned());
            return roots;
        }
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            roots
                .blockers
                .insert("job-pin-directory-unreadable".to_owned());
            return roots;
        }
    };
    let mut count = 0;
    for entry in entries {
        count += 1;
        if count > MAX_DURABLE_CAS_JOB_JOURNALS {
            roots
                .blockers
                .insert("job-pin-journal-limit-exceeded".to_owned());
            return roots;
        }
        let Ok(entry) = entry else {
            roots
                .blockers
                .insert("job-pin-directory-unreadable".to_owned());
            continue;
        };
        let path = entry.path();
        let Some(job_id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix("job-"))
            .and_then(|n| n.strip_suffix(".journal"))
        else {
            roots.blockers.insert("job-pin-unknown-entry".to_owned());
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            roots.blockers.insert("job-pin-unknown-entry".to_owned());
            continue;
        };
        if !metadata.is_file() || is_link_like(&metadata) || validate_job_id(job_id).is_err() {
            roots.blockers.insert("job-pin-unknown-entry".to_owned());
            continue;
        }
        match File::open(&path).and_then(|mut file| read_job_state(&mut file, Some(job_id))) {
            Ok(state) if !state.released => {
                let job_roots = root_set_from_state(&state);
                roots.manifest_hashes.extend(job_roots.manifest_hashes);
                roots.object_hashes.extend(job_roots.object_hashes);
                if !state.sealed {
                    roots.blockers.insert(format!("job-pin-unsealed:{job_id}"));
                }
            }
            Ok(_) => {}
            Err(_) => {
                roots.blockers.insert(format!("job-pin-corrupt:{job_id}"));
            }
        }
    }
    roots
}

pub(crate) fn collect_durable_cas_job_roots_already_guarded(
    repository_root: &Path,
) -> AssetRootSet {
    collect_durable_cas_job_roots_inner(repository_root, true)
}

fn collect_durable_cas_job_roots_inner(
    repository_root: &Path,
    repository_guard_held: bool,
) -> AssetRootSet {
    let mut roots = AssetRootSet::default();
    let directory = match job_pin_directory(repository_root, false) {
        Ok(Some((_, directory))) => directory,
        Ok(None) => return roots,
        Err(_) => {
            roots
                .blockers
                .insert("job-pin-directory-invalid".to_owned());
            return roots;
        }
    };
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(_) => {
            roots
                .blockers
                .insert("job-pin-directory-unreadable".to_owned());
            return roots;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            roots
                .blockers
                .insert("job-pin-directory-unreadable".to_owned());
            return roots;
        };
        paths.push(entry.path());
        if paths.len() > MAX_DURABLE_CAS_JOB_JOURNALS {
            roots
                .blockers
                .insert("job-pin-journal-limit-exceeded".to_owned());
            return roots;
        }
    }
    paths.sort();
    let mut released_journals = Vec::new();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            roots.blockers.insert("job-pin-unknown-entry".to_owned());
            continue;
        };
        let Some(job_id) = file_name
            .strip_prefix("job-")
            .and_then(|name| name.strip_suffix(".journal"))
        else {
            roots.blockers.insert("job-pin-unknown-entry".to_owned());
            continue;
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !is_link_like(&metadata) => metadata,
            _ => {
                roots.blockers.insert("job-pin-unknown-entry".to_owned());
                continue;
            }
        };
        let _ = metadata;
        if validate_job_id(job_id).is_err() {
            roots.blockers.insert("job-pin-invalid-name".to_owned());
            continue;
        }
        let state = File::open(&path).and_then(|mut file| read_job_state(&mut file, Some(job_id)));
        let state = match state {
            Ok(state) => state,
            Err(_) => {
                roots.blockers.insert(format!("job-pin-corrupt:{job_id}"));
                continue;
            }
        };
        if state.released {
            released_journals.push((job_id.to_owned(), path));
        } else {
            let job_roots = root_set_from_state(&state);
            roots.manifest_hashes.extend(job_roots.manifest_hashes);
            roots.object_hashes.extend(job_roots.object_hashes);
            if !state.sealed {
                roots.blockers.insert(format!("job-pin-unsealed:{job_id}"));
            }
        }
    }
    if !released_journals.is_empty() {
        if repository_guard_held {
            cleanup_released_journal_roots(&directory, released_journals, &mut roots);
        } else {
            match super::coordinator::lock_repository_mutation() {
                Ok(_repository_guard) => {
                    cleanup_released_journal_roots(&directory, released_journals, &mut roots)
                }
                Err(_) => {
                    roots.blockers.insert("job-pin-release-cleanup".to_owned());
                }
            }
        }
    }
    roots
}

fn cleanup_released_journal_roots(
    directory: &Path,
    released_journals: Vec<(String, PathBuf)>,
    roots: &mut AssetRootSet,
) {
    let mut removed = false;
    for (job_id, path) in released_journals {
        let state = File::open(&path).and_then(|mut file| read_job_state(&mut file, Some(&job_id)));
        match state {
            Ok(state) if !state.released => {
                let job_roots = root_set_from_state(&state);
                roots.manifest_hashes.extend(job_roots.manifest_hashes);
                roots.object_hashes.extend(job_roots.object_hashes);
                if !state.sealed {
                    roots.blockers.insert(format!("job-pin-unsealed:{job_id}"));
                }
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(_) => {
                roots.blockers.insert(format!("job-pin-corrupt:{job_id}"));
                continue;
            }
        }
        match fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => {
                roots
                    .blockers
                    .insert(format!("job-pin-release-cleanup:{job_id}"));
            }
        }
    }
    if removed && sync_directory(directory).is_err() {
        roots.blockers.insert("job-pin-release-cleanup".to_owned());
    }
}

fn root_set_from_state(state: &JobState) -> AssetRootSet {
    let mut roots = AssetRootSet::default();
    for (hash, pin) in &state.pins {
        match pin.role {
            CasObjectRole::DirectObject => {
                roots.object_hashes.insert(hash.clone());
            }
            CasObjectRole::OwnerManifest => {
                roots.manifest_hashes.insert(hash.clone());
            }
        }
    }
    roots
}

fn job_pin_directory(
    repository_root: &Path,
    create: bool,
) -> io::Result<Option<(PathBuf, PathBuf)>> {
    let repository_root = fs::canonicalize(repository_root)?;
    let assets = ensure_child_directory(&repository_root, "assets", create)?;
    let Some(assets) = assets else {
        return Ok(None);
    };
    let directory = ensure_child_directory(&assets, "job-pins", create)?;
    Ok(directory.map(|directory| (repository_root, directory)))
}

fn ensure_child_directory(parent: &Path, name: &str, create: bool) -> io::Result<Option<PathBuf>> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !is_link_like(&metadata) => {}
        Ok(_) => return invalid_data("CAS job directory is not a real directory"),
        Err(error) if error.kind() == ErrorKind::NotFound && !create => return Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(&path)?;
            let _ = sync_directory(parent)?;
        }
        Err(error) => return Err(error),
    }
    let canonical = fs::canonicalize(&path)?;
    if !canonical.starts_with(parent) {
        return invalid_data("CAS job directory escapes its repository");
    }
    Ok(Some(canonical))
}

fn validate_job_id(job_id: &str) -> io::Result<()> {
    if job_id.is_empty()
        || job_id.len() > 64
        || !job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return invalid_data("CAS job ID must contain only ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn validate_hash(hash: &str) -> io::Result<()> {
    if is_lower_hex_256(hash) {
        return Ok(());
    }
    invalid_data("CAS job hash must be a lowercase SHA-256 hash")
}

const JOURNAL_LABELS: JournalFrameLabels = JournalFrameLabels {
    record_too_large: "CAS job journal record is too large",
    length_invalid: "CAS job journal record length is invalid",
    checksum_mismatch: "CAS job journal checksum mismatch",
};

fn write_record(file: &mut File, record: &JobJournalRecord, sync: bool) -> io::Result<()> {
    journal_frame::write_frame(file, record, sync, &JOURNAL_LABELS)
}

struct InspectedJobState {
    state: JobState,
    incomplete_tail: Option<u64>,
}

fn read_job_state(file: &mut File, expected_job_id: Option<&str>) -> io::Result<JobState> {
    let inspected = inspect_job_state(file, expected_job_id)?;
    if inspected.incomplete_tail.is_some() {
        return invalid_data("CAS job journal has a truncated frame");
    }
    Ok(inspected.state)
}

fn inspect_job_state(
    file: &mut File,
    expected_job_id: Option<&str>,
) -> io::Result<InspectedJobState> {
    let mut state = None;
    let scan = journal_frame::scan_frames(file, &JOURNAL_LABELS, |record: JobJournalRecord| {
        apply_record(&mut state, record, expected_job_id)
    })?;
    let state =
        state.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "CAS job journal is empty"))?;
    Ok(InspectedJobState {
        state,
        incomplete_tail: scan.incomplete_tail,
    })
}

fn apply_record(
    state: &mut Option<JobState>,
    record: JobJournalRecord,
    expected_job_id: Option<&str>,
) -> io::Result<()> {
    match record {
        JobJournalRecord::Begin {
            version,
            sequence,
            job_id,
            job_kind,
            owner,
            created_at_ms,
        } => {
            if state.is_some()
                || version != DURABLE_CAS_JOB_VERSION
                || sequence != 0
                || created_at_ms < 0
            {
                return invalid_data("CAS job journal begin record is invalid");
            }
            validate_job_id(&job_id)?;
            validate_identity(expected_job_id, &job_id)?;
            owner.validate()?;
            *state = Some(JobState {
                job_id,
                kind: job_kind,
                owner,
                created_at_ms,
                pins: BTreeMap::new(),
                sealed: false,
                released: false,
                next_sequence: 1,
            });
        }
        JobJournalRecord::Pin {
            sequence,
            job_id,
            object_hash,
            byte_size,
            object_role,
            published_by_job,
        } => {
            let state = current_state(state, sequence, &job_id, expected_job_id)?;
            if state.sealed || state.released {
                return invalid_data("CAS job journal pin is out of order");
            }
            validate_hash(&object_hash)?;
            if state
                .pins
                .insert(
                    object_hash,
                    PinDescriptor {
                        byte_size,
                        role: object_role,
                        published_by_job,
                    },
                )
                .is_some()
            {
                return invalid_data("CAS job journal contains a duplicate object pin");
            }
            state.next_sequence += 1;
        }
        JobJournalRecord::Seal {
            sequence,
            job_id,
            pin_count,
        } => {
            let state = current_state(state, sequence, &job_id, expected_job_id)?;
            if state.sealed || state.released || pin_count != state.pins.len() as u64 {
                return invalid_data("CAS job journal seal is invalid");
            }
            state.sealed = true;
            state.next_sequence += 1;
        }
        JobJournalRecord::Release {
            sequence,
            job_id,
            outcome,
        } => {
            let state = current_state(state, sequence, &job_id, expected_job_id)?;
            if state.released || (outcome == CasReleaseOutcome::Committed && !state.sealed) {
                return invalid_data("CAS job journal release is invalid");
            }
            state.released = true;
            state.next_sequence += 1;
        }
    }
    Ok(())
}

fn current_state<'a>(
    state: &'a mut Option<JobState>,
    sequence: u64,
    job_id: &str,
    expected_job_id: Option<&str>,
) -> io::Result<&'a mut JobState> {
    validate_identity(expected_job_id, job_id)?;
    let state = state
        .as_mut()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "CAS job record precedes begin"))?;
    if state.job_id != job_id || state.next_sequence != sequence || state.released {
        return invalid_data("CAS job journal identity or sequence is invalid");
    }
    Ok(state)
}

fn validate_identity(expected: Option<&str>, actual: &str) -> io::Result<()> {
    if expected.is_some_and(|expected| expected != actual) {
        return invalid_data("CAS job journal ID does not match its file");
    }
    Ok(())
}

fn recover_incomplete_tail_already_guarded(file: &mut File, valid_length: u64) -> io::Result<()> {
    journal_frame::truncate_to_valid_prefix(file, valid_length)
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::{
        collect_durable_cas_job_roots, collect_durable_cas_job_roots_read_only, write_record,
        CasJobKind, CasJobOwner, CasJobOwnerKind, CasObjectRole, CasReleaseOutcome, DurableCasJob, JobJournalRecord,
        DURABLE_CAS_JOB_VERSION, MAX_DURABLE_CAS_JOB_JOURNALS, read_job_state, root_set_from_state,
    };
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::asset_repository::PayloadCas;
    use crate::persistent_store::PersistentStore;
    use std::fs::OpenOptions;
    use std::io::{Write,Seek,SeekFrom};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn interrupted_staged_publication_recovers_only_its_present_files() {
        use sha2::Digest;
        for published_count in 0..=3 {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let store = PersistentStore::open(root).unwrap();
            let cas = PayloadCas::new(root).unwrap();
            cas.prepare_bytes(b"synthetic unrelated uncataloged file").unwrap();
            let bodies: [&[u8]; 3] = [b"synthetic first", b"synthetic second", b"synthetic third"];
            let hashes = bodies.iter().map(|body| hex::encode(sha2::Sha256::digest(body))).collect::<Vec<_>>();
            let mut staged = bodies.iter().zip(&hashes).map(|(body, hash)|
                cas.stage_reader_expected(&mut std::io::Cursor::new(body), hash, body.len() as u64).unwrap()).collect::<Vec<_>>();
            let mut job = DurableCasJob::begin(root, "interrupted-publication", CasJobKind::DirectAssetOrInlayWrite,
                CasJobOwner::from_another_process(CasJobOwnerKind::ExternalHydration, "interrupted-publication"), 1).unwrap();
            {
                let guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
                job.record_staged_publication(&staged, &guard).unwrap();
                cas.publish_staged_batch(staged.drain(..published_count).collect()).unwrap();
            }
            drop(staged);
            drop(job);
            drop(store);
            let mut recovered = DurableCasJob::open(root, "interrupted-publication").unwrap();
            assert_eq!(recovered.pin_count(), 3);
            recovered.release(CasReleaseOutcome::Aborted).unwrap();
            let _store = PersistentStore::open(root).unwrap();
            let db = rusqlite::Connection::open(root.join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
            let mut query = db.prepare("SELECT object_hash FROM asset_objects").unwrap();
            let registered = query.query_map([], |row| row.get::<_, String>(0)).unwrap()
                .collect::<Result<std::collections::BTreeSet<_>, _>>().unwrap();
            assert_eq!(registered, hashes[..published_count].iter().cloned().collect());
            assert!(super::durable_cas_job_ids(root).unwrap().is_empty());
        }
    }

    #[test]
    fn durable_job_transitions_wait_for_repository_mutation_exclusion() {
        let directory = tempfile::tempdir().expect("create coordinated job directory");
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("lock repository mutation");
        let root = directory.path().to_path_buf();
        let (sent, received) = mpsc::channel();
        let (ready_sent, ready_received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sent.send(()).expect("signal begin attempt");
            let job = DurableCasJob::begin(&root, "coordinated-job", CasJobKind::LocalBackupRestore, CasJobOwner::for_test(), 1)
                .expect("begin coordinated job");
            sent.send(job).expect("send coordinated job");
        });

        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached begin transition");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        drop(guard);
        let mut job = received
            .recv_timeout(Duration::from_secs(5))
            .expect("job begins after exclusion releases");
        assert!(job.journal_path().is_file());
        worker.join().expect("join coordinated job worker");

        let root = directory.path().to_path_buf();
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("lock repository mutation during payload streaming");
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let cas = PayloadCas::new(&root).expect("open payload CAS");
            job.prepare_bytes(&cas, b"coordinated payload", CasObjectRole::DirectObject)
                .expect("prepare coordinated payload without repository guard");
            sent.send(job).expect("send prepared job");
        });
        let mut job = received
            .recv_timeout(Duration::from_secs(5))
            .expect("payload preparation does not wait for repository exclusion");
        drop(guard);
        worker.join().expect("join payload preparation worker");

        let store = PersistentStore::open(directory.path()).expect("open persistent store");
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("lock repository mutation for seal");
        let (sent, received) = mpsc::channel();
        let (ready_sent, ready_received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut store = store;
            ready_sent.send(()).expect("signal seal attempt");
            job.seal(&mut store, 2).expect("seal coordinated job");
            sent.send((job, store)).expect("send sealed job");
        });
        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached seal transition");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        drop(guard);
        let (mut job, store) = received
            .recv_timeout(Duration::from_secs(5))
            .expect("job seals after exclusion releases");
        assert!(job.is_sealed());
        drop(store);
        worker.join().expect("join coordinated seal worker");

        let journal_path = job.journal_path().to_path_buf();
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("lock repository mutation for release");
        let (sent, received) = mpsc::channel();
        let (ready_sent, ready_received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sent.send(()).expect("signal release attempt");
            job.release(CasReleaseOutcome::Committed)
                .expect("release coordinated job");
            sent.send(()).expect("send release completion");
        });
        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached release transition");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        drop(guard);
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("job releases after exclusion releases");
        assert!(!journal_path.exists());
        worker.join().expect("join coordinated release worker");
    }

    #[test]
    fn seal_catalog_registration_waits_for_repository_mutation_exclusion() {
        let directory = tempfile::tempdir().expect("create catalog coordination directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "catalog-coordinated-job",
            CasJobKind::DirectAssetOrInlayWrite,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin catalog coordinated job");
        let prepared = job
            .prepare_bytes(
                &cas,
                b"catalog coordinated payload",
                CasObjectRole::DirectObject,
            )
            .expect("prepare catalog coordinated payload");
        let store = PersistentStore::open(directory.path()).expect("open persistent store");
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("hold final recheck exclusion");
        let (ready_sent, ready_received) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut store = store;
            ready_sent.send(()).expect("signal seal attempt");
            job.seal(&mut store, 2)
                .expect("seal coordinated catalog job");
            sent.send(()).expect("send seal completion");
        });

        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached seal attempt");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        let observer =
            PersistentStore::open(directory.path()).expect("open catalog observer store");
        let before = observer
            .query_asset_object_catalog(16, None)
            .expect("inspect catalog while final recheck owns exclusion");
        assert!(!before
            .items
            .iter()
            .any(|item| item.object_hash == prepared.content_hash));

        drop(guard);
        received
            .recv_timeout(Duration::from_secs(5))
            .expect("seal completes after final recheck releases");
        worker.join().expect("join catalog coordination worker");
        let after = observer
            .query_asset_object_catalog(16, None)
            .expect("inspect catalog after seal");
        assert!(after
            .items
            .iter()
            .any(|item| item.object_hash == prepared.content_hash));
    }

    #[test]
    fn aborted_job_catalogs_only_objects_it_published_for_safe_gc() {
        let directory = tempfile::tempdir().expect("create aborted job directory");
        let mut store = PersistentStore::open(directory.path()).expect("open catalog");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let preexisting = cas
            .prepare_bytes(b"preexisting unowned payload")
            .expect("prepare preexisting payload");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "aborted-catalog-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin aborted job");
        let published = job
            .prepare_bytes(&cas, b"new aborted payload", CasObjectRole::DirectObject)
            .expect("prepare newly published payload");
        job.pin_existing(
            &cas,
            &preexisting.content_hash,
            preexisting.byte_size,
            CasObjectRole::DirectObject,
        )
        .expect("pin preexisting payload");

        job.release(CasReleaseOutcome::Aborted)
            .expect("release aborted job");

        let catalog = store
            .query_asset_object_catalog(16, None)
            .expect("read abort candidates");
        assert!(catalog
            .items
            .iter()
            .any(|candidate| candidate.object_hash == published.content_hash));
        assert!(!catalog
            .items
            .iter()
            .any(|candidate| candidate.object_hash == preexisting.content_hash));

        let deleted = store
            .asset_gc_delete_page(16, None, 100, 10)
            .expect("collect unrooted abort candidate");
        assert_eq!(deleted.report.deleted_hashes, [published.content_hash]);
        assert_eq!(
            cas.stat_object(&preexisting.content_hash)
                .expect("stat preexisting payload"),
            Some(preexisting.byte_size)
        );
    }

    #[test]
    fn aborted_job_keeps_its_journal_when_candidate_registration_fails() {
        let directory = tempfile::tempdir().expect("create failed abort directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "failed-abort-catalog-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin failed abort job");
        let prepared = job
            .prepare_bytes(
                &cas,
                b"candidate registration failure",
                CasObjectRole::DirectObject,
            )
            .expect("prepare abort candidate");
        let journal_path = job.journal_path().to_path_buf();
        let store = PersistentStore::open(directory.path()).expect("open persistent store");
        drop(store);
        let connection =
            rusqlite::Connection::open(directory.path().join("persistent/persistent.sqlite"))
                .expect("open catalog database");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_abort_candidate BEFORE INSERT ON asset_objects
                 BEGIN SELECT RAISE(ABORT, 'synthetic'); END;",
            )
            .expect("install registration failure");
        drop(connection);

        assert!(job.release(CasReleaseOutcome::Aborted).is_err());
        assert!(!job.is_released());
        assert!(journal_path.is_file());
        assert_eq!(
            cas.stat_object(&prepared.content_hash)
                .expect("stat protected candidate"),
            Some(prepared.byte_size)
        );
        assert!(collect_durable_cas_job_roots(directory.path())
            .blockers
            .contains("job-pin-unsealed:failed-abort-catalog-job"));
    }

    #[test]
    fn aborted_job_does_not_create_a_missing_persistent_database() {
        let directory = tempfile::tempdir().expect("create missing store directory");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "missing-store-abort-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin missing store job");
        job.prepare_bytes(&cas, b"orphan candidate", CasObjectRole::DirectObject)
            .expect("prepare orphan candidate");
        let journal_path = job.journal_path().to_path_buf();

        assert!(job.release(CasReleaseOutcome::Aborted).is_err());
        assert!(!directory.path().join("persistent/persistent.sqlite").exists());
        assert!(journal_path.is_file());
        assert!(!job.is_released());
    }

    #[test]
    fn read_only_root_collection_fails_closed_at_the_journal_limit_without_cleanup() {
        let directory = tempfile::tempdir().expect("create journal-limit directory");
        let journal_directory = directory.path().join("assets/job-pins");
        std::fs::create_dir_all(&journal_directory).expect("create journal directory");
        for index in 0..=MAX_DURABLE_CAS_JOB_JOURNALS {
            std::fs::write(
                journal_directory.join(format!("job-limit-{index}.journal")),
                b"unread journal",
            )
            .expect("write journal-limit entry");
        }

        let roots = collect_durable_cas_job_roots_read_only(directory.path());

        assert!(roots.blockers.contains("job-pin-journal-limit-exceeded"));
        assert!(journal_directory.join("job-limit-0.journal").is_file());
        assert!(journal_directory
            .join(format!("job-limit-{MAX_DURABLE_CAS_JOB_JOURNALS}.journal"))
            .is_file());
    }

    #[test]
    fn unsealed_job_blocks_dry_run_then_sealed_manifest_roots_are_transitive() {
        let directory = tempfile::tempdir().expect("create durable job directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let payload = cas
            .prepare_bytes(b"old deduplicated payload")
            .expect("prepare payload");
        let manifest_bytes = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "Old".to_owned(),
                "assets/old.bin".to_owned(),
                "bin".to_owned(),
            ],
            payload_hash: Some(
                hex::decode(&payload.content_hash)
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
        }])
        .expect("encode owner manifest");
        let manifest = cas
            .prepare_bytes(&manifest_bytes)
            .expect("prepare manifest");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "content-import-1",
            CasJobKind::CardOrModuleContentImport,
            CasJobOwner::for_test(),
            10,
        )
        .expect("begin durable job");
        job.pin_existing(
            &cas,
            &manifest.content_hash,
            manifest.byte_size,
            CasObjectRole::OwnerManifest,
        )
        .expect("pin manifest");

        let unsealed = collect_durable_cas_job_roots(directory.path());
        assert!(unsealed
            .blockers
            .iter()
            .any(|blocker| blocker.starts_with("job-pin-unsealed:")));

        job.seal(&mut store, 20).expect("seal durable job");
        let sealed = collect_durable_cas_job_roots(directory.path());
        assert!(sealed.blockers.is_empty());
        assert!(sealed.manifest_hashes.contains(&manifest.content_hash));
        let page = store
            .asset_gc_dry_run(16, None, 100, 10)
            .expect("run catalog dry run");
        assert!(page.report.marked_hashes.contains(&manifest.content_hash));
        assert!(page.report.marked_hashes.contains(&payload.content_hash));
        assert!(page.report.potential_delete_hashes.is_empty());

        job.release(CasReleaseOutcome::Committed)
            .expect("release committed job");
        let released = collect_durable_cas_job_roots(directory.path());
        assert!(!released.manifest_hashes.contains(&manifest.content_hash));
    }

    #[test]
    fn job_recovery_truncates_only_an_incomplete_tail_and_corruption_blocks_sweep() {
        let directory = tempfile::tempdir().expect("create recovery directory");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "recoverable-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin recoverable job");
        let cas = PayloadCas::new(directory.path()).expect("open CAS");
        job.prepare_bytes(&cas, b"recoverable", CasObjectRole::DirectObject)
            .expect("prepare recoverable object");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal tail")
            .write_all(&[3, 0])
            .expect("append incomplete frame");
        let incomplete_length = std::fs::metadata(&journal_path)
            .expect("stat incomplete journal")
            .len();
        let blocked = collect_durable_cas_job_roots(directory.path());
        assert!(blocked.blockers.contains("job-pin-corrupt:recoverable-job"));
        assert_eq!(
            std::fs::metadata(&journal_path)
                .expect("restat incomplete journal")
                .len(),
            incomplete_length
        );
        let reopened = DurableCasJob::open(directory.path(), "recoverable-job")
            .expect("recover incomplete tail");
        assert!(!reopened.is_sealed());
        assert!(
            std::fs::metadata(&journal_path)
                .expect("stat repaired journal")
                .len()
                < incomplete_length
        );
        drop(reopened);

        let mut bytes = std::fs::read(&journal_path).expect("read journal");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&journal_path, bytes).expect("corrupt checksum");
        let corrupt = collect_durable_cas_job_roots(directory.path());
        assert!(corrupt
            .blockers
            .iter()
            .any(|blocker| blocker.starts_with("job-pin-corrupt:")));
    }

    #[test]
    fn incomplete_tail_recovery_waits_for_repository_exclusion() {
        let directory = tempfile::tempdir().expect("create coordinated recovery directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "coordinated-tail-recovery",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin coordinated recovery job");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open journal tail")
            .write_all(&[3, 0])
            .expect("append incomplete frame");
        let incomplete_length = std::fs::metadata(&journal_path)
            .expect("stat incomplete journal")
            .len();
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("hold final recheck exclusion during tail recovery");
        let root = directory.path().to_path_buf();
        let (ready_sent, ready_received) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sent.send(()).expect("signal recovery open");
            let recovered = DurableCasJob::open(&root, "coordinated-tail-recovery")
                .expect("recover incomplete journal tail");
            sent.send(recovered).expect("send recovered job");
        });

        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached recovery open");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(
            std::fs::metadata(&journal_path)
                .expect("restat blocked recovery journal")
                .len(),
            incomplete_length
        );
        drop(guard);
        let recovered = received
            .recv_timeout(Duration::from_secs(5))
            .expect("recovery completes after exclusion releases");
        worker.join().expect("join tail recovery worker");
        assert!(
            std::fs::metadata(recovered.journal_path())
                .expect("stat recovered journal")
                .len()
                < incomplete_length
        );
    }

    #[test]
    fn job_rejects_conflicting_existing_object_pins_and_recovers_by_session_id() {
        let directory = tempfile::tempdir().expect("create pin conflict directory");
        let cas = PayloadCas::new(directory.path()).expect("open CAS");
        let prepared = cas.prepare_bytes(b"same object").expect("prepare object");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "backup-import-1",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin peer job");
        job.pin_existing(
            &cas,
            &prepared.content_hash,
            prepared.byte_size,
            CasObjectRole::DirectObject,
        )
        .expect("pin exact object");
        job.pin_existing(
            &cas,
            &prepared.content_hash,
            prepared.byte_size,
            CasObjectRole::DirectObject,
        )
        .expect("deduplicate exact pin");
        assert!(job
            .pin_existing(
                &cas,
                &prepared.content_hash,
                prepared.byte_size + 1,
                CasObjectRole::DirectObject,
            )
            .is_err());
        assert!(job
            .pin_existing(
                &cas,
                &prepared.content_hash,
                prepared.byte_size,
                CasObjectRole::OwnerManifest,
            )
            .is_err());
        drop(job);

        let reopened = DurableCasJob::open(directory.path(), "backup-import-1")
            .expect("reopen job by session id");
        assert_eq!(reopened.pin_count(), 1);
        assert_eq!(reopened.kind(), CasJobKind::LocalBackupRestore);
    }

    #[test]
    fn recovery_removes_released_journal_left_after_terminal_sync() {
        let directory = tempfile::tempdir().expect("create release recovery directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "released-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin releasable job");
        job.seal(&mut store, 2).expect("seal empty job");
        let journal_path = job.journal_path().to_path_buf();
        let release = JobJournalRecord::Release {
            sequence: job.state.next_sequence,
            job_id: job.state.job_id.clone(),
            outcome: CasReleaseOutcome::Committed,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open released journal");
        write_record(&mut file, &release, true).expect("sync release record");
        drop(file);
        drop(job);

        let roots = collect_durable_cas_job_roots(directory.path());
        assert!(roots.blockers.is_empty());
        assert!(!journal_path.exists());
    }

    #[test]
    fn released_session_reopens_for_cleanup_without_appending_a_second_release() {
        let directory = tempfile::tempdir().expect("create release retry directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "release-retry-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin release retry job");
        job.seal(&mut store, 2).expect("seal retry job");
        let journal_path = job.journal_path().to_path_buf();
        let release = JobJournalRecord::Release {
            sequence: job.state.next_sequence,
            job_id: job.state.job_id.clone(),
            outcome: CasReleaseOutcome::Committed,
        };
        let mut file = OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .expect("open retry journal");
        write_record(&mut file, &release, true).expect("sync retry release");
        drop(file);
        drop(job);

        let mut recovered = DurableCasJob::open(directory.path(), "release-retry-job")
            .expect("recover released session");
        assert!(recovered.is_released());
        recovered
            .release(CasReleaseOutcome::Committed)
            .expect("retry release cleanup");
        assert!(!journal_path.exists());
    }

    #[test]
    fn root_collection_release_cleanup_waits_for_repository_exclusion() {
        let directory = tempfile::tempdir().expect("create coordinated recovery directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "collector-cleanup-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin collector cleanup job");
        job.seal(&mut store, 2).expect("seal collector cleanup job");
        job.leave_release_record_for_cleanup_retry(CasReleaseOutcome::Committed)
            .expect("leave released journal for collector");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("hold final recheck exclusion during collection");
        let root = directory.path().to_path_buf();
        let (ready_sent, ready_received) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sent.send(()).expect("signal root collection");
            sent.send(collect_durable_cas_job_roots(&root))
                .expect("send collected roots");
        });

        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached root collection");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(journal_path.exists());
        drop(guard);
        let roots = received
            .recv_timeout(Duration::from_secs(5))
            .expect("collection completes after exclusion releases");
        worker.join().expect("join coordinated root collector");
        assert!(roots.blockers.is_empty());
        assert!(!journal_path.exists());
    }

    #[test]
    fn reopened_release_cleanup_waits_for_exclusion_and_remains_idempotent() {
        let directory = tempfile::tempdir().expect("create coordinated release retry directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "coordinated-release-retry",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin coordinated release retry job");
        job.seal(&mut store, 2).expect("seal coordinated retry job");
        job.leave_release_record_for_cleanup_retry(CasReleaseOutcome::Committed)
            .expect("leave terminal release for retry");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        let mut recovered = DurableCasJob::open(directory.path(), "coordinated-release-retry")
            .expect("reopen released session");
        let guard = crate::asset_repository::coordinator::lock_repository_mutation()
            .expect("hold final recheck exclusion during release cleanup");
        let (ready_sent, ready_received) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sent.send(()).expect("signal cleanup retry");
            recovered
                .release(CasReleaseOutcome::Committed)
                .expect("clean released journal");
            sent.send(recovered).expect("send cleaned session");
        });

        ready_received
            .recv_timeout(Duration::from_secs(5))
            .expect("worker reached cleanup retry");
        assert!(received.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(journal_path.exists());
        drop(guard);
        let mut recovered = received
            .recv_timeout(Duration::from_secs(5))
            .expect("cleanup retry completes after exclusion releases");
        worker.join().expect("join cleanup retry worker");
        assert!(!journal_path.exists());
        recovered
            .release(CasReleaseOutcome::Committed)
            .expect("repeat cleanup remains idempotent");
    }

    #[test]
    fn unsupported_journal_version_fails_closed() {
        let directory = tempfile::tempdir().expect("create invalid version directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "future-version-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin version fixture");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        let mut file = OpenOptions::new()
            .truncate(true)
            .write(true)
            .open(&journal_path)
            .expect("replace journal");
        write_record(
            &mut file,
            &JobJournalRecord::Begin {
                version: DURABLE_CAS_JOB_VERSION + 1,
                sequence: 0,
                job_id: "future-version-job".to_owned(),
                job_kind: CasJobKind::LocalBackupRestore,
                owner: CasJobOwner::for_test(),
                created_at_ms: 1,
            },
            true,
        )
        .expect("write future-version record");
        drop(file);

        let roots = collect_durable_cas_job_roots(directory.path());
        assert!(roots
            .blockers
            .contains("job-pin-corrupt:future-version-job"));
    }

    #[test]
    fn a_begin_record_without_an_owner_is_corrupt() {
        let directory = tempfile::tempdir().expect("create ownerless directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "ownerless-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin ownerless fixture");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);
        let mut file = OpenOptions::new()
            .truncate(true)
            .write(true)
            .open(&journal_path)
            .expect("replace journal");
        super::journal_frame::write_frame(
            &mut file,
            &serde_json::json!({
                "type": "begin", "version": DURABLE_CAS_JOB_VERSION, "sequence": 0,
                "job_id": "ownerless-job", "job_kind": "local-backup-restore", "created_at_ms": 1,
            }),
            true,
            &super::JOURNAL_LABELS,
        )
        .expect("write ownerless record");
        drop(file);

        assert!(DurableCasJob::open(directory.path(), "ownerless-job").is_err());
        assert!(super::inspect_durable_cas_job(directory.path(), "ownerless-job").is_err());
        assert!(collect_durable_cas_job_roots(directory.path())
            .blockers
            .contains("job-pin-corrupt:ownerless-job"));
    }

    #[test]
    fn a_journal_keeps_its_owner_and_counts_the_handles_that_hold_it() {
        let directory = tempfile::tempdir().expect("create owner directory");
        let owner = CasJobOwner::from_another_process(CasJobOwnerKind::ExternalRestore, "restore-owner");
        assert!(!owner.started_in_this_process());
        assert!(CasJobOwner::for_test().started_in_this_process());
        let first = DurableCasJob::begin(
            directory.path(),
            "owned-job",
            CasJobKind::LocalBackupRestore,
            owner.clone(),
            1,
        )
        .expect("begin owned job");
        assert!(!first.held_elsewhere());
        let second = DurableCasJob::open(directory.path(), "owned-job").expect("reopen owned job");
        assert_eq!(second.owner(), &owner);
        assert!(first.held_elsewhere() && second.held_elsewhere());
        drop(first);
        assert!(!second.held_elsewhere());
        assert!(super::durable_cas_job_held(directory.path(), "owned-job").unwrap());
        let journal_path = second.journal_path().to_path_buf();
        drop(second);
        assert!(!super::durable_cas_job_held(directory.path(), "owned-job").unwrap());

        let mut torn = OpenOptions::new().append(true).open(&journal_path).expect("open journal");
        torn.write_all(&[0x40, 0, 0]).expect("append a torn frame");
        drop(torn);
        let length = std::fs::metadata(&journal_path).unwrap().len();
        let inspected = super::inspect_durable_cas_job(directory.path(), "owned-job")
            .expect("inspect torn journal")
            .expect("unreleased journal");
        assert_eq!(inspected.owner, owner);
        assert!(!inspected.sealed);
        assert_eq!(std::fs::metadata(&journal_path).unwrap().len(), length);
    }

    #[test]
    fn metadata_only_pin_journal_accepts_more_than_one_hundred_thousand_unique_objects() {
        let mut journal=tempfile::tempfile().unwrap();
        let id="synthetic-uncapped-pins";
        write_record(&mut journal,&JobJournalRecord::Begin {version:1,sequence:0,job_id:id.into(),job_kind:CasJobKind::LocalBackupRestore,owner:CasJobOwner::for_test(),created_at_ms:0},false).unwrap();
        for position in 1..=100_001u64 {
            write_record(&mut journal,&JobJournalRecord::Pin {sequence:position,job_id:id.into(),object_hash:format!("{position:064x}"),byte_size:position,object_role:CasObjectRole::DirectObject,published_by_job:false},false).unwrap();
        }
        write_record(&mut journal,&JobJournalRecord::Seal {sequence:100_002,job_id:id.into(),pin_count:100_001},false).unwrap();
        journal.sync_all().unwrap();
        journal.seek(SeekFrom::Start(0)).unwrap();
        let state=read_job_state(&mut journal,Some(id)).unwrap();
        assert!(state.sealed);
        assert_eq!(state.pins.len(),100_001);
        assert_eq!(root_set_from_state(&state).object_hashes.len(),100_001);
    }

    #[test]
    fn native_file_job_startup_does_not_remove_cas_liveness_journals() {
        let directory = tempfile::tempdir().expect("create ownership boundary directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "separate-owner-job",
            CasJobKind::LocalBackupRestore,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin CAS liveness job");
        let journal_path = job.journal_path().to_path_buf();
        drop(job);

        let _native_jobs = crate::native_file_jobs::NativeFileJobState::initialize(
            directory.path().join("native-file-jobs"),
        );
        assert!(journal_path.is_file());
        assert!(collect_durable_cas_job_roots(directory.path())
            .blockers
            .contains("job-pin-unsealed:separate-owner-job"));
    }

    #[test]
    fn journal_identity_and_record_order_fail_closed() {
        let identity_directory = tempfile::tempdir().expect("create identity directory");
        let identity_job = DurableCasJob::begin(
            identity_directory.path(),
            "identity-a",
            CasJobKind::CardOrModuleContentImport,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin identity job");
        let renamed = identity_job
            .journal_path()
            .with_file_name("job-identity-b.journal");
        std::fs::rename(identity_job.journal_path(), &renamed).expect("rename identity journal");
        drop(identity_job);
        assert!(collect_durable_cas_job_roots(identity_directory.path())
            .blockers
            .contains("job-pin-corrupt:identity-b"));

        let order_directory = tempfile::tempdir().expect("create order directory");
        let order_job = DurableCasJob::begin(
            order_directory.path(),
            "record-order",
            CasJobKind::OfficialPublicationOrExportPreparation,
            CasJobOwner::for_test(),
            1,
        )
        .expect("begin order job");
        let mut file = OpenOptions::new()
            .append(true)
            .open(order_job.journal_path())
            .expect("open order journal");
        write_record(
            &mut file,
            &JobJournalRecord::Seal {
                sequence: 1,
                job_id: "record-order".to_owned(),
                pin_count: 0,
            },
            false,
        )
        .expect("write early seal");
        write_record(
            &mut file,
            &JobJournalRecord::Pin {
                sequence: 2,
                job_id: "record-order".to_owned(),
                object_hash: "aa".repeat(32),
                byte_size: 1,
                object_role: CasObjectRole::DirectObject,
                published_by_job: false,
            },
            true,
        )
        .expect("write late pin");
        drop(file);
        drop(order_job);
        assert!(collect_durable_cas_job_roots(order_directory.path())
            .blockers
            .contains("job-pin-corrupt:record-order"));
    }
}
