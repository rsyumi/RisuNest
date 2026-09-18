//! PDS record projection without peer generation or index ownership.
use super::{StoreError, StoreResult};
use crate::{
    asset_repository::{
        owner_manifest_codec::{decode_owner_manifest, encode_owner_manifest, OwnerManifestEntry},
        PayloadCas,
    },
    logical_records::*,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub(super) struct ValidatedOwnerHead {
    pub(super) head: LogicalOwnerHead,
    pub(super) tuples: Option<Vec<Value>>,
    pub(super) dependencies: Vec<LogicalManifestObject>,
    pub(super) derived_manifest: Option<Vec<u8>>,
}

pub(super) fn load_owner_heads(
    connection: &Connection,
    pds_generation: &str,
    character_id: Option<&str>,
) -> StoreResult<Vec<LogicalOwnerHead>> {
    let (sql, locator): (&str, Option<&str>) = match character_id {
        Some(character_id) => (
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
               AND owner_locator = ?2
             ORDER BY owner_kind ASC, owner_locator ASC",
            Some(character_id),
        ),
        None => (
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind IN (
                'root-module-assets', 'persona-embedded-module-assets'
             )
             ORDER BY owner_kind ASC, CAST(owner_locator AS INTEGER) ASC",
            None,
        ),
    };
    let mut statement = connection.prepare(sql)?;
    let mut rows = match locator {
        Some(locator) => statement.query(params![pds_generation, locator])?,
        None => statement.query([pds_generation])?,
    };
    let mut heads = Vec::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let locator: String = row.get(1)?;
        let present: bool = row.get(2)?;
        let manifest_hash: Option<String> = row.get(3)?;
        let entry_count = nonnegative_u64(row.get(4)?, "owner entry count")?;
        let owner = match kind.as_str() {
            "character-additional-assets" => LogicalOwnerLocator::CharacterAdditional {
                character_id: locator,
            },
            "root-module-assets" => LogicalOwnerLocator::RootModule {
                index: parse_owner_index(&locator)?,
            },
            "persona-embedded-module-assets" => LogicalOwnerLocator::PersonaEmbeddedModule {
                index: parse_owner_index(&locator)?,
            },
            _ => return validation("asset owner kind is unsupported"),
        };
        let head = if present {
            let hash = manifest_hash.ok_or_else(|| StoreError::Validation {
                message: "present owner head has no manifest hash".to_owned(),
            })?;
            LogicalOwnerHead::unpositioned_present(owner, hash, entry_count).map_err(codec_error)?
        } else {
            if manifest_hash.is_some() || entry_count != 0 {
                return validation("absent owner head contains manifest data");
            }
            LogicalOwnerHead::absent(owner)
        };
        heads.push(head);
    }
    Ok(heads)
}

fn resolve_owner_heads_with_sizes(
    connection: &Connection,
    cas: &PayloadCas,
    generation: &str,
    value: &Value,
    character_id: Option<&str>,
    size: &dyn Fn(&str) -> StoreResult<u64>,
) -> StoreResult<Vec<ValidatedOwnerHead>> {
    let mut heads = validate_owner_heads_with_sizes(
        cas,
        load_owner_heads(connection, generation, character_id)?,
        size,
    )?;
    for (owner, parent, property) in owner_parents(value, character_id) {
        if heads.iter().any(|head| head.head.owner == owner) {
            continue;
        }
        let Some(property_value) = parent.get(property) else {
            heads.push(ValidatedOwnerHead {
                head: LogicalOwnerHead::absent(owner),
                tuples: None,
                dependencies: Vec::new(),
                derived_manifest: None,
            });
            continue;
        };
        let values = property_value
            .as_array()
            .ok_or_else(|| missing_source("owner array"))?;
        let mut entries = Vec::with_capacity(values.len());
        let mut dependencies = BTreeMap::new();
        for value in values {
            let tuple = value
                .as_array()
                .filter(|tuple| tuple.len() >= 3)
                .ok_or_else(|| missing_source("owner tuple"))?;
            let tuple: [String; 3] = tuple[..3]
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| missing_source("owner tuple string"))
                })
                .collect::<StoreResult<Vec<_>>>()?
                .try_into()
                .unwrap();
            let hash: Option<String> = connection.query_row(
                "SELECT object_hash FROM asset_aliases WHERE generation = ?1 AND kind = 'asset' AND logical_key = ?2",
                params![generation, tuple[1]], |row| row.get(0),
            ).optional()?.flatten();
            let payload_hash = hash
                .map(|hash| -> StoreResult<[u8; 32]> {
                    dependencies.insert(hash.clone(), size(&hash)?);
                    hex::decode(&hash)
                        .ok()
                        .and_then(|bytes| bytes.try_into().ok())
                        .ok_or_else(|| missing_source("owner payload hash"))
                })
                .transpose()?;
            entries.push(OwnerManifestEntry {
                tuple,
                payload_hash,
            });
        }
        let bytes = encode_owner_manifest(&entries).map_err(codec_error)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        dependencies.insert(hash.clone(), bytes.len() as u64);
        heads.push(ValidatedOwnerHead {
            head: LogicalOwnerHead::unpositioned_present(owner, hash, entries.len() as u64)
                .map_err(codec_error)?,
            tuples: Some(
                entries
                    .into_iter()
                    .map(|entry| Value::Array(entry.tuple.into_iter().map(Value::String).collect()))
                    .collect(),
            ),
            dependencies: dependencies
                .into_iter()
                .map(|(hash, size)| LogicalManifestObject { hash, size })
                .collect(),
            derived_manifest: Some(bytes),
        });
    }
    // Stored and derived heads must have the same order, including after import.
    heads.sort_by_key(|head| match &head.head.owner {
        LogicalOwnerLocator::CharacterAdditional { character_id } => (0, 0, character_id.clone()),
        LogicalOwnerLocator::PersonaEmbeddedModule { index } => (1, *index, String::new()),
        LogicalOwnerLocator::RootModule { index } => (2, *index, String::new()),
    });
    Ok(heads)
}

