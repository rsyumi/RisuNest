use super::journal_frame::{self, JournalFrameLabels};
use super::owner_manifest_codec::{decode_owner_manifest, owner_manifest_identity};
use super::payload_cas::{PayloadCas, PreparedPayload};
use crate::trust_boundary::is_lower_hex_256;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read},
    path::{Path, PathBuf},
};

const MIGRATION_JOURNAL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationPayloadKind {
    OrdinaryAsset,
    RestoredInlay,
    OwnerManifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStatus {
    pub migration_id: String,
    pub source_revision: i64,
    pub prepared_pins: u64,
    pub compatibility_hash: Option<String>,
    pub ready: bool,
}

// The staged-migration journal writer is exercised by persistent_store tests
// as a GC-blocking fixture until native asset migration writes these journals.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub struct StagedAssetMigration {
    journal_path: PathBuf,
    status: MigrationStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum MigrationJournalRecord {
    Begin {
        version: u32,
        migration_id: String,
        source_revision: i64,
    },
    Pin {
        kind: MigrationPayloadKind,
        object_hash: String,
        byte_size: u64,
    },
    Ready {
        source_revision: i64,
        compatibility_hash: String,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
impl StagedAssetMigration {
    pub fn begin(
        repository_root: &Path,
        migration_id: &str,
        source_revision: i64,
    ) -> io::Result<Self> {
        validate_migration_id(migration_id)?;
        if source_revision < 0 {
            return invalid_data("migration source revision must be nonnegative");
        }
        let journal_path = migration_journal_path(repository_root, migration_id)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&journal_path)?;
        write_journal_record(
            &mut file,
            &MigrationJournalRecord::Begin {
                version: MIGRATION_JOURNAL_VERSION,
                migration_id: migration_id.to_owned(),
                source_revision,
            },
        )?;
        Ok(Self {
            journal_path,
            status: MigrationStatus {
                migration_id: migration_id.to_owned(),
                source_revision,
                prepared_pins: 0,
                compatibility_hash: None,
                ready: false,
            },
        })
    }

    pub fn open(repository_root: &Path, migration_id: &str) -> io::Result<Self> {
        validate_migration_id(migration_id)?;
        let journal_path = migration_journal_path(repository_root, migration_id)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal_path)?;
        let status = read_migration_status(&mut file, Some(migration_id), true)?;
        Ok(Self {
            journal_path,
            status,
        })
    }

    pub fn prepare_opaque(
        &mut self,
        cas: &PayloadCas,
        kind: MigrationPayloadKind,
        reader: &mut impl Read,
    ) -> io::Result<PreparedPayload> {
        if self.status.ready {
            return invalid_data("ready migration cannot accept more payloads");
        }
        if kind == MigrationPayloadKind::OwnerManifest {
            return invalid_data("owner manifests require manifest validation");
        }
        let prepared = cas.prepare_reader(reader)?;
        self.record_pin(kind, &prepared)?;
        Ok(prepared)
    }

    pub fn prepare_owner_manifest(
        &mut self,
        cas: &PayloadCas,
        canonical_bytes: &[u8],
        expected_entry_count: usize,
    ) -> io::Result<PreparedPayload> {
        if self.status.ready {
            return invalid_data("ready migration cannot accept more manifests");
        }
        let entries = decode_owner_manifest(canonical_bytes)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        if entries.len() != expected_entry_count {
            return invalid_data("owner manifest entry count does not match migration metadata");
        }
        let expected_hash = owner_manifest_identity(canonical_bytes);
        let prepared = cas.prepare_bytes(canonical_bytes)?;
        if prepared.content_hash != expected_hash {
            return invalid_data("owner manifest CAS identity mismatch");
        }
        self.record_pin(MigrationPayloadKind::OwnerManifest, &prepared)?;
        Ok(prepared)
    }

    pub fn seal(&mut self, actual_revision: i64, compatibility_hash: &str) -> io::Result<()> {
        validate_hash(compatibility_hash, "compatibility hash")?;
        if actual_revision != self.status.source_revision {
            return invalid_data("migration source revision changed before activation");
        }
        if self.status.ready {
            return invalid_data("migration is already ready");
        }
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_journal_record(
            &mut file,
            &MigrationJournalRecord::Ready {
                source_revision: actual_revision,
                compatibility_hash: compatibility_hash.to_owned(),
            },
        )?;
        self.status.ready = true;
        self.status.compatibility_hash = Some(compatibility_hash.to_owned());
        Ok(())
    }

    pub fn status(&self) -> &MigrationStatus {
        &self.status
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub fn root_set(&self) -> io::Result<AssetRootSet> {
        let mut file = OpenOptions::new().read(true).open(&self.journal_path)?;
        let mut roots = AssetRootSet::default();
        scan_journal_records(&mut file, false, |record| {
            if let MigrationJournalRecord::Pin {
                object_hash, kind, ..
            } = record
            {
                match kind {
                    MigrationPayloadKind::OwnerManifest => {
                        roots.manifest_hashes.insert(object_hash.clone());
                    }
                    MigrationPayloadKind::OrdinaryAsset | MigrationPayloadKind::RestoredInlay => {
                        roots.object_hashes.insert(object_hash.clone());
                    }
                }
            }
            Ok(())
        })?;
        Ok(roots)
    }

    fn record_pin(
        &mut self,
        kind: MigrationPayloadKind,
        prepared: &PreparedPayload,
    ) -> io::Result<()> {
        validate_hash(&prepared.content_hash, "prepared object hash")?;
        let mut file = OpenOptions::new().append(true).open(&self.journal_path)?;
        write_journal_record(
            &mut file,
            &MigrationJournalRecord::Pin {
                kind,
                object_hash: prepared.content_hash.clone(),
                byte_size: prepared.byte_size,
            },
        )?;
        self.status.prepared_pins = self.status.prepared_pins.checked_add(1).ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidData, "migration pin count overflow")
        })?;
        Ok(())
    }
}

