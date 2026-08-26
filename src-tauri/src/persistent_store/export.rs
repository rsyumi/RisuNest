use super::{compare_plugin_storage_keys, read_target, StoreError, StoreResult};
use flate2::{write::GzEncoder, Compression, GzBuilder};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[allow(dead_code)]
mod destination;

const RISU_SAVE_HEADER: &[u8] = b"RISUSAVE\0";

const CONFIG: u8 = 0;
const ROOT: u8 = 1;
const CHARACTER_WITH_CHAT: u8 = 2;
const BOT_PRESET: u8 = 4;
const MODULES: u8 = 5;
const PLUGINS: u8 = 9;
const LOADOUTS: u8 = 10;
const PLUGIN_STORAGE: u8 = 11;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportedRisuSave {
    pub(crate) path: String,
    pub(crate) bytes: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportOwnership {
    export_id: String,
    lease: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ManagedFileKind {
    Temporary,
    Completed,
    Ownership,
}

pub(super) fn create(
    connection: &Connection,
    snapshots_dir: &Path,
    lease: &str,
    omit_account: bool,
) -> StoreResult<ExportedRisuSave> {
    let target = read_target(connection, Some(lease))?;
    let exports_dir = export_directory(snapshots_dir)?;
    fs::create_dir_all(&exports_dir)?;
    let id = Uuid::new_v4();
    let temporary_path = exports_dir.join(format!("risusave-{id}.tmp"));
    let final_path = exports_dir.join(format!("risusave-{id}.risudat"));
    let ownership_path = exports_dir.join(format!("risusave-{id}.lease"));
    let mut guard = OutputGuard::new(
        temporary_path.clone(),
        final_path.clone(),
        ownership_path.clone(),
    );
    write_ownership(
        &ownership_path,
        &ExportOwnership {
            export_id: id.to_string(),
            lease: lease.to_owned(),
        },
    )?;

    let root: String = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&target.generation],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "Pinned generation has no persistent root".to_owned(),
        })?;
    let mut root = into_object(
        serde_json::from_str(&root)?,
        "Persistent root must be an object",
    )?;
    root.remove("characters");
    root.remove("botPresets");
    let modules = root.remove("modules");
    let loadouts = root.remove("loadouts");
    let plugins = root.remove("plugins");
    root.remove("pluginCustomStorage");
    let plugin_storage = plugin_storage_value(connection, &target.generation)?;
    if omit_account {
        root.remove("account");
    }

    let character_ids = character_ids(connection, &target.generation)?;
    let mut directory = vec![
        Value::String("preset".to_owned()),
        Value::String("modules".to_owned()),
        Value::String("loadouts".to_owned()),
        Value::String("plugins".to_owned()),
        Value::String("pluginStorage".to_owned()),
    ];
    directory.extend(character_ids.iter().cloned().map(Value::String));
    directory.push(Value::String("config".to_owned()));
    root.insert("__directory".to_owned(), Value::Array(directory));

    let mut file = File::create(&temporary_path)?;
    file.write_all(RISU_SAVE_HEADER)?;
    write_block(&mut file, ROOT, "root", |writer| {
        serde_json::to_writer(writer, &root).map_err(StoreError::from)
    })?;
    write_block(&mut file, BOT_PRESET, "preset", |writer| {
        write_preset_array(connection, &target.generation, writer)
    })?;
    write_optional_value_block(&mut file, MODULES, "modules", modules.as_ref())?;
    write_optional_value_block(&mut file, LOADOUTS, "loadouts", loadouts.as_ref())?;
    write_optional_value_block(&mut file, PLUGINS, "plugins", plugins.as_ref())?;
    write_optional_value_block(
        &mut file,
        PLUGIN_STORAGE,
        "pluginStorage",
        Some(&plugin_storage),
    )?;
    for character_id in &character_ids {
        write_block(&mut file, CHARACTER_WITH_CHAT, character_id, |writer| {
            write_character(connection, &target.generation, character_id, writer)
        })?;
    }
    write_block(&mut file, CONFIG, "config", |writer| {
        serde_json::to_writer(writer, &serde_json::json!({ "version": 1 }))
            .map_err(StoreError::from)
    })?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary_path, &final_path)?;
    let bytes = fs::metadata(&final_path)?.len();
    guard.disarm();

    Ok(ExportedRisuSave {
        path: final_path.to_string_lossy().into_owned(),
        bytes,
    })
}

