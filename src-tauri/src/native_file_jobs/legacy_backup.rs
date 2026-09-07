use super::{
    restore, JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError, OpenedJobSource,
};
use crate::asset_repository::job_pins::{
    CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
};
use crate::asset_repository::{owner_manifest_codec, PayloadCas};
use crate::local_backup::{
    parse_legacy_local_backup_v1, write_legacy_local_backup_v1, CancellationProbe,
    LegacyBackupWriteEntry, LegacyBackupWriteSource, LocalBackupError, LocalBackupErrorCode,
    PayloadTarget, StagedLocalBackupEntry, StrictLocalBackupDatabaseRestore,
};
use crate::persistent_store::export::{self, destination};
use crate::persistent_store::{
    AssetAlias, AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState, ColdAlias,
    ColdPayloadAuthorityState, PersistentStore, RevisionResult, StagingResult, StoreResult,
};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

mod pocket_risu;
#[cfg(test)]
mod pocket_risu_tests;

const DATABASE_ENTRY: &str = "database.risudat";
const ENCRYPTION_ENTRY: &str = "encryption.risudat";
const MAX_METADATA_BYTES: u32 = 1024 * 1024;
const MAX_OWNER_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const ARCHIVE_FILE: &str = "archive.bin.part";

struct JobCancellation<'a>(&'a JobControl);

impl CancellationProbe for JobCancellation<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancel_requested()
    }
}

pub(crate) fn restore_legacy_local_backup(
    mut source: OpenedJobSource,
    expected_revision: i64,
    owned_directory: &std::path::Path,
    repository_root: &std::path::Path,
    app: AppHandle,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::ReadingSource)
        .map_err(job_state_error)?;
    let cas = PayloadCas::new(repository_root).map_err(io_job_error)?;
    let cancellation = JobCancellation(job);
    let mut callback = LegacyDatabaseRestore {
        app,
        cas: &cas,
        repository_root,
        expected_revision,
        job,
        result: None,
    };
    let parsed = parse_legacy_local_backup_v1(
        &mut source.file,
        owned_directory,
        PayloadTarget::JobStaging,
        &mut callback,
        &cancellation,
    );
    match parsed {
        Ok(report) => {
            let mut result = callback.result.take().ok_or_else(|| {
                NativeJobError::new("store-error", "legacy backup database restore did not run")
            })??;
            result.source_bytes = report.source_bytes;
            result.source_sha256 = report.source_sha256;
            Ok(result)
        }
        Err(error) => match callback.result.take() {
            Some(Err(native)) => Err(native),
            _ => Err(local_backup_error(error)),
        },
    }
}

pub(crate) fn export_legacy_local_backup(
    destination_path: Option<&std::path::Path>,
    expected_revision: i64,
    owned_directory: &std::path::Path,
    handoff_directory: &std::path::Path,
    mut store: PersistentStore,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::WritingExport)
        .map_err(job_state_error)?;
    let cancellation = JobCancellation(job);
    let repository_root = store.repository_root().to_path_buf();
    let cas = PayloadCas::new(&repository_root).map_err(io_job_error)?;
    let mut prepared = store
        .prepare_risu_save_export(expected_revision)
        .map_err(store_job_error)?;
    let reader = prepared.take_reader().map_err(store_job_error)?;
    let mut durable = None;
    let mut database_path = None;
    let outcome = (|| {
        let inventory = export::pinned_legacy_backup_inventory(&reader.connection, &reader.target)
            .map_err(store_job_error)?;
        if !matches!(
            inventory.asset_authority,
            AssetRepositoryAuthorityState::V2 { .. }
        ) || !matches!(
            inventory.cold_authority,
            ColdPayloadAuthorityState::V2 { .. }
        ) {
            return Err(NativeJobError::new(
                "capability-unavailable",
                "native legacy backup export requires migrated asset and cold repositories",
            ));
        }
        if inventory.revision != expected_revision {
            return Err(NativeJobError::new(
                "revision-conflict",
                "legacy backup payload inventory does not match the requested revision",
            ));
        }
        durable = Some(
            DurableCasJob::begin(
                &repository_root,
                &job.id(),
                CasJobKind::OfficialPublicationOrExportPreparation,
                now_millis(),
            )
            .map_err(io_job_error)?,
        );
        let pins = durable
            .as_mut()
            .expect("durable CAS job was just initialized");
        let owner_manifest_hashes = inventory
            .owner_heads
            .iter()
            .filter(|head| head.present)
            .filter_map(|head| head.manifest_hash.as_deref())
            .collect::<HashSet<_>>();
        for (hash, size) in inventory
            .assets
            .iter()
            .map(|alias| (&alias.object_hash, alias.size))
            .chain(
                inventory
                    .cold
                    .iter()
                    .map(|alias| (&alias.object_hash, alias.size)),
            )
        {
            let hash = hash.as_deref().ok_or_else(|| {
                NativeJobError::new(
                    "invalid-source",
                    "legacy backup alias is missing its object hash",
                )
            })?;
            let role = if owner_manifest_hashes.contains(hash) {
                CasObjectRole::OwnerManifest
            } else {
                CasObjectRole::DirectObject
            };
            pins.pin_existing(&cas, hash, size as u64, role)
                .map_err(io_job_error)?;
        }
        let owner_projection = prepare_owner_export_projection(
            &cas,
            &inventory.assets,
            &inventory.owner_heads,
            &owner_manifest_hashes,
            pins,
            &cancellation,
        )?;
        pins.seal(&mut store, now_millis()).map_err(io_job_error)?;

        let database = export::create_legacy_backup_controlled(
            &reader.connection,
            &prepared.snapshots_dir,
            &reader.target,
            &prepared.lease,
            owner_projection.replacement_keys,
            || job.is_cancel_requested(),
            |completed_bytes, completed_items, total_items| {
                let _ = job.set_progress(JobProgress {
                    completed_bytes,
                    total_bytes: None,
                    completed_items,
                    total_items: Some(total_items),
                });
            },
        )
        .map_err(store_job_error)?;
        database_path = Some(std::path::PathBuf::from(&database.path));
        let mut entries = materialize_export_entries(
            owned_directory,
            &cas,
            &inventory.assets,
            &inventory.cold,
            &cancellation,
        )?;
        entries.extend(materialize_owner_export_entries(
            &cas,
            &owner_projection.payloads,
            &cancellation,
        )?);
        entries.push(LegacyBackupWriteEntry {
            logical_name: DATABASE_ENTRY.to_owned(),
            source: LegacyBackupWriteSource::File(database.path.clone().into()),
        });
        let archive_path = owned_directory.join(ARCHIVE_FILE);
        let mut archive = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&archive_path)
            .map_err(io_job_error)?;
        let report = write_legacy_local_backup_v1(&mut archive, &entries, &cancellation)
            .map_err(local_backup_error)?;
        if !report.is_complete() {
            return Err(NativeJobError::new(
                "invalid-source",
                "legacy backup inventory contains missing payloads",
            ));
        }
        archive.sync_all().map_err(io_job_error)?;
        job.set_phase(JobPhase::PublishingDestination)
            .map_err(job_state_error)?;
        let handoff_path;
        let (destination_root, destination_path, is_handoff) = match destination_path {
            Some(destination_path) => {
                let destination_root = destination_path.parent().ok_or_else(|| {
                    NativeJobError::new(
                        "invalid-destination",
                        "legacy backup destination has no parent",
                    )
                })?;
                (destination_root, destination_path, false)
            }
            None => {
                handoff_path =
                    handoff_directory.join(format!("risu-backup-{}.bin", Uuid::new_v4()));
                (handoff_directory, handoff_path.as_path(), true)
            }
        };
        let phase_error = RefCell::new(None);
        let published = destination::write_legacy_backup_destination_controlled(
            owned_directory,
            &archive_path,
            destination_root,
            destination_path,
            || job.is_cancel_requested() || phase_error.borrow().is_some(),
            |progress| {
                let _ = job.set_progress(JobProgress {
                    completed_bytes: report.archive_bytes.saturating_add(progress.copied_bytes),
                    total_bytes: Some(report.archive_bytes.saturating_mul(2)),
                    completed_items: report.written_entries,
                    total_items: Some(report.written_entries),
                });
            },
            || {
                job.set_phase(JobPhase::FinalizingExport).map_err(|error| {
                    *phase_error.borrow_mut() = Some(error);
                    destination::DestinationWriteError::Cancelled
                })
            },
        )
        .map_err(|error| destination_job_error(error, phase_error.into_inner()))?;
        Ok(JobResultSummary {
            revision: expected_revision,
            source_bytes: published.bytes,
            source_sha256: published.sha256,
            character_count: database.character_count,
            preset_count: database.preset_count,
            warning_codes: Vec::new(),
            handoff_path: is_handoff.then(|| destination_path.to_string_lossy().into_owned()),
            recovery_path: None,
            publication: None,
        })
    })();
    let release_reader = prepared.release(reader).map_err(store_job_error);
    let cleanup_database = database_path
        .as_deref()
        .map(|path| prepared.cleanup_file(path).map_err(store_job_error))
        .unwrap_or(Ok(()));
    let durable_release = durable
        .as_mut()
        .map(|durable| {
            durable
                .release(if outcome.is_ok() {
                    CasReleaseOutcome::Committed
                } else {
                    CasReleaseOutcome::Aborted
                })
                .map_err(io_job_error)
        })
        .unwrap_or(Ok(()));
    match (outcome, release_reader, cleanup_database, durable_release) {
        (Ok(result), Ok(()), Ok(()), Ok(())) => Ok(result),
        (Ok(mut result), _, _, _) => {
            result.warning_codes.push("cleanup-failed".to_owned());
            Ok(result)
        }
        (Err(error), Ok(()), Ok(()), Ok(())) => Err(error),
        (Err(error), _, _, _) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{}; legacy backup cleanup failed", error.message),
        )),
    }
}

