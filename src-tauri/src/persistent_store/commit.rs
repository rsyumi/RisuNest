use super::{
    active_generation, current_revision, generation_is_retained, AssetAlias, AssetOwnerHead,
    AssetOwnerLocator, AssetRepositoryAuthorityState, ColdAlias, ColdPayloadAuthorityState,
    ColdPayloadMigrationInput, ConversationMutation, PluginStorageMutation, RevisionResult,
    StagingResult, StoreError, StoreResult, WorkingSetCommit, GENERATION_TABLES,
};
use crate::asset_repository::PayloadCas;
use crate::peer_sync::logical_delta::LOGICAL_MESSAGE_PAGE_SIZE;
use std::collections::{BTreeMap, BTreeSet, HashSet};

// Every incremental mutation shares one transaction skeleton (Immediate
// transaction, revision check, copy-on-write generation, incremental logical
// commit, set_active) so a new mutation type cannot omit a step. `prepare`
// runs against the active generation before the copy-on-write; `body` applies
// the mutation to the writable generation and performs its logical-index
// maintenance through the handle it receives.
fn incremental_commit<T>(
    connection: &mut Connection,
    expected_revision: i64,
    maintain_logical_index: bool,
    prepare: impl FnOnce(&Transaction<'_>, &str) -> StoreResult<T>,
    body: impl FnOnce(
        &Transaction<'_>,
        &str,
        Option<&super::logical_index::IncrementalLogicalCommit>,
        T,
    ) -> StoreResult<()>,
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
    let generation = writable_generation(&transaction, &active, revision, maintain_logical_index)?;
    let logical = super::logical_index::begin_incremental_logical_commit(
        &transaction,
        &active,
        &generation,
        revision,
    )?;
    body(&transaction, &generation, logical.as_ref(), prepared)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
}

pub(super) fn commit_asset_alias(
    connection: &mut Connection,
    cas: Option<&PayloadCas>,
    alias: &AssetAlias,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        expected_revision,
        cas.is_some(),
        |_, _| alias.validate(),
        |transaction, generation, logical, ()| {
            put_asset_alias(transaction, generation, alias)?;
            if let Some(logical) = logical {
                let cas = cas.ok_or_else(|| StoreError::Validation {
                    message: "active logical index requires a payload CAS".to_owned(),
                })?;
                super::logical_index::maintain_incremental_asset_alias(
                    transaction,
                    cas,
                    logical,
                    alias,
                )?;
            }
            Ok(())
        },
    )
}

pub(super) fn delete_asset_alias(
    connection: &mut Connection,
    maintain_logical_index: bool,
    kind: &str,
    key: &str,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    super::query::validate_asset_kind(kind)?;
    incremental_commit(
        connection,
        expected_revision,
        maintain_logical_index,
        |_, _| Ok(()),
        |transaction, generation, logical, ()| {
            transaction.execute(
                "DELETE FROM asset_aliases WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
                params![generation, kind, key],
            )?;
            if let Some(logical) = logical {
                super::logical_index::maintain_incremental_asset_alias_deletion(
                    transaction,
                    logical,
                    kind,
                    key,
                )?;
            }
            Ok(())
        },
    )
}

pub(super) fn commit_cold_alias(
    connection: &mut Connection,
    cas: Option<&PayloadCas>,
    alias: &ColdAlias,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        expected_revision,
        cas.is_some(),
        |transaction, active| {
            alias.validate()?;
            if alias.object_hash.is_none() {
                return Err(validation("Cold payload v2 alias requires an objectHash"));
            }
            require_cold_v2_authority(transaction, active)
        },
        |transaction, generation, logical, ()| {
            put_cold_alias(transaction, generation, alias)?;
            if let Some(logical) = logical {
                let cas =
                    cas.ok_or_else(|| validation("active logical index requires a payload CAS"))?;
                super::logical_index::maintain_incremental_cold_alias(
                    transaction,
                    cas,
                    logical,
                    &alias.key,
                )?;
            }
            Ok(())
        },
    )
}

