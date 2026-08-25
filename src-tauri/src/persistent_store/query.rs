use super::{
    active_generation, current_revision, read_target, CharacterPage, CharacterQuery,
    CharacterSummary, ConversationPage, ConversationQuery, ConversationSummary, ConversationWindow,
    ConversationWindowQuery, PresetCatalog, PresetSummary, QueryOrder, StoreError, StoreResult,
    Versioned,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

pub(super) fn read_root(
    connection: &Connection,
    lease: Option<&str>,
) -> StoreResult<Versioned<Value>> {
    let target = read_target(connection, lease)?;
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&target.generation],
            |row| row.get(0),
        )
        .optional()?;
    Ok(Versioned {
        revision: target.revision,
        value: value.map_or(Ok(Value::Object(Map::new())), |value| {
            serde_json::from_str(&value).map_err(StoreError::from)
        })?,
    })
}

pub(super) fn query_presets(
    connection: &Connection,
    lease: Option<&str>,
) -> StoreResult<PresetCatalog> {
    let target = read_target(connection, lease)?;
    let mut statement = connection.prepare(
        "SELECT preset_id, name, image, configured_index FROM bot_presets
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let items = statement
        .query_map([&target.generation], |row| {
            Ok(PresetSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                image: row.get(2)?,
                configured_index: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PresetCatalog {
        revision: target.revision,
        items,
    })
}

pub(super) fn read_preset(
    connection: &Connection,
    id: &str,
    lease: Option<&str>,
) -> StoreResult<Option<Versioned<Value>>> {
    let target = read_target(connection, lease)?;
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM bot_presets WHERE generation = ?1 AND preset_id = ?2",
            params![target.generation, id],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| {
            Ok(Versioned {
                revision: target.revision,
                value: serde_json::from_str(&value)?,
            })
        })
        .transpose()
}

pub(super) fn query_characters(
    connection: &Connection,
    query: &CharacterQuery,
    lease: Option<&str>,
) -> StoreResult<CharacterPage> {
    let target = read_target(connection, lease)?;
    let (limit, offset) = page_input(query.limit, query.cursor.as_deref())?;
    let order = order_sql(query.order);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let mut statement = connection.prepare(&format!(
        "SELECT character_id, name, image, configured_index, recent_at, trashed, conversation_count,
                type, creator_notes, trash_time
         FROM characters
         WHERE generation = ?1 AND trashed = ?2
         ORDER BY {order}"
    ))?;
    let mut rows = statement.query(params![target.generation, query.trash as i64])?;
    let mut items = Vec::new();
    let mut matched = 0;
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        let summary = CharacterSummary {
            id: row.get(0)?,
            name: row.get(1)?,
            image: row.get(2)?,
            configured_index: row.get(3)?,
            recent_at: row.get(4)?,
            trashed: row.get::<_, i64>(5)? != 0,
            conversation_count: row.get(6)?,
            r#type: row.get(7)?,
            creator_notes: row.get(8)?,
            trash_time: row.get(9)?,
        };
        if search
            .as_ref()
            .is_some_and(|search| !summary.name.to_lowercase().contains(search))
        {
            continue;
        }
        if matched < offset {
            matched += 1;
            continue;
        }
        if items.len() as i64 == limit {
            has_more = true;
            break;
        }
        items.push(summary);
        matched += 1;
    }
    Ok(CharacterPage {
        revision: target.revision,
        next_cursor: has_more.then(|| (offset + items.len() as i64).to_string()),
        items,
    })
}

pub(super) fn read_character(
    connection: &Connection,
    id: &str,
    lease: Option<&str>,
) -> StoreResult<Option<Versioned<Value>>> {
    let target = read_target(connection, lease)?;
    let detail: Option<String> = connection
        .query_row(
            "SELECT detail FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![target.generation, id],
            |row| row.get(0),
        )
        .optional()?;
    detail
        .map(|detail| {
            Ok(Versioned {
                revision: target.revision,
                value: serde_json::from_str(&detail)?,
            })
        })
        .transpose()
}

pub(super) fn query_conversations(
    connection: &Connection,
    query: &ConversationQuery,
    lease: Option<&str>,
) -> StoreResult<ConversationPage> {
    let target = read_target(connection, lease)?;
    let (limit, offset) = page_input(query.limit, query.cursor.as_deref())?;
    let order = order_sql(query.order);
    let mut statement = connection.prepare(&format!(
        "SELECT conversation_id, character_id, name, configured_index, recent_at, message_count
         FROM conversations WHERE generation = ?1 AND character_id = ?2
         ORDER BY {order} LIMIT ?3 OFFSET ?4"
    ))?;
    let mut rows = statement.query(params![
        target.generation,
        query.character_id,
        limit + 1,
        offset
    ])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(ConversationSummary {
            id: row.get(0)?,
            character_id: row.get(1)?,
            name: row.get(2)?,
            configured_index: row.get(3)?,
            recent_at: row.get(4)?,
            message_count: row.get(5)?,
        });
    }
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.pop();
    }
    Ok(ConversationPage {
        revision: target.revision,
        next_cursor: has_more.then(|| (offset + items.len() as i64).to_string()),
        items,
    })
}

pub(super) fn read_conversation(
    connection: &Connection,
    character_id: &str,
    conversation_id: &str,
    lease: Option<&str>,
) -> StoreResult<Option<Versioned<Value>>> {
    let target = read_target(connection, lease)?;
    let detail: Option<String> = connection
        .query_row(
            "SELECT detail FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![target.generation, character_id, conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(detail) = detail else {
        return Ok(None);
    };
    Ok(Some(Versioned {
        revision: target.revision,
        value: conversation_value(
            connection,
            &target.generation,
            character_id,
            conversation_id,
            detail,
        )?,
    }))
}

