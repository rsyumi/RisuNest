pub use crate::logical_records::LogicalRecordError as LogicalDeltaError;
pub use crate::logical_records::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
pub const LOGICAL_MANIFEST_SCHEMA: &str = "risunest.logical-manifest/v1";
pub const MAX_LOGICAL_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_LOGICAL_MANIFEST_RECORDS: usize = 250_000;
pub const MAX_LOGICAL_MANIFEST_OBJECTS: usize = 500_000;
const MAX_LOGICAL_MANIFEST_ID_BYTES: usize = 1024;
const MAX_GENERATION_SEQUENCE_DIGITS: usize = 64;

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

// The projected-record manifest builder is the test-facing twin of the
// indexed builder; unit and timeout fixtures build manifests through it.
#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedLogicalRecord {
    locator: LogicalRecordLocator,
    state: ProjectedLogicalRecordState,
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuiltLogicalRecordObject {
    pub key: String,
    pub object: EncodedLogicalObject,
}

#[cfg_attr(not(test), allow(dead_code))]
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

fn compare_generation_sequences(left: &str, right: &str) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
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
                if compare_generation_sequences(
                    &record.deleted_generation_sequence,
                    &manifest.generation_sequence,
                )
                .is_gt()
                {
                    return Err(invalid(
                        "tombstone deletedGenerationSequence cannot exceed generationSequence",
                    ));
                }
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn logical_manifest_rejects_tombstones_from_a_future_generation() {
        let result = build_logical_manifest(LogicalManifestBuilderInput {
            library_id: "library-1".to_owned(),
            generation: "generation-1".to_owned(),
            generation_sequence: "1".to_owned(),
            parent_generation: None,
            source_revision: 1,
            records: vec![ProjectedLogicalRecord::tombstone(
                LogicalRecordLocator::Plugin {
                    storage_key: "deleted-plugin".to_owned(),
                },
                "2".to_owned(),
            )],
        });

        assert!(result.is_err());
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
