//! Canonical application record codecs, independent of any sync transport.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const MAX_LOGICAL_RECORD_KEY_BYTES: usize = 64 * 1024;
pub const LOGICAL_MESSAGE_PAGE_SIZE: usize = 128;
pub const LOGICAL_RECORD_SCHEMA: &str = "risunest.logical-record/v1";
pub const LOGICAL_MESSAGE_PAGE_SCHEMA: &str = "risunest.logical-message-page/v1";

const JAVASCRIPT_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalRecordLocator {
    Root,
    Preset {
        preset_id: String,
    },
    Plugin {
        owner: String,
        storage_key: String,
    },
    Character {
        character_id: String,
    },
    Conversation {
        character_id: String,
        conversation_id: String,
    },
    Asset {
        logical_key: String,
    },
    Inlay {
        logical_key: String,
    },
    Cold {
        logical_key: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind")]
pub enum LogicalOwnerLocator {
    #[serde(rename = "character-additional-assets")]
    CharacterAdditional {
        #[serde(rename = "characterId")]
        character_id: String,
    },
    #[serde(rename = "root-module-assets")]
    RootModule { index: u64 },
    #[serde(rename = "persona-embedded-module-assets")]
    PersonaEmbeddedModule { index: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogicalOwnerHead {
    pub owner: LogicalOwnerLocator,
    pub present: bool,
    pub manifest_hash: Option<String>,
    pub entry_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property_index: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalAssetAliasMetadata {
    pub mime: String,
    pub name: String,
    pub ext: String,
    pub inlay_type: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub metadata: Value,
}

impl LogicalAssetAliasMetadata {
    fn validate(&self) -> Result<(), LogicalRecordError> {
        if !self.metadata.is_object() {
            return Err(invalid(
                "logical asset alias extension metadata must be an object",
            ));
        }
        if self.width.is_some_and(|value| value < 0) || self.height.is_some_and(|value| value < 0) {
            return Err(invalid(
                "logical asset alias dimensions must be nonnegative",
            ));
        }
        if self
            .inlay_type
            .as_deref()
            .is_some_and(|value| !matches!(value, "image" | "video" | "audio" | "signature"))
        {
            return Err(invalid("logical asset alias inlayType is invalid"));
        }
        Ok(())
    }
}

pub fn encode_asset_alias_metadata(
    metadata: &LogicalAssetAliasMetadata,
) -> Result<Value, LogicalRecordError> {
    metadata.validate()?;
    serde_json::to_value(metadata).map_err(|error| {
        invalid(format!(
            "logical asset alias metadata encoding failed: {error}"
        ))
    })
}

pub fn decode_asset_alias_metadata(
    value: &Value,
) -> Result<LogicalAssetAliasMetadata, LogicalRecordError> {
    let metadata: LogicalAssetAliasMetadata = serde_json::from_value(value.clone())
        .map_err(|_| invalid("logical asset alias metadata is invalid"))?;
    metadata.validate()?;
    if encode_asset_alias_metadata(&metadata)? != *value {
        return Err(invalid("logical asset alias metadata is not canonical"));
    }
    Ok(metadata)
}

impl LogicalOwnerHead {
    pub fn absent(owner: LogicalOwnerLocator) -> Self {
        Self {
            owner,
            present: false,
            manifest_hash: None,
            entry_count: 0,
            property_index: None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn present(
        owner: LogicalOwnerLocator,
        manifest_hash: String,
        entry_count: u64,
        property_index: u64,
    ) -> Result<Self, LogicalRecordError> {
        let head = Self {
            owner,
            present: true,
            manifest_hash: Some(manifest_hash),
            entry_count,
            property_index: Some(property_index),
        };
        head.validate()?;
        Ok(head)
    }

    pub fn unpositioned_present(
        owner: LogicalOwnerLocator,
        manifest_hash: String,
        entry_count: u64,
    ) -> Result<Self, LogicalRecordError> {
        let head = Self {
            owner,
            present: true,
            manifest_hash: Some(manifest_hash),
            entry_count,
            property_index: None,
        };
        head.validate_identity()?;
        Ok(head)
    }

    fn validate(&self) -> Result<(), LogicalRecordError> {
        self.validate_identity()?;
        if self.present {
            validate_safe_integer(
                self.property_index
                    .ok_or_else(|| invalid("present owner head requires propertyIndex"))?,
                "owner property index",
            )?;
        } else if self.property_index.is_some() {
            return Err(invalid("absent owner head cannot contain propertyIndex"));
        }
        Ok(())
    }

    fn validate_identity(&self) -> Result<(), LogicalRecordError> {
        match &self.owner {
            LogicalOwnerLocator::CharacterAdditional { character_id } => {
                validate_component(character_id, "owner characterId", false)?;
            }
            LogicalOwnerLocator::RootModule { index }
            | LogicalOwnerLocator::PersonaEmbeddedModule { index } => {
                validate_safe_integer(*index, "owner index")?;
            }
        }
        validate_safe_integer(self.entry_count, "owner entry count")?;
        if self.present {
            validate_hash(
                self.manifest_hash.as_deref().unwrap_or_default(),
                "owner manifest hash",
            )?;
        } else if self.manifest_hash.is_some() || self.entry_count != 0 {
            return Err(invalid("absent owner head cannot reference a manifest"));
        }
        Ok(())
    }

    fn storage_key(&self) -> String {
        match &self.owner {
            LogicalOwnerLocator::CharacterAdditional { character_id } => {
                format!("character-additional-assets:{character_id}")
            }
            LogicalOwnerLocator::RootModule { index } => {
                format!("root-module-assets:{index:020}")
            }
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => {
                format!("persona-embedded-module-assets:{index:020}")
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LogicalRecordEnvelope {
    Root {
        value: Value,
        #[serde(rename = "ownerHeads")]
        owner_heads: Vec<LogicalOwnerHead>,
    },
    Preset {
        #[serde(rename = "configuredIndex")]
        configured_index: u64,
        value: Value,
    },
    Plugin {
        owner: String,
        ordinal: u64,
        value: Value,
    },
    Character {
        #[serde(rename = "configuredIndex")]
        configured_index: u64,
        detail: Value,
        #[serde(rename = "ownerHeads")]
        owner_heads: Vec<LogicalOwnerHead>,
    },
    #[serde(rename = "archived-character")]
    ArchivedCharacter {
        #[serde(rename = "configuredIndex")]
        configured_index: u64,
        #[serde(rename = "recentAt")]
        recent_at: i64,
        trashed: bool,
        name: String,
        image: Option<String>,
        #[serde(rename = "type")]
        character_type: String,
        #[serde(rename = "creatorNotes")]
        creator_notes: Option<String>,
        #[serde(rename = "trashTime")]
        trash_time: Option<i64>,
        #[serde(rename = "archiveObjectHash")]
        archive_object_hash: String,
        #[serde(rename = "archiveObjectSize")]
        archive_object_size: u64,
        #[serde(rename = "archivedAt")]
        archived_at: u64,
        #[serde(rename = "conversationCount")]
        conversation_count: u64,
        #[serde(rename = "messageCount")]
        message_count: u64,
        #[serde(rename = "assetHashes")]
        asset_hashes: Vec<String>,
        #[serde(rename = "ownerHeads")]
        owner_heads: Vec<LogicalOwnerHead>,
    },
    Conversation {
        #[serde(rename = "configuredIndex")]
        configured_index: u64,
        #[serde(rename = "recentAt")]
        recent_at: i64,
        detail: Value,
        #[serde(rename = "messagePageHashes")]
        message_page_hashes: Vec<String>,
    },
    Asset {
        #[serde(rename = "objectHash")]
        object_hash: Option<String>,
        size: u64,
        metadata: Value,
    },
    Inlay {
        #[serde(rename = "objectHash")]
        object_hash: Option<String>,
        size: u64,
        metadata: Value,
    },
    Cold {
        #[serde(rename = "objectHash")]
        object_hash: Option<String>,
        size: u64,
        metadata: Value,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedLogicalObject {
    pub hash: String,
    pub size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Deserialize, Serialize)]
struct LogicalRecordDocument {
    schema: String,
    #[serde(flatten)]
    record: LogicalRecordEnvelope,
}

#[derive(Deserialize, Serialize)]
struct LogicalMessagePageDocument {
    schema: String,
    messages: Vec<Value>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalRecordError(String);

impl std::fmt::Display for LogicalRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LogicalRecordError {}

pub fn invalid(message: impl Into<String>) -> LogicalRecordError {
    LogicalRecordError(message.into())
}

pub fn validate_safe_integer(value: u64, description: &str) -> Result<(), LogicalRecordError> {
    if value > JAVASCRIPT_MAX_SAFE_INTEGER {
        return Err(invalid(format!(
            "{description} exceeds the JavaScript safe integer limit"
        )));
    }
    Ok(())
}

pub fn validate_hash(value: &str, description: &str) -> Result<(), LogicalRecordError> {
    if !(value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    {
        return Err(invalid(format!(
            "{description} must be a lowercase SHA-256"
        )));
    }
    Ok(())
}

pub fn validate_object_descriptor(hash: &str, size: u64) -> Result<(), LogicalRecordError> {
    validate_hash(hash, "object hash")?;
    validate_safe_integer(size, "object size")?;
    if (size == 0) != (hash == EMPTY_SHA256) {
        return Err(invalid("empty object must use the SHA-256 of empty bytes"));
    }
    Ok(())
}

fn validate_owner_heads(heads: &[LogicalOwnerHead]) -> Result<(), LogicalRecordError> {
    let mut previous: Option<String> = None;
    for head in heads {
        head.validate()?;
        let key = head.storage_key();
        if previous
            .as_deref()
            .is_some_and(|value| value >= key.as_str())
        {
            return Err(invalid("logical owner heads must be sorted and unique"));
        }
        previous = Some(key);
    }
    Ok(())
}

impl LogicalRecordEnvelope {
    fn validate(&self) -> Result<(), LogicalRecordError> {
        match self {
            Self::Root { owner_heads, .. } => validate_owner_heads(owner_heads),
            Self::Preset {
                configured_index, ..
            }
            | Self::Character {
                configured_index, ..
            }
            | Self::ArchivedCharacter {
                configured_index, ..
            }
            | Self::Conversation {
                configured_index, ..
            } => {
                validate_safe_integer(*configured_index, "configured index")?;
                if let Self::Character { owner_heads, .. } = self {
                    validate_owner_heads(owner_heads)?;
                }
                if let Self::ArchivedCharacter {
                    archive_object_hash,
                    archive_object_size,
                    archived_at,
                    conversation_count,
                    message_count,
                    asset_hashes,
                    owner_heads,
                    ..
                } = self
                {
                    validate_object_descriptor(archive_object_hash, *archive_object_size)?;
                    validate_safe_integer(*archived_at, "archive timestamp")?;
                    validate_safe_integer(*conversation_count, "archived conversation count")?;
                    validate_safe_integer(*message_count, "archived message count")?;
                    let mut previous: Option<&str> = None;
                    for hash in asset_hashes {
                        validate_hash(hash, "archived asset hash")?;
                        if previous.is_some_and(|value| value >= hash.as_str()) {
                            return Err(invalid("archived asset hashes must be sorted and unique"));
                        }
                        previous = Some(hash);
                    }
                    for head in owner_heads {
                        head.validate_identity()?;
                    }
                    let mut keys = owner_heads.iter().map(LogicalOwnerHead::storage_key);
                    let mut previous = keys.next();
                    for key in keys {
                        if previous.as_deref().is_some_and(|value| value >= key.as_str()) {
                            return Err(invalid("logical owner heads must be sorted and unique"));
                        }
                        previous = Some(key);
                    }
                }
                if let Self::Conversation {
                    message_page_hashes,
                    ..
                } = self
                {
                    for hash in message_page_hashes {
                        validate_hash(hash, "message page hash")?;
                    }
                }
                Ok(())
            }
            Self::Plugin { ordinal, .. } => validate_safe_integer(*ordinal, "plugin ordinal"),
            Self::Asset {
                object_hash, size, ..
            }
            | Self::Inlay {
                object_hash, size, ..
            }
            | Self::Cold {
                object_hash, size, ..
            } => {
                validate_safe_integer(*size, "object size")?;
                if let Some(object_hash) = object_hash {
                    validate_object_descriptor(object_hash, *size)?;
                }
                let metadata = match self {
                    Self::Asset { metadata, .. }
                    | Self::Inlay { metadata, .. }
                    | Self::Cold { metadata, .. } => metadata,
                    _ => unreachable!(),
                };
                if !metadata.is_object() {
                    return Err(invalid("logical payload metadata must be an object"));
                }
                Ok(())
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dependency_hashes(&self) -> Vec<String> {
        let mut hashes = match self {
            Self::Root { owner_heads, .. } | Self::Character { owner_heads, .. } => owner_heads
                .iter()
                .filter_map(|head| head.manifest_hash.clone())
                .collect(),
            Self::ArchivedCharacter {
                archive_object_hash,
                asset_hashes,
                owner_heads,
                ..
            } => std::iter::once(archive_object_hash.clone())
                .chain(asset_hashes.iter().cloned())
                .chain(owner_heads.iter().filter_map(|head| head.manifest_hash.clone()))
                .collect(),
            Self::Conversation {
                message_page_hashes,
                ..
            } => message_page_hashes.clone(),
            Self::Asset { object_hash, .. }
            | Self::Inlay { object_hash, .. }
            | Self::Cold { object_hash, .. } => object_hash.iter().cloned().collect(),
            Self::Preset { .. } | Self::Plugin { .. } => Vec::new(),
        };
        hashes.sort();
        hashes.dedup();
        hashes
    }
}

pub fn encoded_object(bytes: Vec<u8>) -> Result<EncodedLogicalObject, LogicalRecordError> {
    let size = u64::try_from(bytes.len()).map_err(|_| invalid("logical object is too large"))?;
    validate_safe_integer(size, "logical object size")?;
    Ok(EncodedLogicalObject {
        hash: hex::encode(Sha256::digest(&bytes)),
        size,
        bytes,
    })
}

pub fn encode_logical_record(
    record: &LogicalRecordEnvelope,
) -> Result<EncodedLogicalObject, LogicalRecordError> {
    record.validate()?;
    let bytes = serde_json::to_vec(&LogicalRecordDocument {
        schema: LOGICAL_RECORD_SCHEMA.to_owned(),
        record: record.clone(),
    })
    .map_err(|error| invalid(format!("logical record encoding failed: {error}")))?;
    encoded_object(bytes)
}

pub fn decode_logical_record(bytes: &[u8]) -> Result<LogicalRecordEnvelope, LogicalRecordError> {
    let document: LogicalRecordDocument = serde_json::from_slice(bytes)
        .map_err(|_| invalid("logical record bytes are not valid UTF-8 JSON"))?;
    if document.schema != LOGICAL_RECORD_SCHEMA {
        return Err(invalid("logical record schema is unsupported"));
    }
    document.record.validate()?;
    if encode_logical_record(&document.record)?.bytes != bytes {
        return Err(invalid("logical record bytes are not canonical"));
    }
    Ok(document.record)
}

pub fn encode_message_page(messages: &[Value]) -> Result<EncodedLogicalObject, LogicalRecordError> {
    if messages.len() > LOGICAL_MESSAGE_PAGE_SIZE {
        return Err(invalid("logical message page exceeds 128 messages"));
    }
    let bytes = serde_json::to_vec(&LogicalMessagePageDocument {
        schema: LOGICAL_MESSAGE_PAGE_SCHEMA.to_owned(),
        messages: messages.to_vec(),
    })
    .map_err(|error| invalid(format!("logical message page encoding failed: {error}")))?;
    encoded_object(bytes)
}

pub fn decode_message_page(bytes: &[u8]) -> Result<Vec<Value>, LogicalRecordError> {
    let document: LogicalMessagePageDocument = serde_json::from_slice(bytes)
        .map_err(|_| invalid("logical message page bytes are not valid UTF-8 JSON"))?;
    if document.schema != LOGICAL_MESSAGE_PAGE_SCHEMA {
        return Err(invalid("logical message page schema is unsupported"));
    }
    if document.messages.len() > LOGICAL_MESSAGE_PAGE_SIZE {
        return Err(invalid("logical message page exceeds 128 messages"));
    }
    if encode_message_page(&document.messages)?.bytes != bytes {
        return Err(invalid("logical message page bytes are not canonical"));
    }
    Ok(document.messages)
}

fn validate_component(
    value: &str,
    description: &str,
    allow_empty: bool,
) -> Result<(), LogicalRecordError> {
    if (!allow_empty && value.is_empty()) || value.len() > MAX_LOGICAL_RECORD_KEY_BYTES {
        return Err(invalid(format!("invalid logical record {description}")));
    }
    Ok(())
}

fn validate_cold_logical_key(value: &str) -> Result<(), LogicalRecordError> {
    validate_component(value, "logicalKey", false)?;
    if value.contains('\0') {
        return Err(invalid("logical cold record key cannot contain NUL"));
    }
    Ok(())
}

fn locator_parts(
    locator: &LogicalRecordLocator,
) -> Result<(&'static str, Vec<&str>), LogicalRecordError> {
    let parts = match locator {
        LogicalRecordLocator::Root => ("root", vec![]),
        LogicalRecordLocator::Preset { preset_id } => {
            validate_component(preset_id, "presetId", false)?;
            ("preset", vec![preset_id.as_str()])
        }
        LogicalRecordLocator::Plugin { owner, storage_key } => {
            validate_component(owner, "owner", false)?;
            validate_component(storage_key, "storageKey", true)?;
            ("plugin", vec![owner.as_str(), storage_key.as_str()])
        }
        LogicalRecordLocator::Character { character_id } => {
            validate_component(character_id, "characterId", false)?;
            ("character", vec![character_id.as_str()])
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => {
            validate_component(character_id, "characterId", false)?;
            validate_component(conversation_id, "conversationId", false)?;
            (
                "conversation",
                vec![character_id.as_str(), conversation_id.as_str()],
            )
        }
        LogicalRecordLocator::Asset { logical_key } => {
            validate_component(logical_key, "logicalKey", true)?;
            ("asset", vec![logical_key.as_str()])
        }
        LogicalRecordLocator::Inlay { logical_key } => {
            validate_component(logical_key, "logicalKey", true)?;
            ("inlay", vec![logical_key.as_str()])
        }
        LogicalRecordLocator::Cold { logical_key } => {
            validate_cold_logical_key(logical_key)?;
            ("cold", vec![logical_key.as_str()])
        }
    };
    Ok(parts)
}

pub fn encode_logical_record_key(
    locator: &LogicalRecordLocator,
) -> Result<String, LogicalRecordError> {
    let (kind, components) = locator_parts(locator)?;
    let encoded = if components.is_empty() {
        format!("r1:{kind}")
    } else {
        let json = serde_json::to_vec(&components)
            .map_err(|error| invalid(format!("logical record key encoding failed: {error}")))?;
        format!("r1:{kind}:{}", URL_SAFE_NO_PAD.encode(json))
    };
    if encoded.len() > MAX_LOGICAL_RECORD_KEY_BYTES {
        return Err(invalid("logical record key exceeds the encoded key limit"));
    }
    Ok(encoded)
}

pub fn decode_logical_record_key(
    encoded: &str,
) -> Result<LogicalRecordLocator, LogicalRecordError> {
    if encoded.len() > MAX_LOGICAL_RECORD_KEY_BYTES {
        return Err(invalid("logical record key exceeds the encoded key limit"));
    }
    if encoded == "r1:root" {
        return Ok(LogicalRecordLocator::Root);
    }
    let mut fields = encoded.split(':');
    if fields.next() != Some("r1") {
        return Err(invalid("logical record key prefix is invalid"));
    }
    let kind = fields
        .next()
        .ok_or_else(|| invalid("logical record key kind is missing"))?;
    let payload = fields
        .next()
        .ok_or_else(|| invalid("logical record key components are missing"))?;
    if fields.next().is_some() || payload.is_empty() {
        return Err(invalid("logical record key format is invalid"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| invalid("logical record key components are not canonical base64url"))?;
    let components: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("logical record key components are invalid"))?;
    let locator = match (kind, components.as_slice()) {
        ("preset", [preset_id]) => LogicalRecordLocator::Preset {
            preset_id: preset_id.clone(),
        },
        ("plugin", [owner, storage_key]) => LogicalRecordLocator::Plugin {
            owner: owner.clone(),
            storage_key: storage_key.clone(),
        },
        ("character", [character_id]) => LogicalRecordLocator::Character {
            character_id: character_id.clone(),
        },
        ("conversation", [character_id, conversation_id]) => LogicalRecordLocator::Conversation {
            character_id: character_id.clone(),
            conversation_id: conversation_id.clone(),
        },
        ("asset", [logical_key]) => LogicalRecordLocator::Asset {
            logical_key: logical_key.clone(),
        },
        ("inlay", [logical_key]) => LogicalRecordLocator::Inlay {
            logical_key: logical_key.clone(),
        },
        ("cold", [logical_key]) => LogicalRecordLocator::Cold {
            logical_key: logical_key.clone(),
        },
        _ => {
            return Err(invalid(
                "logical record key kind or component arity is invalid",
            ))
        }
    };
    let canonical = encode_logical_record_key(&locator)?;
    if canonical != encoded {
        return Err(invalid("logical record key is not canonical"));
    }
    Ok(locator)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalManifestObject {
    pub hash: String,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    fn logical_record_key_golden() -> Value {
        serde_json::from_str(include_str!(
            "../../../src/ts/storage/tests/fixtures/logicalRecordKeyV1Golden.json"
        ))
        .expect("valid logical record key golden fixture")
    }

    fn golden_locator(value: &Value) -> LogicalRecordLocator {
        let field = |name: &str| value[name].as_str().expect("string field").to_owned();
        match value["kind"].as_str().expect("locator kind") {
            "root" => LogicalRecordLocator::Root,
            "preset" => LogicalRecordLocator::Preset {
                preset_id: field("presetId"),
            },
            "plugin" => LogicalRecordLocator::Plugin {
                owner: field("owner"),
                storage_key: field("storageKey"),
            },
            "character" => LogicalRecordLocator::Character {
                character_id: field("characterId"),
            },
            "conversation" => LogicalRecordLocator::Conversation {
                character_id: field("characterId"),
                conversation_id: field("conversationId"),
            },
            "asset" => LogicalRecordLocator::Asset {
                logical_key: field("logicalKey"),
            },
            "inlay" => LogicalRecordLocator::Inlay {
                logical_key: field("logicalKey"),
            },
            "cold" => LogicalRecordLocator::Cold {
                logical_key: field("logicalKey"),
            },
            other => panic!("unknown golden locator kind {other}"),
        }
    }

    #[test]
    fn logical_record_keys_match_typescript_v1_literals_and_round_trip_opaque_components() {
        let golden = logical_record_key_golden();
        let fixtures = golden["roundTrip"].as_array().expect("roundTrip vectors");
        assert!(!fixtures.is_empty());
        for fixture in fixtures {
            let locator = golden_locator(&fixture["locator"]);
            let expected = fixture["encoded"].as_str().expect("encoded key");
            assert_eq!(encode_logical_record_key(&locator).unwrap(), expected);
            assert_eq!(decode_logical_record_key(expected).unwrap(), locator);
        }
    }

    #[test]
    fn logical_record_keys_reject_noncanonical_or_invalid_components() {
        let golden = logical_record_key_golden();
        for encoded in golden["rejectedEncoded"].as_array().expect("rejected keys") {
            let encoded = encoded.as_str().expect("encoded key");
            assert!(
                decode_logical_record_key(encoded).is_err(),
                "expected rejection of {encoded}"
            );
        }
        for locator in golden["rejectedLocators"]
            .as_array()
            .expect("rejected locators")
        {
            let locator = golden_locator(locator);
            assert!(
                encode_logical_record_key(&locator).is_err(),
                "expected encode rejection of {locator:?}"
            );
        }
    }

    #[test]
    fn typed_logical_record_envelopes_encode_canonical_json_and_round_trip() {
        let payload_hash = "1".repeat(64);
        let manifest_hash = "2".repeat(64);
        let page_hash = "3".repeat(64);
        let records = vec![
            LogicalRecordEnvelope::Root {
                value: json!({ "username": "Fixture" }),
                owner_heads: vec![LogicalOwnerHead::present(
                    LogicalOwnerLocator::RootModule { index: 0 },
                    manifest_hash.clone(),
                    2,
                    1,
                )
                .unwrap()],
            },
            LogicalRecordEnvelope::Preset {
                configured_index: 4,
                value: json!({ "name": "Preset" }),
            },
            LogicalRecordEnvelope::Plugin {
                owner: "provider-manager".to_owned(),
                ordinal: 7,
                value: json!({ "enabled": false }),
            },
            LogicalRecordEnvelope::Character {
                configured_index: 2,
                detail: json!({ "chaId": "character-1", "name": "Character" }),
                owner_heads: vec![LogicalOwnerHead::absent(
                    LogicalOwnerLocator::CharacterAdditional {
                        character_id: "character-1".to_owned(),
                    },
                )],
            },
            LogicalRecordEnvelope::ArchivedCharacter {
                configured_index: 3,
                recent_at: 41,
                trashed: false,
                name: "Archived".to_owned(),
                image: Some("assets/archived.png".to_owned()),
                character_type: "character".to_owned(),
                creator_notes: Some("notes".to_owned()),
                trash_time: None,
                archive_object_hash: "4".repeat(64),
                archive_object_size: 128,
                archived_at: 43,
                conversation_count: 2,
                message_count: 7,
                asset_hashes: vec!["5".repeat(64)],
                owner_heads: vec![LogicalOwnerHead::unpositioned_present(
                    LogicalOwnerLocator::CharacterAdditional {
                        character_id: "archived-1".to_owned(),
                    },
                    manifest_hash.clone(),
                    2,
                )
                .unwrap()],
            },
            LogicalRecordEnvelope::Conversation {
                configured_index: 5,
                recent_at: 42,
                detail: json!({ "id": "chat-1", "name": "Chat" }),
                message_page_hashes: vec![page_hash.clone()],
            },
            LogicalRecordEnvelope::Asset {
                object_hash: None,
                size: 6,
                metadata: json!({
                    "mime": "application/octet-stream",
                    "name": "asset.bin",
                    "ext": "bin"
                }),
            },
            LogicalRecordEnvelope::Inlay {
                object_hash: Some(payload_hash.clone()),
                size: 6,
                metadata: json!({
                    "mime": "image/webp",
                    "name": "inlay.webp",
                    "ext": "webp",
                    "inlayType": "image",
                    "width": 320,
                    "height": 200
                }),
            },
            LogicalRecordEnvelope::Cold {
                object_hash: Some(payload_hash),
                size: 6,
                metadata: json!({ "name": "chat.json" }),
            },
        ];

        let expected_kinds = [
            "root",
            "preset",
            "plugin",
            "character",
            "archived-character",
            "conversation",
            "asset",
            "inlay",
            "cold",
        ];
        for (record, expected_kind) in records.into_iter().zip(expected_kinds) {
            let encoded = encode_logical_record(&record).unwrap();
            let json: Value = serde_json::from_slice(&encoded.bytes).unwrap();
            assert_eq!(json["schema"], "risunest.logical-record/v1");
            assert_eq!(json["kind"], expected_kind);
            if expected_kind == "root" {
                assert!(json["ownerHeads"][0].get("manifestSize").is_none());
            }
            assert_eq!(encoded.size, encoded.bytes.len() as u64);
            assert_eq!(decode_logical_record(&encoded.bytes).unwrap(), record);
        }
    }

    #[test]
    fn present_owner_heads_require_a_canonical_property_position() {
        let manifest_hash = "2".repeat(64);
        let canonical = format!(
            "{{\"schema\":\"risunest.logical-record/v1\",\"kind\":\"root\",\"value\":{{}},\"ownerHeads\":[{{\"owner\":{{\"kind\":\"root-module-assets\",\"index\":0}},\"present\":true,\"manifestHash\":\"{manifest_hash}\",\"entryCount\":0,\"propertyIndex\":0}}]}}"
        );
        let decoded = decode_logical_record(canonical.as_bytes())
            .expect("decode present owner head with a property position");
        assert_eq!(
            encode_logical_record(&decoded).unwrap().bytes,
            canonical.as_bytes()
        );

        for invalid in [
            format!(
                "{{\"schema\":\"risunest.logical-record/v1\",\"kind\":\"root\",\"value\":{{}},\"ownerHeads\":[{{\"owner\":{{\"kind\":\"root-module-assets\",\"index\":0}},\"present\":true,\"manifestHash\":\"{manifest_hash}\",\"entryCount\":0}}]}}"
            ),
            format!(
                "{{\"schema\":\"risunest.logical-record/v1\",\"kind\":\"root\",\"value\":{{}},\"ownerHeads\":[{{\"owner\":{{\"kind\":\"root-module-assets\",\"index\":0}},\"present\":true,\"manifestHash\":\"{manifest_hash}\",\"entryCount\":0,\"propertyIndex\":null}}]}}"
            ),
            format!(
                "{{\"schema\":\"risunest.logical-record/v1\",\"kind\":\"root\",\"value\":{{}},\"ownerHeads\":[{{\"owner\":{{\"kind\":\"root-module-assets\",\"index\":0}},\"present\":true,\"manifestHash\":\"{manifest_hash}\",\"entryCount\":0,\"propertyIndex\":9007199254740992}}]}}"
            ),
            "{\"schema\":\"risunest.logical-record/v1\",\"kind\":\"root\",\"value\":{},\"ownerHeads\":[{\"owner\":{\"kind\":\"root-module-assets\",\"index\":0},\"present\":false,\"manifestHash\":null,\"entryCount\":0,\"propertyIndex\":0}]}".to_owned(),
        ] {
            assert!(decode_logical_record(invalid.as_bytes()).is_err());
        }
    }

    #[test]
    fn nullable_payload_hashes_preserve_missing_aliases_without_dependencies() {
        let record = LogicalRecordEnvelope::Cold {
            object_hash: None,
            size: 12,
            metadata: json!({ "name": "expected-missing.json" }),
        };

        let encoded = encode_logical_record(&record).unwrap();
        let json: Value = serde_json::from_slice(&encoded.bytes).unwrap();
        assert!(json["objectHash"].is_null());
        assert!(record.dependency_hashes().is_empty());
        assert_eq!(decode_logical_record(&encoded.bytes).unwrap(), record);

        assert!(encode_logical_record(&LogicalRecordEnvelope::Asset {
            object_hash: Some("1".repeat(64)),
            size: 0,
            metadata: json!({}),
        })
        .is_err());
        assert!(encode_logical_record(&LogicalRecordEnvelope::Inlay {
            object_hash: None,
            size: 0,
            metadata: Value::Null,
        })
        .is_err());
    }

    #[test]
    fn asset_alias_metadata_preserves_typed_and_extension_fields() {
        let typed = LogicalAssetAliasMetadata {
            mime: "image/webp".to_owned(),
            name: "inlay.webp".to_owned(),
            ext: "webp".to_owned(),
            inlay_type: Some("image".to_owned()),
            width: Some(320),
            height: Some(200),
            metadata: json!({ "source": "legacy", "mime": "extension-value" }),
        };
        let encoded = encode_asset_alias_metadata(&typed).unwrap();

        assert_eq!(decode_asset_alias_metadata(&encoded).unwrap(), typed);
        assert_eq!(encoded["mime"], "image/webp");
        assert_eq!(encoded["metadata"]["mime"], "extension-value");
    }

    #[test]
    fn message_page_codec_enforces_the_shared_128_message_boundary() {
        let messages = (0..LOGICAL_MESSAGE_PAGE_SIZE)
            .map(|index| json!({ "chatId": format!("message-{index}"), "data": index }))
            .collect::<Vec<_>>();

        let encoded = encode_message_page(&messages).unwrap();
        assert_eq!(decode_message_page(&encoded.bytes).unwrap(), messages);
        assert_eq!(encoded.size, encoded.bytes.len() as u64);

        let mut oversized = messages;
        oversized.push(json!({ "chatId": "message-128" }));
        assert!(encode_message_page(&oversized).is_err());
    }
}
