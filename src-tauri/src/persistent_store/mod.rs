pub(crate) mod asset_object_catalog;
pub(crate) mod commands;
mod commit;
pub(crate) mod export;
#[cfg(feature = "native-kei-upload-pilot")]
pub(crate) mod kei;
#[allow(dead_code)]
pub(crate) mod logical_delta_source;
mod logical_delta_target;
#[allow(unused_imports)]
pub(crate) use logical_delta_target::establish_logical_common_base;
#[allow(dead_code)]
mod logical_index;
#[allow(dead_code)]
mod logical_schema;
mod owner_projection;
mod query;
mod schema;
mod snapshot;
mod sync_device_registry;
#[cfg(test)]
mod sync_device_registry_tests;

pub(crate) use asset_object_catalog::{AssetObjectCatalog, AssetObjectCatalogPage};
pub(crate) use commands::PersistentStoreState;
pub(crate) use snapshot::RevisionReadLease;
#[allow(unused_imports)]
pub(crate) use sync_device_registry::{
    RegisteredSyncDevice, RegisteredSyncDeviceStatus, SyncGenerationIdentity,
    TombstoneCollectionItem, TombstoneCollectionPage, VerifiedSyncDeviceRegistration,
};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
        let migration_id = match self {
            Self::Legacy => return Ok(()),
            Self::Preparing {
                migration_id,
                source_revision,
            } => {
                if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(source_revision) {
                    return Err(StoreError::Validation {
                        message: "Asset repository sourceRevision is invalid".to_owned(),
                    });
                }
                migration_id
            }
            Self::V2 {
                migration_id,
                compatibility_hash,
            } => {
                validate_hash(compatibility_hash, "Asset repository compatibilityHash")?;
                migration_id
            }
        };
        if migration_id.is_empty()
            || migration_id.len() > 64
            || !migration_id
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-'))
        {
            return Err(StoreError::Validation {
                message: "Asset repository migrationId is invalid".to_owned(),
            });
        }
        Ok(())
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
        let migration_id = match self {
            Self::Legacy => return Ok(()),
            Self::Preparing {
                migration_id,
                source_revision,
            } => {
                if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(source_revision) {
                    return Err(StoreError::Validation {
                        message: "Cold payload sourceRevision is invalid".to_owned(),
                    });
                }
                migration_id
            }
            Self::V2 {
                migration_id,
                compatibility_hash,
            } => {
                validate_hash(compatibility_hash, "Cold payload compatibilityHash")?;
                migration_id
            }
        };
        if migration_id.is_empty()
            || migration_id.len() > 64
            || !migration_id
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'_' | b'-'))
        {
            return Err(StoreError::Validation {
                message: "Cold payload migrationId is invalid".to_owned(),
            });
        }
        Ok(())
    }
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
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
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

