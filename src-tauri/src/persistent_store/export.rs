use super::owner_projection::OwnerManifestProjector;
use super::{
    compare_plugin_storage_keys, AssetAlias, AssetOwnerHead, AssetOwnerLocator,
    AssetRepositoryAuthorityState, ReadTarget, StoreError,
    StoreResult,
};
use crate::asset_repository::owner_manifest_codec::OwnerManifestEntry;
use flate2::{Compression, GzBuilder};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::{self, File};
#[cfg(feature = "native-official-publication")]
use std::io::Read;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) mod destination;

pub(crate) const EXPORT_CANCELLED_MESSAGE: &str = "native RisuSave export cancelled";

pub(crate) struct ProjectedCharacter {
    pub(crate) value: Value,
    pub(crate) additional_asset_entries: Option<Vec<OwnerManifestEntry>>,
}

pub(crate) struct ProjectedRootModule {
    pub(crate) value: Value,
    pub(crate) asset_entries: Option<Vec<OwnerManifestEntry>>,
}

pub(crate) struct PinnedLegacyBackupInventory {
    pub(crate) revision: i64,
    pub(crate) assets: Vec<AssetAlias>,
    pub(crate) owner_heads: Vec<AssetOwnerHead>,
    pub(crate) asset_authority: AssetRepositoryAuthorityState,
}

pub(crate) fn pinned_legacy_backup_inventory(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<PinnedLegacyBackupInventory> {
    Ok(PinnedLegacyBackupInventory {
        revision: target.revision,
        assets: super::query::list_asset_aliases(connection, target)?.value,
        owner_heads: super::query::list_asset_owner_heads(connection, target)?.value,
        asset_authority: super::query::read_asset_repository_authority(connection, target)?.value,
    })
}

pub(crate) fn projected_character(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    character_id: &str,
) -> StoreResult<ProjectedCharacter> {
    let mut character = super::query::read_character(connection, character_id, target)?
        .ok_or_else(|| StoreError::Validation {
            message: "pinned character does not exist".to_owned(),
        })?
        .value;
    let object = character
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "pinned character must be an object".to_owned(),
        })?;
    let additional_asset_entries =
        OwnerManifestProjector::from_snapshots_dir(connection, target, snapshots_dir)?
            .project_character(character_id, object)?;
    Ok(ProjectedCharacter {
        value: character,
        additional_asset_entries,
    })
}

pub(crate) fn projected_root_module(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    module_index: u64,
) -> StoreResult<ProjectedRootModule> {
    let root = super::query::read_root(connection, target)?.value;
    let index = usize::try_from(module_index).map_err(|_| StoreError::Validation {
        message: "pinned root module index does not fit this platform".to_owned(),
    })?;
    let mut module = root
        .get("modules")
        .and_then(Value::as_array)
        .and_then(|modules| modules.get(index))
        .cloned()
        .ok_or_else(|| StoreError::Validation {
            message: "pinned root module does not exist".to_owned(),
        })?;
    let object = module
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "pinned root module must be an object".to_owned(),
        })?;
    let asset_entries =
        OwnerManifestProjector::from_snapshots_dir(connection, target, snapshots_dir)?
            .project_root_module(module_index, object)?;
    Ok(ProjectedRootModule {
        value: module,
        asset_entries,
    })
}

pub(crate) fn pinned_asset_alias(
    connection: &Connection,
    target: &ReadTarget,
    key: &str,
) -> StoreResult<super::AssetAlias> {
    super::query::read_asset_alias(connection, "asset", key, target)?
        .ok_or_else(|| StoreError::Validation {
            message: format!("pinned character asset alias is missing: {key}"),
        })
        .map(|versioned| versioned.value)
}

const RISU_SAVE_HEADER: &[u8] = b"RISUSAVE\0";

const CONFIG: u8 = 0;
const ROOT: u8 = 1;
const CHARACTER_WITH_CHAT: u8 = 2;
const BOT_PRESET: u8 = 4;
const MODULES: u8 = 5;
const PLUGINS: u8 = 9;
const LOADOUTS: u8 = 10;
const PLUGIN_STORAGE: u8 = 11;
const PLUGIN_STORAGE_META: u8 = 12;
#[cfg(feature = "native-official-publication")]
const MAX_EXPORT_OWNERSHIP_BYTES: u64 = 4096;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportedRisuSave {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    pub(crate) excluded_archived_character_count: u64,
    pub(crate) excluded_colliding_plugin_value_count: u64,
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
    target: &ReadTarget,
    lease: &str,
    omit_account: bool,
) -> StoreResult<ExportedRisuSave> {
    create_controlled(
        connection,
        snapshots_dir,
        target,
        lease,
        omit_account,
        || false,
        |_, _, _| {},
    )
}

pub(crate) fn create_controlled(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    lease: &str,
    omit_account: bool,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(u64, u64, u64),
) -> StoreResult<ExportedRisuSave> {
    create_controlled_inner(
        connection,
        snapshots_dir,
        target,
        lease,
        omit_account,
        None,
        None,
        None,
        is_cancelled,
        on_progress,
    )
}

pub(crate) fn create_legacy_backup_controlled(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    lease: &str,
    replacement_keys: HashMap<AssetOwnerLocator, Vec<String>>,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(u64, u64, u64),
) -> StoreResult<ExportedRisuSave> {
    create_controlled_inner(
        connection,
        snapshots_dir,
        target,
        lease,
        true,
        None,
        None,
        Some(replacement_keys),
        is_cancelled,
        on_progress,
    )
}

pub(crate) fn create_projected_controlled(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    lease: &str,
    omit_account: bool,
    replacements: &HashMap<String, String>,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(u64, u64, u64),
) -> StoreResult<ExportedRisuSave> {
    create_controlled_inner(
        connection,
        snapshots_dir,
        target,
        lease,
        omit_account,
        Some(replacements),
        None,
        None,
        is_cancelled,
        on_progress,
    )
}

#[cfg(feature = "native-official-publication")]
pub(crate) fn create_projected_controlled_for_account(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    lease: &str,
    expected_account_id: &str,
    replacements: &HashMap<String, String>,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(u64, u64, u64),
) -> StoreResult<ExportedRisuSave> {
    create_controlled_inner(
        connection,
        snapshots_dir,
        target,
        lease,
        false,
        Some(replacements),
        Some(expected_account_id),
        None,
        is_cancelled,
        on_progress,
    )
}

