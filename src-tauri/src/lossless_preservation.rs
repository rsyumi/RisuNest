//! A source archive is readable without interpreting damaged application data.
//! It is never installed into the live database or CAS by the normal importer.
use super::*;
use rusqlite::{Connection, OpenFlags};

const EXTENSION: &str = "sourcePreservation";
const ENCODING: &str = "sqlite-source";
pub(super) const DATABASE_PATH: &str = "source.sqlite";
// Only data stores, never recovery archives, caches, jobs, or another backup.
const FILE_ROOTS: &[&str] = &["assets-v2/objects", "assets", "blobstore", "coldstorage"];

pub(super) fn can_preserve(error: &LosslessError) -> bool {
    matches!(
        error.code,
        LosslessErrorCode::InvalidDatabase
            | LosslessErrorCode::InvalidManifest
            | LosslessErrorCode::InvalidPath
            | LosslessErrorCode::DuplicatePath
            | LosslessErrorCode::DuplicateLogicalKey
            | LosslessErrorCode::ManifestTooLarge
            | LosslessErrorCode::BackupIncomplete
            | LosslessErrorCode::UnexpectedReference
            | LosslessErrorCode::MissingReference
            | LosslessErrorCode::HashMismatch
            | LosslessErrorCode::LengthMismatch
            | LosslessErrorCode::Store
    )
}

pub(super) fn is_preservation(manifest: &LosslessManifest) -> bool {
    manifest.extensions.get(EXTENSION).is_some()
}

pub(super) fn repair_required() -> LosslessError {
    LosslessError::new(
        LosslessErrorCode::RepairRequired,
        "The source archive was verified and preserves the original database and files. It requires repair before activation; the current library has not been replaced.",
    )
}

pub(crate) fn create_source_preserving_backup(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    durable_job: &mut DurableCasJob,
    sealed_at_ms: i64,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    match create_and_verify_lossless_backup_v1_durable_report(
        output_path,
        job_staging_root,
        cas,
        store,
        expected_revision,
        durable_job,
        sealed_at_ms,
        cancellation,
    ) {
        Ok(report) => Ok(report),
        Err(error) if can_preserve(&error) => {
            check_cancelled(cancellation)?;
            let lease = store
                .acquire_revision(expected_revision)
                .map_err(store_error)?
                .lease;
            let outcome = capture(
                output_path,
                job_staging_root,
                cas,
                store,
                &lease,
                expected_revision,
                cancellation,
            );
            let released = store.release_revision(&lease).map_err(store_error);
            match (outcome, released) {
                (Ok(report), Ok(())) => {
                    // The normal CAS-backed export was abandoned. The verified
                    // source archive now owns all its bytes and needs no pins.
                    durable_job
                        .release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted)
                        .map_err(LosslessError::io)?;
                    Ok(report)
                }
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(cleanup_error(
                    error,
                    "source archive lease release",
                    cleanup,
                )),
            }
        }
        Err(error) => Err(error),
    }
}

