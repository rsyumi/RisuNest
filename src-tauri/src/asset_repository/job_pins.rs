use super::migration_gc::AssetRootSet;
use super::{PayloadCas, PreparedPayload};
use crate::persistent_store::asset_object_catalog::{
    AssetObjectRegistration, ASSET_OBJECT_CATALOG_MAX_PAGE,
};
use crate::persistent_store::PersistentStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

const DURABLE_CAS_JOB_VERSION: u32 = 1;
const MAX_JOURNAL_RECORD_BYTES: usize = 64 * 1024;
const MAX_DURABLE_CAS_JOB_JOURNALS: usize = 4_096;
pub(crate) const MAX_DURABLE_CAS_JOB_PINS: usize = 100_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CasJobKind {
    DirectAssetOrInlayWrite,
    LocalBackupRestore,
    LosslessImport,
    CardOrModuleContentImport,
    OfficialPublicationOrExportPreparation,
    PeerClone,
    AndroidClone,
    LogicalDeltaTarget,
    ColdMigration,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PinDescriptor {
    byte_size: u64,
    role: CasObjectRole,
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
        created_at_ms: i64,
    },
    Pin {
        sequence: u64,
        job_id: String,
        object_hash: String,
        byte_size: u64,
        object_role: CasObjectRole,
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
        created_at_ms: i64,
    ) -> io::Result<Self> {
        validate_job_id(job_id)?;
        if created_at_ms < 0 {
            return invalid_data("CAS job creation time must be nonnegative");
        }
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
                created_at_ms,
            },
            true,
        )?;
        let _ = sync_directory(&directory)?;
        Ok(Self {
            repository_root,
            journal_path,
            state: JobState {
                job_id: job_id.to_owned(),
                kind,
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
        let state = read_job_state(&mut file, Some(job_id), true)?;
        Ok(Self {
            repository_root,
            journal_path,
            state,
        })
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
        self.record_pin(object_hash, byte_size, role)
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
            };
            if let Some(existing) = self.state.pins.get(object_hash) {
                if *existing != descriptor {
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
        if self
            .state
            .pins
            .len()
            .checked_add(pending.len())
            .is_none_or(|count| count > MAX_DURABLE_CAS_JOB_PINS)
        {
            return invalid_data("CAS job exceeds the bounded pin limit");
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
        let mut catalog = store.asset_object_catalog();
        for batch in registrations.chunks(ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
            catalog
                .register(batch, created_at_ms)
                .map_err(|error| io::Error::new(ErrorKind::InvalidData, error.to_string()))?;
        }
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

    pub(crate) fn root_set(&self) -> io::Result<AssetRootSet> {
        if !self.state.sealed || self.state.released {
            return invalid_data("only a sealed active CAS job has a direct root set");
        }
        Ok(root_set_from_state(&self.state))
    }

    pub(crate) fn release(&mut self, outcome: CasReleaseOutcome) -> io::Result<()> {
        if self.state.released {
            return self.cleanup_released_journal();
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
        let mut expected = BTreeMap::new();
        for (object_hash, byte_size, role) in pins {
            if expected
                .insert(
                    object_hash.clone(),
                    PinDescriptor {
                        byte_size: *byte_size,
                        role: *role,
                    },
                )
                .is_some_and(|existing| existing.byte_size != *byte_size || existing.role != *role)
            {
                return false;
            }
        }
        self.state.pins == expected
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
        self.record_pin(&prepared.content_hash, prepared.byte_size, role)
    }

    fn record_pin(
        &mut self,
        object_hash: &str,
        byte_size: u64,
        role: CasObjectRole,
    ) -> io::Result<()> {
        validate_hash(object_hash)?;
        let descriptor = PinDescriptor { byte_size, role };
        if let Some(existing) = self.state.pins.get(object_hash) {
            if *existing == descriptor {
                return Ok(());
            }
            return invalid_data("CAS job contains a conflicting object pin");
        }
        if self.state.pins.len() >= MAX_DURABLE_CAS_JOB_PINS {
            return invalid_data("CAS job exceeds the bounded pin limit");
        }
        let record = JobJournalRecord::Pin {
            sequence: self.state.next_sequence,
            job_id: self.state.job_id.clone(),
            object_hash: object_hash.to_owned(),
            byte_size,
            object_role: role,
        };
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_record(&mut file, &record, false)?;
        self.state.pins.insert(object_hash.to_owned(), descriptor);
        self.state.next_sequence += 1;
        Ok(())
    }
}

pub(crate) fn collect_durable_cas_job_roots(repository_root: &Path) -> AssetRootSet {
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
    let mut removed = false;
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
        let state =
            File::open(&path).and_then(|mut file| read_job_state(&mut file, Some(job_id), false));
        let state = match state {
            Ok(state) => state,
            Err(_) => {
                roots.blockers.insert(format!("job-pin-corrupt:{job_id}"));
                continue;
            }
        };
        if state.released {
            if fs::remove_file(&path).is_ok() {
                removed = true;
            } else {
                roots
                    .blockers
                    .insert(format!("job-pin-release-cleanup:{job_id}"));
            }
        } else {
            let job_roots = root_set_from_state(&state);
            roots.manifest_hashes.extend(job_roots.manifest_hashes);
            roots.object_hashes.extend(job_roots.object_hashes);
            if !state.sealed {
                roots.blockers.insert(format!("job-pin-unsealed:{job_id}"));
            }
        }
    }
    if removed && sync_directory(&directory).is_err() {
        roots.blockers.insert("job-pin-release-cleanup".to_owned());
    }
    roots
}

pub(crate) fn reclaim_abandoned_durable_cas_jobs(
    repository_root: &Path,
    job_id_prefix: &str,
    expected_kind: CasJobKind,
) -> io::Result<usize> {
    if job_id_prefix.is_empty()
        || job_id_prefix.len() >= 64
        || !job_id_prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return invalid_data("CAS job owner prefix is invalid");
    }
    let Some((repository_root, directory)) = job_pin_directory(repository_root, false)? else {
        return Ok(0);
    };
    let mut paths = Vec::new();
    for entry in fs::read_dir(&directory)? {
        paths.push(entry?.path());
        if paths.len() > MAX_DURABLE_CAS_JOB_JOURNALS {
            return invalid_data("CAS job journal limit exceeded");
        }
    }
    paths.sort();

    let mut candidates = Vec::new();
    for path in paths {
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(job_id) = file_name
            .strip_prefix("job-")
            .and_then(|name| name.strip_suffix(".journal"))
        else {
            continue;
        };
        if !job_id.starts_with(job_id_prefix) {
            continue;
        }
        validate_job_id(job_id)?;
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || is_link_like(&metadata) {
            return invalid_data("owned CAS job journal is not a real file");
        }
        let state = File::open(&path)
            .and_then(|mut file| read_job_state(&mut file, Some(job_id), false))?;
        if state.kind != expected_kind {
            return invalid_data("owned CAS job kind does not match its cleanup owner");
        }
        candidates.push((path, state));
    }

    let reclaimed = candidates.len();
    for (journal_path, state) in candidates {
        DurableCasJob {
            repository_root: repository_root.clone(),
            journal_path,
            state,
        }
        .release(CasReleaseOutcome::Aborted)?;
    }
    Ok(reclaimed)
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
    let assets = ensure_child_directory(&repository_root, "assets-v2", create)?;
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
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    invalid_data("CAS job hash must be a lowercase SHA-256 hash")
}

fn write_record(file: &mut File, record: &JobJournalRecord, sync: bool) -> io::Result<()> {
    let payload = serde_json::to_vec(record).map_err(json_error)?;
    if payload.len() > MAX_JOURNAL_RECORD_BYTES {
        return invalid_data("CAS job journal record is too large");
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, "CAS job record is too large"))?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&Sha256::digest(&payload))?;
    if sync {
        file.flush()?;
        file.sync_data()?;
    }
    Ok(())
}

fn read_job_state(
    file: &mut File,
    expected_job_id: Option<&str>,
    recover_tail: bool,
) -> io::Result<JobState> {
    file.seek(SeekFrom::Start(0))?;
    let mut state = None;
    let mut valid_length = 0_u64;
    loop {
        let mut length_bytes = [0_u8; 4];
        match read_exact_or_eof(file, &mut length_bytes)? {
            ExactRead::CleanEof => break,
            ExactRead::Partial => {
                recover_incomplete_tail(file, valid_length, recover_tail)?;
                break;
            }
            ExactRead::Complete => {}
        }
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > MAX_JOURNAL_RECORD_BYTES {
            return invalid_data("CAS job journal record length is invalid");
        }
        let mut payload = vec![0; length];
        if read_exact_or_eof(file, &mut payload)? != ExactRead::Complete {
            recover_incomplete_tail(file, valid_length, recover_tail)?;
            break;
        }
        let mut checksum = [0_u8; 32];
        if read_exact_or_eof(file, &mut checksum)? != ExactRead::Complete {
            recover_incomplete_tail(file, valid_length, recover_tail)?;
            break;
        }
        if Sha256::digest(&payload).as_slice() != checksum {
            return invalid_data("CAS job journal checksum mismatch");
        }
        let record: JobJournalRecord = serde_json::from_slice(&payload).map_err(json_error)?;
        apply_record(&mut state, record, expected_job_id)?;
        valid_length = file.stream_position()?;
    }
    state.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "CAS job journal is empty"))
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
            *state = Some(JobState {
                job_id,
                kind: job_kind,
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
        } => {
            let state = current_state(state, sequence, &job_id, expected_job_id)?;
            if state.sealed || state.released || state.pins.len() >= MAX_DURABLE_CAS_JOB_PINS {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExactRead {
    Complete,
    CleanEof,
    Partial,
}

fn read_exact_or_eof(reader: &mut impl Read, target: &mut [u8]) -> io::Result<ExactRead> {
    let mut offset = 0;
    while offset < target.len() {
        match reader.read(&mut target[offset..]) {
            Ok(0) if offset == 0 => return Ok(ExactRead::CleanEof),
            Ok(0) => return Ok(ExactRead::Partial),
            Ok(read) => offset += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ExactRead::Complete)
}

fn recover_incomplete_tail(file: &mut File, valid_length: u64, enabled: bool) -> io::Result<()> {
    if !enabled {
        return invalid_data("CAS job journal has a truncated frame");
    }
    file.set_len(valid_length)?;
    file.sync_data()?;
    file.seek(SeekFrom::Start(valid_length))?;
    Ok(())
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, error)
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, message.into()))
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<bool> {
    File::open(path)?.sync_all()?;
    Ok(true)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{
        collect_durable_cas_job_roots, reclaim_abandoned_durable_cas_jobs, write_record,
        CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob, JobJournalRecord,
        DURABLE_CAS_JOB_VERSION, MAX_DURABLE_CAS_JOB_PINS,
    };
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::asset_repository::PayloadCas;
    use crate::persistent_store::PersistentStore;
    use std::fs::OpenOptions;
    use std::io::Write;

    #[test]
    fn durable_job_supports_normal_fifty_thousand_asset_packages() {
        assert!(MAX_DURABLE_CAS_JOB_PINS >= 50_000);
    }

    #[test]
    fn abandoned_job_reclaim_releases_only_the_exact_owner_prefix() {
        let directory = tempfile::tempdir().expect("create reclaim directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");

        let mut sealed_owned = DurableCasJob::begin(
            directory.path(),
            "p4-delta-target-00000000-0000-4000-8000-000000000001",
            CasJobKind::LogicalDeltaTarget,
            1,
        )
        .expect("begin sealed owned job");
        let owned = sealed_owned
            .prepare_bytes(&cas, b"owned logical target", CasObjectRole::DirectObject)
            .expect("prepare owned object");
        sealed_owned.seal(&mut store, 2).expect("seal owned job");
        let sealed_owned_path = sealed_owned.journal_path().to_path_buf();
        drop(sealed_owned);

        let unsealed_owned = DurableCasJob::begin(
            directory.path(),
            "p4-delta-target-00000000-0000-4000-8000-000000000002",
            CasJobKind::LogicalDeltaTarget,
            3,
        )
        .expect("begin unsealed owned job");
        let unsealed_owned_path = unsealed_owned.journal_path().to_path_buf();
        drop(unsealed_owned);

        let canonical_job_id = "00000000-0000-4000-8000-000000000003";
        let mut unrelated = DurableCasJob::begin(
            directory.path(),
            canonical_job_id,
            CasJobKind::LogicalDeltaTarget,
            4,
        )
        .expect("begin unrelated logical target job");
        let unrelated_object = unrelated
            .prepare_bytes(
                &cas,
                b"unrelated logical target",
                CasObjectRole::DirectObject,
            )
            .expect("prepare unrelated object");
        unrelated.seal(&mut store, 5).expect("seal unrelated job");
        let unrelated_path = unrelated.journal_path().to_path_buf();
        drop(unrelated);

        assert_eq!(
            reclaim_abandoned_durable_cas_jobs(
                directory.path(),
                "p4-delta-target-",
                CasJobKind::LogicalDeltaTarget,
            )
            .expect("reclaim owned jobs"),
            2
        );

        assert!(!sealed_owned_path.exists());
        assert!(!unsealed_owned_path.exists());
        assert!(unrelated_path.exists());
        let roots = collect_durable_cas_job_roots(directory.path());
        assert!(!roots.object_hashes.contains(&owned.content_hash));
        assert!(roots.object_hashes.contains(&unrelated_object.content_hash));
    }

    #[test]
    fn abandoned_job_reclaim_preserves_an_owned_wrong_kind() {
        let directory = tempfile::tempdir().expect("create wrong-kind directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "p4-delta-target-00000000-0000-4000-8000-000000000004",
            CasJobKind::PeerClone,
            1,
        )
        .expect("begin wrong-kind job");
        let path = job.journal_path().to_path_buf();
        drop(job);

        let error = reclaim_abandoned_durable_cas_jobs(
            directory.path(),
            "p4-delta-target-",
            CasJobKind::LogicalDeltaTarget,
        )
        .expect_err("wrong-kind job must fail closed");

        assert!(error.to_string().contains("kind"));
        assert!(path.exists());
        assert!(collect_durable_cas_job_roots(directory.path())
            .blockers
            .iter()
            .any(|blocker| blocker.contains("p4-delta-target-")));
    }

    #[test]
    fn abandoned_job_reclaim_preserves_an_owned_corrupt_journal_byte_for_byte() {
        let directory = tempfile::tempdir().expect("create corrupt reclaim directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "p4-delta-target-00000000-0000-4000-8000-000000000005",
            CasJobKind::LogicalDeltaTarget,
            1,
        )
        .expect("begin corrupt candidate");
        let path = job.journal_path().to_path_buf();
        drop(job);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open corrupt candidate")
            .write_all(&[3, 0])
            .expect("append incomplete frame");
        let before = std::fs::read(&path).expect("read corrupt candidate");

        assert!(reclaim_abandoned_durable_cas_jobs(
            directory.path(),
            "p4-delta-target-",
            CasJobKind::LogicalDeltaTarget,
        )
        .is_err());

        assert_eq!(
            std::fs::read(&path).expect("reread corrupt candidate"),
            before
        );
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
    fn job_rejects_conflicting_existing_object_pins_and_recovers_by_session_id() {
        let directory = tempfile::tempdir().expect("create pin conflict directory");
        let cas = PayloadCas::new(directory.path()).expect("open CAS");
        let prepared = cas.prepare_bytes(b"same object").expect("prepare object");
        let mut job =
            DurableCasJob::begin(directory.path(), "peer-clone-1", CasJobKind::PeerClone, 1)
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

        let reopened = DurableCasJob::open(directory.path(), "peer-clone-1")
            .expect("reopen job by session id");
        assert_eq!(reopened.pin_count(), 1);
        assert_eq!(reopened.kind(), CasJobKind::PeerClone);
    }

    #[test]
    fn recovery_removes_released_journal_left_after_terminal_sync() {
        let directory = tempfile::tempdir().expect("create release recovery directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let mut job = DurableCasJob::begin(
            directory.path(),
            "released-job",
            CasJobKind::LosslessImport,
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
            CasJobKind::PeerClone,
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
    fn unsupported_journal_version_fails_closed() {
        let directory = tempfile::tempdir().expect("create invalid version directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "future-version-job",
            CasJobKind::AndroidClone,
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
                job_kind: CasJobKind::AndroidClone,
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
    fn native_file_job_startup_does_not_remove_cas_liveness_journals() {
        let directory = tempfile::tempdir().expect("create ownership boundary directory");
        let job = DurableCasJob::begin(
            directory.path(),
            "separate-owner-job",
            CasJobKind::ColdMigration,
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
            CasJobKind::LogicalDeltaTarget,
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
