use super::{
    checkpoint_after_detached_release, compare_plugin_storage_keys, RevisionReadLease, StoreError,
    StoreResult,
};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Body, Url};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

fn javascript_array_index(key: &str) -> Option<u32> {
    let value = key.parse::<u32>().ok()?;
    (value < u32::MAX && value.to_string() == key).then_some(value)
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

fn compare_canonical_keys(left: &str, right: &str) -> Ordering {
    match (javascript_array_index(left), javascript_array_index(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => compare_utf16(left, right),
    }
}

fn write_canonical_value(writer: &mut impl Write, value: &Value) -> StoreResult<()> {
    match value {
        Value::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                write_canonical_value(writer, value)?;
            }
            writer.write_all(b"]")?;
        }
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_by(|left, right| compare_canonical_keys(left, right));
            writer.write_all(b"{")?;
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut *writer, key)?;
                writer.write_all(b":")?;
                write_canonical_value(writer, &object[key])?;
            }
            writer.write_all(b"}")?;
        }
        Value::Number(number) => {
            let number = number.as_f64().ok_or_else(|| StoreError::Validation {
                message: "Persistent JSON number is outside the JavaScript range".to_owned(),
            })?;
            writer.write_all(ryu_js::Buffer::new().format(number).as_bytes())?;
        }
        _ => serde_json::to_writer(writer, value)?,
    }
    Ok(())
}

pub(super) struct PreparedKeiUpload {
    database_path: PathBuf,
    output_directory: PathBuf,
    reader: RevisionReadLease,
    revision: i64,
    url: Url,
    token: String,
}

pub(super) struct KeiPayloadFile {
    path: PathBuf,
    bytes: u64,
    armed: bool,
}

