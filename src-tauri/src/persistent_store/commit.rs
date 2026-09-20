use super::{
    active_generation, current_revision, AssetAlias, AssetOwnerHead, AssetOwnerLocator,
    AssetRepositoryAuthorityState, ConversationMutation, PluginStorageMutation, RevisionResult,
    StagingResult, StoreError,
    StoreResult, WorkingSetCommit, GENERATION_TABLES,
};
use super::plugin_owner;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

pub(super) fn incremental_commit<T>(
    connection: &mut Connection,
    expected_revision: i64,
    prepare: impl FnOnce(&Transaction<'_>, &str) -> StoreResult<T>,
    body: impl FnOnce(&Transaction<'_>, &str, T) -> StoreResult<()>,
) -> StoreResult<RevisionResult> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let active = active_generation(&transaction)?;
    let prepared = prepare(&transaction, &active)?;
    let revision = actual_revision + 1;
    let generation = active;
    super::server_sync_outbox::begin_mutation(&transaction, &generation, revision)?;
    super::content_change_index::begin_mutation(&transaction, &generation, revision, "local")?;
    body(&transaction, &generation, prepared)?;
    super::content_change_index::finish_mutation(&transaction)?;
    super::server_sync_outbox::finish_mutation(&transaction)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
}

pub(super) fn commit_asset_alias(
    connection: &mut Connection,
    alias: &AssetAlias,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        expected_revision,
        |_, _| alias.validate(),
        |transaction, generation, ()| {
            put_asset_alias(transaction, generation, alias)?;
            Ok(())
        },
    )
}

pub(super) fn delete_asset_alias(
    connection: &mut Connection,
    kind: &str,
    key: &str,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    super::query::validate_asset_kind(kind)?;
    incremental_commit(
        connection,
        expected_revision,
        |_, _| Ok(()),
        |transaction, generation, ()| {
            transaction.execute(
                "DELETE FROM asset_aliases WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
                params![generation, kind, key],
            )?;
            Ok(())
        },
    )
}

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};

pub(super) fn apply_root_mutations(
    mut root: Value,
    mutations: &[super::RootMutation],
) -> StoreResult<Value> {
    let value = root
        .as_object_mut()
        .ok_or_else(|| validation("Persistent root must be an object"))?;
    let mut keys = HashSet::new();
    for mutation in mutations {
        let key = match mutation {
            super::RootMutation::Set { key, .. } | super::RootMutation::Delete { key } => key,
        };
        if matches!(
            key.as_str(),
            "characters" | "botPresets" | "pluginCustomStorage" | "pluginStorageMeta"
        ) {
            return Err(validation("Invalid persistent root mutation key"));
        }
        if !keys.insert(key) {
            return Err(validation("Duplicate persistent root mutation key"));
        }
        match mutation {
            super::RootMutation::Set { key, value: next } => {
                value.insert(key.clone(), next.clone());
            }
            super::RootMutation::Delete { key } => {
                value.shift_remove(key);
            }
        }
    }
    Ok(root)
}

pub(super) fn commit(
    connection: &mut Connection,
    input: &WorkingSetCommit,
    asset_aliases: &[AssetAlias],
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        input.expected_revision,
        |transaction, active| {
            if let Some(character) = &input.replace_character {
                validate_character(character, "Selected character replacement")?;
            }
            if let Some(character) = &input.add_character {
                validate_character(character, "Character addition")?;
            }
            let mut alias_identities = HashSet::new();
            for alias in asset_aliases {
                alias.validate()?;
                if !alias_identities.insert((alias.kind.as_str(), alias.key.as_str())) {
                    return Err(validation("Duplicate asset alias"));
                }
            }
            if let Some(details) = &input.character_details {
                validate_character_details(
                    transaction,
                    active,
                    details,
                    input.delete_character_id.as_deref(),
                )?;
            }
            reject_archived_targets(transaction, active, input)?;
            validate_owner_heads_for_commit(input)?;
            retained_commit_owner_heads(transaction, active, input)
        },
        |transaction, generation, retained| {
            if let Some(root) = &input.root {
                put_root(transaction, generation, root)?;
            }
            if let Some(presets) = &input.replace_presets {
                replace_presets(transaction, generation, presets)?;
            }
            if let Some(character_id) = &input.delete_character_id {
                delete_character(transaction, generation, character_id)?;
            }
            if let Some(character) = &input.character {
                put_character_detail(transaction, generation, character)?;
            }
            for detail in input.character_details.as_deref().unwrap_or_default() {
                put_character_detail(transaction, generation, detail)?;
            }
            if let Some(character) = &input.replace_character {
                replace_character(transaction, generation, character)?;
            }
            if let Some(character) = &input.add_character {
                let character_id = required_string(character, "chaId", "Character addition")?;
                if character_exists(transaction, generation, character_id)? {
                    return Err(validation(format!(
                        "Character {character_id} already exists"
                    )));
                }
                replace_character(transaction, generation, character)?;
            }
            for mutation in input.conversations.as_deref().unwrap_or_default() {
                apply_conversation_mutation(transaction, generation, mutation)?;
            }
            for mutation in input.plugin_storage.as_deref().unwrap_or_default() {
                apply_plugin_storage_mutation(transaction, generation, mutation)?;
            }
            for alias in asset_aliases {
                put_asset_alias(transaction, generation, alias)?;
            }
            replace_changed_owner_heads(transaction, generation, input, &retained)?;
            Ok(())
        },
    )
}

/// An archived character has no conversations and only a marker detail, so any
/// mutation other than deleting it would leave the row describing nothing.
fn reject_archived_targets(
    transaction: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
) -> StoreResult<()> {
    let mut targets: Vec<&str> = Vec::new();
    for character in input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter())
        .chain(input.add_character.iter())
    {
        if let Some(character_id) = character.get("chaId").and_then(Value::as_str) {
            targets.push(character_id);
        }
    }
    for mutation in input.conversations.as_deref().unwrap_or_default() {
        targets.push(match mutation {
            ConversationMutation::ReplaceRange { character_id, .. } => character_id,
            ConversationMutation::Delete { character_id, .. } => character_id,
        });
    }
    for character_id in targets {
        if Some(character_id) == input.delete_character_id.as_deref() {
            continue;
        }
        if super::archive::is_archived(transaction, generation, character_id)? {
            return Err(super::archive::archived_error(character_id));
        }
    }
    Ok(())
}

fn owner_entries<'a>(
    input: &'a WorkingSetCommit,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<&'a Vec<Value>>> {
    let parent = match owner {
        AssetOwnerLocator::CharacterAdditionalAssets { character_id } => {
            let values = input
                .character
                .iter()
                .chain(input.character_details.iter().flatten())
                .chain(input.replace_character.iter())
                .chain(input.add_character.iter());
            values
                .filter_map(Value::as_object)
                .filter(|value| value.get("chaId").and_then(Value::as_str) == Some(character_id))
                .last()
                .ok_or_else(|| {
                    validation("Character asset owner head requires its parent mutation")
                })?
        }
        AssetOwnerLocator::RootModuleAssets { index } => input
            .root
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("modules"))
            .and_then(Value::as_array)
            .and_then(|modules| modules.get(*index as usize))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Root module asset owner occurrence does not exist"))?,
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => input
            .root
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("personas"))
            .and_then(Value::as_array)
            .and_then(|personas| personas.get(*index as usize))
            .and_then(Value::as_object)
            .and_then(|persona| persona.get("embeddedModule"))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Persona module asset owner occurrence does not exist"))?,
    };
    parent
        .get(match owner {
            AssetOwnerLocator::CharacterAdditionalAssets { .. } => "additionalAssets",
            _ => "assets",
        })
        .map(|entries| {
            entries
                .as_array()
                .ok_or_else(|| validation("Asset owner property must be an array when present"))
        })
        .transpose()
}

