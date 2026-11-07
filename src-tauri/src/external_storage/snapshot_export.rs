//! Offline export of a fully verified remote snapshot. The downloaded files
//! are activated only in a new scratch PDS, then passed through the normal
//! portable-backup writer and verifier.
use super::{
    content_store::ObjectSource,
    capture::{self, CaptureCatalog, DurableCaptureReference},
    contract::{Cancellation, ErrorKind, Provider, ProviderError, RepositoryHandle, Result},
    packaging::RemoteObject,
    phase_progress::PhaseProgress,
    sections::CapturedSection,
    snapshot_restore::{PreparedObject, PreparedRecord, PreparedRemoteSnapshot},
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
        PayloadCas,
    },
    local_backup::CancellationProbe,
    persistent_store::{
        device_store::sections::PreparedSectionRows,
        external_apply::{
            ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord,
        },
        PersistentStore,
    },
};
use risunest_external_storage_format::format::library_fingerprint_domain;
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotExportReceipt {
    pub destination: PathBuf,
    pub sha256: String,
    pub revision: i64,
}

fn corrupt(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt).caused(&error)
}
fn transient(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient).caused(&error)
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).map_err(corrupt)?;
    bytes.try_into().map_err(|_| corrupt("invalid hash length"))
}

fn bounded_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains('\0')
}

struct Probe<'a>(&'a Cancellation);
impl CancellationProbe for Probe<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.check().is_err()
    }
}

/// Rebuilds the normal verified-snapshot input from a native-owned durable
/// capture. The descriptor contains no authority to name an arbitrary path:
/// capture validation resolves it below the persistent root and checks the
/// catalog hash, identity and logical-record objects before any export begins.
pub(crate) fn prepare_local_conflict_snapshot(
    repository_root: &Path,
    repository_id: &str,
    reference: &DurableCaptureReference,
) -> Result<PreparedRemoteSnapshot> {
    if !bounded_identity(repository_id) {
        return Err(corrupt("conflict repository identity is invalid"));
    }
    let roots = capture::validate_recovery_sources(
        [reference],
        repository_root,
        &crate::local_backup::NeverCancelled,
    )
    .map_err(corrupt)?;
    let mut catalogs = roots.catalogs.into_iter();
    let catalog_path = catalogs
        .next()
        .ok_or_else(|| corrupt("conflict capture catalog is missing"))?;
    if catalogs.next().is_some() {
        return Err(corrupt("conflict capture resolved more than one catalog"));
    }
    let expected_hash = decode_hash(&reference.catalog_hash)?;
    let external_root = repository_root
        .join("external-storage")
        .canonicalize()
        .map_err(corrupt)?;
    let catalog = CaptureCatalog::reopen(
        &catalog_path,
        &external_root,
        &expected_hash,
        &reference.identity,
    )
    .map_err(corrupt)?;
    let scope = library_fingerprint_domain();
    let fingerprint = hex::encode(catalog.content_fingerprint(&scope).map_err(corrupt)?);
    let logical_revision = u64::try_from(reference.identity.revision)
        .map_err(|_| corrupt("conflict capture revision is invalid"))?;

    let mut records = Vec::new();
    let mut query = catalog
        .db
        .prepare("SELECT key,hash,bytes FROM records ORDER BY key")
        .map_err(corrupt)?;
    let mut rows = query.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let key: String = row.get(0).map_err(corrupt)?;
        let content_hash: String = row.get(1).map_err(corrupt)?;
        let bytes: i64 = row.get(2).map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&content_hash) {
            return Err(corrupt("conflict capture record hash is invalid"));
        }
        records.push(PreparedRecord {
            key,
            source: ObjectSource::Captured(content_hash.clone()),
            content_hash,
            byte_length: u64::try_from(bytes)
                .map_err(|_| corrupt("conflict capture record length is invalid"))?,
        });
    }
    drop(rows);
    drop(query);

    let mut objects = Vec::new();
    let mut generated = catalog
        .db
        .prepare("SELECT hash,bytes FROM generated ORDER BY hash")
        .map_err(corrupt)?;
    for row in generated
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(corrupt)?
    {
        let (content_hash, bytes) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&content_hash) {
            return Err(corrupt("conflict capture object hash is invalid"));
        }
        objects.push(PreparedObject {
            source: ObjectSource::Captured(content_hash.clone()),
            content_hash,
            byte_length: u64::try_from(bytes)
                .map_err(|_| corrupt("conflict capture object length is invalid"))?,
        });
    }

    let cas = PayloadCas::new(repository_root).map_err(transient)?;
    let mut dependencies = catalog
        .db
        .prepare(
            "SELECT DISTINCT d.hash,d.bytes FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL ORDER BY d.hash",
        )
        .map_err(corrupt)?;
    for row in dependencies
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(corrupt)?
    {
        let (content_hash, bytes) = row.map_err(corrupt)?;
        if !crate::trust_boundary::is_lower_hex_256(&content_hash) {
            return Err(corrupt("conflict capture dependency hash is invalid"));
        }
        let path = cas
            .object_path(&content_hash)
            .map_err(transient)?
            .ok_or_else(|| corrupt("conflict capture dependency is missing"))?;
        objects.push(PreparedObject {
            source: ObjectSource::File(path),
            content_hash,
            byte_length: u64::try_from(bytes)
                .map_err(|_| corrupt("conflict capture dependency length is invalid"))?,
        });
    }

    Ok(PreparedRemoteSnapshot {
        snapshot_id: reference.capture_id.clone(),
        repository_id: repository_id.into(),
        fingerprint: fingerprint.clone(),
        library_fingerprint: fingerprint,
        logical_revision,
        staging_root: repository_root.canonicalize().map_err(corrupt)?,
        records,
        objects,
        captured_by_device: None,
    })
}