pub(super) fn read_conversation_window(
    connection: &Connection,
    query: &ConversationWindowQuery,
    lease: Option<&str>,
) -> StoreResult<Option<Versioned<ConversationWindow>>> {
    let target = read_target(connection, lease)?;
    let total: Option<i64> = connection
        .query_row(
            "SELECT message_count FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![target.generation, query.character_id, query.conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(total_messages) = total else {
        return Ok(None);
    };

    let (start_index, end_index) = match query.anchor_message_id.as_deref() {
        Some(anchor_id) => {
            let anchor: Option<i64> = connection.query_row(
                "SELECT message_index FROM messages
                 WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_id = ?4
                 ORDER BY message_index ASC LIMIT 1",
                params![target.generation, query.character_id, query.conversation_id, anchor_id],
                |row| row.get(0),
            ).optional()?;
            let Some(anchor) = anchor else {
                return Ok(None);
            };
            let before = query.before.unwrap_or(0).max(0);
            let after = query.after.unwrap_or(0).max(0);
            (
                (anchor - before).max(0),
                (anchor + after + 1).min(total_messages),
            )
        }
        None => {
            let limit = query.limit.unwrap_or(128).max(0);
            ((total_messages - limit).max(0), total_messages)
        }
    };
    let messages = read_messages(
        connection,
        &target.generation,
        &query.character_id,
        &query.conversation_id,
        start_index,
        end_index,
    )?;
    Ok(Some(Versioned {
        revision: target.revision,
        value: ConversationWindow {
            character_id: query.character_id.clone(),
            conversation_id: query.conversation_id.clone(),
            messages,
            start_index,
            end_index,
            total_messages,
            has_more_before: start_index > 0,
            has_more_after: end_index < total_messages,
        },
    }))
}

pub(super) fn materialize(connection: &Connection, revision: Option<i64>) -> StoreResult<Value> {
    let transaction = connection.unchecked_transaction()?;
    let actual = current_revision(&transaction)?;
    let expected = revision.unwrap_or(actual);
    if expected != actual {
        return Err(StoreError::RevisionConflict { expected, actual });
    }
    let generation = active_generation(&transaction)?;
    let root: Option<String> = transaction
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&generation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(root) = root else {
        return Err(StoreError::RevisionConflict { expected, actual });
    };
    let mut database = into_object(
        serde_json::from_str(&root)?,
        "Persistent root must be an object",
    )?;
    let character_records = {
        let mut statement = transaction.prepare(
            "SELECT character_id, detail FROM characters
             WHERE generation = ?1 ORDER BY configured_index ASC",
        )?;
        let rows = statement.query_map([&generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut characters = Vec::new();
    for (character_id, detail) in character_records {
        let mut character = into_object(
            serde_json::from_str(&detail)?,
            "Character detail must be an object",
        )?;
        let conversation_records = {
            let mut statement = transaction.prepare(
                "SELECT conversation_id, detail FROM conversations
                 WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
            )?;
            let rows = statement.query_map(params![generation, character_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut chats = Vec::new();
        for (conversation_id, detail) in conversation_records {
            chats.push(conversation_value(
                &transaction,
                &generation,
                &character_id,
                &conversation_id,
                detail,
            )?);
        }
        character.insert("chats".to_owned(), Value::Array(chats));
        characters.push(Value::Object(character));
    }
    database.insert("characters".to_owned(), Value::Array(characters));
    let presets = {
        let mut statement = transaction.prepare(
            "SELECT value FROM bot_presets WHERE generation = ?1 ORDER BY configured_index ASC",
        )?;
        let presets = statement
            .query_map([&generation], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<StoreResult<Vec<_>>>()?;
        presets
    };
    database.insert("botPresets".to_owned(), Value::Array(presets));
    transaction.commit()?;
    Ok(Value::Object(database))
}

fn page_input(limit: i64, cursor: Option<&str>) -> StoreResult<(i64, i64)> {
    if limit <= 0 {
        return Err(StoreError::Validation {
            message: "Query limit must be a positive number".to_owned(),
        });
    }
    Ok((limit, cursor.and_then(parse_cursor).unwrap_or(0)))
}

fn parse_cursor(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let value = value.strip_prefix('+').unwrap_or(value);
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn order_sql(order: QueryOrder) -> &'static str {
    match order {
        QueryOrder::Configured => "configured_index ASC",
        QueryOrder::Recent => "recent_at DESC, configured_index ASC",
    }
}

fn read_messages(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    start: i64,
    end: i64,
) -> StoreResult<Vec<Value>> {
    let mut statement = connection.prepare("SELECT value FROM messages WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_index >= ?4 AND message_index < ?5 ORDER BY message_index ASC")?;
    let rows = statement.query_map(
        params![generation, character_id, conversation_id, start, end],
        |row| row.get::<_, String>(0),
    )?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn conversation_value(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    detail: String,
) -> StoreResult<Value> {
    let mut value = into_object(
        serde_json::from_str(&detail)?,
        "Conversation detail must be an object",
    )?;
    value.insert(
        "message".to_owned(),
        Value::Array(read_messages(
            connection,
            generation,
            character_id,
            conversation_id,
            0,
            i64::MAX,
        )?),
    );
    Ok(Value::Object(value))
}

fn into_object(value: Value, message: &str) -> StoreResult<Map<String, Value>> {
    value.as_object().cloned().ok_or_else(|| StoreError::Store {
        message: message.to_owned(),
    })
}