fn validate_owner_heads_for_commit(input: &WorkingSetCommit) -> StoreResult<()> {
    let mut identities = HashSet::new();
    for head in input.asset_owner_heads.as_deref().unwrap_or_default() {
        head.validate()?;
        let identity = head.owner.storage_identity();
        if !identities.insert(identity) {
            return Err(validation("Duplicate asset owner head"));
        }
        let entries = owner_entries(input, &head.owner)?;
        if head.present != entries.is_some() {
            return Err(validation(
                "Asset owner head property presence does not match its parent",
            ));
        }
        if head.present && head.entry_count != entries.map_or(0, |value| value.len() as i64) {
            return Err(validation(
                "Asset owner head entryCount does not match its parent",
            ));
        }
    }
    Ok(())
}

fn retained_commit_owner_heads(
    connection: &Connection,
    generation: &str,
    input: &WorkingSetCommit,
) -> StoreResult<Vec<AssetOwnerHead>> {
    if input.root.is_none()
        && input.character.is_none()
        && input.character_details.is_none()
        && input.replace_character.is_none()
        && input.add_character.is_none()
    {
        return Ok(Vec::new());
    }
    let old_root = if input.root.is_some() {
        replacement_root(connection, generation)?
    } else {
        serde_json::json!({})
    };
    let character_ids = input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter())
        .chain(input.add_character.iter())
        .filter_map(|value| value.get("chaId").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let mut retained = Vec::new();
    for mut head in selected_replacement_owner_heads(
        connection,
        generation,
        Some(input.root.is_some()),
        &character_ids,
    )? {
        let original_owner = head.owner.clone();
        match &head.owner {
            AssetOwnerLocator::CharacterAdditionalAssets { character_id } => {
                if input.delete_character_id.as_ref() == Some(character_id) {
                    continue;
                }
            }
            AssetOwnerLocator::RootModuleAssets { index } => {
                let Some(root) = &input.root else {
                    continue;
                };
                let Some(index) = retained_module_index(&old_root, root, "modules", *index, false)
                else {
                    continue;
                };
                head.owner = AssetOwnerLocator::RootModuleAssets { index };
            }
            AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => {
                let Some(root) = &input.root else {
                    continue;
                };
                let Some(index) = retained_module_index(&old_root, root, "personas", *index, true)
                else {
                    continue;
                };
                head.owner = AssetOwnerLocator::PersonaEmbeddedModuleAssets { index };
            }
        }
        let Ok(entries) = owner_entries(input, &head.owner) else {
            continue;
        };
        let new_tuple = Some(match entries {
            Some(entries) => ReplacementOwnerTuple::Present(entries.clone()),
            None => ReplacementOwnerTuple::Absent,
        });
        if replacement_owner_tuple(connection, generation, &old_root, &original_owner)? == new_tuple
        {
            retained.push(head);
        }
    }
    Ok(retained)
}

fn retained_module_index(
    old: &Value,
    new: &Value,
    property: &str,
    index: i64,
    embedded: bool,
) -> Option<i64> {
    let old = old.get(property)?.as_array()?;
    let new = new.get(property)?.as_array()?;
    let source = old.get(usize::try_from(index).ok()?)?;
    fn module(value: &Value, embedded: bool) -> Option<&Value> {
        if embedded {
            value.get("embeddedModule")
        } else {
            Some(value)
        }
    }
    let source_module = module(source, embedded)?;
    let id = source_module
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    if let Some(id) = id {
        let matches_id = |value: &&Value| {
            module(value, embedded)
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str)
                == Some(id)
        };
        if old.iter().filter(matches_id).count() == 1 && new.iter().filter(matches_id).count() == 1
        {
            return new
                .iter()
                .position(|value| matches_id(&value))
                .map(|index| index as i64);
        }
    }
    // Duplicate or missing IDs still have occurrence identity. Exact parent
    // matches can move, but ambiguous duplicates must not borrow another head.
    if new.get(index as usize) == Some(source) {
        return Some(index);
    }
    let matches = new
        .iter()
        .enumerate()
        .filter(|(_, value)| *value == source)
        .collect::<Vec<_>>();
    (matches.len() == 1 && old.iter().filter(|value| *value == source).count() == 1)
        .then(|| matches[0].0 as i64)
}

fn replace_changed_owner_heads(
    transaction: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
    retained: &[AssetOwnerHead],
) -> StoreResult<()> {
    if input.root.is_some() {
        transaction.execute(
            "DELETE FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind IN (
                 'root-module-assets', 'persona-embedded-module-assets'
             )",
            [generation],
        )?;
    }
    let mut character_ids = HashSet::new();
    for character in input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter())
        .chain(input.add_character.iter())
    {
        if let Some(character_id) = character.get("chaId").and_then(Value::as_str) {
            character_ids.insert(character_id.to_owned());
        }
    }
    if let Some(character_id) = &input.delete_character_id {
        character_ids.insert(character_id.clone());
    }
    for character_id in character_ids {
        transaction.execute(
            "DELETE FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
               AND owner_locator = ?2",
            params![generation, character_id],
        )?;
    }
    for head in retained
        .iter()
        .chain(input.asset_owner_heads.as_deref().unwrap_or_default())
    {
        put_asset_owner_head(transaction, generation, head)?;
    }
    Ok(())
}

fn put_asset_owner_head(
    transaction: &Transaction<'_>,
    generation: &str,
    head: &AssetOwnerHead,
) -> StoreResult<()> {
    let (owner_kind, owner_locator) = head.owner.storage_identity();
    transaction.execute(
        "INSERT INTO asset_owner_heads (
            generation, owner_kind, owner_locator, present, manifest_hash, entry_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(generation, owner_kind, owner_locator) DO UPDATE SET
            present = excluded.present,
            manifest_hash = excluded.manifest_hash,
            entry_count = excluded.entry_count",
        params![
            generation,
            owner_kind,
            owner_locator,
            head.present,
            head.manifest_hash,
            head.entry_count,
        ],
    )?;
    Ok(())
}

pub(super) fn replace_begin(connection: &mut Connection) -> StoreResult<StagingResult> {
    let staging_id = format!("staging-{}", uuid::Uuid::new_v4());
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    put_root(&transaction, &staging_id, &Value::Object(Map::new()))?;
    put_asset_repository_authority(
        &transaction,
        &staging_id,
        &AssetRepositoryAuthorityState::Legacy,
    )?;
    transaction.commit()?;
    Ok(StagingResult { staging_id })
}

pub(super) fn replace_put_root(
    connection: &mut Connection,
    staging_id: &str,
    root: &Value,
) -> StoreResult<()> {
    replace_put_root_with_plugin_storage(connection, staging_id, root, None)
}

