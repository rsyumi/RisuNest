//! Character archiving. The `characters` row stays where it is; its conversations,
//! messages and detail move into one compressed CAS object and the row records it.
use super::{RevisionResult, StoreError, StoreResult};
use crate::asset_repository::PayloadCas;
use flate2::{bufread::GzDecoder, write::GzEncoder, Compression};
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};

const ARCHIVE_PAYLOAD_VERSION: u32 = 1;
const MAX_DECODED_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ARCHIVE_CANCELLED_MESSAGE: &str = "character archive operation cancelled";

fn ensure_not_cancelled(is_cancelled: &dyn Fn() -> bool) -> StoreResult<()> {
    if is_cancelled() {
        return validation(ARCHIVE_CANCELLED_MESSAGE);
    }
    Ok(())
}

struct CancellationAwareIo<'a, T> {
    inner: T,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'a, T> CancellationAwareIo<'a, T> {
    fn new(inner: T, is_cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { inner, is_cancelled }
    }

    fn check(&self) -> io::Result<()> {
        if (self.is_cancelled)() {
            return Err(io::Error::other(ARCHIVE_CANCELLED_MESSAGE));
        }
        Ok(())
    }
}

impl<T: Read> Read for CancellationAwareIo<'_, T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.read(buffer)
    }
}

impl<T: Write> Write for CancellationAwareIo<'_, T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check()?;
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.inner.flush()
    }
}

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

#[cfg(test)]
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchivePayload {
    version: u32,
    character_id: String,
    detail: Value,
    conversations: Vec<ArchivedConversation>,
}

#[cfg(test)]
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

struct ArchivePayloadMetadata {
    name: String,
    character_type: String,
    conversation_count: i64,
    message_count: i64,
}