fn create_verified_snapshot_backup(
    store: &mut PersistentStore,
    revision: i64,
    sections: &[PreparedSectionRows],
    destination: &Path,
    scratch: &Path,
    probe: &dyn CancellationProbe,
    source_build: &str,
) -> std::result::Result<String, crate::portable_backup::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut pins = DurableCasJob::begin(
        store.repository_root(),
        &id,
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::snapshot_export(&id),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX),
    )?;
    let outcome = (|| {
        let captured = crate::portable_backup::capture_library_with_sections(
            store, revision, scratch, &mut pins, false, probe, source_build, sections,
        )?;
        if captured.repair_required {
            return Err(crate::portable_backup::Error::Invalid(
                "snapshot export requires a valid library",
            ));
        }
        captured
            .catalog
            .write_candidate(destination, false, probe)?;
        let archive = crate::portable_backup::VerifiedArchive::open(
            File::open(destination)?,
            scratch,
            probe,
        )?;
        archive.validate_library(probe)?;
        if !archive.manifest.library_included
            || archive.manifest.device_included == sections.is_empty()
        {
            return Err(crate::portable_backup::Error::Invalid(
                "snapshot export scope differs",
            ));
        }
        drop(archive);
        let mut file = File::open(destination)?;
        let bytes = file.metadata()?.len();
        crate::portable_backup::copy_hash(&mut file, &mut std::io::sink(), bytes, probe)
    })();
    let released = pins.release(CasReleaseOutcome::Aborted);
    match (outcome, released) {
        (Ok(hash), Ok(())) => Ok(hash),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error.into()),
    }
}

/// The device sections a remote snapshot carries, downloaded for its archive. A snapshot that no
/// device captured carries none.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn download_snapshot_sections(
    remote: &RemoteObject,
    snapshot: &PreparedRemoteSnapshot,
    staging_root: &Path,
    root_key: &[u8; 32],
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    progress: &PhaseProgress,
    cancel: &Cancellation,
) -> Result<Vec<CapturedSection>> {
    if snapshot.captured_by_device.as_deref().is_none_or(str::is_empty) {
        return Ok(Vec::new());
    }
    let wanted = super::runtime_restore::device_section_ids();
    let sections = super::snapshot_restore::download_sections(
        remote,
        &wanted,
        staging_root,
        root_key,
        None,
        provider,
        repository,
        progress,
        cancel,
    )
    .await?;
    if sections.len() != wanted.len() {
        return Err(ProviderError::new(ErrorKind::NotFound));
    }
    Ok(sections)
}

