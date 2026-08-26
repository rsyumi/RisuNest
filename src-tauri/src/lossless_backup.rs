use crate::{
    asset_repository::{PayloadCas, PreparedPayload},
    local_backup::CancellationProbe,
    persistent_store::{AssetAlias, PersistentStore, StoreError},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 17] = b"RISUNESTLOSSLESS\0";
const FORMAT_VERSION: u32 = 1;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_REFERENCES: usize = 4_000_000;
const MAX_WARNINGS: usize = 65_536;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 512;
const DATABASE_PATH: &str = "database.risudat";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadKind {
    Database,
    Asset,
    Inlay,
    Cold,
}

impl PayloadKind {
    fn target_kind(self) -> Option<&'static str> {
        match self {
            Self::Database => None,
            Self::Asset => Some("asset"),
            Self::Inlay => Some("inlay"),
            Self::Cold => Some("cold"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ReferenceStatus {
    Present,
    ExpectedMissing,
    UnexpectedMissing,
    External,
    Invalid,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LosslessReference {
    pub(crate) owner_kind: String,
    pub(crate) owner_id: String,
    pub(crate) source_path: String,
    pub(crate) occurrence: u64,
    pub(crate) target_kind: String,
    pub(crate) target_key: String,
    pub(crate) status: ReferenceStatus,
    pub(crate) metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LosslessWarning {
    pub(crate) code: String,
    pub(crate) message: String,
    pub(crate) metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LosslessManifestEntry {
    pub(crate) logical_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) logical_key: Option<String>,
    pub(crate) kind: PayloadKind,
    pub(crate) byte_length: u64,
    pub(crate) sha256: String,
    pub(crate) metadata: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LosslessCompatibility {
    pub(crate) oracle_version: u32,
    pub(crate) canonical_database_sha256: String,
    pub(crate) reference_graph_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LosslessManifest {
    pub(crate) version: u32,
    pub(crate) compatibility: LosslessCompatibility,
    pub(crate) entries: Vec<LosslessManifestEntry>,
    pub(crate) references: Vec<LosslessReference>,
    pub(crate) warnings: Vec<LosslessWarning>,
    pub(crate) extensions: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LosslessWriteEntry {
    pub(crate) logical_path: String,
    pub(crate) logical_key: Option<String>,
    pub(crate) kind: PayloadKind,
    pub(crate) metadata: Value,
    pub(crate) source: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LosslessWriteReport {
    pub(crate) manifest: LosslessManifest,
    pub(crate) archive_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct StagedLosslessEntry {
    pub(crate) logical_path: String,
    pub(crate) logical_key: Option<String>,
    pub(crate) kind: PayloadKind,
    pub(crate) byte_length: u64,
    pub(crate) sha256: String,
    pub(crate) metadata: Value,
    pub(crate) staged_path: Option<PathBuf>,
    pub(crate) immutable_object: Option<PreparedPayload>,
}

#[derive(Debug)]
pub(crate) struct LosslessReadReport {
    pub(crate) manifest: LosslessManifest,
    pub(crate) entries: Vec<StagedLosslessEntry>,
    pub(crate) archive_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct VerifiedLosslessBackup {
    pub(crate) manifest: LosslessManifest,
    pub(crate) archive_bytes: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LosslessRestoreReport {
    pub(crate) revision: i64,
    pub(crate) source_bytes: u64,
    pub(crate) backup_bytes: u64,
    pub(crate) warnings: Vec<LosslessWarning>,
}

pub(crate) trait LosslessRestoreHooks {
    fn stage_database(
        &mut self,
        store: &mut PersistentStore,
        staging_id: &str,
        database: &StagedLosslessEntry,
    ) -> Result<(), LosslessError>;

    fn validate_reference_graph(
        &mut self,
        manifest: &LosslessManifest,
    ) -> Result<(), LosslessError>;

    fn create_pre_replacement_backup(
        &mut self,
        cancellation: &dyn CancellationProbe,
    ) -> Result<PathBuf, LosslessError>;

    fn confirm_pre_replacement_backup(
        &mut self,
        backup: &VerifiedLosslessBackup,
    ) -> Result<(), LosslessError>;
}

pub(crate) fn project_payload_aliases(
    entries: &[StagedLosslessEntry],
) -> Result<Vec<AssetAlias>, LosslessError> {
    entries
        .iter()
        .filter(|entry| entry.kind != PayloadKind::Database)
        .map(|entry| {
            let metadata = entry.metadata.as_object().ok_or_else(|| {
                invalid_manifest("lossless package payload metadata must be a JSON object")
            })?;
            let string_field = |field: &str| {
                metadata
                    .get(field)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        invalid_manifest(format!(
                            "lossless package payload metadata field {field} must be a string"
                        ))
                    })
            };
            let dimension = |field: &str| -> Result<Option<i64>, LosslessError> {
                match metadata.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => value
                        .as_i64()
                        .filter(|value| *value >= 0)
                        .map(Some)
                        .ok_or_else(|| {
                            invalid_manifest(format!(
                                "lossless package Inlay metadata field {field} must be nonnegative"
                            ))
                        }),
                }
            };
            let size = i64::try_from(entry.byte_length).map_err(|_| {
                invalid_manifest("lossless package payload exceeds the alias size range")
            })?;
            let logical_key = entry.logical_key.clone().ok_or_else(|| {
                invalid_manifest("lossless package payload entry is missing its logical key")
            })?;
            let object_hash = entry
                .immutable_object
                .as_ref()
                .map(|payload| payload.content_hash.clone())
                .or_else(|| Some(entry.sha256.clone()));
            let (kind, inlay_type, width, height) = if entry.kind == PayloadKind::Inlay {
                (
                    "inlay".to_owned(),
                    metadata
                        .get("inlayType")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    dimension("width")?,
                    dimension("height")?,
                )
            } else {
                ("asset".to_owned(), None, None, None)
            };
            Ok(AssetAlias {
                key: logical_key,
                object_hash,
                kind,
                size,
                mime: string_field("mime")?,
                name: string_field("name")?,
                ext: string_field("ext")?,
                inlay_type,
                width,
                height,
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LosslessErrorCode {
    InvalidHeader,
    UnsupportedVersion,
    ManifestTooLarge,
    InvalidManifest,
    InvalidPath,
    DuplicatePath,
    DuplicateLogicalKey,
    MissingDatabase,
    DuplicateDatabase,
    MissingReference,
    UnexpectedReference,
    HashMismatch,
    LengthMismatch,
    TruncatedInput,
    TrailingData,
    Cancelled,
    RevisionConflict,
    Store,
    Io,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LosslessError {
    pub(crate) code: LosslessErrorCode,
    pub(crate) message: String,
}

impl LosslessError {
    fn new(code: LosslessErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_BYTES {
            let mut end = MAX_ERROR_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self { code, message }
    }

    fn io(error: io::Error) -> Self {
        Self::new(LosslessErrorCode::Io, error.to_string())
    }
}

impl std::fmt::Display for LosslessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LosslessError {}

pub(crate) fn write_lossless_package_v1(
    output: &mut impl Write,
    entries: &[LosslessWriteEntry],
    compatibility: LosslessCompatibility,
    references: Vec<LosslessReference>,
    warnings: Vec<LosslessWarning>,
    extensions: Value,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessWriteReport, LosslessError> {
    check_cancelled(cancellation)?;
    let prepared = prepare_write_entries(entries, cancellation)?;
    let manifest = LosslessManifest {
        version: FORMAT_VERSION,
        compatibility,
        entries: prepared
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect(),
        references,
        warnings,
        extensions,
    };
    validate_manifest(&manifest)?;
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| {
        LosslessError::new(LosslessErrorCode::InvalidManifest, error.to_string())
    })?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(LosslessError::new(
            LosslessErrorCode::ManifestTooLarge,
            "lossless package manifest exceeds the configured limit",
        ));
    }
    let manifest_hash = Sha256::digest(&manifest_bytes);
    write_all_checked(output, MAGIC, cancellation)?;
    write_all_checked(output, &FORMAT_VERSION.to_le_bytes(), cancellation)?;
    write_all_checked(
        output,
        &(manifest_bytes.len() as u64).to_le_bytes(),
        cancellation,
    )?;
    write_all_checked(output, &manifest_hash, cancellation)?;
    write_all_checked(output, &manifest_bytes, cancellation)?;

    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    for entry in &prepared {
        check_cancelled(cancellation)?;
        let mut source = File::open(&entry.source).map_err(LosslessError::io)?;
        let mut remaining = entry.manifest.byte_length;
        let mut hasher = Sha256::new();
        while remaining > 0 {
            check_cancelled(cancellation)?;
            let wanted = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                .expect("bounded copy size");
            let read = source
                .read(&mut buffer[..wanted])
                .map_err(LosslessError::io)?;
            if read == 0 {
                return Err(LosslessError::new(
                    LosslessErrorCode::LengthMismatch,
                    format!(
                        "lossless package source shrank: {}",
                        entry.manifest.logical_path
                    ),
                ));
            }
            hasher.update(&buffer[..read]);
            write_all_checked(output, &buffer[..read], cancellation)?;
            remaining -= read as u64;
        }
        check_cancelled(cancellation)?;
        if source.read(&mut buffer[..1]).map_err(LosslessError::io)? != 0 {
            return Err(LosslessError::new(
                LosslessErrorCode::LengthMismatch,
                format!(
                    "lossless package source grew: {}",
                    entry.manifest.logical_path
                ),
            ));
        }
        if hex::encode(hasher.finalize()) != entry.manifest.sha256 {
            return Err(LosslessError::new(
                LosslessErrorCode::HashMismatch,
                format!(
                    "lossless package source changed: {}",
                    entry.manifest.logical_path
                ),
            ));
        }
    }
    check_cancelled(cancellation)?;
    output.flush().map_err(LosslessError::io)?;
    let payload_bytes = manifest.entries.iter().try_fold(0_u64, |total, entry| {
        total.checked_add(entry.byte_length).ok_or_else(|| {
            LosslessError::new(
                LosslessErrorCode::LengthMismatch,
                "lossless package aggregate payload length overflow",
            )
        })
    })?;
    let archive_bytes = (MAGIC.len() as u64)
        .checked_add(4 + 8 + 32)
        .and_then(|value| value.checked_add(manifest_bytes.len() as u64))
        .and_then(|value| value.checked_add(payload_bytes))
        .ok_or_else(|| {
            LosslessError::new(
                LosslessErrorCode::LengthMismatch,
                "lossless package archive length overflow",
            )
        })?;
    Ok(LosslessWriteReport {
        manifest,
        archive_bytes,
    })
}

struct PreparedWriteEntry {
    manifest: LosslessManifestEntry,
    source: PathBuf,
}

fn prepare_write_entries(
    entries: &[LosslessWriteEntry],
    cancellation: &dyn CancellationProbe,
) -> Result<Vec<PreparedWriteEntry>, LosslessError> {
    if entries.len() > MAX_ENTRIES {
        return Err(invalid_manifest("lossless package has too many entries"));
    }
    let mut prepared = Vec::with_capacity(entries.len());
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    for entry in entries {
        check_cancelled(cancellation)?;
        let byte_length = fs::metadata(&entry.source)
            .map_err(LosslessError::io)?
            .len();
        let mut source = File::open(&entry.source).map_err(LosslessError::io)?;
        let mut hasher = Sha256::new();
        let mut actual_length = 0_u64;
        loop {
            check_cancelled(cancellation)?;
            let read = source.read(&mut buffer).map_err(LosslessError::io)?;
            if read == 0 {
                break;
            }
            actual_length = actual_length.checked_add(read as u64).ok_or_else(|| {
                LosslessError::new(
                    LosslessErrorCode::LengthMismatch,
                    "lossless package source length overflow",
                )
            })?;
            hasher.update(&buffer[..read]);
        }
        if actual_length != byte_length {
            return Err(LosslessError::new(
                LosslessErrorCode::LengthMismatch,
                format!(
                    "lossless package source length changed: {}",
                    entry.logical_path
                ),
            ));
        }
        prepared.push(PreparedWriteEntry {
            manifest: LosslessManifestEntry {
                logical_path: entry.logical_path.clone(),
                logical_key: entry.logical_key.clone(),
                kind: entry.kind,
                byte_length,
                sha256: hex::encode(hasher.finalize()),
                metadata: entry.metadata.clone(),
            },
            source: entry.source.clone(),
        });
    }
    Ok(prepared)
}

pub(crate) fn read_lossless_package_v1(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessReadReport, LosslessError> {
    check_cancelled(cancellation)?;
    let mut source = CountingReader::new(reader);
    let manifest = read_manifest(&mut source, cancellation)?;
    let staging_directory = prepare_staging_directory(job_staging_root)?;
    let mut owned_paths = OwnedPaths::default();
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let staged = if entry.kind == PayloadKind::Database {
            stage_database_entry(
                &mut source,
                entry,
                &staging_directory,
                cancellation,
                &mut owned_paths,
            )?
        } else {
            stage_payload_entry(&mut source, entry, cas, cancellation)?
        };
        entries.push(staged);
    }
    check_cancelled(cancellation)?;
    let mut trailing = [0_u8; 1];
    if source.read(&mut trailing).map_err(LosslessError::io)? != 0 {
        return Err(LosslessError::new(
            LosslessErrorCode::TrailingData,
            "lossless package has trailing data",
        ));
    }
    owned_paths.release();
    Ok(LosslessReadReport {
        manifest,
        entries,
        archive_bytes: source.bytes_read,
    })
}

pub(crate) fn verify_lossless_package_v1(
    reader: &mut impl Read,
    cancellation: &dyn CancellationProbe,
) -> Result<VerifiedLosslessBackup, LosslessError> {
    check_cancelled(cancellation)?;
    let mut source = CountingReader::new(reader);
    let manifest = read_manifest(&mut source, cancellation)?;
    let mut sink = io::sink();
    for entry in &manifest.entries {
        let actual_hash = copy_declared(&mut source, &mut sink, entry.byte_length, cancellation)?;
        require_entry_hash(entry, &actual_hash)?;
    }
    check_cancelled(cancellation)?;
    let mut trailing = [0_u8; 1];
    if source.read(&mut trailing).map_err(LosslessError::io)? != 0 {
        return Err(LosslessError::new(
            LosslessErrorCode::TrailingData,
            "lossless package has trailing data",
        ));
    }
    Ok(VerifiedLosslessBackup {
        manifest,
        archive_bytes: source.bytes_read,
    })
}

pub(crate) fn restore_lossless_package_v1(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    hooks: &mut dyn LosslessRestoreHooks,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    let incoming = read_lossless_package_v1(reader, job_staging_root, cas, cancellation)?;
    let database = incoming
        .entries
        .iter()
        .find(|entry| entry.kind == PayloadKind::Database)
        .expect("validated lossless manifest has one database entry");
    let database_path = database.staged_path.clone();
    let staging_id = match store.replace_begin().map_err(store_error) {
        Ok(staging) => staging.staging_id,
        Err(error) => {
            if let Some(path) = database_path {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
    };
    let outcome = (|| {
        hooks.stage_database(store, &staging_id, database)?;
        check_cancelled(cancellation)?;
        hooks.validate_reference_graph(&incoming.manifest)?;
        check_cancelled(cancellation)?;
        let backup_path = hooks.create_pre_replacement_backup(cancellation)?;
        check_cancelled(cancellation)?;
        let mut backup_file = File::open(&backup_path).map_err(|error| {
            LosslessError::new(
                LosslessErrorCode::Io,
                format!("pre-replacement lossless backup cannot be opened: {error}"),
            )
        })?;
        let backup = verify_lossless_package_v1(&mut backup_file, cancellation)?;
        hooks.confirm_pre_replacement_backup(&backup)?;
        check_cancelled(cancellation)?;
        let aliases = project_payload_aliases(&incoming.entries)?;
        store
            .replace_put_asset_aliases(&staging_id, &aliases)
            .map_err(store_error)?;
        check_cancelled(cancellation)?;
        let revision = store
            .replace_commit(&staging_id, Some(expected_revision))
            .map_err(store_error)?
            .revision;
        Ok(LosslessRestoreReport {
            revision,
            source_bytes: incoming.archive_bytes,
            backup_bytes: backup.archive_bytes,
            warnings: incoming.manifest.warnings.clone(),
        })
    })();
    if let Some(path) = database_path {
        let _ = fs::remove_file(path);
    }
    match outcome {
        Ok(report) => Ok(report),
        Err(error) => match store.replace_abort(&staging_id).map_err(store_error) {
            Ok(()) => Err(error),
            Err(abort_error) => Err(LosslessError::new(
                abort_error.code,
                format!(
                    "{}; staging abort failed: {}",
                    error.message, abort_error.message
                ),
            )),
        },
    }
}

fn store_error(error: StoreError) -> LosslessError {
    let code = if matches!(&error, StoreError::RevisionConflict { .. }) {
        LosslessErrorCode::RevisionConflict
    } else {
        LosslessErrorCode::Store
    };
    LosslessError::new(code, error.to_string())
}

fn read_manifest(
    source: &mut impl Read,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessManifest, LosslessError> {
    let mut magic = [0_u8; MAGIC.len()];
    read_exact_checked(
        source,
        &mut magic,
        cancellation,
        "truncated lossless package header",
    )?;
    if &magic != MAGIC {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidHeader,
            "invalid lossless package header",
        ));
    }
    let mut version = [0_u8; 4];
    read_exact_checked(
        source,
        &mut version,
        cancellation,
        "truncated lossless package version",
    )?;
    let version = u32::from_le_bytes(version);
    if version != FORMAT_VERSION {
        return Err(LosslessError::new(
            LosslessErrorCode::UnsupportedVersion,
            format!("unsupported lossless package version {version}"),
        ));
    }
    let mut manifest_length = [0_u8; 8];
    read_exact_checked(
        source,
        &mut manifest_length,
        cancellation,
        "truncated lossless package manifest length",
    )?;
    let manifest_length = u64::from_le_bytes(manifest_length);
    if manifest_length > MAX_MANIFEST_BYTES {
        return Err(LosslessError::new(
            LosslessErrorCode::ManifestTooLarge,
            "lossless package manifest exceeds the configured limit",
        ));
    }
    let mut expected_hash = [0_u8; 32];
    read_exact_checked(
        source,
        &mut expected_hash,
        cancellation,
        "truncated lossless package manifest hash",
    )?;
    let mut manifest_bytes = vec![0_u8; manifest_length as usize];
    read_exact_checked(
        source,
        &mut manifest_bytes,
        cancellation,
        "truncated lossless package manifest",
    )?;
    if Sha256::digest(&manifest_bytes).as_slice() != expected_hash {
        return Err(LosslessError::new(
            LosslessErrorCode::HashMismatch,
            "lossless package manifest hash does not match",
        ));
    }
    let manifest: LosslessManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        LosslessError::new(
            LosslessErrorCode::InvalidManifest,
            format!("invalid lossless package manifest: {error}"),
        )
    })?;
    if manifest.version != version {
        return Err(LosslessError::new(
            LosslessErrorCode::UnsupportedVersion,
            "lossless package header and manifest versions differ",
        ));
    }
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &LosslessManifest) -> Result<(), LosslessError> {
    if manifest.version != FORMAT_VERSION {
        return Err(LosslessError::new(
            LosslessErrorCode::UnsupportedVersion,
            "unsupported lossless package manifest version",
        ));
    }
    if manifest.compatibility.oracle_version != 1 {
        return Err(invalid_manifest(
            "lossless package compatibility oracle version is unsupported",
        ));
    }
    validate_hash(&manifest.compatibility.canonical_database_sha256)?;
    validate_hash(&manifest.compatibility.reference_graph_sha256)?;
    if manifest.entries.len() > MAX_ENTRIES
        || manifest.references.len() > MAX_REFERENCES
        || manifest.warnings.len() > MAX_WARNINGS
    {
        return Err(invalid_manifest(
            "lossless package manifest cardinality exceeds its limit",
        ));
    }
    let mut paths = HashSet::new();
    let mut logical_keys = HashSet::new();
    let mut payload_targets = HashSet::new();
    let mut database_count = 0;
    for entry in &manifest.entries {
        let normalized = normalize_logical_path(&entry.logical_path)?;
        if normalized != entry.logical_path {
            return Err(LosslessError::new(
                LosslessErrorCode::InvalidPath,
                "lossless package paths must use normalized forward slashes",
            ));
        }
        if !paths.insert(entry.logical_path.clone()) {
            return Err(LosslessError::new(
                LosslessErrorCode::DuplicatePath,
                format!("duplicate lossless package path: {}", entry.logical_path),
            ));
        }
        validate_hash(&entry.sha256)?;
        if entry.byte_length > u32::MAX as u64 {
            return Err(invalid_manifest(
                "lossless package v1 entry exceeds the u32 payload limit",
            ));
        }
        if entry.kind == PayloadKind::Database {
            database_count += 1;
            if entry.logical_path != DATABASE_PATH || entry.logical_key.is_some() {
                return Err(invalid_manifest(
                    "lossless package database entry must be root database.risudat",
                ));
            }
        } else {
            let logical_key = entry.logical_key.as_deref().ok_or_else(|| {
                invalid_manifest("lossless package payload entry is missing its logical key")
            })?;
            if logical_key.is_empty() || logical_key.contains('\0') {
                return Err(invalid_manifest(
                    "lossless package payload logical key is invalid",
                ));
            }
            if !logical_keys.insert(logical_key.to_owned()) {
                return Err(LosslessError::new(
                    LosslessErrorCode::DuplicateLogicalKey,
                    format!("duplicate lossless package logical key: {logical_key}"),
                ));
            }
            validate_payload_metadata(entry)?;
            payload_targets.insert((entry.kind.target_kind().unwrap(), logical_key));
        }
    }
    match database_count {
        0 => {
            return Err(LosslessError::new(
                LosslessErrorCode::MissingDatabase,
                "lossless package does not contain database.risudat",
            ))
        }
        1 => {}
        _ => {
            return Err(LosslessError::new(
                LosslessErrorCode::DuplicateDatabase,
                "lossless package contains more than one database entry",
            ))
        }
    }
    let mut has_expected_missing = false;
    for reference in &manifest.references {
        if reference.status == ReferenceStatus::UnexpectedMissing {
            return Err(LosslessError::new(
                LosslessErrorCode::UnexpectedReference,
                format!(
                    "unexpected missing reference at {}: {}",
                    reference.source_path, reference.target_key
                ),
            ));
        }
        if !matches!(reference.target_kind.as_str(), "asset" | "inlay" | "cold") {
            continue;
        }
        let target = (
            reference.target_kind.as_str(),
            reference.target_key.as_str(),
        );
        match reference.status {
            ReferenceStatus::Present if !payload_targets.contains(&target) => {
                return Err(LosslessError::new(
                    LosslessErrorCode::MissingReference,
                    format!(
                        "present reference has no package payload at {}: {}",
                        reference.source_path, reference.target_key
                    ),
                ));
            }
            ReferenceStatus::ExpectedMissing if payload_targets.contains(&target) => {
                return Err(LosslessError::new(
                    LosslessErrorCode::UnexpectedReference,
                    format!(
                        "expected missing reference has a package payload at {}: {}",
                        reference.source_path, reference.target_key
                    ),
                ));
            }
            ReferenceStatus::ExpectedMissing => has_expected_missing = true,
            _ => {}
        }
    }
    if has_expected_missing
        && !manifest
            .warnings
            .iter()
            .any(|warning| warning.code == "expected-missing-reference")
    {
        return Err(invalid_manifest(
            "lossless package expected missing references require an explicit warning",
        ));
    }
    Ok(())
}

fn validate_payload_metadata(entry: &LosslessManifestEntry) -> Result<(), LosslessError> {
    let metadata = entry.metadata.as_object().ok_or_else(|| {
        invalid_manifest("lossless package payload metadata must be a JSON object")
    })?;
    for field in ["name", "ext", "mime"] {
        if !metadata.get(field).is_some_and(Value::is_string) {
            return Err(invalid_manifest(format!(
                "lossless package payload metadata field {field} must be a string"
            )));
        }
    }
    let inlay_type = metadata.get("inlayType").and_then(Value::as_str);
    if entry.kind == PayloadKind::Inlay {
        if !matches!(inlay_type, Some("image" | "audio" | "video" | "signature")) {
            return Err(invalid_manifest(
                "lossless package Inlay metadata requires a supported inlayType",
            ));
        }
    } else if inlay_type.is_some() {
        return Err(invalid_manifest(
            "lossless package non-Inlay metadata cannot declare inlayType",
        ));
    }
    Ok(())
}

fn stage_database_entry(
    source: &mut impl Read,
    entry: &LosslessManifestEntry,
    staging_directory: &Path,
    cancellation: &dyn CancellationProbe,
    owned_paths: &mut OwnedPaths,
) -> Result<StagedLosslessEntry, LosslessError> {
    let path = staging_directory.join(format!("{}.database", uuid::Uuid::new_v4()));
    let mut guard = IncompleteFile::new(path.clone());
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(LosslessError::io)?;
    let actual_hash = copy_declared(source, &mut output, entry.byte_length, cancellation)?;
    output.flush().map_err(LosslessError::io)?;
    output.sync_all().map_err(LosslessError::io)?;
    drop(output);
    require_entry_hash(entry, &actual_hash)?;
    owned_paths.track(path.clone());
    guard.keep();
    Ok(staged_entry(entry, Some(path), None))
}

fn stage_payload_entry(
    source: &mut impl Read,
    entry: &LosslessManifestEntry,
    cas: &PayloadCas,
    cancellation: &dyn CancellationProbe,
) -> Result<StagedLosslessEntry, LosslessError> {
    let mut declared = DeclaredReader {
        source,
        remaining: entry.byte_length,
        cancellation,
    };
    let prepared = cas.prepare_reader(&mut declared).map_err(|error| {
        if cancellation.is_cancelled() {
            cancelled_error()
        } else if error.kind() == io::ErrorKind::UnexpectedEof {
            LosslessError::new(
                LosslessErrorCode::TruncatedInput,
                format!("truncated lossless package payload: {}", entry.logical_path),
            )
        } else {
            LosslessError::io(error)
        }
    })?;
    if prepared.byte_size != entry.byte_length {
        return Err(LosslessError::new(
            LosslessErrorCode::LengthMismatch,
            format!(
                "lossless package payload length mismatch: {}",
                entry.logical_path
            ),
        ));
    }
    require_entry_hash(entry, &prepared.content_hash)?;
    Ok(staged_entry(entry, None, Some(prepared)))
}

fn staged_entry(
    entry: &LosslessManifestEntry,
    staged_path: Option<PathBuf>,
    immutable_object: Option<PreparedPayload>,
) -> StagedLosslessEntry {
    StagedLosslessEntry {
        logical_path: entry.logical_path.clone(),
        logical_key: entry.logical_key.clone(),
        kind: entry.kind,
        byte_length: entry.byte_length,
        sha256: entry.sha256.clone(),
        metadata: entry.metadata.clone(),
        staged_path,
        immutable_object,
    }
}

fn require_entry_hash(
    entry: &LosslessManifestEntry,
    actual_hash: &str,
) -> Result<(), LosslessError> {
    if actual_hash != entry.sha256 {
        return Err(LosslessError::new(
            LosslessErrorCode::HashMismatch,
            format!(
                "lossless package payload hash mismatch: {}",
                entry.logical_path
            ),
        ));
    }
    Ok(())
}

fn copy_declared(
    source: &mut impl Read,
    output: &mut impl Write,
    byte_length: u64,
    cancellation: &dyn CancellationProbe,
) -> Result<String, LosslessError> {
    let mut remaining = byte_length;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        check_cancelled(cancellation)?;
        let wanted =
            usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).expect("bounded copy size");
        let read = source
            .read(&mut buffer[..wanted])
            .map_err(LosslessError::io)?;
        if read == 0 {
            return Err(LosslessError::new(
                LosslessErrorCode::TruncatedInput,
                "truncated lossless package payload",
            ));
        }
        check_cancelled(cancellation)?;
        output
            .write_all(&buffer[..read])
            .map_err(LosslessError::io)?;
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    check_cancelled(cancellation)?;
    Ok(hex::encode(hasher.finalize()))
}

struct DeclaredReader<'a, R> {
    source: &'a mut R,
    remaining: u64,
    cancellation: &'a dyn CancellationProbe,
}

impl<R: Read> Read for DeclaredReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "lossless package operation cancelled",
            ));
        }
        let wanted =
            usize::try_from(self.remaining.min(buffer.len() as u64)).expect("bounded read size");
        let read = self.source.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated lossless package payload",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

struct CountingReader<'a, R> {
    inner: &'a mut R,
    bytes_read: u64,
}

impl<'a, R> CountingReader<'a, R> {
    fn new(inner: &'a mut R) -> Self {
        Self {
            inner,
            bytes_read: 0,
        }
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read = self
            .bytes_read
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "source size overflow"))?;
        Ok(read)
    }
}

fn prepare_staging_directory(root: &Path) -> Result<PathBuf, LosslessError> {
    let root = fs::canonicalize(root).map_err(LosslessError::io)?;
    if !root.is_dir() {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package staging root is not a directory",
        ));
    }
    let staging = root.join("lossless-v1");
    fs::create_dir_all(&staging).map_err(LosslessError::io)?;
    let staging = fs::canonicalize(staging).map_err(LosslessError::io)?;
    if !staging.starts_with(root) {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package staging directory escapes its owned root",
        ));
    }
    Ok(staging)
}

fn normalize_logical_path(path: &str) -> Result<String, LosslessError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package path is empty, oversized, or contains NUL",
        ));
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package path is absolute",
        ));
    }
    for (index, component) in normalized.split('/').enumerate() {
        if component.is_empty() || component == "." || component == ".." {
            return Err(LosslessError::new(
                LosslessErrorCode::InvalidPath,
                "lossless package path has an invalid component",
            ));
        }
        let bytes = component.as_bytes();
        if index == 0 && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return Err(LosslessError::new(
                LosslessErrorCode::InvalidPath,
                "lossless package path has a drive prefix",
            ));
        }
    }
    Ok(normalized)
}