fn write_payload_to(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    mut writer: impl Write,
    is_cancelled: &dyn Fn() -> bool,
) -> StoreResult<ArchivePayloadMetadata> {
    ensure_not_cancelled(is_cancelled)?;
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
    let detail_value: Value = serde_json::from_str(&detail)?;
    let name = detail_value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    writer.write_all(br#"{"version":"#)?;
    serde_json::to_writer(&mut writer, &ARCHIVE_PAYLOAD_VERSION)?;
    writer.write_all(br#","characterId":"#)?;
    serde_json::to_writer(&mut writer, character_id)?;
    writer.write_all(br#","detail":"#)?;
    serde_json::to_writer(&mut writer, &detail_value)?;
    writer.write_all(br#","conversations":["#)?;

    let mut statement = connection.prepare(
        "SELECT conversation_id, configured_index, recent_at, name, message_count, detail
         FROM conversations WHERE generation = ?1 AND character_id = ?2
         ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id])?;
    let mut conversation_count = 0_i64;
    let mut total_message_count = 0_i64;
    while let Some(row) = rows.next()? {
        ensure_not_cancelled(is_cancelled)?;
        let conversation_id = row.get::<_, String>(0)?;
        let configured_index = row.get::<_, i64>(1)?;
        let recent_at = row.get::<_, i64>(2)?;
        let conversation_name = row.get::<_, String>(3)?;
        let message_count = row.get::<_, i64>(4)?;
        let conversation_detail = row.get::<_, String>(5)?;
        let conversation_detail: Value = serde_json::from_str(&conversation_detail)?;

        if conversation_count != 0 {
            writer.write_all(b",")?;
        }
        writer.write_all(br#"{"conversationId":"#)?;
        serde_json::to_writer(&mut writer, &conversation_id)?;
        writer.write_all(br#","configuredIndex":"#)?;
        serde_json::to_writer(&mut writer, &configured_index)?;
        writer.write_all(br#","recentAt":"#)?;
        serde_json::to_writer(&mut writer, &recent_at)?;
        writer.write_all(br#","name":"#)?;
        serde_json::to_writer(&mut writer, &conversation_name)?;
        writer.write_all(br#","messageCount":"#)?;
        serde_json::to_writer(&mut writer, &message_count)?;
        writer.write_all(br#","detail":"#)?;
        serde_json::to_writer(&mut writer, &conversation_detail)?;
        writer.write_all(br#","messages":["#)?;

        let mut statement = connection.prepare_cached(
            "SELECT message_index, message_id, value FROM messages
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
             ORDER BY message_index ASC",
        )?;
        let mut message_rows =
            statement.query(params![generation, character_id, conversation_id])?;
        let mut written_messages = 0_i64;
        while let Some(row) = message_rows.next()? {
            ensure_not_cancelled(is_cancelled)?;
            let message_index = row.get::<_, i64>(0)?;
            let message_id = row.get::<_, Option<String>>(1)?;
            let value = row.get::<_, String>(2)?;
            let value: Value = serde_json::from_str(&value)?;
            if written_messages != 0 {
                writer.write_all(b",")?;
            }
            writer.write_all(br#"{"messageIndex":"#)?;
            serde_json::to_writer(&mut writer, &message_index)?;
            writer.write_all(br#","messageId":"#)?;
            serde_json::to_writer(&mut writer, &message_id)?;
            writer.write_all(br#","value":"#)?;
            serde_json::to_writer(&mut writer, &value)?;
            writer.write_all(b"}")?;
            written_messages += 1;
        }
        writer.write_all(b"]}")?;
        conversation_count += 1;
        total_message_count = total_message_count
            .checked_add(written_messages)
            .ok_or_else(|| StoreError::Validation {
                message: "archived character message count overflow".to_owned(),
            })?;
    }
    writer.write_all(b"]}")?;

    Ok(ArchivePayloadMetadata {
        name,
        character_type,
        conversation_count,
        message_count: total_message_count,
    })
}

#[cfg(test)]
fn decompress(bytes: &[u8]) -> StoreResult<ArchivePayload> {
    decompress_bounded(bytes, MAX_DECODED_ARCHIVE_BYTES)
}

#[cfg(test)]
fn decompress_bounded(bytes: &[u8], max_bytes: u64) -> StoreResult<ArchivePayload> {
    let mut decoder = BufReader::new(GzDecoder::new(bytes).take(max_bytes.saturating_add(1)));
    let payload: ArchivePayload = serde_json::from_reader(&mut decoder)?;
    if decoder.get_ref().limit() == 0 {
        return validation("archived character exceeds the decoded size limit");
    }
    if !decoder.into_inner().into_inner().into_inner().is_empty() {
        return validation("archived character has trailing compressed data");
    }
    if payload.version != ARCHIVE_PAYLOAD_VERSION {
        return validation("archived character payload version is unsupported");
    }
    Ok(payload)
}

const RESTORE_CONVERSATIONS: &str = "archive_restore_conversations";
const RESTORE_MESSAGES: &str = "archive_restore_messages";

struct StagedArchivePayload {
    version: u32,
    character_id: String,
    detail: Value,
    conversation_count: i64,
    message_count: i64,
}

struct ArchivePayloadSeed<'a> {
    connection: &'a Connection,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> DeserializeSeed<'de> for ArchivePayloadSeed<'_> {
    type Value = StagedArchivePayload;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(ArchivePayloadVisitor {
            connection: self.connection,
            is_cancelled: self.is_cancelled,
        })
    }
}

struct ArchivePayloadVisitor<'a> {
    connection: &'a Connection,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> Visitor<'de> for ArchivePayloadVisitor<'_> {
    type Value = StagedArchivePayload;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a version 1 archived character payload")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut version = None;
        let mut character_id = None;
        let mut detail = None;
        let mut counts = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "version" => set_once(&mut version, map.next_value()?, "version")?,
                "characterId" => set_once(&mut character_id, map.next_value()?, "characterId")?,
                "detail" => set_once(&mut detail, map.next_value()?, "detail")?,
                "conversations" => set_once(
                    &mut counts,
                    map.next_value_seed(ConversationsSeed {
                        connection: self.connection,
                        is_cancelled: self.is_cancelled,
                    })?,
                    "conversations",
                )?,
                _ => return Err(de::Error::unknown_field(&field, ARCHIVE_PAYLOAD_FIELDS)),
            }
        }
        let (conversation_count, message_count) =
            counts.ok_or_else(|| de::Error::missing_field("conversations"))?;
        Ok(StagedArchivePayload {
            version: version.ok_or_else(|| de::Error::missing_field("version"))?,
            character_id: character_id.ok_or_else(|| de::Error::missing_field("characterId"))?,
            detail: detail.ok_or_else(|| de::Error::missing_field("detail"))?,
            conversation_count,
            message_count,
        })
    }
}

const ARCHIVE_PAYLOAD_FIELDS: &[&str] = &["version", "characterId", "detail", "conversations"];
const ARCHIVED_CONVERSATION_FIELDS: &[&str] = &[
    "conversationId",
    "configuredIndex",
    "recentAt",
    "name",
    "messageCount",
    "detail",
    "messages",
];

fn set_once<E: de::Error, T>(
    target: &mut Option<T>,
    value: T,
    field: &'static str,
) -> Result<(), E> {
    if target.replace(value).is_some() {
        return Err(E::duplicate_field(field));
    }
    Ok(())
}

struct ConversationsSeed<'a> {
    connection: &'a Connection,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> DeserializeSeed<'de> for ConversationsSeed<'_> {
    type Value = (i64, i64);

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(ConversationsVisitor {
            connection: self.connection,
            is_cancelled: self.is_cancelled,
        })
    }
}

struct ConversationsVisitor<'a> {
    connection: &'a Connection,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> Visitor<'de> for ConversationsVisitor<'_> {
    type Value = (i64, i64);

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of archived conversations")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut conversation_count = 0_i64;
        let mut message_count = 0_i64;
        let mut previous_index = None;
        loop {
            if (self.is_cancelled)() {
                return Err(de::Error::custom(ARCHIVE_CANCELLED_MESSAGE));
            }
            let ordinal = conversation_count;
            let Some(conversation) = sequence.next_element_seed(ConversationSeed {
                connection: self.connection,
                ordinal,
                is_cancelled: self.is_cancelled,
            })?
            else {
                break;
            };
            if previous_index.is_some_and(|previous| conversation.configured_index < previous) {
                return Err(de::Error::custom(
                    "archived conversations are not in configured order",
                ));
            }
            if conversation.message_count != conversation.actual_message_count {
                return Err(de::Error::custom(
                    "archived conversation message count does not match its messages",
                ));
            }
            self.connection
                .execute(
                    "INSERT INTO archive_restore.archive_restore_conversations (
                        ordinal, conversation_id, configured_index, recent_at, name,
                        message_count, detail
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        ordinal,
                        conversation.conversation_id,
                        conversation.configured_index,
                        conversation.recent_at,
                        conversation.name,
                        conversation.message_count,
                        conversation.detail,
                    ],
                )
                .map_err(de::Error::custom)?;
            previous_index = Some(conversation.configured_index);
            conversation_count = conversation_count
                .checked_add(1)
                .ok_or_else(|| de::Error::custom("archived conversation count overflow"))?;
            message_count = message_count
                .checked_add(conversation.actual_message_count)
                .ok_or_else(|| de::Error::custom("archived message count overflow"))?;
        }
        Ok((conversation_count, message_count))
    }
}

struct StagedConversation {
    conversation_id: String,
    configured_index: i64,
    recent_at: i64,
    name: String,
    message_count: i64,
    detail: String,
    actual_message_count: i64,
}

struct ConversationSeed<'a> {
    connection: &'a Connection,
    ordinal: i64,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> DeserializeSeed<'de> for ConversationSeed<'_> {
    type Value = StagedConversation;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(ConversationVisitor {
            connection: self.connection,
            ordinal: self.ordinal,
            is_cancelled: self.is_cancelled,
        })
    }
}

struct ConversationVisitor<'a> {
    connection: &'a Connection,
    ordinal: i64,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> Visitor<'de> for ConversationVisitor<'_> {
    type Value = StagedConversation;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an archived conversation")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut conversation_id = None;
        let mut configured_index = None;
        let mut recent_at = None;
        let mut name = None;
        let mut message_count = None;
        let mut detail: Option<Value> = None;
        let mut actual_message_count = None;
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "conversationId" => {
                    set_once(&mut conversation_id, map.next_value()?, "conversationId")?
                }
                "configuredIndex" => {
                    set_once(&mut configured_index, map.next_value()?, "configuredIndex")?
                }
                "recentAt" => set_once(&mut recent_at, map.next_value()?, "recentAt")?,
                "name" => set_once(&mut name, map.next_value()?, "name")?,
                "messageCount" => set_once(&mut message_count, map.next_value()?, "messageCount")?,
                "detail" => set_once(&mut detail, map.next_value()?, "detail")?,
                "messages" => set_once(
                    &mut actual_message_count,
                    map.next_value_seed(MessagesSeed {
                        connection: self.connection,
                        conversation_ordinal: self.ordinal,
                        is_cancelled: self.is_cancelled,
                    })?,
                    "messages",
                )?,
                _ => {
                    return Err(de::Error::unknown_field(
                        &field,
                        ARCHIVED_CONVERSATION_FIELDS,
                    ))
                }
            }
        }
        Ok(StagedConversation {
            conversation_id: conversation_id
                .ok_or_else(|| de::Error::missing_field("conversationId"))?,
            configured_index: configured_index
                .ok_or_else(|| de::Error::missing_field("configuredIndex"))?,
            recent_at: recent_at.ok_or_else(|| de::Error::missing_field("recentAt"))?,
            name: name.ok_or_else(|| de::Error::missing_field("name"))?,
            message_count: message_count.ok_or_else(|| de::Error::missing_field("messageCount"))?,
            detail: serde_json::to_string(
                &detail.ok_or_else(|| de::Error::missing_field("detail"))?,
            )
            .map_err(de::Error::custom)?,
            actual_message_count: actual_message_count
                .ok_or_else(|| de::Error::missing_field("messages"))?,
        })
    }
}