pub(super) fn replace_put_root_with_plugin_storage(
    connection: &mut Connection,
    staging_id: &str,
    root: &Value,
    plugin_storage_values: Option<&[super::PluginStorageValue]>,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let mut staged_root = object(root, "Persistent root")?.clone();
    let plugin_storage_meta = staged_root.shift_remove("pluginStorageMeta");
    let plugin_storage = staged_root.shift_remove("pluginCustomStorage");
    if let Some(plugin_storage_values) = plugin_storage_values {
        let carried = carried_plugin_import_batches(&transaction)?;
        replace_plugin_storage_values(&transaction, staging_id, plugin_storage_values, &carried)?;
    } else if let Some(plugin_storage) = plugin_storage {
        let plugin_storage = plugin_storage
            .as_object()
            .ok_or_else(|| validation("pluginCustomStorage must be a JSON object"))?;
        let carried = carried_plugin_import_batches(&transaction)?;
        replace_plugin_storage(
            &transaction,
            staging_id,
            plugin_storage,
            plugin_storage_meta.as_ref().and_then(Value::as_object),
            &carried,
        )?;
    }
    put_root(&transaction, staging_id, &Value::Object(staged_root))?;
    transaction.commit()?;
    Ok(())
}

fn replace_plugin_storage_values(
    transaction: &Transaction<'_>,
    generation: &str,
    values: &[super::PluginStorageValue],
    carried: &HashMap<String, String>,
) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM plugin_storage WHERE generation = ?1",
        [generation],
    )?;
    for (ordinal, entry) in values.iter().enumerate() {
        let import_batch = plugin_owner::is_unowned(&entry.owner).then(|| {
            carried
                .get(entry.key.as_str())
                .map(String::as_str)
                .unwrap_or(generation)
        });
        put_plugin_storage(
            transaction,
            generation,
            &entry.owner,
            &entry.key,
            &entry.value,
            Some(ordinal as i64),
            import_batch,
        )?;
    }
    Ok(())
}