pub(super) fn owner_parents<'a>(
    value: &'a Value,
    character_id: Option<&str>,
) -> Vec<(LogicalOwnerLocator, &'a Value, &'static str)> {
    if let Some(character_id) = character_id {
        return vec![(
            LogicalOwnerLocator::CharacterAdditional {
                character_id: character_id.to_owned(),
            },
            value,
            "additionalAssets",
        )];
    }
    let mut parents = Vec::new();
    for (index, module) in value
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        parents.push((
            LogicalOwnerLocator::RootModule {
                index: index as u64,
            },
            module,
            "assets",
        ));
    }
    for (index, persona) in value
        .get("personas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(module) = persona
            .get("embeddedModule")
            .filter(|value| value.is_object())
        {
            parents.push((
                LogicalOwnerLocator::PersonaEmbeddedModule {
                    index: index as u64,
                },
                module,
                "assets",
            ));
        }
    }
    parents
}

fn validate_owner_heads_with_sizes(
    cas: &PayloadCas,
    heads: Vec<LogicalOwnerHead>,
    size: &dyn Fn(&str) -> StoreResult<u64>,
) -> StoreResult<Vec<ValidatedOwnerHead>> {
    let mut validated = Vec::with_capacity(heads.len());
    for head in heads {
        let Some(manifest_hash) = head.manifest_hash.as_deref() else {
            validated.push(ValidatedOwnerHead {
                head,
                tuples: None,
                dependencies: Vec::new(),
                derived_manifest: None,
            });
            continue;
        };
        let bytes = cas
            .read_object(manifest_hash)?
            .ok_or_else(|| StoreError::Validation {
                message: format!("referenced owner manifest {manifest_hash} is missing"),
            })?;
        verify_object_bytes(&bytes, manifest_hash, bytes.len() as u64)?;
        let entries = decode_owner_manifest(&bytes).map_err(codec_error)?;
        if encode_owner_manifest(&entries).map_err(codec_error)? != bytes {
            return validation("owner manifest bytes are not canonical");
        }
        if entries.len() as u64 != head.entry_count {
            return validation("owner manifest entry count does not match its head");
        }
        let mut dependencies = BTreeMap::from([(manifest_hash.to_owned(), bytes.len() as u64)]);
        let mut tuples = Vec::with_capacity(entries.len());
        for entry in entries {
            tuples.push(Value::Array(
                entry.tuple.into_iter().map(Value::String).collect(),
            ));
            if let Some(payload_hash) = entry.payload_hash {
                let hash = hex::encode(payload_hash);
                let size = size(&hash)?;
                if dependencies
                    .insert(hash, size)
                    .is_some_and(|old| old != size)
                {
                    return validation("owner dependency hash has conflicting sizes");
                }
            }
        }
        validated.push(ValidatedOwnerHead {
            head,
            derived_manifest: None,
            tuples: Some(tuples),
            dependencies: dependencies
                .into_iter()
                .map(|(hash, size)| LogicalManifestObject { hash, size })
                .collect(),
        });
    }
    Ok(validated)
}