pub fn collect_staged_migration_roots(repository_root: &Path) -> io::Result<Vec<AssetRootSet>> {
    let repository_root = fs::canonicalize(repository_root)?;
    let directory = repository_root.join("assets-v2").join("migrations");
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return invalid_data("migration journal directory is not a real directory");
    }
    let directory = fs::canonicalize(directory)?;
    if !directory.starts_with(&repository_root) {
        return invalid_data("migration journal directory escapes repository root");
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return invalid_data("migration journal file name is invalid");
        };
        if name.starts_with("migration-") && name.ends_with(".journal") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut root_sets = Vec::with_capacity(paths.len());
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    "migration journal file name is invalid",
                )
            })?;
        let migration_id = file_name
            .strip_prefix("migration-")
            .and_then(|name| name.strip_suffix(".journal"))
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid journal file name"))?;
        validate_migration_id(migration_id)?;
        let mut file = OpenOptions::new().read(true).open(&path)?;
        let status = read_migration_status(&mut file, Some(migration_id), false)?;
        let mut roots = AssetRootSet::default();
        scan_journal_records(&mut file, false, |record| {
            if let MigrationJournalRecord::Pin {
                object_hash, kind, ..
            } = record
            {
                match kind {
                    MigrationPayloadKind::OwnerManifest => {
                        roots.manifest_hashes.insert(object_hash.clone());
                    }
                    MigrationPayloadKind::OrdinaryAsset | MigrationPayloadKind::RestoredInlay => {
                        roots.object_hashes.insert(object_hash.clone());
                    }
                }
            }
            Ok(())
        })?;
        if !status.ready {
            roots
                .blockers
                .insert(format!("staged-migration-unready:{migration_id}"));
        }
        root_sets.push(roots);
    }
    Ok(root_sets)
}

