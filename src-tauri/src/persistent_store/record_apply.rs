//! Validated application row application, independent of peer staging/journals.
use super::StoreError;
use crate::logical_records::{
    decode_asset_alias_metadata, encode_logical_record_key, LogicalOwnerHead, LogicalOwnerLocator,
    LogicalRecordEnvelope, LogicalRecordLocator,
};
use rusqlite::{params, Transaction};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
pub(super) struct ResolvedOwnerHead {
    pub(super) head: LogicalOwnerHead,
    pub(super) tuples: Option<Vec<Value>>,
}

pub(super) fn validate_locator_envelope(
    locator: &LogicalRecordLocator,
    envelope: &LogicalRecordEnvelope,
) -> Result<(), StoreError> {
    let kind_matches = matches!(
        (locator, envelope),
        (
            LogicalRecordLocator::Root,
            LogicalRecordEnvelope::Root { .. }
        ) | (
            LogicalRecordLocator::Preset { .. },
            LogicalRecordEnvelope::Preset { .. }
        ) | (
            LogicalRecordLocator::Plugin { .. },
            LogicalRecordEnvelope::Plugin { .. }
        ) | (
            LogicalRecordLocator::Character { .. },
            LogicalRecordEnvelope::Character { .. }
        ) | (
            LogicalRecordLocator::Conversation { .. },
            LogicalRecordEnvelope::Conversation { .. }
        ) | (
            LogicalRecordLocator::Asset { .. },
            LogicalRecordEnvelope::Asset { .. }
        ) | (
            LogicalRecordLocator::Inlay { .. },
            LogicalRecordEnvelope::Inlay { .. }
        ) | (
            LogicalRecordLocator::Cold { .. },
            LogicalRecordEnvelope::Cold { .. }
        )
    );
    if !kind_matches {
        return validation("logical record kind differs from its encoded key");
    }
    match (locator, envelope) {
        (LogicalRecordLocator::Root, LogicalRecordEnvelope::Root { value, .. }) => {
            let root = json_object(value, "logical root")?;
            if ["characters", "botPresets", "pluginCustomStorage"]
                .iter()
                .any(|key| root.contains_key(*key))
            {
                return validation("logical root contains a separated PDS record family");
            }
        }
        (
            LogicalRecordLocator::Character { character_id },
            LogicalRecordEnvelope::Character { detail, .. },
        ) => {
            let detail = json_object(detail, "logical character detail")?;
            if required_string(detail, "chaId", "logical character detail")? != character_id {
                return validation("logical character ID differs from its encoded key");
            }
            required_display_name(detail, "logical character detail")?;
            if detail.contains_key("chats") {
                return validation("logical character detail contains separated conversations");
            }
        }
        (
            LogicalRecordLocator::Conversation {
                conversation_id, ..
            },
            LogicalRecordEnvelope::Conversation { detail, .. },
        ) => {
            let detail = json_object(detail, "logical conversation detail")?;
            if required_string(detail, "id", "logical conversation detail")? != conversation_id {
                return validation("logical conversation ID differs from its encoded key");
            }
            required_display_name(detail, "logical conversation detail")?;
            if detail.contains_key("message") {
                return validation("logical conversation detail contains separated messages");
            }
        }
        _ => {}
    }
    Ok(())
}

