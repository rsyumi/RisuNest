pub(crate) mod archive;
pub(crate) mod author_note;
mod asset_alias_orphans;
pub(crate) mod gallery;
pub(crate) mod asset_object_catalog;
pub(crate) mod asset_residency;
pub(crate) mod commands;
pub(crate) mod commit;
pub(crate) mod compaction;
pub(crate) mod content_capture;
mod message_pages;
#[cfg(test)]
mod message_pages_copied_database_tests;
#[cfg(test)]
pub(crate) mod hash_work;
pub(crate) mod content_change_index;
pub(crate) mod lww;
mod upstream_identity;
pub(crate) mod upstream_stream;
pub(crate) mod lww_commands;
mod content_locators;
pub(crate) mod device_store;
pub(crate) mod image_geometry;
pub(crate) mod export;
pub(crate) mod external_apply;
pub(crate) mod external_capture;
pub(crate) mod external_lww;
pub(crate) mod external_content_gc;
pub(crate) mod external_runtime;
pub(crate) mod external_storage_state;
#[cfg(feature = "native-kei-upload-pilot")]
pub(crate) mod kei;
pub(crate) mod owner_projection;
pub(crate) mod plugin_owner;
mod plugin_claim_eligibility;
pub(crate) mod portable;
pub(crate) mod portable_identity;
pub(crate) mod portable_validation;
mod preservation;
mod query;
mod record_apply;
mod record_projection;
mod replacement_scan;
mod schema;
#[cfg(test)]
mod schema_contract_tests;
pub(crate) mod server_sync_journal;
pub(crate) mod server_sync_projection;
mod repair;
mod snapshot;
mod snapshot_archive;
mod storage_breakdown;
pub(crate) mod sync_selection;

pub(crate) use asset_object_catalog::{AssetObjectCatalog, AssetObjectCatalogPage};
pub(crate) use commands::PersistentStoreState;
pub(crate) use snapshot::RevisionReadLease;
pub(crate) use snapshot::SnapshotRestoreStep;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
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

pub(crate) const DATABASE_FILE: &str = "persistent.sqlite";

pub(super) const CONVERSATION_RANGE_MAX_LIMIT: i64 = 4_096;
pub(super) const JAVASCRIPT_MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
pub(crate) const ASSET_GC_PRODUCT_PAGE_LIMIT: i64 = 128;
const ASSET_GC_PRODUCT_MINIMUM_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
/// How long the objects of a superseded plugin value stay unreferenced before
/// a sweep may delete them.
const SUPERSEDED_OBJECT_GRACE_MS: i64 = 10 * 60 * 1_000;
pub(crate) const MESSAGE_PAGE_SWEEP_LIMIT: usize = 1_024;

/// The store whose message objects one sweep batch visits. Each keeps its own
/// cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageObjectStore {
    Library,
    Device,
}

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
        "character_id, conversation_id, message_index, message_id, value, canonical_hash, canonical_size",
    ),
    (
        "plugin_storage",
        "owner, storage_key, byte_size, ordinal, value, claimed_from, import_batch_id, assigned_at",
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
    ("message_page_indexes", "character_id, conversation_id, page_start, message_count, hash, byte_length"),
    ("message_page_manifests", "character_id, conversation_id, body"),
];

