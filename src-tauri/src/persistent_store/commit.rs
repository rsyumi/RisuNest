use super::{
    active_generation, current_revision, ConversationMutation, RevisionResult, StagingResult,
    StoreError, StoreResult, WorkingSetCommit,
};
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

    let generation = active_generation(&transaction)?;
    if let Some(root) = &input.root {
        put_root(&transaction, &generation, root)?;
    }
    if let Some(character_id) = &input.delete_character_id {
        delete_character(&transaction, &generation, character_id)?;
    }
    if let Some(character) = &input.character {
        put_character_detail(&transaction, &generation, character)?;
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

    let revision = actual_revision + 1;
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
    put_root(&transaction, staging_id, root)?;
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
    delete_generation(&transaction, &active)?;
    move_generation(&transaction, staging_id, &generation)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
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
    transaction.execute(
        "INSERT INTO root (generation, value) VALUES (?1, ?2) ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
        params![generation, serde_json::to_string(root)?],
    )?;
    Ok(())
}

fn require_staging(transaction: &Transaction<'_>, staging_id: &str) -> StoreResult<()> {
    if !staging_id.starts_with("staging-") {
        return Err(validation("Invalid staging generation"));
    }
    let exists = transaction
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
    transaction.execute(
        "INSERT INTO characters (
            generation, character_id, configured_index, recent_at, trashed, name, image,
            conversation_count, detail
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        ON CONFLICT(generation, character_id) DO UPDATE SET
            configured_index = excluded.configured_index,
            recent_at = excluded.recent_at,
            trashed = excluded.trashed,
            name = excluded.name,
            image = excluded.image,
            conversation_count = excluded.conversation_count,
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
                let configured_index: i64 = transaction.query_row(
                    "SELECT COUNT(*) FROM conversations WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                    |row| row.get(0),
                )?;
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
    transaction.execute("DELETE FROM messages WHERE generation = ?1", [generation])?;
    transaction.execute(
        "DELETE FROM conversations WHERE generation = ?1",
        [generation],
    )?;
    transaction.execute("DELETE FROM characters WHERE generation = ?1", [generation])?;
    transaction.execute("DELETE FROM root WHERE generation = ?1", [generation])?;
    transaction.execute(
        "DELETE FROM snapshot_leases WHERE generation = ?1",
        [generation],
    )?;
    Ok(())
}

fn move_generation(transaction: &Transaction<'_>, source: &str, target: &str) -> StoreResult<()> {
    transaction.execute(
        "UPDATE root SET generation = ?2 WHERE generation = ?1",
        params![source, target],
    )?;
    transaction.execute(
        "UPDATE characters SET generation = ?2 WHERE generation = ?1",
        params![source, target],
    )?;
    transaction.execute(
        "UPDATE conversations SET generation = ?2 WHERE generation = ?1",
        params![source, target],
    )?;
    transaction.execute(
        "UPDATE messages SET generation = ?2 WHERE generation = ?1",
        params![source, target],
    )?;
    Ok(())
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