#[cfg_attr(not(test), allow(dead_code))]
fn migration_journal_path(repository_root: &Path, migration_id: &str) -> io::Result<PathBuf> {
    let repository_root = fs::canonicalize(repository_root)?;
    let directory = repository_root.join("assets-v2").join("migrations");
    fs::create_dir_all(&directory)?;
    let directory = fs::canonicalize(directory)?;
    if !directory.starts_with(&repository_root) {
        return invalid_data("migration journal directory escapes repository root");
    }
    Ok(directory.join(format!("migration-{migration_id}.journal")))
}

fn validate_migration_id(migration_id: &str) -> io::Result<()> {
    if migration_id.is_empty()
        || migration_id.len() > 64
        || !migration_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return invalid_data("migration id must contain only ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn validate_hash(hash: &str, context: &str) -> io::Result<()> {
    if is_lower_hex_256(hash) {
        return Ok(());
    }
    invalid_data(format!("{context} must be a lowercase SHA-256 hash"))
}

#[cfg_attr(not(test), allow(dead_code))]
const JOURNAL_LABELS: JournalFrameLabels = JournalFrameLabels {
    record_too_large: "migration record is too large",
    length_invalid: "migration journal record length is invalid",
    checksum_mismatch: "migration journal checksum mismatch",
};

fn write_journal_record(file: &mut File, record: &MigrationJournalRecord) -> io::Result<()> {
    journal_frame::write_frame(file, record, true, &JOURNAL_LABELS)
}

fn read_migration_status(
    file: &mut File,
    expected_id: Option<&str>,
    recover_trailing_frame: bool,
) -> io::Result<MigrationStatus> {
    let mut status = None;
    scan_journal_records(file, recover_trailing_frame, |record| {
        apply_journal_record(&mut status, record.clone(), expected_id)
    })?;
    status.ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "migration journal is empty"))
}

fn scan_journal_records(
    file: &mut File,
    recover_trailing_frame: bool,
    mut visitor: impl FnMut(&MigrationJournalRecord) -> io::Result<()>,
) -> io::Result<()> {
    let scan =
        journal_frame::scan_frames(file, &JOURNAL_LABELS, |record: MigrationJournalRecord| {
            visitor(&record)
        })?;
    if let Some(valid_length) = scan.incomplete_tail {
        if !recover_trailing_frame {
            return invalid_data("migration journal has a truncated frame");
        }
        journal_frame::truncate_to_valid_prefix(file, valid_length)?;
    }
    Ok(())
}