fn materialize_export_entries(
    owned_directory: &std::path::Path,
    cas: &PayloadCas,
    assets: &[AssetAlias],
    cold: &[ColdAlias],
    cancellation: &dyn CancellationProbe,
) -> Result<Vec<LegacyBackupWriteEntry>, NativeJobError> {
    let mut entries = Vec::with_capacity(assets.len() + cold.len() + 1);
    let mut names = HashSet::new();
    for (index, alias) in assets.iter().enumerate() {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let hash = alias.object_hash.as_deref().ok_or_else(|| {
            NativeJobError::new("invalid-source", "legacy backup asset has no object hash")
        })?;
        let object_path = cas
            .object_path(hash)
            .map_err(io_job_error)?
            .ok_or_else(|| {
                NativeJobError::new("invalid-source", "legacy backup asset object is missing")
            })?;
        let (name, source) = if alias.kind == "inlay" {
            let name = format!("inlay_{}.risuinlay", hex::encode(alias.key.as_bytes()));
            let path = owned_directory.join(format!("inlay-{index}.entry"));
            write_inlay_entry(&path, alias, &object_path, cancellation)?;
            (name, path)
        } else {
            (legacy_asset_entry_name(&alias.key)?, object_path)
        };
        if !names.insert(name.clone()) {
            return Err(NativeJobError::new(
                "invalid-source",
                format!("legacy backup has duplicate entry name: {name}"),
            ));
        }
        entries.push(LegacyBackupWriteEntry {
            logical_name: name,
            source: LegacyBackupWriteSource::File(source),
        });
    }
    for (index, alias) in cold.iter().enumerate() {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let hash = alias.object_hash.as_deref().ok_or_else(|| {
            NativeJobError::new(
                "invalid-source",
                "legacy backup cold payload has no object hash",
            )
        })?;
        let object_path = cas
            .object_path(hash)
            .map_err(io_job_error)?
            .ok_or_else(|| {
                NativeJobError::new("invalid-source", "legacy backup cold object is missing")
            })?;
        let name = format!("coldstorage_{}.json", alias.key);
        if !names.insert(name.clone()) {
            return Err(NativeJobError::new(
                "invalid-source",
                "duplicate cold backup entry",
            ));
        }
        let path = owned_directory.join(format!("cold-{index}.json"));
        let source = File::open(object_path).map_err(io_job_error)?;
        let mut decoder = GzDecoder::new(CancellationReader::new(source, cancellation));
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(io_job_error)?;
        io::copy(&mut decoder, &mut output)
            .map_err(|error| local_backup_error(cancellation_io(error, cancellation)))?;
        output.sync_all().map_err(io_job_error)?;
        entries.push(LegacyBackupWriteEntry {
            logical_name: name,
            source: LegacyBackupWriteSource::File(path),
        });
    }
    Ok(entries)
}

struct LegacyOwnerExportProjection {
    replacement_keys: HashMap<AssetOwnerLocator, Vec<String>>,
    payloads: Vec<(String, String)>,
}

fn prepare_owner_export_projection(
    cas: &PayloadCas,
    assets: &[AssetAlias],
    owner_heads: &[AssetOwnerHead],
    owner_manifest_hashes: &HashSet<&str>,
    pins: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<LegacyOwnerExportProjection, NativeJobError> {
    let mut manifests = Vec::new();
    for head in owner_heads.iter().filter(|head| head.present) {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let hash = head.manifest_hash.as_deref().ok_or_else(|| {
            NativeJobError::new(
                "invalid-source",
                "legacy backup owner head is missing its manifest hash",
            )
        })?;
        let size = cas
            .stat_object(hash)
            .map_err(io_job_error)?
            .ok_or_else(|| {
                NativeJobError::new("invalid-source", "legacy backup owner manifest is missing")
            })?;
        if size > MAX_OWNER_MANIFEST_BYTES {
            return Err(NativeJobError::new(
                "invalid-source",
                "legacy backup owner manifest exceeds the decode limit",
            ));
        }
        pins.pin_existing(cas, hash, size, CasObjectRole::OwnerManifest)
            .map_err(io_job_error)?;
        manifests.push((head, hash.to_owned(), size));
    }

    let mut used_keys = assets
        .iter()
        .map(|alias| alias.key.clone())
        .collect::<HashSet<_>>();
    let mut replacement_keys = HashMap::with_capacity(manifests.len());
    let mut payloads = Vec::new();
    let mut payload_keys = HashMap::<(String, String), String>::new();
    for (head, manifest_hash, manifest_size) in manifests {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let entries = read_owner_manifest(cas, &manifest_hash, manifest_size, cancellation)?;
        if entries.len() as i64 != head.entry_count {
            return Err(NativeJobError::new(
                "invalid-source",
                "legacy backup owner manifest entry count mismatch",
            ));
        }
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            check_cancelled(cancellation).map_err(local_backup_error)?;
            let extension = safe_owner_extension(&entry.tuple[2]);
            let key = if let Some(payload_hash) = entry.payload_hash {
                let payload_hash = hex::encode(payload_hash);
                let identity = (payload_hash.clone(), extension.clone());
                if let Some(key) = payload_keys.get(&identity) {
                    key.clone()
                } else {
                    let size = cas
                        .stat_object(&payload_hash)
                        .map_err(io_job_error)?
                        .ok_or_else(|| {
                            NativeJobError::new(
                                "invalid-source",
                                "legacy backup owner payload is missing",
                            )
                        })?;
                    let role = if owner_manifest_hashes.contains(payload_hash.as_str()) {
                        CasObjectRole::OwnerManifest
                    } else {
                        CasObjectRole::DirectObject
                    };
                    pins.pin_existing(cas, &payload_hash, size, role)
                        .map_err(io_job_error)?;
                    let key = fresh_owner_asset_key(&extension, &mut used_keys);
                    payloads.push((key.clone(), payload_hash));
                    payload_keys.insert(identity, key.clone());
                    key
                }
            } else {
                fresh_owner_asset_key(&extension, &mut used_keys)
            };
            keys.push(key);
        }
        replacement_keys.insert(head.owner.clone(), keys);
    }
    Ok(LegacyOwnerExportProjection {
        replacement_keys,
        payloads,
    })
}