struct MessagesSeed<'a> {
    connection: &'a Connection,
    conversation_ordinal: i64,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> DeserializeSeed<'de> for MessagesSeed<'_> {
    type Value = i64;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(MessagesVisitor {
            connection: self.connection,
            conversation_ordinal: self.conversation_ordinal,
            is_cancelled: self.is_cancelled,
        })
    }
}

struct MessagesVisitor<'a> {
    connection: &'a Connection,
    conversation_ordinal: i64,
    is_cancelled: &'a dyn Fn() -> bool,
}

impl<'de> Visitor<'de> for MessagesVisitor<'_> {
    type Value = i64;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array of archived messages")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut count = 0_i64;
        let mut previous_index = None;
        loop {
            if (self.is_cancelled)() {
                return Err(de::Error::custom(ARCHIVE_CANCELLED_MESSAGE));
            }
            let Some(message) = sequence.next_element::<ArchivedMessage>()? else {
                break;
            };
            if previous_index.is_some_and(|previous| message.message_index <= previous) {
                return Err(de::Error::custom(
                    "archived messages are not in message order",
                ));
            }
            self.connection
                .execute(
                    "INSERT INTO archive_restore.archive_restore_messages (
                        conversation_ordinal, message_index, message_id, value
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        self.conversation_ordinal,
                        message.message_index,
                        message.message_id,
                        serde_json::to_string(&message.value).map_err(de::Error::custom)?,
                    ],
                )
                .map_err(de::Error::custom)?;
            previous_index = Some(message.message_index);
            count = count
                .checked_add(1)
                .ok_or_else(|| de::Error::custom("archived message count overflow"))?;
        }
        Ok(count)
    }
}