/// Create a standalone `.risunest` file at the path chosen by the native
/// save dialog. After this returns, restoring the archive needs neither the
/// cloud provider nor its credentials or repository key.
pub(crate) fn export_verified_snapshot(
    snapshot: PreparedRemoteSnapshot,
    sections: &[CapturedSection],
    destination: &Path,
    scratch_parent: &Path,
    cancel: &Cancellation,
    source_build: &str,
) -> Result<SnapshotExportReceipt> {
    export_verified_snapshot_controlled(
        snapshot,
        sections,
        destination,
        scratch_parent,
        cancel,
        source_build,
        || Ok(()),
    )
}

pub(crate) fn export_verified_snapshot_controlled(
    snapshot: PreparedRemoteSnapshot,
    sections: &[CapturedSection],
    destination: &Path,
    scratch_parent: &Path,
    cancel: &Cancellation,
    source_build: &str,
    before_replace: impl FnOnce() -> std::result::Result<
        (),
        crate::persistent_store::export::destination::DestinationWriteError,
    >,
) -> Result<SnapshotExportReceipt> {
    cancel.check()?;
    if destination.extension().and_then(|value| value.to_str()) != Some("risunest")
        || destination.file_name().is_none()
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    // A backup that a device captured is saved with that device's sections, and only then.
    if snapshot.captured_by_device.as_deref().is_some_and(|device| !device.is_empty())
        == sections.is_empty()
    {
        return Err(corrupt("snapshot device sections differ from its capture"));
    }
    std::fs::create_dir_all(scratch_parent).map_err(transient)?;
    let sections = super::sections::prepare_received_backup_sections(sections, scratch_parent, cancel)?;
    let fingerprint = decode_hash(&snapshot.library_fingerprint)?;
    let scratch = tempfile::Builder::new()
        .prefix("external-snapshot-export-")
        .tempdir_in(scratch_parent)
        .map_err(transient)?;
    let mut store = PersistentStore::open(&scratch.path().join("pds")).map_err(transient)?;
    let probe = Probe(cancel);
    let application = ExternalSnapshotApplication {
        expected_revision: 0,
        staging_root: &snapshot.staging_root,
        scope_id: &library_fingerprint_domain(),
        fingerprint: &fingerprint,
        probe: &probe,
    };
    let records = snapshot.records.into_iter().map(|value| {
        Ok(ExternalSnapshotRecord {
            key: value.key,
            content_hash: value.content_hash,
            byte_length: value.byte_length,
            source: value.source,
        })
    });
    let objects = snapshot.objects.into_iter().map(|value| {
        Ok(ExternalSnapshotObject {
            content_hash: value.content_hash,
            byte_length: value.byte_length,
            source: value.source,
        })
    });
    let prepared = store
        .prepare_external_snapshot_application(&application, records, objects)
        .map_err(|error| {
            if cancel.check().is_err() {
                ProviderError::new(ErrorKind::Cancelled)
            } else {
                transient(error)
            }
        })?;
    let revision = store
        .finish_prepared_replace(prepared)
        .map_err(transient)?
        .revision;
    let archive_scratch = scratch.path().join("archive");
    std::fs::create_dir_all(&archive_scratch).map_err(transient)?;
    // The shared destination publisher accepts this exact verified portable
    // handoff name before its atomic destination replacement.
    let candidate = scratch.path().join("archive.risunest.part");
    let sha256 = create_verified_snapshot_backup(
        &mut store,
        revision,
        &sections,
        &candidate,
        &archive_scratch,
        &probe,
        source_build,
    )
    .map_err(transient)?;
    crate::persistent_store::export::destination::write_portable_destination_controlled(
        scratch.path(),
        &candidate,
        destination
            .parent()
            .ok_or_else(|| transient("destination has no parent"))?,
        destination,
        || cancel.check().is_err(),
        |_| {},
        before_replace,
    )
    .map_err(|error| match error {
        crate::persistent_store::export::destination::DestinationWriteError::Cancelled => {
            ProviderError::new(ErrorKind::Cancelled)
        }
        error => transient(format!("snapshot destination publication failed: {error:?}")),
    })?;
    Ok(SnapshotExportReceipt {
        destination: destination.to_path_buf(),
        sha256,
        revision,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical_records::{
        encode_logical_record, encode_logical_record_key, LogicalRecordEnvelope,
        LogicalRecordLocator,
    };
    use crate::persistent_store::sync_selection::CaptureIdentity;
    use risunest_external_storage_format::format::fingerprint;
    use rusqlite::{params, Connection};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs};

    fn durable_local_capture(
        root: &Path,
        marker: &str,
    ) -> (DurableCaptureReference, String) {
        let external = root.join("external-storage");
        let capture_directory = external.join("captures").join("capture-a");
        fs::create_dir_all(&capture_directory).unwrap();
        let key = encode_logical_record_key(&LogicalRecordLocator::Root).unwrap();
        let encoded = encode_logical_record(&LogicalRecordEnvelope::Root {
            value: serde_json::json!({"marker":marker}),
            owner_heads: Vec::new(),
        })
        .unwrap();
        crate::external_storage::content_store::write_file_fixture(&external, &encoded.hash, &encoded.bytes);
        let identity = CaptureIdentity {
            store_id: "store-a".into(),
            library_epoch: "library-a".into(),
            generation: "generation-a".into(),
            selection_epoch: "selection-a".into(),
            revision: 7,
        };
        let catalog_path = capture_directory.join("capture.sqlite");
        let db = Connection::open(&catalog_path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=DELETE;
             CREATE TABLE capture_info(singleton INTEGER PRIMARY KEY CHECK(singleton=1),identity TEXT NOT NULL);
             CREATE TABLE records(key TEXT PRIMARY KEY,hash TEXT NOT NULL,bytes INTEGER NOT NULL);
             CREATE TABLE generated(hash TEXT PRIMARY KEY,bytes INTEGER NOT NULL);
             CREATE TABLE dependencies(record TEXT NOT NULL,hash TEXT NOT NULL,bytes INTEGER NOT NULL,PRIMARY KEY(record,hash));
             CREATE TABLE delta(key TEXT PRIMARY KEY);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO capture_info VALUES(1,?1)",
            [serde_json::to_string(&identity).unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO records VALUES(?1,?2,?3)",
            params![key, encoded.hash, i64::try_from(encoded.size).unwrap()],
        )
        .unwrap();
        drop(db);
        let catalog_hash = hex::encode(Sha256::digest(fs::read(&catalog_path).unwrap()));
        (
            DurableCaptureReference {
                capture_id: "capture-a".into(),
                identity,
                catalog_path: "captures/capture-a/capture.sqlite".into(),
                catalog_hash,
            },
            key,
        )
    }

    #[test]
    fn malformed_identity_and_non_archive_destination_write_nothing() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let snapshot = PreparedRemoteSnapshot {
            snapshot_id: "synthetic".into(),
            repository_id: "repository".into(),
            fingerprint: "00".repeat(32),
            library_fingerprint: "not-a-hash".into(),
            logical_revision: 1,
            staging_root: staging,
            captured_by_device: None,
            records: Vec::new(),
            objects: Vec::new(),
        };
        let destination = root.path().join("snapshot.bin");
        assert!(export_verified_snapshot(
            snapshot,
            &[],
            &destination,
            &root.path().join("scratch"),
            &Cancellation::default(),
            "9.8.7-synthetic",
        )
        .is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn snapshot_export_refuses_damaged_source_preservation() {
        let root = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(root.path()).unwrap();
        let stage = store.replace_begin().unwrap();
        store.replace_put_root(&stage.staging_id, &serde_json::json!({"marker":"synthetic"})).unwrap();
        store.replace_commit(&stage.staging_id, Some(0)).unwrap();
        let revision = store.revision().unwrap();
        let db = Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE future_records(generation TEXT,value TEXT)").unwrap();
        db.execute("INSERT INTO future_records VALUES('synthetic',?1)",
            [r#"{"account":{"id":"synthetic-account","token":"synthetic-token"}}"#]).unwrap();
        drop(db);
        let destination = scratch.path().join("synthetic.risunest");
        let result = create_verified_snapshot_backup(&mut store, revision, &[], &destination,
            scratch.path(), &crate::local_backup::NeverCancelled, "9.8.7-synthetic");
        assert!(matches!(result, Err(crate::portable_backup::Error::SourceNeedsPreservation)));
        assert!(!destination.exists());
    }

    #[test]
    fn verified_remote_snapshot_becomes_normal_offline_archive() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let scope = library_fingerprint_domain();
        let key = encode_logical_record_key(&LogicalRecordLocator::Root).unwrap();
        let encoded = encode_logical_record(&LogicalRecordEnvelope::Root {
            value: serde_json::json!({"marker":"synthetic-remote"}),
            owner_heads: Vec::new(),
        })
        .unwrap();
        let record_path = staging.join("root.record");
        fs::write(&record_path, &encoded.bytes).unwrap();
        let mut hashes = BTreeMap::new();
        hashes.insert(
            key.clone(),
            hex::decode(&encoded.hash).unwrap().try_into().unwrap(),
        );
        let scope_id = library_fingerprint_domain();
        let snapshot = PreparedRemoteSnapshot {
            snapshot_id: "synthetic-snapshot".into(),
            repository_id: "synthetic-repository".into(),
            fingerprint: hex::encode(fingerprint(&scope_id, &hashes)),
            library_fingerprint: hex::encode(fingerprint(&scope_id, &hashes)),
            logical_revision: 1,
            staging_root: staging,
            captured_by_device: None,
            records: vec![super::super::snapshot_restore::PreparedRecord {
                key,
                content_hash: encoded.hash,
                byte_length: encoded.size,
                source: ObjectSource::File(record_path),
            }],
            objects: Vec::new(),
        };
        let destination = root.path().join("snapshot.risunest");
        let receipt = export_verified_snapshot(
            snapshot,
            &[],
            &destination,
            &root.path().join("scratch"),
            &Cancellation::default(),
            "9.8.7-synthetic",
        )
        .unwrap();
        assert_eq!(receipt.revision, 1);
        let archive = crate::portable_backup::VerifiedArchive::open(
            File::open(&destination).unwrap(),
            root.path(),
            &Probe(&Cancellation::default()),
        )
        .unwrap();
        archive
            .validate_library(&Probe(&Cancellation::default()))
            .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&destination).unwrap()).unwrap();
        let format: serde_json::Value = serde_json::from_reader(zip.by_name("format.json").unwrap()).unwrap();
        assert_eq!(format["sourceAppBuild"], "9.8.7-synthetic");
        assert!(archive.manifest.library_included);
        assert!(!archive.manifest.device_included);
        assert!(!archive.manifest.repair_required);
        let restored_root = tempfile::tempdir().unwrap();
        let mut restored = PersistentStore::open(restored_root.path()).unwrap();
        let cancellation = Cancellation::default();
        let probe = Probe(&cancellation);
        let stage = restored
            .stage_portable_records(&archive.db, &probe)
            .unwrap();
        let prepared = restored
            .prepare_replace_commit(&stage.staging_id, Some(0))
            .unwrap();
        assert_eq!(
            restored.finish_prepared_replace(prepared).unwrap().revision,
            1
        );
        assert_eq!(
            restored.read_root(None).unwrap().value["marker"],
            "synthetic-remote"
        );
    }

    #[test]
    fn durable_local_conflict_capture_becomes_a_normal_offline_archive() {
        let root = tempfile::tempdir().unwrap();
        let (reference, key) = durable_local_capture(root.path(), "synthetic-local");
        let snapshot = prepare_local_conflict_snapshot(
            root.path(),
            "synthetic-repository",
            &reference,
        )
        .unwrap();
        assert_eq!(snapshot.snapshot_id, reference.capture_id);
        assert_eq!(snapshot.repository_id, "synthetic-repository");
        assert_eq!(snapshot.logical_revision, 7);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].key, key);

        let destination = root.path().join("local-conflict.risunest");
        export_verified_snapshot(
            snapshot,
            &[],
            &destination,
            &root.path().join("scratch"),
            &Cancellation::default(),
            "9.8.7-synthetic",
        )
        .unwrap();
        let archive = crate::portable_backup::VerifiedArchive::open(
            File::open(&destination).unwrap(),
            root.path(),
            &Probe(&Cancellation::default()),
        )
        .unwrap();
        archive
            .validate_library(&Probe(&Cancellation::default()))
            .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&destination).unwrap()).unwrap();
        let format: serde_json::Value = serde_json::from_reader(zip.by_name("format.json").unwrap()).unwrap();
        assert_eq!(format["sourceAppBuild"], "9.8.7-synthetic");
        let restored_root = tempfile::tempdir().unwrap();
        let mut restored = PersistentStore::open(restored_root.path()).unwrap();
        let cancellation = Cancellation::default();
        let probe = Probe(&cancellation);
        let stage = restored
            .stage_portable_records(&archive.db, &probe)
            .unwrap();
        let prepared = restored
            .prepare_replace_commit(&stage.staging_id, Some(0))
            .unwrap();
        restored.finish_prepared_replace(prepared).unwrap();
        assert_eq!(
            restored.read_root(None).unwrap().value["marker"],
            "synthetic-local"
        );
    }

    #[test]
    fn local_conflict_capture_rejects_forged_paths_and_identities() {
        let root = tempfile::tempdir().unwrap();
        let (reference, _) = durable_local_capture(root.path(), "synthetic-local");
        let mut forged = reference.clone();
        forged.catalog_path = "../captures/capture-a/capture.sqlite".into();
        assert_eq!(
            prepare_local_conflict_snapshot(root.path(), "repository", &forged)
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(
            prepare_local_conflict_snapshot(root.path(), "forged\0repository", &reference)
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
        let mut forged = reference;
        forged.capture_id = "forged\0capture".into();
        assert_eq!(
            prepare_local_conflict_snapshot(root.path(), "repository", &forged)
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
    }

    #[test]
    fn a_remote_backup_saved_to_a_file_keeps_its_device_sections() {
        use super::super::runtime_restore::tests::packaged_backup_seeded;
        use super::super::snapshot_restore::{download_snapshot, SourceTrust};
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let backup = packaged_backup_seeded(&[vec![43; 2048]], |store| {
                store
                    .device_store_mut()
                    .unwrap()
                    .write_setting("accountst", &serde_json::json!("synthetic-export"))
                    .unwrap();
            })
            .await;
            let connected = &backup.connected;
            let stored: risunest_external_storage_format::snapshot::StoredObject =
                serde_json::from_value(backup.restore_source.clone()).unwrap();
            let remote = RemoteObject::from_stored(&stored, &connected.handle).unwrap();
            let work = tempfile::tempdir().unwrap();
            let staging = work.path().join("verified");
            let cancel = Cancellation::default();
            let progress = PhaseProgress::silent();
            let prepared = download_snapshot(&remote, &staging, &connected.root_key, None, SourceTrust::Downloaded,
                connected.provider.as_ref(), &connected.handle, &progress, &cancel).await.unwrap();
            assert!(prepared.captured_by_device.is_some());
            let sections = download_snapshot_sections(&remote, &prepared, &staging, &connected.root_key,
                connected.provider.as_ref(), &connected.handle, &progress, &cancel).await.unwrap();
            let destination = work.path().join("snapshot.risunest");
            export_verified_snapshot(prepared, &sections, &destination, &work.path().join("scratch"), &cancel, "9.8.7-synthetic")
                .unwrap();
            let archive = crate::portable_backup::VerifiedArchive::open(File::open(&destination).unwrap(), work.path(), &Probe(&cancel))
                .unwrap();
            assert!(archive.manifest.library_included);
            assert!(archive.manifest.device_included);
            let included = archive
                .db
                .prepare("SELECT section,record_count FROM device_sections WHERE included=1 AND complete=1 ORDER BY section")
                .unwrap()
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                included.iter().map(|(section, _)| section.as_str()).collect::<Vec<_>>(),
                ["hypa", "local-plugins", "local-settings"]
            );
            assert!(included.iter().any(|(section, count)| section == "local-settings" && *count > 0));
        });
    }
}
