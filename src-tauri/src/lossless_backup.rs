use crate::trust_boundary::{is_link_like, is_lower_hex_256};
use crate::{
    asset_repository::{
        job_pins::{CasObjectRole, DurableCasJob},
        owner_manifest_codec::{decode_owner_manifest, OwnerManifestEntry},
        PayloadCas, PreparedPayload,
    },
    local_backup::CancellationProbe,
    lossless_f0::{
        legacy_canonical_database_sha256_v1, rebuild_f0_v1, validate_f0_v1, F0Error, F0ErrorCode,
        F0ExpectedMissing, F0PayloadDescriptor, F0PayloadKind, F0Reference, F0ReferenceStatus,
        F0Validation,
    },
    native_file_jobs::{
        restore::{self as block_restore, ReplacementSink, RestoreControl},
        JobDetail, JobPhase, JobProgress,
    },
    persistent_store::{
        materialized_asset_owner_entries, AssetAlias, AssetOwnerHead, AssetOwnerLocator,
        AssetRepositoryAuthorityState, ColdAlias, ColdPayloadAuthorityState, PersistentStore,
        RevisionResult, StagingResult, StoreError, StoreResult,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAGIC: &[u8; 17] = b"RISUNESTLOSSLESS\0";
const FORMAT_VERSION: u32 = 1;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OWNER_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_REFERENCES: usize = 4_000_000;
const MAX_WARNINGS: usize = 65_536;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 512;
const DATABASE_PATH: &str = "database.risudat";
const ALIAS_METADATA_FIELD: &str = "risuNestAliasMetadata";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PayloadKind {
    Database,
    Asset,
    Inlay,
    Cold,
    OwnerManifest,
    OwnerPayload,
}

impl PayloadKind {
    fn target_kind(self) -> Option<&'static str> {
        match self {
            Self::Database => None,
            Self::Asset => Some("asset"),
            Self::Inlay => Some("inlay"),
            Self::Cold => Some("cold"),
            Self::OwnerManifest | Self::OwnerPayload => None,
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
    pub(crate) staged_path: Option<JobOwnedFile>,
    pub(crate) immutable_object: Option<PreparedPayload>,
}

#[derive(Debug)]
pub(crate) struct LosslessReadReport {
    pub(crate) manifest: LosslessManifest,
    pub(crate) entries: Vec<StagedLosslessEntry>,
    pub(crate) archive_bytes: u64,
    pub(crate) archive_sha256: String,
}

#[derive(Debug)]
pub(crate) struct VerifiedLosslessBackup {
    pub(crate) manifest: LosslessManifest,
    pub(crate) archive_bytes: u64,
    pub(crate) archive_sha256: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CreatedLosslessBackup {
    pub(crate) archive_bytes: u64,
    pub(crate) archive_sha256: String,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LosslessPeerSourceBinding {
    operation_id: String,
    side: String,
    generation_id: String,
    manifest_hash: String,
    generation_sequence: String,
}

impl LosslessPeerSourceBinding {
    pub(crate) fn new(
        operation_id: &str,
        side: &str,
        generation_id: &str,
        manifest_hash: &str,
        generation_sequence: &str,
    ) -> Self {
        Self {
            operation_id: operation_id.to_owned(),
            side: side.to_owned(),
            generation_id: generation_id.to_owned(),
            manifest_hash: manifest_hash.to_owned(),
            generation_sequence: generation_sequence.to_owned(),
        }
    }

    pub(crate) fn extension(&self) -> Value {
        serde_json::json!({
            "operationId": self.operation_id,
            "side": self.side,
            "generationId": self.generation_id,
            "manifestHash": self.manifest_hash,
            "generationSequence": self.generation_sequence,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LosslessRestoreReport {
    pub(crate) revision: i64,
    pub(crate) source_bytes: u64,
    pub(crate) source_sha256: String,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    pub(crate) backup_bytes: u64,
    pub(crate) warnings: Vec<LosslessWarning>,
}

pub(crate) fn project_payload_aliases(
    entries: &[StagedLosslessEntry],
) -> Result<Vec<AssetAlias>, LosslessError> {
    entries
        .iter()
        .filter(|entry| matches!(entry.kind, PayloadKind::Asset | PayloadKind::Inlay))
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
            let alias_metadata = match metadata.get(ALIAS_METADATA_FIELD) {
                Some(value) if value.is_object() => value.clone(),
                Some(_) => {
                    return Err(invalid_manifest(
                        "lossless package preserved alias metadata must be an object",
                    ));
                }
                None => entry.metadata.clone(),
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
                metadata: alias_metadata,
            })
        })
        .collect()
}

pub(crate) fn project_cold_aliases(
    entries: &[StagedLosslessEntry],
) -> Result<Vec<ColdAlias>, LosslessError> {
    entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::Cold)
        .map(|entry| {
            if !entry.metadata.is_object() {
                return Err(invalid_manifest(
                    "lossless package cold metadata must be a JSON object",
                ));
            }
            let size = i64::try_from(entry.byte_length).map_err(|_| {
                invalid_manifest("lossless package cold payload exceeds the alias size range")
            })?;
            Ok(ColdAlias {
                key: entry.logical_key.clone().ok_or_else(|| {
                    invalid_manifest("lossless package cold entry is missing its logical key")
                })?,
                object_hash: entry
                    .immutable_object
                    .as_ref()
                    .map(|payload| payload.content_hash.clone())
                    .or_else(|| Some(entry.sha256.clone())),
                size,
                metadata: entry.metadata.clone(),
            })
        })
        .collect()
}

pub(crate) fn project_asset_owner_heads(
    entries: &[StagedLosslessEntry],
) -> Result<Vec<AssetOwnerHead>, LosslessError> {
    entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
        .map(parse_owner_head_entry)
        .collect()
}

fn asset_repository_authority_extension(
    manifest: &LosslessManifest,
) -> Result<Option<AssetRepositoryAuthorityState>, LosslessError> {
    let Some(value) = manifest.extensions.get("assetRepositoryAuthority") else {
        return Ok(None);
    };
    let authority: AssetRepositoryAuthorityState =
        serde_json::from_value(value.clone()).map_err(|error| {
            invalid_manifest(format!(
                "invalid lossless asset repository authority extension: {error}"
            ))
        })?;
    if serde_json::to_value(&authority).map_err(|error| invalid_manifest(error.to_string()))?
        != *value
    {
        return Err(invalid_manifest(
            "lossless asset repository authority extension has unsupported fields",
        ));
    }
    authority.validate().map_err(|error| {
        invalid_manifest(format!(
            "invalid lossless asset repository authority extension: {error}"
        ))
    })?;
    Ok(Some(authority))
}

fn cold_payload_authority_extension(
    manifest: &LosslessManifest,
) -> Result<Option<ColdPayloadAuthorityState>, LosslessError> {
    let Some(value) = manifest.extensions.get("coldPayloadAuthority") else {
        return Ok(None);
    };
    let authority: ColdPayloadAuthorityState =
        serde_json::from_value(value.clone()).map_err(|error| {
            invalid_manifest(format!(
                "invalid lossless cold payload authority extension: {error}"
            ))
        })?;
    if serde_json::to_value(&authority).map_err(|error| invalid_manifest(error.to_string()))?
        != *value
    {
        return Err(invalid_manifest(
            "lossless cold payload authority extension has unsupported fields",
        ));
    }
    authority.validate().map_err(|error| {
        invalid_manifest(format!(
            "invalid lossless cold payload authority extension: {error}"
        ))
    })?;
    Ok(Some(authority))
}

fn require_lossless_v2_authorities(
    manifest: &LosslessManifest,
) -> Result<(AssetRepositoryAuthorityState, ColdPayloadAuthorityState), LosslessError> {
    let asset = asset_repository_authority_extension(manifest)?.ok_or_else(|| {
        invalid_manifest("production lossless package requires asset repository authority")
    })?;
    if !matches!(asset, AssetRepositoryAuthorityState::V2 { .. }) {
        return Err(invalid_manifest(
            "production lossless package requires v2 asset repository authority",
        ));
    }
    let cold = cold_payload_authority_extension(manifest)?.ok_or_else(|| {
        invalid_manifest("production lossless package requires cold payload authority")
    })?;
    if !matches!(cold, ColdPayloadAuthorityState::V2 { .. }) {
        return Err(invalid_manifest(
            "production lossless package requires v2 cold payload authority",
        ));
    }
    Ok((asset, cold))
}

fn parse_owner_head_entry(entry: &StagedLosslessEntry) -> Result<AssetOwnerHead, LosslessError> {
    let manifest = LosslessManifestEntry {
        logical_path: entry.logical_path.clone(),
        logical_key: entry.logical_key.clone(),
        kind: entry.kind,
        byte_length: entry.byte_length,
        sha256: entry.sha256.clone(),
        metadata: entry.metadata.clone(),
    };
    let head = parse_owner_head_manifest_entry(&manifest)?;
    let staged = entry.immutable_object.as_ref().ok_or_else(|| {
        invalid_manifest("lossless owner manifest is not staged in immutable storage")
    })?;
    if staged.content_hash != entry.sha256 || staged.byte_size != entry.byte_length {
        return Err(LosslessError::new(
            LosslessErrorCode::HashMismatch,
            "lossless owner manifest differs from immutable staged object",
        ));
    }
    Ok(head)
}

fn parse_owner_head_manifest_entry(
    entry: &LosslessManifestEntry,
) -> Result<AssetOwnerHead, LosslessError> {
    let head: AssetOwnerHead = serde_json::from_value(entry.metadata.clone())
        .map_err(|error| invalid_manifest(format!("invalid lossless asset owner head: {error}")))?;
    if serde_json::to_value(&head).map_err(|error| invalid_manifest(error.to_string()))?
        != entry.metadata
    {
        return Err(invalid_manifest(
            "lossless asset owner head metadata has unsupported fields",
        ));
    }
    let logical_key = entry.logical_key.as_deref().ok_or_else(|| {
        invalid_manifest("lossless owner manifest entry is missing its logical key")
    })?;
    if logical_key != owner_head_logical_key(&head.owner) {
        return Err(invalid_manifest(
            "lossless owner manifest logical key differs from its owner",
        ));
    }
    match (head.present, head.manifest_hash.as_deref()) {
        (true, Some(hash)) if hash == entry.sha256 => {}
        (true, _) => {
            return Err(LosslessError::new(
                LosslessErrorCode::HashMismatch,
                "lossless owner manifest hash differs from its owner head",
            ))
        }
        (false, None)
            if entry.byte_length == 0 && entry.sha256 == hex::encode(Sha256::digest([])) => {}
        (false, _) => {
            return Err(invalid_manifest(
                "absent lossless asset owner head requires an empty marker entry",
            ))
        }
    }
    Ok(head)
}

fn owner_head_logical_key(owner: &AssetOwnerLocator) -> String {
    match owner {
        AssetOwnerLocator::CharacterAdditionalAssets { character_id } => format!(
            "character-additional-assets/{}",
            hex::encode(character_id.as_bytes())
        ),
        AssetOwnerLocator::RootModuleAssets { index } => {
            format!("root-module-assets/{index}")
        }
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => {
            format!("persona-embedded-module-assets/{index}")
        }
    }
}

struct LosslessRestoreControl<'a> {
    cancellation: &'a dyn CancellationProbe,
}

impl RestoreControl for LosslessRestoreControl<'_> {
    fn is_cancel_requested(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    fn start(&self, _phase: JobPhase) -> Result<(), String> {
        Ok(())
    }

    fn set_phase(&self, _phase: JobPhase) -> Result<(), String> {
        Ok(())
    }

    fn set_progress(&self, _progress: JobProgress) -> Result<(), String> {
        Ok(())
    }

    fn set_detail(&self, _detail: JobDetail) -> Result<(), String> {
        Ok(())
    }
}

struct DirectStagingSink<'a> {
    store: Mutex<&'a mut PersistentStore>,
}

impl DirectStagingSink<'_> {
    fn unavailable<T>() -> StoreResult<T> {
        Err(StoreError::Validation {
            message: "lossless database decoder cannot own replacement activation".to_owned(),
        })
    }
}

impl ReplacementSink for DirectStagingSink<'_> {
    fn begin(&self) -> StoreResult<StagingResult> {
        Self::unavailable()
    }

    fn put_root(&self, staging_id: &str, root: &Value) -> StoreResult<()> {
        self.store
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("lossless staging mutex poisoned: {error}"),
            })?
            .replace_put_root(staging_id, root)
    }

    fn put_presets(&self, staging_id: &str, presets: &[Value]) -> StoreResult<()> {
        self.store
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("lossless staging mutex poisoned: {error}"),
            })?
            .replace_put_presets(staging_id, presets)
    }

    fn add_characters(&self, staging_id: &str, characters: &[Value]) -> StoreResult<()> {
        self.store
            .lock()
            .map_err(|error| StoreError::Store {
                message: format!("lossless staging mutex poisoned: {error}"),
            })?
            .replace_add_characters(staging_id, characters)
    }

    fn commit(&self, _staging_id: &str, _expected_revision: i64) -> StoreResult<RevisionResult> {
        Self::unavailable()
    }

    fn abort(&self, _staging_id: &str) -> StoreResult<()> {
        Self::unavailable()
    }
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
    InvalidDatabase,
    MissingReference,
    UnexpectedReference,
    BackupIncomplete,
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
    read_lossless_package_v1_inner(reader, job_staging_root, cas, None, None, cancellation)
}