fn read_owner_manifest(
    cas: &PayloadCas,
    hash: &str,
    expected_size: u64,
    cancellation: &dyn CancellationProbe,
) -> Result<Vec<owner_manifest_codec::OwnerManifestEntry>, NativeJobError> {
    check_cancelled(cancellation).map_err(local_backup_error)?;
    let file = cas
        .open_object(hash)
        .map_err(io_job_error)?
        .ok_or_else(|| {
            NativeJobError::new("invalid-source", "legacy backup owner manifest is missing")
        })?;
    let mut bytes = Vec::with_capacity(expected_size as usize);
    CancellationReader::new(file, cancellation)
        .take(MAX_OWNER_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| local_backup_error(cancellation_io(error, cancellation)))?;
    if bytes.len() as u64 != expected_size || bytes.len() as u64 > MAX_OWNER_MANIFEST_BYTES {
        return Err(NativeJobError::new(
            "invalid-source",
            "legacy backup owner manifest changed while being read",
        ));
    }
    if owner_manifest_codec::owner_manifest_identity(&bytes) != hash {
        return Err(NativeJobError::new(
            "invalid-source",
            "legacy backup owner manifest content hash mismatch",
        ));
    }
    owner_manifest_codec::decode_owner_manifest(&bytes).map_err(|error| {
        NativeJobError::new(
            "invalid-source",
            format!("legacy backup owner manifest is invalid: {error}"),
        )
    })
}

fn fresh_owner_asset_key(extension: &str, used_keys: &mut HashSet<String>) -> String {
    let extension = safe_owner_extension(extension);
    loop {
        let key = format!("assets/owner-{}.{}", Uuid::new_v4(), extension);
        if used_keys.insert(key.clone()) {
            return key;
        }
    }
}

fn safe_owner_extension(extension: &str) -> String {
    if !extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        extension.to_ascii_lowercase()
    } else {
        "bin".to_owned()
    }
}

fn materialize_owner_export_entries(
    cas: &PayloadCas,
    payloads: &[(String, String)],
    cancellation: &dyn CancellationProbe,
) -> Result<Vec<LegacyBackupWriteEntry>, NativeJobError> {
    payloads
        .iter()
        .map(|(key, hash)| {
            check_cancelled(cancellation).map_err(local_backup_error)?;
            let source = cas
                .object_path(hash)
                .map_err(io_job_error)?
                .ok_or_else(|| {
                    NativeJobError::new("invalid-source", "legacy backup owner payload is missing")
                })?;
            Ok(LegacyBackupWriteEntry {
                logical_name: legacy_asset_entry_name(key)?,
                source: LegacyBackupWriteSource::File(source),
            })
        })
        .collect()
}

fn legacy_asset_entry_name(key: &str) -> Result<String, NativeJobError> {
    key.strip_prefix("assets/")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            NativeJobError::new(
                "invalid-source",
                "legacy backup asset key is outside the assets namespace",
            )
        })
}

fn write_inlay_entry(
    path: &std::path::Path,
    alias: &AssetAlias,
    object_path: &std::path::Path,
    cancellation: &dyn CancellationProbe,
) -> Result<(), NativeJobError> {
    let mut metadata = alias.metadata.as_object().cloned().unwrap_or_default();
    metadata.insert("key".to_owned(), Value::String(alias.key.clone()));
    metadata.insert("kind".to_owned(), Value::String("inlay".to_owned()));
    metadata.insert("size".to_owned(), Value::from(alias.size));
    metadata.insert("mime".to_owned(), Value::String(alias.mime.clone()));
    metadata.insert("name".to_owned(), Value::String(alias.name.clone()));
    metadata.insert("ext".to_owned(), Value::String(alias.ext.clone()));
    if let Some(value) = &alias.inlay_type {
        metadata.insert("inlayType".to_owned(), Value::String(value.clone()));
    }
    if let Some(value) = alias.width {
        metadata.insert("width".to_owned(), Value::from(value));
    }
    if let Some(value) = alias.height {
        metadata.insert("height".to_owned(), Value::from(value));
    }
    let header = serde_json::to_vec(&metadata)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let header_length = u32::try_from(header.len()).map_err(|_| {
        NativeJobError::new("store-error", "legacy backup Inlay metadata is too large")
    })?;
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io_job_error)?;
    output
        .write_all(&header_length.to_le_bytes())
        .map_err(io_job_error)?;
    output.write_all(&header).map_err(io_job_error)?;
    let source = File::open(object_path).map_err(io_job_error)?;
    io::copy(
        &mut CancellationReader::new(source, cancellation),
        &mut output,
    )
    .map_err(|error| local_backup_error(cancellation_io(error, cancellation)))?;
    output.sync_all().map_err(io_job_error)
}

struct LegacyDatabaseRestore<'a> {
    app: AppHandle,
    cas: &'a PayloadCas,
    repository_root: &'a std::path::Path,
    expected_revision: i64,
    job: &'a JobControl,
    result: Option<Result<JobResultSummary, NativeJobError>>,
}

impl StrictLocalBackupDatabaseRestore for LegacyDatabaseRestore<'_> {
    fn restore_database(
        &mut self,
        database: &StagedLocalBackupEntry,
        entries: &[StagedLocalBackupEntry],
    ) -> Result<(), LocalBackupError> {
        preflight_legacy_restore_entries(entries)?;
        let path = database.staged_path.as_ref().ok_or_else(|| {
            LocalBackupError::database_restore("legacy backup database is not staged")
        })?;
        let file = File::open(path).map_err(LocalBackupError::io)?;
        let mut durable = DurableCasJob::begin(
            self.repository_root,
            &self.job.id(),
            CasJobKind::LocalBackupRestore,
            now_millis(),
        )
        .map_err(LocalBackupError::io)?;
        let payloads = match prepare_legacy_restore_payloads(
            entries,
            self.cas,
            &mut durable,
            &JobCancellation(self.job),
        ) {
            Ok(payloads) => payloads,
            Err(error) => {
                let _ = durable.release(CasReleaseOutcome::Aborted);
                return Err(error);
            }
        };
        if let Err(error) = check_cancelled(&JobCancellation(self.job)) {
            let _ = durable.release(CasReleaseOutcome::Aborted);
            return Err(error);
        }
        let sink = LegacyReplacementSink {
            app: self.app.clone(),
            payloads,
            durable: Mutex::new(durable),
            migration_id: format!("legacy-backup-{}", self.job.id()),
        };
        let result = restore::restore_started_risu_save_with_pre_activation(
            OpenedJobSource {
                file,
                total_bytes: database.byte_length,
            },
            self.expected_revision,
            self.job,
            &sink,
            || Ok(None),
        );
        match result {
            Ok(summary) => {
                self.result = Some(Ok(summary));
                Ok(())
            }
            Err(error) => {
                self.result = Some(Err(error.clone()));
                Err(LocalBackupError::database_restore(error.message))
            }
        }
    }
}