fn create_controlled_inner(
    connection: &Connection,
    snapshots_dir: &Path,
    target: &ReadTarget,
    lease: &str,
    omit_account: bool,
    replacements: Option<&HashMap<String, String>>,
    expected_account_id: Option<&str>,
    owner_replacement_keys: Option<HashMap<AssetOwnerLocator, Vec<String>>>,
    is_cancelled: impl Fn() -> bool,
    mut on_progress: impl FnMut(u64, u64, u64),
) -> StoreResult<ExportedRisuSave> {
    check_export_cancelled(&is_cancelled)?;
    let owner_projector = match owner_replacement_keys {
        Some(replacement_keys) => OwnerManifestProjector::from_snapshots_dir_with_replacement_keys(
            connection,
            target,
            snapshots_dir,
            replacement_keys,
        )?,
        None => OwnerManifestProjector::from_snapshots_dir(connection, target, snapshots_dir)?,
    };
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
    if let Some(expected_account_id) = expected_account_id {
        validate_pinned_account(&root, expected_account_id)?;
    }
    owner_projector.project_root(&mut root)?;
    if let Some(replacements) = replacements {
        project_root_resources(&mut root, replacements);
    }
    root.shift_remove("characters");
    root.shift_remove("botPresets");
    let modules = take_root_block_value(&mut root, "modules");
    let loadouts = take_root_block_value(&mut root, "loadouts");
    let plugins = take_root_block_value(&mut root, "plugins");
    root.shift_remove("pluginCustomStorage");
    let flattened = flattened_plugin_storage(connection, &target.generation)?;
    let excluded_colliding_plugin_value_count = flattened.collisions.len() as u64;
    let plugin_storage_meta = plugin_storage_meta_value(&flattened.owners);
    if omit_account {
        root.shift_remove("account");
    }

    let character_ids = character_ids(connection, &target.generation)?;
    let excluded_archived_character_count =
        super::archive::archived_character_ids(connection, &target.generation)?.len() as u64;
    owner_projector.validate_character_owners(&character_ids.iter().cloned().collect())?;
    let preset_count = preset_count(connection, &target.generation)?;
    let total_items = character_ids.len() as u64 + 8;
    let mut completed_items = 0u64;
    let mut directory = vec![
        Value::String("preset".to_owned()),
        Value::String("modules".to_owned()),
        Value::String("loadouts".to_owned()),
        Value::String("plugins".to_owned()),
        Value::String("pluginStorage".to_owned()),
        Value::String("pluginStorageMeta".to_owned()),
    ];
    directory.extend(character_ids.iter().cloned().map(Value::String));
    directory.push(Value::String("config".to_owned()));
    root.insert("__directory".to_owned(), Value::Array(directory));

    let mut file = File::create(&temporary_path)?;
    file.write_all(RISU_SAVE_HEADER)?;
    on_progress(file.stream_position()?, completed_items, total_items);
    check_export_cancelled(&is_cancelled)?;
    write_block(&mut file, ROOT, "root", &is_cancelled, |writer| {
        serde_json::to_writer(writer, &root).map_err(StoreError::from)
    })?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_block(&mut file, BOT_PRESET, "preset", &is_cancelled, |writer| {
        write_preset_array(connection, &target.generation, writer, &is_cancelled)
    })?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_optional_value_block(
        &mut file,
        MODULES,
        "modules",
        modules.as_ref(),
        &is_cancelled,
    )?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_optional_value_block(
        &mut file,
        LOADOUTS,
        "loadouts",
        loadouts.as_ref(),
        &is_cancelled,
    )?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_optional_value_block(
        &mut file,
        PLUGINS,
        "plugins",
        plugins.as_ref(),
        &is_cancelled,
    )?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_block(&mut file, PLUGIN_STORAGE, "pluginStorage", &is_cancelled, |writer| {
        write_plugin_storage(
            connection, &target.generation, &flattened.rows, replacements, writer, &is_cancelled,
        )
    })?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    write_optional_value_block(
        &mut file,
        PLUGIN_STORAGE_META,
        "pluginStorageMeta",
        Some(&plugin_storage_meta),
        &is_cancelled,
    )?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    for character_id in &character_ids {
        check_export_cancelled(&is_cancelled)?;
        write_block(
            &mut file,
            CHARACTER_WITH_CHAT,
            character_id,
            &is_cancelled,
            |writer| {
                write_character(
                    connection,
                    &target.generation,
                    character_id,
                    &owner_projector,
                    replacements,
                    writer,
                    &is_cancelled,
                )
            },
        )?;
        report_export_progress(
            &mut file,
            &mut completed_items,
            total_items,
            &mut on_progress,
        )?;
    }
    check_export_cancelled(&is_cancelled)?;
    write_block(&mut file, CONFIG, "config", &is_cancelled, |writer| {
        serde_json::to_writer(writer, &serde_json::json!({ "version": 1 }))
            .map_err(StoreError::from)
    })?;
    report_export_progress(
        &mut file,
        &mut completed_items,
        total_items,
        &mut on_progress,
    )?;
    check_export_cancelled(&is_cancelled)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary_path, &final_path)?;
    let bytes = fs::metadata(&final_path)?.len();
    guard.disarm();

    Ok(ExportedRisuSave {
        path: final_path.to_string_lossy().into_owned(),
        bytes,
        character_count: character_ids.len() as u64,
        preset_count,
        excluded_archived_character_count,
        excluded_colliding_plugin_value_count,
    })
}