fn read_lossless_package_v1_durable(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    durable_job: &mut DurableCasJob,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessReadReport, LosslessError> {
    read_lossless_package_v1_inner(
        reader,
        job_staging_root,
        cas,
        Some(durable_job),
        job_owner_manifest_hashes,
        cancellation,
    )
}

fn read_lossless_package_v1_inner(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    mut durable_job: Option<&mut DurableCasJob>,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessReadReport, LosslessError> {
    check_cancelled(cancellation)?;
    let mut source = CountingReader::new(reader);
    let manifest = read_manifest(&mut source, cancellation)?;
    let durable_roles = match durable_job.as_ref() {
        Some(_) => {
            require_lossless_v2_authorities(&manifest)?;
            Some(durable_object_roles(&manifest, job_owner_manifest_hashes)?)
        }
        None => None,
    };
    let staging_directory = prepare_staging_directory(job_staging_root)?;
    let mut entries = Vec::with_capacity(manifest.entries.len());
    for entry in &manifest.entries {
        let staged = if entry.kind == PayloadKind::Database {
            stage_database_entry(&mut source, entry, &staging_directory, cancellation)?
        } else if let Some(job) = durable_job.as_deref_mut() {
            match durable_roles
                .as_ref()
                .and_then(|roles| roles.get(&entry.sha256))
            {
                Some(role) => {
                    stage_payload_entry_durable(&mut source, entry, cas, job, *role, cancellation)?
                }
                None => stage_payload_entry(&mut source, entry, cas, cancellation)?,
            }
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
    Ok(LosslessReadReport {
        manifest,
        entries,
        archive_bytes: source.bytes_read,
        archive_sha256: source.sha256(),
    })
}

fn durable_object_roles(
    manifest: &LosslessManifest,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
) -> Result<HashMap<String, CasObjectRole>, LosslessError> {
    let mut roles = HashMap::new();
    for entry in &manifest.entries {
        if entry.kind == PayloadKind::Database {
            continue;
        }
        let mut role = match entry.kind {
            PayloadKind::OwnerManifest => {
                if !parse_owner_head_manifest_entry(entry)?.present {
                    continue;
                }
                CasObjectRole::OwnerManifest
            }
            _ => CasObjectRole::DirectObject,
        };
        if job_owner_manifest_hashes.is_some_and(|hashes| hashes.contains(&entry.sha256)) {
            role = CasObjectRole::OwnerManifest;
        }
        roles
            .entry(entry.sha256.clone())
            .and_modify(|current| {
                if role == CasObjectRole::OwnerManifest {
                    *current = role;
                }
            })
            .or_insert(role);
    }
    Ok(roles)
}

fn present_owner_manifest_hashes(
    manifest: &LosslessManifest,
) -> Result<HashSet<String>, LosslessError> {
    manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
        .filter_map(|entry| match parse_owner_head_manifest_entry(entry) {
            Ok(head) if head.present => Some(Ok(entry.sha256.clone())),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
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
        archive_sha256: source.sha256(),
    })
}

pub(crate) fn verify_lossless_package_v1_for_production(
    reader: &mut impl Read,
    cancellation: &dyn CancellationProbe,
) -> Result<VerifiedLosslessBackup, LosslessError> {
    let verified = verify_lossless_package_v1(reader, cancellation)?;
    require_lossless_v2_authorities(&verified.manifest)?;
    Ok(verified)
}

pub(crate) fn restore_lossless_package_v1(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    pre_replacement_backup: &Path,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    restore_lossless_package_v1_inner(
        reader,
        job_staging_root,
        cas,
        store,
        expected_revision,
        pre_replacement_backup,
        None,
        None,
        None,
        None,
        None,
        None,
        cancellation,
    )
}

pub(crate) fn restore_lossless_package_v1_with_app_kv(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    pre_replacement_backup: &Path,
    app_kv_key: &str,
    app_kv_value: &Value,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    restore_lossless_package_v1_inner(
        reader,
        job_staging_root,
        cas,
        store,
        expected_revision,
        pre_replacement_backup,
        Some((app_kv_key, app_kv_value)),
        None,
        None,
        None,
        None,
        None,
        cancellation,
    )
}

pub(crate) fn restore_lossless_package_v1_controlled(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    pre_replacement_backup: &Path,
    before_activation: &dyn Fn() -> Result<(), String>,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    restore_lossless_package_v1_inner(
        reader,
        job_staging_root,
        cas,
        store,
        expected_revision,
        pre_replacement_backup,
        None,
        Some(before_activation),
        None,
        None,
        None,
        None,
        cancellation,
    )
}

pub(crate) fn restore_verified_lossless_package_v1_durable_controlled(
    reader: &mut impl Read,
    verified: &VerifiedLosslessBackup,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    pre_replacement_backup: &Path,
    app_kv: Option<(&str, &Value)>,
    durable_job: &mut DurableCasJob,
    sealed_at_ms: i64,
    before_activation: &dyn Fn() -> Result<(), String>,
    before_commit_attempt: &dyn Fn(),
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    require_lossless_v2_authorities(&verified.manifest)?;
    restore_lossless_package_v1_inner(
        reader,
        job_staging_root,
        cas,
        store,
        expected_revision,
        pre_replacement_backup,
        app_kv,
        Some(before_activation),
        Some(before_commit_attempt),
        Some(verified),
        Some(durable_job),
        Some(sealed_at_ms),
        cancellation,
    )
}

fn restore_lossless_package_v1_inner(
    reader: &mut impl Read,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    pre_replacement_backup: &Path,
    app_kv: Option<(&str, &Value)>,
    before_activation: Option<&dyn Fn() -> Result<(), String>>,
    before_commit_attempt: Option<&dyn Fn()>,
    expected_verified: Option<&VerifiedLosslessBackup>,
    mut durable_job: Option<&mut DurableCasJob>,
    sealed_at_ms: Option<i64>,
    cancellation: &dyn CancellationProbe,
) -> Result<LosslessRestoreReport, LosslessError> {
    let mut early_lease = None;
    let job_owner_manifest_hashes = if durable_job.is_some() {
        let lease = store
            .acquire_revision(expected_revision)
            .map_err(store_error)?
            .lease;
        let owner_heads = match store
            .list_asset_owner_heads(Some(&lease))
            .map_err(store_error)
        {
            Ok(owner_heads) => owner_heads,
            Err(error) => return Err(release_lease_after_error(store, &lease, error)),
        };
        if owner_heads.revision != expected_revision {
            return Err(release_lease_after_error(
                store,
                &lease,
                LosslessError::new(
                    LosslessErrorCode::RevisionConflict,
                    "pre-replacement owner inventory is not pinned to the expected revision",
                ),
            ));
        }
        let mut hashes = match present_owner_manifest_hashes(
            &expected_verified
                .expect("durable restore has a preverified manifest")
                .manifest,
        ) {
            Ok(hashes) => hashes,
            Err(error) => return Err(release_lease_after_error(store, &lease, error)),
        };
        hashes.extend(
            owner_heads
                .value
                .iter()
                .filter(|head| head.present)
                .filter_map(|head| head.manifest_hash.clone()),
        );
        early_lease = Some(lease);
        Some(hashes)
    } else {
        None
    };
    let incoming_result = (|| {
        let incoming = match durable_job.as_deref_mut() {
            Some(job) => read_lossless_package_v1_durable(
                reader,
                job_staging_root,
                cas,
                job,
                job_owner_manifest_hashes.as_ref(),
                cancellation,
            )?,
            None => read_lossless_package_v1(reader, job_staging_root, cas, cancellation)?,
        };
        if let Some(expected) = expected_verified {
            if incoming.manifest != expected.manifest
                || incoming.archive_bytes != expected.archive_bytes
                || incoming.archive_sha256 != expected.archive_sha256
            {
                return Err(LosslessError::new(
                    LosslessErrorCode::HashMismatch,
                    "lossless package changed after preverification",
                ));
            }
        }
        let authorities = if durable_job.is_some() {
            let (asset, cold) = require_lossless_v2_authorities(&incoming.manifest)?;
            (Some(asset), Some(cold))
        } else {
            (
                asset_repository_authority_extension(&incoming.manifest)?,
                cold_payload_authority_extension(&incoming.manifest)?,
            )
        };
        Ok((incoming, authorities))
    })();
    let (incoming, (asset_repository_authority, cold_payload_authority)) = match incoming_result {
        Ok(incoming) => incoming,
        Err(error) => {
            return match early_lease.as_deref() {
                Some(lease) => Err(release_lease_after_error(store, lease, error)),
                None => Err(error),
            }
        }
    };
    let lease = match early_lease {
        Some(lease) => lease,
        None => {
            store
                .acquire_revision(expected_revision)
                .map_err(store_error)?
                .lease
        }
    };
    let database = incoming
        .entries
        .iter()
        .find(|entry| entry.kind == PayloadKind::Database)
        .expect("validated lossless manifest has one database entry");
    let staging_id = match store.replace_begin().map_err(store_error) {
        Ok(staging) => staging.staging_id,
        Err(error) => {
            return match store.release_revision(&lease).map_err(store_error) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup_error(error, "revision lease release", cleanup)),
            }
        }
    };
    let prepared = (|| {
        stage_block_database(store, &staging_id, database, cancellation)?;
        let aliases = project_payload_aliases(&incoming.entries)?;
        let cold_aliases = project_cold_aliases(&incoming.entries)?;
        let owner_heads = project_asset_owner_heads(&incoming.entries)?;
        store
            .replace_put_asset_aliases(&staging_id, &aliases)
            .map_err(store_error)?;
        store
            .replace_put_cold_aliases(&staging_id, &cold_aliases)
            .map_err(store_error)?;
        store
            .replace_put_asset_owner_heads(&staging_id, &owner_heads)
            .map_err(store_error)?;
        if let Some(authority) = &asset_repository_authority {
            store
                .replace_put_asset_repository_authority(&staging_id, authority)
                .map_err(store_error)?;
        }
        if let Some(authority) = &cold_payload_authority {
            store
                .replace_put_cold_payload_authority(&staging_id, authority)
                .map_err(store_error)?;
        }
        check_cancelled(cancellation)?;
        let staged_database = store
            .materialize_staging(&staging_id)
            .map_err(store_error)?;
        let character_count = staged_database
            .get("characters")
            .and_then(Value::as_array)
            .map_or(0, |characters| characters.len() as u64);
        let preset_count = staged_database
            .get("botPresets")
            .and_then(Value::as_array)
            .map_or(0, |presets| presets.len() as u64);
        validate_staged_f0(&incoming.manifest, &incoming.entries, &staged_database, cas)?;
        check_cancelled(cancellation)?;
        let require_v2 = durable_job.is_some();
        let backup = create_and_verify_pre_replacement_backup(
            pre_replacement_backup,
            job_staging_root,
            cas,
            store,
            &lease,
            expected_revision,
            durable_job.as_deref_mut(),
            job_owner_manifest_hashes.as_ref(),
            require_v2,
            None,
            cancellation,
        )?;
        Ok((backup, character_count, preset_count))
    })();
    let released = store.release_revision(&lease).map_err(store_error);
    let (backup, character_count, preset_count) = match (prepared, released) {
        (Ok(prepared), Ok(())) => prepared,
        (Err(error), Ok(())) => return abort_restore(store, &staging_id, error),
        (Ok(_), Err(cleanup)) => return abort_restore(store, &staging_id, cleanup),
        (Err(error), Err(cleanup)) => {
            return abort_restore(
                store,
                &staging_id,
                cleanup_error(error, "revision lease release", cleanup),
            )
        }
    };
    check_cancelled(cancellation).or_else(|error| abort_restore(store, &staging_id, error))?;
    if let Some(job) = durable_job.as_deref_mut() {
        job.seal(
            store,
            sealed_at_ms.expect("durable restore has a seal timestamp"),
        )
        .map_err(LosslessError::io)
        .or_else(|error| abort_restore(store, &staging_id, error))?;
    }
    if let Some(before_activation) = before_activation {
        before_activation()
            .map_err(|message| {
                LosslessError::new(
                    if cancellation.is_cancelled() {
                        LosslessErrorCode::Cancelled
                    } else {
                        LosslessErrorCode::Store
                    },
                    message,
                )
            })
            .or_else(|error| abort_restore(store, &staging_id, error))?;
    }
    let prepared = match store
        .prepare_replace_commit(&staging_id, Some(expected_revision))
        .map_err(store_error)
    {
        Ok(prepared) => prepared,
        Err(error) => return abort_restore(store, &staging_id, error),
    };
    let authorized = match prepared.create_snapshot().map_err(store_error) {
        Ok(authorized) => authorized,
        Err(error) => return abort_restore(store, &staging_id, error),
    };
    check_cancelled(cancellation).or_else(|error| abort_restore(store, &staging_id, error))?;
    if let Some(before_commit_attempt) = before_commit_attempt {
        before_commit_attempt();
    }
    let committed = match app_kv {
        Some((key, value)) => store.finish_prepared_replace_with_app_kv(authorized, key, value),
        None => store.finish_prepared_replace(authorized),
    };
    let revision = match committed.map_err(store_error) {
        Ok(revision) => revision.revision,
        Err(error) => return abort_restore(store, &staging_id, error),
    };
    Ok(LosslessRestoreReport {
        revision,
        source_bytes: incoming.archive_bytes,
        source_sha256: incoming.archive_sha256,
        character_count,
        preset_count,
        backup_bytes: backup.archive_bytes,
        warnings: incoming.manifest.warnings,
    })
}

fn stage_block_database(
    store: &mut PersistentStore,
    staging_id: &str,
    database: &StagedLosslessEntry,
    cancellation: &dyn CancellationProbe,
) -> Result<(), LosslessError> {
    let path = database.staged_path.as_deref().ok_or_else(|| {
        LosslessError::new(
            LosslessErrorCode::InvalidDatabase,
            "lossless database entry has no owned staging file",
        )
    })?;
    let control = LosslessRestoreControl { cancellation };
    let sink = DirectStagingSink {
        store: Mutex::new(store),
    };
    block_restore::stage_block_risu_save(path, staging_id, &control, &sink)
        .map_err(block_database_error)
}

fn block_database_error(error: crate::native_file_jobs::NativeJobError) -> LosslessError {
    let code = if error.code == "cancelled" {
        LosslessErrorCode::Cancelled
    } else {
        LosslessErrorCode::InvalidDatabase
    };
    LosslessError::new(
        code,
        format!("invalid lossless database payload: {}", error.message),
    )
}

fn validate_staged_f0(
    manifest: &LosslessManifest,
    entries: &[StagedLosslessEntry],
    database: &Value,
    cas: &PayloadCas,
) -> Result<F0Validation, LosslessError> {
    validate_staged_owner_manifests(entries, database, cas)?;
    let payloads = staged_f0_payloads(entries, cas)?;
    let expected_missing = manifest
        .references
        .iter()
        .filter(|reference| reference.status == ReferenceStatus::ExpectedMissing)
        .map(|reference| F0ExpectedMissing {
            target_kind: reference.target_kind.clone(),
            target_key: reference.target_key.clone(),
        })
        .collect::<Vec<_>>();
    let validation = validate_f0_v1(database, &payloads, &expected_missing).map_err(f0_error)?;
    if validation.canonical_database_sha256 != manifest.compatibility.canonical_database_sha256
        && legacy_canonical_database_sha256_v1(database).map_err(f0_error)?
            != manifest.compatibility.canonical_database_sha256
    {
        return Err(LosslessError::new(
            LosslessErrorCode::HashMismatch,
            "lossless package canonical database hash differs from decoded database",
        ));
    }
    if validation.reference_graph_sha256 != manifest.compatibility.reference_graph_sha256
        || validation
            .references
            .iter()
            .map(lossless_reference)
            .collect::<Vec<_>>()
            != manifest.references
    {
        return Err(LosslessError::new(
            LosslessErrorCode::UnexpectedReference,
            "lossless package ordered reference graph differs from decoded database and payload manifest",
        ));
    }
    Ok(validation)
}

fn validate_staged_owner_manifests(
    entries: &[StagedLosslessEntry],
    database: &Value,
    cas: &PayloadCas,
) -> Result<(), LosslessError> {
    let carried_hashes = entries
        .iter()
        .filter(|entry| {
            !matches!(
                entry.kind,
                PayloadKind::Database | PayloadKind::OwnerPayload
            )
        })
        .map(|entry| entry.sha256.as_str())
        .collect::<HashSet<_>>();
    let owner_payloads = entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerPayload)
        .map(|entry| entry.sha256.as_str())
        .collect::<HashSet<_>>();
    if owner_payloads
        .iter()
        .any(|hash| carried_hashes.contains(hash))
    {
        return Err(invalid_manifest(
            "lossless owner payload duplicates another package entry",
        ));
    }
    let mut used_owner_payloads = HashSet::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
    {
        let head = parse_owner_head_entry(entry)?;
        if !head.present {
            continue;
        }
        let hash = head
            .manifest_hash
            .as_deref()
            .expect("validated present owner head has a manifest hash");
        let decoded = read_bounded_owner_manifest(
            cas,
            hash,
            LosslessErrorCode::InvalidManifest,
            "lossless owner manifest",
        )?;
        let parent = materialized_asset_owner_entries(database, &head.owner)
            .map_err(store_error)?
            .ok_or_else(|| {
                invalid_manifest("present lossless owner head requires a staged parent property")
            })?;
        let allow_trailing_fields = matches!(
            &head.owner,
            AssetOwnerLocator::RootModuleAssets { .. }
                | AssetOwnerLocator::PersonaEmbeddedModuleAssets { .. }
        );
        if decoded.len() != head.entry_count as usize
            || !owner_tuples_match(parent, &decoded, allow_trailing_fields)
        {
            return Err(invalid_manifest(
                "lossless owner manifest tuples differ from the staged parent",
            ));
        }
        for owner_entry in decoded {
            let Some(payload_hash) = owner_entry.payload_hash else {
                continue;
            };
            let payload_hash = hex::encode(payload_hash);
            if carried_hashes.contains(payload_hash.as_str()) {
                continue;
            }
            if owner_payloads.contains(payload_hash.as_str()) {
                used_owner_payloads.insert(payload_hash);
                continue;
            }
            return Err(LosslessError::new(
                LosslessErrorCode::MissingReference,
                "lossless owner manifest historical payload is missing from the package",
            ));
        }
    }
    if used_owner_payloads.len() != owner_payloads.len() {
        return Err(invalid_manifest(
            "lossless package contains an unreferenced owner payload",
        ));
    }
    Ok(())
}

fn read_bounded_owner_manifest(
    cas: &PayloadCas,
    hash: &str,
    code: LosslessErrorCode,
    context: &str,
) -> Result<Vec<OwnerManifestEntry>, LosslessError> {
    let file = cas
        .open_object(hash)
        .map_err(|error| LosslessError::new(code, format!("{context} cannot be opened: {error}")))?
        .ok_or_else(|| LosslessError::new(code, format!("{context} is missing")))?;
    let length = file
        .metadata()
        .map_err(|error| LosslessError::new(code, format!("{context} cannot be read: {error}")))?
        .len();
    if length > MAX_OWNER_MANIFEST_BYTES {
        return Err(LosslessError::new(
            code,
            format!("{context} exceeds the decode limit"),
        ));
    }
    let mut canonical = Vec::with_capacity(length as usize);
    file.take(MAX_OWNER_MANIFEST_BYTES + 1)
        .read_to_end(&mut canonical)
        .map_err(|error| LosslessError::new(code, format!("{context} cannot be read: {error}")))?;
    if canonical.len() as u64 != length || canonical.len() as u64 > MAX_OWNER_MANIFEST_BYTES {
        return Err(LosslessError::new(
            code,
            format!("{context} changed while being read"),
        ));
    }
    if hex::encode(Sha256::digest(&canonical)) != hash {
        return Err(LosslessError::new(
            code,
            format!("{context} content hash differs from its identity"),
        ));
    }
    decode_owner_manifest(&canonical)
        .map_err(|error| LosslessError::new(code, format!("invalid {context}: {error}")))
}

fn owner_tuples_match(
    parent: &[Value],
    decoded: &[OwnerManifestEntry],
    allow_trailing_fields: bool,
) -> bool {
    parent.len() == decoded.len()
        && parent.iter().zip(decoded).all(|(value, entry)| {
            value.as_array().is_some_and(|tuple| {
                (if allow_trailing_fields {
                    tuple.len() >= 3
                } else {
                    tuple.len() == 3
                }) && tuple
                    .iter()
                    .take(3)
                    .zip(&entry.tuple)
                    .all(|(value, expected)| value.as_str() == Some(expected))
            })
        })
}

fn staged_f0_payloads(
    entries: &[StagedLosslessEntry],
    cas: &PayloadCas,
) -> Result<Vec<F0PayloadDescriptor>, LosslessError> {
    entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                PayloadKind::Asset | PayloadKind::Inlay | PayloadKind::Cold
            )
        })
        .map(|entry| {
            let prepared = entry.immutable_object.as_ref().ok_or_else(|| {
                LosslessError::new(
                    LosslessErrorCode::InvalidManifest,
                    "lossless payload is not staged in immutable storage",
                )
            })?;
            if prepared.content_hash != entry.sha256 || prepared.byte_size != entry.byte_length {
                return Err(LosslessError::new(
                    LosslessErrorCode::HashMismatch,
                    "lossless payload manifest differs from immutable staged object",
                ));
            }
            let path = cas
                .object_path(&entry.sha256)
                .map_err(LosslessError::io)?
                .ok_or_else(|| {
                    LosslessError::new(
                        LosslessErrorCode::MissingReference,
                        "lossless payload object is missing after staging",
                    )
                })?;
            let actual_length = fs::metadata(&path).map_err(LosslessError::io)?.len();
            if actual_length != entry.byte_length {
                return Err(LosslessError::new(
                    LosslessErrorCode::LengthMismatch,
                    "lossless payload object length differs from its manifest",
                ));
            }
            Ok(F0PayloadDescriptor {
                kind: f0_payload_kind(entry.kind)?,
                key: entry.logical_key.clone().ok_or_else(|| {
                    invalid_manifest("lossless payload entry is missing its logical key")
                })?,
                sha256: entry.sha256.clone(),
                byte_length: entry.byte_length,
                metadata: entry.metadata.clone(),
                cold_source: (entry.kind == PayloadKind::Cold).then_some(path),
            })
        })
        .collect()
}

