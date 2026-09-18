//! Character archiving. The `characters` row stays where it is; its conversations,
//! messages and detail move into one compressed CAS object and the row records it.
use super::{RevisionResult, StoreError, StoreResult};
use crate::asset_repository::PayloadCas;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};

const ARCHIVE_PAYLOAD_VERSION: u32 = 1;
const MAX_DECODED_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The value stored in `characters.archived_object`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ArchivedObject {
    pub(crate) object_hash: String,
    pub(crate) archived_at: i64,
    pub(crate) conversation_count: i64,
    pub(crate) message_count: i64,
    pub(crate) asset_hashes: Vec<String>,
}

/// What the confirmation dialog needs before anything is written.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchivePreview {
    pub(crate) character_id: String,
    pub(crate) name: String,
    pub(crate) conversation_count: i64,
    pub(crate) message_count: i64,
    pub(crate) archived: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchivePayload {
    version: u32,
    character_id: String,
    detail: Value,
    conversations: Vec<ArchivedConversation>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchivedConversation {
    conversation_id: String,
    configured_index: i64,
    recent_at: i64,
    name: String,
    message_count: i64,
    detail: Value,
    messages: Vec<ArchivedMessage>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchivedMessage {
    message_index: i64,
    message_id: Option<String>,
    value: Value,
}

fn validation<T>(message: impl Into<String>) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.into(),
    })
}

pub(super) fn archived_error(character_id: &str) -> StoreError {
    StoreError::Validation {
        message: format!("Character {character_id} is archived"),
    }
}