struct LegacyReplacementSink {
    app: AppHandle,
    payloads: PreparedLegacyRestorePayloads,
    durable: Mutex<DurableCasJob>,
    migration_id: String,
}

impl restore::ReplacementSink for LegacyReplacementSink {
    fn begin(&self) -> StoreResult<StagingResult> {
        crate::persistent_store::commands::with_store_mut(
            self.app.state(),
            PersistentStore::replace_begin,
        )
    }

    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_root(staging_id, root)
        })
    }

    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_presets(staging_id, presets)
        })
    }

    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_characters(staging_id, characters)
        })
    }

    fn commit(&self, staging_id: &str, expected_revision: i64) -> StoreResult<RevisionResult> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_asset_aliases(staging_id, &self.payloads.asset_aliases)?;
            store.replace_put_cold_aliases(staging_id, &self.payloads.cold_aliases)?;
            let compatibility_hash = payload_compatibility_hash(&self.payloads);
            store.replace_put_asset_repository_authority(
                staging_id,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: self.migration_id.clone(),
                    compatibility_hash: compatibility_hash.clone(),
                },
            )?;
            store.replace_put_cold_payload_authority(
                staging_id,
                &ColdPayloadAuthorityState::V2 {
                    migration_id: self.migration_id.clone(),
                    compatibility_hash,
                },
            )?;
            self.durable
                .lock()
                .map_err(|error| crate::persistent_store::StoreError::Store {
                    message: format!("legacy backup CAS job mutex is poisoned: {error}"),
                })?
                .seal(store, now_millis())
                .map_err(crate::persistent_store::StoreError::from)
        })?;
        let committed = crate::persistent_store::commands::replace_commit_with_snapshot(
            &self.app,
            staging_id,
            Some(expected_revision),
        );
        if committed.is_ok() {
            let _ = self.release_durable(CasReleaseOutcome::Committed);
        }
        committed
    }

    fn abort(&self, staging_id: &str) -> StoreResult<()> {
        let abort = crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_abort(staging_id)
        });
        let release = self.release_durable(CasReleaseOutcome::Aborted);
        abort.and(release)
    }
}

impl LegacyReplacementSink {
    fn release_durable(&self, outcome: CasReleaseOutcome) -> StoreResult<()> {
        self.durable
            .lock()
            .map_err(|error| crate::persistent_store::StoreError::Store {
                message: format!("legacy backup CAS job mutex is poisoned: {error}"),
            })?
            .release(outcome)
            .map_err(crate::persistent_store::StoreError::from)
    }
}

impl Drop for LegacyReplacementSink {
    fn drop(&mut self) {
        if let Ok(durable) = self.durable.get_mut() {
            let _ = durable.release(CasReleaseOutcome::Aborted);
        }
    }
}

fn payload_compatibility_hash(payloads: &PreparedLegacyRestorePayloads) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&(&payloads.asset_aliases, &payloads.cold_aliases))
        .expect("legacy payload aliases are serializable");
    hex::encode(Sha256::digest(bytes))
}

fn local_backup_error(error: LocalBackupError) -> NativeJobError {
    let code = match error.code {
        LocalBackupErrorCode::UnsupportedEncryption | LocalBackupErrorCode::UnsupportedFormat => {
            "unsupported-format"
        }
        LocalBackupErrorCode::Cancelled => "cancelled",
        LocalBackupErrorCode::Io => "store-error",
        _ => "invalid-source",
    };
    NativeJobError::new(code, error.message)
}

fn io_job_error(error: io::Error) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

fn store_job_error(error: crate::persistent_store::StoreError) -> NativeJobError {
    let code = match &error {
        crate::persistent_store::StoreError::RevisionConflict { .. } => "revision-conflict",
        crate::persistent_store::StoreError::Validation { .. } => "invalid-source",
        _ => "store-error",
    };
    NativeJobError::new(code, error.to_string())
}

fn job_state_error(error: String) -> NativeJobError {
    NativeJobError::new("store-error", error)
}

fn destination_job_error(
    error: destination::DestinationWriteError,
    phase_error: Option<String>,
) -> NativeJobError {
    if let Some(error) = phase_error {
        return job_state_error(error);
    }
    match error {
        destination::DestinationWriteError::InvalidSource => {
            NativeJobError::new("store-error", "legacy backup staged archive is invalid")
        }
        destination::DestinationWriteError::InvalidDestination => NativeJobError::new(
            "invalid-destination",
            "legacy backup destination is invalid",
        ),
        destination::DestinationWriteError::Cancelled => {
            NativeJobError::new("cancelled", "legacy backup export was cancelled")
        }
        destination::DestinationWriteError::Io { source, .. } => io_job_error(source),
    }
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) struct PreparedLegacyRestorePayloads {
    pub(crate) asset_aliases: Vec<AssetAlias>,
    pub(crate) cold_aliases: Vec<ColdAlias>,
}

pub(crate) fn prepare_legacy_restore_payloads(
    entries: &[StagedLocalBackupEntry],
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    preflight_legacy_restore_entries(entries)?;
    let pocket_metadata = pocket_risu::index_metadata(entries, cancellation)?;
    let mut asset_aliases = Vec::new();
    let mut cold_aliases = Vec::new();
    let mut asset_keys = HashSet::new();
    let mut cold_keys = HashSet::new();

    for entry in entries {
        check_cancelled(cancellation)?;
        if matches!(
            entry.logical_name.as_str(),
            DATABASE_ENTRY | ENCRYPTION_ENTRY
        ) {
            continue;
        }
        if let Some(pocket_entry) = pocket_risu::classify(&entry.logical_name)? {
            if let pocket_risu::Entry::Payload { id, ext } = pocket_entry {
                if !asset_keys.insert(("inlay".to_owned(), id.to_owned())) {
                    return Err(invalid("legacy backup contains a duplicate Inlay key"));
                }
                asset_aliases.push(pocket_risu::prepare(
                    entry,
                    id,
                    ext,
                    &pocket_metadata,
                    cas,
                    durable,
                    cancellation,
                )?);
            }
            continue;
        }
        if let Some(key) = cold_key(&entry.logical_name) {
            if !cold_keys.insert(key.clone()) {
                return Err(invalid(
                    "legacy backup contains a duplicate cold payload key",
                ));
            }
            cold_aliases.push(prepare_cold(entry, key, cas, durable, cancellation)?);
        } else if let Some(encoded_key) = inlay_key_hex(&entry.logical_name) {
            let alias = prepare_inlay(entry, encoded_key, cas, durable, cancellation)?;
            if !asset_keys.insert((alias.kind.clone(), alias.key.clone())) {
                return Err(invalid("legacy backup contains a duplicate Inlay key"));
            }
            asset_aliases.push(alias);
        } else {
            let alias = prepare_asset(entry, cas, durable, cancellation)?;
            if !asset_keys.insert((alias.kind.clone(), alias.key.clone())) {
                return Err(invalid("legacy backup contains a duplicate asset key"));
            }
            asset_aliases.push(alias);
        }
    }
    check_cancelled(cancellation)?;
    Ok(PreparedLegacyRestorePayloads {
        asset_aliases,
        cold_aliases,
    })
}