fn create_restore_staging(connection: &Connection) -> StoreResult<tempfile::NamedTempFile> {
    let staging = tempfile::NamedTempFile::new()?;
    connection.execute("ATTACH DATABASE ?1 AS archive_restore", [staging.path().to_string_lossy().as_ref()])?;
    let created = (|| -> StoreResult<()> {
    connection.execute_batch("PRAGMA archive_restore.journal_mode=OFF; PRAGMA archive_restore.synchronous=OFF; PRAGMA archive_restore.cache_size=-2048;")?;
    connection.execute_batch(&format!(
        "DROP TABLE IF EXISTS archive_restore.{RESTORE_MESSAGES};
         DROP TABLE IF EXISTS archive_restore.{RESTORE_CONVERSATIONS};
         CREATE TABLE archive_restore.{RESTORE_CONVERSATIONS} (
            ordinal INTEGER PRIMARY KEY,
            conversation_id TEXT NOT NULL UNIQUE,
            configured_index INTEGER NOT NULL,
            recent_at INTEGER NOT NULL,
            name TEXT NOT NULL,
            message_count INTEGER NOT NULL,
            detail TEXT NOT NULL
         );
         CREATE TABLE archive_restore.{RESTORE_MESSAGES} (
            conversation_ordinal INTEGER NOT NULL,
            message_index INTEGER NOT NULL,
            message_id TEXT,
            value TEXT NOT NULL,
            PRIMARY KEY (conversation_ordinal, message_index)
         );"
    ))?;
    Ok(())
    })();
    if let Err(error) = created {
        let _ = clear_restore_staging(connection);
        return Err(error);
    }
    Ok(staging)
}

