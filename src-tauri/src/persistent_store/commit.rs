use super::{
    active_generation, current_revision, AssetAlias, AssetOwnerHead, AssetOwnerLocator,
    ConversationMutation, PluginStorageMutation, RevisionResult, StagingResult, StoreError,
    StoreResult, WorkingSetCommit, GENERATION_TABLES,
};
use super::{plugin_owner, replacement_scan};
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
    super::content_change_index::begin_mutation(&transaction, &generation, revision, "local")?;
    body(&transaction, &generation, prepared)?;
    super::content_change_index::finish_mutation(&transaction)?;
    set_active(&transaction, revision, &generation)?;
    transaction.commit()?;
    Ok(RevisionResult { revision })
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
            "characters" | "botPresets" | "pluginCustomStorage" | "pluginStorageMeta" | "account"
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

pub(super) fn commit_lww(
    connection: &mut Connection,
    input: &WorkingSetCommit,
    asset_aliases: &[AssetAlias],
    header: &super::lww::Header,
    stamp: &risunest_sync_wire::stamp::Stamp,
    digest: &str,
) -> StoreResult<RevisionResult> {
    if let Some((previous, revision)) = connection.query_row(
        "SELECT digest,revision FROM lww_requests WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
    ).optional()? {
        if previous != digest { return Err(validation("request-id-integrity")); }
        return Ok(RevisionResult { revision });
    }
    let resolved;
    let write = if let Some(mutations) = &input.root_mutations {
        if input.root.is_some() {
            return Err(validation("Root and rootMutations are mutually exclusive"));
        }
        let revision = current_revision(connection)?;
        if revision != input.expected_revision {
            return Err(StoreError::RevisionConflict { expected: input.expected_revision, actual: revision });
        }
        let current = super::query::read_root(connection, &super::ReadTarget {
            generation: active_generation(connection)?, revision,
        })?;
        resolved = WorkingSetCommit {
            root: Some(apply_root_mutations(current.value, mutations)?),
            root_mutations: None,
            ..input.clone()
        };
        &resolved
    } else { input };
    commit_inner(connection, write, input, asset_aliases, Some((header, stamp, digest)))
}

/// The units a local edit can change, captured before the edit so that `record` queues exactly
/// the ones the edit changed, under the edit's stamp.
pub(super) struct LocalCapture<'a> {
    targets: &'a WorkingSetCommit,
    asset_aliases: &'a [AssetAlias],
    conversation_orders: BTreeSet<String>,
    before: std::collections::BTreeMap<risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue>,
}

impl<'a> LocalCapture<'a> {
    pub(super) fn begin(
        transaction: &Transaction<'_>,
        generation: &str,
        targets: &'a WorkingSetCommit,
        asset_aliases: &'a [AssetAlias],
        conversation_orders: BTreeSet<String>,
    ) -> StoreResult<Self> {
        let before = super::lww::capture_targets(transaction, generation, targets, asset_aliases, true, &conversation_orders)?;
        Ok(Self { targets, asset_aliases, conversation_orders, before })
    }

    pub(super) fn record(
        self,
        transaction: &Transaction<'_>,
        generation: &str,
        header: &super::lww::Header,
        stamp: &risunest_sync_wire::stamp::Stamp,
    ) -> StoreResult<()> {
        let after = super::lww::capture_targets(transaction, generation, self.targets, self.asset_aliases, false, &self.conversation_orders)?;
        let changed = super::lww::record_changes(transaction, self.before, after, stamp, header.binding_authority, &header.request_id)?;
        super::lww::refresh_orders(transaction, generation, &changed)
    }
}

fn commit_inner(
    connection: &mut Connection,
    input: &WorkingSetCommit,
    capture_targets: &WorkingSetCommit,
    asset_aliases: &[AssetAlias],
    lww: Option<(&super::lww::Header,&risunest_sync_wire::stamp::Stamp,&str)>,
) -> StoreResult<RevisionResult> {
    incremental_commit(
        connection,
        input.expected_revision,
        |transaction, active| {
            if let Some(fence) = input.gallery_pending_fence {
                if super::gallery::pending_fence(transaction)? != fence {
                    return Err(StoreError::RevisionConflict { expected: input.expected_revision, actual: current_revision(transaction)? });
                }
            }
            if input.root.as_ref().is_some_and(|root| root.get("account").is_some()) {
                return Err(validation("Account credentials are device-local"));
            }
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
                    input.delete_character_ids.as_deref().unwrap_or_default(),
                )?;
            }
            if let Some(ids) = &input.delete_character_ids {
                let unique: HashSet<_> = ids.iter().collect();
                if ids.len() > 128 || unique.len() != ids.len() || ids.iter().any(String::is_empty) {
                    return Err(validation("Character deletion requires at most 128 unique nonempty IDs"));
                }
            }
            super::lww::validate_local_targets(transaction,input)?;
            reject_archived_targets(transaction, active, input)?;
            let mut previous_root = None;
            let owner_input = owner_validation_commit(transaction, active, input, &mut previous_root)?;
            if owner_input.root.is_some() && previous_root.is_none() {
                previous_root = Some(replacement_root(transaction, active)?);
            }
            validate_changed_owner_shapes(transaction, active, &owner_input, previous_root.as_ref())?;
            validate_owner_heads_for_commit(&owner_input)?;
            let retained = retained_commit_owner_heads(transaction, active, &owner_input, previous_root.as_ref())?;
            let mut conversation_orders = BTreeSet::new();
            for mutation in input.conversations.iter().flatten() {
                match mutation {
                    ConversationMutation::Delete { character_id, .. }
                    | ConversationMutation::Reorder { character_id, .. } => {
                        conversation_orders.insert(character_id.clone());
                    }
                    ConversationMutation::ReplaceRange { character_id, conversation_id, .. } => {
                        let exists: bool = transaction.query_row(
                            "SELECT EXISTS(SELECT 1 FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3)",
                            params![active, character_id, conversation_id], |row| row.get(0),
                        )?;
                        let created_by_unit = input.unit_mutations.iter().flatten().any(|mutation| {
                            if let super::lww::UnitMutation::Set { key, .. } = mutation {
                                let parts = key.components();
                                parts.len() == 4 && parts[0] == "exists" && parts[1] == "conversation"
                                    && parts[2] == *character_id && parts[3] == *conversation_id
                            } else { false }
                        });
                        if !exists && !created_by_unit {
                            conversation_orders.insert(character_id.clone());
                        }
                    }
                }
            }
            let capture = LocalCapture::begin(transaction, active, capture_targets, asset_aliases, conversation_orders)?;
            Ok((owner_input, retained, capture))
        },
        |transaction, generation, (owner_input, retained, capture)| {
            if let Some(root) = &input.root {
                put_root(transaction, generation, root)?;
            }
            if let Some(presets) = &input.replace_presets {
                replace_presets(transaction, generation, presets)?;
            }
            for character_id in input.delete_character_ids.as_deref().unwrap_or_default() {
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
            super::lww::apply_mutations(transaction, generation, input.unit_mutations.as_deref().unwrap_or_default())?;
            for mutation in input.conversations.as_deref().unwrap_or_default() {
                apply_conversation_mutation(transaction, generation, mutation)?;
            }
            for mutation in input.plugin_storage.as_deref().unwrap_or_default() {
                apply_plugin_storage_mutation(transaction, generation, mutation)?;
            }
            for alias in asset_aliases {
                put_asset_alias(transaction, generation, alias)?;
            }
            replace_changed_owner_heads(transaction, generation, &owner_input, &retained)?;
            if let Some((header, stamp, digest)) = lww {
                capture.record(transaction, generation, header, stamp)?;
                transaction.execute("INSERT INTO lww_requests(request_id,digest,revision) VALUES(?1,?2,?3)", params![header.request_id, digest, current_revision(transaction)?+1])?;
            }
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
            ConversationMutation::Reorder { character_id, .. } => character_id,
        });
    }
    for character_id in targets {
        if input.delete_character_ids.as_deref().unwrap_or_default().iter().any(|id| id == character_id) {
            continue;
        }
        if super::archive::is_archived(transaction, generation, character_id)? {
            return Err(super::archive::archived_error(character_id));
        }
    }
    Ok(())
}