impl Drop for KeiPayloadFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl KeiPayloadFile {
    fn cleanup(mut self) -> StoreResult<()> {
        fs::remove_file(&self.path)?;
        self.armed = false;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeiUploadResult {
    pub(crate) revision: i64,
    pub(crate) bytes: u64,
    pub(crate) status: u16,
}

pub(super) fn prepare_upload(
    snapshots_dir: &Path,
    lease: &str,
    reader: RevisionReadLease,
    url: &str,
    expected_account_id: &str,
    token: &str,
) -> Result<PreparedKeiUpload, (StoreError, RevisionReadLease)> {
    let result = (|| -> StoreResult<(PathBuf, PathBuf, Url)> {
        if !lease.starts_with("snapshot-") {
            return Err(StoreError::Validation {
                message: "revision lease must be a snapshot lease".to_owned(),
            });
        }
        let root: String = reader
            .connection
            .query_row(
                "SELECT value FROM root WHERE generation = ?1",
                [&reader.target.generation],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::Validation {
                message: "Pinned generation has no persistent root".to_owned(),
            })?;
        validate_account(&root, expected_account_id, token)?;
        let url = Url::parse(url).map_err(|_| StoreError::Validation {
            message: "KEI backup URL is invalid".to_owned(),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(StoreError::Validation {
                message: "KEI backup URL must use HTTP or HTTPS".to_owned(),
            });
        }
        let persistent_directory = snapshots_dir.parent().ok_or_else(|| StoreError::Store {
            message: "Persistent store directory is unavailable".to_owned(),
        })?;
        reader.publish_detached_asset_roots()?;
        Ok((
            persistent_directory.join("persistent.db"),
            persistent_directory.join("kei-upload"),
            url,
        ))
    })();
    match result {
        Ok((database_path, output_directory, url)) => Ok(PreparedKeiUpload {
            database_path,
            output_directory,
            revision: reader.target.revision,
            reader,
            url,
            token: token.to_owned(),
        }),
        Err(error) => Err((error, reader)),
    }
}

pub(super) fn sweep_abandoned(snapshots_dir: &Path) {
    let Some(persistent_directory) = snapshots_dir.parent() else {
        return;
    };
    let output_directory = persistent_directory.join("kei-upload");
    let Ok(entries) = fs::read_dir(output_directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name
            .strip_prefix("kei-")
            .and_then(|name| name.strip_suffix(".json.tmp"))
        else {
            continue;
        };
        if Uuid::parse_str(id).is_ok() {
            let _ = fs::remove_file(path);
        }
    }
}

fn validate_account(root: &str, expected_account_id: &str, token: &str) -> StoreResult<()> {
    let root: Value = serde_json::from_str(root)?;
    let account = root
        .get("account")
        .and_then(Value::as_object)
        .ok_or_else(|| StoreError::Validation {
            message: "Pinned KEI account is unavailable".to_owned(),
        })?;
    if account.get("kei").and_then(Value::as_bool) != Some(true)
        || account.get("id").and_then(Value::as_str) != Some(expected_account_id)
        || account.get("token").and_then(Value::as_str) != Some(token)
    {
        return Err(StoreError::Validation {
            message: "KEI account changed before the pinned backup".to_owned(),
        });
    }
    Ok(())
}

impl PreparedKeiUpload {
    pub(super) fn create_payload(&self) -> StoreResult<KeiPayloadFile> {
        fs::create_dir_all(&self.output_directory)?;
        let path = self
            .output_directory
            .join(format!("kei-{}.json.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut guard = PayloadOutputGuard::new(path.clone());
        write_payload(
            &self.reader.connection,
            &self.reader.target.generation,
            &self.token,
            &mut file,
        )?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        let bytes = fs::metadata(&path)?.len();
        guard.disarm();
        Ok(KeiPayloadFile {
            path,
            bytes,
            armed: true,
        })
    }

    pub(super) async fn upload(self) -> StoreResult<KeiUploadResult> {
        let revision = self.revision;
        let url = self.url.clone();
        let payload = tokio::task::spawn_blocking(move || {
            let payload = self.create_payload();
            let release =
                checkpoint_after_detached_release_after_close(&self.database_path, self.reader);
            match (payload, release) {
                (Ok(payload), Ok(())) => Ok(payload),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        })
        .await
        .map_err(|_| StoreError::Store {
            message: "KEI payload worker stopped unexpectedly".to_owned(),
        })??;
        let bytes = payload.bytes;
        let upload = upload_payload(&url, &payload).await;
        let cleanup = payload.cleanup();
        let status = match upload {
            Ok(status) => status,
            Err(error) => return Err(error),
        };
        cleanup?;
        Ok(KeiUploadResult {
            revision,
            bytes,
            status,
        })
    }
}

fn checkpoint_after_detached_release_after_close(
    database_path: &Path,
    reader: RevisionReadLease,
) -> StoreResult<()> {
    let active_readers = reader.active_readers();
    super::snapshot::close_revision(reader)?;
    checkpoint_after_detached_release(database_path, &active_readers)
}

async fn upload_payload(url: &Url, payload: &KeiPayloadFile) -> StoreResult<u16> {
    let file = tokio::fs::File::open(&payload.path).await?;
    let body = Body::wrap_stream(ReaderStream::new(file));
    let response = reqwest::Client::new()
        .post(url.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_LENGTH, payload.bytes)
        .body(body)
        .send()
        .await
        .map_err(|error| {
            let summary = if error.is_timeout() {
                "KEI backup upload timed out"
            } else if error.is_connect() {
                "KEI backup endpoint is unavailable"
            } else if error.is_body() {
                "KEI backup payload could not be streamed"
            } else {
                "KEI backup upload failed"
            };
            StoreError::Store {
                message: format!("{summary}: {error}"),
            }
        })?;
    Ok(response.status().as_u16())
}

struct PayloadOutputGuard {
    path: PathBuf,
    armed: bool,
}

impl PayloadOutputGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PayloadOutputGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn write_payload(
    connection: &Connection,
    generation: &str,
    token: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    writer.write_all(b"{\"token\":")?;
    serde_json::to_writer(&mut *writer, token)?;
    writer.write_all(b",\"database\":")?;
    write_database(connection, generation, writer)?;
    writer.write_all(b"}")?;
    Ok(())
}

enum DatabaseField<'a> {
    Root(&'a str),
    Presets,
    Characters,
    PluginStorage,
}

impl DatabaseField<'_> {
    fn key(&self) -> &str {
        match self {
            Self::Root(key) => key,
            Self::Presets => "botPresets",
            Self::Characters => "characters",
            Self::PluginStorage => "pluginCustomStorage",
        }
    }
}

fn write_database(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let root: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "Pinned generation has no persistent root".to_owned(),
        })?;
    let root: Value = serde_json::from_str(&root)?;
    let root = root.as_object().ok_or_else(|| StoreError::Validation {
        message: "Persistent root must be an object".to_owned(),
    })?;
    let mut fields = root
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "botPresets" | "characters" | "pluginCustomStorage"
            )
        })
        .map(|key| DatabaseField::Root(key))
        .collect::<Vec<_>>();
    fields.extend([
        DatabaseField::Presets,
        DatabaseField::Characters,
        DatabaseField::PluginStorage,
    ]);
    fields.sort_by(|left, right| compare_canonical_keys(left.key(), right.key()));