pub(super) fn capture(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &PersistentStore,
    lease: &str,
    expected_revision: i64,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    check_cancelled(cancellation)?;
    let owned = JobOwnedDirectory::create(job_staging_root)?;
    let database_path = owned.as_ref().join("source.db");
    let (character_count, preset_count) =
        match store.capture_preservation_database(lease, &database_path, cancellation) {
            Ok(counts) => counts,
            Err(error) => {
                check_cancelled(cancellation)?;
                return Err(store_error(error));
            }
        };
    check_cancelled(cancellation)?;
    let database_hash = hash_source(&database_path, cancellation)?;
    let mut entries = vec![LosslessWriteEntry {
        logical_path: DATABASE_PATH.into(),
        logical_key: None,
        kind: PayloadKind::Database,
        metadata: serde_json::json!({"encoding": ENCODING}),
        source: database_path,
    }];
    let root = store.repository_root();
    for prefix in FILE_ROOTS {
        let mut path = root.to_owned();
        let mut absent = false;
        for component in prefix.split('/') {
            path.push(component);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if is_link_like(&metadata) => {
                    return Err(invalid_manifest(
                        "source preservation cannot follow a linked storage directory",
                    ))
                }
                Ok(_) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    absent = true;
                    break;
                }
                Err(error) => return Err(LosslessError::io(error)),
            }
        }
        if !absent {
            collect_files(root, &path, &mut entries, cancellation)?;
        }
    }
    entries[1..].sort_by(|left, right| left.logical_path.cmp(&right.logical_path));
    let mut output = IncompleteBackupFile::create(output_path)?;
    let written = write_lossless_package_v1(
        output.file_mut(), &entries,
        LosslessCompatibility {
            oracle_version: FORMAT_VERSION,
            canonical_database_sha256: database_hash,
            reference_graph_sha256: hex::encode(Sha256::digest(b"[]")),
        }, Vec::new(), vec![LosslessWarning {
            code: "source-preserved-repair-required".into(),
            message: "Original storage was preserved without changing or dropping invalid data. Repair is required before activation.".into(),
            metadata: serde_json::json!({}),
        }], serde_json::json!({"sourceRevision":expected_revision, "sourcePreservation":{"encoding":ENCODING}}),
        cancellation,
    )?;
    output.sync()?;
    let verified = verify_lossless_package_v1(
        &mut File::open(output_path).map_err(LosslessError::io)?,
        cancellation,
    )?;
    if verified.manifest != written.manifest || verified.archive_bytes != written.archive_bytes {
        return Err(invalid_manifest(
            "source preservation verification differs from the written archive",
        ));
    }
    // Use the same reader as import, including SQLite structure validation.
    // Preserved objects are job-owned files, never installed under claimed CAS hashes.
    let read_root = JobOwnedDirectory::create(job_staging_root)?;
    let read = read_lossless_package_v1(
        &mut File::open(output_path).map_err(LosslessError::io)?,
        read_root.as_ref(),
        cas,
        cancellation,
    )?;
    if read.archive_sha256 != verified.archive_sha256 {
        return Err(invalid_manifest(
            "source archive changed during import verification",
        ));
    }
    drop(read);
    output.keep();
    Ok(CreatedLosslessBackup {
        archive_bytes: verified.archive_bytes,
        archive_sha256: verified.archive_sha256,
        character_count,
        preset_count,
        warning_codes: vec!["source-preserved-repair-required".into()],
    })
}

fn collect_files(
    root: &Path,
    path: &Path,
    entries: &mut Vec<LosslessWriteEntry>,
    cancellation: &dyn CancellationProbe,
) -> Result<(), LosslessError> {
    check_cancelled(cancellation)?;
    // A file disappearing after directory enumeration is a failed capture,
    // not evidence that it was absent at the start of this inventory.
    let metadata = fs::symlink_metadata(path).map_err(LosslessError::io)?;
    if is_link_like(&metadata) {
        return Err(invalid_manifest(
            "source preservation cannot follow a linked storage path",
        ));
    }
    if metadata.is_dir() {
        for item in fs::read_dir(path).map_err(LosslessError::io)? {
            collect_files(
                root,
                &item.map_err(LosslessError::io)?.path(),
                entries,
                cancellation,
            )?;
        }
    } else if metadata.is_file() {
        let relative = path
            .strip_prefix(root)
            .ok()
            .and_then(Path::to_str)
            .ok_or_else(|| invalid_manifest("source storage path cannot be represented"))?
            .replace('\\', "/");
        validate_source_path(&relative)?;
        if entries.len() >= MAX_ENTRIES {
            return Err(invalid_manifest(
                "source archive inventory exceeds its limit",
            ));
        }
        entries.push(LosslessWriteEntry {
            logical_path: format!("preserved/{}", hex::encode(relative.as_bytes())),
            logical_key: Some(relative),
            kind: PayloadKind::PreservedObject,
            metadata: serde_json::json!({}),
            source: path.to_owned(),
        });
    } else {
        return Err(invalid_manifest(
            "source storage contains a non-regular file",
        ));
    }
    Ok(())
}