fn rehydrate_owner_property(
    parent: &mut Map<String, Value>,
    property: &str,
    head: &ResolvedOwnerHead,
    allow_retained_property: bool,
) -> Result<(), StoreError> {
    if head.head.present {
        let tuples = head.tuples.clone().ok_or_else(|| {
            record_validation("present logical owner head has no decoded manifest".to_owned())
        })?;
        let property_index = head.head.property_index.ok_or_else(|| {
            record_validation("present logical owner head has no property index".to_owned())
        })?;
        if property_index > parent.len() as u64 {
            return validation("logical owner property index exceeds its parent object");
        }
        let property_index = usize::try_from(property_index).map_err(|_| {
            record_validation("logical owner property index exceeds the platform range".to_owned())
        })?;
        if let Some(retained) = parent.get(property) {
            if !allow_retained_property {
                return validation(
                    "logical owner property was not stripped from its parent envelope",
                );
            }
            if parent.keys().position(|key| key == property) != Some(property_index) {
                return validation("retained logical owner property index differs from its head");
            }
            let retained = retained.as_array().ok_or_else(|| {
                record_validation(
                    "retained logical owner property must be a tuple array".to_owned(),
                )
            })?;
            if retained.len() != tuples.len() {
                return validation("retained logical owner tuple count differs from its manifest");
            }
            let mut has_trailing_fields = false;
            for (retained, expected) in retained.iter().zip(&tuples) {
                let (Some(retained), Some(expected)) = (retained.as_array(), expected.as_array())
                else {
                    return validation("retained logical owner tuple differs from its manifest");
                };
                if retained.len() < 3 || expected.len() != 3 || retained[..3] != expected[..] {
                    return validation("retained logical owner tuple differs from its manifest");
                }
                has_trailing_fields |= retained.len() > 3;
            }
            if !has_trailing_fields {
                return validation(
                    "exact-three logical owner property must use canonical stripping",
                );
            }
            return Ok(());
        }
        parent.shift_insert(property_index, property.to_owned(), Value::Array(tuples));
    } else {
        if parent.contains_key(property) {
            return validation("absent logical owner property was retained in its parent envelope");
        }
        if head.tuples.is_some() || head.head.property_index.is_some() {
            return validation("absent logical owner head contains manifest reconstruction data");
        }
    }
    Ok(())
}

pub(super) fn rehydrate_root_owners(
    value: &mut Value,
    heads: &[ResolvedOwnerHead],
) -> Result<(), StoreError> {
    let root = json_object_mut(value, "logical root")?;
    let mut by_identity = BTreeMap::new();
    for head in heads {
        let identity = match &head.head.owner {
            LogicalOwnerLocator::RootModule { index } => format!("module:{index}"),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => format!("persona:{index}"),
            LogicalOwnerLocator::CharacterAdditional { .. } => {
                return validation("logical root contains a character owner head")
            }
        };
        if by_identity.insert(identity, head).is_some() {
            return validation("logical root contains duplicate owner heads");
        }
    }
    let mut expected = 0_usize;
    if let Some(modules) = root.get_mut("modules") {
        let modules = modules
            .as_array_mut()
            .ok_or_else(|| record_validation("logical root modules must be an array".to_owned()))?;
        for (index, module) in modules.iter_mut().enumerate() {
            let module = json_object_mut(module, "logical root module")?;
            let head = by_identity.get(&format!("module:{index}")).ok_or_else(|| {
                record_validation(
                    "logical root module owner head coverage is incomplete".to_owned(),
                )
            })?;
            rehydrate_owner_property(module, "assets", head, true)?;
            expected += 1;
        }
    }
    if let Some(personas) = root.get_mut("personas") {
        let personas = personas.as_array_mut().ok_or_else(|| {
            record_validation("logical root personas must be an array".to_owned())
        })?;
        for (index, persona) in personas.iter_mut().enumerate() {
            let persona = json_object_mut(persona, "logical root persona")?;
            let Some(embedded) = persona.get_mut("embeddedModule") else {
                continue;
            };
            let embedded = json_object_mut(embedded, "logical persona embedded module")?;
            let head = by_identity
                .get(&format!("persona:{index}"))
                .ok_or_else(|| {
                    record_validation(
                        "logical persona owner head coverage is incomplete".to_owned(),
                    )
                })?;
            rehydrate_owner_property(embedded, "assets", head, true)?;
            expected += 1;
        }
    }
    if by_identity.len() != expected {
        return validation("logical root contains owner heads for missing occurrences");
    }
    Ok(())
}

pub(super) fn rehydrate_character_owner(
    detail: &mut Value,
    character_id: &str,
    heads: &[ResolvedOwnerHead],
) -> Result<(), StoreError> {
    let [head] = heads else {
        return validation("logical character owner head coverage must contain exactly one head");
    };
    if !matches!(
        &head.head.owner,
        LogicalOwnerLocator::CharacterAdditional { character_id: owner_id }
            if owner_id == character_id
    ) {
        return validation("logical character owner head differs from its encoded key");
    }
    let detail = json_object_mut(detail, "logical character detail")?;
    rehydrate_owner_property(detail, "additionalAssets", head, false)
}