pub(super) fn logical_owner_heads(heads: &[ValidatedOwnerHead]) -> Vec<LogicalOwnerHead> {
    heads.iter().map(|head| head.head.clone()).collect()
}

pub(super) fn owner_dependencies(
    heads: &[ValidatedOwnerHead],
) -> StoreResult<Vec<LogicalManifestObject>> {
    let mut dependencies = BTreeMap::new();
    for dependency in heads.iter().flat_map(|head| &head.dependencies) {
        if dependencies
            .insert(dependency.hash.clone(), dependency.size)
            .is_some_and(|old| old != dependency.size)
        {
            return validation("owner dependency hash has conflicting sizes");
        }
    }
    Ok(dependencies
        .into_iter()
        .map(|(hash, size)| LogicalManifestObject { hash, size })
        .collect())
}

pub(super) fn strip_owner_property(
    parent: &mut serde_json::Map<String, Value>,
    property: &str,
    head: &mut ValidatedOwnerHead,
    allow_trailing_fields: bool,
) -> StoreResult<()> {
    let property_index = parent.keys().position(|key| key == property);
    if property_index.is_some() != head.head.present {
        return validation("owner head property presence does not match its parent record");
    }
    if let Some(expected) = &head.tuples {
        let Some(actual) = parent.get(property).and_then(Value::as_array) else {
            return validation("owner manifest tuples do not match their parent record");
        };
        if actual.len() != expected.len() {
            return validation("owner manifest tuples do not match their parent record");
        }
        let mut has_trailing_fields = false;
        for (actual, expected) in actual.iter().zip(expected) {
            let (Some(actual), Some(expected)) = (actual.as_array(), expected.as_array()) else {
                return validation("owner manifest tuples do not match their parent record");
            };
            if actual.len() < 3
                || expected.len() != 3
                || actual[..3] != expected[..]
                || (!allow_trailing_fields && actual.len() != 3)
            {
                return validation("owner manifest tuples do not match their parent record");
            }
            has_trailing_fields |= actual.len() > 3;
        }
        head.head.property_index = Some(
            u64::try_from(property_index.expect("present owner property has an index")).map_err(
                |_| StoreError::Validation {
                    message: "owner property index exceeds the wire range".to_owned(),
                },
            )?,
        );
        if !has_trailing_fields {
            parent.shift_remove(property);
        }
    } else {
        head.head.property_index = None;
    }
    Ok(())
}

pub(super) fn strip_root_owner_properties(
    value: &mut Value,
    heads: &mut [ValidatedOwnerHead],
) -> StoreResult<()> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "logical root projection requires an object".to_owned(),
        })?;
    let mut by_identity = BTreeMap::new();
    for (head_index, head) in heads.iter().enumerate() {
        let identity = match &head.head.owner {
            LogicalOwnerLocator::RootModule { index } => format!("module:{index}"),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => format!("persona:{index}"),
            LogicalOwnerLocator::CharacterAdditional { .. } => {
                return validation("root logical record contains a character owner head")
            }
        };
        if by_identity.insert(identity, head_index).is_some() {
            return validation("root logical record contains duplicate owner heads");
        }
    }
    let mut expected = 0_usize;
    if let Some(modules) = root.get_mut("modules") {
        let modules = modules
            .as_array_mut()
            .ok_or_else(|| StoreError::Validation {
                message: "root modules must be an array".to_owned(),
            })?;
        for (index, module) in modules.iter_mut().enumerate() {
            let module = module
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "root module must be an object".to_owned(),
                })?;
            let head_index = *by_identity.get(&format!("module:{index}")).ok_or_else(|| {
                StoreError::Validation {
                    message: "root module owner head coverage is incomplete".to_owned(),
                }
            })?;
            strip_owner_property(module, "assets", &mut heads[head_index], true)?;
            expected += 1;
        }
    }
    if let Some(personas) = root.get_mut("personas") {
        let personas = personas
            .as_array_mut()
            .ok_or_else(|| StoreError::Validation {
                message: "root personas must be an array".to_owned(),
            })?;
        for (index, persona) in personas.iter_mut().enumerate() {
            let persona = persona
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "root persona must be an object".to_owned(),
                })?;
            let Some(embedded) = persona.get_mut("embeddedModule") else {
                continue;
            };
            let embedded = embedded
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "persona embeddedModule must be an object".to_owned(),
                })?;
            let head_index = *by_identity
                .get(&format!("persona:{index}"))
                .ok_or_else(|| StoreError::Validation {
                    message: "persona embedded module owner head coverage is incomplete".to_owned(),
                })?;
            strip_owner_property(embedded, "assets", &mut heads[head_index], true)?;
            expected += 1;
        }
    }
    if by_identity.len() != expected {
        return validation("root logical record contains owner heads for missing occurrences");
    }
    Ok(())
}

