use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_LOGICAL_RECORD_KEY_BYTES: usize = 64 * 1024;
pub const LOGICAL_MESSAGE_PAGE_SIZE: usize = 128;
pub const LOGICAL_RECORD_SCHEMA: &str = "risunest.logical-record/v1";
pub const LOGICAL_MESSAGE_PAGE_SCHEMA: &str = "risunest.logical-message-page/v1";
pub const LOGICAL_MANIFEST_SCHEMA: &str = "risunest.logical-manifest/v1";
pub const MAX_LOGICAL_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_LOGICAL_MANIFEST_RECORDS: usize = 250_000;
pub const MAX_LOGICAL_MANIFEST_OBJECTS: usize = 500_000;

const JAVASCRIPT_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const MAX_LOGICAL_MANIFEST_ID_BYTES: usize = 1024;
const MAX_GENERATION_SEQUENCE_DIGITS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalRecordLocator {
    Root,
    Preset {
        preset_id: String,
    },
    Plugin {
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
    fn validate(&self) -> Result<(), LogicalDeltaError> {
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
) -> Result<Value, LogicalDeltaError> {
    metadata.validate()?;
    serde_json::to_value(metadata).map_err(|error| {
        invalid(format!(
            "logical asset alias metadata encoding failed: {error}"
        ))
    })
}

pub fn decode_asset_alias_metadata(
    value: &Value,
) -> Result<LogicalAssetAliasMetadata, LogicalDeltaError> {
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
        }
    }

    pub fn present(
        owner: LogicalOwnerLocator,
        manifest_hash: String,
        entry_count: u64,
    ) -> Result<Self, LogicalDeltaError> {
        let head = Self {
            owner,
            present: true,
            manifest_hash: Some(manifest_hash),
            entry_count,
        };
        head.validate()?;
        Ok(head)
    }

    fn validate(&self) -> Result<(), LogicalDeltaError> {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalManifestLiveRecord {
    pub key: String,
    pub state: String,
    pub object_hash: String,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalManifestTombstoneRecord {
    pub key: String,
    pub state: String,
    pub deleted_generation_sequence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LogicalManifestRecord {
    Live(LogicalManifestLiveRecord),
    Tombstone(LogicalManifestTombstoneRecord),
}

impl LogicalManifestRecord {
    pub fn key(&self) -> &str {
        match self {
            Self::Live(record) => &record.key,
            Self::Tombstone(record) => &record.key,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalManifestObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogicalManifest {
    pub schema: String,
    pub library_id: String,
    pub generation: String,
    pub generation_sequence: String,
    pub parent_generation: Option<String>,
    pub source_revision: u64,
    pub records: Vec<LogicalManifestRecord>,
    pub objects: Vec<LogicalManifestObject>,
}

#[derive(Clone, Debug, PartialEq)]
enum ProjectedLogicalRecordState {
    Live {
        record: LogicalRecordEnvelope,
        dependencies: Vec<LogicalManifestObject>,
    },
    Tombstone {
        deleted_generation_sequence: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedLogicalRecord {
    locator: LogicalRecordLocator,
    state: ProjectedLogicalRecordState,
}

impl ProjectedLogicalRecord {
    pub fn live(
        locator: LogicalRecordLocator,
        record: LogicalRecordEnvelope,
        dependencies: Vec<LogicalManifestObject>,
    ) -> Self {
        Self {
            locator,
            state: ProjectedLogicalRecordState::Live {
                record,
                dependencies,
            },
        }
    }

    pub fn tombstone(locator: LogicalRecordLocator, deleted_generation_sequence: String) -> Self {
        Self {
            locator,
            state: ProjectedLogicalRecordState::Tombstone {
                deleted_generation_sequence,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogicalManifestBuilderInput {
    pub library_id: String,
    pub generation: String,
    pub generation_sequence: String,
    pub parent_generation: Option<String>,
    pub source_revision: u64,
    pub records: Vec<ProjectedLogicalRecord>,
}

#[derive(Clone, Debug, PartialEq)]
enum IndexedLogicalRecordState {
    Live {
        object: LogicalManifestObject,
        dependencies: Vec<LogicalManifestObject>,
    },
    Tombstone {
        deleted_generation_sequence: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexedLogicalRecord {
    key: String,
    state: IndexedLogicalRecordState,
}

impl IndexedLogicalRecord {
    pub fn live(
        key: String,
        object: LogicalManifestObject,
        dependencies: Vec<LogicalManifestObject>,
    ) -> Self {
        Self {
            key,
            state: IndexedLogicalRecordState::Live {
                object,
                dependencies,
            },
        }
    }

    pub fn tombstone(key: String, deleted_generation_sequence: String) -> Self {
        Self {
            key,
            state: IndexedLogicalRecordState::Tombstone {
                deleted_generation_sequence,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexedLogicalManifestBuilderInput {
    pub library_id: String,
    pub generation: String,
    pub generation_sequence: String,
    pub parent_generation: Option<String>,
    pub source_revision: u64,
    pub records: Vec<IndexedLogicalRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltLogicalRecordObject {
    pub key: String,
    pub object: EncodedLogicalObject,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuiltLogicalManifest {
    pub manifest: LogicalManifest,
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: String,
    pub record_objects: Vec<BuiltLogicalRecordObject>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BuiltIndexedLogicalManifest {
    pub manifest: LogicalManifest,
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalDeltaError(String);

impl std::fmt::Display for LogicalDeltaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LogicalDeltaError {}

fn invalid(message: impl Into<String>) -> LogicalDeltaError {
    LogicalDeltaError(message.into())
}

fn validate_safe_integer(value: u64, description: &str) -> Result<(), LogicalDeltaError> {
    if value > JAVASCRIPT_MAX_SAFE_INTEGER {
        return Err(invalid(format!(
            "{description} exceeds the JavaScript safe integer limit"
        )));
    }
    Ok(())
}

fn validate_hash(value: &str, description: &str) -> Result<(), LogicalDeltaError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(format!(
            "{description} must be a lowercase SHA-256"
        )));
    }
    Ok(())
}

fn validate_object_descriptor(hash: &str, size: u64) -> Result<(), LogicalDeltaError> {
    validate_hash(hash, "object hash")?;
    validate_safe_integer(size, "object size")?;
    if (size == 0) != (hash == EMPTY_SHA256) {
        return Err(invalid("empty object must use the SHA-256 of empty bytes"));
    }
    Ok(())
}

fn validate_owner_heads(heads: &[LogicalOwnerHead]) -> Result<(), LogicalDeltaError> {
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
    fn validate(&self) -> Result<(), LogicalDeltaError> {
        match self {
            Self::Root { owner_heads, .. } => validate_owner_heads(owner_heads),
            Self::Preset {
                configured_index, ..
            }
            | Self::Character {
                configured_index, ..
            }
            | Self::Conversation {
                configured_index, ..
            } => {
                validate_safe_integer(*configured_index, "configured index")?;
                if let Self::Character { owner_heads, .. } = self {
                    validate_owner_heads(owner_heads)?;
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

    fn dependency_hashes(&self) -> Vec<String> {
        let mut hashes = match self {
            Self::Root { owner_heads, .. } | Self::Character { owner_heads, .. } => owner_heads
                .iter()
                .filter_map(|head| head.manifest_hash.clone())
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

fn encoded_object(bytes: Vec<u8>) -> Result<EncodedLogicalObject, LogicalDeltaError> {
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
) -> Result<EncodedLogicalObject, LogicalDeltaError> {
    record.validate()?;
    let bytes = serde_json::to_vec(&LogicalRecordDocument {
        schema: LOGICAL_RECORD_SCHEMA.to_owned(),
        record: record.clone(),
    })
    .map_err(|error| invalid(format!("logical record encoding failed: {error}")))?;
    encoded_object(bytes)
}

pub fn decode_logical_record(bytes: &[u8]) -> Result<LogicalRecordEnvelope, LogicalDeltaError> {
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

pub fn encode_message_page(messages: &[Value]) -> Result<EncodedLogicalObject, LogicalDeltaError> {
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

pub fn decode_message_page(bytes: &[u8]) -> Result<Vec<Value>, LogicalDeltaError> {
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

fn validate_bounded_manifest_string(
    value: &str,
    description: &str,
) -> Result<(), LogicalDeltaError> {
    if value.is_empty() || value.len() > MAX_LOGICAL_MANIFEST_ID_BYTES {
        return Err(invalid(format!(
            "{description} must be a bounded nonempty Unicode string"
        )));
    }
    Ok(())
}

fn validate_generation_sequence(value: &str, description: &str) -> Result<(), LogicalDeltaError> {
    let bytes = value.as_bytes();
    let canonical = bytes == b"0"
        || (!bytes.is_empty()
            && matches!(bytes.first(), Some(b'1'..=b'9'))
            && bytes[1..].iter().all(u8::is_ascii_digit));
    if bytes.is_empty() || bytes.len() > MAX_GENERATION_SEQUENCE_DIGITS || !canonical {
        return Err(invalid(format!(
            "{description} must be a canonical unsigned decimal string"
        )));
    }
    Ok(())
}

fn validate_sorted_hashes(hashes: &[String], description: &str) -> Result<(), LogicalDeltaError> {
    let mut previous: Option<&str> = None;
    for hash in hashes {
        validate_hash(hash, description)?;
        if previous.is_some_and(|value| value >= hash.as_str()) {
            return Err(invalid(format!("{description} must be sorted and unique")));
        }
        previous = Some(hash);
    }
    Ok(())
}

pub fn validate_logical_manifest(manifest: &LogicalManifest) -> Result<(), LogicalDeltaError> {
    if manifest.schema != LOGICAL_MANIFEST_SCHEMA {
        return Err(invalid("logical manifest schema is unsupported"));
    }
    validate_bounded_manifest_string(&manifest.library_id, "logical manifest libraryId")?;
    validate_bounded_manifest_string(&manifest.generation, "logical manifest generation")?;
    validate_generation_sequence(
        &manifest.generation_sequence,
        "logical manifest generationSequence",
    )?;
    if let Some(parent_generation) = &manifest.parent_generation {
        validate_bounded_manifest_string(parent_generation, "logical manifest parentGeneration")?;
    }
    validate_safe_integer(manifest.source_revision, "logical manifest sourceRevision")?;
    if manifest.records.len() > MAX_LOGICAL_MANIFEST_RECORDS {
        return Err(invalid("logical manifest records exceed the count limit"));
    }
    if manifest.objects.len() > MAX_LOGICAL_MANIFEST_OBJECTS {
        return Err(invalid("logical manifest objects exceed the count limit"));
    }

    let mut previous_key: Option<&str> = None;
    let mut reachable = BTreeSet::new();
    for record in &manifest.records {
        let key = record.key();
        decode_logical_record_key(key)?;
        if previous_key.is_some_and(|value| value >= key) {
            return Err(invalid(
                "logical manifest records must be sorted and unique",
            ));
        }
        previous_key = Some(key);
        match record {
            LogicalManifestRecord::Live(record) => {
                if record.state != "live" {
                    return Err(invalid("logical manifest live record state is invalid"));
                }
                validate_hash(&record.object_hash, "live record objectHash")?;
                validate_sorted_hashes(&record.dependencies, "live record dependencies")?;
                reachable.insert(record.object_hash.clone());
                reachable.extend(record.dependencies.iter().cloned());
            }
            LogicalManifestRecord::Tombstone(record) => {
                if record.state != "tombstone" {
                    return Err(invalid("logical manifest tombstone state is invalid"));
                }
                validate_generation_sequence(
                    &record.deleted_generation_sequence,
                    "tombstone deletedGenerationSequence",
                )?;
            }
        }
    }

    let mut object_hashes = BTreeSet::new();
    let mut previous_hash: Option<&str> = None;
    for object in &manifest.objects {
        validate_object_descriptor(&object.hash, object.size)?;
        if previous_hash.is_some_and(|value| value >= object.hash.as_str()) {
            return Err(invalid(
                "logical manifest objects must be sorted and unique",
            ));
        }
        previous_hash = Some(&object.hash);
        object_hashes.insert(object.hash.clone());
    }
    if reachable != object_hashes {
        return Err(invalid(
            "logical manifest objects must exactly cover referenced objects",
        ));
    }
    Ok(())
}

pub fn encode_logical_manifest(manifest: &LogicalManifest) -> Result<Vec<u8>, LogicalDeltaError> {
    validate_logical_manifest(manifest)?;
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| invalid(format!("logical manifest encoding failed: {error}")))?;
    if bytes.len() > MAX_LOGICAL_MANIFEST_BYTES {
        return Err(invalid("logical manifest exceeds the byte limit"));
    }
    Ok(bytes)
}

pub fn decode_logical_manifest(bytes: &[u8]) -> Result<LogicalManifest, LogicalDeltaError> {
    if bytes.len() > MAX_LOGICAL_MANIFEST_BYTES {
        return Err(invalid("logical manifest bytes exceed the byte limit"));
    }
    let manifest: LogicalManifest = serde_json::from_slice(bytes)
        .map_err(|_| invalid("logical manifest bytes are not valid UTF-8 JSON"))?;
    validate_logical_manifest(&manifest)?;
    if encode_logical_manifest(&manifest)? != bytes {
        return Err(invalid("logical manifest bytes are not canonical"));
    }
    Ok(manifest)
}

pub fn hash_logical_manifest(manifest: &LogicalManifest) -> Result<String, LogicalDeltaError> {
    Ok(hex::encode(Sha256::digest(encode_logical_manifest(
        manifest,
    )?)))
}

fn insert_manifest_object(
    objects: &mut BTreeMap<String, u64>,
    object: &LogicalManifestObject,
) -> Result<(), LogicalDeltaError> {
    validate_object_descriptor(&object.hash, object.size)?;
    if let Some(existing_size) = objects.insert(object.hash.clone(), object.size) {
        if existing_size != object.size {
            return Err(invalid("logical object hash has conflicting sizes"));
        }
    }
    Ok(())
}

fn insert_dependency_objects(
    objects: &mut BTreeMap<String, u64>,
    dependencies: Vec<LogicalManifestObject>,
    duplicate_message: &str,
) -> Result<Vec<String>, LogicalDeltaError> {
    let mut hashes = Vec::with_capacity(dependencies.len());
    let mut unique = BTreeSet::new();
    for dependency in dependencies {
        if !unique.insert(dependency.hash.clone()) {
            return Err(invalid(duplicate_message));
        }
        insert_manifest_object(objects, &dependency)?;
        hashes.push(dependency.hash);
    }
    hashes.sort();
    Ok(hashes)
}

struct CanonicalLogicalManifest {
    manifest: LogicalManifest,
    bytes: Vec<u8>,
    hash: String,
}

fn finish_logical_manifest(
    library_id: String,
    generation: String,
    generation_sequence: String,
    parent_generation: Option<String>,
    source_revision: u64,
    mut records: Vec<LogicalManifestRecord>,
    objects: BTreeMap<String, u64>,
) -> Result<CanonicalLogicalManifest, LogicalDeltaError> {
    records.sort_by(|left, right| left.key().cmp(right.key()));
    if records
        .windows(2)
        .any(|pair| pair[0].key() == pair[1].key())
    {
        return Err(invalid("logical manifest records contain duplicate keys"));
    }
    let manifest = LogicalManifest {
        schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
        library_id,
        generation,
        generation_sequence,
        parent_generation,
        source_revision,
        records,
        objects: objects
            .into_iter()
            .map(|(hash, size)| LogicalManifestObject { hash, size })
            .collect(),
    };
    let bytes = encode_logical_manifest(&manifest)?;
    let hash = hex::encode(Sha256::digest(&bytes));
    Ok(CanonicalLogicalManifest {
        manifest,
        bytes,
        hash,
    })
}

pub fn build_indexed_logical_manifest(
    input: IndexedLogicalManifestBuilderInput,
) -> Result<BuiltIndexedLogicalManifest, LogicalDeltaError> {
    let mut records = Vec::with_capacity(input.records.len());
    let mut objects = BTreeMap::new();

    for indexed in input.records {
        decode_logical_record_key(&indexed.key)?;
        match indexed.state {
            IndexedLogicalRecordState::Live {
                object,
                dependencies,
            } => {
                insert_manifest_object(&mut objects, &object)?;
                let dependency_hashes = insert_dependency_objects(
                    &mut objects,
                    dependencies,
                    "indexed logical record dependency descriptors are duplicated",
                )?;
                records.push(LogicalManifestRecord::Live(LogicalManifestLiveRecord {
                    key: indexed.key,
                    state: "live".to_owned(),
                    object_hash: object.hash,
                    dependencies: dependency_hashes,
                }));
            }
            IndexedLogicalRecordState::Tombstone {
                deleted_generation_sequence,
            } => {
                validate_generation_sequence(
                    &deleted_generation_sequence,
                    "tombstone deletedGenerationSequence",
                )?;
                records.push(LogicalManifestRecord::Tombstone(
                    LogicalManifestTombstoneRecord {
                        key: indexed.key,
                        state: "tombstone".to_owned(),
                        deleted_generation_sequence,
                    },
                ));
            }
        }
    }

    let canonical = finish_logical_manifest(
        input.library_id,
        input.generation,
        input.generation_sequence,
        input.parent_generation,
        input.source_revision,
        records,
        objects,
    )?;
    Ok(BuiltIndexedLogicalManifest {
        manifest: canonical.manifest,
        manifest_bytes: canonical.bytes,
        manifest_hash: canonical.hash,
    })
}

pub fn build_logical_manifest(
    input: LogicalManifestBuilderInput,
) -> Result<BuiltLogicalManifest, LogicalDeltaError> {
    let mut records = Vec::with_capacity(input.records.len());
    let mut objects = BTreeMap::new();
    let mut record_objects = Vec::new();

    for projected in input.records {
        let key = encode_logical_record_key(&projected.locator)?;
        match projected.state {
            ProjectedLogicalRecordState::Live {
                record,
                dependencies,
            } => {
                let expected_dependencies = record.dependency_hashes();
                let provided_dependencies = insert_dependency_objects(
                    &mut objects,
                    dependencies,
                    "logical record dependency descriptors are duplicated",
                )?;
                let dependencies_match = if matches!(
                    &record,
                    LogicalRecordEnvelope::Root { .. } | LogicalRecordEnvelope::Character { .. }
                ) {
                    expected_dependencies
                        .iter()
                        .all(|hash| provided_dependencies.binary_search(hash).is_ok())
                } else {
                    provided_dependencies == expected_dependencies
                };
                if !dependencies_match {
                    return Err(invalid(
                        "logical record dependency descriptors do not match its envelope",
                    ));
                }

                let encoded = encode_logical_record(&record)?;
                insert_manifest_object(
                    &mut objects,
                    &LogicalManifestObject {
                        hash: encoded.hash.clone(),
                        size: encoded.size,
                    },
                )?;
                records.push(LogicalManifestRecord::Live(LogicalManifestLiveRecord {
                    key: key.clone(),
                    state: "live".to_owned(),
                    object_hash: encoded.hash.clone(),
                    dependencies: provided_dependencies,
                }));
                record_objects.push(BuiltLogicalRecordObject {
                    key,
                    object: encoded,
                });
            }
            ProjectedLogicalRecordState::Tombstone {
                deleted_generation_sequence,
            } => {
                validate_generation_sequence(
                    &deleted_generation_sequence,
                    "tombstone deletedGenerationSequence",
                )?;
                records.push(LogicalManifestRecord::Tombstone(
                    LogicalManifestTombstoneRecord {
                        key,
                        state: "tombstone".to_owned(),
                        deleted_generation_sequence,
                    },
                ));
            }
        }
    }

    record_objects.sort_by(|left, right| left.key.cmp(&right.key));
    let canonical = finish_logical_manifest(
        input.library_id,
        input.generation,
        input.generation_sequence,
        input.parent_generation,
        input.source_revision,
        records,
        objects,
    )?;
    Ok(BuiltLogicalManifest {
        manifest: canonical.manifest,
        manifest_bytes: canonical.bytes,
        manifest_hash: canonical.hash,
        record_objects,
    })
}

fn validate_component(
    value: &str,
    description: &str,
    allow_empty: bool,
) -> Result<(), LogicalDeltaError> {
    if (!allow_empty && value.is_empty()) || value.len() > MAX_LOGICAL_RECORD_KEY_BYTES {
        return Err(invalid(format!("invalid logical record {description}")));
    }
    Ok(())
}

fn locator_parts(
    locator: &LogicalRecordLocator,
) -> Result<(&'static str, Vec<&str>), LogicalDeltaError> {
    let parts = match locator {
        LogicalRecordLocator::Root => ("root", vec![]),
        LogicalRecordLocator::Preset { preset_id } => {
            validate_component(preset_id, "presetId", false)?;
            ("preset", vec![preset_id.as_str()])
        }
        LogicalRecordLocator::Plugin { storage_key } => {
            validate_component(storage_key, "storageKey", true)?;
            ("plugin", vec![storage_key.as_str()])
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
            validate_component(logical_key, "logicalKey", true)?;
            ("cold", vec![logical_key.as_str()])
        }
    };
    Ok(parts)
}

pub fn encode_logical_record_key(
    locator: &LogicalRecordLocator,
) -> Result<String, LogicalDeltaError> {
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

pub fn decode_logical_record_key(encoded: &str) -> Result<LogicalRecordLocator, LogicalDeltaError> {
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
        ("plugin", [storage_key]) => LogicalRecordLocator::Plugin {
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

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{
        build_indexed_logical_manifest, build_logical_manifest, decode_asset_alias_metadata,
        decode_logical_manifest, decode_logical_record, decode_logical_record_key,
        decode_message_page, encode_asset_alias_metadata, encode_logical_manifest,
        encode_logical_record, encode_logical_record_key, encode_message_page,
        IndexedLogicalManifestBuilderInput, IndexedLogicalRecord, LogicalAssetAliasMetadata,
        LogicalManifest, LogicalManifestBuilderInput, LogicalManifestLiveRecord,
        LogicalManifestObject, LogicalManifestRecord, LogicalManifestTombstoneRecord,
        LogicalOwnerHead, LogicalOwnerLocator, LogicalRecordEnvelope, LogicalRecordLocator,
        ProjectedLogicalRecord, LOGICAL_MANIFEST_SCHEMA, LOGICAL_MESSAGE_PAGE_SIZE,
    };

    #[test]
    fn logical_record_keys_match_typescript_v1_literals_and_round_trip_opaque_components() {
        let fixtures = [
            (LogicalRecordLocator::Root, "r1:root"),
            (
                LogicalRecordLocator::Preset {
                    preset_id: "0".to_owned(),
                },
                "r1:preset:WyIwIl0",
            ),
            (
                LogicalRecordLocator::Plugin {
                    storage_key: "unicode-한국어\\path".to_owned(),
                },
                "r1:plugin:WyJ1bmljb2RlLe2VnOq1reyWtFxccGF0aCJd",
            ),
            (
                LogicalRecordLocator::Conversation {
                    character_id: "character-1".to_owned(),
                    conversation_id: "chat:1/alpha".to_owned(),
                },
                "r1:conversation:WyJjaGFyYWN0ZXItMSIsImNoYXQ6MS9hbHBoYSJd",
            ),
            (
                LogicalRecordLocator::Asset {
                    logical_key: String::new(),
                },
                "r1:asset:WyIiXQ",
            ),
        ];

        for (locator, expected) in fixtures {
            assert_eq!(encode_logical_record_key(&locator).unwrap(), expected);
            assert_eq!(decode_logical_record_key(expected).unwrap(), locator);
        }
    }

    #[test]
    fn logical_record_keys_reject_noncanonical_or_invalid_components() {
        assert!(decode_logical_record_key("r1:preset:WyIwIl0=").is_err());
        assert!(decode_logical_record_key("r1:root:W10").is_err());
        assert!(encode_logical_record_key(&LogicalRecordLocator::Character {
            character_id: String::new(),
        })
        .is_err());
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
                )
                .unwrap()],
            },
            LogicalRecordEnvelope::Preset {
                configured_index: 4,
                value: json!({ "name": "Preset" }),
            },
            LogicalRecordEnvelope::Plugin {
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

    #[test]
    fn logical_manifest_json_matches_the_typescript_field_order_and_schema() {
        let record_hash = "1".repeat(64);
        let dependency_hash = "2".repeat(64);
        let manifest = LogicalManifest {
            schema: LOGICAL_MANIFEST_SCHEMA.to_owned(),
            library_id: "library-1".to_owned(),
            generation: "generation-2".to_owned(),
            generation_sequence: "2".to_owned(),
            parent_generation: Some("generation-1".to_owned()),
            source_revision: 9,
            records: vec![
                LogicalManifestRecord::Tombstone(LogicalManifestTombstoneRecord {
                    key: "r1:asset:WyIiXQ".to_owned(),
                    state: "tombstone".to_owned(),
                    deleted_generation_sequence: "2".to_owned(),
                }),
                LogicalManifestRecord::Live(LogicalManifestLiveRecord {
                    key: "r1:root".to_owned(),
                    state: "live".to_owned(),
                    object_hash: record_hash.clone(),
                    dependencies: vec![dependency_hash.clone()],
                }),
            ],
            objects: vec![
                LogicalManifestObject {
                    hash: record_hash.clone(),
                    size: 8,
                },
                LogicalManifestObject {
                    hash: dependency_hash.clone(),
                    size: 9,
                },
            ],
        };

        let expected = format!(
            concat!(
                "{{\"schema\":\"risunest.logical-manifest/v1\",",
                "\"libraryId\":\"library-1\",\"generation\":\"generation-2\",",
                "\"generationSequence\":\"2\",\"parentGeneration\":\"generation-1\",",
                "\"sourceRevision\":9,\"records\":[",
                "{{\"key\":\"r1:asset:WyIiXQ\",\"state\":\"tombstone\",",
                "\"deletedGenerationSequence\":\"2\"}},",
                "{{\"key\":\"r1:root\",\"state\":\"live\",",
                "\"objectHash\":\"{}\",\"dependencies\":[\"{}\"]}}],",
                "\"objects\":[{{\"hash\":\"{}\",\"size\":8}},",
                "{{\"hash\":\"{}\",\"size\":9}}]}}"
            ),
            record_hash, dependency_hash, record_hash, dependency_hash,
        );

        let bytes = encode_logical_manifest(&manifest).unwrap();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
        assert_eq!(decode_logical_manifest(&bytes).unwrap(), manifest);
    }

    #[test]
    fn pure_manifest_builder_sorts_records_objects_and_explicit_tombstones() {
        let page = encode_message_page(&[json!({ "chatId": "message-1", "data": "Hi" })]).unwrap();
        let conversation = LogicalRecordEnvelope::Conversation {
            configured_index: 0,
            recent_at: 10,
            detail: json!({ "id": "chat-1", "name": "Chat" }),
            message_page_hashes: vec![page.hash.clone()],
        };
        let root = LogicalRecordEnvelope::Root {
            value: json!({ "username": "Fixture" }),
            owner_heads: vec![],
        };
        let built = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:7".to_owned(),
            generation_sequence: "7".to_owned(),
            parent_generation: Some("device-a:6".to_owned()),
            source_revision: 7,
            records: vec![
                ProjectedLogicalRecord::live(
                    LogicalRecordLocator::Conversation {
                        character_id: "character-1".to_owned(),
                        conversation_id: "chat-1".to_owned(),
                    },
                    conversation,
                    vec![LogicalManifestObject {
                        hash: page.hash.clone(),
                        size: page.size,
                    }],
                ),
                ProjectedLogicalRecord::tombstone(
                    LogicalRecordLocator::Asset {
                        logical_key: "deleted.bin".to_owned(),
                    },
                    "7".to_owned(),
                ),
                ProjectedLogicalRecord::live(LogicalRecordLocator::Root, root, vec![]),
            ],
        })
        .unwrap();

        let keys = built
            .manifest
            .records
            .iter()
            .map(LogicalManifestRecord::key)
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "r1:asset:WyJkZWxldGVkLmJpbiJd",
                "r1:conversation:WyJjaGFyYWN0ZXItMSIsImNoYXQtMSJd",
                "r1:root",
            ]
        );
        assert!(matches!(
            &built.manifest.records[0],
            LogicalManifestRecord::Tombstone(record)
                if record.deleted_generation_sequence == "7"
        ));
        assert!(built
            .manifest
            .objects
            .windows(2)
            .all(|pair| pair[0].hash < pair[1].hash));
        assert!(built
            .manifest
            .objects
            .iter()
            .any(|object| object.hash == page.hash));
        assert_eq!(
            decode_logical_manifest(&built.manifest_bytes).unwrap(),
            built.manifest
        );
    }

    #[test]
    fn manifest_builder_rejects_missing_dependencies_and_duplicate_logical_keys() {
        let page_hash = "4".repeat(64);
        let conversation = LogicalRecordEnvelope::Conversation {
            configured_index: 0,
            recent_at: 0,
            detail: json!({ "id": "chat-1", "name": "Chat" }),
            message_page_hashes: vec![page_hash],
        };
        let root = LogicalRecordEnvelope::Root {
            value: json!({}),
            owner_heads: vec![],
        };
        let base = || LogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: None,
            source_revision: 1,
            records: vec![],
        };

        let mut missing = base();
        missing.records.push(ProjectedLogicalRecord::live(
            LogicalRecordLocator::Conversation {
                character_id: "character-1".to_owned(),
                conversation_id: "chat-1".to_owned(),
            },
            conversation,
            vec![],
        ));
        assert!(build_logical_manifest(missing).is_err());

        let mut duplicate = base();
        duplicate.records = vec![
            ProjectedLogicalRecord::live(LogicalRecordLocator::Root, root.clone(), vec![]),
            ProjectedLogicalRecord::live(LogicalRecordLocator::Root, root, vec![]),
        ];
        assert!(build_logical_manifest(duplicate).is_err());
    }

    #[test]
    fn indexed_manifest_builder_uses_only_compact_record_metadata() {
        let record_hash = "1".repeat(64);
        let dependency_hash = "2".repeat(64);
        let built = build_indexed_logical_manifest(IndexedLogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:8".to_owned(),
            generation_sequence: "8".to_owned(),
            parent_generation: Some("device-a:7".to_owned()),
            source_revision: 8,
            records: vec![IndexedLogicalRecord::live(
                "r1:root".to_owned(),
                LogicalManifestObject {
                    hash: record_hash.clone(),
                    size: 11,
                },
                vec![LogicalManifestObject {
                    hash: dependency_hash.clone(),
                    size: 17,
                }],
            )],
        })
        .unwrap();

        assert_eq!(
            built.manifest.records,
            vec![LogicalManifestRecord::Live(LogicalManifestLiveRecord {
                key: "r1:root".to_owned(),
                state: "live".to_owned(),
                object_hash: record_hash.clone(),
                dependencies: vec![dependency_hash.clone()],
            })]
        );
        assert_eq!(
            built.manifest.objects,
            vec![
                LogicalManifestObject {
                    hash: record_hash,
                    size: 11,
                },
                LogicalManifestObject {
                    hash: dependency_hash,
                    size: 17,
                },
            ]
        );
        assert_eq!(
            decode_logical_manifest(&built.manifest_bytes).unwrap(),
            built.manifest
        );
    }

    #[test]
    fn indexed_manifest_bytes_equal_the_equivalent_projected_build() {
        let root = LogicalRecordEnvelope::Root {
            value: json!({ "username": "Fixture" }),
            owner_heads: vec![],
        };
        let encoded_root = encode_logical_record(&root).unwrap();
        let projected = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:9".to_owned(),
            generation_sequence: "9".to_owned(),
            parent_generation: Some("device-a:8".to_owned()),
            source_revision: 9,
            records: vec![
                ProjectedLogicalRecord::live(LogicalRecordLocator::Root, root, vec![]),
                ProjectedLogicalRecord::tombstone(
                    LogicalRecordLocator::Asset {
                        logical_key: "deleted.bin".to_owned(),
                    },
                    "9".to_owned(),
                ),
            ],
        })
        .unwrap();
        let indexed = build_indexed_logical_manifest(IndexedLogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:9".to_owned(),
            generation_sequence: "9".to_owned(),
            parent_generation: Some("device-a:8".to_owned()),
            source_revision: 9,
            records: vec![
                IndexedLogicalRecord::live(
                    "r1:root".to_owned(),
                    LogicalManifestObject {
                        hash: encoded_root.hash,
                        size: encoded_root.size,
                    },
                    vec![],
                ),
                IndexedLogicalRecord::tombstone(
                    "r1:asset:WyJkZWxldGVkLmJpbiJd".to_owned(),
                    "9".to_owned(),
                ),
            ],
        })
        .unwrap();

        assert_eq!(indexed.manifest, projected.manifest);
        assert_eq!(indexed.manifest_bytes, projected.manifest_bytes);
        assert_eq!(indexed.manifest_hash, projected.manifest_hash);
    }

    #[test]
    fn indexed_manifest_builder_rejects_duplicate_keys_and_conflicting_objects() {
        let base = || IndexedLogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "device-a:10".to_owned(),
            generation_sequence: "10".to_owned(),
            parent_generation: Some("device-a:9".to_owned()),
            source_revision: 10,
            records: vec![],
        };

        let mut duplicate_keys = base();
        duplicate_keys.records = vec![
            IndexedLogicalRecord::tombstone("r1:root".to_owned(), "10".to_owned()),
            IndexedLogicalRecord::tombstone("r1:root".to_owned(), "10".to_owned()),
        ];
        assert!(build_indexed_logical_manifest(duplicate_keys).is_err());

        let shared_hash = "5".repeat(64);
        let mut conflicting_objects = base();
        conflicting_objects.records = vec![
            IndexedLogicalRecord::live(
                "r1:root".to_owned(),
                LogicalManifestObject {
                    hash: shared_hash.clone(),
                    size: 7,
                },
                vec![],
            ),
            IndexedLogicalRecord::live(
                "r1:preset:WyIwIl0".to_owned(),
                LogicalManifestObject {
                    hash: shared_hash,
                    size: 8,
                },
                vec![],
            ),
        ];
        assert!(build_indexed_logical_manifest(conflicting_objects).is_err());

        let duplicate_hash = "6".repeat(64);
        let mut duplicate_dependencies = base();
        duplicate_dependencies
            .records
            .push(IndexedLogicalRecord::live(
                "r1:root".to_owned(),
                LogicalManifestObject {
                    hash: "7".repeat(64),
                    size: 9,
                },
                vec![
                    LogicalManifestObject {
                        hash: duplicate_hash.clone(),
                        size: 5,
                    },
                    LogicalManifestObject {
                        hash: duplicate_hash,
                        size: 5,
                    },
                ],
            ));
        assert!(build_indexed_logical_manifest(duplicate_dependencies).is_err());

        let mut invalid_key = base();
        invalid_key.records.push(IndexedLogicalRecord::tombstone(
            "root".to_owned(),
            "10".to_owned(),
        ));
        assert!(build_indexed_logical_manifest(invalid_key).is_err());
    }
}