fn apply_journal_record(
    status: &mut Option<MigrationStatus>,
    record: MigrationJournalRecord,
    expected_id: Option<&str>,
) -> io::Result<()> {
    match record {
        MigrationJournalRecord::Begin {
            version,
            migration_id,
            source_revision,
        } => {
            if status.is_some() {
                return invalid_data("migration journal contains a duplicate begin record");
            }
            if version != MIGRATION_JOURNAL_VERSION {
                return invalid_data("unsupported migration journal version");
            }
            validate_migration_id(&migration_id)?;
            if expected_id.is_some_and(|expected| expected != migration_id) {
                return invalid_data("migration journal id does not match its file");
            }
            if source_revision < 0 {
                return invalid_data("migration source revision must be nonnegative");
            }
            *status = Some(MigrationStatus {
                migration_id,
                source_revision,
                prepared_pins: 0,
                compatibility_hash: None,
                ready: false,
            });
        }
        MigrationJournalRecord::Pin {
            kind: _,
            object_hash,
            byte_size: _,
        } => {
            validate_hash(&object_hash, "migration pin hash")?;
            let current = status.as_mut().ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "migration pin precedes begin")
            })?;
            if current.ready {
                return invalid_data("migration pin follows ready record");
            }
            current.prepared_pins = current.prepared_pins.checked_add(1).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "migration pin count overflow")
            })?;
        }
        MigrationJournalRecord::Ready {
            source_revision,
            compatibility_hash,
        } => {
            validate_hash(&compatibility_hash, "compatibility hash")?;
            let current = status.as_mut().ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    "migration ready record precedes begin",
                )
            })?;
            if current.ready {
                return invalid_data("migration journal contains duplicate ready records");
            }
            if current.source_revision != source_revision {
                return invalid_data("migration ready revision does not match source revision");
            }
            current.compatibility_hash = Some(compatibility_hash);
            current.ready = true;
        }
    }
    Ok(())
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, error)
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, message.into()))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AssetRootSet {
    pub manifest_hashes: BTreeSet<String>,
    pub object_hashes: BTreeSet<String>,
    pub legacy_asset_keys: BTreeSet<String>,
    pub inlay_ids: BTreeSet<String>,
    pub cold_keys: BTreeSet<String>,
    pub blockers: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub retain_all_objects: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) fn validate_root_set(roots: &AssetRootSet) -> io::Result<()> {
    for hash in roots.manifest_hashes.iter().chain(&roots.object_hashes) {
        validate_hash(hash, "asset root hash")?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetGcCandidate {
    pub object_hash: String,
    pub byte_size: u64,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetGcDryRunReport {
    pub marked_hashes: Vec<String>,
    pub grace_retained_hashes: Vec<String>,
    pub potential_delete_hashes: Vec<String>,
    pub potential_delete_bytes: u64,
    pub deleted_hashes: Vec<String>,
    pub deleted_bytes: u64,
    pub blockers: Vec<String>,
    pub deletion_enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssetGcDeleteHookPoint {
    AfterInitialScan,
    AfterTombstone,
    AfterUnlink,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetGcDryRunPage {
    pub(crate) report: AssetGcDryRunReport,
    pub(crate) next_cursor: Option<String>,
}

pub fn dry_run_mark_and_sweep(
    cas: &PayloadCas,
    candidates: impl IntoIterator<Item = AssetGcCandidate>,
    roots: impl IntoIterator<Item = AssetRootSet>,
    now_ms: i64,
    minimum_grace_ms: i64,
) -> io::Result<AssetGcDryRunReport> {
    dry_run_mark_and_sweep_with_remote(cas, candidates, roots, now_ms, minimum_grace_ms, |_| {
        Ok(None)
    })
}
pub(crate) fn dry_run_mark_and_sweep_with_remote(
    cas: &PayloadCas,
    candidates: impl IntoIterator<Item = AssetGcCandidate>,
    roots: impl IntoIterator<Item = AssetRootSet>,
    now_ms: i64,
    minimum_grace_ms: i64,
    remote: impl Fn(&str) -> io::Result<Option<u64>>,
) -> io::Result<AssetGcDryRunReport> {
    if now_ms < 0 || minimum_grace_ms < 0 {
        return invalid_data("asset GC timestamps must be nonnegative");
    }
    let cutoff = now_ms.saturating_sub(minimum_grace_ms);
    let mut manifest_hashes = BTreeSet::new();
    let mut marked_hashes = BTreeSet::new();
    let mut blockers = BTreeSet::new();
    let mut retain_all_objects = false;
    for roots in roots {
        validate_root_set(&roots)?;
        retain_all_objects |= roots.retain_all_objects
            || roots.blockers.contains("plugin-storage-opaque")
            || roots.blockers.contains("cold-payload-unscanned");
        manifest_hashes.extend(roots.manifest_hashes);
        marked_hashes.extend(roots.object_hashes);
        blockers.extend(roots.blockers);
        if !roots.legacy_asset_keys.is_empty() {
            blockers.insert("legacy-asset-roots-unresolved".to_owned());
        }
        if !roots.inlay_ids.is_empty() {
            blockers.insert("inlay-roots-unresolved".to_owned());
        }
        if !roots.cold_keys.is_empty() {
            blockers.insert("cold-payload-unscanned".to_owned());
        }
    }
    for manifest_hash in manifest_hashes {
        let canonical = cas
            .read_object(&manifest_hash)?
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "root manifest is missing"))?;
        if owner_manifest_identity(&canonical) != manifest_hash {
            return invalid_data("root manifest content hash mismatch");
        }
        let entries = decode_owner_manifest(&canonical)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        marked_hashes.insert(manifest_hash);
        for entry in entries {
            if let Some(hash) = entry.payload_hash {
                marked_hashes.insert(hex::encode(hash));
            }
        }
    }
    for hash in &marked_hashes {
        if cas.stat_object(hash)?.is_none() && remote(hash)?.is_none() {
            return invalid_data("marked CAS object is missing");
        }
    }

    let mut seen_candidates = BTreeSet::new();
    let mut grace_retained_hashes = Vec::new();
    let mut potential_delete_hashes = Vec::new();
    let mut potential_delete_bytes = 0_u64;
    for candidate in candidates {
        validate_hash(&candidate.object_hash, "asset GC candidate hash")?;
        if candidate.created_at_ms < 0 || !seen_candidates.insert(candidate.object_hash.clone()) {
            return invalid_data("asset GC candidate is invalid or duplicated");
        }
        let physical = cas.stat_object(&candidate.object_hash)?;
        let actual_size = physical
            .or(remote(&candidate.object_hash)?)
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "asset GC candidate is missing")
            })?;
        if actual_size != candidate.byte_size {
            return invalid_data("asset GC candidate size mismatch");
        }
        // Server custody cleanup owns removal of remote-only catalog entries.
        // Physical GC must not claim disk space was freed for these entries.
        if physical.is_none() {
            continue;
        }
        if retain_all_objects {
            marked_hashes.insert(candidate.object_hash);
            continue;
        }
        if marked_hashes.contains(&candidate.object_hash) {
            continue;
        }
        if candidate.created_at_ms > cutoff {
            grace_retained_hashes.push(candidate.object_hash);
        } else {
            potential_delete_bytes = potential_delete_bytes
                .checked_add(candidate.byte_size)
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "GC byte count overflow"))?;
            potential_delete_hashes.push(candidate.object_hash);
        }
    }
    grace_retained_hashes.sort();
    potential_delete_hashes.sort();
    Ok(AssetGcDryRunReport {
        marked_hashes: marked_hashes.into_iter().collect(),
        grace_retained_hashes,
        potential_delete_hashes,
        potential_delete_bytes,
        deleted_hashes: Vec::new(),
        deleted_bytes: 0,
        blockers: blockers.into_iter().collect(),
        deletion_enabled: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        collect_staged_migration_roots, dry_run_mark_and_sweep, AssetGcCandidate, AssetRootSet,
        MigrationPayloadKind, StagedAssetMigration,
    };
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::asset_repository::payload_cas::PayloadCas;
    use std::{fs::OpenOptions, io::Write};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn staged_migration_reopens_after_an_interrupted_tail_and_preserves_opaque_bytes() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let ordinary = [0, 255, 1, 7, 42];
        let restored_inlay = b"opaque-existing-inlay";
        let mut migration =
            StagedAssetMigration::begin(directory.path(), "migration-1", 7).unwrap();

        let ordinary_prepared = migration
            .prepare_opaque(
                &cas,
                MigrationPayloadKind::OrdinaryAsset,
                &mut ordinary.as_slice(),
            )
            .unwrap();
        let inlay_prepared = migration
            .prepare_opaque(
                &cas,
                MigrationPayloadKind::RestoredInlay,
                &mut restored_inlay.as_slice(),
            )
            .unwrap();

        assert_eq!(
            cas.read_object(&ordinary_prepared.content_hash).unwrap(),
            Some(ordinary.to_vec())
        );
        assert_eq!(
            cas.read_object(&inlay_prepared.content_hash).unwrap(),
            Some(restored_inlay.to_vec())
        );

        OpenOptions::new()
            .append(true)
            .open(migration.journal_path())
            .unwrap()
            .write_all(&[8, 0, 0])
            .unwrap();
        drop(migration);

        let reopened = StagedAssetMigration::open(directory.path(), "migration-1").unwrap();
        assert_eq!(reopened.status().migration_id, "migration-1");
        assert_eq!(reopened.status().source_revision, 7);
        assert_eq!(reopened.status().prepared_pins, 2);
        assert!(!reopened.status().ready);
    }

    #[test]
    fn staged_migration_seal_rejects_a_stale_revision_and_survives_reopen() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let mut migration =
            StagedAssetMigration::begin(directory.path(), "migration-cas", 11).unwrap();

        let error = migration.seal(12, HASH).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("revision"));
        assert!(!migration.status().ready);

        migration.seal(11, HASH).unwrap();
        drop(migration);

        let reopened = StagedAssetMigration::open(directory.path(), "migration-cas").unwrap();
        assert!(reopened.status().ready);
        assert_eq!(reopened.status().source_revision, 11);
        assert_eq!(reopened.status().compatibility_hash.as_deref(), Some(HASH));
    }

    #[test]
    fn staged_owner_manifest_validates_entries_before_pinning_canonical_bytes() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let entries = vec![OwnerManifestEntry {
            tuple: [
                "name".to_owned(),
                "assets/exact.bin".to_owned(),
                "BIN".to_owned(),
            ],
            payload_hash: Some([7; 32]),
        }];
        let canonical = encode_owner_manifest(&entries).unwrap();
        let mut migration =
            StagedAssetMigration::begin(directory.path(), "migration-manifest", 4).unwrap();

        let error = migration
            .prepare_owner_manifest(&cas, &canonical, 2)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(migration.status().prepared_pins, 0);

        let prepared = migration
            .prepare_owner_manifest(&cas, &canonical, 1)
            .unwrap();
        assert_eq!(migration.status().prepared_pins, 1);
        assert_eq!(
            cas.read_object(&prepared.content_hash).unwrap(),
            Some(canonical)
        );
        assert_eq!(
            migration.root_set().unwrap().manifest_hashes,
            [prepared.content_hash].into()
        );
    }

    #[test]
    fn staged_migration_rejects_a_complete_corrupt_frame() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let migration =
            StagedAssetMigration::begin(directory.path(), "migration-corrupt", 3).unwrap();
        let mut journal = OpenOptions::new()
            .append(true)
            .open(migration.journal_path())
            .unwrap();
        journal.write_all(&(2_u32).to_le_bytes()).unwrap();
        journal.write_all(b"{}").unwrap();
        journal.write_all(&[0; 32]).unwrap();
        journal.sync_all().unwrap();
        drop(journal);
        drop(migration);

        let error = StagedAssetMigration::open(directory.path(), "migration-corrupt")
            .expect_err("corrupt frame must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum"));
    }

    #[test]
    fn opaque_snapshot_blockers_retain_every_catalog_candidate() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let candidate = cas.prepare_bytes(b"opaque-snapshot-candidate").unwrap();
        let mut roots = AssetRootSet::default();
        roots.blockers.insert("plugin-storage-opaque".to_owned());
        roots.blockers.insert("cold-payload-unscanned".to_owned());
        let report = dry_run_mark_and_sweep(
            &cas,
            [AssetGcCandidate {
                object_hash: candidate.content_hash.clone(),
                byte_size: candidate.byte_size,
                created_at_ms: 0,
            }],
            [roots],
            100,
            10,
        )
        .unwrap();

        assert_eq!(report.marked_hashes, vec![candidate.content_hash]);
        assert!(report.potential_delete_hashes.is_empty());
        assert!(!report.deletion_enabled);
    }

    #[test]
    fn dry_run_marks_manifest_payloads_and_staged_pins_without_deleting_files() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let kept = cas.prepare_bytes(b"kept-by-manifest").unwrap();
        let staged = cas.prepare_bytes(b"kept-by-staging").unwrap();
        let collectable = cas.prepare_bytes(b"collectable").unwrap();
        let young = cas.prepare_bytes(b"young").unwrap();
        let manifest_bytes = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "kept".to_owned(),
                "assets/kept.bin".to_owned(),
                "bin".to_owned(),
            ],
            payload_hash: Some(hex::decode(&kept.content_hash).unwrap().try_into().unwrap()),
        }])
        .unwrap();
        let manifest = cas.prepare_bytes(&manifest_bytes).unwrap();
        let mut migration =
            StagedAssetMigration::begin(directory.path(), "migration-gc-pin", 1).unwrap();
        migration
            .prepare_opaque(
                &cas,
                MigrationPayloadKind::OrdinaryAsset,
                &mut b"kept-by-staging".as_slice(),
            )
            .unwrap();
        let mut roots = AssetRootSet::default();
        roots.manifest_hashes.insert(manifest.content_hash.clone());

        let report = dry_run_mark_and_sweep(
            &cas,
            [
                candidate(&kept, 0),
                candidate(&staged, 0),
                candidate(&collectable, 0),
                candidate(&young, 95),
                candidate(&manifest, 0),
            ],
            std::iter::once(roots).chain(collect_staged_migration_roots(directory.path()).unwrap()),
            100,
            10,
        )
        .unwrap();

        assert_eq!(
            report.marked_hashes,
            sorted([
                kept.content_hash.clone(),
                staged.content_hash.clone(),
                manifest.content_hash.clone(),
            ])
        );
        assert_eq!(
            report.grace_retained_hashes,
            vec![young.content_hash.clone()]
        );
        assert_eq!(
            report.potential_delete_hashes,
            vec![collectable.content_hash.clone()]
        );
        assert_eq!(report.potential_delete_bytes, collectable.byte_size);
        assert_eq!(
            report.blockers,
            vec!["staged-migration-unready:migration-gc-pin".to_owned()]
        );
        assert!(!report.deletion_enabled);
        assert!(cas
            .stat_object(&collectable.content_hash)
            .unwrap()
            .is_some());
    }

    #[test]
    fn dry_run_retains_candidates_for_conservative_plugin_and_cold_blockers() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let candidate_payload = cas.prepare_bytes(b"unmarked").unwrap();
        let mut roots = AssetRootSet::default();
        roots.blockers.extend([
            "cold-payload-unscanned".to_owned(),
            "plugin-storage-opaque".to_owned(),
        ]);

        let report =
            dry_run_mark_and_sweep(&cas, [candidate(&candidate_payload, 0)], [roots], 100, 10)
                .unwrap();

        assert_eq!(report.marked_hashes, vec![candidate_payload.content_hash]);
        assert!(report.potential_delete_hashes.is_empty());
        assert_eq!(report.potential_delete_bytes, 0);
        assert_eq!(
            report.blockers,
            vec![
                "cold-payload-unscanned".to_owned(),
                "plugin-storage-opaque".to_owned(),
            ]
        );
        assert!(!report.deletion_enabled);
    }

    #[test]
    fn dry_run_fails_closed_when_a_root_manifest_is_corrupt() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        let manifest = cas
            .prepare_bytes(&encode_owner_manifest(&[]).unwrap())
            .unwrap();
        std::fs::write(directory.path().join(&manifest.physical_key), b"corrupt").unwrap();
        let mut roots = AssetRootSet::default();
        roots.manifest_hashes.insert(manifest.content_hash.clone());

        let error = dry_run_mark_and_sweep(&cas, [], [roots], 100, 10)
            .expect_err("corrupt root manifest must abort the mark pass");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    fn candidate(
        prepared: &crate::asset_repository::payload_cas::PreparedPayload,
        created_at_ms: i64,
    ) -> AssetGcCandidate {
        AssetGcCandidate {
            object_hash: prepared.content_hash.clone(),
            byte_size: prepared.byte_size,
            created_at_ms,
        }
    }

    fn sorted<const N: usize>(values: [String; N]) -> Vec<String> {
        let mut values = values.into_iter().collect::<Vec<_>>();
        values.sort();
        values
    }
}
