//! Offline export of a fully verified remote snapshot. The downloaded files
//! are activated only in a new scratch PDS, then passed through the normal
//! portable-backup writer and verifier.
use super::{
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    device_capture::DeviceSnapshot,
    snapshot_restore::PreparedRemoteSnapshot,
};
use crate::{
    asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
    device_backup::validate_archive_catalog,
    local_backup::CancellationProbe,
    persistent_store::{
        external_apply::{
            ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord,
        },
        PersistentStore,
    },
};
use risunest_external_storage_format::format::Scope;
use rusqlite::{Connection, OpenFlags};
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotExportReceipt {
    pub destination: PathBuf,
    pub sha256: String,
    pub revision: i64,
}

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
fn decode_hash(value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(value).map_err(corrupt)?;
    bytes.try_into().map_err(|_| corrupt("invalid hash length"))
}

struct Probe<'a>(&'a Cancellation);
impl CancellationProbe for Probe<'_> {
    fn is_cancelled(&self) -> bool {
        self.0.check().is_err()
    }
}

fn copy_device_into_catalog(
    device: &DeviceSnapshot,
    catalog: &crate::portable_backup::Catalog,
    probe: &dyn CancellationProbe,
) -> std::result::Result<(), crate::portable_backup::Error> {
    let source =
        Connection::open_with_flags(&device.sqlite.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_archive_catalog(&source, probe).map_err(|_| {
        crate::portable_backup::Error::Invalid("device catalog verification failed")
    })?;
    let mut sections = source.prepare(
        "SELECT section,schema_version,included,complete,present,record_count,sha256 \
         FROM device_sections ORDER BY section",
    )?;
    let mut rows = sections.query([])?;
    while let Some(row) = rows.next()? {
        if probe.is_cancelled() {
            return Err(crate::portable_backup::Error::Cancelled);
        }
        catalog.db.execute(
            "INSERT INTO device_sections VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ],
        )?;
    }
    drop(rows);
    drop(sections);
    let mut records = source
        .prepare("SELECT section,ordinal,metadata FROM device_records ORDER BY section,ordinal")?;
    let mut rows = records.query([])?;
    while let Some(row) = rows.next()? {
        if probe.is_cancelled() {
            return Err(crate::portable_backup::Error::Cancelled);
        }
        catalog.db.execute(
            "INSERT INTO device_records VALUES(?1,?2,?3)",
            rusqlite::params![
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ],
        )?;
    }
    for blob in &device.blobs {
        let hash = hex::encode(blob.content_hash);
        catalog.add_pinned_file(
            "device",
            &hash,
            "{}",
            &blob.path,
            blob.byte_length,
            &hash,
            probe,
        )?;
    }
    Ok(())
}

fn create_verified_snapshot_backup(
    store: &mut PersistentStore,
    revision: i64,
    device: Option<&DeviceSnapshot>,
    destination: &Path,
    scratch: &Path,
    probe: &dyn CancellationProbe,
) -> std::result::Result<String, crate::portable_backup::Error> {
    let mut pins = DurableCasJob::begin(
        store.repository_root(),
        &uuid::Uuid::new_v4().to_string(),
        CasJobKind::OfficialPublicationOrExportPreparation,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX),
    )?;
    let outcome = (|| {
        let captured = crate::portable_backup::capture_library(
            store, revision, scratch, &mut pins, false, probe,
        )?;
        if captured.repair_required {
            return Err(crate::portable_backup::Error::Invalid(
                "snapshot export requires a valid library",
            ));
        }
        if let Some(device) = device {
            copy_device_into_catalog(device, &captured.catalog, probe)?;
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
            || archive.manifest.device_included != device.is_some()
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

/// Create a standalone `.risunest` file at the path chosen by the native
/// save dialog. After this returns, restoring the archive needs neither the
/// cloud provider nor its credentials or repository key.
pub(crate) fn export_verified_snapshot(
    mut snapshot: PreparedRemoteSnapshot,
    scope: &Scope,
    destination: &Path,
    scratch_parent: &Path,
    cancel: &Cancellation,
) -> Result<SnapshotExportReceipt> {
    cancel.check()?;
    let device = snapshot.device.take();
    if destination.extension().and_then(|value| value.to_str()) != Some("risunest")
        || destination.file_name().is_none()
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let scope_id = decode_hash(&snapshot.scope_id)?;
    let library_scope_id = decode_hash(&snapshot.library_scope_id)?;
    let fingerprint = decode_hash(&snapshot.library_fingerprint)?;
    if scope.id() != scope_id {
        return Err(corrupt("snapshot scope differs"));
    }
    let library_scope = Scope {
        library: scope.library,
        referenced_assets: scope.referenced_assets,
        device_settings: false,
        device_plugins: false,
    };
    if library_scope.id() != library_scope_id {
        return Err(corrupt("snapshot library scope differs"));
    }
    if (scope.device_settings || scope.device_plugins) != device.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    std::fs::create_dir_all(scratch_parent).map_err(transient)?;
    let scratch = tempfile::Builder::new()
        .prefix("external-snapshot-export-")
        .tempdir_in(scratch_parent)
        .map_err(transient)?;
    let mut store = PersistentStore::open(&scratch.path().join("pds")).map_err(transient)?;
    let application = ExternalSnapshotApplication {
        expected_revision: 0,
        staging_root: &snapshot.staging_root,
        scope: &library_scope,
        scope_id: &library_scope_id,
        fingerprint: &fingerprint,
    };
    let records = snapshot.records.into_iter().map(|value| {
        Ok(ExternalSnapshotRecord {
            key: value.key,
            content_hash: value.content_hash,
            byte_length: value.byte_length,
            path: value.path,
        })
    });
    let objects = snapshot.objects.into_iter().map(|value| {
        Ok(ExternalSnapshotObject {
            content_hash: value.content_hash,
            byte_length: value.byte_length,
            path: value.path,
        })
    });
    let prepared = store
        .prepare_external_snapshot_application(&application, records, objects)
        .map_err(transient)?;
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
        device.as_ref(),
        &candidate,
        &archive_scratch,
        &Probe(cancel),
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
        || Ok(()),
    )
    .map_err(|_| transient("snapshot destination publication failed"))?;
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
    use risunest_external_storage_format::format::fingerprint;
    use std::{collections::BTreeMap, fs};

    #[test]
    fn malformed_identity_and_non_archive_destination_write_nothing() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        let snapshot = PreparedRemoteSnapshot {
            snapshot_id: "synthetic".into(),
            repository_id: "repository".into(),
            scope_id: "00".repeat(32),
            library_scope_id: "00".repeat(32),
            fingerprint: "00".repeat(32),
            library_fingerprint: "not-a-hash".into(),
            logical_revision: 1,
            staging_root: staging,
            records: Vec::new(),
            objects: Vec::new(),
            device: None,
            device_sections: Vec::new(),
        };
        let destination = root.path().join("snapshot.bin");
        assert!(export_verified_snapshot(
            snapshot,
            &Scope {
                library: true,
                referenced_assets: true,
                device_settings: false,
                device_plugins: false,
            },
            &destination,
            &root.path().join("scratch"),
            &Cancellation::default(),
        )
        .is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn verified_remote_snapshot_becomes_normal_offline_archive() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        fs::create_dir(&staging).unwrap();
        let scope = Scope {
            library: true,
            referenced_assets: true,
            device_settings: false,
            device_plugins: false,
        };
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
        let scope_id = scope.id();
        let snapshot = PreparedRemoteSnapshot {
            snapshot_id: "synthetic-snapshot".into(),
            repository_id: "synthetic-repository".into(),
            scope_id: hex::encode(scope_id),
            library_scope_id: hex::encode(scope_id),
            fingerprint: hex::encode(fingerprint(&scope_id, &hashes)),
            library_fingerprint: hex::encode(fingerprint(&scope_id, &hashes)),
            logical_revision: 1,
            staging_root: staging,
            records: vec![super::super::snapshot_restore::PreparedRecord {
                key,
                content_hash: encoded.hash,
                byte_length: encoded.size,
                path: record_path,
            }],
            objects: Vec::new(),
            device: None,
            device_sections: Vec::new(),
        };
        let destination = root.path().join("snapshot.risunest");
        let receipt = export_verified_snapshot(
            snapshot,
            &scope,
            &destination,
            &root.path().join("scratch"),
            &Cancellation::default(),
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
}
