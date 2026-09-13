pub(crate) mod asset_object_catalog;
pub(crate) mod asset_residency;
pub(crate) mod commands;
mod commit;
pub(crate) mod export;
#[cfg(feature = "native-kei-upload-pilot")]
pub(crate) mod kei;
pub(crate) mod owner_projection;
pub(crate) mod portable;
pub(crate) mod portable_validation;
mod preservation;
mod query;
mod record_apply;
mod record_projection;
mod schema;
#[cfg(test)]
mod schema_contract_tests;
pub(crate) mod server_sync_apply;
#[path = "../server_sync/engine.rs"]
pub(crate) mod server_sync_engine;
pub(crate) mod server_sync_journal;
pub(crate) mod server_sync_outbox;
pub(crate) mod server_sync_projection;
mod snapshot;
mod snapshot_archive;

pub(crate) use asset_object_catalog::{AssetObjectCatalog, AssetObjectCatalogPage};
pub(crate) use commands::PersistentStoreState;
pub(crate) use snapshot::RevisionReadLease;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "native-official-publication")]
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

pub(super) type StoreResult<T> = Result<T, StoreError>;

pub(super) const CONVERSATION_RANGE_MAX_LIMIT: i64 = 4_096;
pub(super) const JAVASCRIPT_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
pub(crate) const ASSET_GC_PRODUCT_PAGE_LIMIT: i64 = 128;
const ASSET_GC_PRODUCT_MINIMUM_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