pub(super) fn delete_cold_alias(
    connection: &mut Connection,
    maintain_logical_index: bool,
    key: &str,
    expected_revision: i64,
) -> StoreResult<RevisionResult> {
    ColdAlias {
        key: key.to_owned(),
        object_hash: None,
        size: 0,
        metadata: Value::Object(Map::new()),
    }
    .validate()?;
    incremental_commit(
        connection,
        expected_revision,
        maintain_logical_index,
        |transaction, active| require_cold_v2_authority(transaction, active),
        |transaction, generation, logical, ()| {
            transaction.execute(
                "DELETE FROM cold_aliases WHERE generation = ?1 AND key = ?2",
                params![generation, key],
            )?;
            if let Some(logical) = logical {
                super::logical_index::maintain_incremental_cold_alias_deletion(
                    transaction,
                    logical,
                    key,
                )?;
            }
            Ok(())
        },
    )
}

pub(super) fn activate_cold_payload_migration(
    connection: &mut Connection,
    cas: Option<&PayloadCas>,
    input: &ColdPayloadMigrationInput,
) -> StoreResult<RevisionResult> {
    input.authority().validate()?;
    let mut keys = HashSet::new();
    for alias in &input.cold_aliases {
        alias.validate()?;
        if alias.object_hash.is_none() {
            return Err(validation("Cold payload v2 alias requires an objectHash"));
        }
        if !keys.insert(alias.key.as_str()) {
            return Err(validation("Duplicate cold alias"));
        }
    }
    incremental_commit(
        connection,
        input.source_revision,
        cas.is_some(),
        |transaction, active| {
            let authority = read_cold_payload_authority(transaction, active)?;
            if !matches!(authority, ColdPayloadAuthorityState::Legacy) {
                return Err(validation(
                    "Cold payload migration requires legacy authority",
                ));
            }
            Ok(())
        },
        |transaction, generation, logical, ()| {
            transaction.execute(
                "DELETE FROM cold_aliases WHERE generation = ?1",
                [generation],
            )?;
            for alias in &input.cold_aliases {
                put_cold_alias(transaction, generation, alias)?;
            }
            put_cold_payload_authority(transaction, generation, &input.authority())?;
            if let Some(logical) = logical {
                let cas =
                    cas.ok_or_else(|| validation("active logical index requires a payload CAS"))?;
                super::logical_index::maintain_incremental_cold_alias_replacement(
                    transaction,
                    cas,
                    logical,
                )?;
            }
            Ok(())
        },
    )
}
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{Map, Value};

pub(super) fn commit(
    connection: &mut Connection,
    cas: Option<&PayloadCas>,
    input: &WorkingSetCommit,
    asset_aliases: &[AssetAlias],
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        input.expected_revision,
        cas.is_some(),
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
            validate_owner_heads_for_commit(input)?;
            if cas.is_some() {
                prior_character_conversations(transaction, active, input)
            } else {
                Ok(BTreeMap::new())
            }
        },
        |transaction, generation, logical, prior_character_conversations| {
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
            let mut conversation_changes = Vec::new();
            let mut shifted_conversation_indices = BTreeMap::new();
            for mutation in input.conversations.as_deref().unwrap_or_default() {
                let (change, shifted_index) =
                    apply_conversation_mutation(transaction, generation, mutation)?;
                if let Some((character_id, configured_index)) = shifted_index {
                    shifted_conversation_indices
                        .entry(character_id)
                        .and_modify(|current: &mut u64| *current = (*current).min(configured_index))
                        .or_insert(configured_index);
                }
                super::logical_index::push_conversation_change(&mut conversation_changes, change)?;
            }
            for mutation in input.plugin_storage.as_deref().unwrap_or_default() {
                apply_plugin_storage_mutation(transaction, generation, mutation)?;
            }
            for alias in asset_aliases {
                put_asset_alias(transaction, generation, alias)?;
            }
            replace_changed_owner_heads(transaction, generation, input)?;
            if let Some(logical) = logical {
                let cas = cas.ok_or_else(|| StoreError::Validation {
                    message: "active logical index requires a payload CAS".to_owned(),
                })?;
                super::logical_index::maintain_incremental_logical_commit(
                    transaction,
                    cas,
                    logical,
                    input,
                    &conversation_changes,
                    &shifted_conversation_indices,
                    &prior_character_conversations,
                )?;
                for alias in asset_aliases {
                    super::logical_index::maintain_incremental_asset_alias(
                        transaction,
                        cas,
                        logical,
                        alias,
                    )?;
                }
            }
            Ok(())
        },
    )
}