fn f0_payload_kind(kind: PayloadKind) -> Result<F0PayloadKind, LosslessError> {
    match kind {
        PayloadKind::Asset => Ok(F0PayloadKind::Asset),
        PayloadKind::Inlay => Ok(F0PayloadKind::Inlay),
        PayloadKind::Cold => Ok(F0PayloadKind::Cold),
        PayloadKind::Database | PayloadKind::OwnerManifest | PayloadKind::OwnerPayload => {
            Err(invalid_manifest("entry cannot be used as an F0 payload"))
        }
    }
}

fn lossless_reference(reference: &F0Reference) -> LosslessReference {
    LosslessReference {
        owner_kind: reference.owner_kind.clone(),
        owner_id: reference.owner_id.clone(),
        source_path: reference.source_path.clone(),
        occurrence: reference.occurrence,
        target_kind: reference.target_kind.clone(),
        target_key: reference.target_key.clone(),
        status: match reference.status {
            F0ReferenceStatus::Present => ReferenceStatus::Present,
            F0ReferenceStatus::ExpectedMissing => ReferenceStatus::ExpectedMissing,
            F0ReferenceStatus::UnexpectedMissing => ReferenceStatus::UnexpectedMissing,
            F0ReferenceStatus::External => ReferenceStatus::External,
            F0ReferenceStatus::Invalid => ReferenceStatus::Invalid,
        },
        metadata: reference.metadata.clone(),
    }
}

fn f0_error(error: F0Error) -> LosslessError {
    let code = match error.code {
        F0ErrorCode::UnexpectedMissing => LosslessErrorCode::UnexpectedReference,
        F0ErrorCode::InvalidDatabase | F0ErrorCode::CanonicalValue => {
            LosslessErrorCode::InvalidDatabase
        }
        F0ErrorCode::InvalidInventory | F0ErrorCode::ColdPayload => {
            LosslessErrorCode::InvalidManifest
        }
    };
    LosslessError::new(code, error.message)
}

fn abort_restore<T>(
    store: &mut PersistentStore,
    staging_id: &str,
    error: LosslessError,
) -> Result<T, LosslessError> {
    match store.replace_abort(staging_id).map_err(store_error) {
        Ok(()) => Err(error),
        Err(abort) => Err(cleanup_error(error, "staging abort", abort)),
    }
}

fn cleanup_error(primary: LosslessError, action: &str, cleanup: LosslessError) -> LosslessError {
    LosslessError::new(
        cleanup.code,
        format!("{}; {action} failed: {}", primary.message, cleanup.message),
    )
}

fn release_lease_after_error(
    store: &mut PersistentStore,
    lease: &str,
    error: LosslessError,
) -> LosslessError {
    match store.release_revision(lease).map_err(store_error) {
        Ok(()) => error,
        Err(cleanup) => cleanup_error(error, "revision lease release", cleanup),
    }
}

fn create_and_verify_pre_replacement_backup(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    lease: &str,
    expected_revision: i64,
    mut durable_job: Option<&mut DurableCasJob>,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
    require_v2: bool,
    peer_source: Option<&LosslessPeerSourceBinding>,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    check_cancelled(cancellation)?;
    let database = store.materialize_lease(lease).map_err(store_error)?;
    let assets = store.list_asset_aliases(Some(lease)).map_err(store_error)?;
    let owner_heads = store
        .list_asset_owner_heads(Some(lease))
        .map_err(store_error)?;
    let cold = store.list_cold_aliases(Some(lease)).map_err(store_error)?;
    let asset_repository_authority = store
        .read_asset_repository_authority(Some(lease))
        .map_err(store_error)?;
    let cold_payload_authority = store
        .read_cold_payload_authority(Some(lease))
        .map_err(store_error)?;
    if assets.revision != expected_revision
        || owner_heads.revision != expected_revision
        || cold.revision != expected_revision
        || asset_repository_authority.revision != expected_revision
        || cold_payload_authority.revision != expected_revision
    {
        return Err(LosslessError::new(
            LosslessErrorCode::RevisionConflict,
            "pre-replacement inventory is not pinned to the expected revision",
        ));
    }
    if require_v2
        && (!matches!(
            &asset_repository_authority.value,
            AssetRepositoryAuthorityState::V2 { .. }
        ) || !matches!(
            &cold_payload_authority.value,
            ColdPayloadAuthorityState::V2 { .. }
        ))
    {
        return Err(invalid_manifest(
            "production lossless backup requires v2 asset and cold authority",
        ));
    }
    let exported = store.export_risu_save(lease, false).map_err(store_error)?;
    let export_path = PathBuf::from(&exported.path);
    let outcome = (|| {
        let absent_owner_marker = create_empty_job_file(job_staging_root)?;
        let (entries, payloads) = pinned_backup_entries(
            &export_path,
            &assets.value,
            &owner_heads.value,
            &cold.value,
            absent_owner_marker.as_ref(),
            cas,
            durable_job.as_deref_mut(),
            job_owner_manifest_hashes,
            cancellation,
        )?;
        let diagnostic = rebuild_f0_v1(&database, &payloads, &[]).map_err(f0_error)?;
        if diagnostic
            .references
            .iter()
            .any(|reference| reference.status == F0ReferenceStatus::UnexpectedMissing)
        {
            return Err(LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                "pre-replacement payload absence is not proved across legacy and alias stores",
            ));
        }
        let validation = validate_f0_v1(&database, &payloads, &[]).map_err(f0_error)?;
        let compatibility = LosslessCompatibility {
            oracle_version: FORMAT_VERSION,
            canonical_database_sha256: validation.canonical_database_sha256.clone(),
            reference_graph_sha256: validation.reference_graph_sha256.clone(),
        };
        let references = validation
            .references
            .iter()
            .map(lossless_reference)
            .collect::<Vec<_>>();
        let mut extensions = serde_json::json!({
            "sourceRevision": expected_revision,
            "assetRepositoryAuthority": asset_repository_authority.value,
            "coldPayloadAuthority": cold_payload_authority.value,
        });
        if let Some(peer_source) = peer_source {
            extensions
                .as_object_mut()
                .expect("lossless extensions are an object")
                .insert(
                    "peerBidirectionalSource".to_owned(),
                    peer_source.extension(),
                );
        }
        let mut output_guard = IncompleteBackupFile::create(output_path)?;
        let written = write_lossless_package_v1(
            output_guard.file_mut(),
            &entries,
            compatibility,
            references,
            Vec::new(),
            extensions,
            cancellation,
        )?;
        output_guard.sync()?;
        validate_payload_manifest(&written.manifest, &payloads)?;
        validate_owner_head_manifest(&written.manifest, &owner_heads.value)?;
        let verified = verify_pre_replacement_backup(
            output_path,
            &written.manifest,
            &database,
            job_staging_root,
            cas,
            store,
            durable_job.as_deref_mut(),
            job_owner_manifest_hashes,
            cancellation,
        )?;
        if verified.archive_bytes != written.archive_bytes {
            return Err(LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                "pre-replacement lossless backup byte count changed during verification",
            ));
        }
        output_guard.keep();
        Ok(CreatedLosslessBackup {
            archive_bytes: verified.archive_bytes,
            archive_sha256: verified.archive_sha256,
            character_count: exported.character_count,
            preset_count: exported.preset_count,
        })
    })();
    let cleanup = store
        .cleanup_risu_save_export(&export_path)
        .map_err(store_error);
    match (outcome, cleanup) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(cleanup_error(
            error,
            "pinned database export cleanup",
            cleanup,
        )),
    }
}

pub(crate) fn create_and_verify_lossless_backup_v1(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    cancellation: &dyn CancellationProbe,
) -> Result<u64, LosslessError> {
    create_and_verify_lossless_backup_v1_report(
        output_path,
        job_staging_root,
        cas,
        store,
        expected_revision,
        cancellation,
    )
    .map(|report| report.archive_bytes)
}

pub(crate) fn create_and_verify_lossless_backup_v1_report(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    let lease = store
        .acquire_revision(expected_revision)
        .map_err(store_error)?
        .lease;
    let exported = create_and_verify_pre_replacement_backup(
        output_path,
        job_staging_root,
        cas,
        store,
        &lease,
        expected_revision,
        None,
        None,
        false,
        None,
        cancellation,
    );
    let released = store.release_revision(&lease).map_err(store_error);
    match (exported, released) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(cleanup_error(error, "revision lease release", cleanup)),
    }
}

pub(crate) fn create_and_verify_peer_bidirectional_backup_v1_report(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    peer_source: &LosslessPeerSourceBinding,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    let lease = store
        .acquire_revision(expected_revision)
        .map_err(store_error)?
        .lease;
    let exported = create_and_verify_pre_replacement_backup(
        output_path,
        job_staging_root,
        cas,
        store,
        &lease,
        expected_revision,
        None,
        None,
        true,
        Some(peer_source),
        cancellation,
    );
    let released = store.release_revision(&lease).map_err(store_error);
    match (exported, released) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(cleanup_error(error, "revision lease release", cleanup)),
    }
}

pub(crate) fn create_and_verify_lossless_backup_v1_durable_report(
    output_path: &Path,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    expected_revision: i64,
    durable_job: &mut DurableCasJob,
    sealed_at_ms: i64,
    cancellation: &dyn CancellationProbe,
) -> Result<CreatedLosslessBackup, LosslessError> {
    let lease = store
        .acquire_revision(expected_revision)
        .map_err(store_error)?
        .lease;
    let exported = create_and_verify_pre_replacement_backup(
        output_path,
        job_staging_root,
        cas,
        store,
        &lease,
        expected_revision,
        Some(durable_job),
        None,
        true,
        None,
        cancellation,
    );
    let released = store.release_revision(&lease).map_err(store_error);
    let report = match (exported, released) {
        (Ok(report), Ok(())) => report,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(cleanup)) => {
            let _ = fs::remove_file(output_path);
            return Err(cleanup);
        }
        (Err(error), Err(cleanup)) => {
            return Err(cleanup_error(error, "revision lease release", cleanup));
        }
    };
    if let Err(error) = durable_job
        .seal(store, sealed_at_ms)
        .map_err(LosslessError::io)
    {
        let _ = fs::remove_file(output_path);
        return Err(error);
    }
    Ok(report)
}

fn pinned_backup_entries(
    database_path: &Path,
    assets: &[AssetAlias],
    owner_heads: &[AssetOwnerHead],
    cold: &[ColdAlias],
    absent_owner_marker: &Path,
    cas: &PayloadCas,
    mut durable_job: Option<&mut DurableCasJob>,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
    cancellation: &dyn CancellationProbe,
) -> Result<(Vec<LosslessWriteEntry>, Vec<F0PayloadDescriptor>), LosslessError> {
    let mut entries = Vec::with_capacity(1 + assets.len() + owner_heads.len() + cold.len());
    let mut payloads = Vec::with_capacity(assets.len() + cold.len());
    let mut packaged_hashes = HashSet::new();
    let mut durable_pins = BTreeMap::new();
    entries.push(LosslessWriteEntry {
        logical_path: DATABASE_PATH.to_owned(),
        logical_key: None,
        kind: PayloadKind::Database,
        metadata: Value::Object(serde_json::Map::new()),
        source: database_path.to_path_buf(),
    });
    for alias in assets {
        check_cancelled(cancellation)?;
        let kind = match alias.kind.as_str() {
            "asset" => PayloadKind::Asset,
            "inlay" => PayloadKind::Inlay,
            _ => {
                return Err(LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    "pre-replacement asset inventory contains an unsupported kind",
                ))
            }
        };
        let hash = required_backup_object_hash(alias.object_hash.as_deref(), &alias.key)?;
        packaged_hashes.insert(hash.to_owned());
        let (source, byte_length) = pinned_object(cas, hash, alias.size, &alias.key)?;
        remember_durable_pin(
            &mut durable_pins,
            hash,
            byte_length,
            effective_durable_role(hash, CasObjectRole::DirectObject, job_owner_manifest_hashes),
        )?;
        let metadata = lossless_asset_metadata(alias)?;
        entries.push(LosslessWriteEntry {
            logical_path: backup_logical_path(kind, &alias.key),
            logical_key: Some(alias.key.clone()),
            kind,
            metadata: metadata.clone(),
            source: source.clone(),
        });
        payloads.push(F0PayloadDescriptor {
            kind: f0_payload_kind(kind)?,
            key: alias.key.clone(),
            sha256: hash.to_owned(),
            byte_length,
            metadata,
            cold_source: None,
        });
    }
    for alias in cold {
        check_cancelled(cancellation)?;
        let hash = required_backup_object_hash(alias.object_hash.as_deref(), &alias.key)?;
        packaged_hashes.insert(hash.to_owned());
        let (source, byte_length) = pinned_object(cas, hash, alias.size, &alias.key)?;
        remember_durable_pin(
            &mut durable_pins,
            hash,
            byte_length,
            effective_durable_role(hash, CasObjectRole::DirectObject, job_owner_manifest_hashes),
        )?;
        entries.push(LosslessWriteEntry {
            logical_path: backup_logical_path(PayloadKind::Cold, &alias.key),
            logical_key: Some(alias.key.clone()),
            kind: PayloadKind::Cold,
            metadata: alias.metadata.clone(),
            source: source.clone(),
        });
        payloads.push(F0PayloadDescriptor {
            kind: F0PayloadKind::Cold,
            key: alias.key.clone(),
            sha256: hash.to_owned(),
            byte_length,
            metadata: alias.metadata.clone(),
            cold_source: Some(source),
        });
    }
    packaged_hashes.extend(
        owner_heads
            .iter()
            .filter(|head| head.present)
            .filter_map(|head| head.manifest_hash.clone()),
    );
    if owner_heads.iter().any(|head| !head.present) {
        packaged_hashes.insert(hex::encode(Sha256::digest([])));
    }
    for head in owner_heads {
        check_cancelled(cancellation)?;
        let logical_key = owner_head_logical_key(&head.owner);
        let (source, decoded) = match (head.present, head.manifest_hash.as_deref()) {
            (true, Some(hash)) => {
                let source = cas
                    .object_path(hash)
                    .map_err(LosslessError::io)?
                    .ok_or_else(|| {
                        LosslessError::new(
                            LosslessErrorCode::BackupIncomplete,
                            format!(
                                "pre-replacement owner manifest object is missing: {logical_key}"
                            ),
                        )
                    })?;
                let decoded = read_bounded_owner_manifest(
                    cas,
                    hash,
                    LosslessErrorCode::BackupIncomplete,
                    "pre-replacement owner manifest",
                )?;
                if usize::try_from(head.entry_count).ok() != Some(decoded.len()) {
                    return Err(LosslessError::new(
                        LosslessErrorCode::BackupIncomplete,
                        format!(
                            "pre-replacement owner manifest count differs from its head: {logical_key}"
                        ),
                    ));
                }
                remember_durable_pin(
                    &mut durable_pins,
                    hash,
                    fs::metadata(&source).map_err(LosslessError::io)?.len(),
                    CasObjectRole::OwnerManifest,
                )?;
                (source, decoded)
            }
            (false, None) => (absent_owner_marker.to_path_buf(), Vec::new()),
            _ => {
                return Err(LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    format!("pre-replacement asset owner head is invalid: {logical_key}"),
                ))
            }
        };
        entries.push(LosslessWriteEntry {
            logical_path: backup_logical_path(PayloadKind::OwnerManifest, &logical_key),
            logical_key: Some(logical_key),
            kind: PayloadKind::OwnerManifest,
            metadata: serde_json::to_value(head).map_err(|error| {
                LosslessError::new(LosslessErrorCode::BackupIncomplete, error.to_string())
            })?,
            source,
        });
        for owner_entry in decoded {
            check_cancelled(cancellation)?;
            let Some(payload_hash) = owner_entry.payload_hash else {
                continue;
            };
            let payload_hash = hex::encode(payload_hash);
            if !packaged_hashes.insert(payload_hash.clone()) {
                continue;
            }
            if entries.len() >= MAX_ENTRIES {
                return Err(LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    "pre-replacement owner payload inventory exceeds the package entry limit",
                ));
            }
            let source = cas
                .object_path(&payload_hash)
                .map_err(LosslessError::io)?
                .ok_or_else(|| {
                    LosslessError::new(
                        LosslessErrorCode::BackupIncomplete,
                        format!(
                            "pre-replacement owner historical payload is missing: {payload_hash}"
                        ),
                    )
                })?;
            let byte_length = fs::metadata(&source).map_err(LosslessError::io)?.len();
            if byte_length > u32::MAX as u64 {
                return Err(LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    format!(
                        "pre-replacement owner historical payload exceeds the V1 limit: {payload_hash}"
                    ),
                ));
            }
            remember_durable_pin(
                &mut durable_pins,
                &payload_hash,
                byte_length,
                effective_durable_role(
                    &payload_hash,
                    CasObjectRole::DirectObject,
                    job_owner_manifest_hashes,
                ),
            )?;
            entries.push(LosslessWriteEntry {
                logical_path: backup_logical_path(PayloadKind::OwnerPayload, &payload_hash),
                logical_key: Some(payload_hash),
                kind: PayloadKind::OwnerPayload,
                metadata: Value::Object(serde_json::Map::new()),
                source,
            });
        }
    }
    if entries.len() > MAX_ENTRIES {
        return Err(LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            "pre-replacement inventory exceeds the package entry limit",
        ));
    }
    if let Some(job) = durable_job.as_deref_mut() {
        for (hash, (byte_size, role)) in durable_pins {
            job.pin_existing(cas, &hash, byte_size, role)
                .map_err(LosslessError::io)?;
        }
    }
    Ok((entries, payloads))
}

fn effective_durable_role(
    hash: &str,
    default_role: CasObjectRole,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
) -> CasObjectRole {
    if job_owner_manifest_hashes.is_some_and(|hashes| hashes.contains(hash)) {
        CasObjectRole::OwnerManifest
    } else {
        default_role
    }
}