fn preflight_legacy_restore_entries(
    entries: &[StagedLocalBackupEntry],
) -> Result<(), LocalBackupError> {
    reject_account_encryption(entries)?;
    for entry in entries {
        pocket_risu::classify(&entry.logical_name)?;
    }
    Ok(())
}

fn reject_account_encryption(entries: &[StagedLocalBackupEntry]) -> Result<(), LocalBackupError> {
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.logical_name == ENCRYPTION_ENTRY)
    else {
        return Ok(());
    };
    if entry.byte_length > MAX_METADATA_BYTES as u64 {
        return Err(invalid("legacy backup encryption metadata is too large"));
    }
    let file = open_staged(entry)?;
    let metadata: Value = serde_json::from_reader(BufReader::new(file))
        .map_err(|_| invalid("legacy backup encryption metadata is invalid"))?;
    if metadata.get("type").and_then(Value::as_str) == Some("account") {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::UnsupportedEncryption,
            "account-encrypted legacy backup requires the compatibility importer",
        ));
    }
    Ok(())
}

fn prepare_asset(
    entry: &StagedLocalBackupEntry,
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<AssetAlias, LocalBackupError> {
    let file = open_staged(entry)?;
    let mut source = CancellationReader::new(file, cancellation);
    let payload = durable
        .prepare_reader(cas, &mut source, CasObjectRole::DirectObject)
        .map_err(|error| cancellation_io(error, cancellation))?;
    let name = entry
        .logical_name
        .rsplit('/')
        .next()
        .unwrap_or(&entry.logical_name)
        .to_owned();
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .to_owned();
    Ok(AssetAlias {
        key: format!("assets/{}", entry.logical_name),
        object_hash: Some(payload.content_hash),
        kind: "asset".to_owned(),
        size: to_alias_size(payload.byte_size)?,
        mime: String::new(),
        name,
        ext,
        inlay_type: None,
        width: None,
        height: None,
        metadata: Value::Object(Map::new()),
    })
}

fn prepare_inlay(
    entry: &StagedLocalBackupEntry,
    encoded_key: &str,
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<AssetAlias, LocalBackupError> {
    let expected_key = String::from_utf8(
        hex::decode(encoded_key).map_err(|_| invalid("invalid Inlay entry name"))?,
    )
    .map_err(|_| invalid("invalid Inlay entry key"))?;
    let mut file = open_staged(entry)?;
    let mut length = [0_u8; 4];
    file.read_exact(&mut length).map_err(LocalBackupError::io)?;
    let header_length = u32::from_le_bytes(length);
    if header_length == 0
        || header_length > MAX_METADATA_BYTES
        || u64::from(header_length) + 4 > entry.byte_length
    {
        return Err(invalid("legacy backup Inlay metadata length is invalid"));
    }
    let mut header = vec![0_u8; header_length as usize];
    file.read_exact(&mut header).map_err(LocalBackupError::io)?;
    let metadata: Value = serde_json::from_slice(&header)
        .map_err(|_| invalid("legacy backup Inlay metadata is invalid"))?;
    let object = metadata
        .as_object()
        .ok_or_else(|| invalid("legacy backup Inlay metadata must be an object"))?;
    let key = string_field(object, "key")?;
    if key != expected_key || key.is_empty() || key.starts_with("assets/") {
        return Err(invalid(
            "legacy backup Inlay key does not match its entry name",
        ));
    }
    if string_field(object, "kind")? != "inlay" {
        return Err(invalid("legacy backup Inlay kind is invalid"));
    }
    let inlay_type = string_field(object, "inlayType")?;
    if !matches!(
        inlay_type.as_str(),
        "image" | "video" | "audio" | "signature"
    ) {
        return Err(invalid("legacy backup Inlay type is invalid"));
    }
    let mut source = CancellationReader::new(file, cancellation);
    let payload = durable
        .prepare_reader(cas, &mut source, CasObjectRole::DirectObject)
        .map_err(|error| cancellation_io(error, cancellation))?;
    let dimension = |field: &str| -> Result<Option<i64>, LocalBackupError> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .filter(|value| *value >= 0)
                .map(Some)
                .ok_or_else(|| invalid(format!("legacy backup Inlay {field} is invalid"))),
        }
    };
    Ok(AssetAlias {
        key,
        object_hash: Some(payload.content_hash),
        kind: "inlay".to_owned(),
        size: to_alias_size(payload.byte_size)?,
        mime: string_field(object, "mime")?,
        name: string_field(object, "name")?,
        ext: string_field(object, "ext")?,
        inlay_type: Some(inlay_type),
        width: dimension("width")?,
        height: dimension("height")?,
        metadata: Value::Object(Map::new()),
    })
}

fn prepare_cold(
    entry: &StagedLocalBackupEntry,
    key: String,
    cas: &PayloadCas,
    durable: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<ColdAlias, LocalBackupError> {
    let file = open_staged(entry)?;
    let source = CancellationReader::new(file, cancellation);
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(source));
    if !serde::de::Deserializer::deserialize_any(&mut deserializer, ColdRootVisitor).map_err(
        |_| {
            if cancellation.is_cancelled() {
                LocalBackupError::new(
                    LocalBackupErrorCode::Cancelled,
                    "legacy backup job was cancelled",
                )
            } else {
                invalid("legacy backup cold payload is invalid")
            }
        },
    )? {
        return Err(invalid(
            "legacy backup cold payload has an unsupported root",
        ));
    }
    deserializer
        .end()
        .map_err(|_| invalid("legacy backup cold payload has trailing data"))?;

    check_cancelled(cancellation)?;
    let source = open_staged(entry)?;
    let mut source = CancellationReader::new(source, cancellation);
    let compressed_path = entry
        .staged_path
        .as_deref()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| invalid("legacy backup cold payload is not staged"))?
        .join(format!("cold-compressed-{}.gz", Uuid::new_v4()));
    let mut compressed = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&compressed_path)
        .map_err(LocalBackupError::io)?;
    {
        let mut encoder = GzEncoder::new(&mut compressed, Compression::default());
        io::copy(&mut source, &mut encoder)
            .map_err(|error| cancellation_io(error, cancellation))?;
        encoder.finish().map_err(LocalBackupError::io)?;
    }
    compressed.flush().map_err(LocalBackupError::io)?;
    compressed
        .seek(SeekFrom::Start(0))
        .map_err(LocalBackupError::io)?;
    let mut source = CancellationReader::new(compressed, cancellation);
    let payload = durable
        .prepare_reader(cas, &mut source, CasObjectRole::DirectObject)
        .map_err(|error| cancellation_io(error, cancellation))?;
    let _ = std::fs::remove_file(compressed_path);
    Ok(ColdAlias {
        key,
        object_hash: Some(payload.content_hash),
        size: to_alias_size(payload.byte_size)?,
        metadata: Value::Object(Map::new()),
    })
}