fn clear_restore_staging(connection: &Connection) -> StoreResult<()> {
    connection.execute_batch("DETACH DATABASE archive_restore")?;
    Ok(())
}

fn stage_payload(
    connection: &Connection,
    file: std::fs::File,
    is_cancelled: &dyn Fn() -> bool,
) -> StoreResult<StagedArchivePayload> {
    ensure_not_cancelled(is_cancelled)?;
    let decoder = GzDecoder::new(BufReader::new(CancellationAwareIo::new(file, is_cancelled)));
    let mut limited = decoder.take(MAX_DECODED_ARCHIVE_BYTES.saturating_add(1));
    let payload = {
        let mut deserializer = serde_json::Deserializer::from_reader(&mut limited);
        let payload = ArchivePayloadSeed {
            connection,
            is_cancelled,
        }
        .deserialize(&mut deserializer)?;
        deserializer.end()?;
        payload
    };
    if limited.limit() == 0 {
        return validation("archived character exceeds the decoded size limit");
    }
    let mut compressed = limited.into_inner().into_inner();
    if !compressed.fill_buf()?.is_empty() {
        return validation("archived character has trailing compressed data");
    }
    Ok(payload)
}

#[cfg(test)]
mod decoding_tests {
    use super::*;

    fn encoded(json: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(json).unwrap();
        encoder.finish().unwrap()
    }

    const PAYLOAD: &[u8] =
        br#"{"version":1,"characterId":"synthetic","detail":{"name":"Test"},"conversations":[]}"#;

    #[test]
    fn reads_a_buffered_payload_at_the_exact_limit_and_rejects_overflow() {
        let compressed = encoded(PAYLOAD);
        let payload = decompress_bounded(&compressed, PAYLOAD.len() as u64).unwrap();
        assert_eq!(payload.character_id, "synthetic");
        assert_eq!(payload.detail["name"], "Test");
        assert!(decompress_bounded(&compressed, PAYLOAD.len() as u64 - 1).is_err());
        let mut padded = PAYLOAD.to_vec();
        padded.extend_from_slice(b"  ");
        assert!(decompress_bounded(&encoded(&padded), PAYLOAD.len() as u64).is_err());
    }

    #[test]
    fn checks_gzip_integrity_and_rejects_extra_members_and_trailing_json() {
        let compressed = encoded(PAYLOAD);
        assert!(decompress(&compressed[..compressed.len() - 3]).is_err());
        let mut damaged = compressed.clone();
        let checksum = damaged.len() - 8;
        damaged[checksum] ^= 1;
        assert!(decompress(&damaged).is_err());
        assert!(decompress(&[compressed.clone(), compressed].concat()).is_err());
        assert!(decompress(&encoded(&[PAYLOAD, b"{}"].concat())).is_err());
    }