fn owner_validation_commit<'a>(
    connection: &Connection,
    generation: &str,
    input: &'a WorkingSetCommit,
    previous_root: &mut Option<Value>,
) -> StoreResult<std::borrow::Cow<'a, WorkingSetCommit>> {
    use super::lww::UnitMutation;
    let mut projected = std::borrow::Cow::Borrowed(input);
    let mut characters = HashMap::<String, Value>::new();
    for mutation in input.unit_mutations.iter().flatten() {
        let parts = mutation.key().components();
        let next = match mutation {
            UnitMutation::Set { value, .. } => Some(value),
            UnitMutation::Delete { .. } => None,
        };
        let character_id = match parts.as_slice() {
            [kind, id, field] if kind == "character" && field == "additionalAssets" => Some(id),
            [kind, scope, id] if kind == "exists" && scope == "character" => Some(id),
            _ => None,
        };
        if let Some(id) = character_id {
            let value = if let Some(value) = characters.get_mut(id) { value } else {
                let parent = input.character.iter().chain(input.character_details.iter().flatten())
                    .chain(input.replace_character.iter()).chain(input.add_character.iter())
                    .filter(|value| value.get("chaId").and_then(Value::as_str) == Some(id))
                    .last().cloned();
                let parent = if let Some(parent) = parent { parent } else {
                    let stored: Option<String> = connection.query_row(
                        "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                        params![generation, id], |row| row.get(0),
                    ).optional()?;
                    stored.map(|value| serde_json::from_str(&value)).transpose()?.unwrap_or(Value::Null)
                };
                characters.entry(id.clone()).or_insert(parent)
            };
            if parts[0] == "exists" {
                if next.is_none() {
                    *value = Value::Null;
                } else if value.is_null() {
                    *value = serde_json::json!({"chaId":id,"type":"character","name":""});
                }
            } else {
                let parent = value.as_object_mut().ok_or_else(|| validation("Character asset owner occurrence does not exist"))?;
                if let Some(next) = next { parent.insert("additionalAssets".into(), next.clone()); }
                else { parent.shift_remove("additionalAssets"); }
            }
            continue;
        }
        let (collection, id, field) = match parts.as_slice() {
            [kind, collection] if kind == "root" && matches!(collection.as_str(), "modules" | "personas") => (collection.as_str(), None, None),
            [kind, collection, id] if kind == "record" && matches!(collection.as_str(), "modules" | "personas") => (collection.as_str(), Some(id), None),
            [kind, id, field] if kind == "persona" && field == "embeddedModule" => ("personas", Some(id), Some(field)),
            [kind, scope, id] if kind == "exists" && matches!(scope.as_str(), "modules" | "persona") => (if scope == "persona" { "personas" } else { "modules" }, Some(id), None),
            _ => continue,
        };
        let projected = projected.to_mut();
        if projected.root.is_none() {
            if previous_root.is_none() { *previous_root = Some(replacement_root(connection, generation)?); }
            projected.root = previous_root.clone();
        }
        let root = projected.root.as_mut().unwrap().as_object_mut().ok_or_else(|| validation("Root must be an object"))?;
        let Some(id) = id else {
            if let Some(next) = next { root.insert(collection.into(), next.clone()); }
            else { root.shift_remove(collection); }
            continue;
        };
        let values = root.entry(collection).or_insert_with(|| serde_json::json!([]))
            .as_array_mut().ok_or_else(|| validation("Asset owner collection must be an array"))?;
        let position = values.iter().position(|value| value.get("id").and_then(Value::as_str) == Some(id));
        if let Some(field) = field {
            let position = position.unwrap_or_else(|| { values.push(serde_json::json!({"id":id})); values.len()-1 });
            let parent = values[position].as_object_mut().ok_or_else(|| validation("Persona must be an object"))?;
            if let Some(next) = next { parent.insert(field.clone(), next.clone()); }
            else { parent.shift_remove(field); }
        } else if let Some(next) = next {
            if parts[0] == "exists" {
                if position.is_none() && collection == "personas" { values.push(serde_json::json!({"id":id})); }
            } else if let Some(position) = position { values[position] = next.clone(); }
            else { values.push(next.clone()); }
        } else if let Some(position) = position { values.remove(position); }
    }
    if !characters.is_empty() {
        let projected = projected.to_mut();
        let mut parents = projected.character.iter().chain(projected.character_details.iter().flatten())
            .chain(projected.replace_character.iter()).chain(projected.add_character.iter())
            .filter_map(|value| value.get("chaId").and_then(Value::as_str).map(|id| (id.to_owned(), value.clone())))
            .collect::<HashMap<_, _>>();
        for (id, value) in characters {
            if value.is_null() {
                parents.remove(&id);
                projected.delete_character_ids.get_or_insert_default().push(id);
            } else { parents.insert(id, value); }
        }
        projected.character = None;
        projected.replace_character = None;
        projected.add_character = None;
        projected.character_details = Some(parents.into_values().collect());
    }
    // This view is only for owner validation and retention; persistence still applies the original units.
    Ok(projected)
}