struct ColdRootVisitor;

impl<'de> Visitor<'de> for ColdRootVisitor {
    type Value = bool;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a cold payload array or object")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<bool, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(true)
    }

    fn visit_map<A>(self, mut map: A) -> Result<bool, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut compatible = false;
        while let Some(key) = map.next_key::<String>()? {
            compatible |= matches!(key.as_str(), "character" | "message");
            map.next_value::<IgnoredAny>()?;
        }
        Ok(compatible)
    }
}

struct CancellationReader<'a, R> {
    inner: R,
    cancellation: &'a dyn CancellationProbe,
}

impl<'a, R> CancellationReader<'a, R> {
    fn new(inner: R, cancellation: &'a dyn CancellationProbe) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "legacy backup operation was cancelled",
            ));
        }
        self.inner.read(buffer)
    }
}

fn open_staged(entry: &StagedLocalBackupEntry) -> Result<File, LocalBackupError> {
    let path = entry
        .staged_path
        .as_ref()
        .ok_or_else(|| invalid("legacy backup payload is not staged"))?;
    File::open(path).map_err(LocalBackupError::io)
}

fn string_field(object: &Map<String, Value>, field: &str) -> Result<String, LocalBackupError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("legacy backup Inlay {field} is invalid")))
}

fn inlay_key_hex(name: &str) -> Option<&str> {
    name.strip_prefix("inlay_")?.strip_suffix(".risuinlay")
}

fn cold_key(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("coldstorage/") {
        let key = rest.strip_suffix(".json")?;
        return (!key.is_empty() && !key.contains(['/', '\\'])).then(|| key.to_owned());
    }
    let key = name.strip_prefix("coldstorage_")?.strip_suffix(".json")?;
    uuid::Uuid::parse_str(key)
        .ok()
        .map(|value| value.to_string())
}

fn to_alias_size(size: u64) -> Result<i64, LocalBackupError> {
    i64::try_from(size).map_err(|_| invalid("legacy backup payload is too large"))
}

fn check_cancelled(cancellation: &dyn CancellationProbe) -> Result<(), LocalBackupError> {
    if cancellation.is_cancelled() {
        return Err(LocalBackupError::new(
            LocalBackupErrorCode::Cancelled,
            "legacy backup job was cancelled",
        ));
    }
    Ok(())
}

fn cancellation_io(error: io::Error, cancellation: &dyn CancellationProbe) -> LocalBackupError {
    if cancellation.is_cancelled() {
        LocalBackupError::new(
            LocalBackupErrorCode::Cancelled,
            "legacy backup job was cancelled",
        )
    } else {
        LocalBackupError::io(error)
    }
}