fn validate_hash(hash: &str) -> Result<(), LosslessError> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    Err(invalid_manifest(
        "lossless package SHA-256 must be lowercase hexadecimal",
    ))
}

fn invalid_manifest(message: impl Into<String>) -> LosslessError {
    LosslessError::new(LosslessErrorCode::InvalidManifest, message)
}

fn check_cancelled(cancellation: &dyn CancellationProbe) -> Result<(), LosslessError> {
    if cancellation.is_cancelled() {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

fn cancelled_error() -> LosslessError {
    LosslessError::new(
        LosslessErrorCode::Cancelled,
        "lossless package operation cancelled",
    )
}

fn read_exact_checked(
    reader: &mut impl Read,
    bytes: &mut [u8],
    cancellation: &dyn CancellationProbe,
    truncated_message: &'static str,
) -> Result<(), LosslessError> {
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancelled(cancellation)?;
        let end = (offset + COPY_BUFFER_BYTES).min(bytes.len());
        let read = reader
            .read(&mut bytes[offset..end])
            .map_err(LosslessError::io)?;
        if read == 0 {
            return Err(LosslessError::new(
                LosslessErrorCode::TruncatedInput,
                truncated_message,
            ));
        }
        offset += read;
    }
    Ok(())
}

fn write_all_checked(
    writer: &mut impl Write,
    bytes: &[u8],
    cancellation: &dyn CancellationProbe,
) -> Result<(), LosslessError> {
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancelled(cancellation)?;
        let end = (offset + COPY_BUFFER_BYTES).min(bytes.len());
        let written = writer
            .write(&bytes[offset..end])
            .map_err(LosslessError::io)?;
        if written == 0 {
            return Err(LosslessError::new(
                LosslessErrorCode::Io,
                "lossless package destination stopped accepting bytes",
            ));
        }
        offset += written;
    }
    Ok(())
}