fn remember_durable_pin(
    pins: &mut BTreeMap<String, (u64, CasObjectRole)>,
    hash: &str,
    byte_size: u64,
    role: CasObjectRole,
) -> Result<(), LosslessError> {
    match pins.get_mut(hash) {
        Some((existing_size, existing_role)) => {
            if *existing_size != byte_size {
                return Err(LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    "lossless CAS pin has conflicting byte sizes",
                ));
            }
            if role == CasObjectRole::OwnerManifest {
                *existing_role = role;
            }
        }
        None => {
            pins.insert(hash.to_owned(), (byte_size, role));
        }
    }
    Ok(())
}

fn lossless_asset_metadata(alias: &AssetAlias) -> Result<Value, LosslessError> {
    let mut metadata = alias.metadata.as_object().cloned().ok_or_else(|| {
        LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            format!(
                "pre-replacement payload metadata is not an object: {}",
                alias.key
            ),
        )
    })?;
    for field in [
        "name",
        "ext",
        "mime",
        "inlayType",
        "width",
        "height",
        ALIAS_METADATA_FIELD,
    ] {
        metadata.remove(field);
    }
    metadata.insert("name".to_owned(), Value::String(alias.name.clone()));
    metadata.insert("ext".to_owned(), Value::String(alias.ext.clone()));
    metadata.insert("mime".to_owned(), Value::String(alias.mime.clone()));
    if alias.kind == "inlay" {
        let inlay_type = alias.inlay_type.as_ref().ok_or_else(|| {
            LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                format!(
                    "pre-replacement Inlay has no declared Inlay type: {}",
                    alias.key
                ),
            )
        })?;
        metadata.insert("inlayType".to_owned(), Value::String(inlay_type.clone()));
        if let Some(width) = alias.width {
            metadata.insert("width".to_owned(), Value::from(width));
        }
        if let Some(height) = alias.height {
            metadata.insert("height".to_owned(), Value::from(height));
        }
    }
    metadata.insert(ALIAS_METADATA_FIELD.to_owned(), alias.metadata.clone());
    Ok(Value::Object(metadata))
}

fn required_backup_object_hash<'a>(
    hash: Option<&'a str>,
    logical_key: &str,
) -> Result<&'a str, LosslessError> {
    hash.ok_or_else(|| {
        LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            format!("pre-replacement payload has no immutable object: {logical_key}"),
        )
    })
}

fn pinned_object(
    cas: &PayloadCas,
    hash: &str,
    declared_size: i64,
    logical_key: &str,
) -> Result<(PathBuf, u64), LosslessError> {
    let declared_size = u64::try_from(declared_size).map_err(|_| {
        LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            format!("pre-replacement payload has a negative size: {logical_key}"),
        )
    })?;
    let path = cas
        .object_path(hash)
        .map_err(LosslessError::io)?
        .ok_or_else(|| {
            LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                format!("pre-replacement payload object is missing: {logical_key}"),
            )
        })?;
    let actual_size = fs::metadata(&path).map_err(LosslessError::io)?.len();
    if actual_size != declared_size {
        return Err(LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            format!("pre-replacement payload size differs from its alias: {logical_key}"),
        ));
    }
    Ok((path, actual_size))
}

fn backup_logical_path(kind: PayloadKind, logical_key: &str) -> String {
    if kind == PayloadKind::OwnerPayload {
        return format!("owner-payloads/{logical_key}");
    }
    let namespace = match kind {
        PayloadKind::Database => "database",
        PayloadKind::Asset => "assets",
        PayloadKind::Inlay => "inlays",
        PayloadKind::Cold => "cold",
        PayloadKind::OwnerManifest => "owner-manifests",
        PayloadKind::OwnerPayload => unreachable!("handled above"),
    };
    format!("{namespace}/{}", hex::encode(logical_key.as_bytes()))
}

fn validate_payload_manifest(
    manifest: &LosslessManifest,
    payloads: &[F0PayloadDescriptor],
) -> Result<(), LosslessError> {
    let package_payloads = manifest
        .entries
        .iter()
        .filter_map(|entry| {
            matches!(
                entry.kind,
                PayloadKind::Asset | PayloadKind::Inlay | PayloadKind::Cold
            )
            .then(|| {
                (
                    (
                        entry.kind,
                        entry.logical_key.as_deref().expect("validated payload key"),
                    ),
                    entry,
                )
            })
        })
        .collect::<HashMap<_, _>>();
    if package_payloads.len() != payloads.len() {
        return Err(LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            "pre-replacement payload inventory cardinality differs from its package",
        ));
    }
    for payload in payloads {
        let kind = match payload.kind {
            F0PayloadKind::Asset => PayloadKind::Asset,
            F0PayloadKind::Inlay => PayloadKind::Inlay,
            F0PayloadKind::Cold => PayloadKind::Cold,
        };
        let entry = package_payloads
            .get(&(kind, payload.key.as_str()))
            .ok_or_else(|| {
                LosslessError::new(
                    LosslessErrorCode::BackupIncomplete,
                    format!(
                        "pre-replacement package omitted payload inventory entry: {}",
                        payload.key
                    ),
                )
            })?;
        if entry.sha256 != payload.sha256
            || entry.byte_length != payload.byte_length
            || entry.metadata != payload.metadata
        {
            return Err(LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                format!(
                    "pre-replacement package payload differs from pinned inventory: {}",
                    payload.key
                ),
            ));
        }
    }
    Ok(())
}

fn verify_pre_replacement_backup(
    backup_path: &Path,
    expected_manifest: &LosslessManifest,
    expected_database: &Value,
    job_staging_root: &Path,
    cas: &PayloadCas,
    store: &mut PersistentStore,
    durable_job: Option<&mut DurableCasJob>,
    job_owner_manifest_hashes: Option<&HashSet<String>>,
    cancellation: &dyn CancellationProbe,
) -> Result<VerifiedLosslessBackup, LosslessError> {
    let mut raw = File::open(backup_path).map_err(LosslessError::io)?;
    let verified = verify_lossless_package_v1(&mut raw, cancellation)?;
    if &verified.manifest != expected_manifest {
        return Err(LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            "pre-replacement package manifest changed during verification",
        ));
    }
    let expected_owner_heads = expected_manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
        .map(parse_owner_head_manifest_entry)
        .collect::<Result<Vec<_>, _>>()?;
    let verification_root = JobOwnedDirectory::create(job_staging_root)?;
    let mut raw = File::open(backup_path).map_err(LosslessError::io)?;
    let staged = match durable_job {
        Some(job) => read_lossless_package_v1_durable(
            &mut raw,
            verification_root.as_ref(),
            cas,
            job,
            job_owner_manifest_hashes,
            cancellation,
        )?,
        None => read_lossless_package_v1(&mut raw, verification_root.as_ref(), cas, cancellation)?,
    };
    let database_entry = staged
        .entries
        .iter()
        .find(|entry| entry.kind == PayloadKind::Database)
        .expect("verified backup has one database entry");
    let verification_staging = store.replace_begin().map_err(store_error)?.staging_id;
    let validation = (|| {
        stage_block_database(store, &verification_staging, database_entry, cancellation)?;
        let decoded = store
            .materialize_staging(&verification_staging)
            .map_err(store_error)?;
        if &decoded != expected_database {
            return Err(LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                "pre-replacement database does not decode to the pinned expected revision",
            ));
        }
        let owner_heads = project_asset_owner_heads(&staged.entries)?;
        if owner_heads != expected_owner_heads {
            return Err(LosslessError::new(
                LosslessErrorCode::BackupIncomplete,
                "pre-replacement asset owner heads do not match the pinned inventory",
            ));
        }
        store
            .replace_put_asset_owner_heads(&verification_staging, &owner_heads)
            .map_err(store_error)?;
        validate_staged_f0(&staged.manifest, &staged.entries, &decoded, cas)?;
        Ok(())
    })();
    let aborted = store
        .replace_abort(&verification_staging)
        .map_err(store_error);
    match (validation, aborted) {
        (Ok(()), Ok(())) => Ok(verified),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(error), Err(cleanup)) => Err(cleanup_error(
            error,
            "pre-replacement verification staging abort",
            cleanup,
        )),
    }
}

struct IncompleteBackupFile {
    file: Option<File>,
    path: PathBuf,
    keep: bool,
}