pub(super) fn validate_configured_index_uniqueness(
    transaction: &Transaction<'_>,
    generation: &str,
) -> Result<(), StoreError> {
    for (query, family) in [
        (
            "SELECT EXISTS(
                SELECT 1 FROM bot_presets WHERE generation = ?1
                GROUP BY configured_index HAVING COUNT(*) > 1
             )",
            "preset",
        ),
        (
            "SELECT EXISTS(
                SELECT 1 FROM characters WHERE generation = ?1
                GROUP BY configured_index HAVING COUNT(*) > 1
             )",
            "character",
        ),
        (
            "SELECT EXISTS(
                SELECT 1 FROM conversations WHERE generation = ?1
                GROUP BY character_id, configured_index HAVING COUNT(*) > 1
             )",
            "conversation",
        ),
    ] {
        let duplicate: bool = transaction
            .query_row(query, [generation], |row| row.get(0))
            .map_err(sql_error)?;
        if duplicate {
            return validation(format!(
                "logical delta creates duplicate {family} configured indices"
            ));
        }
    }
    Ok(())
}

pub(super) fn apply_delete(
    transaction: &Transaction<'_>,
    generation: &str,
    locator: &LogicalRecordLocator,
) -> Result<(), StoreError> {
    match locator {
        LogicalRecordLocator::Root => validation("logical delta cannot delete the root record"),
        LogicalRecordLocator::Preset { preset_id } => {
            transaction
                .execute(
                    "DELETE FROM bot_presets WHERE generation = ?1 AND preset_id = ?2",
                    params![generation, preset_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Plugin { storage_key } => {
            transaction
                .execute(
                    "DELETE FROM plugin_storage WHERE generation = ?1 AND storage_key = ?2",
                    params![generation, storage_key],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Character { character_id } => {
            transaction
                .execute(
                    "DELETE FROM messages WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM conversations WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
                       AND owner_locator = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM characters WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => {
            transaction
                .execute(
                    "DELETE FROM messages
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM conversations
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        LogicalRecordLocator::Asset { logical_key } => {
            delete_alias(transaction, generation, "asset", logical_key)
        }
        LogicalRecordLocator::Inlay { logical_key } => {
            delete_alias(transaction, generation, "inlay", logical_key)
        }
        LogicalRecordLocator::Cold { logical_key } => {
            transaction
                .execute(
                    "DELETE FROM cold_aliases WHERE generation = ?1 AND key = ?2",
                    params![generation, logical_key],
                )
                .map_err(sql_error)?;
            Ok(())
        }
    }
}

fn delete_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    kind: &str,
    logical_key: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
            params![generation, kind, logical_key],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(super) fn apply_materialized_record(
    transaction: &Transaction<'_>,
    generation: &str,
    locator: LogicalRecordLocator,
    envelope: LogicalRecordEnvelope,
    messages: Option<&[Value]>,
) -> Result<(), StoreError> {
    let key = encode_logical_record_key(&locator).map_err(|e| record_validation(e.to_string()))?;
    apply_record_rows(
        transaction,
        generation,
        &key,
        &locator,
        &envelope,
        messages.map_or(0, |m| m.len() as u64),
    )?;
    if let (
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        },
        Some(messages),
    ) = (&locator, messages)
    {
        let mut statement=transaction.prepare_cached("INSERT INTO messages(generation,character_id,conversation_id,message_index,message_id,value) VALUES(?1,?2,?3,?4,?5,?6)").map_err(sql_error)?;
        for (index, message) in messages.iter().enumerate() {
            statement
                .execute(params![
                    generation,
                    character_id,
                    conversation_id,
                    index as i64,
                    message.get("chatId").and_then(Value::as_str),
                    serde_json::to_string(message).map_err(json_error)?
                ])
                .map_err(sql_error)?;
        }
    }
    Ok(())
}

pub(super) fn apply_record_rows(
    transaction: &Transaction<'_>,
    generation: &str,
    key: &str,
    locator: &LogicalRecordLocator,
    envelope: &LogicalRecordEnvelope,
    message_count: u64,
) -> Result<(), StoreError> {
    match (locator, envelope) {
        (LogicalRecordLocator::Root, LogicalRecordEnvelope::Root { value, owner_heads }) => {
            transaction
                .execute(
                    "INSERT INTO root (generation, value) VALUES (?1, ?2)
                     ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
                    params![
                        generation,
                        serde_json::to_string(value).map_err(json_error)?
                    ],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind IN (
                        'root-module-assets', 'persona-embedded-module-assets'
                     )",
                    [generation],
                )
                .map_err(sql_error)?;
            insert_owner_heads(transaction, generation, owner_heads)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Preset { preset_id },
            LogicalRecordEnvelope::Preset {
                configured_index,
                value,
            },
        ) => {
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let image = value.get("image").and_then(Value::as_str);
            transaction
                .execute(
                    "INSERT INTO bot_presets (
                        generation, preset_id, configured_index, name, image, value
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(generation, preset_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        name = excluded.name,
                        image = excluded.image,
                        value = excluded.value",
                    params![
                        generation,
                        preset_id,
                        sqlite_i64(*configured_index, "preset configured index")?,
                        name,
                        image,
                        serde_json::to_string(value).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Plugin { storage_key },
            LogicalRecordEnvelope::Plugin { ordinal, value },
        ) => {
            let serialized = serde_json::to_string(value).map_err(json_error)?;
            transaction
                .execute(
                    "INSERT INTO plugin_storage (
                        generation, storage_key, byte_size, ordinal, value
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(generation, storage_key) DO UPDATE SET
                        byte_size = excluded.byte_size,
                        ordinal = excluded.ordinal,
                        value = excluded.value",
                    params![
                        generation,
                        storage_key,
                        sqlite_i64(serialized.len() as u64, "plugin storage byte size")?,
                        sqlite_i64(*ordinal, "plugin storage ordinal")?,
                        serialized,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Character { character_id },
            LogicalRecordEnvelope::Character {
                configured_index,
                detail,
                owner_heads,
            },
        ) => {
            let object = json_object(detail, "logical character detail")?;
            let name = required_display_name(object, "logical character detail")?;
            let conversation_count: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM conversations
                     WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            let recent_at = object
                .get("lastInteraction")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let trash_time = object.get("trashTime").and_then(Value::as_i64);
            transaction
                .execute(
                    "INSERT INTO characters (
                        generation, character_id, configured_index, recent_at, trashed,
                        name, image, conversation_count, type, creator_notes, trash_time, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(generation, character_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        recent_at = excluded.recent_at,
                        trashed = excluded.trashed,
                        name = excluded.name,
                        image = excluded.image,
                        conversation_count = excluded.conversation_count,
                        type = excluded.type,
                        creator_notes = excluded.creator_notes,
                        trash_time = excluded.trash_time,
                        detail = excluded.detail",
                    params![
                        generation,
                        character_id,
                        sqlite_i64(*configured_index, "character configured index")?,
                        recent_at,
                        trash_time.is_some(),
                        name,
                        object.get("image").and_then(Value::as_str),
                        conversation_count,
                        object
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("character"),
                        object.get("creatorNotes").and_then(Value::as_str),
                        trash_time,
                        serde_json::to_string(detail).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "DELETE FROM asset_owner_heads
                     WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
                       AND owner_locator = ?2",
                    params![generation, character_id],
                )
                .map_err(sql_error)?;
            insert_owner_heads(transaction, generation, owner_heads)
        }
        (
            LogicalRecordLocator::Conversation {
                character_id,
                conversation_id,
            },
            LogicalRecordEnvelope::Conversation {
                configured_index,
                recent_at,
                detail,
                ..
            },
        ) => {
            let parent_exists: bool = transaction
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM characters
                        WHERE generation = ?1 AND character_id = ?2
                     )",
                    params![generation, character_id],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if !parent_exists {
                return validation("logical conversation parent character is absent");
            }
            let object = json_object(detail, "logical conversation detail")?;
            let name = required_display_name(object, "logical conversation detail")?;
            transaction
                .execute(
                    "DELETE FROM messages
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                )
                .map_err(sql_error)?;
            transaction
                .execute(
                    "INSERT INTO conversations (
                        generation, character_id, conversation_id, configured_index,
                        recent_at, name, message_count, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(generation, character_id, conversation_id) DO UPDATE SET
                        configured_index = excluded.configured_index,
                        recent_at = excluded.recent_at,
                        name = excluded.name,
                        message_count = excluded.message_count,
                        detail = excluded.detail",
                    params![
                        generation,
                        character_id,
                        conversation_id,
                        sqlite_i64(*configured_index, "conversation configured index")?,
                        recent_at,
                        name,
                        sqlite_i64(message_count, "conversation message count")?,
                        serde_json::to_string(detail).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        (
            LogicalRecordLocator::Asset { logical_key },
            LogicalRecordEnvelope::Asset {
                object_hash,
                size,
                metadata,
            },
        ) => put_alias(
            transaction,
            generation,
            "asset",
            logical_key,
            object_hash.as_deref(),
            *size,
            metadata,
        ),
        (
            LogicalRecordLocator::Inlay { logical_key },
            LogicalRecordEnvelope::Inlay {
                object_hash,
                size,
                metadata,
            },
        ) => put_alias(
            transaction,
            generation,
            "inlay",
            logical_key,
            object_hash.as_deref(),
            *size,
            metadata,
        ),
        (
            LogicalRecordLocator::Cold { logical_key },
            LogicalRecordEnvelope::Cold {
                object_hash,
                size,
                metadata,
            },
        ) => {
            transaction
                .execute(
                    "INSERT INTO cold_aliases (
                        generation, key, object_hash, size, metadata
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(generation, key) DO UPDATE SET
                        object_hash = excluded.object_hash,
                        size = excluded.size,
                        metadata = excluded.metadata",
                    params![
                        generation,
                        logical_key,
                        object_hash,
                        sqlite_i64(*size, "cold alias size")?,
                        serde_json::to_string(metadata).map_err(json_error)?,
                    ],
                )
                .map_err(sql_error)?;
            Ok(())
        }
        _ => validation(format!(
            "logical prepared record {} changed kind before apply",
            key
        )),
    }
}

fn put_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    kind: &str,
    logical_key: &str,
    object_hash: Option<&str>,
    size: u64,
    metadata: &Value,
) -> Result<(), StoreError> {
    let typed = decode_asset_alias_metadata(metadata)
        .map_err(|error| record_validation(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(generation, kind, logical_key) DO UPDATE SET
                object_hash = excluded.object_hash,
                size = excluded.size,
                mime = excluded.mime,
                name = excluded.name,
                ext = excluded.ext,
                inlay_type = excluded.inlay_type,
                width = excluded.width,
                height = excluded.height,
                metadata = excluded.metadata",
            params![
                generation,
                logical_key,
                object_hash,
                kind,
                sqlite_i64(size, "asset alias size")?,
                typed.mime,
                typed.name,
                typed.ext,
                typed.inlay_type,
                typed.width,
                typed.height,
                serde_json::to_string(&typed.metadata).map_err(json_error)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn insert_owner_heads(
    transaction: &Transaction<'_>,
    generation: &str,
    heads: &[LogicalOwnerHead],
) -> Result<(), StoreError> {
    for head in heads {
        let (kind, locator) = match &head.owner {
            LogicalOwnerLocator::CharacterAdditional { character_id } => {
                ("character-additional-assets", character_id.clone())
            }
            LogicalOwnerLocator::RootModule { index } => ("root-module-assets", index.to_string()),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => {
                ("persona-embedded-module-assets", index.to_string())
            }
        };
        transaction
            .execute(
                "INSERT INTO asset_owner_heads (
                    generation, owner_kind, owner_locator, present, manifest_hash, entry_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    generation,
                    kind,
                    locator,
                    head.present,
                    head.manifest_hash,
                    sqlite_i64(head.entry_count, "owner entry count")?,
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn sqlite_i64(value: u64, context: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| record_validation(format!("{context} exceeds SQLite range")))
}

fn json_object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, StoreError> {
    value
        .as_object()
        .ok_or_else(|| record_validation(format!("{context} must be an object")))
}

fn json_object_mut<'a>(
    value: &'a mut Value,
    context: &str,
) -> Result<&'a mut Map<String, Value>, StoreError> {
    value
        .as_object_mut()
        .ok_or_else(|| record_validation(format!("{context} must be an object")))
}

// Match local commits: display names may be empty, but must still be strings.
fn required_display_name<'a>(
    object: &'a Map<String, Value>,
    context: &str,
) -> Result<&'a str, StoreError> {
    object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| record_validation(format!("{context} requires name")))
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<&'a str, StoreError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| record_validation(format!("{context} requires {key}")))
}

fn record_validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
fn validation<T>(message: impl Into<String>) -> Result<T, StoreError> {
    Err(record_validation(message))
}
fn sql_error(error: rusqlite::Error) -> StoreError {
    error.into()
}
fn json_error(error: serde_json::Error) -> StoreError {
    error.into()
}