pub(super) fn strip_character_owner_property(
    detail: &mut Value,
    character_id: &str,
    heads: &mut [ValidatedOwnerHead],
) -> StoreResult<()> {
    let [head] = heads else {
        return validation("character owner head coverage must contain exactly one head");
    };
    if !matches!(
        &head.head.owner,
        LogicalOwnerLocator::CharacterAdditional { character_id: owner_id }
            if owner_id == character_id
    ) {
        return validation("character logical record owner head does not match its key");
    }
    let detail = detail
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "character logical projection requires an object".to_owned(),
        })?;
    strip_owner_property(detail, "additionalAssets", head, false)
}

pub(super) fn reconstruct_record_with_owner_objects(
    connection: &Connection,
    cas: &PayloadCas,
    pds_generation: &str,
    record_key: &str,
    message_page_hashes: Vec<String>,
    mut derived: impl FnMut(&[u8]) -> StoreResult<()>,
    size: &dyn Fn(&str) -> StoreResult<u64>,
) -> StoreResult<Vec<u8>> {
    let locator = decode_logical_record_key(record_key).map_err(codec_error)?;
    let envelope = match locator {
        LogicalRecordLocator::Root => {
            let raw = required_text(
                connection,
                "SELECT value FROM root WHERE generation = ?1",
                params![pds_generation],
                "root record source is missing",
            )?;
            let mut value: Value = serde_json::from_str(&raw)?;
            let mut owner_heads = resolve_owner_heads_with_sizes(
                connection,
                cas,
                pds_generation,
                &value,
                None,
                size,
            )?;
            for head in &owner_heads {
                if let Some(bytes) = &head.derived_manifest {
                    derived(bytes)?;
                }
            }
            strip_root_owner_properties(&mut value, &mut owner_heads)?;
            LogicalRecordEnvelope::Root {
                value,
                owner_heads: logical_owner_heads(&owner_heads),
            }
        }
        LogicalRecordLocator::Preset { preset_id } => {
            let (configured_index, raw): (i64, String) = connection
                .query_row(
                    "SELECT configured_index, value FROM bot_presets
                     WHERE generation = ?1 AND preset_id = ?2",
                    params![pds_generation, preset_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("preset"))?;
            LogicalRecordEnvelope::Preset {
                configured_index: nonnegative_u64(configured_index, "preset configured index")?,
                value: serde_json::from_str(&raw)?,
            }
        }
        LogicalRecordLocator::Plugin { owner, storage_key } => {
            let (ordinal, raw): (i64, String) = connection
                .query_row(
                    "SELECT ordinal, value FROM plugin_storage
                     WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                    params![pds_generation, owner, storage_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("plugin"))?;
            LogicalRecordEnvelope::Plugin {
                owner: owner.clone(),
                ordinal: nonnegative_u64(ordinal, "plugin storage ordinal")?,
                value: serde_json::from_str(&raw)?,
            }
        }
        LogicalRecordLocator::Character { character_id } => {
            let row: (
                i64,
                i64,
                bool,
                String,
                Option<String>,
                String,
                Option<String>,
                Option<i64>,
                String,
                Option<String>,
            ) = connection
                .query_row(
                    "SELECT configured_index, recent_at, trashed, name, image, type,
                            creator_notes, trash_time, detail, archived_object
                     FROM characters
                     WHERE generation = ?1 AND character_id = ?2",
                    params![pds_generation, character_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| missing_source("character"))?;
            let (
                configured_index,
                recent_at,
                trashed,
                name,
                image,
                character_type,
                creator_notes,
                trash_time,
                raw,
                archived_object,
            ) = row;
            if let Some(archived_object) = archived_object {
                let archived: super::archive::ArchivedObject = serde_json::from_str(&archived_object)?;
                let owner_heads = load_owner_heads(connection, pds_generation, Some(&character_id))?;
                let owner_heads = validate_owner_heads_with_sizes(cas, owner_heads, size)?;
                let archive_object_size = size(&archived.object_hash)?;
                return Ok(encode_logical_record(&LogicalRecordEnvelope::ArchivedCharacter {
                    configured_index: nonnegative_u64(
                        configured_index,
                        "character configured index",
                    )?,
                    recent_at,
                    trashed,
                    name,
                    image,
                    character_type,
                    creator_notes,
                    trash_time,
                    archive_object_hash: archived.object_hash,
                    archive_object_size,
                    archived_at: nonnegative_u64(archived.archived_at, "archive timestamp")?,
                    conversation_count: nonnegative_u64(
                        archived.conversation_count,
                        "archived conversation count",
                    )?,
                    message_count: nonnegative_u64(
                        archived.message_count,
                        "archived message count",
                    )?,
                    asset_hashes: archived.asset_hashes,
                    owner_heads: logical_owner_heads(&owner_heads),
                })
                .map_err(codec_error)?
                .bytes);
            }
            let mut detail: Value = serde_json::from_str(&raw)?;
            let mut owner_heads = resolve_owner_heads_with_sizes(
                connection,
                cas,
                pds_generation,
                &detail,
                Some(&character_id),
                size,
            )?;
            for head in &owner_heads {
                if let Some(bytes) = &head.derived_manifest {
                    derived(bytes)?;
                }
            }
            strip_character_owner_property(&mut detail, &character_id, &mut owner_heads)?;
            LogicalRecordEnvelope::Character {
                configured_index: nonnegative_u64(configured_index, "character configured index")?,
                detail,
                owner_heads: logical_owner_heads(&owner_heads),
            }
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => {
            let (configured_index, recent_at, raw): (i64, i64, String) = connection
                .query_row(
                    "SELECT configured_index, recent_at, detail FROM conversations
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![pds_generation, character_id, conversation_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("conversation"))?;
            LogicalRecordEnvelope::Conversation {
                configured_index: nonnegative_u64(
                    configured_index,
                    "conversation configured index",
                )?,
                recent_at,
                detail: serde_json::from_str(&raw)?,
                message_page_hashes,
            }
        }
        LogicalRecordLocator::Asset { logical_key } => {
            reconstruct_asset_alias(connection, pds_generation, "asset", &logical_key)?
        }
        LogicalRecordLocator::Inlay { logical_key } => {
            reconstruct_asset_alias(connection, pds_generation, "inlay", &logical_key)?
        }
        // The store no longer holds cold payloads; the shared record format still carries the variant.
        LogicalRecordLocator::Cold { .. } => return Err(missing_source("cold")),
    };
    Ok(encode_logical_record(&envelope).map_err(codec_error)?.bytes)
}

pub(super) fn reconstruct_asset_alias(
    connection: &Connection,
    pds_generation: &str,
    kind: &str,
    logical_key: &str,
) -> StoreResult<LogicalRecordEnvelope> {
    let (object_hash, size, mime, name, ext, inlay_type, width, height, metadata): (
        Option<String>,
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        String,
    ) = connection
        .query_row(
            "SELECT object_hash, size, mime, name, ext, inlay_type, width, height, metadata
             FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
            params![pds_generation, kind, logical_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| missing_source(kind))?;
    let size = nonnegative_u64(size, "asset alias size")?;
    let metadata = encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
        mime,
        name,
        ext,
        inlay_type,
        width,
        height,
        metadata: serde_json::from_str(&metadata)?,
    })
    .map_err(codec_error)?;
    Ok(if kind == "asset" {
        LogicalRecordEnvelope::Asset {
            object_hash,
            size,
            metadata,
        }
    } else {
        LogicalRecordEnvelope::Inlay {
            object_hash,
            size,
            metadata,
        }
    })
}

pub(super) fn required_text(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
    message: &str,
) -> StoreResult<String> {
    connection
        .query_row(sql, parameters, |row| row.get(0))
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: message.to_owned(),
        })
}

pub(super) fn parse_owner_index(value: &str) -> StoreResult<u64> {
    let parsed = value.parse::<u64>().map_err(|_| StoreError::Validation {
        message: "asset owner locator is not a nonnegative integer".to_owned(),
    })?;
    if parsed.to_string() != value {
        return validation("asset owner locator is not canonical");
    }
    Ok(parsed)
}

pub(super) fn verify_object_bytes(bytes: &[u8], hash: &str, size: u64) -> StoreResult<()> {
    if bytes.len() as u64 != size || hex::encode(Sha256::digest(bytes)) != hash {
        return validation("reconstructed logical object failed hash or size verification");
    }
    Ok(())
}

pub(super) fn nonnegative_u64(value: i64, description: &str) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::Validation {
        message: format!("{description} must be nonnegative"),
    })
}

pub(super) fn codec_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Validation {
        message: error.to_string(),
    }
}

pub(super) fn missing_source(kind: &str) -> StoreError {
    StoreError::Validation {
        message: format!("{kind} logical record source is missing"),
    }
}

pub(super) fn validation<T>(message: impl Into<String>) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.into(),
    })
}
