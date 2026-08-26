use super::{
    active_generation, current_revision, AssetAlias, ConversationMutation, PluginStorageMutation,
    RevisionResult, StagingResult, StoreError, StoreResult, WorkingSetCommit, GENERATION_TABLES,
};

pub(super) fn commit_asset_alias(
    connection: &mut Connection,
    alias: &AssetAlias,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    alias.validate()?;
    let active = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    let generation = writable_generation(&transaction, &active, revision)?;
    put_asset_alias(&transaction, &generation, alias)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
}
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};

pub(super) fn commit(
    connection: &mut Connection,
    input: &WorkingSetCommit,
) -> StoreResult<RevisionResult> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != input.expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: input.expected_revision,
            actual: actual_revision,
        });
    }

    if let Some(character) = &input.replace_character {
        validate_character(character, "Selected character replacement")?;
    }
    if let Some(character) = &input.add_character {
        validate_character(character, "Character addition")?;
    }

    let active = active_generation(&transaction)?;
    if let Some(details) = &input.character_details {
        validate_character_details(
            &transaction,
            &active,
            details,
            input.delete_character_id.as_deref(),
        )?;
    }
    let revision = actual_revision + 1;
    let generation = writable_generation(&transaction, &active, revision)?;
    if let Some(root) = &input.root {
        put_root(&transaction, &generation, root)?;
    }
    if let Some(presets) = &input.replace_presets {
        replace_presets(&transaction, &generation, presets)?;
    }
    if let Some(character_id) = &input.delete_character_id {
        delete_character(&transaction, &generation, character_id)?;
    }
    if let Some(character) = &input.character {
        put_character_detail(&transaction, &generation, character)?;
    }
    for detail in input.character_details.as_deref().unwrap_or_default() {
        put_character_detail(&transaction, &generation, detail)?;
    }
    if let Some(character) = &input.replace_character {
        replace_character(&transaction, &generation, character)?;
    }
    if let Some(character) = &input.add_character {
        let character_id = required_string(character, "chaId", "Character addition")?;
        if character_exists(&transaction, &generation, character_id)? {
            return Err(validation(format!(
                "Character {character_id} already exists"
            )));
        }
        replace_character(&transaction, &generation, character)?;
    }
    for mutation in input.conversations.as_deref().unwrap_or_default() {
        apply_conversation_mutation(&transaction, &generation, mutation)?;
    }
    for mutation in input.plugin_storage.as_deref().unwrap_or_default() {
        apply_plugin_storage_mutation(&transaction, &generation, mutation)?;
    }

    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
}

pub(super) fn replace_begin(connection: &mut Connection) -> StoreResult<StagingResult> {
    let staging_id = format!("staging-{}", uuid::Uuid::new_v4());
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    put_root(&transaction, &staging_id, &Value::Object(Map::new()))?;
    transaction.commit()?;
    Ok(StagingResult { staging_id })
}