/// What asset GC fails with while a capture or another snapshot operation keeps it from reading
/// the asset inventory. Server sync reports it as `library-operation-busy`.
pub(crate) const ASSET_INVENTORY_BUSY: &str = "Asset inventory is held by another library operation";

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "kebab-case")]
pub(crate) enum StoreError {
    RevisionConflict {
        expected: i64,
        actual: i64,
    },
    SnapshotReleased,
    RawBodyUnavailable,
    CommitBusy,
    CommitDecode { message: String },
    Committed { revision: i64, message: String },
    Validation {
        message: String,
    },
    /// The store was written by a build with a different schema and cannot be opened by this one.
    SchemaMismatch {
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
            Self::RawBodyUnavailable => formatter.write_str("raw commit body unavailable"),
            Self::CommitBusy => formatter.write_str("previous commit is still finishing"),
            Self::Committed { message, .. } | Self::CommitDecode { message } | Self::Validation { message } | Self::SchemaMismatch { message } | Self::Store { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

impl crate::native_log::CommandFailure for StoreError {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::RevisionConflict { .. } => "revision-conflict",
            Self::SnapshotReleased => "snapshot-released",
            Self::RawBodyUnavailable => "raw-body-unavailable",
            Self::CommitBusy => "commit-busy",
            Self::CommitDecode { .. } => "commit-decode",
            Self::Committed { .. } => "committed",
            Self::Validation { .. } => "validation",
            Self::SchemaMismatch { .. } => "schema-mismatch",
            Self::Store { .. } => "store-error",
        }
        .into()
    }
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(self.to_string().into())
    }
    fn expected(&self) -> bool {
        match self {
            Self::RevisionConflict { .. } | Self::CommitBusy | Self::SnapshotReleased => true,
            Self::Validation { message } => {
                message == crate::data_health::CANCELLED
                    || message == archive::ARCHIVE_CANCELLED_MESSAGE
                    || message == export::EXPORT_CANCELLED_MESSAGE
            }
            _ => false,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store {
            message: crate::native_log::sqlite_failure(&error),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Store {
            message: crate::native_log::io_failure(&error),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store {
            message: crate::native_log::json_failure(&error),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trash_stamp_ms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) archived: Option<ArchivedCharacterSummary>,
}

/// What the list needs about an archived character. The stored object hash and
/// the asset hash list stay inside the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivedCharacterSummary {
    pub(crate) archived_at: i64,
    pub(crate) conversation_count: i64,
    pub(crate) message_count: i64,
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
    pub(crate) owner: String,
    pub(crate) key: String,
    pub(crate) byte_size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageValueCursor {
    pub(crate) owner: String,
    pub(crate) key: String,
    pub(crate) ordinal: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageValueQuery {
    pub(crate) owner: Option<String>,
    pub(crate) after_key: Option<PluginStorageValueCursor>,
    pub(crate) limit: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageValuePage {
    pub(crate) items: Vec<PluginStorageValue>,
    pub(crate) next_cursor: Option<PluginStorageValueCursor>,
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageValue {
    pub(crate) owner: String,
    pub(crate) key: String,
    pub(crate) value: Value,
}

/// What the plugin data screen lists. Values stay in the store; the screen asks
/// for one when the reader opens it.
/// A claim answers with the value it handed over and the revision it left the
/// library at, so the renderer's write coordinator stays in step.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaimedPluginValue {
    pub(crate) value: Option<Value>,
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssignedPluginStorage {
    #[serde(flatten)]
    pub(crate) outcome: commit::AssignOutcome,
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginStorageListItem {
    pub(crate) owner: String,
    pub(crate) key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) space: Option<String>,
    pub(crate) value_type: String,
    pub(crate) byte_size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) claimed_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) import_batch_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assigned_at: Option<i64>,
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
    RootModuleAssets { module_id: String },
    PersonaEmbeddedModuleAssets { persona_id: String, module_id: String },
}

impl AssetOwnerLocator {
    fn validate(&self) -> StoreResult<()> {
        match self {
            Self::CharacterAdditionalAssets { character_id } if character_id.is_empty() => {
                Err(StoreError::Validation {
                    message: "Character asset owner requires a nonempty characterId".to_owned(),
                })
            }
            Self::RootModuleAssets { module_id }
                if module_id.is_empty() =>
            {
                Err(StoreError::Validation {
                    message: "Asset owner requires a nonempty moduleId"
                        .to_owned(),
                })
            }
            Self::PersonaEmbeddedModuleAssets { persona_id, module_id } if persona_id.is_empty() || module_id.is_empty() => {
                Err(StoreError::Validation {message:"Asset owner requires nonempty personaId and moduleId".to_owned()})
            }
            _ => Ok(()),
        }
    }

    fn storage_identity(&self) -> (&'static str, String) {
        match self {
            Self::CharacterAdditionalAssets { character_id } => {
                ("character-additional-assets", character_id.clone())
            }
            Self::RootModuleAssets { module_id } => ("root-module-assets", module_id.clone()),
            Self::PersonaEmbeddedModuleAssets { persona_id, module_id } => {
                ("persona-embedded-module-assets", serde_json::to_string(&[persona_id,module_id]).expect("string owner identity"))
            }
        }
    }

    pub(crate) fn from_storage(kind:&str,locator:&str)->StoreResult<Self> {
        let owner=match kind {
            "character-additional-assets"=>Self::CharacterAdditionalAssets{character_id:locator.to_owned()},
            "root-module-assets"=>Self::RootModuleAssets{module_id:locator.to_owned()},
            "persona-embedded-module-assets"=>{let [persona_id,module_id]:[String;2]=serde_json::from_str(locator)?;
                if serde_json::to_string(&[&persona_id,&module_id])?!=locator {return Err(StoreError::Validation{message:"Asset owner identity is not canonical".into()});}
                Self::PersonaEmbeddedModuleAssets{persona_id,module_id}},
            _=>return Err(StoreError::Validation{message:"Stored asset owner kind is invalid".into()}),
        };owner.validate()?;Ok(owner)
    }
    pub(crate) fn root_parent<'a>(&self,root:&'a Value)->Option<&'a Value> {
        match self {
            Self::RootModuleAssets{module_id}=>root.get("modules")?.as_array()?.iter().find(|m|m.get("id").and_then(Value::as_str)==Some(module_id)),
            Self::PersonaEmbeddedModuleAssets{persona_id,module_id}=>root.get("personas")?.as_array()?.iter().find(|p|p.get("id").and_then(Value::as_str)==Some(persona_id))?.get("embeddedModule").filter(|m|m.get("id").and_then(Value::as_str)==Some(module_id)),
            Self::CharacterAdditionalAssets{..}=>None,
        }
    }
    pub(super) fn logical(&self)->crate::logical_records::LogicalOwnerLocator {
        use crate::logical_records::LogicalOwnerLocator;
        match self {
            Self::CharacterAdditionalAssets{character_id}=>LogicalOwnerLocator::CharacterAdditional{character_id:character_id.clone()},
            Self::RootModuleAssets{module_id}=>LogicalOwnerLocator::RootModule{module_id:module_id.clone()},
            Self::PersonaEmbeddedModuleAssets{persona_id,module_id}=>LogicalOwnerLocator::PersonaEmbeddedModule{persona_id:persona_id.clone(),module_id:module_id.clone()},
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
pub(crate) struct AuthorNotePresetUsage {
    pub(crate) character_id: String,
    pub(crate) character_name: String,
    pub(crate) conversation_id: String,
    pub(crate) conversation_name: String,
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
    /// Lets a metadata window read report every message as not parser-inert instead of scanning bodies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) skip_parser_work: Option<bool>,
}

/// `M` is `StoredJson` where messages go to the renderer as stored, without parsing them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationWindow<M = Value> {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    pub(crate) messages: Vec<M>,
    pub(crate) start_index: i64,
    pub(crate) end_index: i64,
    pub(crate) total_messages: i64,
    pub(crate) has_more_before: bool,
    pub(crate) has_more_after: bool,
}

/// A stored JSON text, validated and serialized verbatim.
pub(crate) type StoredJson = Box<serde_json::value::RawValue>;

/// A conversation as `read_conversation` returns it, with the messages kept as stored.
#[derive(Debug)]
pub(crate) struct StoredConversation {
    pub(crate) detail: serde_json::Map<String, Value>,
    pub(crate) messages: Vec<StoredJson>,
}

impl Serialize for StoredConversation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // The shape and key order of the detail with `message` inserted into it.
        let replaces = self.detail.contains_key("message");
        let mut map = serializer.serialize_map(Some(self.detail.len() + usize::from(!replaces)))?;
        for (key, value) in &self.detail {
            if key == "message" {
                map.serialize_entry(key, &self.messages)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        if !replaces {
            map.serialize_entry("message", &self.messages)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationMessageMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) chat_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled: Option<Value>,
    pub(crate) parser_inert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationMessageMetadataWindow {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    pub(crate) messages: Vec<ConversationMessageMetadata>,
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
    /// Lists every conversation of the character in its new order.
    Reorder {
        character_id: String,
        conversation_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum PluginStorageMutation {
    Set {
        owner: String,
        key: String,
        value: Value,
    },
    Delete {
        owner: String,
        key: String,
    },
    Clear {
        owner: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkingSetCommit {
    pub(crate) expected_revision: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gallery_pending_fence: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) device_plugin_fence: Option<device_store::plugin_gc::PluginGcFence>,
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
    pub(crate) delete_character_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plugin_storage: Option<Vec<PluginStorageMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) asset_owner_heads: Option<Vec<AssetOwnerHead>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unit_mutations: Option<Vec<lww::UnitMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) messages_changed: Option<Vec<lww::MessageLocator>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) binding_authority: Option<risunest_sync_wire::stamp::DecimalU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
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
    gc_instance: String,
    gallery: gallery::GallerySession,
    revision_leases: HashMap<String, snapshot::RevisionReadLease>,
    active_readers: Arc<snapshot::ActiveReaderRegistry>,
    connection: Connection,
    repository_root: PathBuf,
    database_path: PathBuf,
    snapshots_dir: PathBuf,
    // A device store that cannot be opened must not block the library, so the
    // failure is carried until something actually needs per-device state.
    device_store: Result<device_store::DeviceStore, StoreError>,
    message_object_roots: MessageObjectRootsCache,
    status_library_roots: Arc<StatusLibraryRoots>,
    checked_representative_images: Arc<std::sync::Mutex<asset_residency::CheckedRepresentativeImages>>,
}

/// Disk-backed roots each sweep target last read, kept while neither
/// connection has seen a change and the same revision leases are held.
#[derive(Default)]
struct MessageObjectRootsCache {
    library: Option<(MessageObjectRootsToken, message_pages::ObjectRoots, tempfile::TempDir)>,
    device: Option<(MessageObjectRootsToken, message_pages::ObjectRoots, tempfile::TempDir)>,
    #[cfg(test)]
    computed: usize,
}

/// Commits by other connections move `data_version`; this store's own
/// writes move `total_changes`. Both are (library, device).
#[derive(PartialEq, Eq)]
struct MessageObjectRootsToken {
    versions: (i64, i64),
    changes: (u64, u64),
    leases: Vec<String>,
}

impl MessageObjectRootsToken {
    fn read(library: &Connection, device: &Connection, leases: Vec<String>) -> StoreResult<Self> {
        let version = |db: &Connection| db.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0));
        Ok(Self {
            versions: (version(library)?, version(device)?),
            changes: (library.total_changes(), device.total_changes()),
            leases,
        })
    }
}

impl MessageObjectRootsCache {
    fn roots(
        &mut self,
        target: MessageObjectStore,
        token: MessageObjectRootsToken,
        repository_root: &Path,
        read: impl FnOnce(Connection) -> StoreResult<message_pages::ObjectRoots>,
    ) -> StoreResult<&message_pages::ObjectRoots> {
        let slot = match target {
            MessageObjectStore::Library => &mut self.library,
            MessageObjectStore::Device => &mut self.device,
        };
        let current = match slot.take() {
            Some((cached, roots, scratch)) if cached == token => (cached, roots, scratch),
            _ => {
                let scratch = crate::external_storage::leftovers::managed_scratch(repository_root, "message-roots-")
                    .map_err(|error| StoreError::Store { message: error.to_string() })?;
                let roots = read(crate::sqlite_open::open(scratch.path().join("roots.sqlite"))?)?;
                #[cfg(test)]
                { self.computed += 1; }
                (token, roots, scratch)
            }
        };
        Ok(&slot.insert(current).1)
    }

    /// Moves the counters of roots that were current when the sweep began past
    /// its own commit. Its cursor, marks and deletions reach no target's roots.
    fn committed(&mut self, before: (u64, u64), library: &Connection, device: &Connection) {
        let after = (library.total_changes(), device.total_changes());
        for (token, _, _) in [&mut self.library, &mut self.device].into_iter().flatten() {
            if token.changes == before {
                token.changes = after;
            }
        }
    }
}

/// Bodies of the pages server sync received and has not staged yet. Only its
/// log names their objects, so a sweep reads them on every pass and never
/// creates the log.
fn received_page_bodies(repository_root: &Path) -> StoreResult<Vec<Value>> {
    let path = crate::server_sync::lww_client::OperationLog::path(repository_root);
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let log = crate::sqlite_open::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    log.busy_timeout(Duration::from_secs(5))?;
    let present: bool = log.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='receive_pages')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        return Ok(Vec::new());
    }
    let mut statement = log.prepare("SELECT body FROM receive_pages")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn mark_other_copies(other: &Connection, deleted: &[String], since: i64) -> StoreResult<()> {
    if deleted.is_empty() {
        return Ok(());
    }
    let transaction = other.unchecked_transaction()?;
    message_pages::mark_unreferenced(&transaction, deleted.iter().map(String::as_str), since)?;
    Ok(transaction.commit()?)
}

fn received_roots(
    repository_root: &Path,
    bodies: &[Value],
    objects: &Connection,
) -> StoreResult<Option<(message_pages::ObjectRoots, tempfile::TempDir)>> {
    if bodies.is_empty() {
        return Ok(None);
    }
    let scratch = crate::external_storage::leftovers::managed_scratch(repository_root, "message-roots-")
        .map_err(|error| StoreError::Store { message: error.to_string() })?;
    let roots = message_pages::json_roots(objects, bodies, crate::sqlite_open::open(scratch.path().join("roots.sqlite"))?)?;
    Ok(Some((roots, scratch)))
}

/// One generation, held open for a diagnosis. It outlives the store mutex on purpose: a deep
/// scan reads every stored object, and the library stays writable while it does.
pub(crate) struct DataHealthReader {
    connection: Connection,
    cas: crate::asset_repository::PayloadCas,
    revision: i64,
    quarantined: Vec<lww::QuarantinedIntent>,
}

impl DataHealthReader {
    pub(crate) fn revision(&self) -> i64 {
        self.revision
    }

    pub(crate) fn scan(
        &self,
        limit: usize,
        probe: &dyn crate::local_backup::CancellationProbe,
    ) -> StoreResult<crate::data_health::Findings> {
        let mut findings = crate::data_health::Findings::new(limit);
        self.note_quarantined_intents(&mut findings);
        crate::portable_backup::scan_live_library(&self.connection, &self.cas, &mut findings, probe)
        .map_err(scan_failure)?;
        self.note_unreferenced_objects(&mut findings, probe)?;
        Ok(findings)
    }

    /// Reports the changes recovery set aside because their replay kept failing. They come
    /// first, so the bound on the report never drops them.
    fn note_quarantined_intents(&self, findings: &mut crate::data_health::Findings) {
        use crate::data_health::{codes, FindingSink, Finding};
        for intent in &self.quarantined {
            let mut finding = Finding::new(codes::INTENT_QUARANTINED, "intent", intent.request_id.clone(), intent.error.clone())
                .targeting("intent", intent.kind.clone());
            finding.intent_action = if intent.discardable {
                Some(crate::data_health::IntentAction::Discard)
            } else if intent.recoverable {
                Some(crate::data_health::IntentAction::Complete)
            } else { None };
            findings.record(if finding.intent_action.is_some() {
                finding.at(intent.token.clone(), 0)
            } else {
                finding
            });
        }
    }

    /// Reports the stored objects no alias in this generation names and no archived character
    /// keeps. They cost space and nothing else, so the diagnosis only counts them and sends the
    /// reader to the cleanup, which is the only place a file is actually deleted.
    fn note_unreferenced_objects(
        &self,
        findings: &mut crate::data_health::Findings,
        probe: &dyn crate::local_backup::CancellationProbe,
    ) -> StoreResult<()> {
        use crate::data_health::{codes, FindingSink, Finding};
        let mut statement = self.connection.prepare(
            "WITH archived(value) AS (SELECT archived_object FROM characters WHERE json_valid(archived_object))
             SELECT object_hash,byte_size FROM asset_objects WHERE object_hash NOT IN (
                 SELECT object_hash FROM asset_aliases WHERE object_hash IS NOT NULL
                 UNION SELECT manifest_hash FROM asset_owner_heads WHERE present = 1
                 UNION SELECT json_extract(value,'$.objectHash') FROM archived
                 UNION SELECT json_extract(value,'$.sharedObjectHash') FROM archived
                 UNION SELECT hashes.value FROM archived, json_each(archived.value,'$.assetHashes') AS hashes
                 UNION SELECT hashes.value FROM archived, json_each(archived.value,'$.sharedAssetHashes') AS hashes
             ) ORDER BY object_hash",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if probe.is_cancelled() {
                return Err(StoreError::Validation {
                    message: crate::data_health::CANCELLED.to_owned(),
                });
            }
            let hash: String = row.get(0)?;
            let bytes: i64 = row.get(1)?;
            findings.note(
                Finding::new(
                    codes::OBJECT_UNREFERENCED,
                    "asset",
                    hash,
                    format!("{bytes} bytes no record uses"),
                )
                .targeting("bytes", bytes.to_string()),
            );
        }
        Ok(())
    }

    pub(crate) fn object_totals(&self) -> StoreResult<crate::portable_backup::ObjectTotals> {
        crate::portable_backup::registered_object_totals(&self.connection, &self.cas).map_err(scan_failure)
    }

    pub(crate) fn scan_objects(
        &self,
        after: Option<&str>,
        budget: u64,
        limit: usize,
        probe: &dyn crate::local_backup::CancellationProbe,
    ) -> StoreResult<(crate::portable_backup::ObjectPage, crate::data_health::Findings)> {
        let mut findings = crate::data_health::Findings::new(limit);
        let page = crate::portable_backup::scan_registered_objects(
            &self.connection,
            &self.cas,
            after,
            budget,
            &mut findings,
            probe,
        )
        .map_err(scan_failure)?;
        Ok((page, findings))
    }
}

fn scan_failure(error: crate::portable_backup::Error) -> StoreError {
    match error {
        crate::portable_backup::Error::Cancelled => StoreError::Validation {
            message: crate::data_health::CANCELLED.to_owned(),
        },
        error => StoreError::Store {
            message: error.to_string(),
        },
    }
}

#[cfg(test)]
std::thread_local! {
    static ASSET_GC_ROOT_COLLECTIONS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    pub(crate) static ASSET_GC_LIBRARY_ROOT_COLLECTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ASSET_GC_FINAL_MARKINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The library's own asset roots from one cleanup run, reused by its later pages while the
/// database reports no change since they were collected, and the marks of the last page's
/// complete roots.
pub(crate) struct AssetGcLibraryRoots {
    data_version: i64,
    total_changes: i64,
    roots: crate::asset_repository::migration_gc::AssetRootSet,
    marks: Option<(
        Vec<crate::asset_repository::migration_gc::AssetRootSet>,
        std::sync::Arc<crate::asset_repository::migration_gc::AssetGcMarks>,
    )>,
}

/// The library roots the last residency status read, shared by a store and the job
/// stores opened from it. A connection that never writes reads the library's
/// `data_version` before each collection, so any commit since makes the next
/// status read the roots again.
#[derive(Default)]
struct StatusLibraryRoots(std::sync::Mutex<StatusLibraryRootsState>);

#[derive(Default)]
struct StatusLibraryRootsState {
    observer: Option<Connection>,
    roots: Option<AssetGcLibraryRoots>,
}

impl StatusLibraryRoots {
    /// The library's `data_version` now and the roots stored last, or `None`
    /// when the version cannot be read.
    fn take(&self, database_path: &Path) -> Option<(i64, Option<AssetGcLibraryRoots>)> {
        let mut state = self.lock();
        if state.observer.is_none() {
            state.observer = Self::open_observer(database_path).ok();
        }
        let data_version = state.observer.as_ref()?
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .ok()?;
        Some((data_version, state.roots.take()))
    }

    fn put(&self, roots: Option<AssetGcLibraryRoots>) {
        self.lock().roots = roots;
    }

    /// Opens the library as the revision readers do, never creating it.
    fn open_observer(database_path: &Path) -> StoreResult<Connection> {
        let open = |flags| -> StoreResult<Connection> {
            let connection = crate::sqlite_open::open_with_flags(database_path, flags)?;
            connection.execute_batch("PRAGMA query_only = ON;")?;
            Ok(connection)
        };
        let preferred = snapshot::revision_reader_open_flags_for_target(cfg!(target_os = "android"));
        if cfg!(target_os = "android") {
            return open(preferred);
        }
        open(preferred).or_else(|_| open(OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StatusLibraryRootsState> {
        self.0.lock().unwrap_or_else(|poisoned| {
            self.0.clear_poison();
            let mut state = poisoned.into_inner();
            state.roots = None;
            state
        })
    }
}

pub(crate) struct AssetGcPreview {
    scan: crate::asset_repository::PayloadCasReadScan,
    residency: crate::server_sync::residency::Residency,
    marks: crate::asset_repository::migration_gc::AssetGcMarks,
    /// What holds an object besides the library, so a retained candidate can say why.
    holders: Vec<(&'static str, std::collections::BTreeSet<String>)>,
    /// Objects of unreferenced aliases written within the grace, which the cleanup keeps for now.
    recent_orphan_objects: std::collections::BTreeSet<String>,
}

/// Moves the objects of recently written unreferenced aliases from deletable to recent, since the
/// cleanup deletes those aliases only after the grace.
fn keep_recent_orphan_objects(
    report: &mut crate::asset_repository::migration_gc::AssetGcDryRunReport,
    recent: &std::collections::BTreeSet<String>,
    sizes: &std::collections::HashMap<String, u64>,
) {
    if recent.is_empty() {
        return;
    }
    let (kept, deletable): (Vec<_>, Vec<_>) = std::mem::take(&mut report.potential_delete_hashes)
        .into_iter()
        .partition(|hash| recent.contains(hash));
    for hash in kept {
        report.potential_delete_bytes = report.potential_delete_bytes.saturating_sub(sizes.get(&hash).copied().unwrap_or(0));
        report.grace_retained_hashes.push(hash);
    }
    report.grace_retained_hashes.sort();
    report.potential_delete_hashes = deletable;
}

/// One stored object the cleanup looked at, and what it decided.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssetGcCandidateDetail {
    pub(crate) object_hash: String,
    pub(crate) bytes: u64,
    pub(crate) created_at_ms: i64,
    /// `deletable`, `recent` or `held`.
    pub(crate) state: &'static str,
    /// Named holders for a retained object. Empty with `held` means the library still uses it.
    pub(crate) holders: Vec<&'static str>,
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
    /// Free pages of the library and device store files, which compaction returns.
    pub(crate) database_reclaimable_bytes: u64,
    /// Every catalogued object, including those whose body is not on this device.
    pub(crate) asset_objects: StorageCountBytes,
    /// The object files this device stores.
    pub(crate) asset_bodies: StorageCountBytes,
    /// Catalogued objects whose body is not on this device, by catalog size.
    pub(crate) missing_asset_bodies: StorageCountBytes,
    /// The distinct inlay objects of the active generation stored on this device.
    pub(crate) inlay_bodies: StorageCountBytes,
    pub(crate) asset_aliases: Vec<StorageAliasStats>,
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

    pub(crate) fn create_attached_export(
        &self,
        omit_account: bool,
        account: Option<&Value>,
    ) -> StoreResult<export::ExportedRisuSave> {
        let reader = self.reader()?;
        export::create(
            &reader.connection,
            &self.snapshots_dir,
            &reader.target,
            &self.lease,
            omit_account,
            account,
        )
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
        account: &Value,
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
            account,
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

/// `app_kv` carries only the job markers that must land in the same commit as
/// the replacement they authorize. Everything else a device keeps belongs to
/// `device.sqlite` or to the OS vault, neither of which a restore replaces.
const APP_KV_KEY_PREFIXES: [&str; 2] = ["device-backup-commit:", "external-restore-commit:"];

pub(super) fn validate_app_kv_key(key: &str) -> StoreResult<()> {
    if APP_KV_KEY_PREFIXES
        .iter()
        .any(|prefix| key.len() > prefix.len() && key.starts_with(prefix))
    {
        return Ok(());
    }
    Err(StoreError::Validation {
        message: "app_kv key is outside the allowed job marker key space".to_owned(),
    })
}

fn open_device_store(persistent_dir: &Path) -> Result<device_store::DeviceStore, StoreError> {
    device_store::DeviceStore::open(persistent_dir).map_err(|error| {
        let message = format!("device store is unavailable: {error}");
        crate::nlog!("warn", "{message}");
        match error {
            StoreError::SchemaMismatch { .. } => StoreError::SchemaMismatch { message },
            _ => StoreError::Store { message },
        }
    })
}

fn unavailable_device_store(error: &StoreError) -> StoreError {
    match error {
        StoreError::SchemaMismatch { message } => StoreError::SchemaMismatch { message: message.clone() },
        other => StoreError::Store { message: other.to_string() },
    }
}

pub(crate) fn register_asset_objects_at_root(
    repository_root: &Path,
    objects: &[asset_object_catalog::AssetObjectRegistration],
    created_at_ms: i64,
) -> StoreResult<()> {
    let database_path = repository_root.join("persistent").join(DATABASE_FILE);
    let mut connection =
        crate::sqlite_open::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    AssetObjectCatalog::new(&mut connection).register(objects, created_at_ms)
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
        let retained_stage = crate::device_backup::active_native_portable_stage(app_data_dir)
            .map_err(|failure| StoreError::Validation {
                message: failure.to_string(),
            })?;
        let persistent_dir = app_data_dir.join("persistent");
        let snapshots_dir = persistent_dir.join("snapshots");
        std::fs::create_dir_all(&snapshots_dir)?;

        let database_path = persistent_dir.join(DATABASE_FILE);
        let mut connection = crate::sqlite_open::open(&database_path)?;
        schema::initialize(&mut connection)?;
        plugin_claim_eligibility::close(&connection)?;
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
        let seeded: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM generations WHERE state = 'active')",
            [],
            |row| row.get(0),
        )?;
        if !seeded {
            let active = active_generation(&transaction)?;
            transaction.execute(
                "INSERT INTO generations (id, state) VALUES (?1, 'active')",
                [&active],
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO root (generation, value) VALUES (?1, ?2)",
                params![active, "{}"],
            )?;
        }
        let active = active_generation(&transaction)?;
        message_pages::forget_orphaned_pages(&transaction, Some(&active))?;
        transaction.commit()?;
        export::sweep_abandoned(&snapshots_dir)?;
        #[cfg(feature = "native-kei-upload-pilot")]
        kei::sweep_abandoned(&snapshots_dir);

        let active_readers = Arc::new(snapshot::ActiveReaderRegistry::default());
        let device_store = open_device_store(&persistent_dir);
        let mut store = Self {
            gc_instance: uuid::Uuid::new_v4().to_string(),
            gallery: gallery::GallerySession::default(),
            revision_leases: HashMap::new(),
            active_readers,
            connection,
            repository_root: app_data_dir.to_owned(),
            database_path,
            snapshots_dir,
            device_store,
            message_object_roots: MessageObjectRootsCache::default(),
            status_library_roots: Arc::default(),
            checked_representative_images: Arc::default(),
        };
        // A pending intent may still need its staging generation, so the sweep waits until
        // every intent recovery still runs has settled.
        if store.device_store.is_ok() && store.lww_recover_intents_at_open()? {
            snapshot::sweep_temporary_generations(&mut store.connection, retained_stage.as_deref())?;
        }
        snapshot::checkpoint(&store.connection, CheckpointMode::Truncate)?;
        if store.device_store.is_ok() {
            if let Err(error) = store.server_repair_residency_access() {
                crate::nlog!("warn", "server sync residency repair pending: {}", error.code);
            }
        }
        Ok(store)
    }

    pub(crate) fn open_native_job_store(&self) -> StoreResult<Self> {
        let mut connection = crate::sqlite_open::open(&self.database_path)?;
        schema::initialize(&mut connection)?;
        let device_store = open_device_store(&self.repository_root.join("persistent"));
        Ok(Self {
            gc_instance: uuid::Uuid::new_v4().to_string(),
            gallery: gallery::GallerySession::default(),
            revision_leases: HashMap::new(),
            active_readers: Arc::clone(&self.active_readers),
            connection,
            repository_root: self.repository_root.clone(),
            database_path: self.database_path.clone(),
            snapshots_dir: self.snapshots_dir.clone(),
            device_store,
            message_object_roots: MessageObjectRootsCache::default(),
            status_library_roots: Arc::clone(&self.status_library_roots),
            checked_representative_images: Arc::clone(&self.checked_representative_images),
        })
    }

    pub(crate) fn device_store(&self) -> StoreResult<&device_store::DeviceStore> {
        self.device_store.as_ref().map_err(unavailable_device_store)
    }

    pub(crate) fn device_store_mut(&mut self) -> StoreResult<&mut device_store::DeviceStore> {
        self.device_store.as_mut().map_err(|error| unavailable_device_store(error))
    }

    pub(crate) fn revision(&self) -> StoreResult<i64> {
        current_revision(&self.connection)
    }

    pub(crate) fn library_identity(&self) -> StoreResult<String> {
        active_generation(&self.connection)
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

    pub(crate) fn read_character_summary(
        &self,
        id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<CharacterSummary>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_character_summary(connection, id, &target)
    }

    pub(crate) fn query_conversations(
        &self,
        query: &ConversationQuery,
        lease: Option<&str>,
    ) -> StoreResult<ConversationPage> {
        let (connection, target) = self.read_view(lease)?;
        query::query_conversations(connection, query, &target)
    }

    pub(crate) fn query_author_note_preset_usage(
        &self,
        preset_id: &str,
    ) -> StoreResult<Vec<AuthorNotePresetUsage>> {
        let (connection, target) = self.read_view(None)?;
        query::query_author_note_preset_usage(connection, preset_id, &target)
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

    /// `read_conversation` for the renderer, which receives the messages as stored.
    pub(crate) fn read_stored_conversation(
        &self,
        character_id: &str,
        conversation_id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<StoredConversation>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_stored_conversation(connection, character_id, conversation_id, &target)
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

    /// `read_conversation_window` for the renderer, which receives the messages as stored.
    pub(crate) fn read_stored_conversation_window(
        &self,
        query: &ConversationWindowQuery,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<ConversationWindow<StoredJson>>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_stored_conversation_window(connection, query, &target)
    }

    pub(crate) fn read_conversation_message_metadata_window(
        &self,
        query: &ConversationWindowQuery,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<ConversationMessageMetadataWindow>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_conversation_message_metadata_window(connection, query, &target)
    }

    pub(crate) fn query_plugin_storage(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<PluginStorageCatalog> {
        self.query_plugin_storage_for_owner(lease, None)
    }

    pub(crate) fn query_plugin_storage_for_owner(
        &self,
        lease: Option<&str>,
        owner: Option<&str>,
    ) -> StoreResult<PluginStorageCatalog> {
        let (connection, target) = self.read_view(lease)?;
        query::query_plugin_storage(connection, &target, owner)
    }

    pub(crate) fn read_plugin_storage_page(
        &self,
        input: &PluginStorageValueQuery,
        lease: Option<&str>,
    ) -> StoreResult<PluginStorageValuePage> {
        let (connection, target) = self.read_view(lease)?;
        query::read_plugin_storage_page(connection, &target, input)
    }

    pub(crate) fn list_plugin_storage(
        &self,
        lease: Option<&str>,
    ) -> StoreResult<Vec<PluginStorageListItem>> {
        let (connection, target) = self.read_view(lease)?;
        query::list_plugin_storage(connection, &target)
    }

    /// Opens this plugin's one chance to take values an upstream save left
    /// without an owner. Answers with nothing when no import is waiting or when
    /// this plugin already had its window for that import.
    pub(crate) fn begin_plugin_claim_session(
        &self,
        owner: &str,
        code_hash: &str,
        runtime_instance: &str,
    ) -> StoreResult<Option<String>> {
        let Some(batch) = commit::pending_plugin_import_batch(&self.connection)? else {
            return Ok(None);
        };
        if !plugin_claim_eligibility::consume(&self.connection, &batch, owner, code_hash)? {
            return Ok(None);
        }
        self.device_store()?.open_plugin_claim_session(
            &batch,
            owner,
            code_hash,
            runtime_instance,
            device_store::now_ms()?,
        )
    }

    pub(crate) fn claim_plugin_storage_value(
        &mut self,
        session_id: &str,
        owner: &str,
        code_hash: &str,
        runtime_instance: &str,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<ClaimedPluginValue> {
        let now = device_store::now_ms()?;
        let batch = self.device_store()?.plugin_claim_session_batch(
            session_id,
            owner,
            code_hash,
            runtime_instance,
            now,
        )?;
        let Some(batch) = batch else {
            return Ok(ClaimedPluginValue {
                value: None,
                revision: expected_revision,
            });
        };
        let (value, revision) = self.lww_claim_plugin_value(
            owner,
            key,
            &batch,
            now,
            expected_revision,
        )?;
        Ok(ClaimedPluginValue { value, revision })
    }

    pub(crate) fn close_plugin_claim_eligibility(&self) -> StoreResult<()> {
        plugin_claim_eligibility::close(&self.connection)
    }

    pub(crate) fn close_plugin_claim_session(&self, session_id: &str) -> StoreResult<()> {
        self.device_store()?.close_plugin_claim_session(session_id)
    }

    pub(crate) fn colliding_plugin_storage_keys(
        &self,
        owner: &str,
        keys: &[String],
    ) -> StoreResult<Vec<String>> {
        commit::colliding_plugin_storage_keys(&self.connection, owner, keys)
    }

    pub(crate) fn assign_plugin_storage(
        &mut self,
        sources: &[(String, String)],
        owner: &str,
        collision: commit::AssignCollision,
        expected_revision: i64,
    ) -> StoreResult<AssignedPluginStorage> {
        let assigned_at = device_store::now_ms()?;
        let (outcome, revision) = self.lww_assign_plugin_storage(
            sources,
            owner,
            collision,
            assigned_at,
            expected_revision,
        )?;
        Ok(AssignedPluginStorage { outcome, revision })
    }

    pub(crate) fn read_plugin_storage(
        &self,
        owner: &str,
        key: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_plugin_storage(connection, owner, key, &target)
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

    pub(crate) fn read_asset_owner_head(
        &self,
        owner: &AssetOwnerLocator,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<AssetOwnerHead>>> {
        let (connection, target) = self.read_view(lease)?;
        query::read_asset_owner_head(connection, owner, &target)
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

    /// Root fields and presets with the number of characters (see
    /// [`query::binding_library`]).
    pub(crate) fn binding_library(&self) -> StoreResult<(Value, u64)> {
        let (mut database, characters, target) = query::binding_library(&self.connection)?;
        let root = database.as_object_mut().ok_or_else(|| StoreError::Store {
            message: "Persistent root must be an object".to_owned(),
        })?;
        owner_projection::OwnerManifestProjector::from_snapshots_dir(
            &self.connection,
            &target,
            &self.snapshots_dir,
        )?
        .project_root(root)?;
        Ok((database, characters))
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
        let document = crate::generation_diagnostics::commit_document();
        let mut committed_revision = None;
        let result = self.lww_commit(commit, asset_aliases, &mut committed_revision);
        crate::generation_diagnostics::commit_finished(document, commit.expected_revision, &result, committed_revision);
        result
    }

    /// `commit` with `hold` taken before the library commit and kept until it ends.
    pub(crate) fn commit_holding<G>(
        &mut self,
        commit: &WorkingSetCommit,
        hold: impl FnOnce(&Self) -> StoreResult<G>,
    ) -> StoreResult<RevisionResult> {
        let document = crate::generation_diagnostics::commit_document();
        let mut committed_revision = None;
        let result = self.lww_commit_holding(commit, &[], &mut committed_revision, hold);
        crate::generation_diagnostics::commit_finished(document, commit.expected_revision, &result, committed_revision);
        result
    }

    pub(crate) fn commit_asset_alias(
        &mut self,
        alias: &AssetAlias,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        self.commit_with_asset_aliases(&WorkingSetCommit{expected_revision,..Default::default()}, &[alias.clone()])
    }

    pub(crate) fn delete_asset_alias(
        &mut self,
        kind: &str,
        key: &str,
        expected_revision: i64,
    ) -> StoreResult<RevisionResult> {
        self.commit_with_asset_aliases(&WorkingSetCommit{expected_revision,unit_mutations:Some(vec![lww::UnitMutation::Delete{key:lww::unit_key(&[kind,key])?}]),..Default::default()}, &[])
    }

    pub(crate) fn archive_preview(
        &self,
        character_id: &str,
        lease: Option<&str>,
    ) -> StoreResult<archive::ArchivePreview> {
        let (connection, target) = self.read_view(lease)?;
        archive::preview(connection, &target.generation, character_id)
    }

    pub(crate) fn archive_character(&mut self,character_id:&str,expected_revision:i64,now_ms:i64)->StoreResult<RevisionResult> {self.lww_archive(character_id,expected_revision,now_ms,false,&||false,None)}
    pub(crate) fn archive_character_with_cancellation(&mut self,character_id:&str,expected_revision:i64,now_ms:i64,is_cancelled:&dyn Fn()->bool,cancellation:Option<std::sync::Arc<std::sync::atomic::AtomicBool>>)->StoreResult<RevisionResult> {self.lww_archive(character_id,expected_revision,now_ms,false,is_cancelled,cancellation)}
    pub(crate) fn restore_character(&mut self,character_id:&str,expected_revision:i64)->StoreResult<RevisionResult> {self.lww_archive(character_id,expected_revision,0,true,&||false,None)}
    pub(crate) fn restore_character_with_cancellation(&mut self,character_id:&str,expected_revision:i64,is_cancelled:&dyn Fn()->bool,cancellation:Option<std::sync::Arc<std::sync::atomic::AtomicBool>>)->StoreResult<RevisionResult> {self.lww_archive(character_id,expected_revision,0,true,is_cancelled,cancellation)}

    pub(crate) fn replace_begin(&mut self) -> StoreResult<StagingResult> {
        commit::replace_begin(&mut self.connection)
    }

    pub(crate) fn replace_put_root(&mut self, staging_id: &str, root: &Value) -> StoreResult<()> {
        commit::replace_put_root(&mut self.connection, staging_id, root)
    }

    pub(crate) fn replace_put_root_with_plugin_storage(
        &mut self,
        staging_id: &str,
        root: &Value,
        plugin_storage: Option<&[PluginStorageValue]>,
    ) -> StoreResult<()> {
        commit::replace_put_root_with_plugin_storage(
            &mut self.connection,
            staging_id,
            root,
            plugin_storage,
        )
    }

    pub(crate) fn staged_plugin_preview(
        &self,
        staging_id: &str,
    ) -> StoreResult<commit::StagedPluginPreview> {
        commit::staged_plugin_preview(&self.connection, &self.repository_root, staging_id)
    }

    pub(crate) fn staged_library_counts(&self, staging_id: &str) -> StoreResult<(u64, u64)> {
        commit::require_staging(&self.connection, staging_id)?;
        let (characters, presets): (i64, i64) = self.connection.query_row(
            "SELECT
                (SELECT count(*) FROM characters WHERE generation=?1),
                (SELECT count(*) FROM bot_presets WHERE generation=?1)",
            [staging_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((characters as u64, presets as u64))
    }

    pub(crate) fn assign_staged_plugin_values(
        &mut self,
        staging_id: &str,
        assignments: &[commit::StagedPluginAssignment],
        automatic: bool,
    ) -> StoreResult<()> {
        commit::assign_staged_plugin_values(
            &mut self.connection,
            staging_id,
            assignments,
            automatic,
        )
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

    pub(crate) fn replace_preserve_repositories(
        &mut self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<RevisionResult> {
        let revision = commit::replace_preserve_repositories(
            &mut self.connection,
            &self.repository_root,
            staging_id,
            expected_revision,
        )?;
        Ok(RevisionResult { revision })
    }

    pub(crate) fn replace_add_characters(
        &mut self,
        staging_id: &str,
        characters: &[Value],
    ) -> StoreResult<()> {
        commit::replace_add_characters(&mut self.connection, staging_id, characters)
    }

    pub(crate) fn replace_put_character_detail(
        &mut self,
        staging_id: &str,
        detail: &Value,
        conversation_count: i64,
    ) -> StoreResult<()> {
        commit::replace_put_character_detail(
            &mut self.connection,
            staging_id,
            detail,
            conversation_count,
        )
    }

    pub(crate) fn replace_put_conversation_row(
        &mut self,
        staging_id: &str,
        character_id: &str,
        configured_index: i64,
        detail: &Value,
        recent_at: i64,
        message_count: i64,
    ) -> StoreResult<()> {
        commit::replace_put_conversation_row(
            &mut self.connection,
            staging_id,
            character_id,
            configured_index,
            detail,
            recent_at,
            message_count,
        )
    }

    pub(crate) fn replace_add_conversation_messages(
        &mut self,
        staging_id: &str,
        character_id: &str,
        conversation_id: &str,
        start: i64,
        messages: &[Value],
    ) -> StoreResult<()> {
        commit::replace_add_conversation_messages(
            &mut self.connection,
            staging_id,
            character_id,
            conversation_id,
            start,
            messages,
        )
    }

    pub(crate) fn replace_put_conversation(
        &mut self,
        staging_id: &str,
        character_id: &str,
        configured_index: i64,
        conversation: &Value,
        message_count: i64,
        last_message_time: Option<&Value>,
    ) -> StoreResult<()> {
        commit::replace_put_conversation(
            &mut self.connection,
            staging_id,
            character_id,
            configured_index,
            conversation,
            message_count,
            last_message_time,
        )
    }

    pub(crate) fn replace_add_presets(
        &mut self,
        staging_id: &str,
        presets: &[Value],
    ) -> StoreResult<()> {
        commit::replace_add_presets(&mut self.connection, staging_id, presets)
    }

    pub(crate) fn replace_add_plugin_storage_values(
        &mut self,
        staging_id: &str,
        values: &[PluginStorageValue],
    ) -> StoreResult<()> {
        commit::replace_add_plugin_storage_values(&mut self.connection, staging_id, values)
    }

    pub(crate) fn replace_add_plugin_storage(
        &mut self,
        staging_id: &str,
        storage: &serde_json::Map<String, Value>,
        meta: Option<&serde_json::Map<String, Value>>,
    ) -> StoreResult<()> {
        commit::replace_add_plugin_storage(&mut self.connection, staging_id, storage, meta)
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

    pub(crate) fn prepare_replace_commit(
        &self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<PreparedReplaceCommit> {
        let revision = self.lww_prepared_replacement_revision(staging_id, expected_revision)?;
        Ok(PreparedReplaceCommit {
            staging_id: staging_id.to_owned(),
            revision,
        })
    }

    pub(crate) fn finish_prepared_replace(
        &mut self,
        prepared: PreparedReplaceCommit,
    ) -> StoreResult<RevisionResult> {
        self.finish_prepared_replace_under(prepared, None)
    }

    /// Commits under `binding_authority` when the caller already recorded one for
    /// this staging, so a binding switch since then fails with
    /// `binding-authority-changed` instead of committing under another authority.
    pub(crate) fn finish_prepared_replace_under(
        &mut self,
        prepared: PreparedReplaceCommit,
        binding_authority: Option<&str>,
    ) -> StoreResult<RevisionResult> {
        self.lww_prepared_replacement_revision(&prepared.staging_id, Some(prepared.revision))?;
        let binding_authority = match binding_authority {
            Some(authority) => risunest_sync_wire::stamp::DecimalU64::try_from(authority.to_owned()).map_err(lww::error)?,
            None => self.lww_binding_authority()?,
        };
        let header=lww::Header{binding_authority,request_id:prepared.staging_id.clone()};
        self.lww_commit_replacement(&header,&prepared.staging_id)
    }

    /// The library rows themselves, for a test that has to look at what a
    /// receive did rather than at what it reported.
    #[cfg(test)]
    pub(crate) fn library_rows(&self) -> &rusqlite::Connection {
        &self.connection
    }

    pub(crate) fn replace_abort(&mut self, staging_id: &str) -> StoreResult<()> {
        commit::replace_abort(&mut self.connection, staging_id)
    }

    /// One bounded step of deleting libraries that an activation or the startup
    /// sweep retired. Returns whether retired libraries remain.
    pub(crate) fn purge_retired_batch(&mut self, limit_rows: usize) -> StoreResult<bool> {
        commit::purge_retired_batch(&mut self.connection, limit_rows)
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

    pub(crate) fn release_all_revision_leases(&mut self) -> StoreResult<()> {
        let mut first_error = None;
        for (_, reader) in self.revision_leases.drain() {
            if let Err(error) = snapshot::close_revision(reader) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        let checkpoint = self.checkpoint_after_release();
        match (first_error, checkpoint) {
            (None, result) => result,
            (Some(error), Ok(())) => Err(error),
            (Some(error), Err(checkpoint_error)) => Err(StoreError::Store {
                message: format!(
                    "{error}; checkpoint after renderer lease cleanup failed: {checkpoint_error}"
                ),
            }),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
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
            None,
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

    pub(crate) fn prepare_gallery_file_export(
        &mut self,
        revision: i64,
        kind: &str,
        key: &str,
    ) -> StoreResult<PreparedRisuSaveExport> {
        let (lease, reader) = snapshot::acquire_revision(
            &self.database_path,
            revision,
            Arc::clone(&self.active_readers),
        )?;
        reader.publish_detached_gallery_file_root(kind, key)?;
        Ok(PreparedRisuSaveExport {
            revision,
            lease,
            snapshots_dir: self.snapshots_dir.clone(),
            database_path: self.database_path.clone(),
            reader: Some(reader),
        })
    }

    pub(crate) fn detach_risu_save_export(
        &mut self,
        lease: &str,
    ) -> StoreResult<PreparedRisuSaveExport> {
        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        let reader = self
            .revision_leases
            .get(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        reader.publish_detached_asset_roots()?;
        let reader = self
            .revision_leases
            .remove(lease)
            .ok_or(StoreError::SnapshotReleased)?;
        Ok(PreparedRisuSaveExport {
            revision: reader.target.revision,
            lease: lease.to_owned(),
            snapshots_dir: self.snapshots_dir.clone(),
            database_path: self.database_path.clone(),
            reader: Some(reader),
        })
    }

    pub(crate) fn reattach_risu_save_export(
        &mut self,
        mut prepared: PreparedRisuSaveExport,
    ) -> StoreResult<()> {
        if self.revision_leases.contains_key(&prepared.lease) {
            return Err(StoreError::Validation {
                message: "revision lease was replaced while its export was detached".to_owned(),
            });
        }
        let lease = prepared.lease.clone();
        let reader = prepared.take_reader()?;
        self.revision_leases.insert(lease, reader);
        Ok(())
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
            crate::asset_repository::job_pins::CasJobOwner::native_file_job(job_id),
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
        account: &Value,
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
            account,
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
        account: &Value,
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
            account,
        ) {
            Ok(prepared) => Ok(prepared),
            Err((error, reader)) => {
                self.revision_leases.insert(lease.to_owned(), reader);
                Err(error)
            }
        }
    }

    pub(crate) fn checkpoint(&self, mode: CheckpointMode) -> StoreResult<()> {
        // A truncate would wait out the busy timeout behind an active read lease. The
        // committed frames stay in the WAL, so a passive checkpoint is all that can run.
        let mode = if mode == CheckpointMode::Truncate && self.active_readers.active_count() > 0 {
            CheckpointMode::Passive
        } else {
            mode
        };
        snapshot::checkpoint(&self.connection, mode)
    }

    /// A save path's checkpoint: a truncate kept busy by a reader outside the lease
    /// registry still leaves every commit in the WAL, so it checkpoints passively.
    pub(crate) fn checkpoint_or_passive(&self, mode: CheckpointMode) -> StoreResult<()> {
        match self.checkpoint(mode) {
            Err(error) if mode == CheckpointMode::Truncate && checkpoint_was_busy(&error) => {
                snapshot::checkpoint(&self.connection, CheckpointMode::Passive)
            }
            result => result,
        }
    }

    #[cfg(test)]
    pub(crate) fn snapshot_create_keeping(
        &mut self,
        reason: &str,
        keep: snapshot::KeepCount,
    ) -> StoreResult<SnapshotCreated> {
        snapshot::create(self, reason, keep)
    }

    #[cfg(test)]
    pub(crate) fn snapshot_create(&mut self, reason: &str) -> StoreResult<SnapshotCreated> {
        self.snapshot_create_keeping(reason, snapshot::KeepCount::MAX)
    }

    #[cfg(test)]
    pub(crate) fn snapshot_list(&self) -> StoreResult<Vec<SnapshotInfo>> {
        snapshot::list(&self.snapshots_dir)
    }

    #[cfg(test)]
    pub(crate) fn snapshot_delete(&self, id: &str) -> StoreResult<()> {
        snapshot_archive::Archive::open(&self.snapshots_dir)?.delete(id)
    }

    pub(crate) fn storage_stats(&self) -> StoreResult<PersistentStorageStats> {
        let bodies =
            crate::asset_repository::PayloadCas::new(&self.repository_root)?.object_lengths()?;
        let transaction = self.connection.unchecked_transaction()?;
        let active = active_generation(&transaction)?;
        let database_bytes = snapshot::allocated_database_bytes(&self.database_path)?;
        let asset_objects = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM asset_objects",
            [],
        )?;
        let asset_bodies = StorageCountBytes {
            count: bodies.len() as u64,
            bytes: bodies.values().sum(),
        };
        let mut missing_asset_bodies = StorageCountBytes { count: 0, bytes: 0 };
        let mut statement = transaction.prepare("SELECT object_hash, byte_size FROM asset_objects")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            if !bodies.contains_key(&hash) {
                missing_asset_bodies.count += 1;
                missing_asset_bodies.bytes += nonnegative_u64(row.get(1)?)?;
            }
        }
        drop(rows);
        drop(statement);
        let mut inlay_bodies = StorageCountBytes { count: 0, bytes: 0 };
        let mut statement = transaction.prepare(
            "SELECT DISTINCT object_hash FROM asset_aliases
             WHERE generation = ?1 AND kind = 'inlay' AND object_hash IS NOT NULL",
        )?;
        let mut rows = statement.query([&active])?;
        while let Some(row) = rows.next()? {
            if let Some(bytes) = bodies.get(&row.get::<_, String>(0)?) {
                inlay_bodies.count += 1;
                inlay_bodies.bytes += bytes;
            }
        }
        drop(rows);
        drop(statement);
        let plugin_storage = query_count_bytes(
            &transaction,
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM plugin_storage WHERE generation = ?1",
            [&active],
        )?;
        let active_characters = query_count_bytes(
            &transaction,
            // octet_length reads each value's stored length instead of the value itself.
            "SELECT COUNT(*), COALESCE(SUM(octet_length(detail)), 0) FROM characters WHERE generation = ?1 AND trashed = 0",
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
            snapshot_bytes: snapshot_archive::file_bytes(&self.snapshots_dir)?,
            database_bytes,
            database_reclaimable_bytes: self.database_pages()?.iter().map(|(_, pages)| pages.free_bytes).sum(),
            asset_objects,
            asset_bodies,
            missing_asset_bodies,
            inlay_bodies,
            asset_aliases: aliases,
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


    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn asset_gc_dry_run(
        &self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        let preview = self.prepare_asset_gc_preview(now_ms)?;
        self.asset_gc_preview_page(&preview, limit, cursor, now_ms, minimum_grace_ms)
    }

    pub(crate) fn prepare_asset_gc_preview(&self, now_ms: i64) -> StoreResult<AssetGcPreview> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let scan = cas.into_read_scan();
        // Unreferenced aliases are not holders, so their files show as the cleanup will treat them.
        let scope = asset_alias_orphans::OrphanAliasScope::Cleanup { now_ms };
        let mut search = self.search_unreferenced_asset_aliases(&scope)?;
        let residency = crate::server_sync::residency::Residency::open(&self.repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let mark = |search: Option<&asset_alias_orphans::OrphanAliasSearch>| -> StoreResult<_> {
            let labelled = match search {
                Some(search) => self.collect_asset_gc_roots_without_unreferenced_aliases(search, true)?,
                None => self.collect_labelled_asset_gc_roots(false, true)?,
            };
            // Only the holders that are small and explainable are kept for attribution; the library
            // itself is the default answer and copying its root set would cost as much as it holds.
            let holders: Vec<(&'static str, std::collections::BTreeSet<String>)> = labelled
                .iter()
                .filter(|(label, _)| *label != "library")
                .map(|(label, roots)| (*label, roots.object_hashes.clone()))
                .collect();
            let marks = crate::asset_repository::migration_gc::mark_asset_roots_with_remote_scan(
                &scan,
                labelled.into_iter().map(|(_, roots)| roots),
                |hash| residency.gc_size(hash),
            )?;
            Ok((holders, marks))
        };
        let (mut holders, mut marks) = mark(search.as_ref())?;
        if let Some(found) = &mut search {
            if self.keep_aliases_archives_name(found, &marks, &scope)? {
                (holders, marks) = mark(Some(&*found))?;
            }
        }
        let recent_orphan_objects = search
            .map(|search| search.orphans(&marks, &scope).1.into_iter().map(|alias| alias.object_hash).collect())
            .unwrap_or_default();
        Ok(AssetGcPreview {
            scan,
            residency,
            marks,
            holders,
            recent_orphan_objects,
        })
    }

    /// The same page the preview reports, with a row per object saying what the cleanup decided
    /// and, when it kept one, what is holding it.
    pub(crate) fn asset_gc_preview_page_detail(
        &self,
        preview: &AssetGcPreview,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<(
        crate::asset_repository::migration_gc::AssetGcDryRunPage,
        Vec<AssetGcCandidateDetail>,
    )> {
        let candidates = self.query_asset_object_catalog(limit, cursor)?;
        let looked_at: Vec<(String, u64, i64)> = candidates
            .items
            .iter()
            .map(|candidate| {
                (
                    candidate.object_hash.clone(),
                    candidate.byte_size,
                    candidate.created_at_ms,
                )
            })
            .collect();
        let mut report = crate::asset_repository::migration_gc::sweep_asset_candidates_with_remote_scan(
            &preview.scan,
            candidates.items,
            &preview.marks,
            now_ms,
            minimum_grace_ms,
            |hash| preview.residency.gc_size(hash),
        )
        .map_err(StoreError::from)?;
        let sizes = looked_at.iter().map(|(hash, bytes, _)| (hash.clone(), *bytes)).collect();
        keep_recent_orphan_objects(&mut report, &preview.recent_orphan_objects, &sizes);
        let deletable: std::collections::BTreeSet<&str> = report
            .potential_delete_hashes
            .iter()
            .map(String::as_str)
            .collect();
        let recent: std::collections::BTreeSet<&str> = report
            .grace_retained_hashes
            .iter()
            .map(String::as_str)
            .collect();
        let details = looked_at
            .into_iter()
            .map(|(object_hash, bytes, created_at_ms)| {
                let state = match object_hash.as_str() {
                    _ if !report.blockers.is_empty() => "blocked",
                    hash if deletable.contains(hash) => "deletable",
                    hash if recent.contains(hash) => "recent",
                    _ => "held",
                };
                let holders = match state {
                    "held" => preview
                        .holders
                        .iter()
                        .filter(|(_, hashes)| hashes.contains(&object_hash))
                        .map(|(label, _)| *label)
                        .collect(),
                    _ => Vec::new(),
                };
                AssetGcCandidateDetail {
                    object_hash,
                    bytes,
                    created_at_ms,
                    state,
                    holders,
                }
            })
            .collect();
        Ok((
            crate::asset_repository::migration_gc::AssetGcDryRunPage {
                report,
                next_cursor: candidates.next_cursor,
            },
            details,
        ))
    }

    pub(crate) fn asset_gc_preview_page(
        &self,
        preview: &AssetGcPreview,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::migration_gc::{
            sweep_asset_candidates_with_remote_scan, AssetGcDryRunPage,
        };

        let candidates = self.query_asset_object_catalog(limit, cursor)?;
        let sizes = candidates.items.iter().map(|candidate| (candidate.object_hash.clone(), candidate.byte_size)).collect();
        let mut report = sweep_asset_candidates_with_remote_scan(
            &preview.scan,
            candidates.items,
            &preview.marks,
            now_ms,
            minimum_grace_ms,
            |hash| preview.residency.gc_size(hash),
        )
        .map_err(StoreError::from)?;
        keep_recent_orphan_objects(&mut report, &preview.recent_orphan_objects, &sizes);
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

    /// One bounded pass over one store's message objects. Roots are read under the
    /// store lock and, for the store being swept, inside its write transaction, so
    /// a reference committed before the pass is seen. They are read again only
    /// after a change on either connection or to the held leases; pages server
    /// sync received and has not staged yet are read on every pass. Callers hold
    /// native file admission so no replacement runs meanwhile. Copies of the
    /// deleted objects in the other store are marked with the short grace before
    /// the deletion commits, so a failed mark leaves the deletion to a later pass.
    pub(crate) fn sweep_message_page_objects(
        &mut self,
        target: MessageObjectStore,
        now_ms: i64,
        limit: usize,
    ) -> StoreResult<message_pages::ObjectSweep> {
        self.sweep_message_page_batch(target, now_ms, limit, true)
    }

    /// The next batch of a pass that `sweep_message_page_objects` began. The
    /// pass already visited the marks past their grace, so this batch visits
    /// only the objects after the cursor.
    pub(crate) fn continue_message_page_sweep(
        &mut self,
        target: MessageObjectStore,
        now_ms: i64,
        limit: usize,
    ) -> StoreResult<message_pages::ObjectSweep> {
        self.sweep_message_page_batch(target, now_ms, limit, false)
    }

    fn sweep_message_page_batch(
        &mut self,
        target: MessageObjectStore,
        now_ms: i64,
        limit: usize,
        expired_marks: bool,
    ) -> StoreResult<message_pages::ObjectSweep> {
        let Ok(device) = self.device_store.as_mut() else {
            return Ok(message_pages::ObjectSweep { wrapped: true, ..Default::default() });
        };
        let mut lease_ids: Vec<String> = self.revision_leases.keys().cloned().collect();
        lease_ids.sort();
        let leases = self.revision_leases.values().map(|lease| &lease.connection);
        let received = received_page_bodies(&self.repository_root)?;
        let mut deleted = Vec::new();
        let early = now_ms.saturating_sub(ASSET_GC_PRODUCT_MINIMUM_GRACE_MS - SUPERSEDED_OBJECT_GRACE_MS);
        if target == MessageObjectStore::Device {
            let transaction = device.transaction()?;
            let token = MessageObjectRootsToken::read(&self.connection, &transaction, lease_ids)?;
            let before = token.changes;
            let roots = self.message_object_roots.roots(target, token, &self.repository_root, |scratch| {
                message_pages::object_roots(&self.connection, leases, &transaction, &transaction, scratch)
            })?;
            let received = received_roots(&self.repository_root, &received, &transaction)?;
            let sweep = message_pages::sweep_objects(
                &transaction,
                &[roots].into_iter().chain(received.as_ref().map(|(roots, _)| roots)).collect::<Vec<_>>(),
                now_ms,
                ASSET_GC_PRODUCT_MINIMUM_GRACE_MS,
                limit,
                expired_marks,
                &mut deleted,
            )?;
            mark_other_copies(&self.connection, &deleted, early)?;
            transaction.commit()?;
            self.message_object_roots.committed(before, &self.connection, device.connection());
            return Ok(sweep);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let token = MessageObjectRootsToken::read(&transaction, device.connection(), lease_ids)?;
        let before = token.changes;
        let roots = self.message_object_roots.roots(target, token, &self.repository_root, |scratch| {
            message_pages::object_roots(&transaction, leases, device.connection(), &transaction, scratch)
        })?;
        let received = received_roots(&self.repository_root, &received, &transaction)?;
        let sweep = message_pages::sweep_objects(
            &transaction,
            &[roots].into_iter().chain(received.as_ref().map(|(roots, _)| roots)).collect::<Vec<_>>(),
            now_ms,
            ASSET_GC_PRODUCT_MINIMUM_GRACE_MS,
            limit,
            expired_marks,
            &mut deleted,
        )?;
        mark_other_copies(device.connection(), &deleted, early)?;
        transaction.commit()?;
        self.message_object_roots.committed(before, &self.connection, device.connection());
        Ok(sweep)
    }

    /// How many times a message object sweep read its roots.
    #[cfg(test)]
    pub(crate) fn message_object_roots_computed(&self) -> usize {
        self.message_object_roots.computed
    }

    pub(super) fn invalidate_message_object_roots(&mut self) {
        self.message_object_roots.library = None;
        self.message_object_roots.device = None;
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

    pub(crate) fn prepare_asset_gc_delete_marks(
        &self,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcMarks> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let residency = crate::server_sync::residency::Residency::open(&self.repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let (device_fence, mut plugins) = snapshot::collect_device_plugin_asset_roots(self)?;
        let _device_guard = self.device_store()?.acquire_plugin_gc_barrier(&device_fence)?;
        let transaction = rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let library_revision: i64 = self.connection.query_row("SELECT revision FROM plugin_gc_revision WHERE singleton=1", [], |row| row.get(0))?;
        snapshot::merge_asset_roots(&mut plugins, snapshot::collect_plugin_asset_roots(&self.connection)?);
        for reader in self.revision_leases.values() {
            snapshot::merge_asset_roots(&mut plugins, snapshot::collect_plugin_asset_roots(&reader.connection)?);
        }
        let roots = self.collect_asset_gc_roots_with_plugin_cache(false, false, Some(&plugins))?
            .into_iter().map(|(_, roots)| roots);
        let mut marks = crate::asset_repository::migration_gc::mark_asset_roots_with_remote(&cas, roots, |hash| residency.gc_size(hash))?;
        transaction.rollback()?;
        marks.plugin_cache = Some(crate::asset_repository::migration_gc::PluginGcCache {
            instance: self.gc_instance.clone(), library_revision, device_fence, roots: plugins,
        });
        Ok(marks)
    }

    pub(crate) fn asset_gc_delete_page_with_hook(
        &mut self,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
        hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        let marks = self.prepare_asset_gc_delete_marks()?;
        self.asset_gc_delete_marked_page_with_hook(&marks, limit, cursor, now_ms, minimum_grace_ms, hook)
    }

    pub(crate) fn asset_gc_delete_marked_page_with_hook(
        &mut self,
        initial_marks: &crate::asset_repository::migration_gc::AssetGcMarks,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
        hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_delete_marked_page_reusing_library(
            initial_marks, &mut None, limit, cursor, now_ms, minimum_grace_ms, hook,
        )
    }

    /// Deletes one page, reusing `library` roots an earlier page of the same run collected when
    /// the database is unchanged since. Every other root source is collected again.
    pub(crate) fn asset_gc_delete_marked_page_reusing_library(
        &mut self,
        initial_marks: &crate::asset_repository::migration_gc::AssetGcMarks,
        library: &mut Option<AssetGcLibraryRoots>,
        limit: i64,
        cursor: Option<&str>,
        now_ms: i64,
        minimum_grace_ms: i64,
        hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_delete_catalog_candidates(
            initial_marks, library, |store| store.query_asset_object_catalog(limit, cursor),
            now_ms, minimum_grace_ms, hook,
        )
    }

    /// Deletes exactly the listed objects that no root holds, with the same final recheck as a
    /// cleanup page. Objects missing from the catalog are skipped.
    pub(crate) fn asset_gc_delete_exact_objects(
        &mut self,
        initial_marks: &crate::asset_repository::migration_gc::AssetGcMarks,
        object_hashes: &std::collections::BTreeSet<String>,
        library: &mut Option<AssetGcLibraryRoots>,
        now_ms: i64,
        minimum_grace_ms: i64,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        self.asset_gc_delete_catalog_candidates(
            initial_marks, library,
            |store| asset_object_catalog::query_exact(&store.connection, object_hashes),
            now_ms, minimum_grace_ms, |_| Ok(()),
        )
    }

    /// Deletes the candidates `load` reads that no root holds. `load` runs again under the
    /// writer exclusion and must return the same candidates.
    fn asset_gc_delete_catalog_candidates(
        &mut self,
        initial_marks: &crate::asset_repository::migration_gc::AssetGcMarks,
        library: &mut Option<AssetGcLibraryRoots>,
        load: impl Fn(&Self) -> StoreResult<AssetObjectCatalogPage>,
        now_ms: i64,
        minimum_grace_ms: i64,
        mut hook: impl FnMut(
            crate::asset_repository::migration_gc::AssetGcDeleteHookPoint,
        ) -> StoreResult<()>,
    ) -> StoreResult<crate::asset_repository::migration_gc::AssetGcDryRunPage> {
        use crate::asset_repository::migration_gc::{
            mark_asset_roots_with_remote, sweep_asset_candidates_with_remote,
            AssetGcDeleteHookPoint, AssetGcDryRunPage,
        };

        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let plugins = initial_marks.plugin_cache.as_ref().ok_or_else(|| StoreError::Validation {
            message: "asset GC plugin cache is unavailable".into(),
        })?;
        self.validate_plugin_gc_cache(plugins)?;
        let initial_candidates = load(self)?;
        let residency = crate::server_sync::residency::Residency::open(&self.repository_root)
            .map_err(|error| std::io::Error::other(error.code))?;
        let initial_report = sweep_asset_candidates_with_remote(
            &cas,
            initial_candidates.items.clone(),
            initial_marks,
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
        let _device_guard = self.device_store()?.acquire_plugin_gc_barrier(&plugins.device_fence)?;
        let final_candidates = load(self)?;
        if final_candidates != initial_candidates {
            return Err(StoreError::Validation {
                message: "asset object catalog page changed before final GC recheck".to_owned(),
            });
        }
        self.validate_plugin_gc_cache(plugins)?;
        let final_transaction = rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let library_version: i64 = self.connection.query_row("PRAGMA data_version", [], |row|row.get(0))?;
        let mut expected_changes: i64 = self.connection.query_row("SELECT total_changes()", [], |row|row.get(0))?;
        let current_roots = self.collect_asset_gc_roots_reusing_library(
            true, false, Some(&plugins.roots), Some((library, library_version, expected_changes)),
        )?
            .into_iter().map(|(_, roots)| roots).collect::<Vec<_>>();
        // Marks follow from the roots and content-addressed manifests alone, so a page with the
        // same roots as the last one skips reading every manifest and statting every object.
        let reused = library.as_ref().and_then(|cached| cached.marks.as_ref())
            .filter(|(roots, _)| *roots == current_roots).map(|(_, marks)| marks.clone());
        let final_marks = match reused {
            Some(marks) => marks,
            None => {
                #[cfg(test)]
                ASSET_GC_FINAL_MARKINGS.with(|count| count.set(count.get() + 1));
                let marks = std::sync::Arc::new(mark_asset_roots_with_remote(
                    &cas, current_roots.iter().cloned(), |hash| residency.gc_size(hash),
                )?);
                if let Some(cached) = library.as_mut() { cached.marks = Some((current_roots, marks.clone())); }
                marks
            }
        };
        final_transaction.rollback()?;
        let mut report = sweep_asset_candidates_with_remote(&cas, final_candidates.items.clone(), &final_marks,
            now_ms, minimum_grace_ms, |hash| residency.gc_size(hash))?;
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
            let current_version: i64 = transaction.query_row("PRAGMA data_version", [], |row|row.get(0))?;
            let current_changes: i64 = transaction.query_row("SELECT total_changes()", [], |row|row.get(0))?;
            if current_version != library_version || current_changes != expected_changes {
                return Err(StoreError::CommitBusy);
            }
            transaction.execute(
                "INSERT INTO asset_object_deletions (
                    object_hash, byte_size, physical_key, state, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'pending', ?4)",
                params![object_hash, byte_size, physical_key, now_ms],
            )?;
            expected_changes = transaction.query_row("SELECT total_changes()", [], |row|row.get(0))?;
            transaction.commit()?;
            // Deletion records are not library roots, so the cleanup's own writes keep them valid.
            if let Some(library) = library.as_mut() { library.total_changes = expected_changes; }
            hook(AssetGcDeleteHookPoint::AfterTombstone)?;

            let transaction = rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            self.validate_plugin_gc_cache(plugins)?;
            let current_version: i64 = transaction.query_row("PRAGMA data_version", [], |row|row.get(0))?;
            let current_changes: i64 = transaction.query_row("SELECT total_changes()", [], |row|row.get(0))?;
            let exact: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM asset_objects WHERE object_hash=?1 AND byte_size=?2)", params![object_hash,byte_size], |row|row.get(0))?;
            if current_version != library_version || current_changes != expected_changes || !exact {
                return Err(StoreError::CommitBusy);
            }
            let checked = sweep_asset_candidates_with_remote(&cas, [candidate.clone()], &final_marks,
                now_ms, minimum_grace_ms, |hash| residency.gc_size(hash))?;
            if !checked.blockers.is_empty() || !checked.potential_delete_hashes.contains(&object_hash) {
                return Err(StoreError::Validation { message: "asset GC roots changed before unlink".into() });
            }
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
            expected_changes = transaction.query_row("SELECT total_changes()", [], |row|row.get(0))?;
            transaction.commit()?;
            if let Some(library) = library.as_mut() { library.total_changes = expected_changes; }
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

    fn validate_plugin_gc_cache(&self, cache: &crate::asset_repository::migration_gc::PluginGcCache) -> StoreResult<()> {
        let revision: i64 = self.connection.query_row("SELECT revision FROM plugin_gc_revision WHERE singleton=1", [], |row| row.get(0))?;
        if cache.instance != self.gc_instance || revision != cache.library_revision
            || !self.device_store()?.plugin_gc_fence_is_current(&cache.device_fence)? {
            return Err(StoreError::CommitBusy);
        }
        Ok(())
    }

    fn collect_asset_gc_roots(
        &self,
        repository_guard_held: bool,
        read_only: bool,
    ) -> StoreResult<Vec<crate::asset_repository::migration_gc::AssetRootSet>> {
        Ok(self
            .collect_labelled_asset_gc_roots(repository_guard_held, read_only)?
            .into_iter()
            .map(|(_, roots)| roots)
            .collect())
    }

    /// The same roots, each labelled by what holds it, so a preview can explain a retention.
    fn collect_labelled_asset_gc_roots(
        &self,
        repository_guard_held: bool,
        read_only: bool,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        self.collect_asset_gc_roots_with_backup_references(repository_guard_held, read_only, None)
    }

    fn collect_asset_gc_roots_with_backup_references(
        &self,
        repository_guard_held: bool,
        read_only: bool,
        _backup_references: Option<&mut std::collections::BTreeSet<String>>,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        self.collect_asset_gc_roots_with_plugin_cache(repository_guard_held, read_only, None)
    }

    /// The GC roots a residency status reads. The library's own roots are reused from an
    /// earlier status while no commit has reached the library since that status read them.
    fn collect_status_asset_gc_roots(
        &self,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        // Writes of an open transaction are seen only by its own connection.
        let cached = if self.connection.is_autocommit() {
            self.status_library_roots.take(&self.database_path)
        } else {
            None
        };
        let Some((data_version, mut library)) = cached else {
            return self.collect_asset_gc_roots_with_plugin_cache(false, false, None);
        };
        // The observer never writes, so its `data_version` alone moves with every commit.
        let roots = self.collect_asset_gc_roots_reusing_library(false, false, None, Some((&mut library, data_version, 0)));
        self.status_library_roots.put(library);
        roots
    }

    fn collect_asset_gc_roots_with_plugin_cache(
        &self,
        repository_guard_held: bool,
        read_only: bool,
        plugins: Option<&crate::asset_repository::migration_gc::AssetRootSet>,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        self.collect_asset_gc_roots_reusing_library(repository_guard_held, read_only, plugins, None)
    }

    /// `library` carries roots an earlier page collected and the database `data_version` and
    /// `total_changes()` observed now, which must be read in the transaction the roots come from.
    fn collect_asset_gc_roots_reusing_library(
        &self,
        repository_guard_held: bool,
        read_only: bool,
        plugins: Option<&crate::asset_repository::migration_gc::AssetRootSet>,
        library: Option<(&mut Option<AssetGcLibraryRoots>, i64, i64)>,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        #[cfg(test)]
        ASSET_GC_ROOT_COLLECTIONS.with(|count| {
            let (preliminary, final_checks) = count.get();
            count.set((preliminary + usize::from(!repository_guard_held),
                final_checks + usize::from(repository_guard_held)));
        });
        let collect = |connection: &Connection| match plugins {
            Some(plugins) => snapshot::collect_asset_roots_with_plugin_cache(connection, plugins),
            None => snapshot::collect_asset_roots(connection),
        };
        let collect_library = || {
            #[cfg(test)]
            ASSET_GC_LIBRARY_ROOT_COLLECTIONS.with(|count| count.set(count.get() + 1));
            collect(&self.connection)
        };
        let library_roots = match library {
            Some((Some(cached), data_version, total_changes))
                if cached.data_version == data_version && cached.total_changes == total_changes =>
            {
                cached.roots.clone()
            }
            Some((cache, data_version, total_changes)) => {
                let roots = collect_library()?;
                *cache = Some(AssetGcLibraryRoots { data_version, total_changes, roots: roots.clone(), marks: None });
                roots
            }
            None => collect_library()?,
        };
        let mut roots = vec![("library", library_roots)];
        roots.extend(self.collect_auxiliary_asset_gc_roots(repository_guard_held, read_only, plugins)?);
        Ok(roots)
    }

    fn collect_auxiliary_asset_gc_roots(
        &self, repository_guard_held: bool, read_only: bool,
        plugins: Option<&crate::asset_repository::migration_gc::AssetRootSet>,
    ) -> StoreResult<Vec<(&'static str, crate::asset_repository::migration_gc::AssetRootSet)>> {
        use crate::asset_repository::job_pins::{
            collect_durable_cas_job_roots, collect_durable_cas_job_roots_already_guarded,
            collect_durable_cas_job_roots_read_only,
        };

        let mut roots = vec![("source-preservation", snapshot::collect_preserved_source_roots(self.repository_root()))];
        for reader in self.revision_leases.values() {
            roots.push((
                "library",
                match plugins {
                    Some(plugins) => snapshot::collect_asset_roots_with_plugin_cache(&reader.connection, plugins)?,
                    None => snapshot::collect_asset_roots(&reader.connection)?,
                },
            ));
        }
        roots.extend(
            self.active_readers
                .detached_asset_roots()?
                .into_iter()
                .map(|set| ("library", set)),
        );
        roots.push((
            "remote",
            crate::external_storage::capture::registered_roots(
                &self.connection,
                &self.repository_root,
            )?,
        ));
        // A snapshot delete or restore can hold the archive for minutes, so GC stops here as it
        // does during a capture instead of keeping the store waiting.
        roots.extend(
            snapshot_archive::Archive::try_open(&self.snapshots_dir)?
                .ok_or_else(|| StoreError::Validation { message: ASSET_INVENTORY_BUSY.into() })?
                .roots()?
                .into_iter()
                .map(|set| ("snapshot", set)),
        );
        // A repair journal holds what a repair stopped referencing, so an undo still has it.
        roots.push((
            "repair",
            crate::data_health::journal::roots(&self.repository_root)?,
        ));
        let jobs = if read_only {
            collect_durable_cas_job_roots_read_only(&self.repository_root)
        } else if repository_guard_held {
            collect_durable_cas_job_roots_already_guarded(&self.repository_root)
        } else {
            collect_durable_cas_job_roots(&self.repository_root)
        };
        roots.push(("job", jobs));
        roots.push(("job", self.external_lww_active_asset_roots()?));
        Ok(roots)
    }

    pub(crate) fn get_app_kv(&self, key: &str) -> StoreResult<Option<Value>> {
        validate_app_kv_key(key)?;
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
        validate_app_kv_key(key)?;
        self.connection.execute(
            "INSERT INTO app_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub(crate) fn remove_app_kv(&self, key: &str) -> StoreResult<()> {
        validate_app_kv_key(key)?;
        self.connection
            .execute("DELETE FROM app_kv WHERE key = ?1", [key])?;
        Ok(())
    }

    /// Opens the leased generation for diagnosis. The reader owns its connection, so the scan it
    /// serves runs without the store mutex and never blocks a writer. The lease pins the
    /// generation against collection, and the caller compares its revision again before applying
    /// any repair.
    pub(crate) fn data_health_reader(&self, lease: &str) -> StoreResult<DataHealthReader> {
        let (_, target) = self.read_view(Some(lease))?;
        Ok(DataHealthReader {
            connection: snapshot::open_generation_reader(&self.database_path, &target.generation)?,
            cas: crate::asset_repository::PayloadCas::new(self.repository_root())?,
            revision: target.revision,
            quarantined: match self.device_store {
                Ok(_) => self.lww_quarantined_intents()?,
                Err(_) => Vec::new(),
            },
        })
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
    let connection = crate::sqlite_open::open(database_path)?;
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
mod benchmark_lww;
#[cfg(test)]
pub(crate) mod tests;