fn check_export_cancelled(is_cancelled: &impl Fn() -> bool) -> StoreResult<()> {
    if is_cancelled() {
        Err(StoreError::Validation {
            message: EXPORT_CANCELLED_MESSAGE.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn report_export_progress(
    file: &mut File,
    completed_items: &mut u64,
    total_items: u64,
    on_progress: &mut impl FnMut(u64, u64, u64),
) -> StoreResult<()> {
    let bytes = file.stream_position()?;
    *completed_items += 1;
    on_progress(bytes, *completed_items, total_items);
    Ok(())
}

fn preset_count(connection: &Connection, generation: &str) -> StoreResult<u64> {
    let count = connection.query_row(
        "SELECT COUNT(*) FROM bot_presets WHERE generation = ?1",
        [generation],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(count).map_err(|_| StoreError::Validation {
        message: "Pinned preset count is invalid".to_owned(),
    })
}

/// Ordered ownership metadata; values are read only when they are consumed.
pub(crate) struct PluginStorageRow {
    pub(crate) owner: String,
    pub(crate) key: String,
    pub(crate) assigned_at: Option<i64>,
}

/// Flattened plugin storage plus the keys that could not be flattened.
pub(crate) struct FlattenedPluginStorage {
    pub(crate) rows: Vec<PluginStorageRow>,
    /// Keys held by more than one plugin, with every owner that holds them.
    pub(crate) collisions: Vec<(String, Vec<String>)>,
    /// Owner of each exported key, for the ownership sidecar.
    pub(crate) owners: Vec<(String, String, Option<i64>)>,
}

/// Legacy key order: numeric keys first, then the recorded position.
pub(crate) fn plugin_storage_rows(
    connection: &Connection,
    generation: &str,
) -> StoreResult<Vec<PluginStorageRow>> {
    let mut statement = connection.prepare(
        "SELECT owner, storage_key, ordinal, assigned_at FROM plugin_storage
         WHERE generation = ?1",
    )?;
    let mut values = statement
        .query_map([generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    values.sort_by(
        |(left_owner, left, left_ordinal, _), (right_owner, right, right_ordinal, _)| {
            compare_plugin_storage_keys(left, *left_ordinal, right, *right_ordinal)
                .then_with(|| left_owner.cmp(right_owner))
        },
    );
    Ok(values
        .into_iter()
        .map(|(owner, key, _, assigned_at)| PluginStorageRow {
            owner,
            key,
            assigned_at,
        })
        .collect())
}

/// Upstream saves hold one value per key. A key two plugins both hold cannot be
/// written without handing one plugin the other plugin's value, so neither side
/// goes out and the caller reports the key.
pub(crate) fn flattened_plugin_storage(
    connection: &Connection,
    generation: &str,
) -> StoreResult<FlattenedPluginStorage> {
    let rows = plugin_storage_rows(connection, generation)?;
    let mut owners_by_key: HashMap<String, Vec<String>> = HashMap::new();
    for row in &rows {
        let entry = owners_by_key.entry(row.key.clone()).or_default();
        if !entry.iter().any(|owner| owner == &row.owner) {
            entry.push(row.owner.clone());
        }
    }
    let mut collisions: Vec<(String, Vec<String>)> = owners_by_key
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|(key, owners)| {
            let mut owners = owners.clone();
            owners.sort();
            (key.clone(), owners)
        })
        .collect();
    collisions.sort_by(|left, right| left.0.cmp(&right.0));
    let mut selected = Vec::new();
    let mut owners = Vec::new();
    for row in rows {
        if owners_by_key
            .get(&row.key)
            .is_some_and(|holders| holders.len() > 1)
        {
            continue;
        }
        owners.push((row.key.clone(), row.owner.clone(), row.assigned_at));
        selected.push(row);
    }
    Ok(FlattenedPluginStorage {
        rows: selected,
        collisions,
        owners,
    })
}

/// The in-app compatibility projection keeps every key. A colliding key keeps
/// the later position so the object stays deterministic.
pub(crate) fn materialized_plugin_storage(
    connection: &Connection,
    generation: &str,
) -> StoreResult<Map<String, Value>> {
    let mut values = Map::new();
    for row in plugin_storage_rows(connection, generation)? {
        let value = read_plugin_storage_value(connection, generation, &row)?;
        values.insert(row.key, value);
    }
    Ok(values)
}

fn read_plugin_storage_value(
    connection: &Connection,
    generation: &str,
    row: &PluginStorageRow,
) -> StoreResult<Value> {
    let value: String = connection.prepare_cached(
        "SELECT value FROM plugin_storage WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
    )?.query_row(params![generation, row.owner, row.key], |row| row.get(0))?;
    Ok(serde_json::from_str(&value)?)
}

fn write_plugin_storage(
    connection: &Connection,
    generation: &str,
    rows: &[PluginStorageRow],
    replacements: Option<&HashMap<String, String>>,
    writer: &mut dyn Write,
    is_cancelled: &impl Fn() -> bool,
) -> StoreResult<()> {
    writer.write_all(b"{")?;
    for (index, row) in rows.iter().enumerate() {
        check_export_cancelled(is_cancelled)?;
        if index != 0 { writer.write_all(b",")?; }
        serde_json::to_writer(&mut *writer, &row.key)?;
        writer.write_all(b":")?;
        let mut value = read_plugin_storage_value(connection, generation, row)?;
        if let Some(replacements) = replacements {
            project_plugin_storage_resources(&mut value, replacements);
        }
        serde_json::to_writer(&mut *writer, &value)?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

/// The ownership sidecar upstream RisuAI ignores and RisuNest reads back.
/// `updatedAt` carries the recorded assignment time so two exports of one
/// revision stay byte identical.
pub(crate) fn plugin_storage_meta_value(owners: &[(String, String, Option<i64>)]) -> Value {
    let mut meta = Map::new();
    for (key, owner, assigned_at) in owners {
        if super::plugin_owner::is_unowned(owner) {
            continue;
        }
        let mut entry = Map::new();
        entry.insert("plugin".to_owned(), Value::String(owner.clone()));
        entry.insert("updatedAt".to_owned(), Value::from(assigned_at.unwrap_or(0)));
        meta.insert(key.clone(), Value::Object(entry));
    }
    Value::Object(meta)
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

#[cfg(feature = "native-official-publication")]
fn open_regular_file_no_follow(path: &Path) -> StoreResult<(File, u64)> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Validation {
            message: "Official publication source is not a regular file".to_owned(),
        });
    }
    Ok((file, metadata.len()))
}

#[cfg(any(
    feature = "official-publication-upload-pilot",
    feature = "native-official-publication"
))]
fn read_bounded_ownership(file: File, size_hint: u64) -> StoreResult<ExportOwnership> {
    let mut bytes = Vec::with_capacity(size_hint.min(MAX_EXPORT_OWNERSHIP_BYTES) as usize);
    file.take(MAX_EXPORT_OWNERSHIP_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_EXPORT_OWNERSHIP_BYTES {
        return Err(StoreError::Validation {
            message: "Official publication source has no valid ownership marker".to_owned(),
        });
    }
    Ok(serde_json::from_slice(&bytes)?)
}

// Shared managed-export open for the two upload paths (both features can be
// enabled together). Returns the opened source with its export id and
// ownership record; each wrapper applies its own lease handling so their
// error messages stay unchanged.
#[cfg(any(
    feature = "official-publication-upload-pilot",
    feature = "native-official-publication"
))]
fn open_managed_export_for_upload(
    snapshots_dir: &Path,
    path: &Path,
) -> StoreResult<(File, u64, String, ExportOwnership)> {
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
    let (source, source_len) = open_regular_file_no_follow(path)?;
    let ownership_path = exports_dir.join(format!("risusave-{id}.lease"));
    let (ownership_file, ownership_len) = open_regular_file_no_follow(&ownership_path)?;
    if ownership_len > MAX_EXPORT_OWNERSHIP_BYTES {
        return Err(StoreError::Validation {
            message: "Official publication source has no valid ownership marker".to_owned(),
        });
    }
    let ownership = read_bounded_ownership(ownership_file, ownership_len)?;
    Ok((source, source_len, id, ownership))
}

#[cfg(feature = "official-publication-upload-pilot")]
pub(super) fn open_for_upload(
    snapshots_dir: &Path,
    path: &Path,
) -> StoreResult<(File, u64, String)> {
    let (source, source_len, id, ownership) = open_managed_export_for_upload(snapshots_dir, path)?;
    if ownership.export_id != id {
        return Err(StoreError::Validation {
            message: "Official publication source ownership does not match its export".to_owned(),
        });
    }
    Ok((source, source_len, ownership.lease))
}

#[cfg(feature = "native-official-publication")]
pub(super) fn open_owned_for_upload(
    snapshots_dir: &Path,
    path: &Path,
    expected_lease: &str,
) -> StoreResult<(File, u64)> {
    let (source, source_len, id, ownership) = open_managed_export_for_upload(snapshots_dir, path)?;
    if ownership.export_id != id || ownership.lease != expected_lease {
        return Err(StoreError::Validation {
            message: "Official publication source ownership does not match its lease".to_owned(),
        });
    }
    Ok((source, source_len))
}

pub(super) fn sweep_abandoned(snapshots_dir: &Path) -> StoreResult<()> {
    let exports_dir = export_directory(snapshots_dir)?;
    if !exports_dir.is_dir() {
        return Ok(());
    }

    let mut managed_paths = Vec::new();
    for entry in fs::read_dir(&exports_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some((_id, _kind)) = managed_file(&path) else {
            continue;
        };
        managed_paths.push(path);
    }

    let mut primary_error = None;
    for path in managed_paths {
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
    is_cancelled: &impl Fn() -> bool,
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
        let checked = CancellationAwareWriter {
            file: &mut *file,
            is_cancelled,
        };
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(checked, Compression::default());
        if let Err(error) = write_json(&mut encoder) {
            check_export_cancelled(is_cancelled)?;
            return Err(error);
        }
        if let Err(error) = encoder.try_finish() {
            check_export_cancelled(is_cancelled)?;
            return Err(error.into());
        }
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
    is_cancelled: &impl Fn() -> bool,
) -> StoreResult<()> {
    write_block(file, block_type, name, is_cancelled, |writer| match value {
        Some(value) => serde_json::to_writer(writer, value).map_err(StoreError::from),
        None => Ok(()),
    })
}

struct CancellationAwareWriter<'a, F: Fn() -> bool> {
    file: &'a mut File,
    is_cancelled: &'a F,
}

impl<F: Fn() -> bool> Write for CancellationAwareWriter<'_, F> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if (self.is_cancelled)() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                EXPORT_CANCELLED_MESSAGE,
            ));
        }
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if (self.is_cancelled)() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                EXPORT_CANCELLED_MESSAGE,
            ));
        }
        self.file.flush()
    }
}