    #[test]
    fn preserves_version_and_shape_validation() {
        let mut value: Value = serde_json::from_slice(PAYLOAD).unwrap();
        value["version"] = Value::from(2);
        assert!(decompress(&encoded(&serde_json::to_vec(&value).unwrap())).is_err());
        assert!(decompress(&encoded(br#"{"version":1}"#)).is_err());
    }
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
    archive_character_with_cancellation(
        connection,
        cas,
        character_id,
        expected_revision,
        now_ms,
        &|| false,
    )
}

pub(super) fn archive_character_with_cancellation(
    connection: &mut Connection,
    cas: &PayloadCas,
    character_id: &str,
    expected_revision: i64,
    now_ms: i64,
    is_cancelled: &dyn Fn() -> bool,
) -> StoreResult<RevisionResult> {
    ensure_not_cancelled(is_cancelled)?;
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
    let asset_hashes = super::snapshot::collect_character_asset_hashes(
        connection,
        cas,
        &generation,
        character_id,
    )?;
    let mut staging = cas.create_ipc_staging_file()?;
    let metadata = {
        let mut encoder = GzEncoder::new(
            CancellationAwareIo::new(staging.as_file_mut(), is_cancelled),
            Compression::default(),
        );
        let metadata = write_payload_to(
            connection,
            &generation,
            character_id,
            &mut encoder,
            is_cancelled,
        )?;
        encoder.finish()?;
        metadata
    };
    ensure_not_cancelled(is_cancelled)?;
    staging.as_file_mut().seek(SeekFrom::Start(0))?;
    let mut reader = CancellationAwareIo::new(staging.as_file_mut(), is_cancelled);
    let prepared = cas.prepare_reader(&mut reader)?;
    ensure_not_cancelled(is_cancelled)?;
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
        conversation_count: metadata.conversation_count,
        message_count: metadata.message_count,
        asset_hashes,
    };
    let encoded = serde_json::to_string(&archived)?;
    let marker = serde_json::to_string(&marker_detail(
        character_id,
        &metadata.name,
        &metadata.character_type,
    ))?;

    super::commit::incremental_commit(
        connection,
        expected_revision,
        |transaction, active| {
            ensure_not_cancelled(is_cancelled)?;
            if is_archived(transaction, active, character_id)? {
                return Err(archived_error(character_id));
            }
            Ok(())
        },
        |transaction, generation, ()| {
            ensure_not_cancelled(is_cancelled)?;
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
    restore_character_with_cancellation(
        connection,
        cas,
        character_id,
        expected_revision,
        &|| false,
    )
}

pub(super) fn restore_character_with_cancellation(
    connection: &mut Connection,
    cas: &PayloadCas,
    character_id: &str,
    expected_revision: i64,
    is_cancelled: &dyn Fn() -> bool,
) -> StoreResult<RevisionResult> {
    ensure_not_cancelled(is_cancelled)?;
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
    let Some(file) = cas.open_object(&archived.object_hash)? else {
        return validation(format!(
            "Archived character {character_id} is missing its stored data"
        ));
    };
    let _staging = create_restore_staging(connection)?;
    let result = (|| {
        let payload = stage_payload(connection, file, is_cancelled)?;
        ensure_not_cancelled(is_cancelled)?;
        if payload.version != ARCHIVE_PAYLOAD_VERSION {
            return validation("archived character payload version is unsupported");
        }
        if payload.character_id != character_id {
            return validation(format!(
                "Archived character {character_id} holds another character"
            ));
        }
        if payload.conversation_count != archived.conversation_count
            || payload.message_count != archived.message_count
        {
            return validation(format!(
                "Archived character {character_id} counts do not match its stored data"
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
                ensure_not_cancelled(is_cancelled)?;
                if !is_archived(transaction, active, character_id)? {
                    return validation(format!("Character {character_id} is not archived"));
                }
                Ok(())
            },
            |transaction, generation, ()| {
                ensure_not_cancelled(is_cancelled)?;
                let inserted_conversations = transaction.execute(
                    "INSERT INTO conversations (
                        generation, character_id, conversation_id, configured_index, recent_at,
                        name, message_count, detail
                     )
                     SELECT ?1, ?2, conversation_id, configured_index, recent_at,
                            name, message_count, detail
                     FROM archive_restore.archive_restore_conversations
                     ",
                    params![generation, character_id],
                )?;
                if inserted_conversations as i64 != payload.conversation_count {
                    return validation("archived character conversation staging is incomplete");
                }
                ensure_not_cancelled(is_cancelled)?;
                let inserted_messages = transaction.execute(
                    "INSERT INTO messages (
                        generation, character_id, conversation_id, message_index,
                        message_id, value
                     )
                     SELECT ?1, ?2, conversations.conversation_id, messages.message_index,
                            messages.message_id, messages.value
                     FROM archive_restore.archive_restore_messages AS messages
                     JOIN archive_restore.archive_restore_conversations AS conversations
                       ON conversations.ordinal = messages.conversation_ordinal
                     ",
                    params![generation, character_id],
                )?;
                if inserted_messages as i64 != payload.message_count {
                    return validation("archived character message staging is incomplete");
                }
                ensure_not_cancelled(is_cancelled)?;
                let updated = transaction.execute(
                    "UPDATE characters
                     SET detail = ?3, archived_object = NULL, conversation_count = ?4, recent_at = ?5
                     WHERE generation = ?1 AND character_id = ?2",
                    params![
                        generation,
                        character_id,
                        detail,
                        payload.conversation_count,
                        recent_at,
                    ],
                )?;
                if updated != 1 {
                    return validation(format!("Character {character_id} does not exist"));
                }
                transaction.execute_batch(
                    "DROP TABLE archive_restore.archive_restore_messages;
                     DROP TABLE archive_restore.archive_restore_conversations;",
                )?;
                Ok(())
            },
        )
    })();
    let cleanup = clear_restore_staging(connection);
    match result {
        Ok(value) => { cleanup?; Ok(value) }
        Err(error) => { let _ = cleanup; Err(error) }
    }
}