impl IncompleteBackupFile {
    fn create(path: &Path) -> Result<Self, LosslessError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(LosslessError::io)?;
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
            keep: false,
        })
    }

    fn file_mut(&mut self) -> &mut File {
        self.file.as_mut().expect("backup output remains open")
    }

    fn sync(&mut self) -> Result<(), LosslessError> {
        self.file_mut().sync_all().map_err(LosslessError::io)?;
        self.file.take();
        Ok(())
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for IncompleteBackupFile {
    fn drop(&mut self) {
        self.file.take();
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct JobOwnedDirectory {
    path: PathBuf,
}

impl JobOwnedDirectory {
    fn create(parent: &Path) -> Result<Self, LosslessError> {
        let parent = fs::canonicalize(parent).map_err(LosslessError::io)?;
        let path = parent.join(format!("lossless-verify-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).map_err(LosslessError::io)?;
        let metadata = fs::symlink_metadata(&path).map_err(LosslessError::io)?;
        if !metadata.is_dir() || is_link_like(&metadata) {
            return Err(LosslessError::new(
                LosslessErrorCode::InvalidPath,
                "lossless verification root is not an owned plain directory",
            ));
        }
        let path = fs::canonicalize(path).map_err(LosslessError::io)?;
        if path.parent() != Some(parent.as_path()) {
            return Err(LosslessError::new(
                LosslessErrorCode::InvalidPath,
                "lossless verification root escapes its job-owned parent",
            ));
        }
        Ok(Self { path })
    }
}

impl AsRef<Path> for JobOwnedDirectory {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for JobOwnedDirectory {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            if metadata.is_dir() && !is_link_like(&metadata) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
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
    asset_repository_authority_extension(&manifest)?;
    cold_payload_authority_extension(&manifest)?;
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
        if entry.kind == PayloadKind::OwnerManifest && entry.byte_length > MAX_OWNER_MANIFEST_BYTES
        {
            return Err(invalid_manifest(
                "lossless owner manifest decode limit exceeded",
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
            if !logical_keys.insert((entry.kind, logical_key.to_owned())) {
                return Err(LosslessError::new(
                    LosslessErrorCode::DuplicateLogicalKey,
                    format!("duplicate lossless package logical key: {logical_key}"),
                ));
            }
            validate_payload_metadata(entry)?;
            if entry.kind == PayloadKind::OwnerPayload
                && (logical_key != entry.sha256
                    || entry.logical_path
                        != backup_logical_path(PayloadKind::OwnerPayload, logical_key)
                    || !entry
                        .metadata
                        .as_object()
                        .is_some_and(|metadata| metadata.is_empty()))
            {
                return Err(invalid_manifest(
                    "lossless owner payload must use its SHA-256 identity and empty metadata",
                ));
            }
            if let Some(target_kind) = entry.kind.target_kind() {
                payload_targets.insert((target_kind, logical_key));
            }
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
    if matches!(entry.kind, PayloadKind::Asset | PayloadKind::Inlay) {
        for field in ["name", "ext", "mime"] {
            if !metadata.get(field).is_some_and(Value::is_string) {
                return Err(invalid_manifest(format!(
                    "lossless package payload metadata field {field} must be a string"
                )));
            }
        }
    }
    let inlay_type = metadata.get("inlayType").and_then(Value::as_str);
    if matches!(entry.kind, PayloadKind::Asset | PayloadKind::Inlay)
        && metadata
            .get(ALIAS_METADATA_FIELD)
            .is_some_and(|value| !value.is_object())
    {
        return Err(invalid_manifest(
            "lossless package preserved alias metadata must be an object",
        ));
    }
    if entry.kind == PayloadKind::Inlay {
        if !matches!(inlay_type, Some("image" | "audio" | "video" | "signature")) {
            return Err(invalid_manifest(
                "lossless package Inlay metadata requires a supported inlayType",
            ));
        }
    } else if entry.kind == PayloadKind::Asset && inlay_type.is_some() {
        return Err(invalid_manifest(
            "lossless package non-Inlay metadata cannot declare inlayType",
        ));
    }
    if entry.kind == PayloadKind::OwnerManifest {
        parse_owner_head_manifest_entry(entry)?;
    }
    Ok(())
}

fn validate_owner_head_manifest(
    manifest: &LosslessManifest,
    expected: &[AssetOwnerHead],
) -> Result<(), LosslessError> {
    let actual = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
        .map(parse_owner_head_manifest_entry)
        .collect::<Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err(LosslessError::new(
            LosslessErrorCode::BackupIncomplete,
            "pre-replacement asset owner-head inventory differs from its package",
        ));
    }
    Ok(())
}

fn stage_database_entry(
    source: &mut impl Read,
    entry: &LosslessManifestEntry,
    staging_directory: &Path,
    cancellation: &dyn CancellationProbe,
) -> Result<StagedLosslessEntry, LosslessError> {
    let path = staging_directory.join(format!("{}.database", uuid::Uuid::new_v4()));
    let owned = JobOwnedFile::new(path.clone());
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
    Ok(staged_entry(entry, Some(owned), None))
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

fn stage_payload_entry_durable(
    source: &mut impl Read,
    entry: &LosslessManifestEntry,
    cas: &PayloadCas,
    durable_job: &mut DurableCasJob,
    role: CasObjectRole,
    cancellation: &dyn CancellationProbe,
) -> Result<StagedLosslessEntry, LosslessError> {
    let mut declared = DeclaredReader {
        source,
        remaining: entry.byte_length,
        cancellation,
    };
    let prepared = durable_job
        .prepare_reader(cas, &mut declared, role)
        .map_err(|error| {
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
    staged_path: Option<JobOwnedFile>,
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
    hasher: Sha256,
}

impl<'a, R> CountingReader<'a, R> {
    fn new(inner: &'a mut R) -> Self {
        Self {
            inner,
            bytes_read: 0,
            hasher: Sha256::new(),
        }
    }

    fn sha256(&self) -> String {
        hex::encode(self.hasher.clone().finalize())
    }
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read = self
            .bytes_read
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "source size overflow"))?;
        self.hasher.update(&buffer[..read]);
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
    for candidate in fs::read_dir(&root).map_err(LosslessError::io)? {
        let candidate = candidate.map_err(LosslessError::io)?;
        let file_name = candidate.file_name();
        let Some(id) = file_name
            .to_str()
            .and_then(|name| name.strip_prefix("lossless-verify-"))
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(candidate.path()).map_err(LosslessError::io)?;
        if uuid::Uuid::parse_str(id).is_err() || !metadata.is_dir() || is_link_like(&metadata) {
            continue;
        }
        let path = fs::canonicalize(candidate.path()).map_err(LosslessError::io)?;
        if path.parent() == Some(root.as_path()) {
            fs::remove_dir_all(candidate.path()).map_err(LosslessError::io)?;
        }
    }
    let staging = root.join("lossless-v1");
    fs::create_dir_all(&staging).map_err(LosslessError::io)?;
    let staging_metadata = fs::symlink_metadata(&staging).map_err(LosslessError::io)?;
    if !staging_metadata.is_dir() || is_link_like(&staging_metadata) {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package staging directory is not an owned plain directory",
        ));
    }
    let staging = fs::canonicalize(staging).map_err(LosslessError::io)?;
    if staging.parent() != Some(root.as_path()) {
        return Err(LosslessError::new(
            LosslessErrorCode::InvalidPath,
            "lossless package staging directory escapes its owned root",
        ));
    }
    for candidate in fs::read_dir(&staging).map_err(LosslessError::io)? {
        let candidate = candidate.map_err(LosslessError::io)?;
        let file_name = candidate.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(id) = file_name
            .strip_suffix(".database")
            .or_else(|| file_name.strip_suffix(".owner-empty"))
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(candidate.path()).map_err(LosslessError::io)?;
        if uuid::Uuid::parse_str(id).is_ok() && metadata.is_file() && !is_link_like(&metadata) {
            fs::remove_file(candidate.path()).map_err(LosslessError::io)?;
        }
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
    if is_lower_hex_256(hash) {
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

#[derive(Debug)]
pub(crate) struct JobOwnedFile {
    path: PathBuf,
}

fn create_empty_job_file(root: &Path) -> Result<JobOwnedFile, LosslessError> {
    let staging = prepare_staging_directory(root)?;
    let path = staging.join(format!("{}.owner-empty", uuid::Uuid::new_v4()));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(LosslessError::io)?;
    file.sync_all().map_err(LosslessError::io)?;
    drop(file);
    Ok(JobOwnedFile::new(path))
}

impl JobOwnedFile {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl AsRef<Path> for JobOwnedFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for JobOwnedFile {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for JobOwnedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::{
        job_pins::{collect_durable_cas_job_roots, CasJobKind, CasReleaseOutcome, DurableCasJob},
        owner_manifest_codec::encode_owner_manifest,
    };
    use crate::local_backup::NeverCancelled;
    use crate::peer_sync::{
        prepare_lossless_clone_session, CloneActivation, CloneObjectKind, CloneTargetAdapter,
        LosslessCloneTargetAdapter, PeerSyncError, CLONE_LOSSLESS_DATABASE_FORMAT,
    };
    use crate::persistent_store::WorkingSetCommit;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde_json::json;
    use std::{
        collections::BTreeSet,
        fs,
        io::Cursor,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
    };

    struct CancelAfterBytes<R> {
        inner: R,
        bytes_read: usize,
        cancel_after: usize,
        cancelled: Arc<AtomicBool>,
    }

    impl<R: Read> Read for CancelAfterBytes<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.bytes_read += read;
            if self.bytes_read >= self.cancel_after {
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    struct CheckCountingCancellation {
        calls: AtomicUsize,
        cancel_at: Option<usize>,
    }

    impl CancellationProbe for CheckCountingCancellation {
        fn is_cancelled(&self) -> bool {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.cancel_at.is_some_and(|cancel_at| call >= cancel_at)
        }
    }

    struct CommitSameManifestOnFirstCheck {
        store_root: PathBuf,
        manifest_id: String,
        fired: AtomicBool,
    }

    impl CancellationProbe for CommitSameManifestOnFirstCheck {
        fn is_cancelled(&self) -> bool {
            if self.fired.swap(true, Ordering::SeqCst) {
                return false;
            }
            let mut concurrent = PersistentStore::open(&self.store_root).unwrap();
            let mut root = concurrent.read_root(None).unwrap().value;
            root["username"] = Value::String("New".to_owned());
            concurrent
                .commit(&WorkingSetCommit {
                    expected_revision: 1,
                    root: Some(root),
                    replace_presets: None,
                    character: None,
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: None,
                    delete_character_id: None,
                    plugin_storage: None,
                    asset_owner_heads: None,
                })
                .unwrap();
            concurrent
                .set_app_kv(
                    "peerCloneActiveManifest",
                    &json!({ "manifestId": self.manifest_id, "revision": 2 }),
                )
                .unwrap();
            false
        }
    }

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
    fn arbitrary_database_bytes_cannot_pass_with_manifest_constants() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = minimal_package(directory.path(), "new", b"new-db", b"new-asset");
        let backup_path = directory.path().join("pre-replacement.lossless");
        let cas = PayloadCas::new(repository).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            0,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidDatabase);
        assert_eq!(store.revision().unwrap(), 0);
        assert!(!backup_path.exists());
    }

    #[test]
    fn production_block_decoder_preserves_semantics_across_the_w0_wire_order_gap() {
        let encoded = STANDARD
            .decode(
                include_str!(
                    "../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-block-v4.input.base64"
                )
                .trim(),
            )
            .expect("decode frozen W0 block RisuSave");
        assert_eq!(encoded.len(), 743);
        assert_eq!(
            hex::encode(Sha256::digest(&encoded)),
            "6e179d45f5239d50562407e5b4808bab1b651c9425bde488137c5f5d538f36c1"
        );
        let directory = tempfile::tempdir().expect("create W0 production decode directory");
        let database_path = directory.path().join("w0.risudat");
        fs::write(&database_path, &encoded).expect("write W0 production artifact");
        let database = StagedLosslessEntry {
            logical_path: DATABASE_PATH.to_owned(),
            logical_key: None,
            kind: PayloadKind::Database,
            byte_length: encoded.len() as u64,
            sha256: hex::encode(Sha256::digest(&encoded)),
            metadata: json!({}),
            staged_path: Some(JobOwnedFile::new(database_path)),
            immutable_object: None,
        };
        let mut store = PersistentStore::open(directory.path()).expect("open W0 staging store");
        let staging_id = store
            .replace_begin()
            .expect("begin W0 production staging")
            .staging_id;

        stage_block_database(&mut store, &staging_id, &database, &NeverCancelled)
            .expect("decode W0 through the production block reader");
        let decoded = store
            .materialize_staging(&staging_id)
            .expect("materialize production-decoded W0");
        let expected: Value = serde_json::from_str(include_str!(
            "../../src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/risusave-block-v4.expected.json"
        ))
        .expect("parse frozen W0 expectation");

        let expected_missing = [
            F0ExpectedMissing {
                target_kind: "persona".to_owned(),
                target_key: "#undefined".to_owned(),
            },
            F0ExpectedMissing {
                target_kind: "asset".to_owned(),
                target_key: "fixture.png".to_owned(),
            },
            F0ExpectedMissing {
                target_kind: "conversation".to_owned(),
                target_key: "#undefined".to_owned(),
            },
        ];
        let source_validation = validate_f0_v1(&expected, &[], &expected_missing)
            .expect("validate the frozen W0 expectation through F0");
        assert_eq!(
            legacy_canonical_database_sha256_v1(&expected).unwrap(),
            "9a8f355262eba1a3163b51e9c12e22802765305299c4f60eb4834d0d1122eec6"
        );
        let validation = validate_f0_v1(&decoded, &[], &expected_missing)
            .expect("validate production-decoded W0 through F0");
        assert_ne!(
            decoded
                .as_object()
                .expect("production W0 database object")
                .keys()
                .collect::<Vec<_>>(),
            expected
                .as_object()
                .expect("frozen W0 database object")
                .keys()
                .collect::<Vec<_>>()
        );
        assert_eq!(
            legacy_canonical_database_sha256_v1(&decoded).unwrap(),
            "55195e4a86e010d50bb37c502802f2180c769d508f210f4dc4846a89935a80f7"
        );
        assert_eq!(
            validation.canonical_database_sha256,
            source_validation.canonical_database_sha256
        );
        assert_eq!(validation.references, source_validation.references);
        assert_eq!(
            validation
                .references
                .iter()
                .map(|reference| (
                    reference.target_kind.as_str(),
                    reference.target_key.as_str(),
                    reference.status,
                ))
                .collect::<Vec<_>>(),
            vec![
                ("preset", "Fixture preset", F0ReferenceStatus::Present),
                ("persona", "#undefined", F0ReferenceStatus::ExpectedMissing,),
                ("preset", "<undefined>", F0ReferenceStatus::Invalid),
                ("persona", "<undefined>", F0ReferenceStatus::Invalid),
                ("asset", "fixture.png", F0ReferenceStatus::ExpectedMissing,),
                (
                    "conversation",
                    "#undefined",
                    F0ReferenceStatus::ExpectedMissing,
                ),
            ]
        );
        store
            .replace_abort(&staging_id)
            .expect("abort verified W0 staging generation");
    }

    #[test]
    fn staged_f0_verification_accepts_a_legacy_iteration_order_database_hash() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("repository")).unwrap();
        let cas = PayloadCas::new(directory.path().join("repository")).unwrap();
        let database: Value = serde_json::from_str(
            r#"{"zeta":0,"characters":[],"botPresets":[{"name":"legacy-fixture-preset"}],"botPresetsId":0,"personas":[{"id":"legacy-fixture-persona"}],"selectedPersona":0,"alpha":{"zeta":false,"alpha":true}}"#,
        )
        .unwrap();
        let validation = validate_f0_v1(&database, &[], &[]).unwrap();
        let legacy_hash = legacy_canonical_database_sha256_v1(&database).unwrap();
        assert_ne!(legacy_hash, validation.canonical_database_sha256);
        let manifest = LosslessManifest {
            version: FORMAT_VERSION,
            compatibility: LosslessCompatibility {
                oracle_version: FORMAT_VERSION,
                canonical_database_sha256: legacy_hash,
                reference_graph_sha256: validation.reference_graph_sha256,
            },
            entries: Vec::new(),
            references: validation
                .references
                .iter()
                .map(lossless_reference)
                .collect(),
            warnings: Vec::new(),
            extensions: json!({}),
        };

        validate_staged_f0(&manifest, &[], &database, &cas).unwrap();
    }

    #[test]
    fn owner_manifest_must_be_canonical_and_match_staged_parent_before_activation() {
        let mismatched = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "different".to_owned(),
                "shared".to_owned(),
                "BIN".to_owned(),
            ],
            payload_hash: Some(Sha256::digest(b"new-asset").into()),
        }])
        .unwrap();
        for (owner_manifest, expected_code) in [
            (
                b"not-a-romf-manifest".to_vec(),
                LosslessErrorCode::InvalidManifest,
            ),
            (mismatched, LosslessErrorCode::InvalidManifest),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path().join("job-staging");
            let repository = directory.path().join("repository");
            fs::create_dir(&staging).unwrap();
            fs::create_dir(&repository).unwrap();
            let incoming = production_package_with_owner_manifest(
                directory.path(),
                "New",
                b"new",
                Some(owner_manifest),
            );
            let backup_path = directory.path().join("pre-replacement.lossless");
            let mut store = PersistentStore::open(&repository).unwrap();
            let cas = PayloadCas::new(&repository).unwrap();
            seed_active_store(&mut store, &cas, "Old", b"old");

            let error = restore_lossless_package_v1(
                &mut Cursor::new(incoming),
                &staging,
                &cas,
                &mut store,
                1,
                &backup_path,
                &NeverCancelled,
            )
            .unwrap_err();

            assert_eq!(error.code, expected_code);
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(store.materialize(None).unwrap()["username"], "Old");
            assert!(!backup_path.exists());
        }
    }

    #[test]
    fn owner_manifest_null_payload_hash_does_not_require_an_object() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let owner_manifest = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
            payload_hash: None,
        }])
        .unwrap();
        let incoming = production_package_with_owner_manifest(
            directory.path(),
            "New",
            b"new",
            Some(owner_manifest.clone()),
        );
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let report = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        let head = store
            .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            cas.read_object(head.value.manifest_hash.as_ref().unwrap())
                .unwrap()
                .unwrap(),
            owner_manifest
        );
    }

    #[test]
    fn owner_manifest_prefix_matching_retains_module_tails_but_not_character_tails() {
        let manifest = vec![OwnerManifestEntry {
            tuple: [
                "label".to_owned(),
                "assets/owner.bin".to_owned(),
                "bin".to_owned(),
            ],
            payload_hash: None,
        }];
        let retained = vec![json!([
            "label",
            "assets/owner.bin",
            "bin",
            "tail",
            {"rank":1}
        ])];

        assert!(owner_tuples_match(&retained, &manifest, true));
        assert!(!owner_tuples_match(&retained, &manifest, false));
        assert!(!owner_tuples_match(
            &[json!(["different", "assets/owner.bin", "bin", "tail"])],
            &manifest,
            true,
        ));
    }

    #[test]
    fn pinned_source_export_creates_a_verified_complete_lossless_package() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("source-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let output = directory.path().join("peer-source.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Source", b"source");

        let exported = create_and_verify_lossless_backup_v1(
            &output,
            &staging,
            &cas,
            &mut store,
            1,
            &NeverCancelled,
        )
        .unwrap();
        let verified =
            verify_lossless_package_v1(&mut File::open(&output).unwrap(), &NeverCancelled).unwrap();

        assert_eq!(exported, verified.archive_bytes);
        assert_eq!(verified.manifest.extensions["sourceRevision"], 1);
        assert_eq!(
            verified.manifest.extensions["assetRepositoryAuthority"],
            json!({
                "format": "v2",
                "migrationId": "lossless-test-migration",
                "compatibilityHash": "ab".repeat(32),
            })
        );
        assert_eq!(
            verified.manifest.extensions["coldPayloadAuthority"],
            json!({
                "format": "v2",
                "migrationId": "lossless-test-cold-migration",
                "compatibilityHash": "cd".repeat(32),
            })
        );
        for kind in [
            PayloadKind::Database,
            PayloadKind::Asset,
            PayloadKind::Inlay,
            PayloadKind::Cold,
            PayloadKind::OwnerManifest,
            PayloadKind::OwnerPayload,
        ] {
            assert!(verified
                .manifest
                .entries
                .iter()
                .any(|entry| entry.kind == kind));
        }
        assert_eq!(store.revision().unwrap(), 1);
    }

    #[test]
    fn pinned_source_export_accepts_a_root_member_after_split_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("source-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let output = directory.path().join("peer-source.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Source", b"source");
        let mut root = store.read_root(None).unwrap().value;
        root.as_object_mut()
            .unwrap()
            .insert("trailingRootMember".to_owned(), json!({ "nested": true }));
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();

        create_and_verify_lossless_backup_v1(
            &output,
            &staging,
            &cas,
            &mut store,
            2,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(store.revision().unwrap(), 2);
    }

    #[test]
    fn managed_export_report_matches_the_exact_package_bytes_and_counts() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("source-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let output = directory.path().join("managed-source.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Source", b"source");

        let report = create_and_verify_lossless_backup_v1_report(
            &output,
            &staging,
            &cas,
            &mut store,
            1,
            &NeverCancelled,
        )
        .unwrap();
        let package = fs::read(&output).unwrap();

        assert_eq!(report.archive_bytes, package.len() as u64);
        assert_eq!(report.archive_sha256, hex::encode(Sha256::digest(package)));
        assert_eq!(report.character_count, 1);
        assert_eq!(report.preset_count, 1);
    }

    #[test]
    fn durable_export_seals_every_lossless_cas_root_until_publication_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("source-staging");
        fs::create_dir(&staging).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Source", b"source");
        let output = directory.path().join("managed-source.lossless");
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-export-test",
            CasJobKind::OfficialPublicationOrExportPreparation,
            1,
        )
        .unwrap();

        let report = create_and_verify_lossless_backup_v1_durable_report(
            &output,
            &staging,
            &cas,
            &mut store,
            1,
            &mut durable,
            2,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.archive_bytes, fs::metadata(&output).unwrap().len());
        let verified =
            verify_lossless_package_v1(&mut File::open(&output).unwrap(), &NeverCancelled).unwrap();
        let expected_direct = verified
            .manifest
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.kind,
                    PayloadKind::Asset
                        | PayloadKind::Inlay
                        | PayloadKind::Cold
                        | PayloadKind::OwnerPayload
                )
            })
            .map(|entry| entry.sha256.clone())
            .collect();
        let expected_manifests = verified
            .manifest
            .entries
            .iter()
            .filter(|entry| {
                entry.kind == PayloadKind::OwnerManifest
                    && parse_owner_head_manifest_entry(entry).unwrap().present
            })
            .map(|entry| entry.sha256.clone())
            .collect();
        let roots = durable.root_set().unwrap();
        assert_eq!(roots.object_hashes, expected_direct);
        assert_eq!(roots.manifest_hashes, expected_manifests);
        let collected = collect_durable_cas_job_roots(directory.path());
        assert_eq!(collected, roots);
        durable.release(CasReleaseOutcome::Committed).unwrap();
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn durable_export_rejects_a_non_v2_authority_before_sealing() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("source-staging");
        fs::create_dir(&staging).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let output = directory.path().join("managed-source.lossless");
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-export-legacy",
            CasJobKind::OfficialPublicationOrExportPreparation,
            1,
        )
        .unwrap();

        let error = create_and_verify_lossless_backup_v1_durable_report(
            &output,
            &staging,
            &cas,
            &mut store,
            0,
            &mut durable,
            2,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        assert!(!durable.is_sealed());
        assert!(!output.exists());
        durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn durable_restore_promotes_only_the_preverified_package_and_seals_before_finalize() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        fs::create_dir(&staging).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let verified =
            verify_lossless_package_v1(&mut Cursor::new(&incoming), &NeverCancelled).unwrap();
        let recovery = directory.path().join("pre-replacement.lossless.part");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-restore-test",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();
        let finalized = AtomicBool::new(false);
        let commit_attempted = AtomicBool::new(false);

        let report = restore_verified_lossless_package_v1_durable_controlled(
            &mut Cursor::new(&incoming),
            &verified,
            &staging,
            &cas,
            &mut store,
            1,
            &recovery,
            None,
            &mut durable,
            2,
            &|| {
                assert!(recovery.is_file());
                let recovery_verified = verify_lossless_package_v1(
                    &mut File::open(&recovery).unwrap(),
                    &NeverCancelled,
                )
                .unwrap();
                let mut expected_direct = BTreeSet::new();
                let mut expected_manifests = BTreeSet::new();
                for manifest in [&verified.manifest, &recovery_verified.manifest] {
                    for entry in &manifest.entries {
                        match entry.kind {
                            PayloadKind::Asset
                            | PayloadKind::Inlay
                            | PayloadKind::Cold
                            | PayloadKind::OwnerPayload => {
                                expected_direct.insert(entry.sha256.clone());
                            }
                            PayloadKind::OwnerManifest
                                if parse_owner_head_manifest_entry(entry).unwrap().present =>
                            {
                                expected_manifests.insert(entry.sha256.clone());
                            }
                            PayloadKind::Database | PayloadKind::OwnerManifest => {}
                        }
                    }
                }
                for hash in &expected_manifests {
                    expected_direct.remove(hash);
                }
                let roots = collect_durable_cas_job_roots(directory.path());
                assert_eq!(roots.object_hashes, expected_direct);
                assert_eq!(roots.manifest_hashes, expected_manifests);
                assert!(roots.blockers.is_empty());
                finalized.store(true, Ordering::SeqCst);
                Ok(())
            },
            &|| commit_attempted.store(true, Ordering::SeqCst),
            &NeverCancelled,
        )
        .unwrap();

        assert!(finalized.load(Ordering::SeqCst));
        assert!(commit_attempted.load(Ordering::SeqCst));
        assert_eq!(report.revision, 2);
        assert_eq!(report.source_sha256, verified.archive_sha256);
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
        durable.release(CasReleaseOutcome::Committed).unwrap();
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn durable_restore_rejects_bytes_that_changed_after_verification_before_activation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        fs::create_dir(&staging).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let verified =
            verify_lossless_package_v1(&mut Cursor::new(&incoming), &NeverCancelled).unwrap();
        let changed = production_package(directory.path(), "Changed", b"changed");
        let recovery = directory.path().join("pre-replacement.lossless.part");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-restore-changed",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();

        let error = restore_verified_lossless_package_v1_durable_controlled(
            &mut Cursor::new(changed),
            &verified,
            &staging,
            &cas,
            &mut store,
            1,
            &recovery,
            None,
            &mut durable,
            2,
            &|| panic!("changed package must not reach finalization"),
            &|| panic!("changed package must not reach commit"),
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::HashMismatch);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(!recovery.exists());
        durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn durable_restore_uses_job_wide_owner_manifest_role_precedence() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        fs::create_dir(&staging).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let incoming_manifest = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
            payload_hash: Some(Sha256::digest(b"new-asset").into()),
        }])
        .unwrap();
        let verified =
            verify_lossless_package_v1(&mut Cursor::new(&incoming), &NeverCancelled).unwrap();
        let recovery = directory.path().join("pre-replacement.lossless.part");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store_with_asset_bytes(
            &mut store,
            &cas,
            "Old",
            b"old",
            &incoming_manifest,
            b"old-owner-history",
        );
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-restore-role-precedence",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();

        let report = restore_verified_lossless_package_v1_durable_controlled(
            &mut Cursor::new(incoming),
            &verified,
            &staging,
            &cas,
            &mut store,
            1,
            &recovery,
            None,
            &mut durable,
            2,
            &|| Ok(()),
            &|| {},
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        let collision_hash = hex::encode(Sha256::digest(&incoming_manifest));
        let roots = durable.root_set().unwrap();
        assert!(roots.manifest_hashes.contains(&collision_hash));
        assert!(!roots.object_hashes.contains(&collision_hash));
        durable.release(CasReleaseOutcome::Committed).unwrap();
    }

    #[test]
    fn durable_restore_commits_app_kv_with_the_replacement_generation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        fs::create_dir(&staging).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let verified =
            verify_lossless_package_v1(&mut Cursor::new(&incoming), &NeverCancelled).unwrap();
        let recovery = directory.path().join("pre-replacement.lossless.part");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-restore-app-kv",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();
        let marker = json!({ "manifestId": "manifest-2", "revision": 2 });

        let report = restore_verified_lossless_package_v1_durable_controlled(
            &mut Cursor::new(incoming),
            &verified,
            &staging,
            &cas,
            &mut store,
            1,
            &recovery,
            Some(("peerCloneActiveManifest", &marker)),
            &mut durable,
            2,
            &|| Ok(()),
            &|| {},
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        assert_eq!(
            store.get_app_kv("peerCloneActiveManifest").unwrap(),
            Some(marker)
        );
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
        durable.release(CasReleaseOutcome::Committed).unwrap();
    }

    #[test]
    fn durable_restore_rolls_back_when_the_atomic_app_kv_write_fails() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        fs::create_dir(&staging).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let verified =
            verify_lossless_package_v1(&mut Cursor::new(&incoming), &NeverCancelled).unwrap();
        let recovery = directory.path().join("pre-replacement.lossless.part");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        rusqlite::Connection::open(directory.path().join("persistent/persistent.db"))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_peer_clone_marker
                 BEFORE INSERT ON app_kv
                 WHEN NEW.key = 'peerCloneActiveManifest'
                 BEGIN
                     SELECT RAISE(ABORT, 'marker rejected');
                 END;",
            )
            .unwrap();
        let mut durable = DurableCasJob::begin(
            directory.path(),
            "lossless-restore-app-kv-rollback",
            CasJobKind::LosslessImport,
            1,
        )
        .unwrap();
        let marker = json!({ "manifestId": "manifest-2", "revision": 2 });

        restore_verified_lossless_package_v1_durable_controlled(
            &mut Cursor::new(incoming),
            &verified,
            &staging,
            &cas,
            &mut store,
            1,
            &recovery,
            Some(("peerCloneActiveManifest", &marker)),
            &mut durable,
            2,
            &|| Ok(()),
            &|| {},
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert_eq!(store.get_app_kv("peerCloneActiveManifest").unwrap(), None);
        durable.release(CasReleaseOutcome::Aborted).unwrap();
    }

    #[test]
    fn managed_restore_activation_fence_aborts_staging_and_keeps_the_backup() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let activation_attempted = AtomicBool::new(false);

        let error = restore_lossless_package_v1_controlled(
            &mut Cursor::new(&incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &|| {
                activation_attempted.store(true, Ordering::SeqCst);
                Err("managed restore finalization was not approved".to_owned())
            },
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::Store);
        assert!(activation_attempted.load(Ordering::SeqCst));
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert!(backup_path.is_file());
        verify_lossless_package_v1(&mut File::open(&backup_path).unwrap(), &NeverCancelled)
            .unwrap();
    }

    #[test]
    fn low_level_restore_keeps_v1_packages_without_asset_authority_compatible() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package_with_extensions(
            directory.path(),
            "New",
            b"new",
            json!({ "fixtureUnknown": { "preserved": true } }),
        );
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let report = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
        assert_eq!(
            store.read_asset_repository_authority(None).unwrap().value,
            AssetRepositoryAuthorityState::Legacy
        );
        assert_eq!(
            store.read_cold_payload_authority(None).unwrap().value,
            ColdPayloadAuthorityState::Legacy
        );
    }

    #[test]
    fn verifier_rejects_unsupported_known_authority_extensions() {
        let directory = tempfile::tempdir().unwrap();
        let extensions = [
            json!({
                "assetRepositoryAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-migration",
                    "compatibilityHash": "ab".repeat(32),
                    "unsupported": true,
                }
            }),
            json!({
                "coldPayloadAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-cold-migration",
                    "compatibilityHash": "cd".repeat(32),
                    "unsupported": true,
                }
            }),
            json!({
                "assetRepositoryAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-migration",
                    "compatibilityHash": "not-a-sha256",
                }
            }),
            json!({
                "coldPayloadAuthority": {
                    "format": "preparing",
                    "migrationId": "lossless-test-cold-migration",
                    "sourceRevision": -1,
                }
            }),
        ];
        for (index, extension) in extensions.into_iter().enumerate() {
            let incoming = production_package_with_extensions(
                directory.path(),
                &format!("Invalid authority {index}"),
                b"invalid-authority",
                extension,
            );

            let error = verify_lossless_package_v1(&mut Cursor::new(incoming), &NeverCancelled)
                .unwrap_err();

            assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        }
    }

    #[test]
    fn unsupported_asset_authority_extension_fails_without_activation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package_with_extensions(
            directory.path(),
            "New",
            b"new",
            json!({
                "assetRepositoryAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-migration",
                    "compatibilityHash": "ab".repeat(32),
                    "unsupported": true,
                }
            }),
        );
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert_eq!(
            store.read_asset_repository_authority(None).unwrap().value,
            AssetRepositoryAuthorityState::V2 {
                migration_id: "lossless-test-migration".to_owned(),
                compatibility_hash: "ab".repeat(32),
            }
        );
        assert!(!backup_path.exists());
    }

    #[test]
    fn unsupported_cold_authority_extension_fails_without_activation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package_with_extensions(
            directory.path(),
            "New",
            b"new",
            json!({
                "coldPayloadAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-cold-migration",
                    "compatibilityHash": "cd".repeat(32),
                    "unsupported": true,
                }
            }),
        );
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert_eq!(
            store.read_cold_payload_authority(None).unwrap().value,
            ColdPayloadAuthorityState::V2 {
                migration_id: "lossless-test-cold-migration".to_owned(),
                compatibility_hash: "cd".repeat(32),
            }
        );
        assert!(!backup_path.exists());
    }

    #[test]
    fn restore_commits_activation_marker_with_lossless_generation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let report = restore_lossless_package_v1_with_app_kv(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            "peerCloneActiveManifest",
            &json!({ "manifestId": "manifest-2", "revision": 2 }),
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        assert_eq!(
            store.get_app_kv("peerCloneActiveManifest").unwrap(),
            Some(json!({ "manifestId": "manifest-2", "revision": 2 }))
        );
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
    }

    #[test]
    fn production_target_activates_the_verified_lossless_package_once() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &NeverCancelled)
                .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();

        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::Activated
        );
        assert_eq!(
            target.active_manifest_id().unwrap().as_deref(),
            Some(manifest_id)
        );
        assert_eq!(
            target
                .activate_if_current(&mut stage, Some(manifest_id), manifest_id)
                .unwrap(),
            CloneActivation::AlreadyActive
        );
        drop(target);
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
        assert_eq!(fs::read_dir(&target_root).unwrap().count(), 1);
    }

    #[test]
    fn production_target_reports_the_exact_pre_replacement_backup() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
        let mut observed = Vec::new();
        let mut observer = |path: &Path| {
            observed.push(path.to_owned());
            Ok(())
        };
        let mut target = LosslessCloneTargetAdapter::new_with_backup_observer(
            &mut store,
            &cas,
            &target_root,
            1,
            &NeverCancelled,
            &mut observer,
        )
        .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();

        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::Activated
        );
        let expected = target.committed_backup_path().unwrap().to_owned();
        let backups = fs::canonicalize(target_root.join("backups")).unwrap();
        assert_eq!(expected.parent(), Some(backups.as_path()));
        drop(target);
        assert_eq!(observed, vec![expected.clone()]);
        assert!(expected.is_file());
    }

    #[test]
    fn production_target_retries_backup_observation_before_cleaning_a_committed_stage() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let mut rejecting_observer = |_path: &Path| {
            Err(PeerSyncError::Storage(
                "backup receipt unavailable".to_owned(),
            ))
        };
        let mut target = LosslessCloneTargetAdapter::new_with_backup_observer(
            &mut store,
            &cas,
            &target_root,
            1,
            &NeverCancelled,
            &mut rejecting_observer,
        )
        .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();

        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap_err(),
            PeerSyncError::Storage("backup receipt unavailable".to_owned())
        );
        drop(target);
        assert_eq!(store.revision().unwrap(), 2);
        let expected = fs::canonicalize(
            fs::read_dir(target_root.join("backups"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        let retained_stage = fs::read_dir(&target_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir() && path.file_name().unwrap() != "backups")
            .unwrap();
        assert!(expected.is_file());
        assert!(retained_stage.is_dir());

        let mut observed = Vec::new();
        let mut observer = |path: &Path| {
            observed.push(path.to_owned());
            Ok(())
        };
        let target = LosslessCloneTargetAdapter::new_with_backup_observer(
            &mut store,
            &cas,
            &target_root,
            2,
            &NeverCancelled,
            &mut observer,
        )
        .unwrap();
        assert_eq!(target.committed_backup_path(), Some(expected.as_path()));
        drop(target);
        assert_eq!(observed, vec![expected.clone()]);
        assert!(!retained_stage.exists());
    }

    #[test]
    fn production_target_does_not_report_a_foreign_same_manifest_stage() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "abababababababababababababababababababababababababababababababab";
        let old_session_id = uuid::Uuid::new_v4().to_string();
        let mut old_observer = |_path: &Path| Ok(());
        let mut target = LosslessCloneTargetAdapter::new_with_owned_backup_observer(
            &mut store,
            &cas,
            &target_root,
            1,
            &NeverCancelled,
            manifest_id,
            &old_session_id,
            &mut old_observer,
        )
        .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();
        target.leave_durable_job_after_commit_once_for_test();
        assert!(target
            .activate_if_current(&mut stage, None, manifest_id)
            .is_err());
        drop(target);

        let new_session_id = uuid::Uuid::new_v4().to_string();
        let mut observed = Vec::new();
        let mut new_observer = |path: &Path| {
            observed.push(path.to_owned());
            Ok(())
        };
        let target = LosslessCloneTargetAdapter::new_with_owned_backup_observer(
            &mut store,
            &cas,
            &target_root,
            2,
            &NeverCancelled,
            manifest_id,
            &new_session_id,
            &mut new_observer,
        )
        .unwrap();

        assert_eq!(target.committed_backup_path(), None);
        drop(target);
        assert!(observed.is_empty());
        assert_eq!(
            fs::read_dir(target_root.join("backups")).unwrap().count(),
            1
        );
    }

    #[test]
    fn production_target_retries_same_manifest_with_a_new_pre_replacement_backup() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &NeverCancelled)
                .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming.clone()),
            )
            .unwrap();
        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::Activated
        );
        drop(target);

        let mut root = store.read_root(None).unwrap().value;
        root["username"] = Value::String("Local edit".to_owned());
        store
            .commit(&WorkingSetCommit {
                expected_revision: 2,
                root: Some(root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: None,
            })
            .unwrap();

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 3, &NeverCancelled)
                .unwrap();
        assert_eq!(target.active_manifest_id().unwrap(), None);
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();
        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::Activated
        );
        drop(target);

        assert_eq!(store.revision().unwrap(), 4);
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
        assert_eq!(
            fs::read_dir(target_root.join("backups")).unwrap().count(),
            2
        );
    }

    #[test]
    fn production_target_adopts_same_manifest_that_wins_the_commit_race() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let cancellation = CommitSameManifestOnFirstCheck {
            store_root: directory.path().to_owned(),
            manifest_id: manifest_id.to_owned(),
            fired: AtomicBool::new(false),
        };
        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &cancellation)
                .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();

        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::AlreadyActive
        );
        target.abort(stage).unwrap();
        drop(target);

        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(store.materialize(None).unwrap()["username"], "New");
    }

    #[test]
    fn production_target_recovers_a_sealed_precommit_peer_clone_job() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &NeverCancelled)
                .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming.clone()),
            )
            .unwrap();
        target.leave_durable_job_after_precommit_failure_once_for_test();

        assert!(target
            .activate_if_current(&mut stage, None, manifest_id)
            .is_err());
        let retained = collect_durable_cas_job_roots(directory.path());
        assert!(retained.blockers.is_empty());
        assert!(!retained.object_hashes.is_empty() || !retained.manifest_hashes.is_empty());
        drop(stage);
        drop(target);
        assert_eq!(store.revision().unwrap(), 1);

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &NeverCancelled)
                .unwrap();
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
        let mut retry = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut retry,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();
        assert_eq!(
            target
                .activate_if_current(&mut retry, None, manifest_id)
                .unwrap(),
            CloneActivation::Activated
        );
        drop(target);
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn production_target_recovers_a_committed_peer_clone_job_before_idempotent_retry() {
        let directory = tempfile::tempdir().unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let target_root = directory.path().join("peer-target");
        let manifest_id = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 1, &NeverCancelled)
                .unwrap();
        let mut stage = target.begin(manifest_id).unwrap();
        target
            .stage_object(
                &mut stage,
                CloneObjectKind::Database,
                "database",
                &Value::Null,
                &mut Cursor::new(incoming),
            )
            .unwrap();
        target.leave_durable_job_after_commit_once_for_test();

        assert!(target
            .activate_if_current(&mut stage, None, manifest_id)
            .is_err());
        assert_eq!(
            target.active_manifest_id().unwrap().as_deref(),
            Some(manifest_id)
        );
        let retained = collect_durable_cas_job_roots(directory.path());
        assert!(retained.blockers.is_empty());
        assert!(!retained.object_hashes.is_empty() || !retained.manifest_hashes.is_empty());
        drop(target);
        assert_eq!(store.revision().unwrap(), 2);

        let mut target =
            LosslessCloneTargetAdapter::new(&mut store, &cas, &target_root, 2, &NeverCancelled)
                .unwrap();
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
        assert_eq!(
            target
                .activate_if_current(&mut stage, None, manifest_id)
                .unwrap(),
            CloneActivation::AlreadyActive
        );
        assert_eq!(
            collect_durable_cas_job_roots(directory.path()),
            Default::default()
        );
    }

    #[test]
    fn production_source_pins_one_complete_lossless_package_before_serving() {
        let directory = tempfile::tempdir().unwrap();
        let repository = directory.path().join("repository");
        let preparation = directory.path().join("source-preparation");
        let session_root = directory.path().join("source-session");
        fs::create_dir(&repository).unwrap();
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Source", b"source");

        let session = prepare_lossless_clone_session(
            &mut store,
            &cas,
            1,
            &preparation,
            &session_root,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(session.manifest().source_revision, 1);
        assert_eq!(
            session.manifest().database.format,
            CLONE_LOSSLESS_DATABASE_FORMAT
        );
        assert!(session.manifest().payloads.is_empty());
        assert_eq!(session.manifest().objects.len(), 1);
        assert!(!preparation.exists());
        assert_eq!(store.revision().unwrap(), 1);
    }

    #[test]
    fn owner_manifest_missing_historical_payload_fails_before_activation() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let owner_manifest = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
            payload_hash: Some(Sha256::digest(b"ambient-only").into()),
        }])
        .unwrap();
        let incoming = production_package_with_owner_manifest(
            directory.path(),
            "New",
            b"new",
            Some(owner_manifest),
        );
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        cas.prepare_bytes(b"ambient-only").unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::MissingReference);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(!backup_path.exists());
    }

    #[test]
    fn prebackup_deduplicates_zero_length_owner_payload_with_absent_marker() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("job-staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let backup_path = directory.path().join("pre-replacement.lossless");
        let mut store = PersistentStore::open(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        seed_active_store_with_owner_history(&mut store, &cas, "Old", b"old", b"");

        let report = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap();

        assert_eq!(report.revision, 2);
        let empty_hash = hex::encode(Sha256::digest([]));
        let backup =
            verify_lossless_package_v1(&mut File::open(&backup_path).unwrap(), &NeverCancelled)
                .unwrap();
        assert_eq!(
            backup
                .manifest
                .entries
                .iter()
                .filter(|entry| entry.sha256 == empty_hash)
                .count(),
            1
        );
        assert!(!backup.manifest.entries.iter().any(|entry| {
            entry.kind == PayloadKind::OwnerPayload && entry.sha256 == empty_hash
        }));

        let isolated_repository = directory.path().join("isolated-zero-repository");
        let isolated_staging = directory.path().join("isolated-zero-staging");
        fs::create_dir(&isolated_repository).unwrap();
        fs::create_dir(&isolated_staging).unwrap();
        let isolated_cas = PayloadCas::new(&isolated_repository).unwrap();
        let isolated = read_lossless_package_v1(
            &mut File::open(&backup_path).unwrap(),
            &isolated_staging,
            &isolated_cas,
            &NeverCancelled,
        )
        .unwrap();
        assert_eq!(
            isolated_cas.read_object(&empty_hash).unwrap().unwrap(),
            Vec::<u8>::new()
        );
        drop(isolated);
    }

    #[test]
    fn sqlite_reopen_observes_complete_old_or_database_and_typed_payload_generation() {
        for fail_before_commit in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let staging = directory.path().join("job-staging");
            let repository = directory.path().join("repository");
            fs::create_dir(&staging).unwrap();
            fs::create_dir(&repository).unwrap();
            let incoming = production_package(directory.path(), "New", b"new");
            let backup_path = directory.path().join("pre-replacement.lossless");
            let mut store = crate::persistent_store::PersistentStore::open(&repository).unwrap();
            let cas = PayloadCas::new(&repository).unwrap();
            seed_active_store(&mut store, &cas, "Old", b"old");

            let result = restore_lossless_package_v1(
                &mut Cursor::new(incoming),
                &staging,
                &cas,
                &mut store,
                if fail_before_commit { 0 } else { 1 },
                &backup_path,
                &NeverCancelled,
            );

            drop(store);
            let reopened = crate::persistent_store::PersistentStore::open(&repository).unwrap();
            if fail_before_commit {
                assert_eq!(
                    result.unwrap_err().code,
                    LosslessErrorCode::RevisionConflict
                );
                assert_eq!(reopened.revision().unwrap(), 1);
                assert_eq!(reopened.materialize(None).unwrap()["username"], "Old");
                assert!(!backup_path.exists());
            } else {
                assert_eq!(result.unwrap().revision, 2);
                assert_eq!(reopened.revision().unwrap(), 2);
                assert_eq!(reopened.materialize(None).unwrap()["username"], "New");
                assert_eq!(
                    reopened
                        .read_asset_repository_authority(None)
                        .unwrap()
                        .value,
                    AssetRepositoryAuthorityState::V2 {
                        migration_id: "lossless-test-migration".to_owned(),
                        compatibility_hash: "ab".repeat(32),
                    }
                );
                assert_eq!(
                    reopened.read_cold_payload_authority(None).unwrap().value,
                    ColdPayloadAuthorityState::V2 {
                        migration_id: "lossless-test-cold-migration".to_owned(),
                        compatibility_hash: "cd".repeat(32),
                    }
                );
                let asset = reopened
                    .read_asset_alias("asset", "shared", None)
                    .unwrap()
                    .unwrap();
                let inlay = reopened
                    .read_asset_alias("inlay", "shared", None)
                    .unwrap()
                    .unwrap();
                let cold = reopened.read_cold_alias("shared", None).unwrap().unwrap();
                assert_eq!((asset.revision, inlay.revision, cold.revision), (2, 2, 2));
                assert_eq!(asset.value.metadata["nested"]["label"], "New-asset");
                assert_eq!(inlay.value.metadata["nested"]["label"], "New-inlay");
                assert_eq!(cold.value.metadata["nested"]["label"], "New-cold");
                assert_eq!(cold.value.metadata[ALIAS_METADATA_FIELD], "user collision");
                let owner = reopened
                    .read_asset_owner_head(&AssetOwnerLocator::RootModuleAssets { index: 0 }, None)
                    .unwrap()
                    .unwrap();
                assert!(owner.value.present);
                assert_eq!(owner.value.entry_count, 1);
                let owner_manifest = cas
                    .read_object(owner.value.manifest_hash.as_ref().unwrap())
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    decode_owner_manifest(&owner_manifest).unwrap(),
                    vec![OwnerManifestEntry {
                        tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
                        payload_hash: Some(Sha256::digest(b"new-asset").into()),
                    }]
                );
                assert_eq!(
                    reopened
                        .read_asset_owner_head(
                            &AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 },
                            None,
                        )
                        .unwrap()
                        .unwrap()
                        .value,
                    AssetOwnerHead::absent(AssetOwnerLocator::PersonaEmbeddedModuleAssets {
                        index: 0,
                    })
                );
                assert_eq!(
                    cas.read_object(asset.value.object_hash.as_ref().unwrap())
                        .unwrap()
                        .unwrap(),
                    b"new-asset"
                );
                assert_eq!(
                    cas.read_object(inlay.value.object_hash.as_ref().unwrap())
                        .unwrap()
                        .unwrap(),
                    b"new-inlay"
                );
                assert!(backup_path.is_file());
                let backup = verify_lossless_package_v1(
                    &mut File::open(&backup_path).unwrap(),
                    &NeverCancelled,
                )
                .unwrap();
                assert_eq!(backup.manifest.extensions["sourceRevision"], 1);
                assert_eq!(backup.manifest.entries.len(), 7);
                assert!(backup
                    .manifest
                    .entries
                    .iter()
                    .any(|entry| entry.kind == PayloadKind::Cold
                        && entry.logical_key.as_deref() == Some("shared")));
                let backup_cold = backup
                    .manifest
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.kind == PayloadKind::Cold
                            && entry.logical_key.as_deref() == Some("shared")
                    })
                    .unwrap();
                assert_eq!(backup_cold.metadata["source"], "legacy-cold");
                assert_eq!(backup_cold.metadata["ordinal"], 7);
                assert_eq!(backup_cold.metadata[ALIAS_METADATA_FIELD], "user collision");
                assert!(backup_cold.metadata.get("name").is_none());
                assert_eq!(
                    backup
                        .manifest
                        .entries
                        .iter()
                        .filter(|entry| entry.kind == PayloadKind::OwnerManifest)
                        .count(),
                    2
                );
                let backup_owner = backup
                    .manifest
                    .entries
                    .iter()
                    .find(|entry| {
                        entry.kind == PayloadKind::OwnerManifest
                            && entry.metadata["present"] == true
                    })
                    .unwrap();
                let expected_owner_manifest = encode_owner_manifest(&[OwnerManifestEntry {
                    tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
                    payload_hash: Some(Sha256::digest(b"old-owner-history").into()),
                }])
                .unwrap();
                assert_eq!(
                    backup_owner.sha256,
                    hex::encode(Sha256::digest(&expected_owner_manifest))
                );
                assert_eq!(
                    backup_owner.byte_length,
                    expected_owner_manifest.len() as u64
                );
                let isolated_repository = directory.path().join("isolated-repository");
                let isolated_staging = directory.path().join("isolated-staging");
                fs::create_dir(&isolated_repository).unwrap();
                fs::create_dir(&isolated_staging).unwrap();
                let isolated_cas = PayloadCas::new(&isolated_repository).unwrap();
                let isolated = read_lossless_package_v1(
                    &mut File::open(&backup_path).unwrap(),
                    &isolated_staging,
                    &isolated_cas,
                    &NeverCancelled,
                )
                .unwrap();
                let historical_bytes = b"old-owner-history";
                let historical_hash = hex::encode(Sha256::digest(historical_bytes));
                assert_eq!(
                    isolated_cas.read_object(&historical_hash).unwrap().unwrap(),
                    historical_bytes
                );
                drop(isolated);
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
        store
            .replace_put_cold_aliases(
                &abandoned,
                &project_cold_aliases(&incoming.entries).unwrap(),
            )
            .unwrap();

        drop(store);
        let reopened = PersistentStore::open(directory.path()).unwrap();

        assert_eq!(reopened.revision().unwrap(), 1);
        assert_eq!(reopened.materialize(None).unwrap()["username"], "Old");
        assert_eq!(
            reopened
                .read_asset_alias("asset", "asset.bin", None)
                .unwrap(),
            None
        );
        assert_eq!(reopened.read_cold_alias("character", None).unwrap(), None);
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
    fn payload_logical_keys_are_unique_within_each_namespace() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("database");
        let asset = directory.path().join("asset");
        let inlay = directory.path().join("inlay");
        let cold = directory.path().join("cold");
        fs::write(&database, b"database").unwrap();
        fs::write(&asset, b"asset").unwrap();
        fs::write(&inlay, b"inlay").unwrap();
        fs::write(&cold, b"cold").unwrap();
        let entries = vec![
            LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database,
            },
            LosslessWriteEntry {
                logical_path: "assets/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Asset,
                metadata: json!({ "name": "asset", "ext": "", "mime": "application/octet-stream" }),
                source: asset,
            },
            LosslessWriteEntry {
                logical_path: "inlays/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Inlay,
                metadata: json!({
                    "name": "inlay", "ext": "", "mime": "application/octet-stream",
                    "inlayType": "image"
                }),
                source: inlay,
            },
            LosslessWriteEntry {
                logical_path: "cold/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Cold,
                metadata: json!({ "name": "cold", "ext": "", "mime": "application/octet-stream" }),
                source: cold,
            },
        ];

        let report = write_lossless_package_v1(
            &mut Vec::new(),
            &entries,
            compatibility(),
            vec![
                reference("asset", "shared", ReferenceStatus::Present, 0),
                reference("inlay", "shared", ReferenceStatus::Present, 1),
                reference("cold", "shared", ReferenceStatus::Present, 2),
            ],
            vec![],
            json!({}),
            &NeverCancelled,
        )
        .expect("kind-scoped keys do not collide");

        assert_eq!(report.manifest.entries.len(), 4);
    }

    #[test]
    fn successful_read_owns_database_file_and_reopen_sweeps_forgotten_job_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let package = minimal_package(directory.path(), "owned", b"database", b"asset");
        let cas = PayloadCas::new(repository).unwrap();

        let first =
            read_lossless_package_v1(&mut Cursor::new(&package), &staging, &cas, &NeverCancelled)
                .unwrap();
        let forgotten = first
            .entries
            .iter()
            .find(|entry| entry.kind == PayloadKind::Database)
            .unwrap()
            .staged_path
            .as_ref()
            .unwrap()
            .to_path_buf();
        assert!(forgotten.is_file());
        std::mem::forget(first);
        let forgotten_owner_marker = staging
            .join("lossless-v1")
            .join(format!("{}.owner-empty", uuid::Uuid::new_v4()));
        fs::write(&forgotten_owner_marker, []).unwrap();
        let forgotten_verification =
            staging.join(format!("lossless-verify-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&forgotten_verification).unwrap();
        fs::write(forgotten_verification.join("database.risudat"), b"stale").unwrap();

        let second =
            read_lossless_package_v1(&mut Cursor::new(&package), &staging, &cas, &NeverCancelled)
                .unwrap();
        assert!(
            !forgotten.exists(),
            "reopen must sweep the forgotten job file"
        );
        assert!(!forgotten_owner_marker.exists());
        assert!(!forgotten_verification.exists());
        let active = second
            .entries
            .iter()
            .find(|entry| entry.kind == PayloadKind::Database)
            .unwrap()
            .staged_path
            .as_ref()
            .unwrap()
            .to_path_buf();
        assert!(active.is_file());
        drop(second);
        assert!(
            !active.exists(),
            "the successful report retains cleanup ownership"
        );
    }

    #[test]
    fn staging_cleanup_does_not_follow_a_linked_owned_directory() {
        let root = tempfile::tempdir().unwrap();
        let unrelated = root.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        let unrelated_file = unrelated.join(format!("{}.database", uuid::Uuid::new_v4()));
        fs::write(&unrelated_file, b"unrelated").unwrap();
        let staging = root.path().join("lossless-v1");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&unrelated, &staging).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_dir(&unrelated, &staging) {
            if error.raw_os_error() == Some(1314) {
                return;
            }
            panic!("failed to create linked staging fixture: {error}");
        }

        let error = prepare_staging_directory(root.path()).unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidPath);
        assert_eq!(fs::read(unrelated_file).unwrap(), b"unrelated");
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
    fn owner_manifest_decode_limit_rejects_oversize_before_payload_allocation() {
        let head = AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            "11".repeat(32),
            1,
        );
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
                    logical_path: "owner-manifests/bounded".to_owned(),
                    logical_key: Some(owner_head_logical_key(&head.owner)),
                    kind: PayloadKind::OwnerManifest,
                    byte_length: MAX_OWNER_MANIFEST_BYTES,
                    sha256: head.manifest_hash.clone().unwrap(),
                    metadata: serde_json::to_value(&head).unwrap(),
                },
            ],
            references: vec![],
            warnings: vec![],
            extensions: json!({}),
        };

        validate_manifest(&manifest).expect("accepted M5 manifest range remains representable");
        manifest.entries[1].byte_length = MAX_OWNER_MANIFEST_BYTES + 1;
        let error = validate_manifest(&manifest).unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::InvalidManifest);
        assert!(error.message.contains("owner manifest decode limit"));
    }

    #[test]
    fn cancellation_and_incomplete_pinned_inventory_abort_without_activation_or_backup() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let cas = PayloadCas::new(&repository).unwrap();
        let mut store = PersistentStore::open(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let backup_path = directory.path().join("cancelled.lossless");
        let cancelled = Arc::new(AtomicBool::new(true));
        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming.clone()),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &crate::local_backup::AtomicCancellation::new(cancelled),
        )
        .unwrap_err();
        assert_eq!(error.code, LosslessErrorCode::Cancelled);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(!backup_path.exists());

        let old_asset = store
            .read_asset_alias("asset", "shared", None)
            .unwrap()
            .unwrap();
        let object = cas
            .object_path(old_asset.value.object_hash.as_ref().unwrap())
            .unwrap()
            .unwrap();
        fs::remove_file(object).unwrap();
        let backup_path = directory.path().join("incomplete.lossless");
        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::BackupIncomplete);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(!backup_path.exists());
    }

    #[test]
    fn cancellation_during_copy_cleans_partial_staging_and_preserves_active_revision() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let large_prefix = vec![0x5a; 2 * 1024 * 1024];
        let incoming = production_package(directory.path(), "New", &large_prefix);
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut source = CancelAfterBytes {
            inner: Cursor::new(incoming.clone()),
            bytes_read: 0,
            cancel_after: incoming.len() / 2,
            cancelled: cancelled.clone(),
        };
        let cas = PayloadCas::new(&repository).unwrap();
        let mut store = PersistentStore::open(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let backup_path = directory.path().join("mid-copy.lossless");

        let error = restore_lossless_package_v1(
            &mut source,
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &crate::local_backup::AtomicCancellation::new(cancelled),
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::Cancelled);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert!(!backup_path.exists());
        assert!(fs::read_dir(staging.join("lossless-v1"))
            .unwrap()
            .next()
            .is_none());
        assert!(fs::read_dir(repository.join("assets-v2").join("staging"))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn final_cancellation_check_runs_after_snapshot_before_activation() {
        let baseline_directory = tempfile::tempdir().unwrap();
        let baseline_staging = baseline_directory.path().join("staging");
        let baseline_repository = baseline_directory.path().join("repository");
        fs::create_dir(&baseline_staging).unwrap();
        fs::create_dir(&baseline_repository).unwrap();
        let baseline_incoming = production_package(baseline_directory.path(), "New", b"new");
        let baseline_cas = PayloadCas::new(&baseline_repository).unwrap();
        let mut baseline_store = PersistentStore::open(&baseline_repository).unwrap();
        seed_active_store(&mut baseline_store, &baseline_cas, "Old", b"old");
        let baseline_probe = CheckCountingCancellation {
            calls: AtomicUsize::new(0),
            cancel_at: None,
        };
        restore_lossless_package_v1(
            &mut Cursor::new(baseline_incoming),
            &baseline_staging,
            &baseline_cas,
            &mut baseline_store,
            1,
            &baseline_directory.path().join("baseline.lossless"),
            &baseline_probe,
        )
        .unwrap();
        let final_check = baseline_probe.calls.load(Ordering::SeqCst);
        assert!(final_check > 0);

        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let cas = PayloadCas::new(&repository).unwrap();
        let mut store = PersistentStore::open(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");
        let backup_path = directory.path().join("boundary.lossless");
        let cancellation = CheckCountingCancellation {
            calls: AtomicUsize::new(0),
            cancel_at: Some(final_check),
        };

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::Cancelled);
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "Old");
        assert!(backup_path.is_file());
        verify_lossless_package_v1(&mut File::open(&backup_path).unwrap(), &NeverCancelled)
            .unwrap();
        assert_eq!(store.snapshot_list().unwrap().len(), 1);
    }

    #[test]
    fn present_unaliased_legacy_payload_fails_closed_before_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let incoming = production_package(directory.path(), "New", b"new");
        let cas = PayloadCas::new(repository).unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        stage_f0_database(&mut store, "OldMissing");
        let legacy_asset = directory.path().join("blobstore").join("assets");
        fs::create_dir_all(&legacy_asset).unwrap();
        fs::write(legacy_asset.join("shared"), b"legacy-present").unwrap();
        let backup_path = directory.path().join("legacy-missing.lossless");

        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_path,
            &NeverCancelled,
        )
        .unwrap_err();

        assert_eq!(error.code, LosslessErrorCode::BackupIncomplete);
        assert!(error.message.contains("absence is not proved"));
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(store.materialize(None).unwrap()["username"], "OldMissing");
        assert!(!backup_path.exists());
    }

    #[test]
    fn decoded_graph_mismatch_and_backup_output_failpoint_preserve_active_revision() {
        let directory = tempfile::tempdir().unwrap();
        let staging = directory.path().join("staging");
        let repository = directory.path().join("repository");
        fs::create_dir(&staging).unwrap();
        fs::create_dir(&repository).unwrap();
        let cas = PayloadCas::new(&repository).unwrap();
        let mut store = PersistentStore::open(&repository).unwrap();
        seed_active_store(&mut store, &cas, "Old", b"old");

        let mut mismatched = production_package(directory.path(), "Mismatch", b"mismatch");
        rewrite_manifest(&mut mismatched, |manifest| {
            manifest.compatibility.reference_graph_sha256 = "00".repeat(32);
        });
        let graph_backup = directory.path().join("graph-mismatch.lossless");
        let error = restore_lossless_package_v1(
            &mut Cursor::new(mismatched),
            &staging,
            &cas,
            &mut store,
            1,
            &graph_backup,
            &NeverCancelled,
        )
        .unwrap_err();
        assert_eq!(error.code, LosslessErrorCode::UnexpectedReference);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(!graph_backup.exists());

        let incoming = production_package(directory.path(), "OutputFailure", b"output");
        let backup_directory = directory.path().join("backup-output-failpoint");
        fs::create_dir(&backup_directory).unwrap();
        let error = restore_lossless_package_v1(
            &mut Cursor::new(incoming),
            &staging,
            &cas,
            &mut store,
            1,
            &backup_directory,
            &NeverCancelled,
        )
        .unwrap_err();
        assert_eq!(error.code, LosslessErrorCode::Io);
        assert_eq!(store.revision().unwrap(), 1);
        assert!(backup_directory.is_dir());
    }

    #[test]
    fn asset_backup_metadata_fills_canonical_fields_without_discarding_unknowns() {
        let asset = AssetAlias {
            key: "shared".to_owned(),
            object_hash: Some("11".repeat(32)),
            kind: "asset".to_owned(),
            size: 1,
            mime: "application/octet-stream".to_owned(),
            name: "shared.bin".to_owned(),
            ext: "bin".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({
                "nested": { "preserved": true },
                "name": 17,
                "ext": null,
                "mime": false,
                "inlayType": "image",
                "width": "legacy",
                "height": 99,
                "risuNestAliasMetadata": "user collision"
            }),
        };
        let inlay = AssetAlias {
            key: "shared".to_owned(),
            object_hash: Some("22".repeat(32)),
            kind: "inlay".to_owned(),
            size: 1,
            mime: "image/png".to_owned(),
            name: "original.png".to_owned(),
            ext: "png".to_owned(),
            inlay_type: Some("image".to_owned()),
            width: None,
            height: Some(13),
            metadata: json!({
                "nested": { "preserved": true },
                "inlayType": "audio",
                "width": "legacy",
                "height": "legacy"
            }),
        };

        let asset_metadata = lossless_asset_metadata(&asset).unwrap();
        let inlay_metadata = lossless_asset_metadata(&inlay).unwrap();
        assert_eq!(
            asset_metadata,
            json!({
                "nested": { "preserved": true },
                "name": "shared.bin",
                "ext": "bin",
                "mime": "application/octet-stream",
                "risuNestAliasMetadata": asset.metadata
            })
        );
        assert_eq!(
            inlay_metadata,
            json!({
                "nested": { "preserved": true },
                "name": "original.png",
                "ext": "png",
                "mime": "image/png",
                "inlayType": "image",
                "height": 13,
                "risuNestAliasMetadata": inlay.metadata
            })
        );

        let staged = [
            StagedLosslessEntry {
                logical_path: "assets/shared".to_owned(),
                logical_key: Some(asset.key.clone()),
                kind: PayloadKind::Asset,
                byte_length: asset.size as u64,
                sha256: asset.object_hash.clone().unwrap(),
                metadata: asset_metadata,
                staged_path: None,
                immutable_object: None,
            },
            StagedLosslessEntry {
                logical_path: "inlays/shared".to_owned(),
                logical_key: Some(inlay.key.clone()),
                kind: PayloadKind::Inlay,
                byte_length: inlay.size as u64,
                sha256: inlay.object_hash.clone().unwrap(),
                metadata: inlay_metadata,
                staged_path: None,
                immutable_object: None,
            },
        ];
        assert_eq!(
            project_payload_aliases(&staged).unwrap(),
            vec![asset, inlay]
        );
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

    #[test]
    fn fifty_thousand_payload_inventory_entries_validate_by_identity() {
        let metadata = json!({
            "name": "asset.bin",
            "ext": "bin",
            "mime": "application/octet-stream"
        });
        let entries = (0..50_000)
            .map(|index| {
                let key = format!("asset-{index:05}");
                LosslessManifestEntry {
                    logical_path: format!("assets/{key}"),
                    logical_key: Some(key),
                    kind: PayloadKind::Asset,
                    byte_length: 0,
                    sha256: "11".repeat(32),
                    metadata: metadata.clone(),
                }
            })
            .collect::<Vec<_>>();
        let payloads = (0..50_000)
            .rev()
            .map(|index| F0PayloadDescriptor {
                kind: F0PayloadKind::Asset,
                key: format!("asset-{index:05}"),
                sha256: "11".repeat(32),
                byte_length: 0,
                metadata: metadata.clone(),
                cold_source: None,
            })
            .collect::<Vec<_>>();
        let manifest = LosslessManifest {
            version: FORMAT_VERSION,
            compatibility: compatibility(),
            entries,
            references: vec![],
            warnings: vec![],
            extensions: json!({}),
        };

        validate_payload_manifest(&manifest, &payloads).unwrap();
    }

    fn production_package(root: &Path, username: &str, payload_prefix: &[u8]) -> Vec<u8> {
        production_package_with_owner_manifest(root, username, payload_prefix, None)
    }

    fn production_package_with_owner_manifest(
        root: &Path,
        username: &str,
        payload_prefix: &[u8],
        owner_manifest_override: Option<Vec<u8>>,
    ) -> Vec<u8> {
        production_package_with_owner_manifest_and_extensions(
            root,
            username,
            payload_prefix,
            owner_manifest_override,
            json!({
                "fixtureUnknown": { "preserved": true },
                "assetRepositoryAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-migration",
                    "compatibilityHash": "ab".repeat(32),
                },
                "coldPayloadAuthority": {
                    "format": "v2",
                    "migrationId": "lossless-test-cold-migration",
                    "compatibilityHash": "cd".repeat(32),
                }
            }),
        )
    }

    fn production_package_with_extensions(
        root: &Path,
        username: &str,
        payload_prefix: &[u8],
        extensions: Value,
    ) -> Vec<u8> {
        production_package_with_owner_manifest_and_extensions(
            root,
            username,
            payload_prefix,
            None,
            extensions,
        )
    }

    fn production_package_with_owner_manifest_and_extensions(
        root: &Path,
        username: &str,
        payload_prefix: &[u8],
        owner_manifest_override: Option<Vec<u8>>,
        extensions: Value,
    ) -> Vec<u8> {
        let database_root = root.join(format!("package-database-{username}"));
        fs::create_dir(&database_root).unwrap();
        let mut source_store = PersistentStore::open(&database_root).unwrap();
        stage_f0_database(&mut source_store, username);
        let lease = source_store.acquire_revision(1).unwrap().lease;
        let database = source_store.materialize_lease(&lease).unwrap();
        let export = source_store.export_risu_save(&lease, false).unwrap();
        let database_path = PathBuf::from(&export.path);

        let asset_bytes = [payload_prefix, b"-asset"].concat();
        let inlay_bytes = [payload_prefix, b"-inlay"].concat();
        let cold_bytes = cold_payload(&json!({
            "message": [{ "data": format!("{}-cold", username) }]
        }));
        let asset_path = root.join(format!("package-asset-{username}"));
        let inlay_path = root.join(format!("package-inlay-{username}"));
        let cold_path = root.join(format!("package-cold-{username}"));
        let owner_manifest_bytes = owner_manifest_override.unwrap_or_else(|| {
            encode_owner_manifest(&[OwnerManifestEntry {
                tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
                payload_hash: Some(Sha256::digest(&asset_bytes).into()),
            }])
            .unwrap()
        });
        let owner_manifest_path = root.join(format!("package-owner-manifest-{username}"));
        let absent_owner_path = root.join(format!("package-absent-owner-{username}"));
        fs::write(&asset_path, &asset_bytes).unwrap();
        fs::write(&inlay_path, &inlay_bytes).unwrap();
        fs::write(&cold_path, &cold_bytes).unwrap();
        fs::write(&owner_manifest_path, &owner_manifest_bytes).unwrap();
        fs::write(&absent_owner_path, []).unwrap();
        let metadata = payload_metadata(username);
        let present_owner = AssetOwnerHead::present(
            AssetOwnerLocator::RootModuleAssets { index: 0 },
            hex::encode(Sha256::digest(&owner_manifest_bytes)),
            1,
        );
        let absent_owner =
            AssetOwnerHead::absent(AssetOwnerLocator::PersonaEmbeddedModuleAssets { index: 0 });
        let entries = vec![
            LosslessWriteEntry {
                logical_path: DATABASE_PATH.to_owned(),
                logical_key: None,
                kind: PayloadKind::Database,
                metadata: json!({}),
                source: database_path.clone(),
            },
            LosslessWriteEntry {
                logical_path: "assets/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Asset,
                metadata: metadata.0.clone(),
                source: asset_path,
            },
            LosslessWriteEntry {
                logical_path: "inlays/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Inlay,
                metadata: metadata.1.clone(),
                source: inlay_path,
            },
            LosslessWriteEntry {
                logical_path: "cold/shared".to_owned(),
                logical_key: Some("shared".to_owned()),
                kind: PayloadKind::Cold,
                metadata: metadata.2.clone(),
                source: cold_path.clone(),
            },
            LosslessWriteEntry {
                logical_path: backup_logical_path(
                    PayloadKind::OwnerManifest,
                    &owner_head_logical_key(&present_owner.owner),
                ),
                logical_key: Some(owner_head_logical_key(&present_owner.owner)),
                kind: PayloadKind::OwnerManifest,
                metadata: serde_json::to_value(&present_owner).unwrap(),
                source: owner_manifest_path,
            },
            LosslessWriteEntry {
                logical_path: backup_logical_path(
                    PayloadKind::OwnerManifest,
                    &owner_head_logical_key(&absent_owner.owner),
                ),
                logical_key: Some(owner_head_logical_key(&absent_owner.owner)),
                kind: PayloadKind::OwnerManifest,
                metadata: serde_json::to_value(&absent_owner).unwrap(),
                source: absent_owner_path,
            },
        ];
        let payloads = vec![
            test_f0_payload(F0PayloadKind::Asset, &asset_bytes, metadata.0, None),
            test_f0_payload(F0PayloadKind::Inlay, &inlay_bytes, metadata.1, None),
            test_f0_payload(
                F0PayloadKind::Cold,
                &cold_bytes,
                metadata.2,
                Some(cold_path),
            ),
        ];
        let validation = validate_f0_v1(&database, &payloads, &[]).unwrap();
        let mut package = Vec::new();
        write_lossless_package_v1(
            &mut package,
            &entries,
            LosslessCompatibility {
                oracle_version: 1,
                canonical_database_sha256: validation.canonical_database_sha256,
                reference_graph_sha256: validation.reference_graph_sha256,
            },
            validation
                .references
                .iter()
                .map(lossless_reference)
                .collect(),
            vec![],
            extensions,
            &NeverCancelled,
        )
        .unwrap();
        source_store
            .cleanup_risu_save_export(&database_path)
            .unwrap();
        source_store.release_revision(&lease).unwrap();
        package
    }

    fn seed_active_store(
        store: &mut PersistentStore,
        cas: &PayloadCas,
        username: &str,
        payload_prefix: &[u8],
    ) {
        let owner_history = [payload_prefix, b"-owner-history"].concat();
        seed_active_store_with_owner_history(store, cas, username, payload_prefix, &owner_history);
    }

    fn seed_active_store_with_owner_history(
        store: &mut PersistentStore,
        cas: &PayloadCas,
        username: &str,
        payload_prefix: &[u8],
        owner_history: &[u8],
    ) {
        let asset_bytes = [payload_prefix, b"-asset"].concat();
        seed_active_store_with_asset_bytes(
            store,
            cas,
            username,
            payload_prefix,
            &asset_bytes,
            owner_history,
        );
    }

    fn seed_active_store_with_asset_bytes(
        store: &mut PersistentStore,
        cas: &PayloadCas,
        username: &str,
        payload_prefix: &[u8],
        asset_bytes: &[u8],
        owner_history: &[u8],
    ) {
        let inlay_bytes = [payload_prefix, b"-inlay"].concat();
        let cold_bytes = cold_payload(&json!({
            "message": [{ "data": format!("{}-cold", username) }]
        }));
        let asset = cas.prepare_bytes(asset_bytes).unwrap();
        let inlay = cas.prepare_bytes(&inlay_bytes).unwrap();
        let cold = cas.prepare_bytes(&cold_bytes).unwrap();
        let historical = cas.prepare_bytes(owner_history).unwrap();
        let owner_manifest_bytes = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: ["shared".to_owned(), "shared".to_owned(), "BIN".to_owned()],
            payload_hash: Some(
                hex::decode(historical.content_hash)
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
        }])
        .unwrap();
        let owner_manifest = cas.prepare_bytes(&owner_manifest_bytes).unwrap();
        let metadata = payload_metadata(username);
        let staging = store.replace_begin().unwrap().staging_id;
        put_f0_database(store, &staging, username);
        store
            .replace_put_asset_aliases(
                &staging,
                &[
                    AssetAlias {
                        key: "shared".to_owned(),
                        object_hash: Some(asset.content_hash),
                        kind: "asset".to_owned(),
                        size: asset_bytes.len() as i64,
                        mime: "application/octet-stream".to_owned(),
                        name: "shared.bin".to_owned(),
                        ext: "bin".to_owned(),
                        inlay_type: None,
                        width: None,
                        height: None,
                        metadata: metadata.0,
                    },
                    AssetAlias {
                        key: "shared".to_owned(),
                        object_hash: Some(inlay.content_hash),
                        kind: "inlay".to_owned(),
                        size: inlay_bytes.len() as i64,
                        mime: "image/webp".to_owned(),
                        name: "shared.webp".to_owned(),
                        ext: "webp".to_owned(),
                        inlay_type: Some("image".to_owned()),
                        width: Some(7),
                        height: Some(9),
                        metadata: metadata.1,
                    },
                ],
            )
            .unwrap();
        store
            .replace_put_cold_aliases(
                &staging,
                &[ColdAlias {
                    key: "shared".to_owned(),
                    object_hash: Some(cold.content_hash),
                    size: cold_bytes.len() as i64,
                    metadata: json!({
                        "source": "legacy-cold",
                        "ordinal": 7,
                        "risuNestAliasMetadata": "user collision",
                        "nested": { "label": format!("{username}-cold") }
                    }),
                }],
            )
            .unwrap();
        store
            .replace_put_asset_owner_heads(
                &staging,
                &[
                    AssetOwnerHead::present(
                        AssetOwnerLocator::RootModuleAssets { index: 0 },
                        owner_manifest.content_hash,
                        1,
                    ),
                    AssetOwnerHead::absent(AssetOwnerLocator::PersonaEmbeddedModuleAssets {
                        index: 0,
                    }),
                ],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "lossless-test-migration".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        store
            .replace_put_cold_payload_authority(
                &staging,
                &ColdPayloadAuthorityState::V2 {
                    migration_id: "lossless-test-cold-migration".to_owned(),
                    compatibility_hash: "cd".repeat(32),
                },
            )
            .unwrap();
        store.replace_commit(&staging, Some(0)).unwrap();
    }

    fn stage_f0_database(store: &mut PersistentStore, username: &str) {
        let staging = store.replace_begin().unwrap().staging_id;
        put_f0_database(store, &staging, username);
        store.replace_commit(&staging, Some(0)).unwrap();
    }

    fn put_f0_database(store: &mut PersistentStore, staging: &str, username: &str) {
        store
            .replace_put_root(
                staging,
                &json!({
                    "username": username,
                    "botPresetsId": 0,
                    "personas": [{
                        "id": "persona",
                        "embeddedModule": { "id": "embedded", "name": "Embedded" }
                    }],
                    "selectedPersona": 0,
                    "enabledModules": [],
                    "characterOrder": [],
                    "userIcon": "shared",
                    "modules": [{
                        "id": "module",
                        "name": "Module",
                        "description": "",
                        "assets": [["shared", "shared", "BIN"]]
                    }],
                    "loadouts": [],
                    "plugins": [],
                    "pluginCustomStorage": {
                        "fixture": { "username": username }
                    }
                }),
            )
            .unwrap();
        store
            .replace_put_presets(staging, &[json!({ "name": "preset" })])
            .unwrap();
        store
            .replace_add_characters(
                staging,
                &[json!({
                    "chaId": "character",
                    "type": "character",
                    "name": username,
                    "image": "",
                    "desc": "{{inlay::shared}}",
                    "coldstorage": "shared",
                    "chats": [],
                    "chatPage": 0
                })],
            )
            .unwrap();
    }

    fn payload_metadata(username: &str) -> (Value, Value, Value) {
        (
            json!({
                "name": "shared.bin",
                "ext": "bin",
                "mime": "application/octet-stream",
                "nested": { "label": format!("{username}-asset") }
            }),
            json!({
                "name": "shared.webp",
                "ext": "webp",
                "mime": "image/webp",
                "inlayType": "image",
                "width": 7,
                "height": 9,
                "nested": { "label": format!("{username}-inlay") }
            }),
            json!({
                "name": "shared.json.zlib",
                "ext": "zlib",
                "mime": "application/octet-stream",
                "risuNestAliasMetadata": "user collision",
                "nested": { "label": format!("{username}-cold") }
            }),
        )
    }

    fn test_f0_payload(
        kind: F0PayloadKind,
        bytes: &[u8],
        metadata: Value,
        cold_source: Option<PathBuf>,
    ) -> F0PayloadDescriptor {
        F0PayloadDescriptor {
            kind,
            key: "shared".to_owned(),
            sha256: hex::encode(Sha256::digest(bytes)),
            byte_length: bytes.len() as u64,
            metadata,
            cold_source,
        }
    }

    fn cold_payload(value: &Value) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        serde_json::to_writer(&mut encoder, value).unwrap();
        encoder.finish().unwrap()
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

    fn rewrite_manifest(package: &mut [u8], mutate: impl FnOnce(&mut LosslessManifest)) {
        let manifest_length_offset = MAGIC.len() + std::mem::size_of::<u32>();
        let manifest_hash_offset = manifest_length_offset + std::mem::size_of::<u64>();
        let manifest_offset = manifest_hash_offset + 32;
        let manifest_length = u64::from_le_bytes(
            package[manifest_length_offset..manifest_hash_offset]
                .try_into()
                .unwrap(),
        ) as usize;
        let manifest_end = manifest_offset + manifest_length;
        let mut manifest: LosslessManifest =
            serde_json::from_slice(&package[manifest_offset..manifest_end]).unwrap();
        mutate(&mut manifest);
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        assert_eq!(manifest_bytes.len(), manifest_length);
        package[manifest_hash_offset..manifest_offset]
            .copy_from_slice(&Sha256::digest(&manifest_bytes));
        package[manifest_offset..manifest_end].copy_from_slice(&manifest_bytes);
    }
}