fn invalid(message: impl Into<String>) -> LocalBackupError {
    LocalBackupError::new(LocalBackupErrorCode::DatabaseRestore, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::{owner_manifest_codec, PayloadCas};
    use crate::import_export_jobs::{parse_json_card, JobStaging};
    use crate::local_backup::{
        parse_legacy_local_backup_v1, NeverCancelled, PayloadTarget, StagedLocalBackupEntry,
        StrictLocalBackupDatabaseRestore,
    };
    use crate::native_file_jobs::content::content_classification_limits;
    use crate::native_file_jobs::{
        character_json_export::export_character_json, JobKind, JobRegistry,
    };
    use crate::persistent_store::{
        AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState, ColdPayloadAuthorityState,
    };
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::Cursor;

    struct PayloadPlanner<'a> {
        cas: &'a PayloadCas,
        durable: DurableCasJob,
        prepared: Option<PreparedLegacyRestorePayloads>,
    }

    impl StrictLocalBackupDatabaseRestore for PayloadPlanner<'_> {
        fn restore_database(
            &mut self,
            _database: &StagedLocalBackupEntry,
            entries: &[StagedLocalBackupEntry],
        ) -> Result<(), crate::local_backup::LocalBackupError> {
            self.prepared = Some(prepare_legacy_restore_payloads(
                entries,
                self.cas,
                &mut self.durable,
                &NeverCancelled,
            )?);
            Ok(())
        }
    }

    fn entry(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[derive(Default)]
    struct ArchiveCapture {
        entries: Vec<(String, Vec<u8>)>,
    }

    impl StrictLocalBackupDatabaseRestore for ArchiveCapture {
        fn restore_database(
            &mut self,
            _database: &StagedLocalBackupEntry,
            entries: &[StagedLocalBackupEntry],
        ) -> Result<(), crate::local_backup::LocalBackupError> {
            self.entries = entries
                .iter()
                .map(|entry| {
                    let bytes = fs::read(
                        entry
                            .staged_path
                            .as_ref()
                            .expect("job-staged archive entry has a path"),
                    )
                    .map_err(crate::local_backup::LocalBackupError::io)?;
                    Ok((entry.logical_name.clone(), bytes))
                })
                .collect::<Result<_, crate::local_backup::LocalBackupError>>()?;
            Ok(())
        }
    }

    fn decode_risu_save_blocks(bytes: &[u8]) -> Vec<(u8, String, Value)> {
        assert!(bytes.starts_with(b"RISUSAVE\0"));
        let mut cursor = &bytes[b"RISUSAVE\0".len()..];
        let mut blocks = Vec::new();
        while !cursor.is_empty() {
            let block_type = cursor[0];
            let compression = cursor[1];
            let name_length = cursor[2] as usize;
            cursor = &cursor[3..];
            let name = std::str::from_utf8(&cursor[..name_length])
                .unwrap()
                .to_owned();
            cursor = &cursor[name_length..];
            let payload_length = u32::from_le_bytes(cursor[..4].try_into().unwrap()) as usize;
            cursor = &cursor[4..];
            let encoded = &cursor[..payload_length];
            cursor = &cursor[payload_length..];
            let decoded = match compression {
                0 => encoded.to_vec(),
                1 => {
                    let mut decoded = Vec::new();
                    GzDecoder::new(encoded).read_to_end(&mut decoded).unwrap();
                    decoded
                }
                value => panic!("unexpected RisuSave compression {value}"),
            };
            let value = if decoded.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&decoded).unwrap()
            };
            blocks.push((block_type, name, value));
        }
        blocks
    }

    #[test]
    fn legacy_backup_export_preserves_owner_only_asset_occurrence_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let first = cas.prepare_bytes(b"first-occurrence").unwrap();
        let second = cas.prepare_bytes(b"second-owner-only-occurrence").unwrap();
        let alias = AssetAlias {
            key: "assets/shared.bin".to_owned(),
            object_hash: Some(first.content_hash.clone()),
            kind: "asset".to_owned(),
            size: first.byte_size as i64,
            mime: "application/octet-stream".to_owned(),
            name: "shared.bin".to_owned(),
            ext: "bin".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: serde_json::json!({}),
        };
        let manifest = cas
            .prepare_bytes(
                &owner_manifest_codec::encode_owner_manifest(&[
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: ["first".to_owned(), alias.key.clone(), "BIN".to_owned()],
                        payload_hash: Some(
                            hex::decode(&first.content_hash)
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        ),
                    },
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: ["second".to_owned(), alias.key.clone(), "bIn".to_owned()],
                        payload_hash: Some(
                            hex::decode(&second.content_hash)
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        ),
                    },
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: ["missing".to_owned(), alias.key.clone(), "BIN".to_owned()],
                        payload_hash: None,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let root_manifest = cas
            .prepare_bytes(
                &owner_manifest_codec::encode_owner_manifest(&[
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: ["module".to_owned(), alias.key.clone(), "BIN".to_owned()],
                        payload_hash: Some(
                            hex::decode(&second.content_hash)
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        ),
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let persona_manifest = cas
            .prepare_bytes(
                &owner_manifest_codec::encode_owner_manifest(&[
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: ["persona".to_owned(), alias.key.clone(), "BIN".to_owned()],
                        payload_hash: Some(
                            hex::decode(&first.content_hash)
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        ),
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let character = serde_json::json!({
            "type": "character",
            "chaId": "legacy-owner-character",
            "name": "Legacy Owner Character",
            "additionalAssets": [
                ["first", alias.key.clone(), "BIN"],
                ["second", alias.key.clone(), "bIn"],
                ["missing", alias.key.clone(), "BIN"]
            ],
            "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &serde_json::json!({
                    "loadouts": [],
                    "plugins": [],
                    "modules": [{
                        "assets": [["module", alias.key.clone(), "BIN", {"tail": 1}]]
                    }],
                    "personas": [{
                        "embeddedModule": {
                            "assets": [["persona", alias.key.clone(), "BIN", "tuple-tail"]]
                        }
                    }]
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store
            .replace_put_asset_aliases(&staging, std::slice::from_ref(&alias))
            .unwrap();
        store
            .replace_put_asset_owner_heads(
                &staging,
                &[
                    AssetOwnerHead::present(
                        AssetOwnerLocator::CharacterAdditionalAssets {
                            character_id: "legacy-owner-character".to_owned(),
                        },
                        manifest.content_hash,
                        3,
                    ),
                    AssetOwnerHead::present(
                        AssetOwnerLocator::RootModuleAssets { index: 0 },
                        root_manifest.content_hash,
                        1,
                    ),
                    AssetOwnerHead::present(
                        AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 },
                        persona_manifest.content_hash,
                        1,
                    ),
                ],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "legacy-owner-assets".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        store
            .replace_put_cold_payload_authority(
                &staging,
                &ColdPayloadAuthorityState::V2 {
                    migration_id: "legacy-owner-cold".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let owned = directory.path().join("export-owned");
        let handoff = directory.path().join("handoff");
        let parsed = directory.path().join("parsed");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&handoff).unwrap();
        fs::create_dir(&parsed).unwrap();
        let destination = directory.path().join("owner-backup.bin");
        let job = JobRegistry::default()
            .create(JobKind::ExportLegacyLocalBackup)
            .unwrap();

        let result =
            export_legacy_local_backup(Some(&destination), revision, &owned, &handoff, store, &job)
                .unwrap();

        assert_eq!(result.revision, revision);
        let mut capture = ArchiveCapture::default();
        parse_legacy_local_backup_v1(
            &mut fs::File::open(&destination).unwrap(),
            &parsed,
            PayloadTarget::JobStaging,
            &mut capture,
            &NeverCancelled,
        )
        .unwrap();
        let archived_payloads = capture
            .entries
            .iter()
            .filter(|(name, _)| name != DATABASE_ENTRY)
            .map(|(_, bytes)| bytes.as_slice())
            .collect::<Vec<_>>();
        assert!(archived_payloads.contains(&b"first-occurrence".as_slice()));
        assert!(archived_payloads.contains(&b"second-owner-only-occurrence".as_slice()));
        assert_eq!(
            archived_payloads
                .iter()
                .filter(|bytes| **bytes == b"second-owner-only-occurrence")
                .count(),
            1
        );
        let database = capture
            .entries
            .iter()
            .find(|(name, _)| name == DATABASE_ENTRY)
            .unwrap();
        let blocks = decode_risu_save_blocks(&database.1);
        let character = &blocks
            .iter()
            .find(|(block_type, name, _)| *block_type == 2 && name == "legacy-owner-character")
            .unwrap()
            .2;
        let tuples = character["additionalAssets"].as_array().unwrap();
        assert_eq!(tuples[0][0], "first");
        assert_eq!(tuples[0][2], "BIN");
        assert_eq!(tuples[1][0], "second");
        assert_eq!(tuples[1][2], "bIn");
        assert_eq!(tuples[2][0], "missing");
        assert_eq!(tuples[2][2], "BIN");
        let archived_for_key = |key: &str| {
            capture.entries.iter().find(|(name, _)| {
                key.strip_prefix("assets/")
                    .is_some_and(|relative| relative == name)
            })
        };
        let first_key = tuples[0][1].as_str().unwrap();
        let second_key = tuples[1][1].as_str().unwrap();
        let missing_key = tuples[2][1].as_str().unwrap();
        assert_ne!(first_key, alias.key);
        assert_ne!(second_key, alias.key);
        assert_ne!(missing_key, alias.key);
        assert_eq!(archived_for_key(first_key).unwrap().1, b"first-occurrence");
        assert_eq!(
            archived_for_key(second_key).unwrap().1,
            b"second-owner-only-occurrence"
        );
        assert!(archived_for_key(missing_key).is_none());
        let modules = &blocks
            .iter()
            .find(|(block_type, name, _)| *block_type == 5 && name == "modules")
            .unwrap()
            .2;
        let module_tuple = &modules[0]["assets"][0];
        assert_eq!(module_tuple[0], "module");
        assert_eq!(module_tuple[2], "BIN");
        assert_eq!(module_tuple[3], serde_json::json!({"tail": 1}));
        assert_eq!(
            archived_for_key(module_tuple[1].as_str().unwrap())
                .unwrap()
                .1,
            b"second-owner-only-occurrence"
        );
        let root = &blocks
            .iter()
            .find(|(block_type, name, _)| *block_type == 1 && name == "root")
            .unwrap()
            .2;
        let persona_tuple = &root["personas"][0]["embeddedModule"]["assets"][0];
        assert_eq!(persona_tuple[0], "persona");
        assert_eq!(persona_tuple[2], "BIN");
        assert_eq!(persona_tuple[3], "tuple-tail");
        assert_eq!(persona_tuple[1], tuples[0][1]);
        assert_eq!(
            archived_for_key(persona_tuple[1].as_str().unwrap())
                .unwrap()
                .1,
            b"first-occurrence"
        );
    }

    #[test]
    fn legacy_backup_export_reads_ordinary_inventory_from_the_detached_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &serde_json::json!({"modules": [], "loadouts": [], "plugins": []}),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "ordinary-assets".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        store
            .replace_put_cold_payload_authority(
                &staging,
                &ColdPayloadAuthorityState::V2 {
                    migration_id: "ordinary-cold".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let owned = directory.path().join("ordinary-owned");
        let handoff = directory.path().join("ordinary-handoff");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&handoff).unwrap();
        let destination = directory.path().join("ordinary.bin");
        let job = JobRegistry::default()
            .create(JobKind::ExportLegacyLocalBackup)
            .unwrap();

        let result =
            export_legacy_local_backup(Some(&destination), revision, &owned, &handoff, store, &job)
                .unwrap();

        assert_eq!(result.revision, revision);
        assert!(fs::metadata(destination).unwrap().len() > 0);
    }

    #[test]
    fn prepares_asset_inlay_and_cold_payloads_without_mutating_live_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let inlay_metadata = serde_json::json!({
            "key": "inlay-id",
            "kind": "inlay",
            "inlayType": "image",
            "mime": "image/webp",
            "name": "image.webp",
            "ext": "webp",
            "size": 4,
            "width": 2,
            "height": 3
        });
        let header = serde_json::to_vec(&inlay_metadata).unwrap();
        let mut inlay = Vec::new();
        inlay.extend_from_slice(&(header.len() as u32).to_le_bytes());
        inlay.extend_from_slice(&header);
        inlay.extend_from_slice(b"webp");
        let cold_key = "123e4567-e89b-42d3-a456-426614174000";
        let bytes = [
            entry(b"portrait.png", b"original"),
            entry(b"inlay_696e6c61792d6964.risuinlay", &inlay),
            entry(
                format!("coldstorage_{cold_key}.json").as_bytes(),
                br#"{"message":[{"data":"cold"}]}"#,
            ),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };

        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();

        let prepared = planner.prepared.take().unwrap();
        assert_eq!(prepared.asset_aliases.len(), 2);
        assert_eq!(prepared.asset_aliases[0].key, "assets/portrait.png");
        assert_eq!(prepared.asset_aliases[1].key, "inlay-id");
        assert_eq!(prepared.cold_aliases[0].key, cold_key);
        for alias in &prepared.asset_aliases {
            assert!(cas
                .object_path(alias.object_hash.as_deref().unwrap())
                .unwrap()
                .is_some());
        }
        assert!(cas
            .object_path(prepared.cold_aliases[0].object_hash.as_deref().unwrap())
            .unwrap()
            .is_some());
        assert!(!directory
            .path()
            .join("persistent/persistent.sqlite3")
            .exists());
        assert_eq!(planner.durable.pin_count(), 3);
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn legacy_backup_empty_mime_asset_roundtrips_through_native_json_with_exact_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"legacy-empty-mime-payload";
        let bytes = [
            entry(b"legacy.bin", payload),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };
        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();
        let prepared_payloads = planner.prepared.take().unwrap();
        let alias = prepared_payloads.asset_aliases[0].clone();
        assert_eq!(alias.key, "assets/legacy.bin");
        assert!(alias.mime.is_empty());

        let mut store = PersistentStore::open(directory.path()).unwrap();
        let character = serde_json::json!({
            "type": "character",
            "chaId": "legacy-json-character",
            "name": "Legacy JSON",
            "image": "",
            "ccAssets": [{
                "type": "x-risu-asset",
                "uri": alias.key,
                "name": "legacy",
                "ext": "bin"
            }],
            "additionalAssets": [],
            "emotionImages": [],
            "triggerscript": [],
            "customscript": [],
            "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&staging, &serde_json::json!({}))
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store.replace_put_asset_aliases(&staging, &[alias]).unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "legacy-json-test".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let metadata = serde_json::json!({
            "spec": "chara_card_v3",
            "spec_version": "3.0",
            "data": {
                "name": "Legacy JSON",
                "extensions": {"risuai": {
                    "triggerscript": [],
                    "customScripts": []
                }},
                "assets": [
                    {"type": "x-risu-asset", "uri": "assets/legacy.bin", "name": "legacy", "ext": "bin"},
                    {"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}
                ]
            }
        });
        let prepared = store.prepare_risu_save_export(revision).unwrap();
        let owned = directory.path().join("json-owned");
        let parsed_root = directory.path().join("json-parsed");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&parsed_root).unwrap();
        let destination = directory.path().join("legacy.json");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();

        export_character_json(
            prepared,
            "legacy-json-character",
            metadata,
            &owned,
            &directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();

        let exported = fs::read(&destination).unwrap();
        assert!(
            String::from_utf8_lossy(&exported).contains("data:application/octet-stream;base64,")
        );
        let staging = JobStaging::open(&parsed_root).unwrap();
        let parsed = parse_json_card(
            &mut exported.as_slice(),
            &staging,
            &content_classification_limits(),
            &|| false,
        )
        .unwrap();
        assert_eq!(parsed.payloads[0].payload.byte_size, payload.len() as u64);
        assert_eq!(
            parsed.payloads[0].payload.sha256,
            hex::encode(Sha256::digest(payload))
        );
        assert_eq!(
            fs::read(parsed_root.join(&parsed.payloads[0].payload.staged_name)).unwrap(),
            payload
        );
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn rejects_account_encryption_before_preparing_any_payload_object() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let bytes = [
            entry(b"portrait.png", b"original"),
            entry(b"encryption.risudat", br#"{"type":"account","time":123}"#),
            entry(b"database.risudat", b"encrypted"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };

        let error = parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(
            error.code,
            crate::local_backup::LocalBackupErrorCode::UnsupportedEncryption
        );
        assert_eq!(
            fs::read_dir(directory.path().join("assets-v2/objects"))
                .map(|entries| entries.count())
                .unwrap_or(0),
            0,
        );
        assert_eq!(planner.durable.pin_count(), 0);
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn imports_pocket_risu_110_inlays_with_sidecars_after_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let bytes = [
            entry(b"portrait.png", b"original"),
            entry(b"inlay/pocket.webp", b"webp"),
            entry(b"inlay/voice.mp3", b"audio"),
            entry(b"inlay_meta/pocket", br#"{"derived":true}"#),
            entry(b"inlay_thumb/pocket", b"thumbnail"),
            entry(
                b"inlay_sidecar/pocket",
                br#"{"ext":"webp","name":"original.webp","type":"image","width":2,"height":3}"#,
            ),
            entry(
                b"inlay_sidecar/voice",
                br#"{"ext":"mp3","name":"voice.mp3","type":"audio"}"#,
            ),
            entry(b"database.risudat", b"RISUSAVE\0"),
        ]
        .concat();
        let mut planner = PayloadPlanner {
            cas: &cas,
            durable: DurableCasJob::begin(
                directory.path(),
                &Uuid::new_v4().to_string(),
                CasJobKind::LocalBackupRestore,
                now_millis(),
            )
            .unwrap(),
            prepared: None,
        };

        parse_legacy_local_backup_v1(
            &mut Cursor::new(bytes),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut planner,
            &NeverCancelled,
        )
        .unwrap();

        let prepared = planner.prepared.take().unwrap();
        assert_eq!(prepared.asset_aliases.len(), 3);
        for (key, bytes, mime, inlay_type) in [
            ("pocket", b"webp".as_slice(), "image/webp", "image"),
            ("voice", b"audio".as_slice(), "audio/mpeg", "audio"),
        ] {
            let alias = prepared
                .asset_aliases
                .iter()
                .find(|alias| alias.key == key)
                .unwrap();
            assert_eq!(alias.kind, "inlay");
            assert_eq!(alias.mime, mime);
            assert_eq!(alias.inlay_type.as_deref(), Some(inlay_type));
            assert_eq!(
                fs::read(
                    cas.object_path(alias.object_hash.as_deref().unwrap())
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                bytes
            );
        }
        let image = &prepared.asset_aliases[1];
        assert_eq!(image.name, "original.webp");
        assert_eq!((image.width, image.height), (Some(2), Some(3)));
        assert!(!directory
            .path()
            .join("persistent/persistent.sqlite3")
            .exists());
        assert_eq!(planner.durable.pin_count(), 3);
        planner.durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn preserves_nested_asset_paths_in_legacy_archive_names() {
        assert_eq!(
            legacy_asset_entry_name("assets/foo/bar.png").unwrap(),
            "foo/bar.png"
        );
        assert_eq!(
            legacy_asset_entry_name("foo/bar.png").unwrap_err().code,
            "invalid-source"
        );
    }
}