pub(super) fn replace_add_characters(
    connection: &mut Connection,
    staging_id: &str,
    characters: &[Value],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let mut configured_index: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM characters WHERE generation = ?1",
        [staging_id],
        |row| row.get(0),
    )?;
    for character in characters {
        validate_character(character, "Persistent data import")?;
        let character_id = required_string(character, "chaId", "Persistent data import")?;
        if character_exists(&transaction, staging_id, character_id)? {
            return Err(validation(
                "Persistent data import requires unique character IDs",
            ));
        }
        put_full_character(&transaction, staging_id, character, configured_index)?;
        configured_index += 1;
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_put_character_detail(
    connection: &mut Connection,
    staging_id: &str,
    detail: &Value,
    conversation_count: i64,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let character_id = required_string(detail, "chaId", "Persistent data import")?;
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
    put_character_records(
        &transaction,
        staging_id,
        detail,
        configured_index,
        conversation_count,
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_put_conversation_row(
    connection: &mut Connection,
    staging_id: &str,
    character_id: &str,
    configured_index: i64,
    detail: &Value,
    recent_at: i64,
    message_count: i64,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let conversation_id = required_string(detail, "id", "Persistent data import conversation")?;
    let name = required_display_name(detail, "Persistent data import conversation")?;
    let exists = transaction
        .query_row(
            "SELECT 1 FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![staging_id, character_id, conversation_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Err(validation(
            "Persistent data import requires unique, nonempty chat IDs",
        ));
    }
    put_conversation_record(
        &transaction,
        staging_id,
        character_id,
        conversation_id,
        configured_index,
        recent_at,
        name,
        message_count,
        detail,
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_add_conversation_messages(
    connection: &mut Connection,
    staging_id: &str,
    character_id: &str,
    conversation_id: &str,
    start: i64,
    messages: &[Value],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let expected_start: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM messages WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
        params![staging_id, character_id, conversation_id],
        |row| row.get(0),
    )?;
    if start != expected_start {
        return Err(validation(
            "Persistent data import message pages must be contiguous",
        ));
    }
    insert_messages(
        &transaction,
        staging_id,
        character_id,
        conversation_id,
        start,
        messages,
    )?;
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

pub(super) fn replace_put_asset_owner_heads(
    connection: &mut Connection,
    staging_id: &str,
    heads: &[AssetOwnerHead],
) -> StoreResult<()> {
    let database = super::query::materialize_staging(connection, staging_id)?;
    validate_staged_owner_heads(&database, heads)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    for head in heads {
        put_asset_owner_head(&transaction, staging_id, head)?;
    }
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_put_asset_repository_authority(
    connection: &mut Connection,
    staging_id: &str,
    authority: &AssetRepositoryAuthorityState,
) -> StoreResult<()> {
    authority.validate()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    put_asset_repository_authority(&transaction, staging_id, authority)?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn replace_preserve_repositories(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<i64> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let actual_revision = current_revision(&transaction)?;
    if let Some(expected) = expected_revision {
        if actual_revision != expected {
            return Err(StoreError::RevisionConflict {
                expected,
                actual: actual_revision,
            });
        }
    }
    let active = active_generation(&transaction)?;
    let asset_authority = read_asset_repository_authority(&transaction, &active)?;
    if matches!(
        asset_authority,
        AssetRepositoryAuthorityState::Preparing { .. }
    ) {
        return Err(validation(
            "Active asset repository generation cannot be preparing",
        ));
    }
    let source_root = replacement_root(&transaction, &active)?;
    let staged_root = replacement_root(&transaction, staging_id)?;
    let owner_heads = replacement_owner_heads(&transaction, &active)?;
    let retained_owner_heads = owner_heads
        .iter()
        .map(|head| -> StoreResult<Option<&AssetOwnerHead>> {
            let source = replacement_owner_tuple(&transaction, &active, &source_root, &head.owner)?;
            let staged =
                replacement_owner_tuple(&transaction, staging_id, &staged_root, &head.owner)?;
            Ok((source.is_some() && source == staged).then_some(head))
        })
        .collect::<StoreResult<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    preserve_archived_characters(&transaction, &active, staging_id)?;
    transaction.execute(
        "DELETE FROM asset_aliases WHERE generation = ?1",
        [staging_id],
    )?;
    transaction.execute(
        "INSERT INTO asset_aliases (
            generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata
         )
         SELECT ?1, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
         FROM asset_aliases WHERE generation = ?2",
        params![staging_id, active],
    )?;
    transaction.execute(
        "DELETE FROM asset_alias_replacement_candidates WHERE generation = ?1",
        [staging_id],
    )?;
    transaction.execute(
        "INSERT INTO asset_alias_replacement_candidates (
            generation, kind, logical_key, object_hash, byte_size
         )
         SELECT ?1, kind, logical_key, object_hash, size
         FROM asset_aliases
         WHERE generation = ?1 AND object_hash IS NOT NULL",
        [staging_id],
    )?;
    put_asset_repository_authority(&transaction, staging_id, &asset_authority)?;

    prune_proven_unreachable_forwarded_aliases(&transaction, staging_id)?;

    transaction.execute(
        "DELETE FROM asset_owner_heads WHERE generation = ?1",
        [staging_id],
    )?;
    for head in retained_owner_heads {
        put_asset_owner_head(&transaction, staging_id, head)?;
    }
    preserve_archived_owner_heads(&transaction, &active, staging_id)?;
    transaction.commit()?;
    Ok(actual_revision)
}

/// The archived rows carry their own asset ownership, which the retained-head
/// comparison cannot see because the marker detail lists nothing.
fn preserve_archived_owner_heads(
    transaction: &Transaction<'_>,
    active: &str,
    staging_id: &str,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT OR REPLACE INTO asset_owner_heads (
            generation, owner_kind, owner_locator, present, manifest_hash, entry_count
         )
         SELECT ?1, heads.owner_kind, heads.owner_locator, heads.present,
                heads.manifest_hash, heads.entry_count
         FROM asset_owner_heads AS heads
         JOIN characters AS archived
           ON archived.generation = ?2 AND archived.character_id = heads.owner_locator
         WHERE heads.generation = ?2
           AND heads.owner_kind = 'character-additional-assets'
           AND archived.archived_object IS NOT NULL",
        params![staging_id, active],
    )?;
    Ok(())
}

/// An upstream database has no archive, so a full replacement carries the
/// archived rows across instead of dropping them.
fn preserve_archived_characters(
    transaction: &Transaction<'_>,
    active: &str,
    staging_id: &str,
) -> StoreResult<()> {
    let archived = {
        let mut statement = transaction.prepare(
            "SELECT character_id, configured_index FROM characters
             WHERE generation = ?1 AND archived_object IS NOT NULL
             ORDER BY configured_index ASC",
        )?;
        let archived = statement
            .query_map([active], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        archived
    };
    if archived.is_empty() {
        return Ok(());
    }
    for (character_id, configured_index) in archived {
        if character_exists(transaction, staging_id, &character_id)? {
            return Err(validation(format!(
                "Character {character_id} is archived and the replacement carries the same character"
            )));
        }
        transaction.execute(
            "UPDATE characters SET configured_index = configured_index + 1
             WHERE generation = ?1 AND configured_index >= ?2",
            params![staging_id, configured_index],
        )?;
        transaction.execute(
            "INSERT INTO characters (
                generation, character_id, configured_index, recent_at, trashed, name, image,
                conversation_count, type, creator_notes, trash_time, detail, archived_object
             )
             SELECT ?1, character_id, ?3, recent_at, trashed, name, image,
                    conversation_count, type, creator_notes, trash_time, detail, archived_object
             FROM characters WHERE generation = ?2 AND character_id = ?4",
            params![staging_id, active, configured_index, character_id],
        )?;
    }
    Ok(())
}

fn prune_proven_unreachable_forwarded_aliases(
    transaction: &Transaction<'_>,
    generation: &str,
) -> StoreResult<()> {
    if !replacement_asset_owner_scan_is_complete(transaction, generation)? {
        return Ok(());
    }
    let plugin_rows: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM plugin_storage WHERE generation = ?1",
        [generation],
        |row| row.get(0),
    )?;
    if plugin_rows != 0 {
        return Ok(());
    }

    let mut references = BTreeSet::new();
    for query in [
        "SELECT value FROM root WHERE generation = ?1",
        "SELECT value FROM bot_presets WHERE generation = ?1",
        "SELECT detail FROM characters WHERE generation = ?1",
        "SELECT detail FROM conversations WHERE generation = ?1",
        "SELECT value FROM messages WHERE generation = ?1",
    ] {
        let mut statement = transaction.prepare(query)?;
        let mut rows = statement.query([generation])?;
        while let Some(row) = rows.next()? {
            let encoded: String = row.get(0)?;
            observe_replacement_alias_references(&serde_json::from_str(&encoded)?, &mut references);
        }
    }
    for query in [
        "SELECT image FROM bot_presets WHERE generation = ?1 AND image IS NOT NULL",
        "SELECT image FROM characters WHERE generation = ?1 AND image IS NOT NULL",
    ] {
        let mut statement = transaction.prepare(query)?;
        let mut rows = statement.query([generation])?;
        while let Some(row) = rows.next()? {
            observe_replacement_alias_text(&row.get::<_, String>(0)?, &mut references);
        }
    }

    let candidates = {
        let mut statement = transaction.prepare(
            "SELECT kind, logical_key, object_hash, byte_size
             FROM asset_alias_replacement_candidates
             WHERE generation = ?1
             ORDER BY kind ASC, logical_key ASC, object_hash ASC",
        )?;
        let candidates = statement
            .query_map([generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        candidates
    };
    for (kind, logical_key, object_hash, byte_size) in candidates {
        if references.contains(&logical_key) {
            continue;
        }
        transaction.execute(
            "DELETE FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3
               AND object_hash = ?4 AND size = ?5",
            params![generation, kind, logical_key, object_hash, byte_size],
        )?;
    }
    Ok(())
}

fn replacement_asset_owner_scan_is_complete(
    transaction: &Transaction<'_>,
    generation: &str,
) -> StoreResult<bool> {
    let root = replacement_root(transaction, generation)?;
    let root = root
        .as_object()
        .ok_or_else(|| validation("Replacement root must be an object"))?;
    match root.get("plugins") {
        None => {}
        Some(Value::Array(plugins)) if plugins.is_empty() => {}
        Some(_) => return Ok(false),
    }
    for property in ["modules", "personas"] {
        if root.get(property).is_some_and(|value| !value.is_array()) {
            return Ok(false);
        }
    }
    for module in root
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(module) = module.as_object() else {
            return Ok(false);
        };
        if module.get("assets").is_some_and(|value| !value.is_array()) {
            return Ok(false);
        }
    }
    for persona in root
        .get("personas")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(persona) = persona.as_object() else {
            return Ok(false);
        };
        let Some(module) = persona.get("embeddedModule") else {
            continue;
        };
        let Some(module) = module.as_object() else {
            return Ok(false);
        };
        if module.get("assets").is_some_and(|value| !value.is_array()) {
            return Ok(false);
        }
    }

    let mut statement = transaction
        .prepare("SELECT detail FROM characters WHERE generation = ?1 ORDER BY character_id ASC")?;
    let mut rows = statement.query([generation])?;
    while let Some(row) = rows.next()? {
        let detail: Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let Some(detail) = detail.as_object() else {
            return Ok(false);
        };
        if detail
            .get("additionalAssets")
            .is_some_and(|value| !value.is_array())
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn observe_replacement_alias_references(value: &Value, references: &mut BTreeSet<String>) {
    match value {
        Value::String(value) => observe_replacement_alias_text(value, references),
        Value::Array(values) => {
            for value in values {
                observe_replacement_alias_references(value, references);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                observe_replacement_alias_references(value, references);
            }
        }
        _ => {}
    }
}

fn observe_replacement_alias_text(value: &str, references: &mut BTreeSet<String>) {
    if value.starts_with("assets/") {
        references.insert(value.to_owned());
    }
    for prefix in ["{{inlay::", "{{inlayed::", "{{inlayeddata::"] {
        let mut remainder = value;
        while let Some(start) = remainder.find(prefix) {
            remainder = &remainder[start + prefix.len()..];
            let Some(end) = remainder.find("}}") else {
                break;
            };
            references.insert(remainder[..end].to_owned());
            remainder = &remainder[end + 2..];
        }
    }
}

fn put_asset_repository_authority(
    transaction: &Transaction<'_>,
    generation: &str,
    authority: &AssetRepositoryAuthorityState,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO asset_repository_authority (generation, value) VALUES (?1, ?2)
         ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
        params![generation, serde_json::to_string(authority)?],
    )?;
    Ok(())
}

fn validate_staged_owner_heads(database: &Value, heads: &[AssetOwnerHead]) -> StoreResult<()> {
    let mut identities = HashSet::new();
    for head in heads {
        head.validate()?;
        if !identities.insert(head.owner.storage_identity()) {
            return Err(validation("Duplicate asset owner head"));
        }
        let entries = staged_owner_entries(database, &head.owner)?;
        if head.present != entries.is_some() {
            return Err(validation(
                "Asset owner head property presence does not match its staged parent",
            ));
        }
        if head.present && head.entry_count != entries.map_or(0, |value| value.len() as i64) {
            return Err(validation(
                "Asset owner head entryCount does not match its staged parent",
            ));
        }
    }
    Ok(())
}

pub(super) fn staged_owner_entries<'a>(
    database: &'a Value,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<&'a Vec<Value>>> {
    let root = database
        .as_object()
        .ok_or_else(|| validation("Staged database must be an object"))?;
    let parent = match owner {
        AssetOwnerLocator::CharacterAdditionalAssets { character_id } => root
            .get("characters")
            .and_then(Value::as_array)
            .and_then(|characters| {
                characters.iter().find_map(|character| {
                    character.as_object().filter(|character| {
                        character.get("chaId").and_then(Value::as_str)
                            == Some(character_id.as_str())
                    })
                })
            })
            .ok_or_else(|| validation("Character asset owner occurrence does not exist"))?,
        AssetOwnerLocator::RootModuleAssets { index } => root
            .get("modules")
            .and_then(Value::as_array)
            .and_then(|modules| modules.get(*index as usize))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Root module asset owner occurrence does not exist"))?,
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => root
            .get("personas")
            .and_then(Value::as_array)
            .and_then(|personas| personas.get(*index as usize))
            .and_then(Value::as_object)
            .and_then(|persona| persona.get("embeddedModule"))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Persona module asset owner occurrence does not exist"))?,
    };
    parent
        .get(match owner {
            AssetOwnerLocator::CharacterAdditionalAssets { .. } => "additionalAssets",
            _ => "assets",
        })
        .map(|entries| {
            entries
                .as_array()
                .ok_or_else(|| validation("Asset owner property must be an array when present"))
        })
        .transpose()
}

#[derive(PartialEq)]
enum ReplacementOwnerTuple {
    Absent,
    Present(Vec<Value>),
}

fn replacement_root(connection: &Connection, generation: &str) -> StoreResult<Value> {
    let stored: String = connection.query_row(
        "SELECT value FROM root WHERE generation = ?1",
        [generation],
        |row| row.get(0),
    )?;
    let value: Value = serde_json::from_str(&stored)?;
    if !value.is_object() {
        return Err(validation("Replacement root must be an object"));
    }
    Ok(value)
}

fn replacement_owner_heads(
    connection: &Connection,
    generation: &str,
) -> StoreResult<Vec<AssetOwnerHead>> {
    selected_replacement_owner_heads(connection, generation, None, &[])
}

fn selected_replacement_owner_heads(
    connection: &Connection,
    generation: &str,
    include_root: Option<bool>,
    character_ids: &[&str],
) -> StoreResult<Vec<AssetOwnerHead>> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads WHERE generation = ?1 AND (
                ?2 IS NULL OR (?2 = 1 AND owner_kind IN ('root-module-assets', 'persona-embedded-module-assets'))
                OR (owner_kind = 'character-additional-assets' AND owner_locator IN (SELECT value FROM json_each(?3))))
             ORDER BY owner_kind ASC, owner_locator ASC",
        )?;
        let rows = statement.query_map(
            params![
                generation,
                include_root,
                serde_json::to_string(character_ids)?
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    rows.into_iter()
        .map(
            |(owner_kind, owner_locator, present, manifest_hash, entry_count)| {
                let owner = match owner_kind.as_str() {
                    "character-additional-assets" => AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: owner_locator,
                    },
                    "root-module-assets" => AssetOwnerLocator::RootModuleAssets {
                        index: replacement_owner_index(&owner_locator)?,
                    },
                    "persona-embedded-module-assets" => {
                        AssetOwnerLocator::PersonaEmbeddedModuleAssets {
                            index: replacement_owner_index(&owner_locator)?,
                        }
                    }
                    _ => return Err(validation("Stored asset owner kind is invalid")),
                };
                let head = AssetOwnerHead {
                    owner,
                    present,
                    manifest_hash,
                    entry_count,
                };
                head.validate()?;
                Ok(head)
            },
        )
        .collect()
}

fn replacement_owner_index(value: &str) -> StoreResult<i64> {
    let index = value
        .parse::<i64>()
        .map_err(|_| validation("Stored asset owner locator is invalid"))?;
    if index.to_string() != value {
        return Err(validation("Stored asset owner locator is noncanonical"));
    }
    Ok(index)
}

fn replacement_owner_tuple(
    connection: &Connection,
    generation: &str,
    root: &Value,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<ReplacementOwnerTuple>> {
    let root = root
        .as_object()
        .ok_or_else(|| validation("Replacement root must be an object"))?;
    match owner {
        AssetOwnerLocator::CharacterAdditionalAssets { character_id } => {
            let detail: Option<String> = connection
                .query_row(
                    "SELECT detail FROM characters
                     WHERE generation = ?1 AND character_id = ?2",
                    params![generation, character_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(detail) = detail else {
                return Ok(None);
            };
            replacement_owner_tuple_from_parent(&serde_json::from_str(&detail)?, "additionalAssets")
        }
        AssetOwnerLocator::RootModuleAssets { index } => {
            let Some(module) = root
                .get("modules")
                .and_then(Value::as_array)
                .and_then(|modules| {
                    usize::try_from(*index)
                        .ok()
                        .and_then(|index| modules.get(index))
                })
            else {
                return Ok(None);
            };
            replacement_owner_tuple_from_parent(module, "assets")
        }
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => {
            let Some(module) = root
                .get("personas")
                .and_then(Value::as_array)
                .and_then(|personas| {
                    usize::try_from(*index)
                        .ok()
                        .and_then(|index| personas.get(index))
                })
                .and_then(Value::as_object)
                .and_then(|persona| persona.get("embeddedModule"))
            else {
                return Ok(None);
            };
            replacement_owner_tuple_from_parent(module, "assets")
        }
    }
}

// A malformed parent or non-array property yields no tuple, so the head is
// dropped instead of failing the whole replacement. Staging accepts such
// shapes, and extraction here exists only to compare retention candidates.
fn replacement_owner_tuple_from_parent(
    parent: &Value,
    property: &str,
) -> StoreResult<Option<ReplacementOwnerTuple>> {
    let Some(parent) = parent.as_object() else {
        return Ok(None);
    };
    match parent.get(property) {
        None => Ok(Some(ReplacementOwnerTuple::Absent)),
        Some(entries) => Ok(entries
            .as_array()
            .map(|entries| ReplacementOwnerTuple::Present(entries.clone()))),
    }
}

fn put_asset_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    alias: &AssetAlias,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO asset_aliases (
            generation, logical_key, object_hash, kind, size, mime, name, ext,
            inlay_type, width, height, metadata
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(generation, kind, logical_key) DO UPDATE SET
            object_hash = excluded.object_hash,
            kind = excluded.kind,
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
            serde_json::to_string(&alias.metadata)?,
        ],
    )?;
    Ok(())
}

pub(super) fn replace_commit(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<RevisionResult> {
    replace_commit_with_app_kv(connection, staging_id, expected_revision, None)
}

pub(super) fn replace_commit_with_app_kv(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
    app_kv: Option<(&str, &Value)>,
) -> StoreResult<RevisionResult> {
    replace_commit_transaction(connection, staging_id, expected_revision, app_kv, None)
}

pub(super) fn replace_commit_from_external(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: i64,
    job: &str,
) -> StoreResult<RevisionResult> {
    replace_commit_transaction(
        connection,
        staging_id,
        Some(expected_revision),
        None,
        Some(job),
    )
}

fn replace_commit_transaction(
    connection: &mut Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
    app_kv: Option<(&str, &Value)>,
    external_job: Option<&str>,
) -> StoreResult<RevisionResult> {
    if let Some((key, _)) = app_kv {
        super::validate_app_kv_key(key)?;
    }
    let serialized_app_kv = app_kv
        .map(|(key, value)| serde_json::to_string(value).map(|value| (key, value)))
        .transpose()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    require_activatable_authority(&transaction, staging_id)?;
    let actual_revision = current_revision(&transaction)?;
    if let Some(expected) = expected_revision {
        if expected != actual_revision {
            return Err(StoreError::RevisionConflict {
                expected,
                actual: actual_revision,
            });
        }
    }

    if let Some(job) = external_job {
        super::external_storage_state::begin_receive_activation(&transaction, job)?;
    }
    let active = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    let generation = format!("revision-{revision}");
    delete_generation(&transaction, &active)?;
    move_generation(&transaction, staging_id, &generation)?;
    super::server_sync_outbox::full_replacement(&transaction)?;
    super::content_change_index::full_replacement(&transaction, &generation, revision)?;
    if external_job.is_none() {
        super::sync_selection::replaced(&transaction)?;
    }
    set_active(&transaction, revision, &generation)?;
    if let Some(job) = external_job {
        super::external_storage_state::finish_receive_activation(&transaction, job)?;
    }
    if let Some((key, value)) = serialized_app_kv {
        transaction.execute(
            "INSERT INTO app_kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    Ok(RevisionResult { revision })
}

pub(super) fn validate_replace_commit(
    connection: &Connection,
    staging_id: &str,
    expected_revision: Option<i64>,
) -> StoreResult<i64> {
    require_staging(connection, staging_id)?;
    require_activatable_authority(connection, staging_id)?;
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

fn require_activatable_authority(connection: &Connection, staging_id: &str) -> StoreResult<()> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM asset_repository_authority WHERE generation = ?1",
            [staging_id],
            |row| row.get(0),
        )
        .optional()?;
    let authority = match stored {
        Some(stored) => serde_json::from_str(&stored)
            .map_err(|_| validation("Asset repository authority state is invalid"))?,
        None => AssetRepositoryAuthorityState::Legacy,
    };
    authority.validate()?;
    if matches!(authority, AssetRepositoryAuthorityState::Preparing { .. }) {
        return Err(validation(
            "Asset repository preparing generation cannot be activated",
        ));
    }
    Ok(())
}

pub(super) fn read_asset_repository_authority(
    connection: &Connection,
    generation: &str,
) -> StoreResult<AssetRepositoryAuthorityState> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM asset_repository_authority WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(AssetRepositoryAuthorityState::Legacy);
    };
    let authority: AssetRepositoryAuthorityState = serde_json::from_str(&stored)
        .map_err(|_| validation("Asset repository authority state is invalid"))?;
    authority.validate()?;
    Ok(authority)
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
    root.shift_remove("characters");
    root.shift_remove("botPresets");
    root.shift_remove("pluginCustomStorage");
    root.shift_remove("pluginStorageMeta");
    transaction.execute(
        "INSERT INTO root (generation, value) VALUES (?1, ?2) ON CONFLICT(generation) DO UPDATE SET value = excluded.value",
        params![generation, serde_json::to_string(&root)?],
    )?;
    Ok(())
}

/// Upstream saves carry a flat object. Without an ownership sidecar every key
/// lands on the sentinel owner and the management screen assigns it later.
/// A key that still waits for an owner keeps the import it arrived in, so a
/// full replacement written afterwards cannot hand a plugin a second chance at
/// the values a person left alone.
fn carried_plugin_import_batches(
    transaction: &Transaction<'_>,
) -> StoreResult<HashMap<String, String>> {
    let active = active_generation(transaction)?;
    let mut statement = transaction.prepare(
        "SELECT storage_key, import_batch_id FROM plugin_storage
         WHERE generation = ?1 AND import_batch_id IS NOT NULL AND assigned_at IS NULL",
    )?;
    let rows = statement
        .query_map([&active], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<(String, String)>, _>>()?;
    Ok(rows.into_iter().collect())
}

pub(super) fn replace_plugin_storage(
    transaction: &Transaction<'_>,
    generation: &str,
    values: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    carried: &HashMap<String, String>,
) -> StoreResult<()> {
    transaction.execute(
        "DELETE FROM plugin_storage WHERE generation = ?1",
        [generation],
    )?;
    for (ordinal, (key, value)) in values.iter().enumerate() {
        let owner = sidecar_owner(meta, key);
        // The staging identity names this import, so a plugin that starts once
        // afterwards can take a value the save left without an owner.
        let import_batch = plugin_owner::is_unowned(owner)
            .then(|| carried.get(key.as_str()).map(String::as_str).unwrap_or(generation));
        put_plugin_storage(
            transaction,
            generation,
            owner,
            key,
            value,
            Some(ordinal as i64),
            import_batch,
        )?;
    }
    Ok(())
}

fn sidecar_owner<'a>(meta: Option<&'a Map<String, Value>>, key: &str) -> &'a str {
    meta.and_then(|meta| meta.get(key))
        .and_then(|entry| entry.get("plugin"))
        .and_then(Value::as_str)
        .filter(|owner| {
            !plugin_owner::is_unowned(owner) && plugin_owner::validate_owner(owner)
        })
        .unwrap_or(plugin_owner::UNOWNED_OWNER)
}

fn put_plugin_storage(
    transaction: &Transaction<'_>,
    generation: &str,
    owner: &str,
    key: &str,
    value: &Value,
    ordinal: Option<i64>,
    import_batch: Option<&str>,
) -> StoreResult<()> {
    if !plugin_owner::validate_owner(owner) {
        return Err(validation("plugin storage owner is invalid"));
    }
    let serialized = serde_json::to_string(value)?;
    transaction.execute(
        "INSERT INTO plugin_storage
             (generation, owner, storage_key, byte_size, ordinal, value, import_batch_id)
         VALUES (
             ?1,
             ?2,
             ?3,
             ?4,
             COALESCE(
                 ?5,
                 (SELECT COALESCE(MAX(ordinal) + 1, 0)
                  FROM plugin_storage WHERE generation = ?1)
             ),
             ?6,
             ?7
         )
         ON CONFLICT(generation, owner, storage_key) DO UPDATE SET
             byte_size = excluded.byte_size,
             value = excluded.value",
        params![
            generation,
            owner,
            key,
            serialized.len() as i64,
            ordinal,
            serialized,
            import_batch
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
        PluginStorageMutation::Set { owner, key, value } => {
            put_plugin_storage(transaction, generation, owner, key, value, None, None)
        }
        PluginStorageMutation::Delete { owner, key } => {
            transaction.execute(
                "DELETE FROM plugin_storage
                 WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                params![generation, owner, key],
            )?;
            Ok(())
        }
        PluginStorageMutation::Clear { owner } => {
            super::server_sync_outbox::capture_clear(transaction, generation, owner)?;
            transaction.execute(
                "DELETE FROM plugin_storage WHERE generation = ?1 AND owner = ?2",
                params![generation, owner],
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

pub(super) fn require_staging(connection: &Connection, staging_id: &str) -> StoreResult<()> {
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
    let configured_index = match transaction
        .query_row(
            "SELECT configured_index FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
    {
        Some(configured_index) => configured_index,
        None => transaction.query_row(
            "SELECT COUNT(*) FROM characters WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )?,
    };
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
    let configured_index = match transaction
        .query_row(
            "SELECT configured_index FROM characters WHERE generation = ?1 AND character_id = ?2",
            params![generation, character_id],
            |row| row.get(0),
        )
        .optional()?
    {
        Some(configured_index) => configured_index,
        None => transaction.query_row(
            "SELECT COALESCE(MAX(configured_index) + 1, 0) FROM characters WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )?,
    };

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
    let name = required_display_name(detail, "Character detail")?;
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
    let name = required_display_name(conversation, "Conversation")?;
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
            refresh_character_summary(transaction, generation, character_id)?;
            Ok(())
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
                let (conversation_count, append_configured_index): (i64, i64) = transaction
                    .query_row(
                        "SELECT COUNT(*), COALESCE(MAX(configured_index) + 1, 0)
                     FROM conversations WHERE generation = ?1 AND character_id = ?2",
                        params![generation, character_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                let requested_ordinal = requested_configured_index
                    .map(|configured_index| configured_index.clamp(0, conversation_count));
                let shifts_siblings = requested_ordinal
                    .is_some_and(|configured_index| configured_index < conversation_count);
                let configured_index = if let Some(ordinal) = requested_ordinal {
                    if shifts_siblings {
                        transaction.query_row(
                            "SELECT configured_index FROM conversations
                             WHERE generation = ?1 AND character_id = ?2
                             ORDER BY configured_index ASC, conversation_id ASC
                             LIMIT 1 OFFSET ?3",
                            params![generation, character_id, ordinal],
                            |row| row.get(0),
                        )?
                    } else {
                        append_configured_index
                    }
                } else {
                    append_configured_index
                };
                if shifts_siblings {
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
                refresh_character_summary(transaction, generation, character_id)?;
                return Ok(());
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
            let name = required_display_name(&detail, "Conversation")?;
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
            refresh_character_summary(transaction, generation, character_id)?;

            Ok(())
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

pub(super) fn delete_generation(
    transaction: &Transaction<'_>,
    generation: &str,
) -> StoreResult<()> {
    for (table, _) in GENERATION_TABLES.iter().rev() {
        mutate_generation_rows(transaction, table, generation, None)?;
    }
    transaction.execute(
        "DELETE FROM snapshot_leases WHERE generation = ?1",
        [generation],
    )?;
    Ok(())
}

fn move_generation(transaction: &Transaction<'_>, source: &str, target: &str) -> StoreResult<()> {
    for (table, _) in GENERATION_TABLES {
        mutate_generation_rows(transaction, table, source, Some(target))?;
    }
    Ok(())
}

// A bulk UPDATE/DELETE keeps a statement rollback journal containing the old pages.
// Android's bundled SQLite forces that journal into memory, even with temp_store=FILE.
// Bound each statement to one record while retaining the enclosing atomic transaction,
// triggers, and final revision/marker commit. Only row IDs are buffered across statements.
fn mutate_generation_rows(
    transaction: &Transaction<'_>,
    table: &str,
    source: &str,
    target: Option<&str>,
) -> StoreResult<()> {
    let mut select = transaction.prepare(&format!(
        "SELECT rowid FROM {table} WHERE generation=?1 LIMIT 256"
    ))?;
    let sql = match target {
        Some(_) => format!("UPDATE {table} SET generation=?3 WHERE rowid=?1 AND generation=?2"),
        None => format!("DELETE FROM {table} WHERE rowid=?1 AND generation=?2"),
    };
    let mut mutate = transaction.prepare(&sql)?;
    loop {
        let rows = select
            .query_map([source], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            match target {
                Some(target) => mutate.execute(params![row, source, target])?,
                None => mutate.execute(params![row, source])?,
            };
        }
    }
    Ok(())
}

pub(super) fn set_active(
    transaction: &Transaction<'_>,
    revision: i64,
    generation: &str,
) -> StoreResult<()> {
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
    object.shift_remove(field);
    Ok(Value::Object(object))
}

#[cfg(test)]
mod ordered_removal_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn without_field_preserves_the_order_of_remaining_keys() {
        let value = json!({
            "a": 1,
            "removed": true,
            "b": 2,
            "c": 3,
        });

        let result = without_field(&value, "removed").unwrap();
        let keys = result
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();

        assert_eq!(keys, ["a", "b", "c"]);
    }
}

fn object<'a>(value: &'a Value, context: &str) -> StoreResult<&'a Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| validation(format!("{context} must be a JSON object")))
}

// Upstream allows unnamed characters, groups, and conversations. IDs remain nonempty.
fn required_display_name<'a>(value: &'a Value, context: &str) -> StoreResult<&'a str> {
    value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| validation(format!("{context} requires name")))
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

/// Moves one value from the unowned side to the plugin that asked for it, in
/// the revision path so the change index and the outbox see it. Answers with
/// nothing when the plugin already holds that key or no unassigned row from
/// this import carries it, and then nothing is written.
pub(super) fn claim_unowned_plugin_value(
    connection: &mut Connection,
    owner: &str,
    key: &str,
    import_batch_id: &str,
    assigned_at: i64,
    expected_revision: i64,
) -> StoreResult<(Option<Value>, i64)> {
    if !plugin_owner::validate_owner(owner) || plugin_owner::is_unowned(owner) {
        return Err(validation("plugin storage owner is invalid"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let generation = active_generation(&transaction)?;
    let held: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
        params![generation, owner, key],
        |row| row.get(0),
    )?;
    if held != 0 {
        return Ok((None, actual_revision));
    }
    let source: Option<(i64, i64, String)> = transaction
        .query_row(
            "SELECT byte_size, ordinal, value FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3
               AND import_batch_id = ?4 AND assigned_at IS NULL",
            params![
                generation,
                plugin_owner::UNOWNED_OWNER,
                key,
                import_batch_id
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((byte_size, ordinal, serialized)) = source else {
        return Ok((None, actual_revision));
    };
    let value: Value = serde_json::from_str(&serialized)?;
    let revision = actual_revision + 1;
    super::server_sync_outbox::begin_mutation(&transaction, &generation, revision)?;
    super::content_change_index::begin_mutation(&transaction, &generation, revision, "local")?;
    // A delete and an insert, so both sides of the move reach the change index.
    transaction.execute(
        "DELETE FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
        params![generation, plugin_owner::UNOWNED_OWNER, key],
    )?;
    transaction.execute(
        "INSERT INTO plugin_storage
             (generation, owner, storage_key, byte_size, ordinal, value, claimed_from,
              import_batch_id, assigned_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            generation,
            owner,
            key,
            byte_size,
            ordinal,
            serialized,
            plugin_owner::CLAIMED_FROM_UNOWNED,
            import_batch_id,
            assigned_at
        ],
    )?;
    super::content_change_index::finish_mutation(&transaction)?;
    super::server_sync_outbox::finish_mutation(&transaction)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok((Some(value), revision))
}

/// The import whose unassigned values a plugin may still be offered, when there
/// is exactly one. Values that reached the store any other way carry no import
/// and are never offered.
pub(super) fn pending_plugin_import_batch(connection: &Connection) -> StoreResult<Option<String>> {
    let generation = active_generation(connection)?;
    let mut statement = connection.prepare(
        "SELECT DISTINCT import_batch_id FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2 AND import_batch_id IS NOT NULL
           AND assigned_at IS NULL
         LIMIT 2",
    )?;
    let batches = statement
        .query_map(params![generation, plugin_owner::UNOWNED_OWNER], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(match batches.len() {
        1 => batches.into_iter().next(),
        _ => None,
    })
}

/// What to do with a key the target plugin already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AssignCollision {
    Replace,
    Discard,
    Defer,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AssignOutcome {
    pub(crate) moved: u64,
    pub(crate) replaced: u64,
    pub(crate) discarded: u64,
    pub(crate) deferred: u64,
}

/// Hands chosen values to one plugin. A key the target already holds follows the
/// choice the person made for the whole batch, so a value never silently
/// replaces another. The move is a delete and an insert so both sides reach the
/// change index.
pub(super) fn assign_plugin_storage(
    connection: &mut Connection,
    sources: &[(String, String)],
    to_owner: &str,
    collision: AssignCollision,
    assigned_at: i64,
    expected_revision: i64,
) -> StoreResult<(AssignOutcome, i64)> {
    if !plugin_owner::validate_owner(to_owner) || plugin_owner::is_unowned(to_owner) {
        return Err(validation("plugin storage owner is invalid"));
    }
    let mut outcome = AssignOutcome::default();
    if sources.is_empty() {
        return Ok((outcome, expected_revision));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let generation = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    super::server_sync_outbox::begin_mutation(&transaction, &generation, revision)?;
    super::content_change_index::begin_mutation(&transaction, &generation, revision, "local")?;
    for (from_owner, key) in sources {
        if from_owner == to_owner {
            continue;
        }
        let source: Option<(i64, i64, String)> = transaction
            .query_row(
                "SELECT byte_size, ordinal, value FROM plugin_storage
                 WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                params![generation, from_owner, key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((byte_size, ordinal, value)) = source else {
            continue;
        };
        let held: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
            params![generation, to_owner, key],
            |row| row.get(0),
        )?;
        if held != 0 {
            match collision {
                AssignCollision::Defer => {
                    outcome.deferred += 1;
                    continue;
                }
                AssignCollision::Discard => {
                    transaction.execute(
                        "DELETE FROM plugin_storage
                         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                        params![generation, from_owner, key],
                    )?;
                    outcome.discarded += 1;
                    continue;
                }
                AssignCollision::Replace => {
                    transaction.execute(
                        "DELETE FROM plugin_storage
                         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                        params![generation, to_owner, key],
                    )?;
                    outcome.replaced += 1;
                }
            }
        }
        transaction.execute(
            "DELETE FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
            params![generation, from_owner, key],
        )?;
        transaction.execute(
            "INSERT INTO plugin_storage
                 (generation, owner, storage_key, byte_size, ordinal, value, claimed_from,
                  import_batch_id, assigned_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7)",
            params![generation, to_owner, key, byte_size, ordinal, value, assigned_at],
        )?;
        outcome.moved += 1;
    }
    super::content_change_index::finish_mutation(&transaction)?;
    super::server_sync_outbox::finish_mutation(&transaction)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok((outcome, revision))
}

/// The keys a plugin already holds among the ones a person is about to hand it.
pub(super) fn colliding_plugin_storage_keys(
    connection: &Connection,
    to_owner: &str,
    keys: &[String],
) -> StoreResult<Vec<String>> {
    let generation = active_generation(connection)?;
    let mut statement = connection.prepare(
        "SELECT 1 FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
    )?;
    let mut colliding = Vec::new();
    for key in keys {
        let held = statement
            .query_row(params![generation, to_owner, key], |_| Ok(()))
            .optional()?;
        if held.is_some() {
            colliding.push(key.clone());
        }
    }
    Ok(colliding)
}

/// One staged value a save left without an owner. The list carries sizes, never
/// the values themselves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagedPluginValue {
    pub(crate) key: String,
    pub(crate) byte_size: i64,
    pub(crate) value_type: String,
}

/// Keys a person handed to one plugin before the import is applied.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StagedPluginAssignment {
    pub(crate) owner: String,
    pub(crate) keys: Vec<String>,
}

/// What a staged save offers the one assignment pass: the values it left
/// without an owner, and the plugins it carries to hand them to.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagedPluginPreview {
    pub(crate) values: Vec<StagedPluginValue>,
    pub(crate) plugin_names: Vec<String>,
}

pub(super) fn staged_plugin_preview(
    connection: &Connection,
    staging_id: &str,
) -> StoreResult<StagedPluginPreview> {
    let mut statement = connection.prepare(
        "SELECT storage_key, byte_size,
                CASE WHEN substr(value, 1, 1) = '\"' THEN 'string' ELSE 'json' END
         FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2
         ORDER BY ordinal",
    )?;
    let values = statement
        .query_map(params![staging_id, plugin_owner::UNOWNED_OWNER], |row| {
            Ok(StagedPluginValue {
                key: row.get(0)?,
                byte_size: row.get(1)?,
                value_type: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StagedPluginPreview {
        values,
        plugin_names: staged_plugin_names(connection, staging_id)?,
    })
}

/// The plugins the staged save carries. The working set still holds the
/// database being replaced, so the names have to come from the staging.
fn staged_plugin_names(connection: &Connection, staging_id: &str) -> StoreResult<Vec<String>> {
    let plugins: Option<String> = connection
        .query_row(
            "SELECT json_extract(value, '$.plugins') FROM root WHERE generation = ?1",
            params![staging_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(plugins) = plugins else {
        return Ok(Vec::new());
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&plugins) else {
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry.get("name") {
            Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
            _ => None,
        })
        .collect())
}

/// Applies the choices made before an import is activated. Staging is invisible
/// to the change index, so this is an ordinary write. Turning automatic
/// assignment off clears the import the remaining values arrived in, which is
/// what stops a plugin from being offered them later.
pub(super) fn assign_staged_plugin_values(
    connection: &mut Connection,
    staging_id: &str,
    assignments: &[StagedPluginAssignment],
    automatic: bool,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    for assignment in assignments {
        if !plugin_owner::validate_owner(&assignment.owner)
            || plugin_owner::is_unowned(&assignment.owner)
        {
            return Err(validation("plugin storage owner is invalid"));
        }
        for key in &assignment.keys {
            let held: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM plugin_storage
                 WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                params![staging_id, assignment.owner, key],
                |row| row.get(0),
            )?;
            if held != 0 {
                continue;
            }
            transaction.execute(
                "UPDATE plugin_storage SET owner = ?2
                 WHERE generation = ?1 AND owner = ?3 AND storage_key = ?4",
                params![
                    staging_id,
                    assignment.owner,
                    plugin_owner::UNOWNED_OWNER,
                    key
                ],
            )?;
        }
    }
    if !automatic {
        transaction.execute(
            "UPDATE plugin_storage SET import_batch_id = NULL
             WHERE generation = ?1 AND owner = ?2",
            params![staging_id, plugin_owner::UNOWNED_OWNER],
        )?;
    }
    transaction.commit()?;
    Ok(())
}