fn validate_object_hash(hash: &Option<String>, subject: &str) -> StoreResult<()> {
    if hash.as_ref().is_some_and(|hash| {
        hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(StoreError::Validation {
            message: format!(
                "{subject} objectHash must be null or 64 lowercase hexadecimal characters"
            ),
        });
    }
    Ok(())
}

fn validate_hash(hash: &str, subject: &str) -> StoreResult<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
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

pub(crate) fn materialized_asset_owner_entries<'a>(
    database: &'a Value,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<&'a Vec<Value>>> {
    commit::staged_owner_entries(database, owner)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotInfo {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) modified_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotCreated {
    pub(crate) path: String,
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
}

pub(crate) struct PreparedReplaceCommit {
    staging_id: String,
    revision: i64,
    database_path: PathBuf,
    snapshots_dir: PathBuf,
}

pub(crate) struct SnapshotAuthorizedReplaceCommit {
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

impl PreparedRisuSaveExport {
    pub(crate) fn take_reader(&mut self) -> StoreResult<RevisionReadLease> {
        self.reader.take().ok_or_else(|| StoreError::Validation {
            message: "native export reader has already been taken".to_owned(),
        })
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

impl PreparedReplaceCommit {
    pub(crate) fn create_snapshot(self) -> StoreResult<SnapshotAuthorizedReplaceCommit> {
        if self.revision > 0 {
            let connection = Connection::open(&self.database_path)?;
            connection.execute_batch("PRAGMA busy_timeout = 5000")?;
            let created = snapshot::create(&connection, &self.snapshots_dir, "pre-replace")?;
            drop(connection);
            let snapshot_connection = Connection::open(&created.path)?;
            let snapshot_revision = current_revision(&snapshot_connection)?;
            if snapshot_revision != self.revision {
                return Err(StoreError::RevisionConflict {
                    expected: self.revision,
                    actual: snapshot_revision,
                });
            }
        }
        Ok(SnapshotAuthorizedReplaceCommit {
            staging_id: self.staging_id,
            revision: self.revision,
        })
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
        snapshot::apply_pending_restore(&persistent_dir, &snapshots_dir)?;

        let database_path = persistent_dir.join("persistent.db");
        let mut connection = Connection::open(&database_path)?;
        schema::initialize(&mut connection)?;

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
        logical_index::cleanup_abandoned_logical_staging(&mut connection)?;
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
        })
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
        let cas = logical_index::logical_index_is_active(&self.connection)?
            .then(|| crate::asset_repository::PayloadCas::new(&self.repository_root))
            .transpose()?;
        commit::commit(&mut self.connection, cas.as_ref(), commit, asset_aliases)
    }

    pub(crate) fn commit_asset_alias(
        &mut self,
        alias: &AssetAlias,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        let cas = logical_index::logical_index_is_active(&self.connection)?
            .then(|| crate::asset_repository::PayloadCas::new(&self.repository_root))
            .transpose()?;
        commit::commit_asset_alias(&mut self.connection, cas.as_ref(), alias, expected_revision)
    }

    pub(crate) fn delete_asset_alias(
        &mut self,
        kind: &str,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        let maintain_logical_index = logical_index::logical_index_is_active(&self.connection)?;
        commit::delete_asset_alias(
            &mut self.connection,
            maintain_logical_index,
            kind,
            key,
            expected_revision,
        )
    }

    pub(crate) fn commit_cold_alias(
        &mut self,
        alias: &ColdAlias,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        verify_cold_alias_object(&cas, alias)?;
        let logical_cas = logical_index::logical_index_is_active(&self.connection)?.then_some(&cas);
        commit::commit_cold_alias(&mut self.connection, logical_cas, alias, expected_revision)
    }

    pub(crate) fn delete_cold_alias(
        &mut self,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        let maintain_logical_index = logical_index::logical_index_is_active(&self.connection)?;
        commit::delete_cold_alias(
            &mut self.connection,
            maintain_logical_index,
            key,
            expected_revision,
        )
    }

    pub(crate) fn activate_cold_payload_migration(
        &mut self,
        input: &ColdPayloadMigrationInput,
    ) -> StoreResult<RevisionResult> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        for alias in &input.cold_aliases {
            verify_cold_alias_object(&cas, alias)?;
        }
        let logical_cas = logical_index::logical_index_is_active(&self.connection)?.then_some(&cas);
        commit::activate_cold_payload_migration(&mut self.connection, logical_cas, input)
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

    pub(crate) fn replace_preserve_cold_payloads(
        &mut self,
        staging_id: &str,
        expected_revision: i64,
    ) -> StoreResult<()> {
        commit::replace_preserve_cold_payloads(&mut self.connection, staging_id, expected_revision)
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
        let authorized = prepared.create_snapshot()?;
        self.finish_prepared_replace(authorized)
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
            database_path: self.database_path.clone(),
            snapshots_dir: self.snapshots_dir.clone(),
        })
    }

    pub(crate) fn finish_prepared_replace(
        &mut self,
        prepared: SnapshotAuthorizedReplaceCommit,
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
        prepared: SnapshotAuthorizedReplaceCommit,
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

    pub(crate) fn snapshot_restore_request(&self, path: &Path) -> StoreResult<()> {
        snapshot::restore_request(&self.snapshots_dir, path)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn asset_gc_dry_run(
        &self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::job_pins::collect_durable_cas_job_roots;
        use crate::asset_repository::migration_gc::{
            collect_staged_migration_roots, dry_run_mark_and_sweep,
            read_snapshot_asset_root_sidecar, AssetGcDryRunPage,
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
        let mut roots = vec![snapshot::collect_asset_roots(&self.connection, &cas)?];
        for reader in self.revision_leases.values() {
            roots.push(snapshot::collect_asset_roots(&reader.connection, &cas)?);
        }
        roots.extend(self.active_readers.detached_asset_roots()?);
        for snapshot in snapshot::list(&self.snapshots_dir)? {
            roots.push(read_snapshot_asset_root_sidecar(Path::new(&snapshot.path))?.roots);
        }
        roots.extend(collect_staged_migration_roots(repository_root)?);
        roots.push(collect_durable_cas_job_roots(repository_root));
        #[cfg(not(unix))]
        roots.push(crate::asset_repository::migration_gc::AssetRootSet {
            blockers: ["cas-directory-sync-unverified".to_owned()].into(),
            ..Default::default()
        });
        let candidates = self.query_asset_object_catalog(limit, cursor)?;
        let report =
            dry_run_mark_and_sweep(&cas, candidates.items, roots, now_ms, minimum_grace_ms)
                .map_err(StoreError::from)?;
        Ok(AssetGcDryRunPage {
            report,
            next_cursor: candidates.next_cursor,
        })
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

pub(super) fn generation_is_retained(
    connection: &Connection,
    generation: &str,
) -> StoreResult<bool> {
    let logical_schema_exists: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'logical_sync_generations'
         )",
        [],
        |row| row.get(0),
    )?;
    if !logical_schema_exists {
        return Ok(false);
    }
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_sync_generations
                WHERE pds_generation = ?1 AND state = 'complete'
             )",
            [generation],
            |row| row.get(0),
        )
        .map_err(Into::into)
}
#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod logical_delta_source_tests;
#[cfg(test)]
mod tests;