struct IncompleteFile {
    path: PathBuf,
    keep: bool,
}

impl IncompleteFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for IncompleteFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Default)]
struct OwnedPaths {
    paths: Vec<PathBuf>,
}

impl OwnedPaths {
    fn track(&mut self, path: PathBuf) {
        self.paths.push(path);
    }

    fn release(&mut self) {
        self.paths.clear();
    }
}

impl Drop for OwnedPaths {
    fn drop(&mut self) {
        for path in self.paths.iter().rev() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_backup::NeverCancelled;
    use serde_json::json;
    use std::{
        fs,
        io::Cursor,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };

    #[test]
    fn versioned_package_round_trips_every_payload_kind_and_ordered_references_exactly() {
        let directory = tempfile::tempdir().expect("temporary package fixture");
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir_all(&staging).expect("staging directory");
        fs::create_dir_all(&repository).expect("repository directory");
        let payloads = [
            (
                "database.risudat",
                PayloadKind::Database,
                b"database".as_slice(),
            ),
            (
                "assets/opaque.weird",
                PayloadKind::Asset,
                b"\0\xffasset".as_slice(),
            ),
            ("inlays/image", PayloadKind::Inlay, b"image-webp".as_slice()),
            ("inlays/audio", PayloadKind::Inlay, b"audio-ogg".as_slice()),
            ("inlays/video", PayloadKind::Inlay, b"video-webm".as_slice()),
            (
                "inlays/signature",
                PayloadKind::Inlay,
                b"signature".as_slice(),
            ),
            (
                "cold/character",
                PayloadKind::Cold,
                b"cold-character".as_slice(),
            ),
            ("assets/empty.dat", PayloadKind::Asset, b"".as_slice()),
        ];
        let mut entries = Vec::new();
        for (index, (logical_path, kind, bytes)) in payloads.iter().enumerate() {
            let source = directory.path().join(format!("source-{index}"));
            fs::write(&source, bytes).expect("write source payload");
            entries.push(LosslessWriteEntry {
                logical_path: (*logical_path).to_owned(),
                logical_key: (*logical_path != "database.risudat").then(|| {
                    logical_path
                        .split_once('/')
                        .map_or((*logical_path).to_owned(), |(_, key)| key.to_owned())
                }),
                kind: *kind,
                metadata: json!({
                    "name": logical_path,
                    "ext": logical_path.rsplit_once('.').map_or("", |(_, ext)| ext),
                    "mime": "application/octet-stream",
                    "inlayType": match *logical_path {
                        "inlays/image" => Some("image"),
                        "inlays/audio" => Some("audio"),
                        "inlays/video" => Some("video"),
                        "inlays/signature" => Some("signature"),
                        _ => None,
                    },
                    "roadmap14Unknown": { "ordered": [false, 0, ""] }
                }),
                source,
            });
        }
        let references = vec![
            reference("asset", "opaque.weird", ReferenceStatus::Present, 0),
            reference("asset", "opaque.weird", ReferenceStatus::Present, 1),
            reference(
                "inlay",
                "known-missing",
                ReferenceStatus::ExpectedMissing,
                2,
            ),
        ];
        let warnings = vec![LosslessWarning {
            code: "expected-missing-reference".to_owned(),
            message: "inlay known-missing is absent in the source".to_owned(),
            metadata: json!({ "source": "fixture", "unknown": false }),
        }];
        let mut package = Vec::new();

        let written = write_lossless_package_v1(
            &mut package,
            &entries,
            f0_compatibility(),
            references.clone(),
            warnings.clone(),
            json!({ "roadmap14Unknown": { "empty": {}, "enabled": false } }),
            &NeverCancelled,
        )
        .expect("write lossless package");
        let cas = crate::asset_repository::PayloadCas::new(&repository).expect("payload CAS");
        let restored =
            read_lossless_package_v1(&mut Cursor::new(&package), &staging, &cas, &NeverCancelled)
                .expect("read lossless package");

        assert_eq!(written.manifest, restored.manifest);
        assert_eq!(restored.manifest.references, references);
        assert_eq!(restored.manifest.warnings, warnings);
        assert_eq!(restored.manifest.compatibility, f0_compatibility());
        assert_eq!(
            restored.manifest.extensions["roadmap14Unknown"]["enabled"],
            false
        );
        for (expected, restored_entry) in payloads.iter().zip(&restored.entries) {
            assert_eq!(restored_entry.logical_path, expected.0);
            let actual = if let Some(path) = &restored_entry.staged_path {
                fs::read(path).expect("read staged database")
            } else {
                cas.read_object(&restored_entry.sha256)
                    .expect("read CAS object")
                    .expect("CAS object exists")
            };
            assert_eq!(actual, expected.2, "byte mismatch for {}", expected.0);
        }
    }

    #[test]
    fn restore_verifies_graph_and_complete_old_backup_before_one_activation() {
        struct Hooks {
            events: Vec<&'static str>,
            backup_path: PathBuf,
            backup_bytes: Vec<u8>,
        }

        impl LosslessRestoreHooks for Hooks {
            fn stage_database(
                &mut self,
                store: &mut PersistentStore,
                staging_id: &str,
                database: &StagedLosslessEntry,
            ) -> Result<(), LosslessError> {
                assert_eq!(
                    fs::read(database.staged_path.as_ref().unwrap()).unwrap(),
                    b"new-db"
                );
                store
                    .replace_put_root(staging_id, &json!({ "database": "new" }))
                    .map_err(store_error)?;
                store
                    .replace_put_presets(staging_id, &[])
                    .map_err(store_error)?;
                store
                    .replace_add_characters(staging_id, &[])
                    .map_err(store_error)?;
                self.events.push("stage-database");
                Ok(())
            }

            fn validate_reference_graph(
                &mut self,
                manifest: &LosslessManifest,
            ) -> Result<(), LosslessError> {
                assert_eq!(manifest.references[0].occurrence, 0);
                assert_eq!(manifest.compatibility, f0_compatibility());
                self.events.push("validate-graph");
                Ok(())
            }

            fn create_pre_replacement_backup(
                &mut self,
                _cancellation: &dyn CancellationProbe,
            ) -> Result<PathBuf, LosslessError> {
                self.events.push("create-backup");
                fs::write(&self.backup_path, &self.backup_bytes).unwrap();
                Ok(self.backup_path.clone())
            }

            fn confirm_pre_replacement_backup(
                &mut self,
                backup: &VerifiedLosslessBackup,
            ) -> Result<(), LosslessError> {
                assert_eq!(
                    backup.manifest.entries[0].sha256,
                    hex::encode(Sha256::digest(b"old-db"))
                );
                self.events.push("confirm-backup");
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = minimal_package(directory.path(), "new", b"new-db", b"new-asset");
        let backup_bytes = minimal_package(directory.path(), "old", b"old-db", b"old-asset");
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut hooks = Hooks {
            events: Vec::new(),
            backup_path,
            backup_bytes,
        };
        let cas = PayloadCas::new(repository).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();

        let restored = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            0,
            &mut hooks,
            &NeverCancelled,
        )
        .expect("restore commits complete new state");

        assert_eq!(restored.revision, 1);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["database"], "new");
        assert_eq!(
            hooks.events,
            [
                "stage-database",
                "validate-graph",
                "create-backup",
                "confirm-backup",
            ]
        );
    }

    #[test]
    fn sqlite_reopen_observes_complete_old_or_database_with_one_alias_generation() {
        struct StoreHooks {
            backup_path: PathBuf,
            backup_bytes: Vec<u8>,
        }

        impl LosslessRestoreHooks for StoreHooks {
            fn stage_database(
                &mut self,
                store: &mut PersistentStore,
                staging_id: &str,
                database: &StagedLosslessEntry,
            ) -> Result<(), LosslessError> {
                let value: Value = serde_json::from_slice(
                    &fs::read(database.staged_path.as_ref().unwrap()).map_err(LosslessError::io)?,
                )
                .map_err(|error| invalid_manifest(error.to_string()))?;
                store
                    .replace_put_root(staging_id, &value)
                    .map_err(store_error)?;
                store
                    .replace_put_presets(staging_id, &[])
                    .map_err(store_error)?;
                store
                    .replace_add_characters(staging_id, &[])
                    .map_err(store_error)?;
                Ok(())
            }

            fn validate_reference_graph(
                &mut self,
                manifest: &LosslessManifest,
            ) -> Result<(), LosslessError> {
                assert_eq!(
                    manifest
                        .references
                        .iter()
                        .map(|reference| reference.occurrence)
                        .collect::<Vec<_>>(),
                    [0, 1, 2]
                );
                assert_eq!(manifest.compatibility, f0_compatibility());
                Ok(())
            }

            fn create_pre_replacement_backup(
                &mut self,
                _cancellation: &dyn CancellationProbe,
            ) -> Result<PathBuf, LosslessError> {
                fs::write(&self.backup_path, &self.backup_bytes).map_err(LosslessError::io)?;
                Ok(self.backup_path.clone())
            }

            fn confirm_pre_replacement_backup(
                &mut self,
                backup: &VerifiedLosslessBackup,
            ) -> Result<(), LosslessError> {
                let database = backup
                    .manifest
                    .entries
                    .iter()
                    .find(|entry| entry.kind == PayloadKind::Database)
                    .unwrap();
                if database.sha256 != hex::encode(Sha256::digest(br#"{"username":"Old"}"#)) {
                    return Err(invalid_manifest(
                        "pre-replacement backup is not the current database",
                    ));
                }
                Ok(())
            }
        }

        for fail_before_commit in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path().join("job-staging");
            let repository = directory.path().join("repository");
            fs::create_dir(&staging).unwrap();
            fs::create_dir(&repository).unwrap();
            let incoming = atomic_package(directory.path());
            let backup = minimal_package(
                directory.path(),
                if fail_before_commit {
                    "old-fail"
                } else {
                    "old-success"
                },
                br#"{"username":"Old"}"#,
                b"old-asset",
            );
            let backup_path = directory.path().join("pre-replacement.lossless");
            let mut store =
                crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
            let old = store.replace_begin().unwrap().staging_id;
            store
                .replace_put_root(&old, &json!({ "username": "Old" }))
                .unwrap();
            store.replace_put_presets(&old, &[]).unwrap();
            store.replace_add_characters(&old, &[]).unwrap();
            store.replace_commit(&old, Some(0)).unwrap();
            let mut hooks = StoreHooks {
                backup_path,
                backup_bytes: backup,
            };
            let cas = PayloadCas::new(&repository).unwrap();

            let result = restore_lossless_package_v1(
                &mut Cursor::new(incoming),
                &staging,
                &cas,
                &mut store,
                if fail_before_commit { 0 } else { 1 },
                &mut hooks,
                &NeverCancelled,
            );

            drop(store);
            let reopened =
                crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
            if fail_before_commit {
                assert_eq!(
                    result.unwrap_err().code,
                    LosslessErrorCode::RevisionConflict
                );
                assert_eq!(reopened.revision().unwrap(), 1);
                assert_eq!(reopened.materialize(None).unwrap()["username"], "Old");
                assert_eq!(reopened.read_asset_alias("asset.bin", None).unwrap(), None);
            } else {
                assert_eq!(result.unwrap().revision, 2);
                assert_eq!(reopened.revision().unwrap(), 2);
                assert_eq!(reopened.materialize(None).unwrap()["username"], "New");
                for key in ["asset.bin", "audio", "character"] {
                    let alias = reopened
                        .read_asset_alias(key, None)
                        .unwrap()
                        .expect("payload alias activates with database");
                    assert_eq!(alias.revision, 2);
                    assert_eq!(
                        cas.read_object(alias.value.object_hash.as_ref().unwrap())
                            .unwrap()
                            .is_some(),
                        true
                    );
                }
                assert_eq!(
                    reopened
                        .read_asset_alias("character", None)
                        .unwrap()
                        .unwrap()
                        .value
                        .kind,
                    "asset"
                );
            }
        }
    }

    #[test]
    fn process_reopen_sweeps_an_abandoned_lossless_database_and_alias_generation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let package = atomic_package(directory.path());
        let cas = PayloadCas::new(&repository).unwrap();
        let incoming =
            read_lossless_package_v1(&mut Cursor::new(package), &staging, &cas, &NeverCancelled)
                .unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let old = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&old, &json!({ "username": "Old" }))
            .unwrap();
        store.replace_put_presets(&old, &[]).unwrap();
        store.replace_add_characters(&old, &[]).unwrap();
        store.replace_commit(&old, Some(0)).unwrap();
        let abandoned = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&abandoned, &json!({ "username": "New" }))
            .unwrap();
        store.replace_put_presets(&abandoned, &[]).unwrap();
        store.replace_add_characters(&abandoned, &[]).unwrap();
        store
            .replace_put_asset_aliases(
                &abandoned,
                &project_payload_aliases(&incoming.entries).unwrap(),
            )
            .unwrap();

        drop(store);
        let reopened = PersistentStore::open(directory.path()).unwrap();

        assert_eq!(reopened.revision().unwrap(), 1);
        assert_eq!(reopened.materialize(None).unwrap()["username"], "Old");
        assert_eq!(reopened.read_asset_alias("asset.bin", None).unwrap(), None);
    }

    #[test]
    fn large_manifest_and_payload_never_request_more_than_one_copy_buffer() {
        struct TrackingIo<T> {
            inner: T,
            largest_read: usize,
            largest_write: usize,
        }

        impl<T: Read> Read for TrackingIo<T> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                self.largest_read = self.largest_read.max(bytes.len());
                self.inner.read(bytes)
            }
        }

        impl<T: Write> Write for TrackingIo<T> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.largest_write = self.largest_write.max(bytes.len());
                self.inner.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                self.inner.flush()
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let database = directory.path().join("bounded-database");
        let payload = directory.path().join("bounded-payload");
        fs::write(&database, b"database").unwrap();
        File::create(&payload)
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        let package_path = directory.path().join("bounded.lossless");
        let mut output = TrackingIo {
            inner: File::create(&package_path).unwrap(),
            largest_read: 0,
            largest_write: 0,
        };
        let entries = vec![
            LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database,
            },
            LosslessWriteEntry {
                logical_path: "assets/bounded.bin".to_owned(),
                logical_key: Some("bounded.bin".to_owned()),
                kind: PayloadKind::Asset,
                metadata: json!({
                    "name": "bounded.bin",
                    "ext": "bin",
                    "mime": "application/octet-stream",
                    "unknownPadding": "x".repeat(128 * 1024)
                }),
                source: payload,
            },
        ];

        write_lossless_package_v1(
            &mut output,
            &entries,
            compatibility(),
            vec![reference(
                "asset",
                "bounded.bin",
                ReferenceStatus::Present,
                0,
            )],
            vec![],
            json!({}),
            &NeverCancelled,
        )
        .unwrap();
        assert!(output.largest_write <= COPY_BUFFER_BYTES);
        drop(output);

        let mut input = TrackingIo {
            inner: File::open(package_path).unwrap(),
            largest_read: 0,
            largest_write: 0,
        };
        let cas = PayloadCas::new(repository).unwrap();
        let restored =
            read_lossless_package_v1(&mut input, &staging, &cas, &NeverCancelled).unwrap();

        assert!(input.largest_read <= COPY_BUFFER_BYTES);
        assert!(restored.archive_bytes > 16 * 1024 * 1024);
    }

    #[test]
    fn codec_rejects_corruption_truncation_duplicates_and_missing_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let package = minimal_package(directory.path(), "valid", b"database", b"asset");
        let cas = PayloadCas::new(&repository).unwrap();

        let mut corrupt_manifest = package.clone();
        corrupt_manifest[MAGIC.len() + 4 + 8] ^= 1;
        assert_eq!(
            read_lossless_package_v1(
                &mut Cursor::new(corrupt_manifest),
                &staging,
                &cas,
                &NeverCancelled,
            )
            .unwrap_err()
            .code,
            LosslessErrorCode::HashMismatch
        );

        let mut corrupt_payload = package.clone();
        let manifest_length = u64::from_le_bytes(
            package[MAGIC.len() + 4..MAGIC.len() + 12]
                .try_into()
                .unwrap(),
        ) as usize;
        let payload_offset = MAGIC.len() + 4 + 8 + 32 + manifest_length;
        corrupt_payload[payload_offset] ^= 1;
        assert_eq!(
            read_lossless_package_v1(
                &mut Cursor::new(corrupt_payload),
                &staging,
                &cas,
                &NeverCancelled,
            )
            .unwrap_err()
            .code,
            LosslessErrorCode::HashMismatch
        );

        for invalid in [package[..package.len() - 1].to_vec(), {
            let mut trailing = package.clone();
            trailing.push(0);
            trailing
        }] {
            let error = read_lossless_package_v1(
                &mut Cursor::new(invalid),
                &staging,
                &cas,
                &NeverCancelled,
            )
            .unwrap_err();
            assert!(matches!(
                error.code,
                LosslessErrorCode::TruncatedInput | LosslessErrorCode::TrailingData
            ));
        }

        let database_source = directory.path().join("duplicate-database");
        let first_source = directory.path().join("duplicate-first");
        let second_source = directory.path().join("duplicate-second");
        fs::write(&database_source, b"database").unwrap();
        fs::write(&first_source, b"one").unwrap();
        fs::write(&second_source, b"two").unwrap();
        let database_entry = LosslessWriteEntry {
            logical_path: DATABASE_PATH.to_owned(),
            logical_key: None,
            kind: PayloadKind::Database,
            metadata: json!({}),
            source: database_source,
        };
        let asset_entry = |path: &str, key: &str, source: &Path| LosslessWriteEntry {
            logical_path: path.to_owned(),
            logical_key: Some(key.to_owned()),
            kind: PayloadKind::Asset,
            metadata: json!({ "name": key, "ext": "bin", "mime": "application/octet-stream" }),
            source: source.to_path_buf(),
        };
        let duplicate_path = vec![
            database_entry.clone(),
            asset_entry("assets/duplicate.bin", "one", &first_source),
            asset_entry("assets/duplicate.bin", "two", &second_source),
        ];
        assert_eq!(
            write_lossless_package_v1(
                &mut Vec::new(),
                &duplicate_path,
                compatibility(),
                vec![],
                vec![],
                json!({}),
                &NeverCancelled,
            )
            .unwrap_err()
            .code,
            LosslessErrorCode::DuplicatePath
        );
        let duplicate_key = vec![
            database_entry.clone(),
            asset_entry("assets/one.bin", "duplicate", &first_source),
            asset_entry("assets/two.bin", "duplicate", &second_source),
        ];
        assert_eq!(
            write_lossless_package_v1(
                &mut Vec::new(),
                &duplicate_key,
                compatibility(),
                vec![],
                vec![],
                json!({}),
                &NeverCancelled,
            )
            .unwrap_err()
            .code,
            LosslessErrorCode::DuplicateLogicalKey
        );
        assert_eq!(
            write_lossless_package_v1(
                &mut Vec::new(),
                &[database_entry],
                compatibility(),
                vec![reference("asset", "missing", ReferenceStatus::Present, 0)],
                vec![],
                json!({}),
                &NeverCancelled,
            )
            .unwrap_err()
            .code,
            LosslessErrorCode::MissingReference
        );
        assert!(fs::read_dir(staging.join("lossless-v1"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn version_one_retains_the_u32_per_entry_limit_without_allocating_the_boundary() {
        let mut manifest = LosslessManifest {
            version: FORMAT_VERSION,
            compatibility: compatibility(),
            entries: vec![
                LosslessManifestEntry {
                    logical_path: DATABASE_PATH.to_owned(),
                    logical_key: None,
                    kind: PayloadKind::Database,
                    byte_length: 0,
                    sha256: hex::encode(Sha256::digest([])),
                    metadata: json!({}),
                },
                LosslessManifestEntry {
                    logical_path: "assets/boundary.bin".to_owned(),
                    logical_key: Some("boundary.bin".to_owned()),
                    kind: PayloadKind::Asset,
                    byte_length: u32::MAX as u64,
                    sha256: "11".repeat(32),
                    metadata: json!({
                        "name": "boundary.bin", "ext": "bin", "mime": "application/octet-stream"
                    }),
                },
            ],
            references: vec![],
            warnings: vec![],
            extensions: json!({}),
        };

        validate_manifest(&manifest).expect("u32 maximum remains representable");
        manifest.entries[1].byte_length = u32::MAX as u64 + 1;
        let error = validate_manifest(&manifest).unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        assert!(error.message.contains("u32 payload limit"));
    }

    #[test]
    fn cancellation_graph_rejection_and_corrupt_backup_abort_without_activation() {
        #[derive(Clone, Copy)]
        enum Failure {
            Graph,
            CancelAfterGraph,
            CorruptBackup,
        }

        struct Hooks {
            failure: Failure,
            events: Vec<&'static str>,
            cancelled: Arc<AtomicBool>,
            backup_path: PathBuf,
            backup: Vec<u8>,
        }

        impl LosslessRestoreHooks for Hooks {
            fn stage_database(
                &mut self,
                store: &mut PersistentStore,
                staging_id: &str,
                _database: &StagedLosslessEntry,
            ) -> Result<(), LosslessError> {
                store
                    .replace_put_root(staging_id, &json!({ "database": "new" }))
                    .map_err(store_error)?;
                store
                    .replace_put_presets(staging_id, &[])
                    .map_err(store_error)?;
                store
                    .replace_add_characters(staging_id, &[])
                    .map_err(store_error)?;
                self.events.push("stage");
                Ok(())
            }

            fn validate_reference_graph(
                &mut self,
                _manifest: &LosslessManifest,
            ) -> Result<(), LosslessError> {
                self.events.push("graph");
                match self.failure {
                    Failure::Graph => Err(LosslessError::new(
                        LosslessErrorCode::UnexpectedReference,
                        "F0 ordered reference graph differs",
                    )),
                    Failure::CancelAfterGraph => {
                        self.cancelled.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                    Failure::CorruptBackup => Ok(()),
                }
            }

            fn create_pre_replacement_backup(
                &mut self,
                _cancellation: &dyn CancellationProbe,
            ) -> Result<PathBuf, LosslessError> {
                self.events.push("backup");
                let mut bytes = self.backup.clone();
                *bytes.last_mut().unwrap() ^= 1;
                fs::write(&self.backup_path, bytes).map_err(LosslessError::io)?;
                Ok(self.backup_path.clone())
            }

            fn confirm_pre_replacement_backup(
                &mut self,
                _backup: &VerifiedLosslessBackup,
            ) -> Result<(), LosslessError> {
                self.events.push("confirm");
                Ok(())
            }
        }

        for (index, failure) in [
            Failure::Graph,
            Failure::CancelAfterGraph,
            Failure::CorruptBackup,
        ]
        .into_iter()
        .enumerate()
        {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path().join("staging");
            let repository = directory.path().join("repository");
            fs::create_dir(&staging).unwrap();
            fs::create_dir(&repository).unwrap();
            let incoming = minimal_package(
                directory.path(),
                &format!("incoming-{index}"),
                b"new-db",
                b"new-asset",
            );
            let backup = minimal_package(
                directory.path(),
                &format!("backup-{index}"),
                b"old-db",
                b"old-asset",
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancellation = crate::local_backup::AtomicCancellation::new(Arc::clone(&cancelled));
            let mut hooks = Hooks {
                failure,
                events: Vec::new(),
                cancelled,
                backup_path: directory.path().join("pre-replacement.lossless"),
                backup,
            };
            let cas = PayloadCas::new(repository).unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();

            let error = restore_lossless_package_v1(
                &mut Cursor::new(incoming),
                &staging,
                &cas,
                &mut store,
                0,
                &mut hooks,
                &cancellation,
            )
            .unwrap_err();

            assert_eq!(store.revision().unwrap(), 0);
            assert_eq!(
                store.read_asset_alias("incoming-0.bin", None).unwrap(),
                None
            );
            match failure {
                Failure::Graph => assert_eq!(error.code, LosslessErrorCode::UnexpectedReference),
                Failure::CancelAfterGraph => {
                    assert_eq!(error.code, LosslessErrorCode::Cancelled)
                }
                Failure::CorruptBackup => assert_eq!(error.code, LosslessErrorCode::HashMismatch),
            }
            assert!(!hooks.events.contains(&"confirm"));
        }

        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("cancelled-database");
        fs::write(&database, b"database").unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));
        let error = write_lossless_package_v1(
            &mut Vec::new(),
            &[LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database,
            }],
            compatibility(),
            vec![],
            vec![],
            json!({}),
            &crate::local_backup::AtomicCancellation::new(cancelled),
        )
        .unwrap_err();
        assert_eq!(error.code, LosslessErrorCode::Cancelled);
    }

    #[test]
    fn fifty_thousand_ordered_reference_occurrences_verify_without_payload_materialization() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("database");
        let asset = directory.path().join("asset");
        let package = directory.path().join("fifty-thousand.lossless");
        fs::write(&database, b"database").unwrap();
        fs::write(&asset, b"asset").unwrap();
        let entries = vec![
            LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database,
            },
            LosslessWriteEntry {
                logical_path: "assets/shared.bin".to_owned(),
                logical_key: Some("shared.bin".to_owned()),
                kind: PayloadKind::Asset,
                metadata: json!({
                    "name": "shared.bin", "ext": "bin", "mime": "application/octet-stream"
                }),
                source: asset,
            },
        ];
        let references = (0..50_000)
            .map(|occurrence| {
                reference("asset", "shared.bin", ReferenceStatus::Present, occurrence)
            })
            .collect();

        let mut output = File::create(&package).unwrap();
        write_lossless_package_v1(
            &mut output,
            &entries,
            f0_compatibility(),
            references,
            vec![],
            json!({}),
            &NeverCancelled,
        )
        .unwrap();
        drop(output);
        let verified =
            verify_lossless_package_v1(&mut File::open(package).unwrap(), &NeverCancelled).unwrap();

        assert_eq!(verified.manifest.references.len(), 50_000);
        assert_eq!(verified.manifest.references[0].occurrence, 0);
        assert_eq!(verified.manifest.references[49_999].occurrence, 49_999);
    }

    fn atomic_package(root: &Path) -> Vec<u8> {
        let inputs = [
            (
                "database.risudat",
                None,
                PayloadKind::Database,
                br#"{"username":"New"}"#.as_slice(),
                json!({}),
            ),
            (
                "assets/asset.bin",
                Some("asset.bin"),
                PayloadKind::Asset,
                b"asset".as_slice(),
                json!({
                    "name": "asset.bin", "ext": "bin", "mime": "application/octet-stream"
                }),
            ),
            (
                "inlays/audio",
                Some("audio"),
                PayloadKind::Inlay,
                b"audio".as_slice(),
                json!({
                    "name": "audio.ogg", "ext": "ogg", "mime": "audio/ogg", "inlayType": "audio"
                }),
            ),
            (
                "cold/character",
                Some("character"),
                PayloadKind::Cold,
                b"cold".as_slice(),
                json!({
                    "name": "character.json", "ext": "json", "mime": "application/json"
                }),
            ),
        ];
        let entries = inputs
            .iter()
            .enumerate()
            .map(
                |(index, (logical_path, logical_key, kind, bytes, metadata))| {
                    let source = root.join(format!("atomic-source-{index}"));
                    fs::write(&source, bytes).unwrap();
                    LosslessWriteEntry {
                        logical_path: (*logical_path).to_owned(),
                        logical_key: logical_key.map(str::to_owned),
                        kind: *kind,
                        metadata: metadata.clone(),
                        source,
                    }
                },
            )
            .collect::<Vec<_>>();
        let references = vec![
            reference("asset", "asset.bin", ReferenceStatus::Present, 0),
            reference("inlay", "audio", ReferenceStatus::Present, 1),
            reference("cold", "character", ReferenceStatus::Present, 2),
        ];
        let mut package = Vec::new();
        write_lossless_package_v1(
            &mut package,
            &entries,
            f0_compatibility(),
            references,
            vec![],
            json!({}),
            &NeverCancelled,
        )
        .unwrap();
        package
    }

    fn minimal_package(root: &Path, prefix: &str, database: &[u8], asset: &[u8]) -> Vec<u8> {
        let database_source = root.join(format!("{prefix}-database"));
        let asset_source = root.join(format!("{prefix}-asset"));
        fs::write(&database_source, database).unwrap();
        fs::write(&asset_source, asset).unwrap();
        let entries = vec![
            LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database_source,
            },
            LosslessWriteEntry {
                logical_path: format!("assets/{prefix}.bin"),
                logical_key: Some(format!("{prefix}.bin")),
                kind: PayloadKind::Asset,
                metadata: json!({
                    "name": format!("{prefix}.bin"),
                    "ext": "bin",
                    "mime": "application/octet-stream"
                }),
                source: asset_source,
            },
        ];
        let references = vec![reference(
            "asset",
            &format!("{prefix}.bin"),
            ReferenceStatus::Present,
            0,
        )];
        let mut package = Vec::new();
        write_lossless_package_v1(
            &mut package,
            &entries,
            f0_compatibility(),
            references,
            vec![],
            json!({}),
            &NeverCancelled,
        )
        .unwrap();
        package
    }

    fn reference(
        target_kind: &str,
        target_key: &str,
        status: ReferenceStatus,
        occurrence: u64,
    ) -> LosslessReference {
        LosslessReference {
            owner_kind: "fixture".to_owned(),
            owner_id: "root".to_owned(),
            source_path: format!("$.references[{occurrence}]"),
            occurrence,
            target_kind: target_kind.to_owned(),
            target_key: target_key.to_owned(),
            status,
            metadata: json!({ "field": "fixture", "unknown": occurrence }),
        }
    }

    fn compatibility() -> LosslessCompatibility {
        LosslessCompatibility {
            oracle_version: 1,
            canonical_database_sha256: "11".repeat(32),
            reference_graph_sha256: "22".repeat(32),
        }
    }

    fn f0_compatibility() -> LosslessCompatibility {
        let manifest: Value = serde_json::from_str(include_str!(
            "../../src/ts/storage/tests/roadmap14/fixtures/compatibility-manifest.json"
        ))
        .expect("F0 compatibility manifest");
        LosslessCompatibility {
            oracle_version: manifest["version"].as_u64().unwrap() as u32,
            canonical_database_sha256: manifest["canonicalSha256"].as_str().unwrap().to_owned(),
            reference_graph_sha256: manifest["referenceGraphSha256"]
                .as_str()
                .unwrap()
                .to_owned(),
        }
    }
}