// One enumerator serves the RisuSave export and the official account projection,
// so an archived character cannot reach either as a marker-only shell.
fn character_ids(connection: &Connection, generation: &str) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT character_id FROM characters
         WHERE generation = ?1 AND archived_object IS NULL
         ORDER BY configured_index ASC",
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
    is_cancelled: &impl Fn() -> bool,
) -> StoreResult<()> {
    writer.write_all(b"[")?;
    let mut statement = connection.prepare(
        "SELECT value FROM bot_presets
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query([generation])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        check_export_cancelled(is_cancelled)?;
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
    owner_projector: &OwnerManifestProjector,
    replacements: Option<&HashMap<String, String>>,
    writer: &mut dyn Write,
    is_cancelled: &impl Fn() -> bool,
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
    owner_projector.project_character(character_id, &mut character)?;
    if let Some(replacements) = replacements {
        project_character_resources(&mut character, replacements);
    }
    character.remove("chats");
    write_object_with_array(writer, character, "chats", |writer| {
        write_conversations(connection, generation, character_id, writer, is_cancelled)
    })
}

fn write_conversations(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    writer: &mut dyn Write,
    is_cancelled: &impl Fn() -> bool,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT conversation_id, detail FROM conversations
         WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        check_export_cancelled(is_cancelled)?;
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
                is_cancelled,
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
    is_cancelled: &impl Fn() -> bool,
) -> StoreResult<()> {
    let mut statement = connection.prepare(
        "SELECT value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![generation, character_id, conversation_id])?;
    let mut first = true;
    while let Some(row) = rows.next()? {
        check_export_cancelled(is_cancelled)?;
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

fn project_root_resources(root: &mut Map<String, Value>, replacements: &HashMap<String, String>) {
    replace_mapped_string(root.get_mut("customBackground"), replacements);
    replace_mapped_string(root.get_mut("userIcon"), replacements);

    if let Some(Value::Array(modules)) = root.get_mut("modules") {
        for module in modules {
            let Value::Object(module) = module else {
                continue;
            };
            project_tuple_resources(module.get_mut("assets"), replacements);
            replace_mapped_string(module.get_mut("icon"), replacements);
        }
    }

    if let Some(Value::Array(personas)) = root.get_mut("personas") {
        for persona in personas {
            let Value::Object(persona) = persona else {
                continue;
            };
            replace_mapped_string(persona.get_mut("icon"), replacements);
            let Some(Value::Object(embedded_module)) = persona.get_mut("embeddedModule") else {
                continue;
            };
            project_tuple_resources(embedded_module.get_mut("assets"), replacements);
            replace_mapped_string(embedded_module.get_mut("icon"), replacements);
        }
    }

    if let Some(Value::Array(character_order)) = root.get_mut("characterOrder") {
        for item in character_order {
            let Value::Object(item) = item else {
                continue;
            };
            replace_mapped_string(item.get_mut("imgFile"), replacements);
        }
    }
}

fn project_plugin_storage_resources(value: &mut Value, replacements: &HashMap<String, String>) {
    match value {
        Value::String(source) => {
            let normalized = source.replace('\\', "/");
            if !normalized.starts_with("assets/") || normalized.len() == "assets/".len() {
                return;
            }
            if let Some(replacement) = replacements
                .get(source.as_str())
                .or_else(|| replacements.get(normalized.as_str()))
            {
                source.clone_from(replacement);
            }
        }
        Value::Array(values) => {
            for value in values {
                project_plugin_storage_resources(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                project_plugin_storage_resources(value, replacements);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn project_character_resources(
    character: &mut Map<String, Value>,
    replacements: &HashMap<String, String>,
) {
    replace_mapped_string(character.get_mut("image"), replacements);
    project_tuple_resources(character.get_mut("emotionImages"), replacements);

    if character.get("type").and_then(Value::as_str) == Some("group") {
        return;
    }

    project_tuple_resources(character.get_mut("additionalAssets"), replacements);
    if let Some(Value::Object(vits)) = character.get_mut("vits") {
        if let Some(Value::Object(files)) = vits.get_mut("files") {
            for file in files.values_mut() {
                replace_mapped_string(Some(file), replacements);
            }
        }
    }
    if let Some(Value::Array(assets)) = character.get_mut("ccAssets") {
        for asset in assets {
            let Value::Object(asset) = asset else {
                continue;
            };
            replace_mapped_string(asset.get_mut("uri"), replacements);
        }
    }
}

fn project_tuple_resources(value: Option<&mut Value>, replacements: &HashMap<String, String>) {
    let Some(Value::Array(items)) = value else {
        return;
    };
    for item in items {
        let Value::Array(tuple) = item else {
            continue;
        };
        replace_mapped_string(tuple.get_mut(1), replacements);
    }
}

fn replace_mapped_string(value: Option<&mut Value>, replacements: &HashMap<String, String>) {
    let Some(Value::String(source)) = value else {
        return;
    };
    if source.is_empty() {
        return;
    }
    if let Some(replacement) = replacements.get(source.as_str()) {
        source.clone_from(replacement);
    }
}

fn validate_pinned_account(
    root: &Map<String, Value>,
    expected_account_id: &str,
) -> StoreResult<()> {
    let account_id = root
        .get("account")
        .and_then(Value::as_object)
        .and_then(|account| account.get("id"))
        .and_then(Value::as_str);
    if account_id != Some(expected_account_id) {
        return Err(StoreError::Validation {
            message: "Pinned official publication account does not match the request".to_owned(),
        });
    }
    Ok(())
}

fn into_object(value: Value, message: &str) -> StoreResult<Map<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(StoreError::Validation {
            message: message.to_owned(),
        }),
    }
}

fn take_root_block_value(root: &mut Map<String, Value>, key: &str) -> Option<Value> {
    root.shift_remove(key)
}

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_export_selects_metadata_before_reading_values_and_preserves_legacy_order() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE plugin_storage (generation TEXT, owner TEXT, storage_key TEXT, value TEXT, ordinal INTEGER, assigned_at INTEGER);
             CREATE UNIQUE INDEX plugin_key ON plugin_storage (generation, owner, storage_key);",
        ).unwrap();
        for (owner, key, value, ordinal) in [
            ("a", "z", r#"{"nested":"assets/old.bin"}"#, 0),
            ("a", "10", "10", 1),
            ("a", "2", "2", 2),
            ("a", "shared", "not parsed because it is excluded", 3),
            ("b", "shared", "also excluded", 4),
        ] {
            connection.execute(
                "INSERT INTO plugin_storage VALUES ('g', ?1, ?2, ?3, ?4, 1234)",
                rusqlite::params![owner, key, value, ordinal],
            ).unwrap();
        }
        let selected = super::flattened_plugin_storage(&connection, "g").unwrap();
        assert_eq!(selected.collisions.len(), 1);
        assert_eq!(selected.rows.iter().map(|row| row.key.as_str()).collect::<Vec<_>>(), ["2", "10", "z"]);
        let replacements = std::collections::HashMap::from([("assets/old.bin".to_owned(), "assets/new.bin".to_owned())]);
        let mut output = Vec::new();
        super::write_plugin_storage(&connection, "g", &selected.rows, Some(&replacements), &mut output, &|| false).unwrap();
        assert_eq!(output, br#"{"2":2,"10":10,"z":{"nested":"assets/new.bin"}}"#);
        assert!(super::write_plugin_storage(&connection, "g", &selected.rows, None, &mut Vec::new(), &|| true).is_err());
        connection.execute("UPDATE plugin_storage SET value = 'invalid' WHERE storage_key = 'z'", []).unwrap();
        assert!(super::write_plugin_storage(&connection, "g", &selected.rows, None, &mut Vec::new(), &|| false).is_err());
    }

    use super::super::plugin_owner::UNOWNED_OWNER;
    use super::*;
    use crate::asset_repository::{owner_manifest_codec, PayloadCas};
    use crate::persistent_store::{
        AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState, PersistentStore,
    };
    use flate2::read::GzDecoder;
    use serde_json::{json, Value};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::fs;
    use std::io::Read;
    use tempfile::TempDir;

    const HEADER: &[u8] = b"RISUSAVE\0";

    #[test]
    fn cancellation_writer_stops_write_all_without_retrying_into_output() {
        let directory = TempDir::new().unwrap();
        let mut file = File::create(directory.path().join("cancelled.bin")).unwrap();
        let cancellation_checks = Cell::new(0);
        let is_cancelled = || {
            let checks = cancellation_checks.get() + 1;
            cancellation_checks.set(checks);
            checks <= 3
        };
        let mut writer = CancellationAwareWriter {
            file: &mut file,
            is_cancelled: &is_cancelled,
        };

        let error = writer
            .write_all(b"synthetic payload")
            .expect_err("cancelled writer must stop");

        assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), EXPORT_CANCELLED_MESSAGE);
        assert_eq!(cancellation_checks.get(), 1);
        drop(writer);
        assert_eq!(file.stream_position().unwrap(), 0);
    }

    #[test]
    fn risusave_export_naming_matches_the_shared_taxonomy_golden_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../src/ts/storage/tests/fixtures/nativeFileTaxonomyV1Golden.json"
        ))
        .unwrap();
        let export = &fixture["risuSaveExport"];
        let prefix = export["prefix"].as_str().unwrap();
        let uuid = fixture["uuid"].as_str().unwrap();
        for (suffix_key, expected_kind) in [
            ("dataSuffix", ManagedFileKind::Completed),
            ("temporarySuffix", ManagedFileKind::Temporary),
            ("leaseSuffix", ManagedFileKind::Ownership),
        ] {
            let suffix = export[suffix_key].as_str().unwrap();
            let name = format!("{prefix}{uuid}{suffix}");
            let (id, kind) =
                managed_file(Path::new(&name)).unwrap_or_else(|| panic!("{name} is not managed"));
            assert_eq!(id, uuid);
            assert!(kind == expected_kind, "{name} kind mismatch");
        }
        for rejected in [
            format!("{prefix}{}{}", uuid.to_uppercase(), ".risudat"),
            format!("{prefix}not-a-uuid.risudat"),
            format!("save-{uuid}.risudat"),
            format!("{prefix}{uuid}.zip"),
        ] {
            assert!(
                managed_file(Path::new(&rejected)).is_none(),
                "{rejected} must not be managed"
            );
        }
    }

    #[derive(Debug)]
    struct Block {
        block_type: u8,
        name: String,
        value: Value,
    }

    fn assert_semantically_equal(actual: &Value, expected: &Value) {
        match (actual, expected) {
            (Value::Object(actual), Value::Object(expected)) => {
                assert_eq!(actual.len(), expected.len());
                for (key, expected) in expected {
                    assert!(
                        actual.contains_key(key),
                        "semantic object keys differ: missing {key}"
                    );
                    assert_semantically_equal(actual.get(key).unwrap(), expected);
                }
            }
            (Value::Array(actual), Value::Array(expected)) => {
                assert_eq!(actual.len(), expected.len());
                for (actual, expected) in actual.iter().zip(expected) {
                    assert_semantically_equal(actual, expected);
                }
            }
            _ => assert_eq!(actual, expected),
        }
    }

    #[test]
    #[should_panic(expected = "semantic object keys differ")]
    fn semantic_comparison_rejects_same_length_objects_with_a_missing_null_key() {
        assert_semantically_equal(&json!({"different": null}), &json!({"a": null}));
    }

    fn native_content_projection_fixture() -> (TempDir, PersistentStore, String) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let first_payload = cas.prepare_bytes(b"first occurrence payload").unwrap();
        let second_payload = cas.prepare_bytes(b"second occurrence payload").unwrap();
        let entries = [
            owner_manifest_codec::OwnerManifestEntry {
                tuple: [
                    "first".to_owned(),
                    "assets/shared.bin".to_owned(),
                    "PNG".to_owned(),
                ],
                payload_hash: Some(
                    hex::decode(&first_payload.content_hash)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
            owner_manifest_codec::OwnerManifestEntry {
                tuple: [
                    "second".to_owned(),
                    "assets/shared.bin".to_owned(),
                    "pNg".to_owned(),
                ],
                payload_hash: Some(
                    hex::decode(&second_payload.content_hash)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
        ];
        let manifest = cas
            .prepare_bytes(&owner_manifest_codec::encode_owner_manifest(&entries).unwrap())
            .unwrap();
        let character_manifest = cas
            .prepare_bytes(
                &owner_manifest_codec::encode_owner_manifest(&[
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: [
                            "first".to_owned(),
                            "assets/shared.bin".to_owned(),
                            "BIN".to_owned(),
                        ],
                        payload_hash: entries[0].payload_hash,
                    },
                    owner_manifest_codec::OwnerManifestEntry {
                        tuple: [
                            "second".to_owned(),
                            "assets/shared.bin".to_owned(),
                            "bIn".to_owned(),
                        ],
                        payload_hash: entries[1].payload_hash,
                    },
                ])
                .unwrap(),
            )
            .unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "modules": [
                        {
                            "name": "Leased Module",
                            "assets": [
                                ["first", "assets/shared.bin", "PNG", {"tail": 1}],
                                ["second", "assets/shared.bin", "pNg", "tuple-tail"]
                            ]
                        },
                        {"name": "Missing Assets"},
                        {"name": "Empty Assets", "assets": []}
                    ]
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(
                &staging,
                &[json!({
                    "type": "character",
                    "chaId": "corpus-character",
                    "name": "Leased Character",
                    "additionalAssets": [
                        ["first", "assets/shared.bin", "BIN"],
                        ["second", "assets/shared.bin", "bIn"]
                    ],
                    "chats": []
                })],
            )
            .unwrap();
        store
            .replace_put_asset_owner_heads(
                &staging,
                &[
                    AssetOwnerHead::present(
                        AssetOwnerLocator::CharacterAdditionalAssets {
                            character_id: "corpus-character".to_owned(),
                        },
                        character_manifest.content_hash,
                        2,
                    ),
                    AssetOwnerHead::present(
                        AssetOwnerLocator::RootModuleAssets { index: 0 },
                        manifest.content_hash,
                        2,
                    ),
                    AssetOwnerHead::absent(AssetOwnerLocator::RootModuleAssets { index: 1 }),
                    AssetOwnerHead::present(
                        AssetOwnerLocator::RootModuleAssets { index: 2 },
                        cas.prepare_bytes(
                            &owner_manifest_codec::encode_owner_manifest(&[]).unwrap(),
                        )
                        .unwrap()
                        .content_hash,
                        0,
                    ),
                ],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "native-content-export-corpus".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let lease = store.acquire_revision(revision).unwrap().lease;

        let current = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&current, &json!({"modules": [{"name": "Current Module"}]}))
            .unwrap();
        store.replace_put_presets(&current, &[]).unwrap();
        store
            .replace_add_characters(
                &current,
                &[json!({
                    "type": "character",
                    "chaId": "corpus-character",
                    "name": "Current Character",
                    "chats": []
                })],
            )
            .unwrap();
        store.replace_commit(&current, Some(revision)).unwrap();
        (directory, store, lease)
    }

    #[test]
    fn native_content_corpus_projects_exact_leased_character_and_root_module_occurrences() {
        let (_directory, store, lease) = native_content_projection_fixture();
        let (connection, target) = store.read_view(Some(&lease)).unwrap();
        for fixture in [
            include_str!("../../fixtures/native-content-export-v1/appended-charx-jpeg.json"),
            include_str!("../../fixtures/native-content-export-v1/json-card.json"),
            include_str!("../../fixtures/native-content-export-v1/png-card.json"),
        ] {
            let fixture: Value = serde_json::from_str(fixture).unwrap();
            let projected = projected_character(
                connection,
                &store.snapshots_dir,
                &target,
                fixture["characterId"].as_str().unwrap(),
            )
            .unwrap();
            assert_semantically_equal(
                &json!({
                    "name": projected.value["name"],
                    "additionalAssets": projected.value["additionalAssets"]
                }),
                &fixture["expected"],
            );
            assert_eq!(
                projected
                    .additional_asset_entries
                    .unwrap()
                    .into_iter()
                    .map(|entry| entry.tuple)
                    .collect::<Vec<_>>(),
                [
                    [
                        "first".to_owned(),
                        "assets/shared.bin".to_owned(),
                        "BIN".to_owned()
                    ],
                    [
                        "second".to_owned(),
                        "assets/shared.bin".to_owned(),
                        "bIn".to_owned()
                    ]
                ]
            );
        }

        let fixture: Value = serde_json::from_str(include_str!(
            "../../fixtures/native-content-export-v1/risu-module.json"
        ))
        .unwrap();
        let projected = projected_root_module(
            connection,
            &store.snapshots_dir,
            &target,
            fixture["moduleIndex"].as_u64().unwrap(),
        )
        .unwrap();
        assert_semantically_equal(&projected.value, &fixture["expected"]);
        let occurrences = projected.asset_entries.unwrap();
        assert_eq!(
            occurrences
                .iter()
                .map(|entry| &entry.tuple)
                .collect::<Vec<_>>(),
            [
                &[
                    "first".to_owned(),
                    "assets/shared.bin".to_owned(),
                    "PNG".to_owned()
                ],
                &[
                    "second".to_owned(),
                    "assets/shared.bin".to_owned(),
                    "pNg".to_owned()
                ]
            ]
        );
        assert_ne!(occurrences[0].payload_hash, occurrences[1].payload_hash);

        let missing = projected_root_module(connection, &store.snapshots_dir, &target, 1).unwrap();
        let empty = projected_root_module(connection, &store.snapshots_dir, &target, 2).unwrap();
        assert!(missing.value.get("assets").is_none());
        assert_eq!(missing.asset_entries, Some(Vec::new()));
        assert_eq!(empty.value["assets"], json!([]));
        assert_eq!(empty.asset_entries, Some(Vec::new()));
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

    fn projection_fixture(projected: bool) -> (TempDir, PersistentStore, i64, String) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        let resource = if projected { "new" } else { "old" };
        let plugin_resource = if projected {
            "remote/assets/plugin.bin"
        } else {
            "assets/plugin.bin"
        };
        store
            .replace_put_root(
                &staging,
                &json!({
                    "customBackground": resource,
                    "userIcon": resource,
                    "unrelated": "old",
                    "modules": [{
                        "assets": [["module", resource, "png"]],
                        "icon": resource,
                        "unrelated": "old"
                    }],
                    "personas": [{
                        "icon": resource,
                        "embeddedModule": {
                            "assets": [["persona", resource, "png"]],
                            "icon": resource,
                            "unrelated": "old"
                        }
                    }],
                    "characterOrder": [{ "imgFile": resource, "unrelated": "old" }],
                    "loadouts": [{ "resource": "old" }],
                    "plugins": [{ "resource": "old" }],
                    "pluginCustomStorage": {
                        "resource": plugin_resource,
                        "prose": "prefix assets/plugin.bin"
                    }
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(
                &staging,
                &[json!({
                    "type": "character",
                    "chaId": "projected-character",
                    "name": "Projected Character",
                    "image": resource,
                    "emotionImages": [["happy", resource]],
                    "additionalAssets": [["additional", resource]],
                    "vits": { "files": { "voice": resource } },
                    "ccAssets": [{ "uri": resource }],
                    "unrelated": "old",
                    "chats": [{
                        "id": "chat",
                        "name": "old",
                        "message": [{ "role": "user", "data": "old", "chatId": "message" }]
                    }]
                })],
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
    fn projects_only_supported_root_resource_paths_with_exact_single_lookups() {
        let replacements = HashMap::from([
            ("old".to_owned(), "new".to_owned()),
            ("chain-start".to_owned(), "chain-middle".to_owned()),
            ("chain-middle".to_owned(), "chain-end".to_owned()),
            ("erase".to_owned(), String::new()),
            (r"path\old".to_owned(), r"path\new".to_owned()),
            (String::new(), "must-not-replace-empty".to_owned()),
        ]);
        let mut root = into_object(
            json!({
                "customBackground": "old",
                "userIcon": "erase",
                "unrelated": "old",
                "nested": { "icon": "old" },
                "modules": [
                    {
                        "assets": [
                            ["first", "old", "png"],
                            ["second", "chain-start"],
                            ["missing-value"],
                            ["null-value", null],
                            ["number-value", 7],
                            null,
                            "malformed"
                        ],
                        "icon": "path\\old",
                        "unrelated": "old"
                    },
                    null,
                    "malformed"
                ],
                "personas": [
                    {
                        "icon": "old",
                        "embeddedModule": {
                            "assets": [["embedded", "old"], null],
                            "icon": "old",
                            "unrelated": "old"
                        }
                    },
                    { "icon": null, "embeddedModule": null },
                    null
                ],
                "characterOrder": [
                    { "imgFile": "old", "unrelated": "old" },
                    { "imgFile": null },
                    "old",
                    null
                ]
            }),
            "root",
        )
        .unwrap();

        project_root_resources(&mut root, &replacements);

        assert_eq!(
            Value::Object(root),
            json!({
                "customBackground": "new",
                "userIcon": "",
                "unrelated": "old",
                "nested": { "icon": "old" },
                "modules": [
                    {
                        "assets": [
                            ["first", "new", "png"],
                            ["second", "chain-middle"],
                            ["missing-value"],
                            ["null-value", null],
                            ["number-value", 7],
                            null,
                            "malformed"
                        ],
                        "icon": "path\\new",
                        "unrelated": "old"
                    },
                    null,
                    "malformed"
                ],
                "personas": [
                    {
                        "icon": "new",
                        "embeddedModule": {
                            "assets": [["embedded", "new"], null],
                            "icon": "new",
                            "unrelated": "old"
                        }
                    },
                    { "icon": null, "embeddedModule": null },
                    null
                ],
                "characterOrder": [
                    { "imgFile": "new", "unrelated": "old" },
                    { "imgFile": null },
                    "old",
                    null
                ]
            })
        );
    }

    #[test]
    fn projects_only_exact_plugin_storage_asset_values_once() {
        let replacements = HashMap::from([
            (
                "assets/direct.bin".to_owned(),
                "remote/direct.bin".to_owned(),
            ),
            (
                "assets/chain.bin".to_owned(),
                "assets/chain-step.bin".to_owned(),
            ),
            (
                "assets/chain-step.bin".to_owned(),
                "remote/chain-final.bin".to_owned(),
            ),
            ("assets/proto.bin".to_owned(), "remote/proto.bin".to_owned()),
            (
                "assets/windows.bin".to_owned(),
                "remote/windows.bin".to_owned(),
            ),
            (
                "assets/folder/legacy.bin".to_owned(),
                "remote/legacy.bin".to_owned(),
            ),
        ]);
        let mut storage = json!({
            "direct": "assets/direct.bin",
            "legacy": "assets/folder/legacy.bin",
            "nested": ["assets/chain.bin", "prefix assets/direct.bin"],
            "windows": "assets\\windows.bin",
            "assets/direct.bin": "object-key",
            "__proto__": "assets/proto.bin",
            "serialized": "{\"path\":\"assets/direct.bin\"}"
        });

        project_plugin_storage_resources(&mut storage, &replacements);

        assert_eq!(storage["direct"], json!("remote/direct.bin"));
        assert_eq!(storage["legacy"], json!("remote/legacy.bin"));
        assert_eq!(storage["nested"][0], json!("assets/chain-step.bin"));
        assert_eq!(storage["nested"][1], json!("prefix assets/direct.bin"));
        assert_eq!(storage["assets/direct.bin"], json!("object-key"));
        assert_eq!(storage["windows"], json!("remote/windows.bin"));
        assert_eq!(storage["__proto__"], json!("remote/proto.bin"));
        assert_eq!(
            storage["serialized"],
            json!("{\"path\":\"assets/direct.bin\"}")
        );
    }

    #[test]
    fn projects_non_group_character_resources_without_touching_unrelated_values() {
        let replacements = HashMap::from([
            ("old".to_owned(), "new".to_owned()),
            ("chain-start".to_owned(), "chain-middle".to_owned()),
            ("chain-middle".to_owned(), "chain-end".to_owned()),
        ]);
        let mut character = into_object(
            json!({
                "type": null,
                "image": "chain-start",
                "emotionImages": [["happy", "old"], ["missing"], null, "malformed"],
                "additionalAssets": [["asset", "old"], ["null", null], null],
                "vits": {
                    "files": { "voice": "old", "empty": "", "null": null, "number": 7 },
                    "unrelated": "old"
                },
                "ccAssets": [{ "uri": "old", "unrelated": "old" }, { "uri": null }, null],
                "chats": [{ "message": [{ "data": "old" }] }],
                "plugin": { "resource": "old" },
                "unrelated": "old"
            }),
            "character",
        )
        .unwrap();

        project_character_resources(&mut character, &replacements);

        assert_eq!(character["image"], json!("chain-middle"));
        assert_eq!(character["emotionImages"][0][1], json!("new"));
        assert_eq!(character["additionalAssets"][0][1], json!("new"));
        assert_eq!(character["vits"]["files"]["voice"], json!("new"));
        assert_eq!(character["vits"]["files"]["empty"], json!(""));
        assert_eq!(character["vits"]["files"]["null"], Value::Null);
        assert_eq!(character["vits"]["files"]["number"], json!(7));
        assert_eq!(character["ccAssets"][0]["uri"], json!("new"));
        assert_eq!(character["chats"][0]["message"][0]["data"], json!("old"));
        assert_eq!(character["plugin"]["resource"], json!("old"));
        assert_eq!(character["unrelated"], json!("old"));
    }

    #[test]
    fn group_projection_skips_character_only_resource_paths() {
        let replacements = HashMap::from([("old".to_owned(), "new".to_owned())]);
        let mut group = into_object(
            json!({
                "type": "group",
                "image": "old",
                "emotionImages": [["happy", "old"]],
                "additionalAssets": [["asset", "old"]],
                "vits": { "files": { "voice": "old" } },
                "ccAssets": [{ "uri": "old" }]
            }),
            "group",
        )
        .unwrap();

        project_character_resources(&mut group, &replacements);

        assert_eq!(group["image"], json!("new"));
        assert_eq!(group["emotionImages"][0][1], json!("new"));
        assert_eq!(group["additionalAssets"][0][1], json!("old"));
        assert_eq!(group["vits"]["files"]["voice"], json!("old"));
        assert_eq!(group["ccAssets"][0]["uri"], json!("old"));
    }

    #[test]
    fn projected_export_matches_a_preprojected_fixture_and_preserves_other_blocks() {
        let (_source_directory, source_store, _source_revision, source_lease) =
            projection_fixture(false);
        let (_expected_directory, expected_store, _expected_revision, expected_lease) =
            projection_fixture(true);
        let replacements = HashMap::from([
            ("old".to_owned(), "new".to_owned()),
            (
                "assets/plugin.bin".to_owned(),
                "remote/assets/plugin.bin".to_owned(),
            ),
        ]);
        let (source_connection, source_target) =
            source_store.read_view(Some(&source_lease)).unwrap();

        let actual = create_projected_controlled(
            source_connection,
            &source_store.snapshots_dir,
            &source_target,
            &source_lease,
            false,
            &replacements,
            || false,
            |_, _, _| {},
        )
        .unwrap();
        let expected = expected_store
            .export_risu_save(&expected_lease, false)
            .unwrap();

        assert_eq!(
            fs::read(&actual.path).unwrap(),
            fs::read(&expected.path).unwrap()
        );
        let blocks = read_blocks(Path::new(&actual.path));
        assert_eq!(blocks[0].value["customBackground"], json!("new"));
        assert_eq!(blocks[0].value["unrelated"], json!("old"));
        assert_eq!(blocks[2].value[0]["assets"][0][1], json!("new"));
        assert_eq!(blocks[4].value[0]["resource"], json!("old"));
        assert_eq!(
            blocks[5].value["resource"],
            json!("remote/assets/plugin.bin")
        );
        assert_eq!(blocks[5].value["prose"], json!("prefix assets/plugin.bin"));
        assert_eq!(blocks[7].value["image"], json!("new"));
        assert_eq!(blocks[7].value["unrelated"], json!("old"));
        assert_eq!(blocks[7].value["chats"][0]["name"], json!("old"));
        assert_eq!(
            blocks[7].value["chats"][0]["message"][0]["data"],
            json!("old")
        );

        let source_after_projection = source_store.export_risu_save(&source_lease, false).unwrap();
        let source_blocks = read_blocks(Path::new(&source_after_projection.path));
        assert_eq!(
            source_blocks[5].value["resource"],
            json!("assets/plugin.bin")
        );
        assert_eq!(
            source_blocks[5].value["prose"],
            json!("prefix assets/plugin.bin")
        );
    }

    fn archived_fixture() -> (TempDir, PersistentStore, i64, String) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "username": "Archive Export",
                    "account": { "id": "account-1", "token": "secret" },
                    "modules": [],
                    "loadouts": [],
                    "plugins": []
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(
                &staging,
                &[
                    json!({
                        "type": "character",
                        "chaId": "kept",
                        "name": "Kept",
                        "chats": [{ "id": "kept-chat", "name": "Kept Chat", "message": [] }]
                    }),
                    json!({
                        "type": "character",
                        "chaId": "archived",
                        "name": "Archived",
                        "chats": [{
                            "id": "archived-chat",
                            "name": "Archived Chat",
                            "message": [{ "role": "user", "data": "hidden", "chatId": "a1" }]
                        }]
                    }),
                ],
            )
            .unwrap();
        let revision = store.replace_commit(&staging, None).unwrap().revision;
        store.archive_character("archived", revision, 10).unwrap();
        let revision = store.revision().unwrap();
        let lease = store.acquire_revision(revision).unwrap().lease;
        (directory, store, revision, lease)
    }

    /// Invariant 10. The export reads `detail` straight from SQL, so the marker
    /// can only be kept out by leaving the character out of the enumeration.
    #[test]
    fn a_risu_save_export_omits_archived_characters_and_their_marker() {
        let (_directory, store, _revision, lease) = archived_fixture();
        let exported = store.export_risu_save(&lease, false).unwrap();
        assert_eq!(exported.character_count, 1);
        assert_eq!(exported.excluded_archived_character_count, 1);

        let blocks = read_blocks(Path::new(&exported.path));
        let characters = blocks
            .iter()
            .filter(|block| block.block_type == CHARACTER_WITH_CHAT)
            .collect::<Vec<_>>();
        assert_eq!(
            characters
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["kept"]
        );
        for block in &blocks {
            assert!(
                !block.value.to_string().contains("risuNestArchived"),
                "block {} leaks the archive marker",
                block.name
            );
        }
    }

    /// Invariant 4. The account projection has its own entry point, and the
    /// shared enumeration is what keeps the archive out of both.
    #[cfg(feature = "native-official-publication")]
    #[test]
    fn the_official_account_projection_omits_archived_characters() {
        let (_directory, store, _revision, lease) = archived_fixture();
        let (connection, target) = store.read_view(Some(&lease)).unwrap();
        let exported = create_projected_controlled_for_account(
            connection,
            &store.snapshots_dir,
            &target,
            &lease,
            "account-1",
            &HashMap::new(),
            || false,
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(exported.character_count, 1);
        assert_eq!(exported.excluded_archived_character_count, 1);

        let blocks = read_blocks(Path::new(&exported.path));
        let characters = blocks
            .iter()
            .filter(|block| block.block_type == CHARACTER_WITH_CHAT)
            .collect::<Vec<_>>();
        assert_eq!(
            characters
                .iter()
                .map(|block| block.name.as_str())
                .collect::<Vec<_>>(),
            ["kept"]
        );
        assert!(characters
            .iter()
            .all(|block| !block.value.to_string().contains("risuNestArchived")));
    }

    #[test]
    fn empty_projection_map_is_byte_identical_to_the_ordinary_export() {
        let (_directory, store, _revision, lease) = projection_fixture(false);
        let ordinary = store.export_risu_save(&lease, false).unwrap();
        let (connection, target) = store.read_view(Some(&lease)).unwrap();

        let projected = create_projected_controlled(
            connection,
            &store.snapshots_dir,
            &target,
            &lease,
            false,
            &HashMap::new(),
            || false,
            |_, _, _| {},
        )
        .unwrap();

        assert_eq!(
            fs::read(ordinary.path).unwrap(),
            fs::read(projected.path).unwrap()
        );
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn projected_publication_validates_the_pinned_account_during_root_projection() {
        let (_directory, store, _revision, lease) = fixture();
        let (connection, target) = store.read_view(Some(&lease)).unwrap();

        let mismatch = create_projected_controlled_for_account(
            connection,
            &store.snapshots_dir,
            &target,
            &lease,
            "different-account",
            &HashMap::new(),
            || false,
            |_, _, _| {},
        )
        .expect_err("reject a mismatched pinned account");
        assert!(matches!(mismatch, StoreError::Validation { .. }));

        let (_directory, mut store, _revision, _lease) = fixture();
        let staging = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(
                &staging,
                &json!({
                    "account": { "id": "account-1", "token": "not-pinned" },
                    "customBackground": "old",
                    "modules": [],
                    "loadouts": [],
                    "plugins": []
                }),
            )
            .unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        let revision = store.replace_commit(&staging, None).unwrap().revision;
        let lease = store.acquire_revision(revision).unwrap().lease;
        let (connection, target) = store.read_view(Some(&lease)).unwrap();
        let exported = create_projected_controlled_for_account(
            connection,
            &store.snapshots_dir,
            &target,
            &lease,
            "account-1",
            &HashMap::from([("old".to_owned(), "new".to_owned())]),
            || false,
            |_, _, _| {},
        )
        .unwrap();
        let blocks = read_blocks(Path::new(&exported.path));
        assert_eq!(blocks[0].value["account"]["token"], "not-pinned");
        assert_eq!(blocks[0].value["customBackground"], "new");
    }

    #[test]
    fn exports_current_framing_and_configured_trash_order_from_a_lease() {
        let (_directory, store, _revision, lease) = fixture();

        let exported = store.export_risu_save(&lease, true).unwrap();
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
                (12, "pluginStorageMeta"),
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
                "pluginStorageMeta",
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
        assert_eq!(blocks[7].value["chats"][0]["message"][0]["data"], "trash");
        assert_eq!(blocks[8].value["chats"][0]["message"][1]["data"], "world");
        assert_eq!(exported.bytes, fs::metadata(&exported.path).unwrap().len());
    }

    #[test]
    fn moves_large_owned_root_and_block_values_without_recursive_clones() {
        let parsed = json!({
            "before": { "payload": "r".repeat(2 * 1024 * 1024) },
            "modules": [{ "payload": "m".repeat(2 * 1024 * 1024) }],
            "loadouts": [{ "payload": "l".repeat(2 * 1024 * 1024) }],
            "plugins": [{ "payload": "p".repeat(2 * 1024 * 1024) }],
            "after": 2,
        });
        let root_payload = parsed["before"]["payload"].as_str().unwrap().as_ptr();
        let module_payload = parsed["modules"][0]["payload"].as_str().unwrap().as_ptr();
        let loadout_payload = parsed["loadouts"][0]["payload"].as_str().unwrap().as_ptr();
        let plugin_payload = parsed["plugins"][0]["payload"].as_str().unwrap().as_ptr();

        let mut root = into_object(parsed, "root").unwrap();
        let modules = take_root_block_value(&mut root, "modules").unwrap();
        let loadouts = take_root_block_value(&mut root, "loadouts").unwrap();
        let plugins = take_root_block_value(&mut root, "plugins").unwrap();

        assert_eq!(
            root["before"]["payload"].as_str().unwrap().as_ptr(),
            root_payload
        );
        assert!(!root.contains_key("modules"));
        assert!(!root.contains_key("loadouts"));
        assert!(!root.contains_key("plugins"));
        assert_eq!(
            root.keys().map(String::as_str).collect::<Vec<_>>(),
            ["before", "after"]
        );
        assert_eq!(
            modules[0]["payload"].as_str().unwrap().as_ptr(),
            module_payload
        );
        assert_eq!(
            loadouts[0]["payload"].as_str().unwrap().as_ptr(),
            loadout_payload
        );
        assert_eq!(
            plugins[0]["payload"].as_str().unwrap().as_ptr(),
            plugin_payload
        );
    }

    #[test]
    fn exports_deterministic_bytes_and_cleans_only_managed_files() {
        let (directory, store, _revision, lease) = fixture();

        let first = store.export_risu_save(&lease, false).unwrap();
        let second = store.export_risu_save(&lease, false).unwrap();
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
    fn ownership_reader_rejects_growth_beyond_its_opened_metadata_hint() {
        let directory = TempDir::new().unwrap();
        let marker = directory.path().join("marker.lease");
        let mut contents = serde_json::to_vec(&ExportOwnership {
            export_id: "id".to_owned(),
            lease: "lease".to_owned(),
        })
        .unwrap();
        contents.resize(4097, b' ');
        fs::write(&marker, contents).unwrap();

        assert!(read_bounded_ownership(File::open(marker).unwrap(), 1).is_err());
    }

    #[cfg(feature = "official-publication-upload-pilot")]
    #[test]
    fn upload_sources_are_opened_without_following_file_symlinks() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source.risudat");
        let alias = directory.path().join("alias.risudat");
        fs::write(&source, b"managed export").unwrap();

        let (mut file, bytes) = open_regular_file_no_follow(&source).unwrap();
        let mut body = Vec::new();
        file.read_to_end(&mut body).unwrap();
        assert_eq!(bytes, body.len() as u64);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source, &alias).unwrap();
            assert!(open_regular_file_no_follow(&alias).is_err());
        }
        #[cfg(windows)]
        match std::os::windows::fs::symlink_file(&source, &alias) {
            Ok(()) => assert!(open_regular_file_no_follow(&alias).is_err()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(error) => panic!("could not create test symlink: {error}"),
        }
    }

    #[cfg(feature = "official-publication-upload-pilot")]
    #[test]
    fn opens_only_a_managed_completed_export_with_its_live_lease() {
        let (directory, mut store, _revision, lease) = fixture();
        let exported = store.export_risu_save(&lease, false).unwrap();

        let (mut source, bytes) = store
            .open_risu_save_export_for_upload(Path::new(&exported.path))
            .unwrap();
        let mut body = Vec::new();
        source.read_to_end(&mut body).unwrap();
        assert_eq!(bytes, exported.bytes);
        assert_eq!(body.len() as u64, exported.bytes);

        let external = directory
            .path()
            .join(Path::new(&exported.path).file_name().unwrap());
        fs::copy(&exported.path, &external).unwrap();
        assert!(store.open_risu_save_export_for_upload(&external).is_err());

        store.release_revision(&lease).unwrap();
        assert!(store
            .open_risu_save_export_for_upload(Path::new(&exported.path))
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
                root_mutations: None,
                root: None,
                replace_presets: None,
                character: None,
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                asset_owner_heads: None,
                plugin_storage: Some(vec![
                    PluginStorageMutation::Clear { owner: UNOWNED_OWNER.to_owned() },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "zeta".to_owned(),
                        value: json!("first string"),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "10".to_owned(),
                        value: json!("ten"),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "2".to_owned(),
                        value: json!(0),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "01".to_owned(),
                        value: json!("non-index"),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "4294967294".to_owned(),
                        value: json!(true),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "4294967295".to_owned(),
                        value: json!(false),
                    },
                    PluginStorageMutation::Set {
                        owner: UNOWNED_OWNER.to_owned(),
                        key: "\u{ffff}x".to_owned(),
                        value: json!("unicode"),
                    },
                ]),
            })
            .unwrap();
        let lease = store.acquire_revision(committed.revision).unwrap().lease;

        let exported = store.export_risu_save(&lease, false).unwrap();
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
        let (_directory, mut store, revision, lease) = fixture();
        store.release_revision(&lease).unwrap();
        let generation = super::super::active_generation(&store.connection).unwrap();
        store
            .connection
            .execute(
                "UPDATE root SET value = '{' WHERE generation = ?1",
                [&generation],
            )
            .unwrap();

        let lease = store.acquire_revision(revision).unwrap().lease;
        assert!(store.export_risu_save(&lease, false).is_err());
        store.release_revision(&lease).unwrap();

        let exports_dir = store.snapshots_dir.parent().unwrap().join("exports");
        let remaining = fs::read_dir(exports_dir)
            .map(|entries| entries.collect::<Result<Vec<_>, _>>().unwrap())
            .unwrap_or_default();
        assert!(remaining.is_empty());
    }

    #[test]
    fn reopen_reclaims_export_files_and_invalidates_every_runtime_lease() {
        let (directory, mut store, revision, export_lease) = fixture();
        let unrelated_lease = store.acquire_revision(revision).unwrap().lease;
        let exported = store.export_risu_save(&export_lease, false).unwrap();
        let exported_path = PathBuf::from(&exported.path);
        let ownership_path = exported_path.with_extension("lease");
        assert!(ownership_path.is_file());
        assert!(fs::read_to_string(&ownership_path)
            .unwrap()
            .contains(&export_lease));

        drop(store);
        let reopened = PersistentStore::open(directory.path()).unwrap();

        assert!(!exported_path.exists());
        assert!(!ownership_path.exists());
        assert!(matches!(
            reopened.read_root(Some(&export_lease)),
            Err(StoreError::SnapshotReleased)
        ));
        assert!(matches!(
            reopened.read_root(Some(&unrelated_lease)),
            Err(StoreError::SnapshotReleased)
        ));
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