// Add every generation-scoped record family here so staged moves and cleanup cannot omit it.
pub(super) const GENERATION_TABLES: &[(&str, &str)] = &[
    ("root", "value"),
    (
        "bot_presets",
        "preset_id, configured_index, name, image, value",
    ),
    (
        "characters",
        "character_id, configured_index, recent_at, trashed, name, image, conversation_count, type, creator_notes, trash_time, detail",
    ),
    (
        "conversations",
        "character_id, conversation_id, configured_index, recent_at, name, message_count, detail",
    ),
    (
        "messages",
        "character_id, conversation_id, message_index, message_id, value",
    ),
    (
        "plugin_storage",
        "storage_key, byte_size, ordinal, value",
    ),
    (
        "asset_aliases",
        "logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata",
    ),
    (
        "asset_alias_replacement_candidates",
        "kind, logical_key, object_hash, byte_size",
    ),
    (
        "asset_owner_heads",
        "owner_kind, owner_locator, present, manifest_hash, entry_count",
    ),
    ("asset_repository_authority", "value"),
    ("cold_payload_authority", "value"),
    ("cold_aliases", "key, object_hash, size, metadata"),
];

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "kebab-case")]
pub(crate) enum StoreError {
    RevisionConflict {
        expected: i64,
        actual: i64,
    },
    SnapshotReleased,
    Validation {
        message: String,
    },
    #[serde(rename = "store-error")]
    Store {
        message: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "expected data revision {expected}, but current revision is {actual}"
                )
            }
            Self::SnapshotReleased => {
                formatter.write_str("persistent revision snapshot has been released")
            }
            Self::Validation { message } | Self::Store { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Versioned<T> {
    pub(crate) revision: i64,
    pub(crate) value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevisionResult {
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QueryOrder {
    Configured,
    Recent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) search: Option<String>,
    pub(crate) order: QueryOrder,
    pub(crate) trash: bool,
    pub(crate) limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image: Option<String>,
    pub(crate) configured_index: i64,
    pub(crate) recent_at: i64,
    pub(crate) trashed: bool,
    pub(crate) conversation_count: i64,
    pub(crate) r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) creator_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trash_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image: Option<String>,
    pub(crate) configured_index: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetCatalog {
    pub(crate) revision: i64,
    pub(crate) items: Vec<PresetSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageSummary {
    pub(crate) key: String,
    pub(crate) byte_size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageCatalog {
    pub(crate) revision: i64,
    pub(crate) items: Vec<PluginStorageSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetAliasListQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    pub(crate) limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetAliasPage {
    pub(crate) revision: i64,
    pub(crate) items: Vec<AssetAlias>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "format",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum AssetRepositoryAuthorityState {
    Legacy,
    Preparing {
        migration_id: String,
        source_revision: i64,
    },
    #[serde(rename = "v2")]
    V2 {
        migration_id: String,
        compatibility_hash: String,
    },
}

impl AssetRepositoryAuthorityState {
    pub(crate) fn validate(&self) -> StoreResult<()> {
        match self {
            Self::Legacy => Ok(()),
            Self::Preparing {
                migration_id,
                source_revision,
            } => validate_authority_fields(
                "Asset repository",
                migration_id,
                Some(*source_revision),
                None,
            ),
            Self::V2 {
                migration_id,
                compatibility_hash,
            } => validate_authority_fields(
                "Asset repository",
                migration_id,
                None,
                Some(compatibility_hash),
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "format",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum ColdPayloadAuthorityState {
    Legacy,
    Preparing {
        migration_id: String,
        source_revision: i64,
    },
    #[serde(rename = "v2")]
    V2 {
        migration_id: String,
        compatibility_hash: String,
    },
}

impl ColdPayloadAuthorityState {
    pub(crate) fn validate(&self) -> StoreResult<()> {
        match self {
            Self::Legacy => Ok(()),
            Self::Preparing {
                migration_id,
                source_revision,
            } => validate_authority_fields(
                "Cold payload",
                migration_id,
                Some(*source_revision),
                None,
            ),
            Self::V2 {
                migration_id,
                compatibility_hash,
            } => validate_authority_fields(
                "Cold payload",
                migration_id,
                None,
                Some(compatibility_hash),
            ),
        }
    }
}

// Shared field validation for the two structurally identical authority-state
// enums so their rules cannot drift; the enums themselves stay distinct for
// type safety between the two authorities.
fn validate_authority_fields(
    subject: &str,
    migration_id: &str,
    source_revision: Option<i64>,
    compatibility_hash: Option<&str>,
) -> StoreResult<()> {
    if let Some(source_revision) = source_revision {
        if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(&source_revision) {
            return Err(StoreError::Validation {
                message: format!("{subject} sourceRevision is invalid"),
            });
        }
    }
    if let Some(compatibility_hash) = compatibility_hash {
        validate_hash(compatibility_hash, &format!("{subject} compatibilityHash"))?;
    }
    if migration_id.is_empty()
        || migration_id.len() > 64
        || !migration_id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-'))
    {
        return Err(StoreError::Validation {
            message: format!("{subject} migrationId is invalid"),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetAlias {
    pub(crate) key: String,
    pub(crate) object_hash: Option<String>,
    pub(crate) kind: String,
    pub(crate) size: i64,
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) ext: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inlay_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<i64>,
    #[serde(default = "empty_alias_metadata")]
    pub(crate) metadata: Value,
}

impl AssetAlias {
    pub(super) fn validate(&self) -> StoreResult<()> {
        validate_object_hash(&self.object_hash, "Asset alias")?;
        if !matches!(self.kind.as_str(), "asset" | "inlay") {
            return Err(StoreError::Validation {
                message: "Asset alias kind must be asset or inlay".to_owned(),
            });
        }
        match self.kind.as_str() {
            "inlay" if self.inlay_type.is_none() => {
                return Err(StoreError::Validation {
                    message: "Asset alias inlayType is required and must be valid".to_owned(),
                });
            }
            "asset"
                if self.inlay_type.is_some() || self.width.is_some() || self.height.is_some() =>
            {
                return Err(StoreError::Validation {
                    message: "Asset alias Inlay metadata is forbidden for ordinary assets"
                        .to_owned(),
                });
            }
            _ => {}
        }
        if self.size < 0 {
            return Err(StoreError::Validation {
                message: "Asset alias size must be nonnegative".to_owned(),
            });
        }
        if let Some(inlay_type) = &self.inlay_type {
            if !matches!(
                inlay_type.as_str(),
                "image" | "video" | "audio" | "signature"
            ) {
                return Err(StoreError::Validation {
                    message: "Asset alias inlayType is invalid".to_owned(),
                });
            }
        }
        if self.width.is_some_and(|value| value < 0) {
            return Err(StoreError::Validation {
                message: "Asset alias width must be nonnegative".to_owned(),
            });
        }
        if self.height.is_some_and(|value| value < 0) {
            return Err(StoreError::Validation {
                message: "Asset alias height must be nonnegative".to_owned(),
            });
        }
        if !self.metadata.is_object() {
            return Err(StoreError::Validation {
                message: "Asset alias metadata must be a JSON object".to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum AssetOwnerLocator {
    CharacterAdditionalAssets { character_id: String },
    RootModuleAssets { index: i64 },
    PersonaEmbeddedModuleAssets { index: i64 },
}

impl AssetOwnerLocator {
    fn validate(&self) -> StoreResult<()> {
        match self {
            Self::CharacterAdditionalAssets { character_id } if character_id.is_empty() => {
                Err(StoreError::Validation {
                    message: "Character asset owner requires a nonempty characterId".to_owned(),
                })
            }
            Self::RootModuleAssets { index } | Self::PersonaEmbeddedModuleAssets { index }
                if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(index) =>
            {
                Err(StoreError::Validation {
                    message: "Asset owner occurrence index must be a nonnegative safe integer"
                        .to_owned(),
                })
            }
            _ => Ok(()),
        }
    }

    fn storage_identity(&self) -> (&'static str, String) {
        match self {
            Self::CharacterAdditionalAssets { character_id } => {
                ("character-additional-assets", character_id.clone())
            }
            Self::RootModuleAssets { index } => ("root-module-assets", index.to_string()),
            Self::PersonaEmbeddedModuleAssets { index } => {
                ("persona-embedded-module-assets", index.to_string())
            }
        }
    }
}

fn empty_alias_metadata() -> Value {
    Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetOwnerHead {
    pub(crate) owner: AssetOwnerLocator,
    pub(crate) present: bool,
    pub(crate) manifest_hash: Option<String>,
    pub(crate) entry_count: i64,
}

impl AssetOwnerHead {
    #[cfg(test)]
    pub(crate) fn present(
        owner: AssetOwnerLocator,
        manifest_hash: String,
        entry_count: i64,
    ) -> Self {
        Self {
            owner,
            present: true,
            manifest_hash: Some(manifest_hash),
            entry_count,
        }
    }

    #[cfg(test)]
    pub(crate) fn absent(owner: AssetOwnerLocator) -> Self {
        Self {
            owner,
            present: false,
            manifest_hash: None,
            entry_count: 0,
        }
    }

    pub(super) fn validate(&self) -> StoreResult<()> {
        self.owner.validate()?;
        if self.entry_count < 0 {
            return Err(StoreError::Validation {
                message: "Asset owner head entryCount must be nonnegative".to_owned(),
            });
        }
        if !self.present {
            if self.manifest_hash.is_some() || self.entry_count != 0 {
                return Err(StoreError::Validation {
                    message: "Absent asset owner property cannot reference a manifest".to_owned(),
                });
            }
            return Ok(());
        }
        let Some(hash) = &self.manifest_hash else {
            return Err(StoreError::Validation {
                message: "Present asset owner property requires a lowercase SHA-256 manifestHash"
                    .to_owned(),
            });
        };
        if !is_lowercase_sha256_hex(hash) {
            return Err(StoreError::Validation {
                message: "Present asset owner property requires a lowercase SHA-256 manifestHash"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ColdAlias {
    pub(crate) key: String,
    pub(crate) object_hash: Option<String>,
    pub(crate) size: i64,
    pub(crate) metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ColdPayloadMigrationInput {
    pub(crate) source_revision: i64,
    pub(crate) migration_id: String,
    pub(crate) compatibility_hash: String,
    pub(crate) cold_aliases: Vec<ColdAlias>,
}

impl ColdPayloadMigrationInput {
    fn authority(&self) -> ColdPayloadAuthorityState {
        ColdPayloadAuthorityState::V2 {
            migration_id: self.migration_id.clone(),
            compatibility_hash: self.compatibility_hash.clone(),
        }
    }
}

impl ColdAlias {
    pub(super) fn validate(&self) -> StoreResult<()> {
        if self.key.is_empty() || self.key.contains('\0') {
            return Err(StoreError::Validation {
                message: "Cold alias key must be nonempty and contain no NUL characters".to_owned(),
            });
        }
        validate_object_hash(&self.object_hash, "Cold alias")?;
        if self.size < 0 {
            return Err(StoreError::Validation {
                message: "Cold alias size must be nonnegative".to_owned(),
            });
        }
        if !self.metadata.is_object() {
            return Err(StoreError::Validation {
                message: "Cold alias metadata must be a JSON object".to_owned(),
            });
        }
        Ok(())
    }
}

pub(crate) use crate::trust_boundary::is_lower_hex_256 as is_lowercase_sha256_hex;

fn validate_object_hash(hash: &Option<String>, subject: &str) -> StoreResult<()> {
    if hash
        .as_ref()
        .is_some_and(|hash| !is_lowercase_sha256_hex(hash))
    {
        return Err(StoreError::Validation {
            message: format!(
                "{subject} objectHash must be null or 64 lowercase hexadecimal characters"
            ),
        });
    }
    Ok(())
}

fn validate_hash(hash: &str, subject: &str) -> StoreResult<()> {
    if !is_lowercase_sha256_hex(hash) {
        return Err(StoreError::Validation {
            message: format!("{subject} must be 64 lowercase hexadecimal characters"),
        });
    }
    Ok(())
}

fn verify_cold_alias_object(
    cas: &crate::asset_repository::PayloadCas,
    alias: &ColdAlias,
) -> StoreResult<()> {
    alias.validate()?;
    let hash = alias
        .object_hash
        .as_deref()
        .ok_or_else(|| StoreError::Validation {
            message: "Cold payload v2 alias requires an objectHash".to_owned(),
        })?;
    let actual_size = cas
        .stat_object(hash)?
        .ok_or_else(|| StoreError::Validation {
            message: format!("Cold payload CAS object {hash} is missing"),
        })?;
    if actual_size != alias.size as u64 {
        return Err(StoreError::Validation {
            message: format!(
                "Cold payload alias size {} does not match CAS size {actual_size}",
                alias.size
            ),
        });
    }
    Ok(())
}

fn plugin_storage_array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    (value < u32::MAX && value.to_string() == key).then_some(value)
}

pub(super) fn compare_plugin_storage_keys(
    left_key: &str,
    left_ordinal: i64,
    right_key: &str,
    right_ordinal: i64,
) -> Ordering {
    match (
        plugin_storage_array_index(left_key),
        plugin_storage_array_index(right_key),
    ) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left_ordinal
            .cmp(&right_ordinal)
            .then_with(|| left_key.cmp(right_key)),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterPage {
    pub(crate) revision: i64,
    pub(crate) items: Vec<CharacterSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationQuery {
    pub(crate) character_id: String,
    pub(crate) order: QueryOrder,
    pub(crate) limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationSummary {
    pub(crate) id: String,
    pub(crate) character_id: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) folder_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) binded_persona: Option<String>,
    pub(crate) configured_index: i64,
    pub(crate) recent_at: i64,
    pub(crate) message_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fm_index: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationPage {
    pub(crate) revision: i64,
    pub(crate) items: Vec<ConversationSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistentConversationMetadata {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    pub(crate) conversation: Value,
    pub(crate) total_messages: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AnchorOccurrence {
    First,
    Last,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationWindowQuery {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) start_index: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor_occurrence: Option<AnchorOccurrence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) after: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationWindow {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    pub(crate) messages: Vec<Value>,
    pub(crate) start_index: i64,
    pub(crate) end_index: i64,
    pub(crate) total_messages: i64,
    pub(crate) has_more_before: bool,
    pub(crate) has_more_after: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ConversationMutation {
    ReplaceRange {
        character_id: String,
        conversation_id: String,
        start: i64,
        delete_count: i64,
        messages: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        configured_index: Option<i64>,
    },
    Delete {
        character_id: String,
        conversation_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum PluginStorageMutation {
    Set { key: String, value: Value },
    Delete { key: String },
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkingSetCommit {
    pub(crate) expected_revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) root: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) root_mutations: Option<Vec<RootMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replace_presets: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) character_details: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replace_character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) add_character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) conversations: Option<Vec<ConversationMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delete_character_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plugin_storage: Option<Vec<PluginStorageMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) asset_owner_heads: Option<Vec<AssetOwnerHead>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum RootMutation {
    Set { key: String, value: Value },
    Delete { key: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LeaseResult {
    pub(crate) lease: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagingResult {
    pub(crate) staging_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotInfo {
    pub(crate) id: String,
    pub(crate) reason: String,
    pub(crate) reclaimable_bytes: u64,
    pub(crate) bytes: u64,
    pub(crate) modified_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotCreated {
    pub(crate) id: String,
    pub(crate) revision: i64,
    pub(crate) bytes: u64,
    pub(crate) duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CheckpointMode {
    Passive,
    Truncate,
}

#[derive(Clone)]
pub(super) struct ReadTarget {
    pub(super) revision: i64,
    pub(super) generation: String,
}

pub(crate) struct PersistentStore {
    revision_leases: HashMap<String, snapshot::RevisionReadLease>,
    active_readers: Arc<snapshot::ActiveReaderRegistry>,
    connection: Connection,
    repository_root: PathBuf,
    database_path: PathBuf,
    snapshots_dir: PathBuf,
    pending_restore_failure: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageCountBytes {
    pub(crate) count: u64,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageCharacterStats {
    pub(crate) active: StorageCountBytes,
    pub(crate) trashed_count: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageConversationStats {
    pub(crate) count: u64,
    pub(crate) message_count: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageAliasStats {
    pub(crate) kind: String,
    pub(crate) inlay_type: Option<String>,
    pub(crate) count: u64,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageDeletionStats {
    pub(crate) state: String,
    pub(crate) count: u64,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersistentStorageStats {
    pub(crate) snapshot_bytes: u64,
    pub(crate) database_bytes: u64,
    pub(crate) asset_objects: StorageCountBytes,
    pub(crate) asset_aliases: Vec<StorageAliasStats>,
    pub(crate) cold_aliases: StorageCountBytes,
    pub(crate) plugin_storage: StorageCountBytes,
    pub(crate) characters: StorageCharacterStats,
    pub(crate) conversations: StorageConversationStats,
    pub(crate) asset_object_deletions: Vec<StorageDeletionStats>,
}

pub(crate) struct PreparedReplaceCommit {
    staging_id: String,
    revision: i64,
}

pub(crate) struct PreparedRisuSaveExport {
    pub(crate) revision: i64,
    pub(crate) lease: String,
    pub(crate) snapshots_dir: PathBuf,
    database_path: PathBuf,
    reader: Option<RevisionReadLease>,
}

#[cfg(feature = "native-official-publication")]
pub(crate) struct PreparedOfficialPublication {
    pub(crate) revision: i64,
    lease: String,
    snapshots_dir: PathBuf,
    database_path: PathBuf,
    reader: Option<RevisionReadLease>,
}

#[cfg(feature = "native-official-publication")]
#[derive(Debug)]
pub(crate) struct OfficialPublicationPayload {
    path: PathBuf,
    snapshots_dir: PathBuf,
    lease: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    armed: bool,
}

impl PreparedRisuSaveExport {
    pub(crate) fn repository_root(&self) -> StoreResult<&Path> {
        self.snapshots_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| StoreError::Validation {
                message: "native export repository root is unavailable".to_owned(),
            })
    }

    pub(crate) fn take_reader(&mut self) -> StoreResult<RevisionReadLease> {
        self.reader.take().ok_or_else(|| StoreError::Validation {
            message: "native export reader has already been taken".to_owned(),
        })
    }

    pub(crate) fn reader(&self) -> StoreResult<&RevisionReadLease> {
        self.reader.as_ref().ok_or_else(|| StoreError::Validation {
            message: "native export reader has already been released".to_owned(),
        })
    }

    pub(crate) fn release_reader(&mut self) -> StoreResult<()> {
        let reader = self.take_reader()?;
        self.release(reader)
    }

    pub(crate) fn release(&self, reader: RevisionReadLease) -> StoreResult<()> {
        let active_readers = reader.active_readers();
        snapshot::close_revision(reader)?;
        checkpoint_after_detached_release(&self.database_path, &active_readers)
    }

    pub(crate) fn cleanup_file(&self, path: &Path) -> StoreResult<()> {
        export::cleanup(&self.snapshots_dir, path)
    }
}

#[cfg(feature = "native-official-publication")]
impl PreparedOfficialPublication {
    pub(crate) fn create_payload(
        mut self,
        expected_account_id: &str,
        replacements: &HashMap<String, String>,
        is_cancelled: impl Fn() -> bool,
        on_progress: impl FnMut(u64, u64, u64),
    ) -> StoreResult<OfficialPublicationPayload> {
        let reader = self.reader.as_ref().ok_or(StoreError::SnapshotReleased)?;
        let exported = export::create_projected_controlled_for_account(
            &reader.connection,
            &self.snapshots_dir,
            &reader.target,
            &self.lease,
            expected_account_id,
            replacements,
            &is_cancelled,
            on_progress,
        );
        let exported = match exported {
            Ok(exported) => exported,
            Err(error) => {
                return Err(combine_publication_cleanup_error(
                    error,
                    self.release_reader(),
                    "reader release",
                ));
            }
        };
        let path = PathBuf::from(&exported.path);
        if let Err(error) = self.release_reader() {
            return Err(combine_publication_cleanup_error(
                error,
                export::cleanup(&self.snapshots_dir, &path),
                "payload cleanup",
            ));
        }
        let sha256 = match hash_exact_file(&path, &is_cancelled) {
            Ok(hash) => hash,
            Err(error) => {
                return Err(combine_publication_cleanup_error(
                    error,
                    export::cleanup(&self.snapshots_dir, &path),
                    "payload cleanup",
                ));
            }
        };
        Ok(OfficialPublicationPayload {
            path,
            snapshots_dir: self.snapshots_dir.clone(),
            lease: self.lease.clone(),
            bytes: exported.bytes,
            sha256,
            character_count: exported.character_count,
            preset_count: exported.preset_count,
            armed: true,
        })
    }

    fn release_reader(&mut self) -> StoreResult<()> {
        let Some(reader) = self.reader.take() else {
            return Ok(());
        };
        let active_readers = reader.active_readers();
        snapshot::close_revision(reader)?;
        checkpoint_after_detached_release(&self.database_path, &active_readers)
    }
}

#[cfg(feature = "native-official-publication")]
impl Drop for PreparedOfficialPublication {
    fn drop(&mut self) {
        let _ = self.release_reader();
    }
}

#[cfg(feature = "native-official-publication")]
impl OfficialPublicationPayload {
    pub(crate) fn open(&self) -> StoreResult<(std::fs::File, u64)> {
        export::open_owned_for_upload(&self.snapshots_dir, &self.path, &self.lease)
    }

    pub(crate) fn cleanup(mut self) -> StoreResult<()> {
        export::cleanup(&self.snapshots_dir, &self.path)?;
        self.armed = false;
        Ok(())
    }
}

#[cfg(feature = "native-official-publication")]
impl Drop for OfficialPublicationPayload {
    fn drop(&mut self) {
        if self.armed {
            let _ = export::cleanup(&self.snapshots_dir, &self.path);
        }
    }
}

#[cfg(feature = "native-official-publication")]
fn hash_exact_file(path: &Path, is_cancelled: &impl Fn() -> bool) -> StoreResult<String> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    loop {
        if is_cancelled() {
            return Err(StoreError::Validation {
                message: export::EXPORT_CANCELLED_MESSAGE.to_owned(),
            });
        }
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(feature = "native-official-publication")]
fn combine_publication_cleanup_error(
    primary: StoreError,
    cleanup: StoreResult<()>,
    operation: &str,
) -> StoreError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => StoreError::Store {
            message: format!("{primary}; official publication {operation} failed: {cleanup}"),
        },
    }
}

impl PersistentStore {
    pub(crate) fn asset_object_catalog(&mut self) -> AssetObjectCatalog<'_> {
        AssetObjectCatalog::new(&mut self.connection)
    }

    pub(crate) fn query_asset_object_catalog(
        &self,
        limit: i64,
        cursor: Option<&str>,
    ) -> StoreResult<AssetObjectCatalogPage> {
        asset_object_catalog::query(&self.connection, limit, cursor)
    }

    pub(crate) fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    pub(crate) fn open(app_data_dir: &Path) -> StoreResult<Self> {
        let persistent_dir = app_data_dir.join("persistent");
        let snapshots_dir = persistent_dir.join("snapshots");
        std::fs::create_dir_all(&snapshots_dir)?;
        let pending_restore_failure =
            snapshot::apply_pending_restore(&persistent_dir, &snapshots_dir)?;

        let database_path = persistent_dir.join("persistent.db");
        let mut connection = Connection::open(&database_path)?;
        schema::initialize(&mut connection)?;
        recover_asset_object_deletions(&mut connection, app_data_dir)?;

        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params!["currentRevision", "0"],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params!["activeGeneration", "\"revision-0\""],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO root (generation, value) VALUES (?1, ?2)",
            params!["revision-0", "{}"],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO asset_repository_authority (generation, value) VALUES (?1, ?2)",
            params!["revision-0", r#"{"format":"legacy"}"#],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO cold_payload_authority (generation, value) VALUES (?1, ?2)",
            params!["revision-0", r#"{"format":"legacy"}"#],
        )?;
        transaction.commit()?;
        export::sweep_abandoned(&snapshots_dir)?;
        #[cfg(feature = "native-kei-upload-pilot")]
        kei::sweep_abandoned(&snapshots_dir);
        snapshot::sweep_temporary_generations(&mut connection)?;
        snapshot::checkpoint(&connection, CheckpointMode::Truncate)?;

        let active_readers = Arc::new(snapshot::ActiveReaderRegistry::default());
        Ok(Self {
            revision_leases: HashMap::new(),
            active_readers,
            connection,
            repository_root: app_data_dir.to_owned(),
            database_path,
            snapshots_dir,
            pending_restore_failure,
        })
    }

    // Reports a user-requested snapshot restore that was skipped during this
    // open, so command surfaces can tell the frontend instead of silently
    // proceeding on the old database.
    pub(crate) fn pending_restore_failure(&self) -> Option<&str> {
        self.pending_restore_failure.as_deref()
    }

    pub(crate) fn open_native_job_store(&self) -> StoreResult<Self> {
        let mut connection = Connection::open(&self.database_path)?;
        schema::initialize(&mut connection)?;
        Ok(Self {
            revision_leases: HashMap::new(),
            active_readers: Arc::clone(&self.active_readers),
            connection,
            repository_root: self.repository_root.clone(),
            database_path: self.database_path.clone(),
            snapshots_dir: self.snapshots_dir.clone(),
            pending_restore_failure: None,
        })
    }

    pub(crate) fn revision(&self) -> StoreResult<i64> {
        current_revision(&self.connection)
    }

    pub(crate) fn read_root(&self, lease: Option<&str>) -> StoreResult<Versioned<Value>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_root(connection, &target)
    }

    pub(crate) fn query_presets(&self, lease: Option<&str>) -> StoreResult<PresetCatalog> {
        let (connection, target) = self.read_view(lease)?;
        query::query_presets(connection, &target)
    }

    pub(crate) fn read_preset(
        &self,
        id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_preset(connection, id, &target)
    }

    pub(crate) fn query_characters(
        &self,
        query: &CharacterQuery,
        lease: Option<&str>,
    ) -> StoreResult<CharacterPage> {
        let (connection, target) = self.read_view(lease)?;
        query::query_characters(connection, query, &target)
    }

    pub(crate) fn read_character(
        &self,
        id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_character(connection, id, &target)
    }

    pub(crate) fn query_conversations(
        &self,
        query: &ConversationQuery,
        lease: Option<&str>,
    ) -> StoreResult<ConversationPage> {
        let (connection, target) = self.read_view(lease)?;
        query::query_conversations(connection, query, &target)
    }

    pub(crate) fn read_conversation(
        &self,
        character_id: &str,
        conversation_id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_conversation(connection, character_id, conversation_id, &target)
    }

    pub(crate) fn read_conversation_metadata(
        &self,
        character_id: &str,
        conversation_id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<PersistentConversationMetadata>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_conversation_metadata(connection, character_id, conversation_id, &target)
    }

    pub(crate) fn read_conversation_window(
        &self,
        query: &ConversationWindowQuery,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<ConversationWindow>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_conversation_window(connection, query, &target)
    }

    pub(crate) fn query_plugin_storage(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<PluginStorageCatalog> {
        let (connection, target) = self.read_view(lease)?;
        query::query_plugin_storage(connection, &target)
    }

    pub(crate) fn read_plugin_storage(
        &self,
        key: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_plugin_storage(connection, key, &target)
    }

    pub(crate) fn read_asset_alias(
        &self,
        kind: &str,
        key: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<AssetAlias>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_asset_alias(connection, kind, key, &target)
    }

    pub(crate) fn read_asset_aliases_by_keys(
        &self,
        kind: &str,
        keys: &[String],
        lease: Option<&str>,
    ) -> StoreResult<Versioned<Vec<AssetAlias>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_asset_aliases_by_keys(connection, kind, keys, &target)
    }

    pub(crate) fn list_asset_alias_page(
        &self,
        query_input: &AssetAliasListQuery,
        lease: Option<&str>,
    ) -> StoreResult<AssetAliasPage> {
        let (connection, target) = self.read_view(lease)?;
        query::list_asset_alias_page(connection, query_input, &target)
    }

    pub(crate) fn read_asset_repository_authority(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Versioned<AssetRepositoryAuthorityState>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_asset_repository_authority(connection, &target)
    }

    pub(crate) fn read_cold_payload_authority(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Versioned<ColdPayloadAuthorityState>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_cold_payload_authority(connection, &target)
    }

    pub(crate) fn read_asset_owner_head(
        &self,
        owner: &AssetOwnerLocator,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<AssetOwnerHead>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_asset_owner_head(connection, owner, &target)
    }

    pub(crate) fn read_cold_alias(
        &self,
        key: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<ColdAlias>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_cold_alias(connection, key, &target)
    }

    pub(crate) fn list_asset_aliases(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Versioned<Vec<AssetAlias>>> {
        let (connection, target) = self.read_view(lease)?;
        query::list_asset_aliases(connection, &target)
    }

    pub(crate) fn list_asset_owner_heads(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Versioned<Vec<AssetOwnerHead>>> {
        let (connection, target) = self.read_view(lease)?;
        query::list_asset_owner_heads(connection, &target)
    }

    pub(crate) fn list_cold_aliases(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Versioned<Vec<ColdAlias>>> {
        let (connection, target) = self.read_view(lease)?;
        query::list_cold_aliases(connection, &target)
    }

    pub(crate) fn materialize(&self, revision: Option<i64>) -> StoreResult<Value> {
        let (mut database, target) = query::materialize_with_target(&self.connection, revision)?;
        owner_projection::OwnerManifestProjector::from_snapshots_dir(
            &self.connection,
            &target,
            &self.snapshots_dir,
        )?
        .project_database(&mut database)?;
        Ok(database)
    }

    pub(crate) fn materialize_lease(&self, lease: &str) -> StoreResult<Value> {
        let (connection, target) = self.read_view(Some(lease))?;
        let mut database = query::materialize_target(connection, &target)?;
        owner_projection::OwnerManifestProjector::from_snapshots_dir(
            connection,
            &target,
            &self.snapshots_dir,
        )?
        .project_database(&mut database)?;
        Ok(database)
    }

    pub(crate) fn materialize_staging(&self, staging_id: &str) -> StoreResult<Value> {
        query::materialize_staging(&self.connection, staging_id)
    }

    pub(crate) fn commit(&mut self, commit: &WorkingSetCommit) -> StoreResult<RevisionResult> {
        self.commit_with_asset_aliases(commit, &[])
    }

    pub(crate) fn commit_with_asset_aliases(
        &mut self,
        commit: &WorkingSetCommit,
        asset_aliases: &[AssetAlias],
    ) -> StoreResult<RevisionResult> {
        let resolved;
        let commit = if let Some(mutations) = &commit.root_mutations {
            if commit.root.is_some() {
                return Err(StoreError::Validation {
                    message: "Root and rootMutations are mutually exclusive".to_owned(),
                });
            }
            let current = self.read_root(None)?;
            if current.revision != commit.expected_revision {
                return Err(StoreError::RevisionConflict {
                    expected: commit.expected_revision,
                    actual: current.revision,
                });
            }
            resolved = WorkingSetCommit {
                root: Some(commit::apply_root_mutations(current.value, mutations)?),
                root_mutations: None,
                ..commit.clone()
            };
            &resolved
        } else {
            commit
        };
        commit::commit(&mut self.connection, commit, asset_aliases)
    }

    pub(crate) fn commit_asset_alias(
        &mut self,
        alias: &AssetAlias,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        commit::commit_asset_alias(&mut self.connection, alias, expected_revision)
    }

    pub(crate) fn delete_asset_alias(
        &mut self,
        kind: &str,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        commit::delete_asset_alias(&mut self.connection, kind, key, expected_revision)
    }

    pub(crate) fn commit_cold_alias(
        &mut self,
        alias: &ColdAlias,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        verify_cold_alias_object(&cas, alias)?;
        commit::commit_cold_alias(&mut self.connection, alias, expected_revision)
    }

    pub(crate) fn delete_cold_alias(
        &mut self,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        commit::delete_cold_alias(&mut self.connection, key, expected_revision)
    }

    pub(crate) fn activate_cold_payload_migration(
        &mut self,
        input: &ColdPayloadMigrationInput,
    ) -> StoreResult<RevisionResult> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        for alias in &input.cold_aliases {
            verify_cold_alias_object(&cas, alias)?;
        }
        commit::activate_cold_payload_migration(&mut self.connection, input)
    }

    pub(crate) fn replace_begin(&mut self) -> StoreResult<StagingResult> {
        commit::replace_begin(&mut self.connection)
    }

    pub(crate) fn replace_put_root(&mut self, staging_id: &str, root: &Value) -> StoreResult<()> {
        commit::replace_put_root(&mut self.connection, staging_id, root)
    }

    pub(crate) fn replace_put_presets(
        &mut self,
        staging_id: &str,
        presets: &[Value],
    ) -> StoreResult<()> {
        commit::replace_put_presets(&mut self.connection, staging_id, presets)
    }

    pub(crate) fn replace_put_asset_aliases(
        &mut self,
        staging_id: &str,
        aliases: &[AssetAlias],
    ) -> StoreResult<()> {
        commit::replace_put_asset_aliases(&mut self.connection, staging_id, aliases)
    }

    pub(crate) fn replace_put_asset_owner_heads(
        &mut self,
        staging_id: &str,
        heads: &[AssetOwnerHead],
    ) -> StoreResult<()> {
        commit::replace_put_asset_owner_heads(&mut self.connection, staging_id, heads)
    }

    pub(crate) fn replace_put_asset_repository_authority(
        &mut self,
        staging_id: &str,
        authority: &AssetRepositoryAuthorityState,
    ) -> StoreResult<()> {
        commit::replace_put_asset_repository_authority(&mut self.connection, staging_id, authority)
    }

    pub(crate) fn replace_put_cold_payload_authority(
        &mut self,
        staging_id: &str,
        authority: &ColdPayloadAuthorityState,
    ) -> StoreResult<()> {
        commit::replace_put_cold_payload_authority(&mut self.connection, staging_id, authority)
    }

    pub(crate) fn replace_preserve_repositories(
        &mut self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<RevisionResult> {
        let revision = commit::replace_preserve_repositories(
            &mut self.connection,
            staging_id,
            expected_revision,
        )?;
        Ok(RevisionResult { revision })
    }

    pub(crate) fn replace_put_cold_aliases(
        &mut self,
        staging_id: &str,
        aliases: &[ColdAlias],
    ) -> StoreResult<()> {
        commit::replace_put_cold_aliases(&mut self.connection, staging_id, aliases)
    }

    pub(crate) fn replace_add_characters(
        &mut self,
        staging_id: &str,
        characters: &[Value],
    ) -> StoreResult<()> {
        commit::replace_add_characters(&mut self.connection, staging_id, characters)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn replace_commit(
        &mut self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<RevisionResult> {
        let prepared = self.prepare_replace_commit(staging_id, expected_revision)?;
        self.finish_prepared_replace(prepared)
    }

    fn verify_staged_cold_payload_objects(&self, staging_id: &str) -> StoreResult<()> {
        let authority = commit::read_cold_payload_authority(&self.connection, staging_id)?;
        if !matches!(authority, ColdPayloadAuthorityState::V2 { .. }) {
            return Ok(());
        }
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let mut statement = self.connection.prepare(
            "SELECT key, object_hash, size, metadata
             FROM cold_aliases WHERE generation = ?1 ORDER BY key ASC",
        )?;
        let mut rows = statement.query([staging_id])?;
        while let Some(row) = rows.next()? {
            let metadata: String = row.get(3)?;
            let alias = ColdAlias {
                key: row.get(0)?,
                object_hash: row.get(1)?,
                size: row.get(2)?,
                metadata: serde_json::from_str(&metadata)?,
            };
            verify_cold_alias_object(&cas, &alias)?;
        }
        Ok(())
    }

    pub(crate) fn prepare_replace_commit(
        &self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<PreparedReplaceCommit> {
        let revision =
            commit::validate_replace_commit(&self.connection, staging_id, expected_revision)?;
        self.verify_staged_cold_payload_objects(staging_id)?;
        Ok(PreparedReplaceCommit {
            staging_id: staging_id.to_owned(),
            revision,
        })
    }

    pub(crate) fn finish_prepared_replace(
        &mut self,
        prepared: PreparedReplaceCommit,
    ) -> StoreResult<RevisionResult> {
        self.verify_staged_cold_payload_objects(&prepared.staging_id)?;
        commit::replace_commit(
            &mut self.connection,
            &prepared.staging_id,
            Some(prepared.revision),
        )
    }

    pub(crate) fn finish_prepared_replace_with_app_kv(
        &mut self,
        prepared: PreparedReplaceCommit,
        key: &str,
        value: &Value,
    ) -> StoreResult<RevisionResult> {
        self.verify_staged_cold_payload_objects(&prepared.staging_id)?;
        commit::replace_commit_with_app_kv(
            &mut self.connection,
            &prepared.staging_id,
            Some(prepared.revision),
            Some((key, value)),
        )
    }

    pub(crate) fn replace_abort(&mut self, staging_id: &str) -> StoreResult<()> {
        commit::replace_abort(&mut self.connection, staging_id)
    }

    pub(crate) fn acquire_revision(&mut self, revision: i64) -> StoreResult<LeaseResult> {
        let (lease, reader) = snapshot::acquire_revision(
            &self.database_path,
            revision,
            Arc::clone(&self.active_readers),
        )?;
        self.revision_leases.insert(lease.clone(), reader);
        Ok(LeaseResult { lease })
    }

    pub(crate) fn release_revision(&mut self, lease: &str) -> StoreResult<()> {
        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        if let Some(reader) = self.revision_leases.remove(lease) {
            snapshot::close_revision(reader)?;
        }
        self.checkpoint_after_release()
    }

    pub(crate) fn export_risu_save(
        &self,
        lease: &str,
        omit_account: bool,
    ) -> StoreResult<export::ExportedRisuSave> {
        let (connection, target) = self.read_view(Some(lease))?;
        export::create(
            connection,
            &self.snapshots_dir,
            &target,
            lease,
            omit_account,
        )
    }

    pub(crate) fn prepare_risu_save_export(
        &mut self,
        revision: i64,
    ) -> StoreResult<PreparedRisuSaveExport> {
        let (lease, reader) = snapshot::acquire_revision(
            &self.database_path,
            revision,
            Arc::clone(&self.active_readers),
        )?;
        reader.publish_detached_asset_roots()?;
        Ok(PreparedRisuSaveExport {
            revision,
            lease,
            snapshots_dir: self.snapshots_dir.clone(),
            database_path: self.database_path.clone(),
            reader: Some(reader),
        })
    }

    #[cfg(feature = "native-official-publication")]
    pub(crate) fn prepare_official_publication(
        &mut self,
        lease: &str,
        expected_revision: i64,
    ) -> StoreResult<PreparedOfficialPublication> {
        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        let reader = self
            .revision_leases
            .get(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        if reader.target.revision != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
                actual: reader.target.revision,
            });
        }
        reader.publish_detached_asset_roots()?;
        let reader = self
            .revision_leases
            .remove(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        Ok(PreparedOfficialPublication {
            revision: reader.target.revision,
            lease: lease.to_owned(),
            snapshots_dir: self.snapshots_dir.clone(),
            database_path: self.database_path.clone(),
            reader: Some(reader),
        })
    }

    #[cfg(feature = "native-official-publication")]
    pub(crate) fn prepare_official_publication_for_job(
        &mut self,
        lease: &str,
        expected_revision: i64,
        job_id: &str,
        created_at_ms: i64,
    ) -> StoreResult<PreparedOfficialPublication> {
        use crate::asset_repository::job_pins::{
            CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
        };

        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        let roots = {
            let reader = self
                .revision_leases
                .get(lease)
                .ok_or(StoreError::SnapshotReleased)?;
            if reader.target.revision != expected_revision {
                return Err(StoreError::RevisionConflict {
                    expected: expected_revision,
                    actual: reader.target.revision,
                });
            }
            reader.asset_roots()?
        };
        let mut durable = DurableCasJob::begin(
            &self.repository_root,
            job_id,
            CasJobKind::OfficialPublicationOrExportPreparation,
            created_at_ms,
        )?;
        let pin_result = (|| -> StoreResult<()> {
            let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
            let mut pins = Vec::with_capacity(
                roots
                    .manifest_hashes
                    .len()
                    .saturating_add(roots.object_hashes.len()),
            );
            for hash in &roots.manifest_hashes {
                let byte_size = cas
                    .stat_object(hash)?
                    .ok_or_else(|| StoreError::Validation {
                        message: "Pinned publication owner manifest is missing from CAS".to_owned(),
                    })?;
                pins.push((hash.clone(), byte_size, CasObjectRole::OwnerManifest));
            }
            for hash in roots
                .object_hashes
                .iter()
                .filter(|hash| !roots.manifest_hashes.contains(*hash))
            {
                let byte_size = cas
                    .stat_object(hash)?
                    .ok_or_else(|| StoreError::Validation {
                        message: "Pinned publication object is missing from CAS".to_owned(),
                    })?;
                pins.push((hash.clone(), byte_size, CasObjectRole::DirectObject));
            }
            durable.pin_existing_batch(&cas, &pins)?;
            durable.seal(self, created_at_ms)?;
            Ok(())
        })();
        if let Err(error) = pin_result {
            return Err(combine_publication_cleanup_error(
                error,
                durable
                    .release(CasReleaseOutcome::Aborted)
                    .map_err(StoreError::from),
                "CAS job abort",
            ));
        }
        match self.prepare_official_publication(lease, expected_revision) {
            Ok(prepared) => Ok(prepared),
            Err(error) => Err(combine_publication_cleanup_error(
                error,
                durable
                    .release(CasReleaseOutcome::Aborted)
                    .map_err(StoreError::from),
                "CAS job abort",
            )),
        }
    }

    pub(crate) fn cleanup_risu_save_export(&self, path: &Path) -> StoreResult<()> {
        export::cleanup(&self.snapshots_dir, path)
    }

    #[cfg(feature = "official-publication-upload-pilot")]
    pub(crate) fn open_risu_save_export_for_upload(
        &self,
        path: &Path,
    ) -> StoreResult<(std::fs::File, u64)> {
        let (source, bytes, lease) = export::open_for_upload(&self.snapshots_dir, path)?;
        self.read_view(Some(&lease))?;
        Ok((source, bytes))
    }

    #[cfg(feature = "native-kei-upload-pilot")]
    fn prepare_kei_upload(
        &mut self,
        lease: &str,
        url: &str,
        expected_account_id: &str,
        token: &str,
    ) -> StoreResult<kei::PreparedKeiUpload> {
        let reader = self
            .revision_leases
            .remove(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        match kei::prepare_upload(
            &self.snapshots_dir,
            lease,
            reader,
            url,
            expected_account_id,
            token,
        ) {
            Ok(prepared) => Ok(prepared),
            Err((error, reader)) => {
                self.revision_leases.insert(lease.to_owned(), reader);
                Err(error)
            }
        }
    }

    #[cfg(feature = "native-kei-upload-pilot")]
    pub(crate) fn prepare_kei_job_upload(
        &mut self,
        lease: &str,
        expected_revision: i64,
        url: &str,
        expected_account_id: &str,
        token: &str,
    ) -> StoreResult<kei::PreparedKeiUpload> {
        let reader = self
            .revision_leases
            .remove(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        match kei::prepare_job_upload(
            &self.snapshots_dir,
            lease,
            reader,
            expected_revision,
            url,
            expected_account_id,
            token,
        ) {
            Ok(prepared) => Ok(prepared),
            Err((error, reader)) => {
                self.revision_leases.insert(lease.to_owned(), reader);
                Err(error)
            }
        }
    }

    pub(crate) fn checkpoint(&self, mode: CheckpointMode) -> StoreResult<()> {
        if mode == CheckpointMode::Truncate && self.active_readers.active_count() > 0 {
            return Err(StoreError::Store {
                message: "truncate checkpoint cannot run while an active read lease pins the WAL"
                    .to_owned(),
            });
        }
        snapshot::checkpoint(&self.connection, mode)
    }

    pub(crate) fn snapshot_create(&self, reason: &str) -> StoreResult<SnapshotCreated> {
        snapshot::create(&self.connection, &self.snapshots_dir, reason)
    }

    pub(crate) fn snapshot_list(&self) -> StoreResult<Vec<SnapshotInfo>> {
        snapshot::list(&self.snapshots_dir)
    }

    pub(crate) fn snapshot_delete(&self, id: &str) -> StoreResult<()> {
        snapshot_archive::Archive::open(&self.snapshots_dir)?.delete(id)
    }

    pub(crate) fn storage_stats(&self) -> StoreResult<PersistentStorageStats> {
        let transaction = self.connection.unchecked_transaction()?;
        let active = active_generation(&transaction)?;
        let database_bytes = query_count_bytes(
            &transaction,
            "SELECT 1, page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
        )?
        .bytes;
        let asset_objects = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM asset_objects",
            [],
        )?;
        let cold_aliases = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM cold_aliases WHERE generation = ?1",
            [&active],
        )?;
        let plugin_storage = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM plugin_storage WHERE generation = ?1",
            [&active],
        )?;
        let active_characters = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(length(detail)), 0) FROM characters WHERE generation = ?1 AND trashed = 0",
            [&active],
        )?;
        let trashed_count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM characters WHERE generation = ?1 AND trashed = 1",
            [&active],
            |row| row.get(0),
        )?;
        let (conversation_count, message_count): (i64, i64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM conversations WHERE generation = ?1",
            [&active],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let mut aliases = Vec::new();
        let mut statement = transaction.prepare(
            "SELECT kind, inlay_type, COUNT(*), COALESCE(SUM(size), 0)
             FROM asset_aliases WHERE generation = ?1 GROUP BY kind, inlay_type",
        )?;
        for row in statement.query_map([&active], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })? {
            let (kind, inlay_type, count, bytes) = row?;
            aliases.push(StorageAliasStats {
                kind,
                inlay_type,
                count: nonnegative_u64(count)?,
                bytes: nonnegative_u64(bytes)?,
            });
        }
        drop(statement);
        let mut deletions = Vec::new();
        let mut statement = transaction.prepare(
            "SELECT state, COUNT(*), COALESCE(SUM(byte_size), 0)
             FROM asset_object_deletions GROUP BY state",
        )?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })? {
            let (state, count, bytes) = row?;
            deletions.push(StorageDeletionStats {
                state,
                count: nonnegative_u64(count)?,
                bytes: nonnegative_u64(bytes)?,
            });
        }
        drop(statement);
        transaction.commit()?;
        Ok(PersistentStorageStats {
            snapshot_bytes: snapshot_archive::Archive::open(&self.snapshots_dir)?.bytes()?,
            database_bytes,
            asset_objects,
            asset_aliases: aliases,
            cold_aliases,
            plugin_storage,
            characters: StorageCharacterStats {
                active: active_characters,
                trashed_count: nonnegative_u64(trashed_count)?,
            },
            conversations: StorageConversationStats {
                count: nonnegative_u64(conversation_count)?,
                message_count: nonnegative_u64(message_count)?,
            },
            asset_object_deletions: deletions,
        })
    }

    pub(crate) fn snapshot_restore_request(&self, id: &str) -> StoreResult<()> {
        snapshot_archive::Archive::open(&self.snapshots_dir)?.request_restore(id)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn asset_gc_dry_run(
        &self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::migration_gc::{
            dry_run_mark_and_sweep_with_remote, AssetGcDryRunPage,
        };

        let persistent_dir = self
            .snapshots_dir
            .parent()
            .ok_or_else(|| StoreError::Store {
                message: "persistent snapshots directory has no parent".to_owned(),
            })?;
        let repository_root = persistent_dir.parent().ok_or_else(|| StoreError::Store {
            message: "persistent directory has no repository root".to_owned(),
        })?;
        let cas = crate::asset_repository::PayloadCas::new(repository_root)?;
        let roots = self.collect_asset_gc_roots(&cas, false, true)?;
        let candidates = self.query_asset_object_catalog(limit, cursor)?;
        let residency = crate::server_sync::residency::Residency::open(repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let report = dry_run_mark_and_sweep_with_remote(
            &cas,
            candidates.items,
            roots,
            now_ms,
            minimum_grace_ms,
            |hash| residency.gc_size(hash),
        )
        .map_err(StoreError::from)?;
        Ok(AssetGcDryRunPage {
            report,
            next_cursor: candidates.next_cursor,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn asset_gc_delete_page(
        &mut self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_delete_page_with_hook(limit, cursor, now_ms, minimum_grace_ms, |_| Ok(()))
    }

    pub(crate) fn asset_gc_product_maintenance_page(
        &mut self,
        now_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_product_maintenance_page_with_hook_inner(now_ms, |_| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn asset_gc_product_maintenance_page_with_hook(
        &mut self,
        now_ms: i64,
        hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_product_maintenance_page_with_hook_inner(now_ms, hook)
    }

    fn asset_gc_product_maintenance_page_with_hook_inner(
        &mut self,
        now_ms: i64,
        hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::migration_gc::{AssetGcDryRunPage, AssetGcDryRunReport};

        let cursor: Option<String> = self.connection.query_row(
            "SELECT catalog_cursor FROM asset_gc_maintenance_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if let Some(cursor) = cursor.as_deref() {
            let blocker = if asset_object_catalog::validate_cursor(cursor).is_err() {
                Some("asset-gc-cursor-invalid")
            } else if !asset_object_catalog::cursor_has_successor(&self.connection, cursor)? {
                Some("asset-gc-cursor-stale")
            } else {
                None
            };
            if let Some(blocker) = blocker {
                self.record_asset_gc_maintenance_cursor(None)?;
                return Ok(AssetGcDryRunPage {
                    report: AssetGcDryRunReport {
                        marked_hashes: Vec::new(),
                        grace_retained_hashes: Vec::new(),
                        potential_delete_hashes: Vec::new(),
                        potential_delete_bytes: 0,
                        deleted_hashes: Vec::new(),
                        deleted_bytes: 0,
                        blockers: vec![blocker.to_owned()],
                        deletion_enabled: false,
                    },
                    next_cursor: None,
                });
            }
        }
        let page = self.asset_gc_delete_page_with_hook(
            ASSET_GC_PRODUCT_PAGE_LIMIT,
            cursor.as_deref(),
            now_ms,
            ASSET_GC_PRODUCT_MINIMUM_GRACE_MS,
            hook,
        )?;
        self.record_asset_gc_maintenance_cursor(page.next_cursor.as_deref())?;
        Ok(page)
    }

    fn record_asset_gc_maintenance_cursor(&mut self, cursor: Option<&str>) -> StoreResult<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transaction.execute(
            "UPDATE asset_gc_maintenance_state SET catalog_cursor = ?1 WHERE singleton = 1",
            [cursor],
        )? != 1
        {
            return Err(StoreError::Validation {
                message: "asset GC maintenance state row is missing".to_owned(),
            });
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn asset_gc_delete_page_with_hook(
        &mut self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
        mut hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::migration_gc::{
            dry_run_mark_and_sweep_with_remote, AssetGcDeleteHookPoint, AssetGcDryRunPage,
        };

        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let initial_candidates = self.query_asset_object_catalog(limit, cursor)?;
        let residency = crate::server_sync::residency::Residency::open(&self.repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let initial_report = dry_run_mark_and_sweep_with_remote(
            &cas,
            initial_candidates.items.clone(),
            self.collect_asset_gc_roots(&cas, false, false)?,
            now_ms,
            minimum_grace_ms,
            |hash| residency.gc_size(hash),
        )?;
        if !initial_report.blockers.is_empty() {
            return Ok(AssetGcDryRunPage {
                report: initial_report,
                next_cursor: initial_candidates.next_cursor,
            });
        }
        if initial_report.potential_delete_hashes.is_empty() {
            let mut report = initial_report;
            report.deletion_enabled = true;
            return Ok(AssetGcDryRunPage {
                report,
                next_cursor: initial_candidates.next_cursor,
            });
        }
        hook(AssetGcDeleteHookPoint::AfterInitialScan)?;

        let _repository_guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
        let final_candidates = self.query_asset_object_catalog(limit, cursor)?;
        if final_candidates != initial_candidates {
            return Err(StoreError::Validation {
                message: "asset object catalog page changed before final GC recheck".to_owned(),
            });
        }
        let mut report = dry_run_mark_and_sweep_with_remote(
            &cas,
            final_candidates.items.clone(),
            self.collect_asset_gc_roots(&cas, true, false)?,
            now_ms,
            minimum_grace_ms,
            |hash| residency.gc_size(hash),
        )?;
        if !report.blockers.is_empty() {
            return Ok(AssetGcDryRunPage {
                report,
                next_cursor: final_candidates.next_cursor,
            });
        }
        report.deletion_enabled = true;
        if report.potential_delete_hashes.is_empty() {
            return Ok(AssetGcDryRunPage {
                report,
                next_cursor: final_candidates.next_cursor,
            });
        }

        for object_hash in report.potential_delete_hashes.clone() {
            let candidate = final_candidates
                .items
                .iter()
                .find(|candidate| candidate.object_hash == object_hash)
                .ok_or_else(|| StoreError::Validation {
                    message: "final GC candidate is absent from its exact catalog page".to_owned(),
                })?;
            let physical_key = crate::asset_repository::object_physical_key(&object_hash);
            let byte_size =
                i64::try_from(candidate.byte_size).map_err(|_| StoreError::Validation {
                    message: "asset GC candidate size exceeds the SQLite integer limit".to_owned(),
                })?;
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO asset_object_deletions (
                    object_hash, byte_size, physical_key, state, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'pending', ?4)",
                params![object_hash, byte_size, physical_key, now_ms],
            )?;
            transaction.commit()?;
            hook(AssetGcDeleteHookPoint::AfterTombstone)?;

            let unlink = cas.unlink_exact_object(
                &candidate.object_hash,
                candidate.byte_size,
                &physical_key,
            )?;
            hook(AssetGcDeleteHookPoint::AfterUnlink)?;
            let directory_entries_synced = match unlink {
                crate::asset_repository::ExactObjectUnlink::Missing => {
                    return Err(StoreError::Validation {
                        message: "exact GC object disappeared before unlink".to_owned(),
                    });
                }
                crate::asset_repository::ExactObjectUnlink::Removed {
                    directory_entries_synced,
                } => directory_entries_synced,
            };
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if transaction.execute(
                "DELETE FROM asset_objects
                 WHERE object_hash = ?1 AND byte_size = ?2",
                params![object_hash, byte_size],
            )? != 1
            {
                return Err(StoreError::Validation {
                    message: "exact GC catalog row changed before deletion completion".to_owned(),
                });
            }
            transaction.execute(
                "UPDATE asset_object_deletions SET state = 'unlinked'
                 WHERE object_hash = ?1 AND state = 'pending'",
                [&object_hash],
            )?;
            if directory_entries_synced {
                transaction.execute(
                    "DELETE FROM asset_object_deletions WHERE object_hash = ?1",
                    [&object_hash],
                )?;
            }
            transaction.commit()?;
            report.deleted_bytes = report
                .deleted_bytes
                .checked_add(candidate.byte_size)
                .ok_or_else(|| StoreError::Validation {
                    message: "asset GC deleted byte count overflow".to_owned(),
                })?;
            report.deleted_hashes.push(object_hash);
        }
        Ok(AssetGcDryRunPage {
            report,
            next_cursor: final_candidates.next_cursor,
        })
    }

    fn collect_asset_gc_roots(
        &self,
        cas: &crate::asset_repository::PayloadCas,
        repository_guard_held: bool,
        read_only: bool,
    ) -> StoreResult<Vec<crate::asset_repository::migration_gc::AssetRootSet>> {
        use crate::asset_repository::job_pins::{
            collect_durable_cas_job_roots, collect_durable_cas_job_roots_already_guarded,
            collect_durable_cas_job_roots_read_only,
        };
        use crate::asset_repository::migration_gc::collect_staged_migration_roots;

        let mut roots = vec![snapshot::collect_asset_roots(&self.connection, cas)?];
        for reader in self.revision_leases.values() {
            roots.push(snapshot::collect_asset_roots(&reader.connection, cas)?);
        }
        roots.extend(self.active_readers.detached_asset_roots()?);
        roots.extend(snapshot_archive::Archive::open(&self.snapshots_dir)?.roots()?);
        roots.extend(collect_staged_migration_roots(&self.repository_root)?);
        roots.push(if read_only {
            collect_durable_cas_job_roots_read_only(&self.repository_root)
        } else if repository_guard_held {
            collect_durable_cas_job_roots_already_guarded(&self.repository_root)
        } else {
            collect_durable_cas_job_roots(&self.repository_root)
        });
        Ok(roots)
    }

    pub(crate) fn get_app_kv(&self, key: &str) -> StoreResult<Option<Value>> {
        let value: Option<String> = self
            .connection
            .query_row("SELECT value FROM app_kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    pub(crate) fn set_app_kv(&self, key: &str, value: &Value) -> StoreResult<()> {
        self.connection.execute(
            "INSERT INTO app_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub(crate) fn remove_app_kv(&self, key: &str) -> StoreResult<()> {
        self.connection
            .execute("DELETE FROM app_kv WHERE key = ?1", [key])?;
        Ok(())
    }

    fn read_view(&self, lease: Option<&str>) -> StoreResult<(&Connection, ReadTarget)> {
        match lease {
            None => Ok((
                &self.connection,
                ReadTarget {
                    revision: current_revision(&self.connection)?,
                    generation: active_generation(&self.connection)?,
                },
            )),
            Some(lease) => {
                let reader = self
                    .revision_leases
                    .get(lease)
                    .ok_or(StoreError::SnapshotReleased)?;
                Ok((&reader.connection, reader.target.clone()))
            }
        }
    }

    fn checkpoint_after_release(&self) -> StoreResult<()> {
        let mode = if self.active_readers.active_count() == 0 {
            CheckpointMode::Truncate
        } else {
            CheckpointMode::Passive
        };
        match snapshot::checkpoint(&self.connection, mode) {
            Err(error) if mode == CheckpointMode::Truncate && checkpoint_was_busy(&error) => {
                snapshot::checkpoint(&self.connection, CheckpointMode::Passive)
            }
            result => result,
        }
    }

    #[cfg(test)]
    fn lease_diagnostics(&self) -> LeaseDiagnostics {
        let now = Instant::now();
        LeaseDiagnostics {
            active_count: self.active_readers.active_count(),
            oldest_age_us: self
                .revision_leases
                .values()
                .map(|lease| now.duration_since(lease.acquired_at).as_micros() as u64)
                .max()
                .unwrap_or(0),
        }
    }
}

fn nonnegative_u64(value: i64) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::Validation {
        message: "storage statistic is negative".to_owned(),
    })
}

fn query_count_bytes<P: rusqlite::Params>(
    connection: &Connection,
    sql: &str,
    params: P,
) -> StoreResult<StorageCountBytes> {
    let (count, bytes): (i64, i64) =
        connection.query_row(sql, params, |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(StorageCountBytes {
        count: nonnegative_u64(count)?,
        bytes: nonnegative_u64(bytes)?,
    })
}

fn recover_asset_object_deletions(
    connection: &mut Connection,
    repository_root: &Path,
) -> StoreResult<()> {
    use asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE;

    let tombstones = {
        let mut statement = connection.prepare(
            "SELECT object_hash, byte_size, physical_key, state
             FROM asset_object_deletions
             ORDER BY state ASC, created_at_ms ASC, object_hash ASC
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map([ASSET_OBJECT_CATALOG_MAX_PAGE], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    if tombstones.is_empty() {
        return Ok(());
    }

    let _repository_guard = crate::asset_repository::coordinator::lock_repository_mutation()?;
    let cas = crate::asset_repository::PayloadCas::new(repository_root)?;
    for (object_hash, byte_size, physical_key, state) in tombstones {
        let byte_size = u64::try_from(byte_size).map_err(|_| StoreError::Validation {
            message: "asset deletion tombstone size is invalid".to_owned(),
        })?;
        if physical_key != crate::asset_repository::object_physical_key(&object_hash) {
            return Err(StoreError::Validation {
                message: "asset deletion tombstone physical key is not canonical".to_owned(),
            });
        }
        let catalog_size: Option<i64> = connection
            .query_row(
                "SELECT byte_size FROM asset_objects WHERE object_hash = ?1",
                [&object_hash],
                |row| row.get(0),
            )
            .optional()?;
        match state.as_str() {
            "pending" => {
                if catalog_size != Some(byte_size as i64) {
                    return Err(StoreError::Validation {
                        message: "pending asset deletion has no exact catalog row".to_owned(),
                    });
                }
                match cas.stat_object(&object_hash)? {
                    Some(actual_size) if actual_size == byte_size => {
                        connection.execute(
                            "DELETE FROM asset_object_deletions
                             WHERE object_hash = ?1 AND state = 'pending'",
                            [&object_hash],
                        )?;
                    }
                    Some(_) => {
                        return Err(StoreError::Validation {
                            message: "pending asset deletion size is ambiguous".to_owned(),
                        });
                    }
                    None => {
                        let transaction =
                            connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        if transaction.execute(
                            "DELETE FROM asset_objects
                             WHERE object_hash = ?1 AND byte_size = ?2",
                            params![object_hash, byte_size as i64],
                        )? != 1
                        {
                            return Err(StoreError::Validation {
                                message: "pending asset deletion catalog row changed".to_owned(),
                            });
                        }
                        transaction.execute(
                            "UPDATE asset_object_deletions SET state = 'unlinked'
                             WHERE object_hash = ?1 AND state = 'pending'",
                            [&object_hash],
                        )?;
                        transaction.commit()?;
                    }
                }
            }
            "unlinked" => {
                if catalog_size.is_some() {
                    return Err(StoreError::Validation {
                        message: "completed asset deletion unexpectedly has a catalog row"
                            .to_owned(),
                    });
                }
                match cas.stat_object(&object_hash)? {
                    None => {
                        connection.execute(
                            "DELETE FROM asset_object_deletions
                             WHERE object_hash = ?1 AND state = 'unlinked'",
                            [&object_hash],
                        )?;
                    }
                    Some(actual_size) if actual_size == byte_size => {}
                    Some(_) => {
                        return Err(StoreError::Validation {
                            message: "unlinked asset deletion size is ambiguous".to_owned(),
                        });
                    }
                }
            }
            _ => {
                return Err(StoreError::Validation {
                    message: "asset deletion tombstone state is invalid".to_owned(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
struct LeaseDiagnostics {
    active_count: usize,
    oldest_age_us: u64,
}

pub(super) fn checkpoint_after_detached_release(
    database_path: &Path,
    active_readers: &snapshot::ActiveReaderRegistry,
) -> StoreResult<()> {
    let connection = Connection::open(database_path)?;
    connection.busy_timeout(Duration::ZERO)?;
    if active_readers.active_count() > 0 {
        return snapshot::checkpoint(&connection, CheckpointMode::Passive);
    }
    match snapshot::checkpoint(&connection, CheckpointMode::Truncate) {
        Err(error) if checkpoint_was_busy(&error) => {
            snapshot::checkpoint(&connection, CheckpointMode::Passive)
        }
        result => result,
    }
}

fn checkpoint_was_busy(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Store { message }
            if message == "truncate checkpoint could not complete because the database is busy"
    )
}

pub(super) fn current_revision(connection: &Connection) -> StoreResult<i64> {
    let value: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'currentRevision'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

pub(super) fn active_generation(connection: &Connection) -> StoreResult<String> {
    let value: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'activeGeneration'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod tests;