fn prior_character_conversations(
    transaction: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
) -> StoreResult<BTreeMap<String, Vec<String>>> {
    let mut character_ids = HashSet::new();
    if let Some(character_id) = input.delete_character_id.as_deref() {
        character_ids.insert(character_id.to_owned());
    }
    if let Some(character_id) = input
        .replace_character
        .as_ref()
        .and_then(|character| character.get("chaId"))
        .and_then(Value::as_str)
    {
        character_ids.insert(character_id.to_owned());
    }
    let mut result = BTreeMap::new();
    for character_id in character_ids {
        let mut statement = transaction.prepare_cached(
            "SELECT conversation_id FROM conversations
             WHERE generation = ?1 AND character_id = ?2 ORDER BY conversation_id ASC",
        )?;
        let conversations = statement
            .query_map(params![generation, character_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        result.insert(character_id, conversations);
    }
    Ok(result)
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

fn replace_changed_owner_heads(
    transaction: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
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
    for head in input.asset_owner_heads.as_deref().unwrap_or_default() {
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
    put_cold_payload_authority(
        &transaction,
        &staging_id,
        &ColdPayloadAuthorityState::Legacy,
    )?;
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
    if let Some(plugin_storage) = staged_root.shift_remove("pluginCustomStorage") {
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

pub(super) fn replace_put_cold_payload_authority(
    connection: &mut Connection,
    staging_id: &str,
    authority: &ColdPayloadAuthorityState,
) -> StoreResult<()> {
    authority.validate()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    put_cold_payload_authority(&transaction, staging_id, authority)?;
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
    let cold_authority = read_cold_payload_authority(&transaction, &active)?;
    if matches!(cold_authority, ColdPayloadAuthorityState::Preparing { .. }) {
        return Err(validation(
            "Active cold payload generation cannot be preparing",
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

    transaction.execute(
        "DELETE FROM cold_aliases WHERE generation = ?1",
        [staging_id],
    )?;
    transaction.execute(
        "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
         SELECT ?1, key, object_hash, size, metadata
         FROM cold_aliases WHERE generation = ?2",
        params![staging_id, active],
    )?;
    prune_proven_unreachable_forwarded_aliases(&transaction, staging_id)?;
    put_cold_payload_authority(&transaction, staging_id, &cold_authority)?;

    transaction.execute(
        "DELETE FROM asset_owner_heads WHERE generation = ?1",
        [staging_id],
    )?;
    for head in retained_owner_heads {
        put_asset_owner_head(&transaction, staging_id, head)?;
    }
    transaction.commit()?;
    Ok(actual_revision)
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
    let cold_rows: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM cold_aliases WHERE generation = ?1",
        [generation],
        |row| row.get(0),
    )?;
    if plugin_rows != 0 || cold_rows != 0 {
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

fn put_cold_payload_authority(
    transaction: &Transaction<'_>,
    generation: &str,
    authority: &ColdPayloadAuthorityState,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO cold_payload_authority (generation, value) VALUES (?1, ?2)
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
    let rows = {
        let mut statement = connection.prepare(
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads WHERE generation = ?1
             ORDER BY owner_kind ASC, owner_locator ASC",
        )?;
        let rows = statement.query_map([generation], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
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

pub(super) fn replace_put_cold_aliases(
    connection: &mut Connection,
    staging_id: &str,
    aliases: &[ColdAlias],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    for alias in aliases {
        alias.validate()?;
    }
    for alias in aliases {
        put_cold_alias(&transaction, staging_id, alias)?;
    }
    transaction.commit()?;
    Ok(())
}

fn put_cold_alias(
    transaction: &Transaction<'_>,
    generation: &str,
    alias: &ColdAlias,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(generation, key) DO UPDATE SET
            object_hash = excluded.object_hash,
            size = excluded.size,
            metadata = excluded.metadata",
        params![
            generation,
            alias.key,
            alias.object_hash,
            alias.size,
            serde_json::to_string(&alias.metadata)?,
        ],
    )?;
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

    let active = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    let generation = format!("revision-{revision}");
    super::logical_index::detach_logical_head_for_full_replace(&transaction, &active)?;
    if !generation_is_retained(&transaction, &active)? {
        delete_generation(&transaction, &active)?;
    }
    move_generation(&transaction, staging_id, &generation)?;
    set_active(&transaction, revision, &generation)?;
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
    let cold_authority = read_cold_payload_authority(connection, staging_id)?;
    if matches!(cold_authority, ColdPayloadAuthorityState::Preparing { .. }) {
        return Err(validation(
            "Cold payload preparing generation cannot be activated",
        ));
    }
    if matches!(cold_authority, ColdPayloadAuthorityState::V2 { .. }) {
        let incomplete: bool = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM cold_aliases
                WHERE generation = ?1 AND object_hash IS NULL
            )",
            [staging_id],
            |row| row.get(0),
        )?;
        if incomplete {
            return Err(validation(
                "Cold payload v2 generation contains a legacy alias",
            ));
        }
    }
    Ok(())
}

fn read_asset_repository_authority(
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

pub(super) fn read_cold_payload_authority(
    connection: &Connection,
    generation: &str,
) -> StoreResult<ColdPayloadAuthorityState> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM cold_payload_authority WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )
        .optional()?;
    let stored = stored.ok_or_else(|| validation("Cold payload authority state is missing"))?;
    let authority: ColdPayloadAuthorityState = serde_json::from_str(&stored)
        .map_err(|_| validation("Cold payload authority state is invalid"))?;
    authority.validate()?;
    Ok(authority)
}

fn require_cold_v2_authority(connection: &Connection, generation: &str) -> StoreResult<()> {
    if !matches!(
        read_cold_payload_authority(connection, generation)?,
        ColdPayloadAuthorityState::V2 { .. }
    ) {
        return Err(validation("Cold payload mutation requires v2 authority"));
    }
    Ok(())
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
) -> StoreResult<(
    super::logical_index::ConversationChange,
    Option<(String, u64)>,
)> {
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
            Ok((
                super::logical_index::ConversationChange::Delete {
                    character_id: character_id.clone(),
                    conversation_id: conversation_id.clone(),
                },
                None,
            ))
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
                refresh_character_summary(transaction, generation, character_id)?;
                let shifted_index = if configured_index < conversation_count {
                    Some((
                        character_id.clone(),
                        u64::try_from(configured_index).map_err(|_| {
                            validation("Conversation configured index must be nonnegative")
                        })?,
                    ))
                } else {
                    None
                };
                return Ok((
                    super::logical_index::ConversationChange::ReplaceRange {
                        character_id: character_id.clone(),
                        conversation_id: conversation_id.clone(),
                        old_count: 0,
                        new_count: messages.len() as u64,
                        fixed_page_ranges: Vec::new(),
                        tail_from_page: Some(0),
                        was_new: true,
                    },
                    shifted_index,
                ));
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
            refresh_character_summary(transaction, generation, character_id)?;
            let start = start as u64;
            let affected_end =
                start.saturating_add((delete_count as u64).max(messages.len() as u64));
            let first_page = start / LOGICAL_MESSAGE_PAGE_SIZE as u64;
            let (fixed_page_ranges, tail_from_page) = if delta != 0 {
                (Vec::new(), Some(first_page))
            } else if affected_end > start {
                (
                    vec![(
                        first_page,
                        (affected_end - 1) / LOGICAL_MESSAGE_PAGE_SIZE as u64 + 1,
                    )],
                    None,
                )
            } else {
                (Vec::new(), None)
            };
            Ok((
                super::logical_index::ConversationChange::ReplaceRange {
                    character_id: character_id.clone(),
                    conversation_id: conversation_id.clone(),
                    old_count: old_count as u64,
                    new_count: (old_count + delta) as u64,
                    fixed_page_ranges,
                    tail_from_page,
                    was_new: false,
                },
                None,
            ))
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
    retain_logical_generations: bool,
) -> StoreResult<String> {
    if !retain_logical_generations || !generation_is_retained(transaction, source)? {
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