fn plugin_storage_value(connection: &Connection, generation: &str) -> StoreResult<Value> {
    let mut statement = connection.prepare(
        "SELECT storage_key, value, ordinal FROM plugin_storage
         WHERE generation = ?1",
    )?;
    let mut values = statement
        .query_map([generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .map(|row| {
            let (key, value, ordinal) = row?;
            Ok((key, serde_json::from_str(&value)?, ordinal))
        })
        .collect::<StoreResult<Vec<(String, Value, i64)>>>()?;
    values.sort_by(|(left, _, left_ordinal), (right, _, right_ordinal)| {
        compare_plugin_storage_keys(left, *left_ordinal, right, *right_ordinal)
    });
    Ok(Value::Object(
        values
            .into_iter()
            .map(|(key, value, _)| (key, value))
            .collect::<Map<String, Value>>(),
    ))
}

pub(super) fn cleanup(snapshots_dir: &Path, path: &Path) -> StoreResult<()> {
    let exports_dir = export_directory(snapshots_dir)?;
    let Some((id, ManagedFileKind::Completed)) = managed_file(path) else {
        return Err(StoreError::Validation {
            message: "Native export path is outside the temporary export directory".to_owned(),
        });
    };
    if path.parent() != Some(exports_dir.as_path()) {
        return Err(StoreError::Validation {
            message: "Native export path is outside the temporary export directory".to_owned(),
        });
    }
    let ownership_path = exports_dir.join(format!("risusave-{id}.lease"));
    let mut primary_error = remove_file_if_exists(path).err();
    if let Err(error) = remove_file_if_exists(&ownership_path) {
        if primary_error.is_none() {
            primary_error = Some(error);
        }
    }
    match primary_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(feature = "official-publication-upload-pilot")]
pub(super) fn open_for_upload(
    connection: &Connection,
    snapshots_dir: &Path,
    path: &Path,
) -> StoreResult<(File, u64)> {
    let exports_dir = export_directory(snapshots_dir)?;
    let Some((id, ManagedFileKind::Completed)) = managed_file(path) else {
        return Err(StoreError::Validation {
            message: "Official publication source is not a managed RisuSave export".to_owned(),
        });
    };
    if path.parent() != Some(exports_dir.as_path()) {
        return Err(StoreError::Validation {
            message: "Official publication source is outside the export directory".to_owned(),
        });
    }
    let source_metadata = fs::symlink_metadata(path)?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(StoreError::Validation {
            message: "Official publication source is not a regular export file".to_owned(),
        });
    }
    let ownership_path = exports_dir.join(format!("risusave-{id}.lease"));
    let ownership_metadata = fs::symlink_metadata(&ownership_path)?;
    if ownership_metadata.file_type().is_symlink()
        || !ownership_metadata.is_file()
        || ownership_metadata.len() > 4096
    {
        return Err(StoreError::Validation {
            message: "Official publication source has no valid ownership marker".to_owned(),
        });
    }
    let ownership: ExportOwnership = serde_json::from_slice(&fs::read(&ownership_path)?)?;
    if ownership.export_id != id {
        return Err(StoreError::Validation {
            message: "Official publication source ownership does not match its export".to_owned(),
        });
    }
    read_target(connection, Some(&ownership.lease))?;
    Ok((File::open(path)?, source_metadata.len()))
}

pub(super) fn sweep_abandoned(
    connection: &mut Connection,
    snapshots_dir: &Path,
) -> StoreResult<()> {
    let exports_dir = export_directory(snapshots_dir)?;
    if !exports_dir.is_dir() {
        return Ok(());
    }

    let mut managed_paths = Vec::new();
    let mut ownership = Vec::new();
    for entry in fs::read_dir(&exports_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some((id, kind)) = managed_file(&path) else {
            continue;
        };
        managed_paths.push(path.clone());
        if kind != ManagedFileKind::Ownership || !entry.file_type()?.is_file() {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_slice::<ExportOwnership>(&bytes) else {
            continue;
        };
        if marker.export_id != id || read_target(connection, Some(&marker.lease)).is_err() {
            continue;
        }
        ownership.push((path, marker.lease));
    }

    let mut primary_error = None;
    let mut retained_markers = Vec::new();
    for (path, lease) in ownership {
        if let Err(error) = super::snapshot::release_revision(connection, &lease) {
            retained_markers.push(path);
            if primary_error.is_none() {
                primary_error = Some(error);
            }
        }
    }
    for path in managed_paths {
        if retained_markers.contains(&path) {
            continue;
        }
        if let Err(error) = remove_file_if_exists(&path) {
            if primary_error.is_none() {
                primary_error = Some(error);
            }
        }
    }
    match primary_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn export_directory(snapshots_dir: &Path) -> StoreResult<PathBuf> {
    snapshots_dir
        .parent()
        .map(|parent| parent.join("exports"))
        .ok_or_else(|| StoreError::Validation {
            message: "Persistent export directory is unavailable".to_owned(),
        })
}

struct OutputGuard {
    temporary_path: PathBuf,
    final_path: PathBuf,
    ownership_path: PathBuf,
    armed: bool,
}

impl OutputGuard {
    fn new(temporary_path: PathBuf, final_path: PathBuf, ownership_path: PathBuf) -> Self {
        Self {
            temporary_path,
            final_path,
            ownership_path,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = fs::remove_file(&self.temporary_path);
        let _ = fs::remove_file(&self.final_path);
        let _ = fs::remove_file(&self.ownership_path);
    }
}

fn write_ownership(path: &Path, ownership: &ExportOwnership) -> StoreResult<()> {
    let mut file = File::create(path)?;
    serde_json::to_writer(&mut file, ownership)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn managed_file(path: &Path) -> Option<(String, ManagedFileKind)> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_prefix("risusave-")?;
    let (id, kind) = if let Some(id) = name.strip_suffix(".tmp") {
        (id, ManagedFileKind::Temporary)
    } else if let Some(id) = name.strip_suffix(".risudat") {
        (id, ManagedFileKind::Completed)
    } else if let Some(id) = name.strip_suffix(".lease") {
        (id, ManagedFileKind::Ownership)
    } else {
        return None;
    };
    let parsed = Uuid::parse_str(id).ok()?;
    if parsed.hyphenated().to_string() != id {
        return None;
    }
    Some((id.to_owned(), kind))
}

fn remove_file_if_exists(path: &Path) -> StoreResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_block(
    file: &mut File,
    block_type: u8,
    name: &str,
    write_json: impl FnOnce(&mut dyn Write) -> StoreResult<()>,
) -> StoreResult<()> {
    let name = name.as_bytes();
    let name_length = u8::try_from(name.len()).map_err(|_| StoreError::Validation {
        message: "RisuSave block name exceeds 255 bytes".to_owned(),
    })?;
    file.write_all(&[block_type, 1, name_length])?;
    file.write_all(name)?;
    let length_position = file.stream_position()?;
    file.write_all(&0u32.to_le_bytes())?;
    let data_position = file.stream_position()?;
    {
        let mut encoder: GzEncoder<&mut File> = GzBuilder::new()
            .mtime(0)
            .write(file, Compression::default());
        write_json(&mut encoder)?;
        encoder.try_finish()?;
    }
    let end_position = file.stream_position()?;
    let length =
        u32::try_from(end_position - data_position).map_err(|_| StoreError::Validation {
            message: "RisuSave block exceeds the 4 GiB wire limit".to_owned(),
        })?;
    file.seek(SeekFrom::Start(length_position))?;
    file.write_all(&length.to_le_bytes())?;
    file.seek(SeekFrom::Start(end_position))?;
    Ok(())
}

fn write_optional_value_block(
    file: &mut File,
    block_type: u8,
    name: &str,
    value: Option<&Value>,
) -> StoreResult<()> {
    write_block(file, block_type, name, |writer| match value {
        Some(value) => serde_json::to_writer(writer, value).map_err(StoreError::from),
        None => Ok(()),
    })
}

fn character_ids(connection: &Connection, generation: &str) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT character_id FROM characters
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let ids = statement
        .query_map([generation], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    Ok(ids)
}

fn write_preset_array(
    connection: &Connection,
    generation: &str,
    writer: &mut dyn Write,
) -> StoreResult<()> {
    writer.write_all(b"[")?;
    let mut statement = connection.prepare(
        "SELECT value FROM bot_presets
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let serialized: String = row.get(0)?;
        let value: Value = serde_json::from_str(&serialized)?;
        serde_json::to_writer(&mut *writer, &value)?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_character(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    writer: &mut dyn Write,
) -> StoreResult<()> {
    let detail: String = connection
        .query_row(
            "SELECT detail FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: format!("Pinned character {character_id} is missing"),
        })?;
    let mut character = into_object(
        serde_json::from_str(&detail)?,
        "Character detail must be an object",
    )?;
    character.remove("chats");
    write_object_with_array(writer, character, "chats", |writer| {
        write_conversations(connection, generation, character_id, writer)
    })
}

fn write_conversations(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    writer: &mut dyn Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT conversation_id, detail FROM conversations
         WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let conversation_id: String = row.get(0)?;
        let serialized: String = row.get(1)?;
        let mut conversation = into_object(
            serde_json::from_str(&serialized)?,
            "Conversation detail must be an object",
        )?;
        conversation.remove("message");
        write_object_with_array(writer, conversation, "message", |writer| {
            write_messages(
                connection,
                generation,
                character_id,
                &conversation_id,
                writer,
            )
        })?;
    }
    Ok(())
}

fn write_messages(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    writer: &mut dyn Write,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id, conversation_id])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        let serialized: String = row.get(0)?;
        let value: Value = serde_json::from_str(&serialized)?;
        serde_json::to_writer(&mut *writer, &value)?;
    }
    Ok(())
}

fn write_object_with_array(
    writer: &mut dyn Write,
    object: Map<String, Value>,
    array_name: &str,
    write_items: impl FnOnce(&mut dyn Write) -> StoreResult<()>,
) -> StoreResult<()> {
    writer.write_all(b"{")?;
    let mut first = true;
    for (key, value) in object {
        if !first {
            writer.write_all(b",")?;
        }
        first = false;
        serde_json::to_writer(&mut *writer, &key)?;
        writer.write_all(b":")?;
        serde_json::to_writer(&mut *writer, &value)?;
    }
    if !first {
        writer.write_all(b",")?;
    }
    serde_json::to_writer(&mut *writer, array_name)?;
    writer.write_all(b":[")?;
    write_items(writer)?;
    writer.write_all(b"]}")?;
    Ok(())
}

fn into_object(value: Value, message: &str) -> StoreResult<Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| StoreError::Validation {
            message: message.to_owned(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::PersistentStore;
    use flate2::read::GzDecoder;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::Read;
    use tempfile::TempDir;

    const HEADER: &[u8] = b"RISUSAVE\0";

    #[derive(Debug)]
    struct Block {
        block_type: u8,
        name: String,
        value: Value,
    }

    fn fixture() -> (TempDir, PersistentStore, i64, String) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": "Native Export",
                    "account": { "token": "secret" },
                    "modules": [{ "name": "Module" }],
                    "loadouts": [{ "name": "Loadout" }],
                    "plugins": [{ "name": "Plugin" }],
                    "pluginCustomStorage": { "plugin": { "enabled": true } }
                }),
            )
            .unwrap();
        store
            .replace_put_presets(
                &staging,
                &[json!({ "name": "Preset A" }), json!({ "name": "Preset B" })],
            )
            .unwrap();
        store
            .replace_add_characters(
                &staging,
                &[
                    json!({
                        "type": "character",
                        "chaId": "trash-first",
                        "name": "Trash First",
                        "trashTime": 10,
                        "chats": [{
                            "id": "trash-chat",
                            "name": "Trash Chat",
                            "message": [{ "role": "user", "data": "trash", "chatId": "t1" }]
                        }]
                    }),
                    json!({
                        "type": "character",
                        "chaId": "live-second",
                        "name": "Live Second",
                        "chats": [{
                            "id": "live-chat",
                            "name": "Live Chat",
                            "message": [
                                { "role": "user", "data": "hello", "chatId": "l1" },
                                { "role": "char", "data": "world", "chatId": "l2" }
                            ]
                        }]
                    }),
                ],
            )
            .unwrap();
        let revision = store.replace_commit(&staging, None).unwrap().revision;
        let lease = store.acquire_revision(revision).unwrap().lease;
        (directory, store, revision, lease)
    }

    fn read_blocks(path: &Path) -> Vec<Block> {
        let bytes = fs::read(path).unwrap();
        assert_eq!(&bytes[..HEADER.len()], HEADER);
        let mut offset = HEADER.len();
        let mut blocks = Vec::new();
        while offset < bytes.len() {
            let block_type = bytes[offset];
            assert_eq!(bytes[offset + 1], 1);
            let name_length = bytes[offset + 2] as usize;
            offset += 3;
            let name = String::from_utf8(bytes[offset..offset + name_length].to_vec()).unwrap();
            offset += name_length;
            let data_length =
                u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            let mut decoder = GzDecoder::new(&bytes[offset..offset + data_length]);
            let mut json = String::new();
            decoder.read_to_string(&mut json).unwrap();
            offset += data_length;
            blocks.push(Block {
                block_type,
                name,
                value: serde_json::from_str(&json).unwrap(),
            });
        }
        blocks
    }

    #[test]
    fn exports_current_framing_and_configured_trash_order_from_a_lease() {
        let (_directory, store, _revision, lease) = fixture();

        let exported = create(&store.connection, &store.snapshots_dir, &lease, true).unwrap();
        let blocks = read_blocks(Path::new(&exported.path));

        assert_eq!(
            blocks
                .iter()
                .map(|block| (block.block_type, block.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "root"),
                (4, "preset"),
                (5, "modules"),
                (10, "loadouts"),
                (9, "plugins"),
                (11, "pluginStorage"),
                (2, "trash-first"),
                (2, "live-second"),
                (0, "config"),
            ],
        );
        assert!(blocks[0].value.get("account").is_none());
        assert_eq!(
            blocks[0].value["__directory"],
            json!([
                "preset",
                "modules",
                "loadouts",
                "plugins",
                "pluginStorage",
                "trash-first",
                "live-second",
                "config"
            ]),
        );
        assert_eq!(
            blocks[1].value,
            json!([{ "name": "Preset A" }, { "name": "Preset B" }])
        );
        assert_eq!(blocks[5].value, json!({ "plugin": { "enabled": true } }));
        assert_eq!(blocks[6].value["chats"][0]["message"][0]["data"], "trash");
        assert_eq!(blocks[7].value["chats"][0]["message"][1]["data"], "world");
        assert_eq!(exported.bytes, fs::metadata(&exported.path).unwrap().len());
    }

    #[test]
    fn exports_deterministic_bytes_and_cleans_only_managed_files() {
        let (directory, store, _revision, lease) = fixture();

        let first = create(&store.connection, &store.snapshots_dir, &lease, false).unwrap();
        let second = create(&store.connection, &store.snapshots_dir, &lease, false).unwrap();
        assert_eq!(
            fs::read(&first.path).unwrap(),
            fs::read(&second.path).unwrap()
        );
        assert_eq!(
            read_blocks(Path::new(&first.path))[0].value["account"]["token"],
            "secret"
        );

        let first_ownership = PathBuf::from(&first.path).with_extension("lease");
        assert!(first_ownership.is_file());
        cleanup(&store.snapshots_dir, Path::new(&first.path)).unwrap();
        assert!(!Path::new(&first.path).exists());
        assert!(!first_ownership.exists());
        cleanup(&store.snapshots_dir, Path::new(&first.path)).unwrap();
        assert!(cleanup(
            &store.snapshots_dir,
            &directory.path().join("unmanaged.risudat")
        )
        .is_err());
        cleanup(&store.snapshots_dir, Path::new(&second.path)).unwrap();
    }

    #[cfg(feature = "official-publication-upload-pilot")]
    #[test]
    fn opens_only_a_managed_completed_export_with_its_live_lease() {
        let (directory, mut store, _revision, lease) = fixture();
        let exported = create(&store.connection, &store.snapshots_dir, &lease, false).unwrap();

        let (mut source, bytes) = open_for_upload(
            &store.connection,
            &store.snapshots_dir,
            Path::new(&exported.path),
        )
        .unwrap();
        let mut body = Vec::new();
        source.read_to_end(&mut body).unwrap();
        assert_eq!(bytes, exported.bytes);
        assert_eq!(body.len() as u64, exported.bytes);

        let external = directory
            .path()
            .join(Path::new(&exported.path).file_name().unwrap());
        fs::copy(&exported.path, &external).unwrap();
        assert!(open_for_upload(&store.connection, &store.snapshots_dir, &external).is_err());

        store.release_revision(&lease).unwrap();
        assert!(open_for_upload(
            &store.connection,
            &store.snapshots_dir,
            Path::new(&exported.path),
        )
        .is_err());
        cleanup(&store.snapshots_dir, Path::new(&exported.path)).unwrap();
    }

    #[test]
    fn exports_plugin_storage_in_legacy_object_key_order() {
        use crate::persistent_store::{PluginStorageMutation, WorkingSetCommit};

        let (_directory, mut store, _revision, initial_lease) = fixture();
        store.release_revision(&initial_lease).unwrap();
        let committed = store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: Some(vec![
                    PluginStorageMutation::Clear,
                    PluginStorageMutation::Set {
                        key: "zeta".to_owned(),
                        value: json!("first string"),
                    },
                    PluginStorageMutation::Set {
                        key: "10".to_owned(),
                        value: json!("ten"),
                    },
                    PluginStorageMutation::Set {
                        key: "2".to_owned(),
                        value: json!(0),
                    },
                    PluginStorageMutation::Set {
                        key: "01".to_owned(),
                        value: json!("non-index"),
                    },
                    PluginStorageMutation::Set {
                        key: "4294967294".to_owned(),
                        value: json!(true),
                    },
                    PluginStorageMutation::Set {
                        key: "4294967295".to_owned(),
                        value: json!(false),
                    },
                    PluginStorageMutation::Set {
                        key: "\u{ffff}x".to_owned(),
                        value: json!("unicode"),
                    },
                ]),
            })
            .unwrap();
        let lease = store.acquire_revision(committed.revision).unwrap().lease;

        let exported = create(&store.connection, &store.snapshots_dir, &lease, false).unwrap();
        let plugin_storage = &read_blocks(Path::new(&exported.path))
            .into_iter()
            .find(|block| block.block_type == PLUGIN_STORAGE)
            .unwrap()
            .value;

        assert_eq!(
            plugin_storage
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "2",
                "10",
                "4294967294",
                "zeta",
                "01",
                "4294967295",
                "\u{ffff}x",
            ]
        );
        assert_eq!(plugin_storage["2"], json!(0));
    }

    #[test]
    fn removes_partial_output_when_export_fails() {
        let (_directory, store, _revision, lease) = fixture();
        let generation = read_target(&store.connection, Some(&lease))
            .unwrap()
            .generation;
        store
            .connection
            .execute(
                "UPDATE root SET value = '{' WHERE generation = ?1",
                [&generation],
            )
            .unwrap();

        assert!(create(&store.connection, &store.snapshots_dir, &lease, false).is_err());

        let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
        let remaining = fs::read_dir(exports_dir)
            .map(|entries| entries.collect::<Result<Vec<_>, _>>().unwrap())
            .unwrap_or_default();
        assert!(remaining.is_empty());
    }

    #[test]
    fn reopen_reclaims_export_owned_lease_but_preserves_unrelated_fresh_lease() {
        let (directory, mut store, revision, export_lease) = fixture();
        let unrelated_lease = store.acquire_revision(revision).unwrap().lease;
        let exported = create(
            &store.connection,
            &store.snapshots_dir,
            &export_lease,
            false,
        )
        .unwrap();
        let exported_path = PathBuf::from(&exported.path);
        let ownership_path = exported_path.with_extension("lease");
        assert!(ownership_path.is_file());
        assert!(fs::read_to_string(&ownership_path)
            .unwrap()
            .contains(&export_lease));

        drop(store);
        let mut reopened = PersistentStore::open(directory.path()).unwrap();

        assert!(!exported_path.exists());
        assert!(!ownership_path.exists());
        assert!(matches!(
            reopened.read_root(Some(&export_lease)),
            Err(StoreError::SnapshotReleased)
        ));
        assert!(reopened.read_root(Some(&unrelated_lease)).is_ok());
        reopened.release_revision(&unrelated_lease).unwrap();
    }

    #[test]
    fn reopen_removes_only_strictly_named_managed_orphan_files() {
        let (directory, store, _revision, _lease) = fixture();
        let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
        fs::create_dir_all(&exports_dir).unwrap();
        let temporary = exports_dir.join(format!("risusave-{}.tmp", Uuid::new_v4()));
        let completed = exports_dir.join(format!("risusave-{}.risudat", Uuid::new_v4()));
        let corrupt_marker = exports_dir.join(format!("risusave-{}.lease", Uuid::new_v4()));
        let unmanaged = exports_dir.join("risusave-not-a-uuid.risudat");
        fs::write(&temporary, b"partial").unwrap();
        fs::write(&completed, b"complete").unwrap();
        fs::write(&corrupt_marker, b"not valid ownership").unwrap();
        fs::write(&unmanaged, b"keep").unwrap();

        drop(store);
        let _reopened = PersistentStore::open(directory.path()).unwrap();

        assert!(!temporary.exists());
        assert!(!completed.exists());
        assert!(!corrupt_marker.exists());
        assert!(unmanaged.exists());
    }
}