    writer.write_all(b"{")?;
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, field.key())?;
        writer.write_all(b":")?;
        match field {
            DatabaseField::Root(key) => write_canonical_value(writer, &root[key])?,
            DatabaseField::Presets => write_presets(connection, generation, writer)?,
            DatabaseField::Characters => write_characters(connection, generation, writer)?,
            DatabaseField::PluginStorage => write_plugin_storage(connection, generation, writer)?,
        }
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_presets(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM bot_presets WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write_stored_value(writer, &row.get::<_, String>(0)?)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_characters(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT character_id, detail FROM characters
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let character_id = row.get::<_, String>(0)?;
        let detail = row.get::<_, String>(1)?;
        write_character(connection, generation, &character_id, &detail, writer)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_character(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    detail: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let detail: Value = serde_json::from_str(detail)?;
    let detail = detail.as_object().ok_or_else(|| StoreError::Validation {
        message: "Character detail must be an object".to_owned(),
    })?;
    write_object_with_virtual_field(writer, detail, "chats", |writer| {
        write_conversations(connection, generation, character_id, writer)
    })
}

fn write_conversations(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT conversation_id, detail FROM conversations
         WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let conversation_id = row.get::<_, String>(0)?;
        let detail = row.get::<_, String>(1)?;
        write_conversation(
            connection,
            generation,
            character_id,
            &conversation_id,
            &detail,
            writer,
        )?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_conversation(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    detail: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let detail: Value = serde_json::from_str(detail)?;
    let detail = detail.as_object().ok_or_else(|| StoreError::Validation {
        message: "Conversation detail must be an object".to_owned(),
    })?;
    write_object_with_virtual_field(writer, detail, "message", |writer| {
        write_messages(
            connection,
            generation,
            character_id,
            conversation_id,
            writer,
        )
    })
}

fn write_messages(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id, conversation_id])?;
    writer.write_all(b"[")?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        write_stored_value(writer, &row.get::<_, String>(0)?)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_object_with_virtual_field<W, F>(
    writer: &mut W,
    object: &serde_json::Map<String, Value>,
    virtual_key: &str,
    write_virtual: F,
) -> StoreResult<()>
where
    W: Write,
    F: FnOnce(&mut W) -> StoreResult<()>,
{
    let mut keys = object
        .keys()
        .filter(|key| key.as_str() != virtual_key)
        .map(String::as_str)
        .chain(std::iter::once(virtual_key))
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| compare_canonical_keys(left, right));
    writer.write_all(b"{")?;
    let mut write_virtual = Some(write_virtual);
    for (index, key) in keys.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, key)?;
        writer.write_all(b":")?;
        if key == virtual_key {
            write_virtual.take().expect("virtual field is written once")(writer)?;
        } else {
            write_canonical_value(writer, &object[key])?;
        }
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_plugin_storage(
    connection: &Connection,
    generation: &str,
    writer: &mut impl Write,
) -> StoreResult<()> {
    let mut statement = connection
        .prepare("SELECT storage_key, ordinal FROM plugin_storage WHERE generation = ?1")?;
    let mut entries = statement
        .query_map([generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|(left, left_ordinal), (right, right_ordinal)| {
        compare_plugin_storage_keys(left, *left_ordinal, right, *right_ordinal)
    });
    writer.write_all(b"{")?;
    for (index, (key, _)) in entries.into_iter().enumerate() {
        if index > 0 {
            writer.write_all(b",")?;
        }
        serde_json::to_writer(&mut *writer, &key)?;
        writer.write_all(b":")?;
        let value: String = connection.query_row(
            "SELECT value FROM plugin_storage WHERE generation = ?1 AND storage_key = ?2",
            params![generation, key],
            |row| row.get(0),
        )?;
        write_stored_value(writer, &value)?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_stored_value(writer: &mut impl Write, serialized: &str) -> StoreResult<()> {
    let value: Value = serde_json::from_str(serialized)?;
    write_canonical_value(writer, &value)
}

#[cfg(test)]
mod tests {
    use super::write_canonical_value;
    use crate::persistent_store::{AssetAlias, PersistentStore, WorkingSetCommit};
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::mpsc;
    use std::thread;

    const EXPECTED_PAYLOAD: &str = "{\"token\":\"secret-token\",\"database\":{\"account\":{\"data\":{},\"id\":\"account-1\",\"kei\":true,\"token\":\"secret-token\"},\"botPresets\":[{\"a\":1,\"name\":\"preset\",\"z\":2}],\"characters\":[{\"a\":1,\"chaId\":\"char-1\",\"chats\":[{\"id\":\"chat-1\",\"message\":[{\"chatId\":\"message-1\",\"data\":\"hello\",\"role\":\"user\"}],\"name\":\"Chat\",\"note\":\"\"}],\"name\":\"Char\",\"type\":\"character\",\"z\":2}],\"pluginCustomStorage\":{\"2\":\"index\",\"beta\":{\"a\":1,\"z\":2},\"alpha\":\"first\"},\"z\":{\"2\":\"two\",\"10\":\"ten\",\"a\":\"line\\n\",\"b\":2}}}";

    fn open_store_with_fixture() -> (tempfile::TempDir, PersistentStore, String) {
        let directory = tempfile::tempdir().expect("create temp directory");
        let mut store = PersistentStore::open(directory.path()).expect("open store");
        let database = json!({
            "z": { "10": "ten", "2": "two", "b": 2, "a": "line\n" },
            "account": {
                "token": "secret-token",
                "kei": true,
                "id": "account-1",
                "data": {}
            },
            "botPresets": [{ "name": "preset", "z": 2, "a": 1 }],
            "pluginCustomStorage": {
                "beta": { "z": 2, "a": 1 },
                "2": "index",
                "alpha": "first"
            },
            "characters": [{
                "name": "Char",
                "type": "character",
                "chaId": "char-1",
                "chats": [{
                    "name": "Chat",
                    "id": "chat-1",
                    "message": [{ "role": "user", "data": "hello", "chatId": "message-1" }],
                    "note": ""
                }],
                "z": 2,
                "a": 1
            }]
        });
        let staging = store.replace_begin().expect("begin replacement");
        let mut root = database.clone();
        let root = root.as_object_mut().expect("database object");
        let characters = root
            .remove("characters")
            .expect("characters")
            .as_array()
            .expect("character array")
            .clone();
        let presets = root
            .remove("botPresets")
            .expect("presets")
            .as_array()
            .expect("preset array")
            .clone();
        store
            .replace_put_root(
                &staging.staging_id,
                &serde_json::Value::Object(root.clone()),
            )
            .expect("stage root");
        store
            .replace_put_presets(&staging.staging_id, &presets)
            .expect("stage presets");
        store
            .replace_add_characters(&staging.staging_id, &characters)
            .expect("stage characters");
        store
            .replace_put_asset_aliases(
                &staging.staging_id,
                &[AssetAlias {
                    key: "assets/kei-reader.bin".to_owned(),
                    object_hash: Some("ab".repeat(32)),
                    kind: "asset".to_owned(),
                    size: 1,
                    mime: "application/octet-stream".to_owned(),
                    name: "kei-reader.bin".to_owned(),
                    ext: "bin".to_owned(),
                    inlay_type: None,
                    width: None,
                    height: None,
                    metadata: json!({}),
                }],
            )
            .expect("stage KEI reader asset root");
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("commit fixture");
        let lease = store.acquire_revision(1).expect("acquire lease").lease;
        (directory, store, lease)
    }

    #[test]
    fn canonical_writer_matches_javascript_property_order_and_escaping() {
        let value = json!({
            "10": "ten",
            "2": "two",
            "\u{e000}": "bmp",
            "\u{10000}": "supplementary",
            "a": "line\nquote\"",
            "large-decimal": 1e20,
            "large-exponent": 1e21,
            "small-decimal": 1e-6,
            "small-exponent": 1e-7,
            "negative-zero": -0.0,
        });
        let mut bytes = Vec::new();

        write_canonical_value(&mut bytes, &value).expect("write canonical JSON");

        assert_eq!(
            String::from_utf8(bytes).expect("UTF-8 JSON"),
            "{\"2\":\"two\",\"10\":\"ten\",\"a\":\"line\\nquote\\\"\",\"large-decimal\":100000000000000000000,\"large-exponent\":1e+21,\"negative-zero\":0,\"small-decimal\":0.000001,\"small-exponent\":1e-7,\"𐀀\":\"supplementary\",\"\":\"bmp\"}"
        );
    }

    #[test]
    fn pinned_payload_matches_the_current_kei_json_shape_byte_for_byte() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");

        let payload = prepared.create_payload().expect("create payload");
        let path = payload.path.clone();
        let body = fs::read_to_string(&path).expect("read payload");

        assert_eq!(payload.bytes, body.len() as u64);
        assert_eq!(body, EXPECTED_PAYLOAD);
        drop(payload);
        assert!(!path.exists());
    }

    #[test]
    fn transferred_reader_roots_remain_registered_until_prepared_upload_drops() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare transferred reader");

        let roots = store
            .active_readers
            .detached_asset_roots()
            .expect("read detached KEI roots");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].object_hashes, ["ab".repeat(32)].into());

        drop(prepared);
        assert!(store
            .active_readers
            .detached_asset_roots()
            .expect("read detached roots after drop")
            .is_empty());
    }

    #[test]
    fn file_upload_preserves_body_headers_and_ignored_http_status_semantics() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
        let address = listener.local_addr().expect("mock address");
        let (request_tx, request_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("read request");
                assert!(read > 0, "request ended before headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8(request[..header_end].to_vec()).expect("UTF-8 headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content-length header");
            while request.len() - header_end < content_length {
                let read = stream.read(&mut buffer).expect("read body");
                assert!(read > 0, "request ended before body");
                request.extend_from_slice(&buffer[..read]);
            }
            request_tx
                .send((
                    headers,
                    request[header_end..header_end + content_length].to_vec(),
                ))
                .expect("send captured request");
            stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write response");
            stream.flush().expect("flush response");
            stream.shutdown(Shutdown::Write).expect("finish response");
            while stream.read(&mut buffer).expect("drain client close") > 0 {}
        });
        let prepared = store
            .prepare_kei_upload(
                &lease,
                &format!("http://{address}/autobackup/save"),
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let result = runtime.block_on(prepared.upload()).expect("upload payload");
        let (headers, body) = request_rx.recv().expect("receive request");
        server.join().expect("join mock endpoint");

        assert_eq!(result.revision, 1);
        assert_eq!(result.status, 503);
        assert_eq!(result.bytes, body.len() as u64);
        assert!(headers.starts_with("POST /autobackup/save HTTP/1.1\r\n"));
        assert!(headers
            .lines()
            .any(|line| { line.eq_ignore_ascii_case("content-type: application/json") }));
        assert_eq!(body, EXPECTED_PAYLOAD.as_bytes());
        assert!(fs::read_dir(output_directory)
            .expect("read upload directory")
            .next()
            .is_none());
    }

    #[test]
    fn failed_upload_removes_the_temporary_payload_without_exposing_the_token() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve closed endpoint");
        let address = listener.local_addr().expect("closed endpoint address");
        drop(listener);
        let prepared = store
            .prepare_kei_upload(
                &lease,
                &format!("http://{address}/autobackup/save"),
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");

        let error = runtime
            .block_on(prepared.upload())
            .expect_err("closed endpoint must fail");

        let message = error.to_string();
        assert!(
            message.starts_with("KEI backup endpoint is unavailable: "),
            "request source was not preserved: {message}"
        );
        assert!(!message.contains("secret-token"));
        assert!(fs::read_dir(output_directory)
            .expect("read upload directory")
            .next()
            .is_none());
    }

    #[test]
    fn later_commits_do_not_change_the_pinned_payload() {
        let (_directory, mut store, lease) = open_store_with_fixture();
        let mut root = store.read_root(None).expect("read current root").value;
        root["account"]["token"] = json!("new-token");
        root["z"] = json!({ "changed": true });
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: Some(root),
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: None,
            })
            .expect("commit later revision");

        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare pinned upload");
        let payload = prepared.create_payload().expect("create pinned payload");
        let body = fs::read_to_string(&payload.path).expect("read pinned payload");

        assert!(body.contains("\"token\":\"secret-token\""));
        assert!(!body.contains("new-token"));
        assert!(!body.contains("\"changed\":true"));
    }

    #[test]
    fn account_mismatch_is_rejected_without_creating_a_payload() {
        let (_directory, mut store, lease) = open_store_with_fixture();

        let error = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "another-account",
                "secret-token",
            )
            .err()
            .expect("reject account mismatch");

        assert_eq!(
            error.to_string(),
            "KEI account changed before the pinned backup"
        );
    }

    #[test]
    fn startup_removes_only_abandoned_owned_payloads() {
        let (directory, mut store, lease) = open_store_with_fixture();
        let prepared = store
            .prepare_kei_upload(
                &lease,
                "http://127.0.0.1/autobackup/save",
                "account-1",
                "secret-token",
            )
            .expect("prepare upload");
        let output_directory = prepared.output_directory.clone();
        let payload = prepared.create_payload().expect("create payload");
        let payload_path = payload.path.clone();
        let unrelated = output_directory.join("keep-me.txt");
        fs::write(&unrelated, b"keep").expect("write unrelated file");
        std::mem::forget(payload);
        drop(prepared);
        drop(store);

        let reopened = PersistentStore::open(directory.path()).expect("reopen store");

        assert!(!payload_path.exists());
        assert_eq!(fs::read(unrelated).expect("read unrelated file"), b"keep");
        drop(reopened);
    }
}