pub(super) fn read_archived_object(
    connection: &Connection,
    generation: &str,
    character_id: &str,
) -> StoreResult<Option<ArchivedObject>> {
    let stored: Option<Option<String>> = connection
        .query_row(
            "SELECT archived_object FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .flatten()
        .map(|stored| serde_json::from_str(&stored).map_err(StoreError::from))
        .transpose()
}

pub(super) fn is_archived(
    connection: &Connection,
    generation: &str,
    character_id: &str,
) -> StoreResult<bool> {
    let archived: Option<i64> = connection
        .query_row(
            "SELECT archived_object IS NOT NULL FROM characters
             WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(archived.unwrap_or(0) != 0)
}

pub(super) fn archived_character_ids(
    connection: &Connection,
    generation: &str,
) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT character_id FROM characters
         WHERE generation = ?1 AND archived_object IS NOT NULL
         ORDER BY configured_index ASC",
    )?;
    let ids = statement
        .query_map([generation], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// The detail an archived row keeps. `detail` is NOT NULL, and readers that only
/// need identity still work; anything that needs the character fails explicitly.
pub(super) fn marker_detail(character_id: &str, name: &str, character_type: &str) -> Value {
    serde_json::json!({
        "chaId": character_id,
        "name": name,
        "type": character_type,
        "chats": [],
        "risuNestArchived": true,
    })
}

#[cfg(test)]
pub(super) fn is_marker_detail(detail: &Value) -> bool {
    detail
        .get("risuNestArchived")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn preview(
    connection: &Connection,
    generation: &str,
    character_id: &str,
) -> StoreResult<ArchivePreview> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT name, archived_object FROM characters
             WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((name, archived_object)) = row else {
        return validation(format!("Character {character_id} does not exist"));
    };
    if let Some(archived_object) = archived_object {
        let archived: ArchivedObject = serde_json::from_str(&archived_object)?;
        return Ok(ArchivePreview {
            character_id: character_id.to_owned(),
            name,
            conversation_count: archived.conversation_count,
            message_count: archived.message_count,
            archived: true,
        });
    }
    let (conversation_count, message_count): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), COALESCE(SUM(message_count), 0) FROM conversations
         WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(ArchivePreview {
        character_id: character_id.to_owned(),
        name,
        conversation_count,
        message_count,
        archived: false,
    })
}

fn read_payload(
    connection: &Connection,
    generation: &str,
    character_id: &str,
) -> StoreResult<(ArchivePayload, String, String)> {
    let row: Option<(String, String, Option<String>)> = connection
        .query_row(
            "SELECT detail, type, archived_object FROM characters
             WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((detail, character_type, archived_object)) = row else {
        return validation(format!("Character {character_id} does not exist"));
    };
    if archived_object.is_some() {
        return Err(archived_error(character_id));
    }
    let detail: Value = serde_json::from_str(&detail)?;
    let name = detail
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut statement = connection.prepare(
        "SELECT conversation_id, configured_index, recent_at, name, message_count, detail
         FROM conversations WHERE generation = ?1 AND character_id = ?2
         ORDER BY configured_index ASC",
    )?;
    let conversation_rows = {
        statement
            .query_map(params![generation, character_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut conversations = Vec::with_capacity(conversation_rows.len());
    for (conversation_id, configured_index, recent_at, name, message_count, detail) in
        conversation_rows
    {
        let mut statement = connection.prepare_cached(
            "SELECT message_index, message_id, value FROM messages
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
             ORDER BY message_index ASC",
        )?;
        let messages = {
            statement
                .query_map(params![generation, character_id, conversation_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let messages = messages
            .into_iter()
            .map(|(message_index, message_id, value)| {
                Ok(ArchivedMessage {
                    message_index,
                    message_id,
                    value: serde_json::from_str(&value)?,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        conversations.push(ArchivedConversation {
            conversation_id,
            configured_index,
            recent_at,
            name,
            message_count,
            detail: serde_json::from_str(&detail)?,
            messages,
        });
    }

    Ok((
        ArchivePayload {
            version: ARCHIVE_PAYLOAD_VERSION,
            character_id: character_id.to_owned(),
            detail,
            conversations,
        },
        name,
        character_type,
    ))
}

fn compress(payload: &ArchivePayload) -> StoreResult<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    serde_json::to_writer(&mut encoder, payload)?;
    encoder.write_all(&[])?;
    encoder.finish().map_err(StoreError::from)
}

fn decompress(bytes: &[u8]) -> StoreResult<ArchivePayload> {
    let mut decoder = GzDecoder::new(bytes).take(MAX_DECODED_ARCHIVE_BYTES);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;
    let payload: ArchivePayload = serde_json::from_slice(&decoded)?;
    if payload.version != ARCHIVE_PAYLOAD_VERSION {
        return validation("archived character payload version is unsupported");
    }
    Ok(payload)
}

/// Writes the payload object first and commits the row change afterwards, so a
/// failure leaves an unreferenced object for the next sweep and no DB change.
pub(super) fn archive_character(
    connection: &mut Connection,
    cas: &PayloadCas,
    character_id: &str,
    expected_revision: i64,
    now_ms: i64,
) -> StoreResult<RevisionResult> {
    if now_ms < 0 {
        return validation("archive timestamp must be nonnegative");
    }
    let generation = super::active_generation(connection)?;
    let actual_revision = super::current_revision(connection)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let (payload, name, character_type) = read_payload(connection, &generation, character_id)?;
    let asset_hashes =
        super::snapshot::collect_character_asset_hashes(connection, cas, &generation, character_id)?;
    let conversation_count = payload.conversations.len() as i64;
    let message_count = payload
        .conversations
        .iter()
        .map(|conversation| conversation.messages.len() as i64)
        .sum();
    let bytes = compress(&payload)?;
    let prepared = cas.prepare_bytes(&bytes)?;
    super::AssetObjectCatalog::new(connection).register(
        &[super::asset_object_catalog::AssetObjectRegistration {
            object_hash: prepared.content_hash.clone(),
            byte_size: prepared.byte_size,
        }],
        now_ms,
    )?;
    let archived = ArchivedObject {
        object_hash: prepared.content_hash,
        archived_at: now_ms,
        conversation_count,
        message_count,
        asset_hashes,
    };
    let encoded = serde_json::to_string(&archived)?;
    let marker = serde_json::to_string(&marker_detail(character_id, &name, &character_type))?;

    super::commit::incremental_commit(
        connection,
        expected_revision,
        |transaction, active| {
            if is_archived(transaction, active, character_id)? {
                return Err(archived_error(character_id));
            }
            Ok(())
        },
        |transaction, generation, ()| {
            transaction.execute(
                "DELETE FROM messages WHERE generation = ?1 AND character_id = ?2",
                params![generation, character_id],
            )?;
            transaction.execute(
                "DELETE FROM conversations WHERE generation = ?1 AND character_id = ?2",
                params![generation, character_id],
            )?;
            let updated = transaction.execute(
                "UPDATE characters
                 SET detail = ?3, archived_object = ?4, conversation_count = 0
                 WHERE generation = ?1 AND character_id = ?2",
                params![generation, character_id, marker, encoded],
            )?;
            if updated != 1 {
                return validation(format!("Character {character_id} does not exist"));
            }
            Ok(())
        },
    )
}

pub(super) fn restore_character(
    connection: &mut Connection,
    cas: &PayloadCas,
    character_id: &str,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    let generation = super::active_generation(connection)?;
    let actual_revision = super::current_revision(connection)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let Some(archived) = read_archived_object(connection, &generation, character_id)? else {
        return validation(format!("Character {character_id} is not archived"));
    };
    let Some(bytes) = cas.read_object(&archived.object_hash)? else {
        return validation(format!(
            "Archived character {character_id} is missing its stored data"
        ));
    };
    let payload = decompress(&bytes)?;
    if payload.character_id != character_id {
        return validation(format!(
            "Archived character {character_id} holds another character"
        ));
    }
    let detail = serde_json::to_string(&payload.detail)?;
    let recent_at = payload
        .detail
        .get("lastInteraction")
        .and_then(Value::as_i64)
        .unwrap_or_default();

    super::commit::incremental_commit(
        connection,
        expected_revision,
        |transaction, active| {
            if !is_archived(transaction, active, character_id)? {
                return validation(format!("Character {character_id} is not archived"));
            }
            Ok(())
        },
        |transaction, generation, ()| {
            for conversation in &payload.conversations {
                transaction.execute(
                    "INSERT INTO conversations (
                        generation, character_id, conversation_id, configured_index, recent_at,
                        name, message_count, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        generation,
                        character_id,
                        conversation.conversation_id,
                        conversation.configured_index,
                        conversation.recent_at,
                        conversation.name,
                        conversation.message_count,
                        serde_json::to_string(&conversation.detail)?,
                    ],
                )?;
                for message in &conversation.messages {
                    transaction.execute(
                        "INSERT INTO messages (
                            generation, character_id, conversation_id, message_index,
                            message_id, value
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![
                            generation,
                            character_id,
                            conversation.conversation_id,
                            message.message_index,
                            message.message_id,
                            serde_json::to_string(&message.value)?,
                        ],
                    )?;
                }
            }
            let updated = transaction.execute(
                "UPDATE characters
                 SET detail = ?3, archived_object = NULL, conversation_count = ?4, recent_at = ?5
                 WHERE generation = ?1 AND character_id = ?2",
                params![
                    generation,
                    character_id,
                    detail,
                    payload.conversations.len() as i64,
                    recent_at,
                ],
            )?;
            if updated != 1 {
                return validation(format!("Character {character_id} does not exist"));
            }
            Ok(())
        },
    )
}