fn validate_source_path(path: &str) -> Result<(), LosslessError> {
    if normalize_logical_path(path)? != path
        || !FILE_ROOTS
            .iter()
            .any(|root| path.starts_with(&format!("{root}/")))
        || path.split('/').any(|part| part.contains(':'))
    {
        return Err(invalid_manifest("source archive storage path is invalid"));
    }
    Ok(())
}

fn hash_source(path: &Path, cancellation: &dyn CancellationProbe) -> Result<String, LosslessError> {
    let mut source = File::open(path).map_err(LosslessError::io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; COPY_BUFFER_BYTES];
    loop {
        check_cancelled(cancellation)?;
        let count = source.read(&mut buffer).map_err(LosslessError::io)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) fn validate_manifest(manifest: &LosslessManifest) -> Result<(), LosslessError> {
    if !is_preservation(manifest) {
        if manifest.entries.iter().any(|entry| {
            entry.kind == PayloadKind::PreservedObject
                || entry
                    .metadata
                    .get("encoding")
                    .is_some_and(|value| value == ENCODING)
        }) {
            return Err(invalid_manifest(
                "preserved source entries require the source archive declaration",
            ));
        }
        return Ok(());
    }
    if manifest.extensions[EXTENSION] != serde_json::json!({"encoding":ENCODING})
        || !manifest
            .extensions
            .get("sourceRevision")
            .and_then(Value::as_i64)
            .is_some_and(|revision| revision >= 0)
        || !manifest.references.is_empty()
        || !manifest
            .warnings
            .iter()
            .any(|warning| warning.code == "source-preserved-repair-required")
        || manifest.compatibility.reference_graph_sha256 != hex::encode(Sha256::digest(b"[]"))
    {
        return Err(invalid_manifest("source archive declaration is invalid"));
    }
    for entry in &manifest.entries {
        match entry.kind {
            PayloadKind::Database => {
                if entry.metadata != serde_json::json!({"encoding":ENCODING})
                    || entry.sha256 != manifest.compatibility.canonical_database_sha256
                {
                    return Err(invalid_manifest(
                        "source archive database identity is invalid",
                    ));
                }
            }
            PayloadKind::PreservedObject => {
                let key = entry
                    .logical_key
                    .as_deref()
                    .ok_or_else(|| invalid_manifest("source storage entry has no identity"))?;
                validate_source_path(key)?;
                if entry.logical_path != format!("preserved/{}", hex::encode(key.as_bytes()))
                    || entry.metadata != serde_json::json!({})
                {
                    return Err(invalid_manifest("source storage entry identity is invalid"));
                }
            }
            _ => {
                return Err(invalid_manifest(
                    "source archive cannot mix activatable payload entries",
                ))
            }
        }
    }
    Ok(())
}

pub(super) fn validate_database(
    manifest: &LosslessManifest,
    entries: &[StagedLosslessEntry],
) -> Result<(), LosslessError> {
    let database = entries
        .iter()
        .find(|entry| entry.kind == PayloadKind::Database)
        .and_then(|entry| entry.staged_path.as_deref())
        .ok_or_else(|| invalid_manifest("source archive database has not been staged"))?;
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| invalid_manifest("source archive database cannot be opened"))?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| invalid_manifest("source archive schema cannot be read"))?;
    if version != 1 {
        return Err(invalid_manifest(
            "source archive database schema is unsupported",
        ));
    }
    let integrity: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| invalid_manifest("source archive database structure is invalid"))?;
    if integrity != "ok" {
        return Err(invalid_manifest(
            "source archive database structure is invalid",
        ));
    }
    let revision: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key='currentRevision'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| invalid_manifest("source archive revision is missing"))?;
    let revision: i64 = serde_json::from_str(&revision)
        .map_err(|_| invalid_manifest("source archive revision is invalid"))?;
    if Some(revision)
        != manifest
            .extensions
            .get("sourceRevision")
            .and_then(Value::as_i64)
    {
        return Err(invalid_manifest(
            "source archive revision differs from its manifest",
        ));
    }
    Ok(())
}