pub(super) fn replace_put_root(
    connection: &mut Connection,
    staging_id: &str,
    root: &Value,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let mut staged_root = object(root, "Persistent root")?.clone();
    if let Some(plugin_storage) = staged_root.remove("pluginCustomStorage") {
        let plugin_storage = plugin_storage
            .as_object()
            .ok_or_else(|| validation("pluginCustomStorage must be a JSON object"))?;
        replace_plugin_storage(&transaction, staging_id, plugin_storage)?;
    }
    put_root(&transaction, staging_id, &Value::Object(staged_root))?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_add_characters(
    connection: &mut Connection,
    staging_id: &str,
    characters: &[Value],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    for character in characters {
        validate_character(character, "Persistent data import")?;
        let character_id = required_string(character, "chaId", "Persistent data import")?;
        if character_exists(&transaction, staging_id, character_id)? {
            return Err(validation(
                "Persistent data import requires unique character IDs",
            ));
        }
        let configured_index: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM characters WHERE generation = ?1",
            [staging_id],
            |row| row.get(0),
        )?;
        put_full_character(&transaction, staging_id, character, configured_index)?;
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_put_presets(
    connection: &mut Connection,
    staging_id: &str,
    presets: &[Value],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    replace_presets(&transaction, staging_id, presets)?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_put_asset_aliases(
    connection: &mut Connection,
    staging_id: &str,
    aliases: &[AssetAlias],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    for alias in aliases {
        alias.validate()?;
    }
    for alias in aliases {
        put_asset_alias(&transaction, staging_id, alias)?;
    }
    transaction.commit()?;
    Ok(())
}

fn put_asset_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    alias: &AssetAlias,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO asset_aliases (
            generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(generation, logical_key) DO UPDATE SET
            object_hash = excluded.object_hash,
            kind = excluded.kind,
            size = excluded.size,
            mime = excluded.mime,
            name = excluded.name,
            ext = excluded.ext,
            inlay_type = excluded.inlay_type,
            width = excluded.width,
            height = excluded.height",
        params![
            generation,
            alias.key,
            alias.object_hash,
            alias.kind,
            alias.size,
            alias.mime,
            alias.name,
            alias.ext,
            alias.inlay_type,
            alias.width,
            alias.height,
        ],
    )?;
    Ok(())
}

pub(super) fn replace_commit(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<RevisionResult> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let actual_revision = current_revision(&transaction)?;
    if let Some(expected) = expected_revision {
        if expected != actual_revision {
            return Err(StoreError::RevisionConflict {
                expected,
                actual: actual_revision,
            });
        }
    }

    let active = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    let generation = format!("revision-{revision}");
    if !generation_is_leased(&transaction, &active)? {
        delete_generation(&transaction, &active)?;
    }
    move_generation(&transaction, staging_id, &generation)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
}

pub(super) fn validate_replace_commit(
    connection: &Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<i64> {
    require_staging(connection, staging_id)?;
    let actual_revision = current_revision(connection)?;
    if let Some(expected) = expected_revision {
        if expected != actual_revision {
            return Err(StoreError::RevisionConflict {
                expected,
                actual: actual_revision,
            });
        }
    }
    Ok(actual_revision)
}

pub(super) fn replace_abort(connection: &mut Connection, staging_id: &str) -> StoreResult<()> {
    if !staging_id.starts_with("staging-") {
        return Err(validation("Invalid staging generation"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    delete_generation(&transaction, staging_id)?;
    transaction.commit()?;
    Ok(())
}

fn put_root(transaction: &Transaction<'_>, generation: &str, root: &Value) -> StoreResult<()> {
    let mut root = object(root, "Persistent root")?.clone();
    root.remove("characters");
    root.remove("botPresets");
    root.remove("pluginCustomStorage");
    transaction.execute(
        "INSERT INTO root (generation, value) VALUES (?1, ?2) ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
        params![generation, serde_json::to_string(&root)?],
    )?;
    Ok(())
}

fn replace_plugin_storage(
    transaction: &Transaction<'_>,
    generation: &str,
    values: &Map<String, Value>,
) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM plugin_storage WHERE generation = ?1",
        [generation],
    )?;
    for (ordinal, (key, value)) in values.iter().enumerate() {
        put_plugin_storage(transaction, generation, key, value, Some(ordinal as i64))?;
    }
    Ok(())
}

fn put_plugin_storage(
    transaction: &Transaction<'_>,
    generation: &str,
    key: &str,
    value: &Value,
    ordinal: Option<i64>,
) -> StoreResult<()> {
    let serialized = serde_json::to_string(value)?;
    transaction.execute(
        "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
         VALUES (
             ?1,
             ?2,
             ?3,
             COALESCE(
                 ?4,
                 (SELECT COALESCE(MAX(ordinal) + 1, 0)
                  FROM plugin_storage WHERE generation = ?1)
             ),
             ?5
         )
         ON CONFLICT(generation, storage_key) DO UPDATE SET
             byte_size = excluded.byte_size,
             value = excluded.value",
        params![
            generation,
            key,
            serialized.len() as i64,
            ordinal,
            serialized
        ],
    )?;
    Ok(())
}

fn apply_plugin_storage_mutation(
    transaction: &Transaction<'_>,
    generation: &str,
    mutation: &PluginStorageMutation,
) -> StoreResult<()> {
    match mutation {
        PluginStorageMutation::Set { key, value } => {
            put_plugin_storage(transaction, generation, key, value, None)
        }
        PluginStorageMutation::Delete { key } => {
            transaction.execute(
                "DELETE FROM plugin_storage WHERE generation = ?1 AND storage_key = ?2",
                params![generation, key],
            )?;
            Ok(())
        }
        PluginStorageMutation::Clear => {
            transaction.execute(
                "DELETE FROM plugin_storage WHERE generation = ?1",
                [generation],
            )?;
            Ok(())
        }
    }
}

fn replace_presets(
    transaction: &Transaction<'_>,
    generation: &str,
    presets: &[Value],
) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM bot_presets WHERE generation = ?1",
        [generation],
    )?;
    let mut statement = transaction.prepare_cached(
        "INSERT INTO bot_presets (generation, preset_id, configured_index, name, image, value)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (configured_index, preset) in presets.iter().enumerate() {
        statement.execute(params![
            generation,
            configured_index.to_string(),
            configured_index as i64,
            preset
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            preset.get("image").and_then(Value::as_str),
            serde_json::to_string(preset)?,
        ])?;
    }
    Ok(())
}

fn require_staging(connection: &Connection, staging_id: &str) -> StoreResult<()> {
    if !staging_id.starts_with("staging-") {
        return Err(validation("Invalid staging generation"));
    }
    let exists = connection
        .query_row(
            "SELECT 1 FROM root WHERE generation = ?1",
            [staging_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Err(validation("Staging generation does not exist"));
    }
    Ok(())
}

fn validate_character(character: &Value, context: &str) -> StoreResult<()> {
    let character = object(character, context)?;
    let character_id = character
        .get("chaId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if character_id.is_empty() {
        return Err(validation(format!(
            "{context} requires a nonempty character ID"
        )));
    }

    let mut conversation_ids = std::collections::HashSet::new();
    for conversation in character
        .get("chats")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = conversation
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.is_empty() || !conversation_ids.insert(id) {
            return Err(validation(format!(
                "{context} requires unique, nonempty chat IDs"
            )));
        }
    }
    Ok(())
}

fn validate_character_details(
    transaction: &Transaction<'_>,
    generation: &str,
    details: &[Value],
    delete_character_id: Option<&str>,
) -> StoreResult<()> {
    let mut character_ids = std::collections::HashSet::new();
    for detail in details {
        let character_id = required_string(detail, "chaId", "Batch character detail mutation")?;
        if Some(character_id) == delete_character_id || !character_ids.insert(character_id) {
            return Err(validation(
                "Batch character detail mutation requires unique retained character IDs",
            ));
        }
        if !character_exists(transaction, generation, character_id)? {
            return Err(validation(format!(
                "Character {character_id} does not exist"
            )));
        }
    }
    Ok(())
}

fn put_character_detail(
    transaction: &Transaction<'_>,
    generation: &str,
    detail: &Value,
) -> StoreResult<()> {
    let character_id = required_string(detail, "chaId", "Character update")?;
    let configured_index = transaction
        .query_row(
            "SELECT configured_index FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(transaction.query_row(
            "SELECT COUNT(*) FROM characters WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )?);
    let conversation_count: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM conversations WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
        |row| row.get(0),
    )?;
    put_character_records(
        transaction,
        generation,
        detail,
        configured_index,
        conversation_count,
    )
}

fn replace_character(
    transaction: &Transaction<'_>,
    generation: &str,
    character: &Value,
) -> StoreResult<()> {
    let character_id = required_string(character, "chaId", "Character replacement")?;
    let configured_index = transaction
        .query_row(
            "SELECT configured_index FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(transaction.query_row(
            "SELECT COALESCE(MAX(configured_index) + 1, 0) FROM characters WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )?);

    delete_character_contents(transaction, generation, character_id)?;
    put_full_character(transaction, generation, character, configured_index)
}

fn put_full_character(
    transaction: &Transaction<'_>,
    generation: &str,
    character: &Value,
    configured_index: i64,
) -> StoreResult<()> {
    let character_id = required_string(character, "chaId", "Character")?;
    let chats = character
        .get("chats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let detail = without_field(character, "chats")?;
    put_character_records(
        transaction,
        generation,
        &detail,
        configured_index,
        chats.len() as i64,
    )?;
    for (index, conversation) in chats.iter().enumerate() {
        put_conversation(
            transaction,
            generation,
            character_id,
            conversation,
            index as i64,
        )?;
    }
    Ok(())
}

fn put_character_records(
    transaction: &Transaction<'_>,
    generation: &str,
    detail: &Value,
    configured_index: i64,
    conversation_count: i64,
) -> StoreResult<()> {
    let object = object(detail, "Character detail")?;
    let character_id = required_string(detail, "chaId", "Character detail")?;
    let name = required_string(detail, "name", "Character detail")?;
    let image = object.get("image").and_then(Value::as_str);
    let recent_at = object
        .get("lastInteraction")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let trashed = object.contains_key("trashTime");
    let character_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("character");
    let creator_notes = object.get("creatorNotes").and_then(Value::as_str);
    let trash_time = object.get("trashTime").and_then(Value::as_i64);
    transaction.execute(
        "INSERT INTO characters (
            generation, character_id, configured_index, recent_at, trashed, name, image,
            conversation_count, type, creator_notes, trash_time, detail
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
            configured_index,
            recent_at,
            trashed,
            name,
            image,
            conversation_count,
            character_type,
            creator_notes,
            trash_time,
            serde_json::to_string(detail)?,
        ],
    )?;
    Ok(())
}

fn put_conversation(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
    conversation: &Value,
    configured_index: i64,
) -> StoreResult<()> {
    let object = object(conversation, "Conversation")?;
    let conversation_id = required_string(conversation, "id", "Conversation")?;
    let name = required_string(conversation, "name", "Conversation")?;
    let messages = object
        .get("message")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let recent_at = object
        .get("lastDate")
        .and_then(Value::as_i64)
        .or_else(|| {
            messages
                .last()
                .and_then(|message| message.get("time"))
                .and_then(Value::as_i64)
        })
        .unwrap_or_default();
    let detail = without_field(conversation, "message")?;
    put_conversation_record(
        transaction,
        generation,
        character_id,
        conversation_id,
        configured_index,
        recent_at,
        name,
        messages.len() as i64,
        &detail,
    )?;
    insert_messages(
        transaction,
        generation,
        character_id,
        conversation_id,
        0,
        &messages,
    )
}

#[allow(clippy::too_many_arguments)]
fn put_conversation_record(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    configured_index: i64,
    recent_at: i64,
    name: &str,
    message_count: i64,
    detail: &Value,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO conversations (
            generation, character_id, conversation_id, configured_index, recent_at,
            name, message_count, detail
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
            configured_index,
            recent_at,
            name,
            message_count,
            serde_json::to_string(detail)?,
        ],
    )?;
    Ok(())
}

fn insert_messages(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    start: i64,
    messages: &[Value],
) -> StoreResult<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO messages (
            generation, character_id, conversation_id, message_index, message_id, value
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (offset, message) in messages.iter().enumerate() {
        let message_id = message.get("chatId").and_then(Value::as_str);
        statement.execute(params![
            generation,
            character_id,
            conversation_id,
            start + offset as i64,
            message_id,
            serde_json::to_string(message)?,
        ])?;
    }
    Ok(())
}

fn apply_conversation_mutation(
    transaction: &Transaction<'_>,
    generation: &str,
    mutation: &ConversationMutation,
) -> StoreResult<()> {
    match mutation {
        ConversationMutation::Delete {
            character_id,
            conversation_id,
        } => {
            transaction.execute(
                "DELETE FROM messages WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                params![generation, character_id, conversation_id],
            )?;
            transaction.execute(
                "DELETE FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                params![generation, character_id, conversation_id],
            )?;
            refresh_character_summary(transaction, generation, character_id)
        }
        ConversationMutation::ReplaceRange {
            character_id,
            conversation_id,
            start,
            delete_count,
            messages,
            conversation,
            configured_index: requested_configured_index,
        } => {
            let existing: Option<(i64, i64, i64, String, String)> = transaction
                .query_row(
                    "SELECT configured_index, recent_at, message_count, name, detail
                     FROM conversations
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;

            if existing.is_some() && requested_configured_index.is_some() {
                return Err(validation(format!(
                    "Conversation {conversation_id} already exists"
                )));
            }

            let Some((configured_index, existing_recent_at, old_count, _, existing_detail)) =
                existing
            else {
                let Some(detail) = conversation else {
                    return Err(validation(format!(
                        "Conversation {conversation_id} does not exist"
                    )));
                };
                let mut value = object(detail, "Conversation")?.clone();
                value.insert("id".to_owned(), Value::String(conversation_id.clone()));
                value.insert("message".to_owned(), Value::Array(messages.clone()));
                let conversation_count: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM conversations WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                    |row| row.get(0),
                )?;
                let configured_index = requested_configured_index
                    .unwrap_or(conversation_count)
                    .clamp(0, conversation_count);
                if configured_index < conversation_count {
                    transaction.execute(
                        "UPDATE conversations SET configured_index = configured_index + 1
                         WHERE generation = ?1 AND character_id = ?2 AND configured_index >= ?3",
                        params![generation, character_id, configured_index],
                    )?;
                }
                put_conversation(
                    transaction,
                    generation,
                    character_id,
                    &Value::Object(value),
                    configured_index,
                )?;
                return refresh_character_summary(transaction, generation, character_id);
            };

            let start = (*start).clamp(0, old_count);
            let delete_count = (*delete_count).clamp(0, old_count - start);
            let delta = messages.len() as i64 - delete_count;
            if delete_count > 0 {
                transaction.execute(
                    "DELETE FROM messages
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
                       AND message_index >= ?4 AND message_index < ?5",
                    params![
                        generation,
                        character_id,
                        conversation_id,
                        start,
                        start + delete_count
                    ],
                )?;
            }
            if delta != 0 {
                transaction.execute(
                    "UPDATE messages SET message_index = -(message_index + ?4) - 1
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
                       AND message_index >= ?5",
                    params![
                        generation,
                        character_id,
                        conversation_id,
                        delta,
                        start + delete_count
                    ],
                )?;
                transaction.execute(
                    "UPDATE messages SET message_index = -message_index - 1
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
                       AND message_index < 0",
                    params![generation, character_id, conversation_id],
                )?;
            }
            insert_messages(
                transaction,
                generation,
                character_id,
                conversation_id,
                start,
                messages,
            )?;

            let detail = match conversation {
                Some(value) => value.clone(),
                None => serde_json::from_str(&existing_detail)?,
            };
            let name = required_string(&detail, "name", "Conversation")?;
            let recent_at = detail
                .get("lastDate")
                .and_then(Value::as_i64)
                .unwrap_or(existing_recent_at);
            put_conversation_record(
                transaction,
                generation,
                character_id,
                conversation_id,
                configured_index,
                recent_at,
                name,
                old_count + delta,
                &detail,
            )?;
            refresh_character_summary(transaction, generation, character_id)
        }
    }
}

fn refresh_character_summary(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<()> {
    transaction.execute(
        "UPDATE characters SET conversation_count = (
            SELECT COUNT(*) FROM conversations
            WHERE generation = ?1 AND character_id = ?2
         ) WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
    )?;
    Ok(())
}

fn character_exists(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<bool> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn delete_character(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<()> {
    delete_character_contents(transaction, generation, character_id)?;
    transaction.execute(
        "DELETE FROM characters WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
    )?;
    Ok(())
}

fn delete_character_contents(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM messages WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
    )?;
    transaction.execute(
        "DELETE FROM conversations WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
    )?;
    Ok(())
}

fn delete_generation(transaction: &Transaction<'_>, generation: &str) -> StoreResult<()> {
    for (table, _) in GENERATION_TABLES.iter().rev() {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE generation = ?1"),
            [generation],
        )?;
    }
    transaction.execute(
        "DELETE FROM snapshot_leases WHERE generation = ?1",
        [generation],
    )?;
    Ok(())
}

fn move_generation(transaction: &Transaction<'_>, source: &str, target: &str) -> StoreResult<()> {
    for (table, _) in GENERATION_TABLES {
        transaction.execute(
            &format!("UPDATE {table} SET generation = ?2 WHERE generation = ?1"),
            params![source, target],
        )?;
    }
    Ok(())
}

fn writable_generation(
    transaction: &Transaction<'_>,
    source: &str,
    revision: i64,
) -> StoreResult<String> {
    if !generation_is_leased(transaction, source)? {
        return Ok(source.to_owned());
    }
    let target = format!("revision-{revision}");
    for (table, columns) in GENERATION_TABLES {
        transaction.execute(
            &format!(
                "INSERT INTO {table} (generation, {columns})
                 SELECT ?1, {columns} FROM {table} WHERE generation = ?2"
            ),
            params![target, source],
        )?;
    }
    Ok(target)
}

fn generation_is_leased(transaction: &Transaction<'_>, generation: &str) -> StoreResult<bool> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM snapshot_leases WHERE generation = ?1 LIMIT 1",
            [generation],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn set_active(transaction: &Transaction<'_>, revision: i64, generation: &str) -> StoreResult<()> {
    transaction.execute(
        "UPDATE meta SET value = ?2 WHERE key = ?1",
        params!["currentRevision", serde_json::to_string(&revision)?],
    )?;
    transaction.execute(
        "UPDATE meta SET value = ?2 WHERE key = ?1",
        params!["activeGeneration", serde_json::to_string(generation)?],
    )?;
    Ok(())
}

fn without_field(value: &Value, field: &str) -> StoreResult<Value> {
    let mut object = object(value, "JSON object")?.clone();
    object.remove(field);
    Ok(Value::Object(object))
}

fn object<'a>(value: &'a Value, context: &str) -> StoreResult<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| validation(format!("{context} must be a JSON object")))
}

fn required_string<'a>(value: &'a Value, key: &str, context: &str) -> StoreResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| validation(format!("{context} requires {key}")))
}

fn validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