fn validate_changed_owner_shapes(connection: &Connection, generation: &str, input: &WorkingSetCommit, previous_root: Option<&Value>) -> StoreResult<()> {
    let validate = |next: &Value, previous: &Value, character_id: Option<&str>| -> StoreResult<()> {
        let mut old_parents = HashMap::new();
        for (owner, parent, property) in super::record_projection::owner_parents(previous, character_id)? {
            old_parents.entry(serde_json::to_string(&owner)?).or_insert(parent.get(property));
        }
        for (owner, parent, property) in super::record_projection::owner_parents(next, character_id)? {
            let value = parent.get(property);
            let old = old_parents.get(&serde_json::to_string(&owner)?).copied().flatten();
            if value == old { continue; }
            if let Some(value) = value {
                let values = value.as_array().ok_or_else(|| validation(format!(
                    "{} {property} must be an array", character_id.unwrap_or("root"),
                )))?;
                super::record_projection::owner_tuple_shape(values, character_id.is_none())
                    .map_err(|_| validation(format!("{} {property} contains an invalid owner tuple", character_id.unwrap_or("root"))))?;
            }
        }
        Ok(())
    };
    if let Some(root) = &input.root {
        validate(root, previous_root.expect("root owner comparison"), None)?;
    }
    for character in input.character.iter().chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter()).chain(input.add_character.iter()) {
        let id = required_string(character, "chaId", "Character mutation")?;
        let old: Option<String> = connection.query_row(
            "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2", params![generation, id], |row| row.get(0),
        ).optional()?;
        let old = old.map(|value| serde_json::from_str(&value)).transpose()?.unwrap_or(Value::Null);
        validate(character, &old, Some(id))?;
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
        AssetOwnerLocator::RootModuleAssets { module_id } => input
            .root
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("modules"))
            .and_then(Value::as_array)
            .and_then(|modules| modules.iter().find(|m|m.get("id").and_then(Value::as_str)==Some(module_id)))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Root module asset owner occurrence does not exist"))?,
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { persona_id, module_id } => input
            .root
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|root| root.get("personas"))
            .and_then(Value::as_array)
            .and_then(|personas| personas.iter().find(|p|p.get("id").and_then(Value::as_str)==Some(persona_id)))
            .and_then(Value::as_object)
            .and_then(|persona| persona.get("embeddedModule")).filter(|m|m.get("id").and_then(Value::as_str)==Some(module_id))
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
    let root_parents = root_owner_parents(input.root.as_ref())?;
    for head in input.asset_owner_heads.as_deref().unwrap_or_default() {
        head.validate()?;
        let identity = head.owner.storage_identity();
        if !identities.insert(identity) {
            return Err(validation("Duplicate asset owner head"));
        }
        let entries = if matches!(head.owner, AssetOwnerLocator::CharacterAdditionalAssets { .. }) {
            owner_entries(input, &head.owner)?
        } else {
            root_parents.get(&serde_json::to_string(&head.owner.logical())?)
                .ok_or_else(|| validation("Root asset owner occurrence does not exist"))?
                .get("assets").map(|value| value.as_array().ok_or_else(|| validation("Asset owner property must be an array when present"))).transpose()?
        };
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

fn root_owner_parents(root: Option<&Value>) -> StoreResult<HashMap<String, &Value>> {
    let mut parents = HashMap::new();
    if let Some(root) = root {
        for (owner, parent, _) in super::record_projection::owner_parents(root, None)? {
            parents.entry(serde_json::to_string(&owner)?).or_insert(parent);
        }
    }
    Ok(parents)
}

fn retained_commit_owner_heads(
    connection: &Connection,
    generation: &str,
    input: &WorkingSetCommit,
    previous_root: Option<&Value>,
) -> StoreResult<Vec<AssetOwnerHead>> {
    if input.root.is_none()
        && input.character.is_none()
        && input.character_details.is_none()
        && input.replace_character.is_none()
        && input.add_character.is_none()
    {
        return Ok(Vec::new());
    }
    let old_parents = root_owner_parents(previous_root)?;
    let new_parents = root_owner_parents(input.root.as_ref())?;
    let character_ids = input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter())
        .chain(input.add_character.iter())
        .filter_map(|value| value.get("chaId").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let mut retained = Vec::new();
    for head in selected_replacement_owner_heads(
        connection,
        generation,
        Some(input.root.is_some()),
        &character_ids,
    )? {
        match &head.owner {
            AssetOwnerLocator::CharacterAdditionalAssets { character_id } => {
                if input.delete_character_ids.as_deref().unwrap_or_default().contains(character_id) {
                    continue;
                }
            }
            AssetOwnerLocator::RootModuleAssets { .. } | AssetOwnerLocator::PersonaEmbeddedModuleAssets { .. } => {
                let identity = serde_json::to_string(&head.owner.logical())?;
                if let (Some(old), Some(new)) = (old_parents.get(&identity), new_parents.get(&identity)) {
                    let old = old.get("assets");
                    let new = new.get("assets");
                    if old == new && old.is_none_or(Value::is_array) { retained.push(head); }
                }
                continue;
            }
        }
        let Ok(entries) = owner_entries(input, &head.owner) else {
            continue;
        };
        let new_tuple = Some(match entries {
            Some(entries) => ReplacementOwnerTuple::Present(entries.clone()),
            None => ReplacementOwnerTuple::Absent,
        });
        if replacement_owner_tuple(connection, generation, &serde_json::json!({}), &head.owner)? == new_tuple
        {
            retained.push(head);
        }
    }
    Ok(retained)
}


fn replace_changed_owner_heads(
    transaction: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
    retained: &[AssetOwnerHead],
) -> StoreResult<()> {
    if input.root.is_some() {
        let kept = retained.iter().map(|head| head.owner.storage_identity()).collect::<HashSet<_>>();
        for head in selected_replacement_owner_heads(transaction, generation, Some(true), &[])? {
            let (kind, locator) = head.owner.storage_identity();
            if !kept.contains(&(kind, locator.clone())) {
                transaction.execute("DELETE FROM asset_owner_heads WHERE generation=?1 AND owner_kind=?2 AND owner_locator=?3",
                    params![generation, kind, locator])?;
            }
        }
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
    for character_id in input.delete_character_ids.as_deref().unwrap_or_default() {
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
        .filter(|head| matches!(head.owner, AssetOwnerLocator::CharacterAdditionalAssets { .. }))
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
            entry_count = excluded.entry_count
         WHERE asset_owner_heads.present IS NOT excluded.present
            OR asset_owner_heads.manifest_hash IS NOT excluded.manifest_hash
            OR asset_owner_heads.entry_count IS NOT excluded.entry_count",
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
    begin_generation(&transaction, &staging_id)?;
    put_root(&transaction, &staging_id, &Value::Object(Map::new()))?;
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
    put_plugin_storage_values(transaction, generation, values, carried, 0)
}

fn put_plugin_storage_values(
    transaction: &Transaction<'_>,
    generation: &str,
    values: &[super::PluginStorageValue],
    carried: &HashMap<String, String>,
    first_ordinal: i64,
) -> StoreResult<()> {
    for (offset, entry) in values.iter().enumerate() {
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
            Some(first_ordinal + offset as i64),
            import_batch,
        )?;
    }
    Ok(())
}

fn next_plugin_storage_ordinal(transaction: &Transaction<'_>, generation: &str) -> StoreResult<i64> {
    Ok(transaction.query_row(
        "SELECT COALESCE(MAX(ordinal) + 1, 0) FROM plugin_storage WHERE generation = ?1",
        [generation],
        |row| row.get(0),
    )?)
}

/// Appends plugin values after those already staged, in the order given.
pub(super) fn replace_add_plugin_storage_values(
    connection: &mut Connection,
    staging_id: &str,
    values: &[super::PluginStorageValue],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let carried = carried_plugin_import_batches(&transaction)?;
    let first = next_plugin_storage_ordinal(&transaction, staging_id)?;
    put_plugin_storage_values(&transaction, staging_id, values, &carried, first)?;
    transaction.commit()?;
    Ok(())
}

/// Appends entries of a root's `pluginCustomStorage`, owned through the
/// matching `pluginStorageMeta` entries, as staging the whole root would.
pub(super) fn replace_add_plugin_storage(
    connection: &mut Connection,
    staging_id: &str,
    storage: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let carried = carried_plugin_import_batches(&transaction)?;
    let first = next_plugin_storage_ordinal(&transaction, staging_id)?;
    put_plugin_storage_map(&transaction, staging_id, storage, meta, &carried, first)?;
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
    let mut configured_index: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(configured_index) + 1, 0) FROM characters WHERE generation = ?1",
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
        page_staged_conversations(&transaction, staging_id, character_id)?;
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
        "SELECT COALESCE(MAX(configured_index) + 1, 0) FROM characters WHERE generation = ?1",
        [staging_id],
        |row| row.get(0),
    )?;
    put_character_records(
        &transaction,
        staging_id,
        detail,
        None,
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
    page_staged_conversation(&transaction, staging_id, character_id, conversation_id)?;
    transaction.commit()?;
    Ok(())
}

/// Stages a conversation whose messages follow in pages, with the recency it
/// would get if it were staged with its messages.
pub(super) fn replace_put_conversation(
    connection: &mut Connection,
    staging_id: &str,
    character_id: &str,
    configured_index: i64,
    conversation: &Value,
    message_count: i64,
    last_message_time: Option<&Value>,
) -> StoreResult<()> {
    replace_put_conversation_row(
        connection,
        staging_id,
        character_id,
        configured_index,
        conversation,
        conversation_recent_at(conversation, last_message_time),
        message_count,
    )
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
        "SELECT COALESCE(MAX(message_index)+1,0) FROM messages WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
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
    page_staged_conversation(&transaction, staging_id, character_id, conversation_id)?;
    transaction.commit()?;
    Ok(())
}

/// Extends staged pages after each message batch, completing the manifest when
/// all messages have arrived so activation never has to read the conversation.
pub(super) fn page_staged_conversation(
    transaction: &Transaction<'_>,
    staging_id: &str,
    character_id: &str,
    conversation_id: &str,
) -> StoreResult<()> {
    super::message_pages::stage_conversation_pages(
        transaction, staging_id, character_id, conversation_id, &|| Ok(()),
    )
}

fn page_staged_conversations(
    transaction: &Transaction<'_>,
    staging_id: &str,
    character_id: &str,
) -> StoreResult<()> {
    let mut statement = transaction
        .prepare("SELECT conversation_id FROM conversations WHERE generation = ?1 AND character_id = ?2")?;
    let conversations = statement.query_map(params![staging_id, character_id], |row| row.get::<_, String>(0))?;
    for conversation_id in conversations {
        page_staged_conversation(transaction, staging_id, character_id, &conversation_id?)?;
    }
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

/// Appends presets after those already staged.
pub(super) fn replace_add_presets(
    connection: &mut Connection,
    staging_id: &str,
    presets: &[Value],
) -> StoreResult<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_staging(&transaction, staging_id)?;
    let mut ids = HashSet::new();
    for preset in presets {
        let id = required_string(preset, "id", "Preset")?;
        let staged = transaction
            .query_row(
                "SELECT 1 FROM bot_presets WHERE generation = ?1 AND preset_id = ?2",
                params![staging_id, id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if staged || !ids.insert(id) {
            return Err(validation("Duplicate preset ID"));
        }
    }
    let first_index: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM bot_presets WHERE generation = ?1",
        [staging_id],
        |row| row.get(0),
    )?;
    put_preset_rows(&transaction, staging_id, presets, first_index)?;
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

pub(super) fn replace_preserve_repositories(
    connection: &mut Connection,
    repository_root: &std::path::Path,
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
    let source_root = super::upstream_stream::RootSpool::from_stored(&transaction, repository_root, &active)?;
    let staged_root = super::upstream_stream::RootSpool::from_stored(&transaction, repository_root, staging_id)?;
    let owner_heads = replacement_owner_heads(&transaction, &active)?;
    let retained_owner_heads = owner_heads
        .iter()
        .map(|head| -> StoreResult<Option<&AssetOwnerHead>> {
            let source = streamed_replacement_owner_tuple(&transaction, &active, &source_root, &head.owner)?;
            let staged =
                streamed_replacement_owner_tuple(&transaction, staging_id, &staged_root, &head.owner)?;
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
    prune_proven_unreachable_forwarded_aliases(&transaction, staging_id, &staged_root)?;

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
    root: &super::upstream_stream::RootSpool,
) -> StoreResult<()> {
    if !replacement_asset_owner_scan_is_complete(transaction, generation, root)? {
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
    // The spool's own decoder reads root values without a depth limit.
    root.visit_value_texts(|text| replacement_scan::observe_alias_references(text, true, &mut references))?;
    for query in [
        "SELECT value FROM bot_presets WHERE generation = ?1",
        "SELECT detail FROM characters WHERE generation = ?1",
        "SELECT detail FROM conversations WHERE generation = ?1",
        "SELECT value FROM messages WHERE generation = ?1",
    ] {
        let mut statement = transaction.prepare(query)?;
        let mut rows = statement.query([generation])?;
        while let Some(row) = rows.next()? {
            let encoded = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            replacement_scan::observe_alias_references(encoded, false, &mut references)?;
        }
    }
    for query in [
        "SELECT image FROM bot_presets WHERE generation = ?1 AND image IS NOT NULL",
        "SELECT image FROM characters WHERE generation = ?1 AND image IS NOT NULL",
    ] {
        let mut statement = transaction.prepare(query)?;
        let mut rows = statement.query([generation])?;
        while let Some(row) = rows.next()? {
            replacement_scan::observe_alias_text(&row.get::<_, String>(0)?, &mut references);
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
    root: &super::upstream_stream::RootSpool,
) -> StoreResult<bool> {
    use super::upstream_stream::FieldKind;
    match root.kind("plugins")? {
        None => {}
        Some(FieldKind::Array) if root.count("plugins")? == 0 => {}
        Some(_) => return Ok(false),
    }
    for property in ["modules", "personas"] {
        if root.kind(property)?.is_some_and(|kind| kind != FieldKind::Array) {
            return Ok(false);
        }
    }
    let mut complete=true;
    root.visit("modules",|_,module| {
        complete &= module.as_object().is_some_and(|module|module.get("assets").is_none_or(Value::is_array));
        Ok(())
    })?;
    root.visit("personas",|_,persona| {
        complete &= persona.as_object().is_some_and(|persona|persona.get("embeddedModule").is_none_or(|module|
            module.as_object().is_some_and(|module|module.get("assets").is_none_or(Value::is_array))));
        Ok(())
    })?;
    if !complete { return Ok(false); }

    let mut statement = transaction
        .prepare("SELECT detail FROM characters WHERE generation = ?1 ORDER BY character_id ASC")?;
    let mut rows = statement.query([generation])?;
    while let Some(row) = rows.next()? {
        let detail = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        if !replacement_scan::additional_assets_are_listed(detail)? {
            return Ok(false);
        }
    }
    Ok(true)
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
        AssetOwnerLocator::RootModuleAssets { module_id } => root
            .get("modules")
            .and_then(Value::as_array)
            .and_then(|modules| modules.iter().find(|m|m.get("id").and_then(Value::as_str)==Some(module_id)))
            .and_then(Value::as_object)
            .ok_or_else(|| validation("Root module asset owner occurrence does not exist"))?,
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { persona_id, module_id } => root
            .get("personas")
            .and_then(Value::as_array)
            .and_then(|personas| personas.iter().find(|p|p.get("id").and_then(Value::as_str)==Some(persona_id)))
            .and_then(Value::as_object)
            .and_then(|persona| persona.get("embeddedModule")).filter(|m|m.get("id").and_then(Value::as_str)==Some(module_id))
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
                let owner = AssetOwnerLocator::from_storage(&owner_kind,&owner_locator)?;
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

fn replacement_owner_tuple(
    connection: &Connection,
    generation: &str,
    root: &Value,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<ReplacementOwnerTuple>> {
    if !root.is_object() { return Err(validation("Replacement root must be an object")); }
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
        AssetOwnerLocator::RootModuleAssets { .. } | AssetOwnerLocator::PersonaEmbeddedModuleAssets { .. } => {
            let Some(module)=owner.root_parent(root) else{return Ok(None)};
            replacement_owner_tuple_from_parent(module,"assets")
        }
    }
}

fn streamed_replacement_owner_tuple(
    connection: &Connection,
    generation: &str,
    root: &super::upstream_stream::RootSpool,
    owner: &AssetOwnerLocator,
) -> StoreResult<Option<ReplacementOwnerTuple>> {
    let parent=match owner {
        AssetOwnerLocator::CharacterAdditionalAssets { .. } => {
            return replacement_owner_tuple(connection,generation,&serde_json::json!({}),owner);
        }
        AssetOwnerLocator::RootModuleAssets { module_id } => root.record_by_id("modules",module_id)?,
        AssetOwnerLocator::PersonaEmbeddedModuleAssets { persona_id,module_id } => {
            root.record_by_id("personas",persona_id)?
                .and_then(|mut persona|persona.as_object_mut()?.shift_remove("embeddedModule"))
                .filter(|module|module.get("id").and_then(Value::as_str)==Some(module_id))
        }
    };
    let Some(parent)=parent else { return Ok(None); };
    replacement_owner_tuple_from_parent(&parent,"assets")
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

pub(super) fn put_asset_alias(
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

/// What a non-target replacement froze in its intent.
pub(super) struct ReplacementProof<'a> {
    pub(super) base_revision: i64,
    pub(super) staging_digest: &'a str,
}

pub(super) fn replace_commit_lww(
    connection: &mut Connection,
    root: &std::path::Path,
    staging_id: &str,
    header: &super::lww::Header,
    stamp: &risunest_sync_wire::stamp::Stamp,
    digest: &str,
    changes: &[(risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue)],
    target: bool,
    received: &[super::lww::Change],
    received_authority: Option<risunest_sync_wire::stamp::DecimalU64>,
    selection_change: Option<&super::sync_selection::BindingSelectionChange>,
    replacement_proof: Option<ReplacementProof<'_>>,
) -> StoreResult<RevisionResult> {
    let rows = super::lww::FrozenRows::new(root)?;
    if target {
        for change in received { rows.insert(super::lww::intent_rows::target_row(change)?)?; }
    } else {
        for (key, value) in changes {
            rows.insert(super::lww::intent_rows::replacement_row(key, value, true)?)?;
        }
    }
    replace_commit_lww_rows(connection, root, staging_id, header, stamp, digest, &rows, target, received_authority, selection_change, replacement_proof)
}

pub(super) fn replace_commit_lww_rows(
    connection: &mut Connection,
    root: &std::path::Path,
    staging_id: &str,
    header: &super::lww::Header,
    stamp: &risunest_sync_wire::stamp::Stamp,
    digest: &str,
    changes: &super::lww::FrozenRows,
    target: bool,
    received_authority: Option<risunest_sync_wire::stamp::DecimalU64>,
    selection_change: Option<&super::sync_selection::BindingSelectionChange>,
    replacement_proof: Option<ReplacementProof<'_>>,
) -> StoreResult<RevisionResult> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let completed: Option<(String,i64)> = transaction.query_row(
        "SELECT digest,revision FROM lww_requests WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    if let Some((old,revision)) = completed {
        if old!=digest {return Err(validation("request-id-integrity"));}
        return Ok(RevisionResult {revision});
    }
    if let Some(change)=selection_change {super::sync_selection::apply_binding_selection(&transaction,change)?;}
    require_staging(&transaction, staging_id)?;
    if let Some(proof) = &replacement_proof {
        let actual = current_revision(&transaction)?;
        if actual != proof.base_revision {
            return Err(StoreError::RevisionConflict { expected: proof.base_revision, actual });
        }
        if super::lww::catalog_digest(&transaction, staging_id)? != proof.staging_digest {
            return Err(validation("replacement-stage-changed"));
        }
    }
    if target {
        super::sync_selection::validate_binding_stage_content(&transaction,staging_id)?;
        super::lww::validate_binding_source_rows(&transaction,root,staging_id,header,changes)?;
    }
    let active=active_generation(&transaction)?;
    if target {super::lww::preserve_local_root(&transaction,&active,staging_id)?;}
    let revision=current_revision(&transaction)?+1;
    let generation=staging_id;
    super::plugin_claim_eligibility::capture(&transaction, staging_id)?;
    activate_generation(&transaction,&active,generation)?;
    super::content_change_index::full_replacement(&transaction,generation,revision)?;
    if target {
        transaction.execute("DELETE FROM lww_units",[])?;
        transaction.execute("DELETE FROM lww_retired",[])?;
        transaction.execute("DELETE FROM lww_outbox",[])?;
        transaction.execute("DELETE FROM lww_receive_rows",[])?;
        changes.visit(false, |key, received_stamp, value, _| {
            let received_stamp = received_stamp.ok_or_else(|| validation("request-id-integrity"))?;
            if !super::lww::is_device(&key) {
                super::lww::put_unit(&transaction,&key,&received_stamp,&value,&header.request_id,None)?;
                super::lww::witness_received(&transaction,&super::lww::Change { key, stamp: received_stamp, value },received_authority.unwrap_or(header.binding_authority))?;
            }
            Ok(())
        })?;
        super::lww::seed_binding_holds(&transaction,staging_id,header)?;
    } else {
        changes.visit(false, |key, _, value, _| {
            if super::lww::parent_status(&transaction,&key)?=="retired"&&!(key.components()[0]=="exists"&&matches!(value,risunest_sync_wire::unit::UnitValue::Deleted)){return Ok(());}
            super::lww::put_unit(&transaction,&key,stamp,&value,&header.request_id,Some(header.binding_authority))?;
            Ok(())
        })?;
        super::lww::project_replacement_units(&transaction,generation,header,stamp,changes)?;
        changes.visit(false, |key, _, _, _| super::lww::ensure_publishable_parents(&transaction,&key,header.binding_authority))?;
    }
    changes.refresh_orders(&transaction,generation)?;
    set_active(&transaction,revision,generation)?;
    transaction.execute("INSERT INTO lww_requests(request_id,digest,revision,activated_generation) VALUES(?1,?2,?3,?4)",params![header.request_id,digest,revision,generation])?;
    transaction.commit()?;Ok(RevisionResult{revision})
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

/// Refused once the stage was activated: from then on it is the library or a
/// retired library that only the purge removes.
pub(crate) const STAGE_ACTIVATED: &str = "replacement-stage-activated";

pub(super) fn replace_abort(connection: &mut Connection, staging_id: &str) -> StoreResult<()> {
    if !staging_id.starts_with("staging-") {
        return Err(validation("Invalid staging generation"));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if generation_state(&transaction, staging_id)?.is_some_and(|state| state != "staging") {
        return Err(validation(STAGE_ACTIVATED));
    }
    delete_generation(&transaction, staging_id)?;
    transaction.commit()?;
    Ok(())
}

fn put_root(transaction: &Transaction<'_>, generation: &str, root: &Value) -> StoreResult<()> {
    let mut root = object(root, "Persistent root")?.clone();
    root.shift_remove("account");
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
pub(super) fn carried_plugin_import_batches(
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
    put_plugin_storage_map(transaction, generation, values, meta, carried, 0)
}

pub(super) fn put_plugin_storage_map(
    transaction: &Transaction<'_>,
    generation: &str,
    values: &Map<String, Value>,
    meta: Option<&Map<String, Value>>,
    carried: &HashMap<String, String>,
    first_ordinal: i64,
) -> StoreResult<()> {
    for (offset, (key, value)) in values.iter().enumerate() {
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
            Some(first_ordinal + offset as i64),
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

pub(super) fn apply_plugin_storage_mutation(
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
            transaction.execute(
                "DELETE FROM plugin_storage WHERE generation = ?1 AND owner = ?2",
                params![generation, owner],
            )?;
            Ok(())
        }
    }
}

pub(super) fn replace_presets(
    transaction: &Transaction<'_>,
    generation: &str,
    presets: &[Value],
) -> StoreResult<()> {
    let mut ids = HashSet::new();
    for preset in presets {
        let id = required_string(preset, "id", "Preset")?;
        if !ids.insert(id) { return Err(validation("Duplicate preset ID")); }
    }
    let existing: Vec<String> = {
        let mut statement = transaction.prepare("SELECT preset_id FROM bot_presets WHERE generation=?1")?;
        let rows = statement.query_map([generation], |row| row.get(0))?.collect::<Result<_, _>>()?;
        rows
    };
    for id in existing { if !ids.contains(id.as_str()) {
        transaction.execute("DELETE FROM bot_presets WHERE generation=?1 AND preset_id=?2", params![generation,id])?;
    }}
    put_preset_rows(transaction, generation, presets, 0)
}

pub(super) fn put_preset_rows(
    transaction: &Transaction<'_>,
    generation: &str,
    presets: &[Value],
    first_index: i64,
) -> StoreResult<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO bot_presets (generation, preset_id, configured_index, name, image, value)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(generation,preset_id) DO UPDATE SET configured_index=excluded.configured_index,
             name=excluded.name,image=excluded.image,value=excluded.value",
    )?;
    for (offset, preset) in presets.iter().enumerate() {
        statement.execute(params![
            generation,
            required_string(preset, "id", "Preset")?,
            first_index + offset as i64,
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
    if generation_state(connection, staging_id)?.as_deref() != Some("staging") {
        return Err(validation("Staging generation does not exist"));
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
    delete_character_ids: &[String],
) -> StoreResult<()> {
    let mut character_ids = std::collections::HashSet::new();
    for detail in details {
        let character_id = required_string(detail, "chaId", "Batch character detail mutation")?;
        if delete_character_ids.iter().any(|id| id == character_id) || !character_ids.insert(character_id) {
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

pub(super) fn put_character_detail(
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
        None,
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
    put_character_records(
        transaction,
        generation,
        character,
        Some("chats"),
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
    omitted: Option<&str>,
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
            serde_json::to_string(&WithoutField(object, omitted))?,
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
    let recent_at = conversation_recent_at(
        conversation,
        messages.last().and_then(|message| message.get("time")),
    );
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

fn conversation_recent_at(conversation: &Value, last_message_time: Option<&Value>) -> i64 {
    conversation
        .get("lastDate")
        .and_then(Value::as_i64)
        .or_else(|| last_message_time.and_then(Value::as_i64))
        .unwrap_or_default()
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
            generation, character_id, conversation_id, message_index, message_id, value, canonical_hash, canonical_size
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for (offset, message) in messages.iter().enumerate() {
        let canonical = risunest_sync_wire::payload_value::encode(message).map_err(super::lww::error)?;
        let message_id = message.get("chatId").and_then(Value::as_str);
        statement.execute(params![
            generation,
            character_id,
            conversation_id,
            start + offset as i64,
            message_id,
            serde_json::to_string(message)?,
            {
                #[cfg(test)]
                crate::persistent_store::hash_work::observe("native_message_insert", canonical.len());
                risunest_sync_wire::hash(&canonical)
            },
            canonical.len() as i64,
        ])?;
    }
    Ok(())
}

pub(super) fn apply_conversation_mutation(
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
            super::message_pages::forget_pages(transaction, generation, character_id, conversation_id)?;
            refresh_character_summary(transaction, generation, character_id)?;
            Ok(())
        }
        ConversationMutation::Reorder {
            character_id,
            conversation_ids,
        } => {
            let existing = {
                let mut statement = transaction.prepare(
                    "SELECT conversation_id, configured_index FROM conversations
                     WHERE generation = ?1 AND character_id = ?2",
                )?;
                let rows = statement.query_map(params![generation, character_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                rows.collect::<Result<HashMap<_, _>, _>>()?
            };
            let requested: HashSet<&str> = conversation_ids.iter().map(String::as_str).collect();
            if requested.len() != conversation_ids.len()
                || conversation_ids.len() != existing.len()
                || conversation_ids.iter().any(|id| !existing.contains_key(id))
            {
                return Err(validation(format!(
                    "Conversation order for {character_id} must list each conversation once"
                )));
            }
            for (configured_index, conversation_id) in conversation_ids.iter().enumerate() {
                let configured_index = configured_index as i64;
                if existing[conversation_id] == configured_index {
                    continue;
                }
                transaction.execute(
                    "UPDATE conversations SET configured_index = ?4
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![generation, character_id, conversation_id, configured_index],
                )?;
            }
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

            let stored_detail: Value;
            let detail = match conversation {
                Some(value) => value,
                None => {
                    stored_detail = serde_json::from_str(&existing_detail)?;
                    &stored_detail
                }
            };
            let name = required_display_name(detail, "Conversation")?;
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
                detail,
            )?;
            refresh_character_summary(transaction, generation, character_id)?;

            Ok(())
        }
    }
}

pub(super) fn refresh_character_summary(
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

/// Selects the first conversation when the stored `chatPage` names none of the character's
/// conversations, as upstream does when it opens such a character.
pub(super) fn reset_stale_chat_page(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<()> {
    let row: Option<(String, i64)> = transaction
        .query_row(
            "SELECT detail, conversation_count FROM characters
             WHERE generation = ?1 AND character_id = ?2 AND archived_object IS NULL",
            params![generation, character_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((detail, conversation_count)) = row else {
        return Ok(());
    };
    let mut detail: Value = serde_json::from_str(&detail)?;
    let selected = detail.get("chatPage").and_then(Value::as_u64);
    if conversation_count == 0
        || selected.is_some_and(|index| index < conversation_count as u64)
    {
        return Ok(());
    }
    detail
        .as_object_mut()
        .ok_or_else(|| validation("Character detail must be an object"))?
        .insert("chatPage".to_owned(), Value::from(0));
    transaction.execute(
        "UPDATE characters SET detail = ?3 WHERE generation = ?1 AND character_id = ?2",
        params![generation, character_id, serde_json::to_string(&detail)?],
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

pub(super) fn delete_character(
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

pub(super) fn delete_character_contents(
    transaction: &Transaction<'_>,
    generation: &str,
    character_id: &str,
) -> StoreResult<()> {
    for table in ["messages", "conversations", "message_page_indexes", "message_page_manifests"] {
        transaction.execute(
            &format!("DELETE FROM {table} WHERE generation = ?1 AND character_id = ?2"),
            params![generation, character_id],
        )?;
    }
    Ok(())
}

pub(super) fn delete_generation(
    transaction: &Transaction<'_>,
    generation: &str,
) -> StoreResult<()> {
    if active_generation(transaction)? == generation
        || generation_state(transaction, generation)?.as_deref() == Some("active")
    {
        return Err(validation("The active generation cannot be deleted"));
    }
    for (table, _) in GENERATION_TABLES.iter().rev() {
        while delete_generation_rows(transaction, table, generation, 256)? != 0 {}
    }
    while delete_generation_rows(transaction, "replacement_source_units", generation, 256)? != 0 {}
    transaction.execute(
        "DELETE FROM snapshot_leases WHERE generation = ?1",
        [generation],
    )?;
    transaction.execute("DELETE FROM generations WHERE id = ?1", [generation])?;
    super::portable_identity::forget_identity_remap(transaction, generation)?;
    Ok(())
}

// A bulk DELETE keeps a statement rollback journal containing the old pages.
// Android's bundled SQLite forces that journal into memory, even with temp_store=FILE.
// Bound each statement to one record while retaining the enclosing atomic transaction
// and its triggers. Only row IDs are buffered across statements.
fn delete_generation_rows(
    transaction: &Transaction<'_>,
    table: &str,
    generation: &str,
    limit: usize,
) -> StoreResult<usize> {
    let rows = transaction
        .prepare_cached(&format!("SELECT rowid FROM {table} WHERE generation=?1 LIMIT ?2"))?
        .query_map(params![generation, limit as i64], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut delete = transaction
        .prepare_cached(&format!("DELETE FROM {table} WHERE rowid=?1 AND generation=?2"))?;
    for row in &rows {
        delete.execute(params![row, generation])?;
    }
    Ok(rows.len())
}

pub(super) fn generation_state(
    connection: &Connection,
    generation: &str,
) -> StoreResult<Option<String>> {
    Ok(connection
        .query_row("SELECT state FROM generations WHERE id = ?1", [generation], |row| row.get(0))
        .optional()?)
}

pub(super) fn begin_generation(transaction: &Transaction<'_>, generation: &str) -> StoreResult<()> {
    transaction.execute("INSERT INTO generations (id, state) VALUES (?1, 'staging')", [generation])?;
    Ok(())
}

fn activate_generation(
    transaction: &Transaction<'_>,
    previous: &str,
    staging_id: &str,
) -> StoreResult<()> {
    let retired = transaction.execute(
        "UPDATE generations SET state = 'retired' WHERE id = ?1 AND state = 'active'",
        [previous],
    )?;
    let activated = transaction.execute(
        "UPDATE generations SET state = 'active' WHERE id = ?1 AND state = 'staging'",
        [staging_id],
    )?;
    if retired != 1 || activated != 1 {
        return Err(validation("Generation states differ from the active library"));
    }
    super::portable_identity::forget_identity_remap(transaction, staging_id)?;
    Ok(())
}

/// Hands a stage that can no longer be activated to the purge.
pub(super) fn retire_generation(transaction: &Transaction<'_>, generation: &str) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO generations (id, state) VALUES (?1, 'retired')
         ON CONFLICT(id) DO UPDATE SET state = 'retired' WHERE state = 'staging'",
        [generation],
    )?;
    super::portable_identity::forget_identity_remap(transaction, generation)?;
    Ok(())
}

/// Deletes up to `limit` rows of retired generations in one transaction and
/// forgets a generation once none of its rows remain. Returns whether retired
/// generations remain.
pub(super) fn purge_retired_batch(connection: &mut Connection, limit: usize) -> StoreResult<bool> {
    let pending: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM generations WHERE state = 'retired')",
        [],
        |row| row.get(0),
    )?;
    if !pending {
        return Ok(false);
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let retired = transaction
        .prepare("SELECT id FROM generations WHERE state = 'retired' ORDER BY id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut budget = limit.max(1);
    for generation in &retired {
        for table in GENERATION_TABLES.iter().rev().map(|(table, _)| *table).chain(["replacement_source_units"]) {
            if budget == 0 {
                break;
            }
            budget -= delete_generation_rows(&transaction, table, generation, budget)?;
        }
        if budget == 0 {
            break;
        }
        transaction.execute("DELETE FROM snapshot_leases WHERE generation = ?1", [generation])?;
        transaction.execute(
            "DELETE FROM generations WHERE id = ?1 AND state = 'retired'",
            [generation],
        )?;
    }
    let remaining: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM generations WHERE state = 'retired')",
        [],
        |row| row.get(0),
    )?;
    transaction.commit()?;
    Ok(remaining)
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

// Serializes the same text as `without_field` without copying the object.
struct WithoutField<'a>(&'a Map<String, Value>, Option<&'a str>);

impl Serialize for WithoutField<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.0.iter().filter(|(key, _)| Some(key.as_str()) != self.1))
    }
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

// Upstream allows unnamed characters and conversations. IDs remain nonempty.
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

pub(super) fn plugin_claim_source(
    connection: &Connection,
    owner: &str,
    key: &str,
    import_batch_id: &str,
    expected_revision: i64,
) -> StoreResult<Option<(i64, i64, String)>> {
    if !plugin_owner::validate_owner(owner) || plugin_owner::is_unowned(owner) {
        return Err(validation("plugin storage owner is invalid"));
    }
    let actual_revision = current_revision(connection)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let generation = active_generation(connection)?;
    let held: i64 = connection.query_row(
        "SELECT COUNT(*) FROM plugin_storage
         WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
        params![generation, owner, key],
        |row| row.get(0),
    )?;
    if held != 0 {
        return Ok(None);
    }
    Ok(connection
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
        .optional()?)
}

/// Moves one value without changing its global position. Both owner keys and
/// their order values are compared and recorded in the same transaction.
pub(super) fn claim_unowned_plugin_value(
    connection: &mut Connection,
    owner: &str,
    key: &str,
    import_batch_id: &str,
    assigned_at: i64,
    expected_revision: i64,
    header: &super::lww::Header,
    stamp: &risunest_sync_wire::stamp::Stamp,
    digest: &str,
) -> StoreResult<(Option<Value>, i64)> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some((previous, revision)) = transaction.query_row(
        "SELECT digest,revision FROM lww_requests WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
    ).optional()? {
        if previous != digest { return Err(validation("request-id-integrity")); }
        return Ok((None, revision));
    }
    let source = plugin_claim_source(&transaction, owner, key, import_batch_id, expected_revision)?;
    let Some((byte_size, ordinal, serialized)) = source else {
        return Ok((None, expected_revision));
    };
    let generation = active_generation(&transaction)?;
    let value: Value = serde_json::from_str(&serialized)?;
    let input = WorkingSetCommit {
        expected_revision,
        plugin_storage: Some(vec![
            PluginStorageMutation::Delete { owner: plugin_owner::UNOWNED_OWNER.into(), key: key.into() },
            PluginStorageMutation::Set { owner: owner.into(), key: key.into(), value: value.clone() },
        ]),
        ..Default::default()
    };
    let before = super::lww::capture_targets(&transaction, &generation, &input, &[], true, &BTreeSet::new())?;
    let revision = expected_revision + 1;
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
    let after = super::lww::capture_targets(&transaction, &generation, &input, &[], false, &BTreeSet::new())?;
    let changed = super::lww::record_changes(&transaction, before, after, stamp, header.binding_authority, &header.request_id)?;
    super::lww::refresh_orders(&transaction, &generation, &changed)?;
    transaction.execute("INSERT INTO lww_requests(request_id,digest,revision) VALUES(?1,?2,?3)", params![header.request_id, digest, revision])?;
    super::content_change_index::finish_mutation(&transaction)?;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

pub(super) fn validate_assignment_owner(to_owner: &str) -> StoreResult<()> {
    if !plugin_owner::validate_owner(to_owner) || plugin_owner::is_unowned(to_owner) {
        return Err(validation("plugin storage owner is invalid"));
    }
    Ok(())
}

enum AssignStep {
    Move { replace: bool, byte_size: i64, ordinal: i64, value: String },
    Discard,
}

/// Hands chosen values to one plugin. A key the target already holds follows the
/// choice the person made for the whole batch, so a value never silently
/// replaces another. The move is a delete and an insert so both sides reach the
/// change index, and both owners' keys and orders are recorded under one stamp.
pub(super) fn assign_plugin_storage(
    connection: &mut Connection,
    sources: &[(String, String)],
    to_owner: &str,
    collision: AssignCollision,
    assigned_at: i64,
    expected_revision: i64,
    header: &super::lww::Header,
    stamp: &risunest_sync_wire::stamp::Stamp,
    digest: &str,
) -> StoreResult<(AssignOutcome, i64)> {
    validate_assignment_owner(to_owner)?;
    let mut outcome = AssignOutcome::default();
    if sources.is_empty() {
        return Ok((outcome, expected_revision));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some((previous, revision)) = transaction.query_row(
        "SELECT digest,revision FROM lww_requests WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
    ).optional()? {
        if previous != digest { return Err(validation("request-id-integrity")); }
        return Ok((outcome, revision));
    }
    let actual_revision = current_revision(&transaction)?;
    if actual_revision != expected_revision {
        return Err(StoreError::RevisionConflict {
            expected: expected_revision,
            actual: actual_revision,
        });
    }
    let generation = active_generation(&transaction)?;
    let revision = actual_revision + 1;
    let mut steps = Vec::new();
    let mut seen = HashSet::new();
    let mut held_keys = HashSet::new();
    for (from_owner, key) in sources {
        if from_owner == to_owner || !seen.insert((from_owner.as_str(), key.as_str())) {
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
        let held = held_keys.contains(key.as_str()) || transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3)",
            params![generation, to_owner, key],
            |row| row.get::<_, bool>(0),
        )?;
        let step = match (held, collision) {
            (true, AssignCollision::Defer) => {
                outcome.deferred += 1;
                continue;
            }
            (true, AssignCollision::Discard) => {
                outcome.discarded += 1;
                AssignStep::Discard
            }
            (true, AssignCollision::Replace) => {
                outcome.replaced += 1;
                outcome.moved += 1;
                AssignStep::Move { replace: true, byte_size, ordinal, value }
            }
            (false, _) => {
                outcome.moved += 1;
                AssignStep::Move { replace: false, byte_size, ordinal, value }
            }
        };
        if matches!(step, AssignStep::Move { .. }) {
            held_keys.insert(key.as_str());
        }
        steps.push((from_owner, key, step));
    }
    let mut mutations = Vec::new();
    for (from_owner, key, step) in &steps {
        mutations.push(PluginStorageMutation::Delete { owner: (*from_owner).clone(), key: (*key).clone() });
        if let AssignStep::Move { value, .. } = step {
            mutations.push(PluginStorageMutation::Set {
                owner: to_owner.to_owned(), key: (*key).clone(), value: serde_json::from_str(value)?,
            });
        }
    }
    let input = WorkingSetCommit {
        expected_revision,
        plugin_storage: Some(mutations),
        ..Default::default()
    };
    let before = super::lww::capture_targets(&transaction, &generation, &input, &[], true, &BTreeSet::new())?;
    super::content_change_index::begin_mutation(&transaction, &generation, revision, "local")?;
    for (from_owner, key, step) in steps {
        if let AssignStep::Move { replace: true, .. } = step {
            transaction.execute(
                "DELETE FROM plugin_storage
                 WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
                params![generation, to_owner, key],
            )?;
        }
        transaction.execute(
            "DELETE FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
            params![generation, from_owner, key],
        )?;
        if let AssignStep::Move { byte_size, ordinal, value, .. } = step {
            transaction.execute(
                "INSERT INTO plugin_storage
                     (generation, owner, storage_key, byte_size, ordinal, value, claimed_from,
                      import_batch_id, assigned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, ?7)",
                params![generation, to_owner, key, byte_size, ordinal, value, assigned_at],
            )?;
        }
    }
    let after = super::lww::capture_targets(&transaction, &generation, &input, &[], false, &BTreeSet::new())?;
    let changed = super::lww::record_changes(&transaction, before, after, stamp, header.binding_authority, &header.request_id)?;
    super::lww::refresh_orders(&transaction, &generation, &changed)?;
    transaction.execute("INSERT INTO lww_requests(request_id,digest,revision) VALUES(?1,?2,?3)", params![header.request_id, digest, revision])?;
    super::content_change_index::finish_mutation(&transaction)?;
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
    repository_root: &std::path::Path,
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
        plugin_names: staged_plugin_names(connection, repository_root, staging_id)?,
    })
}

/// The plugins the staged save carries. The working set still holds the
/// database being replaced, so the names have to come from the staging.
fn staged_plugin_names(connection: &Connection, repository_root: &std::path::Path, staging_id: &str) -> StoreResult<Vec<String>> {
    let exists: bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM root WHERE generation=?1)",[staging_id],|r|r.get(0))?;
    if !exists { return Ok(Vec::new()); }
    let root=super::upstream_stream::RootSpool::from_stored(connection,repository_root,staging_id)?;
    if root.kind("plugins")? != Some(super::upstream_stream::FieldKind::Array) {
        let scalar=root.scalar("plugins")?;
        if scalar.as_ref().is_some_and(|value|value.is_number() || value.is_boolean()) {
            let _: Option<String>=connection.query_row("SELECT json_extract(value,'$.plugins') FROM root WHERE generation=?1",[staging_id],|r|r.get(0))?;
        }
        if let Some(Value::String(raw))=scalar {
            if let Ok(Value::Array(entries))=serde_json::from_str::<Value>(&raw) {
                return Ok(entries.into_iter().filter_map(|entry|entry.get("name").and_then(Value::as_str).filter(|name|!name.is_empty()).map(str::to_owned)).collect());
            }
        }
        return Ok(Vec::new());
    }
    let mut names=Vec::new();
    root.visit("plugins",|_,entry| {
        if let Some(Value::String(name))=entry.get("name") {
            if !name.is_empty() { names.push(name.clone()); }
        }
        Ok(())
    })?;
    Ok(names)
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
