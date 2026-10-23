use super::classification::*;
use super::*;
use serde_json::{json, Map};
use device_store::plugin_values::{self, text_value as plugin_text, raw_value as plugin_raw};

const PERSONA_FIELDS: &[&str] = &[
    "personaPrompt",
    "name",
    "icon",
    "largePortrait",
    "note",
    "embeddedModule",
];
const PROTECTED_FIELDS: &[&str] = &[
    "seperateModelsForAxModels",
    "seperateModels",
    "fallbackModels",
    "fallbackWhenBlankResponse",
    "seperateParameters",
];
pub(in crate::persistent_store) fn is_device(key: &UnitKey) -> bool {
    matches!(key.components()[0].as_str(), "hypa" | "plugin-local")
}
pub(in crate::persistent_store) fn plugin_participates(db: &Connection) -> StoreResult<bool> {
    Ok(db.query_row(
        "SELECT participating FROM device_sections WHERE section='local-plugins'",
        [],
        |r| r.get(0),
    )?)
}
pub(super) fn known(key: &UnitKey) -> bool {
    let p = key.components();
    match p[0].as_str() {
        "root" => ROOT_FIELDS.contains(&p[1].as_str()),
        "character" => {
            (CHARACTER_FIELDS.contains(&p[2].as_str()) || p[2] == "statics")
                && p[2] != "chatFolders"
        }
        "conversation" => CONVERSATION_FIELDS.contains(&p[3].as_str()),
        "preset" => PRESET_FIELDS.contains(&p[2].as_str()) && p[2] != "openAIKey",
        "persona" => PERSONA_FIELDS.contains(&p[2].as_str()),
        "preset-protected" => PROTECTED_FIELDS.contains(&p[1].as_str()),
        "exists" => matches!(
            p[1].as_str(),
            "character"
                | "conversation"
                | "preset"
                | "persona"
                | "modules"
                | "loadouts"
                | "customModels"
        ),
        "record" => matches!(
            p[1].as_str(),
            "modules" | "plugins" | "loadouts" | "customModels"
        ),
        "order" => matches!(
            p[1].as_str(),
            "characters"
                | "conversations"
                | "presets"
                | "personas"
                | "modules"
                | "plugins"
                | "loadouts"
                | "customModels"
                | "plugin-storage"
        ),
        "toggle" | "variable" | "plugin" | "asset" | "inlay" | "hypa"
        | "plugin-local" | "messages" | "archive" => true,
        _ => false,
    }
}
fn text_value(
    db: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
) -> StoreResult<Option<Value>> {
    let raw: Option<String> = db.query_row(sql, parameters, |r| r.get(0)).optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
fn root(db: &Connection, generation: &str) -> StoreResult<Value> {
    Ok(text_value(
        db,
        "SELECT value FROM root WHERE generation=?1",
        [generation],
    )?
    .unwrap_or_else(|| json!({})))
}
fn add(
    db: &Connection,
    out: &mut BTreeMap<UnitKey, UnitValue>,
    parts: &[&str],
    value: &Value,
) -> StoreResult<()> {
    out.insert(unit_key(parts)?, unit_value(db, value)?);
    Ok(())
}
fn fields(
    db: &Connection,
    out: &mut BTreeMap<UnitKey, UnitValue>,
    prefix: &[&str],
    record: &Value,
    names: &[&str],
) -> StoreResult<()> {
    for field in names {
        if let Some(value) = record.get(*field) {
            let mut p = prefix.to_vec();
            p.push(field);
            add(db, out, &p, value)?;
        }
    }
    Ok(())
}
fn capture_root(
    db: &Connection,
    generation: &str,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    capture_root_fields(db, generation, None, out)
}
fn capture_root_fields(
    db: &Connection,
    generation: &str,
    selected: Option<&BTreeSet<String>>,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    let includes = |field: &str| selected.is_none_or(|fields| fields.contains(field));
    let value = root(db, generation)?;
    let root_fields: Vec<_> = ROOT_FIELDS.iter().copied().filter(|field| includes(field)).collect();
    fields(db, out, &["root"], &value, &root_fields)?;
    for (collection, id_field) in [
        ("modules", "id"),
        ("plugins", "name"),
        ("loadouts", "id"),
        ("customModels", "id"),
    ] {
        if !includes(collection) { continue; }
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        for record in value
            .get(collection)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = record
                .get(id_field)
                .and_then(Value::as_str)
                .ok_or_else(|| error("record-identity-required"))?;
            if id.is_empty() || !seen.insert(id) {
                return Err(error("record-identity-invalid"));
            }
            if collection != "plugins" {
                add(db, out, &["exists", collection, id], &json!(true))?;
            }
            let mut record = record.clone();
            if let Some(o) = record.as_object_mut() {
                o.shift_remove("configured_index");
            }
            add(db, out, &["record", collection, id], &record)?;
            order.push(id.to_owned());
        }
        if value.get(collection).is_some() {
            add(db, out, &["order", collection], &json!(order))?;
        }
    }
    let mut personas = Vec::new();
    for persona in value
        .get("personas").filter(|_| includes("personas"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = persona
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| error("persona-identity-required"))?;
        if id.is_empty() || personas.iter().any(|old| old == id) {
            return Err(error("persona-identity-invalid"));
        }
        add(db, out, &["exists", "persona", id], &json!(true))?;
        fields(db, out, &["persona", id], persona, PERSONA_FIELDS)?;
        personas.push(id.to_owned());
    }
    if includes("personas") && value.get("personas").is_some() {
        add(db, out, &["order", "personas"], &json!(personas))?;
    }
    if let Some(vars) = value
        .get("explicitGlobalChatVariables").filter(|_| includes("explicitGlobalChatVariables"))
        .and_then(Value::as_object)
    {
        for (key, value) in vars {
            add(
                db,
                out,
                &[
                    if key.starts_with("toggle_") {
                        "toggle"
                    } else {
                        "variable"
                    },
                    key,
                ],
                value,
            )?;
        }
    }
    if let Some(values) = value.get("protectedPresetValues").filter(|_| includes("protectedPresetValues")) {
        fields(db, out, &["preset-protected"], values, PROTECTED_FIELDS)?;
    }
    if let Some(order) = value.get("characterOrder").filter(|_| includes("characterOrder")) {
        add(db, out, &["order", "characters"], order)?;
    }
    Ok(())
}
fn capture_preset(
    db: &Connection,
    generation: &str,
    id: &str,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    if let Some(value) = text_value(
        db,
        "SELECT value FROM bot_presets WHERE generation=?1 AND preset_id=?2",
        params![generation, id],
    )? {
        add(db, out, &["exists", "preset", id], &json!(true))?;
        fields(db, out, &["preset", id], &value, PRESET_FIELDS)?;
    }
    Ok(())
}
fn capture_presets(
    db: &Connection,
    generation: &str,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    let ids: Vec<String> = {
        let mut s=db.prepare("SELECT preset_id FROM bot_presets WHERE generation=?1 ORDER BY configured_index,preset_id")?;
        let v = s
            .query_map([generation], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        v
    };
    for id in &ids {
        capture_preset(db, generation, id, out)?;
    }
    add(db, out, &["order", "presets"], &json!(ids))?;
    Ok(())
}
fn capture_character(
    db: &Transaction<'_>,
    generation: &str,
    id: &str,
    full: bool,
    before: bool,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    if let Some(value) = text_value(
        db,
        "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
        params![generation, id],
    )? {
        add(
            db,
            out,
            &["exists", "character", id],
            &json!({"type":value.get("type").cloned().unwrap_or(json!("character"))}),
        )?;
        fields(db, out, &["character", id], &value, CHARACTER_FIELDS)?;
        out.remove(&unit_key(&["character", id, "statics"])?);
        if let Some(mut statics) = value.get("statics").cloned() {
            if let Some(o) = statics.as_object_mut() {
                o.shift_remove("messages");
            }
            add(db, out, &["character", id, "statics"], &statics)?;
        }
        let archived: Option<String> = db.query_row(
            "SELECT archived_object FROM characters WHERE generation=?1 AND character_id=?2",
            params![generation, id],
            |r| r.get(0),
        )?;
        if let Some(hash) = archived {
            let archived = serde_json::from_str::<super::super::archive::ArchivedObject>(&hash)?;
            let key = unit_key(&["archive", id])?;
            let prior =
                read_unit(db, &key)?.and_then(|(_, value)| match archive_metadata(db, &value) {
                    Ok(metadata) if metadata.object_hash == archived.shared_object_hash => {
                        Some(value)
                    }
                    _ => None,
                });
            out.insert(
                key,
                if let Some(prior) = prior {
                    prior
                } else {
                    archive_value(db, &archived)?
                },
            );
        }
        if full {
            let ids: Vec<String> = {
                let mut s=db.prepare("SELECT conversation_id FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index,conversation_id")?;
                let v = s
                    .query_map(params![generation, id], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                v
            };
            for conv in &ids {
                capture_conversation(db, generation, id, conv, None, before, out)?;
            }
            add(
                db,
                out,
                &["order", "conversations", id],
                &json!({"ids":ids,"folders":value.get("chatFolders").cloned().unwrap_or(json!([]))}),
            )?;
        }
    }
    Ok(())
}
fn capture_character_fields(
    db: &Transaction<'_>,
    generation: &str,
    id: &str,
    fields: &BTreeSet<String>,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    if let Some(detail) = text_value(db,
        "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2", params![generation, id],
    )? {
        for field in fields {
            if let Some(mut value) = detail.get(field).cloned() {
                if field == "statics" {
                    if let Some(object) = value.as_object_mut() { object.shift_remove("messages"); }
                }
                add(db, out, &["character", id, field], &value)?;
            }
        }
    }
    Ok(())
}
fn capture_conversation(
    db: &Transaction<'_>,
    generation: &str,
    char_id: &str,
    id: &str,
    edits: Option<&[super::super::message_pages::MessageEdit]>,
    before: bool,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    if let Some(value)=text_value(db,"SELECT detail FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,char_id,id])? {
        add(db,out,&["exists","conversation",char_id,id],&json!(true))?;fields(db,out,&["conversation",char_id,id],&value,CONVERSATION_FIELDS)?;
        let unchanged=edits.is_some_and(<[_]>::is_empty)||edits.is_none()&&out.contains_key(&unit_key(&["messages",char_id,id])?);
        let manifest=if before || unchanged {super::super::message_pages::current_manifest(db,generation,char_id,id)?}else{super::super::message_pages::capture_manifest(db,generation,char_id,id,edits)?};
        out.insert(unit_key(&["messages",char_id,id])?,manifest);
    }
    Ok(())
}
pub(in crate::persistent_store) fn capture_targets(
    db: &Transaction<'_>,
    generation: &str,
    input: &WorkingSetCommit,
    aliases: &[super::super::AssetAlias],
    before: bool,
    conversation_orders: &BTreeSet<String>,
) -> StoreResult<BTreeMap<UnitKey, UnitValue>> {
    let mut out = BTreeMap::new();
    if input.root.is_some() {
        capture_root(db, generation, &mut out)?;
    } else if let Some(mutations) = &input.root_mutations {
        let fields = mutations.iter().map(|mutation| match mutation {
            super::super::RootMutation::Set { key, .. } | super::super::RootMutation::Delete { key } => key.clone(),
        }).collect();
        capture_root_fields(db, generation, Some(&fields), &mut out)?;
    }
    if input.replace_presets.is_some() {
        capture_presets(db, generation, &mut out)?;
    }
    let mut characters = BTreeMap::new();
    for value in input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
    {
        if let Some(id) = value.get("chaId").and_then(Value::as_str) {
            characters.entry(id.to_owned()).or_insert(false);
        }
    }
    for value in input
        .replace_character
        .iter()
        .chain(input.add_character.iter())
    {
        if let Some(id) = value.get("chaId").and_then(Value::as_str) {
            characters.insert(id.to_owned(), true);
        }
    }
    for id in input.delete_character_ids.iter().flatten() {
        characters.insert(id.clone(), true);
    }
    for (id, full) in characters {
        capture_character(db, generation, &id, full, before, &mut out)?;
    }
    // Every range of one conversation is captured once, from the pages the
    // ranges touch, so later ranges see the count the earlier ones left.
    let mut ranges: Vec<((&str, &str), Option<Vec<(i64, i64, i64)>>)> = Vec::new();
    for mutation in input.conversations.iter().flatten() {
        let (conversation, range) = match mutation {
            super::super::ConversationMutation::ReplaceRange {
                character_id,
                conversation_id,
                start,
                delete_count,
                messages,
                ..
            } => ((character_id.as_str(), conversation_id.as_str()), Some((*start, *delete_count, messages.len() as i64))),
            super::super::ConversationMutation::Delete {
                character_id,
                conversation_id,
            } => ((character_id.as_str(), conversation_id.as_str()), None),
            super::super::ConversationMutation::Reorder { .. } => continue,
        };
        let index = match ranges.iter().position(|(existing, _)| *existing == conversation) {
            Some(index) => index,
            None => {
                ranges.push((conversation, Some(Vec::new())));
                ranges.len() - 1
            }
        };
        match (range, &mut ranges[index].1) {
            (Some(range), Some(group)) => group.push(range),
            (None, group) => *group = None,
            (Some(_), None) => {}
        }
    }
    for ((character_id, conversation_id), group) in ranges {
        let edits = match group {
            Some(group) => conversation_edits(db, generation, character_id, conversation_id, &group, before)?,
            None => None,
        };
        capture_conversation(db, generation, character_id, conversation_id, edits.as_deref(), before, &mut out)?;
    }
    for character_id in conversation_orders {
        let ids: Vec<String> = {
            let mut s=db.prepare("SELECT conversation_id FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index,conversation_id")?;
            let v = s
                .query_map(params![generation, character_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            v
        };
        let folders = text_value(
            db,
            "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
            params![generation, character_id],
        )?
        .and_then(|v| v.get("chatFolders").cloned())
        .unwrap_or(json!([]));
        add(
            db,
            &mut out,
            &["order", "conversations", character_id],
            &json!({"ids":ids,"folders":folders}),
        )?;
    }
    for loc in input.messages_changed.iter().flatten() {
        capture_conversation(
            db,
            generation,
            &loc.character_id,
            &loc.conversation_id,
            None,
            before,
            &mut out,
        )?;
    }
    let mut character_fields: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for mutation in input.unit_mutations.iter().flatten() {
        let key = mutation.key();
        if known(key) {
            let parts = key.components();
            if parts[0] == "character" {
                character_fields.entry(parts[1].clone()).or_default().insert(parts[2].clone());
                continue;
            }
            if !before && key.components()[0] == "order" {
                out.insert(
                    key.clone(),
                    match mutation {
                        UnitMutation::Set { value, .. } => unit_value(db, value)?,
                        UnitMutation::Delete { .. } => UnitValue::Deleted,
                    },
                );
            } else {
                capture_key(db, generation, key, &mut out)?;
            }
        }
    }
    for (id, fields) in character_fields {
        capture_character_fields(db, generation, &id, &fields, &mut out)?;
    }
    for mutation in input.plugin_storage.iter().flatten() {
        match mutation {
            super::super::PluginStorageMutation::Set { owner, key, .. }
            | super::super::PluginStorageMutation::Delete { owner, key } => capture_key(
                db,
                generation,
                &unit_key(&["plugin", owner, key])?,
                &mut out,
            )?,
            super::super::PluginStorageMutation::Clear { owner } => {
                let keys: Vec<String> = {
                    let mut s = db.prepare(
                        "SELECT storage_key FROM plugin_storage WHERE generation=?1 AND owner=?2",
                    )?;
                    let v = s
                        .query_map(params![generation, owner], |r| r.get(0))?
                        .collect::<Result<_, _>>()?;
                    v
                };
                for key in keys {
                    capture_key(
                        db,
                        generation,
                        &unit_key(&["plugin", owner, &key])?,
                        &mut out,
                    )?;
                }
            }
        }
    }
    let owners = input
        .plugin_storage
        .iter()
        .flatten()
        .map(|m| match m {
            super::super::PluginStorageMutation::Set { owner, .. }
            | super::super::PluginStorageMutation::Delete { owner, .. }
            | super::super::PluginStorageMutation::Clear { owner } => owner,
        })
        .collect::<BTreeSet<_>>();
    for owner in owners {
        capture_plugin_order(db, generation, owner, &mut out)?;
    }
    for alias in aliases {
        capture_key(
            db,
            generation,
            &unit_key(&[&alias.kind, &alias.key])?,
            &mut out,
        )?;
    }
    Ok(out)
}
fn capture_key(
    db: &Transaction<'_>,
    generation: &str,
    key: &UnitKey,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    let p = key.components();
    match p[0].as_str() {
        "character" => capture_character_fields(db, generation, &p[1], &BTreeSet::from([p[2].clone()]), out)?,
        "archive" => capture_character(db, generation, &p[1], false, true, out)?,
        "order" if p[1] == "conversations" => {
            let ids: Vec<String> = {
                let mut q=db.prepare("SELECT conversation_id FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index,conversation_id")?;
                let v = q
                    .query_map(params![generation, p[2]], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                v
            };
            let folders = text_value(
                db,
                "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                params![generation, p[2]],
            )?
            .and_then(|v| v.get("chatFolders").cloned())
            .unwrap_or(json!([]));
            add(
                db,
                out,
                &["order", "conversations", &p[2]],
                &json!({"ids":ids,"folders":folders}),
            )?;
        }
        "order" if p[1] == "plugin-storage" => {
            capture_plugin_order(db, generation, &p[2], out)?;
        }
        "order" if p[1] == "presets" => {
            let ids: Vec<String> = {
                let mut q=db.prepare("SELECT preset_id FROM bot_presets WHERE generation=?1 ORDER BY configured_index,preset_id")?;
                let v = q
                    .query_map([generation], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                v
            };
            add(db, out, &["order", "presets"], &json!(ids))?;
        }
        "conversation" | "messages" => {
            capture_conversation(db, generation, &p[1], &p[2], None, true, out)?
        }
        "preset" => capture_preset(db, generation, &p[1], out)?,
        "exists" => match p[1].as_str() {
            "character" => capture_character(db, generation, &p[2], false, true, out)?,
            "conversation" => capture_conversation(db, generation, &p[2], &p[3], None, true, out)?,
            "preset" => capture_preset(db, generation, &p[2], out)?,
            _ => capture_root(db, generation, out)?,
        },
        "plugin" => {
            if let Some(value)=text_value(db,"SELECT value FROM plugin_storage WHERE generation=?1 AND owner=?2 AND storage_key=?3",params![generation,p[1],p[2]])? {out.insert(key.clone(),unit_value(db,&value)?);}
        }
        "asset" | "inlay" => {
            let alias = super::super::query::read_asset_alias(
                db,
                &p[0],
                &p[1],
                &super::super::ReadTarget {
                    generation: generation.into(),
                    revision: 0,
                },
            )?;
            if let Some(alias) = alias {
                out.insert(key.clone(), inline(&serde_json::to_value(alias.value)?)?);
            }
        }
        _ => {
            let v = root(db, generation)?;
            let selected = match p[0].as_str() {
                "root" => v.get(&p[1]).cloned(),
                "toggle" | "variable" => v
                    .get("explicitGlobalChatVariables")
                    .and_then(|v| v.get(&p[1]))
                    .cloned(),
                "preset-protected" => v
                    .get("protectedPresetValues")
                    .and_then(|v| v.get(&p[1]))
                    .cloned(),
                "persona" => v
                    .get("personas")
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items
                            .iter()
                            .find(|r| r.get("id").and_then(Value::as_str) == Some(&p[1]))
                    })
                    .and_then(|r| r.get(&p[2]))
                    .cloned(),
                "record" => v
                    .get(&p[1])
                    .and_then(Value::as_array)
                    .and_then(|items| {
                        items.iter().find(|r| {
                            r.get(if p[1] == "plugins" { "name" } else { "id" })
                                .and_then(Value::as_str)
                                == Some(&p[2])
                        })
                    })
                    .cloned(),
                "order" if p[1] == "characters" => v.get("characterOrder").cloned(),
                "order" => v.get(&p[1]).and_then(Value::as_array).map(|items| {
                    json!(items
                        .iter()
                        .filter_map(|r| r
                            .get(if p[1] == "plugins" { "name" } else { "id" })
                            .and_then(Value::as_str))
                        .collect::<Vec<_>>())
                }),
                _ => None,
            };
            if let Some(value) = selected {
                out.insert(key.clone(), unit_value(db, &value)?);
            }
        }
    }
    Ok(())
}
/// Visits one record at a time. Order arrays remain single logical unit values.
pub(in crate::persistent_store) fn visit_generation_units(
    tx: &Transaction<'_>, generation: &str,
    mut emit: impl FnMut(UnitKey, UnitValue) -> StoreResult<()>,
) -> StoreResult<()> {
    let mut flush = |values: BTreeMap<UnitKey, UnitValue>| -> StoreResult<()> {
        for (key, value) in values { emit(key, value)?; }
        Ok(())
    };
    let mut values = BTreeMap::new();
    capture_root(tx, generation, &mut values)?;
    flush(values)?;
    let mut characters = tx.prepare("SELECT character_id FROM characters WHERE generation=?1 ORDER BY character_id")?;
    let mut rows = characters.query([generation])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let mut values = BTreeMap::new();
        capture_character(tx, generation, &id, false, true, &mut values)?;
        flush(values)?;
        let Some(detail) = text_value(tx, "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2", params![generation,id])? else { continue; };
        let mut conversations = tx.prepare("SELECT conversation_id FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index,conversation_id")?;
        let mut conversations = conversations.query(params![generation,id])?;
        let mut order = Vec::<String>::new();
        while let Some(row) = conversations.next()? {
            let conversation: String = row.get(0)?;
            let mut values = BTreeMap::new();
            capture_conversation(tx, generation, &id, &conversation, None, true, &mut values)?;
            flush(values)?;
            order.push(conversation);
        }
        let mut values = BTreeMap::new();
        add(tx, &mut values, &["order","conversations",&id], &json!({"ids":order,"folders":detail.get("chatFolders").cloned().unwrap_or(json!([]))}))?;
        flush(values)?;
    }
    let mut presets = tx.prepare("SELECT preset_id FROM bot_presets WHERE generation=?1 ORDER BY configured_index,preset_id")?;
    let mut rows = presets.query([generation])?;
    let mut order = Vec::<String>::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let mut values = BTreeMap::new();
        capture_preset(tx, generation, &id, &mut values)?;
        flush(values)?;
        order.push(id);
    }
    let mut values = BTreeMap::new();
    add(tx, &mut values, &["order","presets"], &json!(order))?;
    flush(values)?;
    let mut plugins = tx.prepare("SELECT owner,storage_key FROM plugin_storage WHERE generation=?1 ORDER BY owner,storage_key")?;
    let mut rows = plugins.query([generation])?;
    while let Some(row) = rows.next()? {
        let (owner,key): (String,String) = (row.get(0)?,row.get(1)?);
        let mut values = BTreeMap::new();
        capture_key(tx, generation, &unit_key(&["plugin",&owner,&key])?, &mut values)?;
        flush(values)?;
    }
    let mut owners = tx.prepare("SELECT DISTINCT owner FROM plugin_storage WHERE generation=?1 ORDER BY owner")?;
    let mut rows = owners.query([generation])?;
    while let Some(row) = rows.next()? {
        let owner: String = row.get(0)?;
        let mut values = BTreeMap::new();
        capture_plugin_order(tx, generation, &owner, &mut values)?;
        flush(values)?;
    }
    let mut aliases = tx.prepare("SELECT kind,logical_key FROM asset_aliases WHERE generation=?1 ORDER BY kind,logical_key")?;
    let mut rows = aliases.query([generation])?;
    while let Some(row) = rows.next()? {
        let (kind,key): (String,String) = (row.get(0)?,row.get(1)?);
        let mut values = BTreeMap::new();
        capture_key(tx, generation, &unit_key(&[&kind,&key])?, &mut values)?;
        flush(values)?;
    }
    Ok(())
}

/// The whole library in one map, as replacements captured it before they
/// merged one character at a time. With `current_manifests`, a conversation
/// that already has a page manifest is read from it rather than paged again.
#[cfg(test)]
pub(in crate::persistent_store) fn capture_all(
    db: &mut Connection,
    generation: &str,
    current_manifests: bool,
) -> StoreResult<BTreeMap<UnitKey, UnitValue>> {
    let tx = db.transaction()?;
    let mut out = BTreeMap::new();
    capture_root(&tx, generation, &mut out)?;
    capture_presets(&tx, generation, &mut out)?;
    let ids: Vec<String> = {
        let mut s = tx.prepare("SELECT character_id FROM characters WHERE generation=?1")?;
        let v = s
            .query_map([generation], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        v
    };
    for id in ids {
        capture_character(&tx, generation, &id, true, current_manifests, &mut out)?;
    }
    let plugins: Vec<(String, String)> = {
        let mut s =
            tx.prepare("SELECT owner,storage_key FROM plugin_storage WHERE generation=?1")?;
        let v = s
            .query_map([generation], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        v
    };
    for (owner, key) in plugins {
        capture_key(
            &tx,
            generation,
            &unit_key(&["plugin", &owner, &key])?,
            &mut out,
        )?;
    }
    let owners: Vec<String> = {
        let mut q = tx.prepare("SELECT DISTINCT owner FROM plugin_storage WHERE generation=?1")?;
        let rows = q
            .query_map([generation], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows
    };
    for owner in owners {
        capture_plugin_order(&tx, generation, &owner, &mut out)?;
    }
    let aliases: Vec<(String, String)> = {
        let mut s = tx.prepare("SELECT kind,logical_key FROM asset_aliases WHERE generation=?1")?;
        let v = s
            .query_map([generation], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        v
    };
    for (kind, key) in aliases {
        capture_key(&tx, generation, &unit_key(&[&kind, &key])?, &mut out)?;
    }
    tx.commit()?;
    Ok(out)
}
pub(in crate::persistent_store) fn validate_received(
    _db: &Connection,
    key: &UnitKey,
    value: &UnitValue,
) -> StoreResult<()> {
    if !known(key) {
        return Ok(());
    }
    if key.components()[0] == "plugin-local" {
        return validate_plugin_unit(_db, value);
    }
    if key.components()[0] == "messages" && !matches!(value, UnitValue::Deleted) {
        return super::super::message_pages::validate_manifest(_db, value);
    }
    if key.components()[0] == "archive" && !matches!(value, UnitValue::Deleted) {
        archive_metadata(_db, value)?;
        return Ok(());
    }
    if matches!(key.components()[0].as_str(), "asset" | "inlay")
        && matches!(value, UnitValue::Object { .. })
    {
        return Err(error("invalid-unit-payload"));
    }
    if let Some(v) = validate_large_unit(_db, value)? {
        let p = key.components();
        match p[0].as_str() {
            "exists" => {
                if !(v.is_object() || v == Value::Bool(true)) {
                    return Err(error("invalid-unit-payload"));
                }
            }
            "record" => {
                if !v.is_object()
                    || v.get(if p[1] == "plugins" { "name" } else { "id" })
                        .and_then(Value::as_str)
                        != Some(&p[2])
                {
                    return Err(error("record-identity-mismatch"));
                }
            }
            "order" => {
                if p[1] == "conversations" {
                    if !v.get("ids").is_some_and(Value::is_array)
                        || !v.get("folders").is_some_and(Value::is_array)
                    {
                        return Err(error("invalid-order-value"));
                    }
                } else if !v.is_array() {
                    return Err(error("invalid-order-value"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn mutation_value(
    tx: &Transaction<'_>,
    mutation: &UnitMutation,
) -> StoreResult<Option<UnitValue>> {
    let value = match mutation {
        UnitMutation::Set { value, .. } => unit_value(tx, value)?,
        UnitMutation::Delete { .. } => UnitValue::Deleted,
    };
    let key = mutation.key();
    let p = key.components();
    if is_device(key) || p[0] == "messages" {
        return Err(error("unit-not-editable"));
    }
    if !known(key)
        && !matches!(
            p[0].as_str(),
            "root" | "character" | "conversation" | "preset" | "persona"
        )
    {
        return Err(error("unit-not-editable"));
    }
    if (p[0] == "root"
        && matches!(
            p[1].as_str(),
            "characters" | "botPresets" | "account" | "pluginCustomStorage" | "pluginStorageMeta"
        ))
        || (matches!(
            p[0].as_str(),
            "character" | "conversation" | "preset" | "persona"
        ) && matches!(
            p.last().unwrap().as_str(),
            "id" | "chaId" | "type" | "message" | "chats"
        ))
    {
        return Err(error("unit-not-editable"));
    }
    if parent_status(tx, key)? == "retired" {
        return Ok(None);
    }
    Ok(Some(value))
}
pub(in crate::persistent_store) fn apply_mutation(
    tx: &Transaction<'_>, generation: &str, mutation: &UnitMutation,
) -> StoreResult<()> {
    if let Some(value) = mutation_value(tx, mutation)? {
        apply_value(tx, generation, mutation.key(), &value, true)?;
    }
    Ok(())
}

pub(in crate::persistent_store) fn apply_mutations(
    tx: &Transaction<'_>, generation: &str, mutations: &[UnitMutation],
) -> StoreResult<()> {
    let mut root_mutations = Vec::new();
    let mut characters = BTreeMap::new();
    let flush = |root_mutations: &mut Vec<&UnitMutation>, characters: &mut BTreeMap<String, Vec<&UnitMutation>>| -> StoreResult<()> {
        if !root_mutations.is_empty() {
            let mut value = None;
            for mutation in root_mutations.drain(..) {
                if let Some(next) = mutation_value(tx, mutation)? {
                    if value.is_none() { value = Some(root(tx, generation)?); }
                    patch_root(value.as_mut().unwrap(), &mutation.key().components(), json_value_resolved(tx, &next)?)?;
                }
            }
            if let Some(value) = value { write_root(tx, generation, &value)?; }
        }
        for (id, mutations) in std::mem::take(characters) {
            let mut detail = None;
            for mutation in mutations {
                if let Some(next) = mutation_value(tx, mutation)? {
                    if detail.is_none() { detail = Some(character_detail(tx, generation, &id)?); }
                    patch_character(detail.as_mut().unwrap(), &mutation.key().components(), json_value_resolved(tx, &next)?, true)?;
                }
            }
            if let Some(detail) = detail { commit::put_character_detail(tx, generation, &detail)?; }
        }
        Ok(())
    };
    for mutation in mutations {
        let p = mutation.key().components();
        if matches!(p[0].as_str(), "root" | "toggle" | "variable" | "preset-protected" | "persona" | "record") {
            root_mutations.push(mutation);
        } else if p[0] == "character" {
            characters.entry(p[1].clone()).or_insert_with(Vec::new).push(mutation);
        } else {
            flush(&mut root_mutations, &mut characters)?;
            apply_mutation(tx, generation, mutation)?;
        }
    }
    flush(&mut root_mutations, &mut characters)
}

fn patch_root(v: &mut Value, p: &[String], next: Option<Value>) -> StoreResult<()> {
    match p[0].as_str() {
        "root" => patch(v, &p[1], next),
        "toggle" | "variable" | "preset-protected" => {
            let map = if p[0] == "preset-protected" { "protectedPresetValues" } else { "explicitGlobalChatVariables" };
            let field = v.as_object_mut().ok_or_else(|| error("invalid-root"))?.entry(map).or_insert(json!({}));
            patch(field, &p[1], next)
        }
        "persona" => patch_array(v, "personas", "id", &p[1], Some(&p[2]), next),
        "record" => patch_array(v, &p[1], if p[1] == "plugins" { "name" } else { "id" }, &p[2], None, next),
        _ => Err(error("unit-not-editable")),
    }
}

fn patch(value: &mut Value, field: &str, next: Option<Value>) -> StoreResult<()> {
    let map = value
        .as_object_mut()
        .ok_or_else(|| error("invalid-unit-record"))?;
    if let Some(v) = next {
        map.insert(field.into(), v);
    } else {
        map.shift_remove(field);
    }
    Ok(())
}
fn write_root(tx: &Connection, generation: &str, value: &Value) -> StoreResult<()> {
    tx.execute(
        "UPDATE root SET value=?2 WHERE generation=?1",
        params![generation, serde_json::to_string(value)?],
    )?;
    Ok(())
}
fn patch_array(
    root: &mut Value,
    collection: &str,
    id_field: &str,
    id: &str,
    field: Option<&str>,
    next: Option<Value>,
) -> StoreResult<()> {
    let root = root.as_object_mut().ok_or_else(|| error("invalid-root"))?;
    let list = root
        .entry(collection)
        .or_insert(json!([]))
        .as_array_mut()
        .ok_or_else(|| error("invalid-record-collection"))?;
    let index = list
        .iter()
        .position(|v| v.get(id_field).and_then(Value::as_str) == Some(id));
    match (field, next) {
        (None, None) => {
            if let Some(i) = index {
                list.remove(i);
            }
        }
        (None, Some(v)) => {
            if let Some(i) = index {
                list[i] = v;
            } else {
                list.push(v);
            }
        }
        (Some(field), next) => {
            let i = if let Some(i) = index {
                i
            } else {
                list.push(json!({id_field:id}));
                list.len() - 1
            };
            patch(&mut list[i], field, next)?;
        }
    }
    Ok(())
}
pub(in crate::persistent_store) fn apply(
    tx: &Transaction<'_>,
    generation: &str,
    key: &UnitKey,
    value: &UnitValue,
) -> StoreResult<()> {
    if key.components()[0] == "plugin-local" {
        apply_received(tx, generation, key, value).map(|_| ())
    } else {
        apply_value(tx, generation, key, value, false)
    }
}

pub(in crate::persistent_store) fn apply_received(
    tx: &Transaction<'_>, generation: &str, key: &UnitKey, value: &UnitValue,
) -> StoreResult<bool> {
    let changed = plugin_storage_changed(tx, generation, key, value)?;
    apply_value(tx, generation, key, value, false)?;
    let p = key.components();
    if changed && p[0] == "plugin-local" {
        plugin_values::advance_remote_sequence(tx, &p[1], &p[2], &p[3])?;
    }
    Ok(changed)
}

fn plugin_storage_changed(tx: &Transaction<'_>, generation: &str, key: &UnitKey, value: &UnitValue) -> StoreResult<bool> {
    let p = key.components();
    let current = if p[0] == "plugin-local" {
        if !plugin_participates(tx)? { return Ok(false); }
        plugin_unit_value(tx, &p[1], &p[2], &p[3])?
    } else if p[0] == "plugin" {
        let mut values = BTreeMap::new();
        capture_key(tx, generation, key, &mut values)?;
        values.remove(key).unwrap_or(UnitValue::Deleted)
    } else { return Ok(false); };
    Ok(wire(current.identity())? != wire(value.identity())?)
}
/// The character whose detail `apply` patches for this key, when the patch
/// touches nothing else.
pub(in crate::persistent_store) fn character_detail_key(key: &UnitKey) -> Option<String> {
    let mut p = key.components();
    (matches!(p[0].as_str(), "character") && known(key)).then(|| p.swap_remove(1))
}
pub(in crate::persistent_store) fn character_detail(
    tx: &Transaction<'_>,
    generation: &str,
    id: &str,
) -> StoreResult<Value> {
    text_value(
        tx,
        "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
        params![generation, id],
    )?
    .ok_or_else(|| error("parent-missing"))
}
/// Patches a held character detail the way `apply` would, for a key that
/// `character_detail_key` accepts.
pub(in crate::persistent_store) fn patch_character_detail(
    tx: &Transaction<'_>,
    detail: &mut Value,
    key: &UnitKey,
    value: &UnitValue,
) -> StoreResult<()> {
    patch_character(detail, &key.components(), json_value_resolved(tx, value)?, false)
}
fn patch_character(v: &mut Value, p: &[String], next: Option<Value>, local: bool) -> StoreResult<()> {
    if p[2] == "statics" {
        let messages = if local {
            None
        } else {
            v.get("statics").and_then(|v| v.get("messages")).cloned()
        };
        let mut next = next.unwrap_or(json!({}));
        if let Some(messages) = messages {
            patch(&mut next, "messages", Some(messages))?;
        }
        patch(v, "statics", Some(next))?;
    } else {
        patch(v, &p[2], next)?;
    }
    Ok(())
}
fn apply_value(
    tx: &Transaction<'_>,
    generation: &str,
    key: &UnitKey,
    value: &UnitValue,
    local: bool,
) -> StoreResult<()> {
    if !local && !known(key) {
        return Ok(());
    }
    let p = key.components();
    if p[0] == "messages" {
        if matches!(value, UnitValue::Deleted) {
            return Ok(());
        }
        return super::super::message_pages::apply_manifest(tx, generation, &p[1], &p[2], value);
    }
    if p[0] == "archive" {
        if matches!(value, UnitValue::Deleted) {
            tx.execute("UPDATE characters SET archived_object=NULL WHERE generation=?1 AND character_id=?2",params![generation,p[1]])?;
        } else {
            let mut archived = archive_metadata(tx, value)?;
            if let Some(mut local) =
                super::super::archive::read_archived_object(tx, generation, &p[1])?
            {
                local.shared_object_hash = archived.shared_object_hash;
                local.shared_asset_hashes = archived.shared_asset_hashes;
                local.archived_at = archived.archived_at;
                archived = local;
            }
            let detail = text_value(
                tx,
                "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                params![generation, p[1]],
            )?
            .ok_or_else(|| error("parent-missing"))?;
            let marker = super::super::archive::marker_detail(
                &p[1],
                detail
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                detail
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("character"),
            );
            commit::delete_character_contents(tx, generation, &p[1])?;
            tx.execute("UPDATE characters SET archived_object=?3,detail=?4,conversation_count=0 WHERE generation=?1 AND character_id=?2",params![generation,p[1],serde_json::to_string(&archived)?,serde_json::to_string(&marker)?])?;
        }
        return Ok(());
    }
    if p[0] == "plugin-local" && !plugin_participates(tx)? {
        return Ok(());
    }
    if p[0] == "plugin-local" {
        if let UnitValue::Object { descriptor, .. } = value {
            let (rowid, length) = plugin_text::object_row(tx, &descriptor.object_hash)?;
            if plugin_raw::is_object(tx, &descriptor.object_hash)? {
                if p[2] != "json" { return Err(error("plugin-local-namespace-invalid")); }
                validate_plugin_unit(tx, value)?;
                let info = plugin_raw::inspect_object(tx, &descriptor.object_hash)?;
                let (clock, writer): (String, String) = tx.query_row("SELECT write_clock,writer_id FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3", params![p[1],p[2],p[3]], |row| Ok((row.get(0)?, row.get(1)?))).optional()?.unwrap_or(("0".into(), String::new()));
                return plugin_raw::write_reserved(tx, &p[1], &p[3], &info, &clock, &writer, |output| {
                    let mut input = tx.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
                    std::io::copy(&mut input, output)?; Ok(())
                });
            }
            if length > plugin_text::PART_BYTES as u64 {
                validate_plugin_unit(tx, value)?;
                let info = plugin_text::inspect_object(tx, &descriptor.object_hash)?;
                let (clock, writer): (String, String) = tx.query_row(
                    "SELECT write_clock,writer_id FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3",
                    params![p[1], p[2], p[3]], |row| Ok((row.get(0)?, row.get(1)?))).optional()?.unwrap_or(("0".into(), String::new()));
                return plugin_values::write_reserved(tx, &p[1], &p[2], &p[3], info.byte_length, &clock, &writer, |output| {
                    let input = tx.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
                    if plugin_text::decode(input, output, plugin_text::MAX_VALUE_BYTES)? != info {
                        return Err(error("invalid-large-unit"));
                    }
                    Ok(())
                });
            }
        }
    }
    let next = json_value_resolved(tx, value)?;
    match p[0].as_str() {
        "root" | "toggle" | "variable" | "preset-protected" | "persona" | "record" => {
            let mut v = root(tx, generation)?;
            patch_root(&mut v, &p, next)?;
            write_root(tx, generation, &v)?;
        }
        "character" => {
            let mut v = character_detail(tx, generation, &p[1])?;
            patch_character(&mut v, &p, next, local)?;
            commit::put_character_detail(tx, generation, &v)?;
        }
        "conversation" => {
            let mut v=text_value(tx,"SELECT detail FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,p[1],p[2]])?.ok_or_else(||error("parent-missing"))?;
            patch(&mut v, &p[3], next)?;
            tx.execute("UPDATE conversations SET detail=?4,name=?5,recent_at=CASE WHEN ?7='lastDate' THEN ?6 ELSE recent_at END WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,p[1],p[2],serde_json::to_string(&v)?,v.get("name").and_then(Value::as_str).unwrap_or_default(),v.get("lastDate").and_then(Value::as_i64).unwrap_or_default(),p[3]])?;
        }
        "preset" => {
            let mut v = text_value(
                tx,
                "SELECT value FROM bot_presets WHERE generation=?1 AND preset_id=?2",
                params![generation, p[1]],
            )?
            .ok_or_else(|| error("parent-missing"))?;
            patch(&mut v, &p[2], next)?;
            tx.execute("UPDATE bot_presets SET value=?3,name=?4,image=?5 WHERE generation=?1 AND preset_id=?2",params![generation,p[1],serde_json::to_string(&v)?,v.get("name").and_then(Value::as_str).unwrap_or_default(),v.get("image").and_then(Value::as_str)])?;
        }
        "exists" => {
            if next.is_none() {
                match p[1].as_str() {
                    "character" => commit::delete_character(tx, generation, &p[2])?,
                    "conversation" => {
                        let present: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3)",
                            params![generation, p[2], p[3]],
                            |r| r.get(0),
                        )?;
                        commit::apply_conversation_mutation(
                            tx,
                            generation,
                            &super::super::ConversationMutation::Delete {
                                character_id: p[2].clone(),
                                conversation_id: p[3].clone(),
                            },
                        )?;
                        if present {
                            commit::reset_stale_chat_page(tx, generation, &p[2])?;
                        }
                    }
                    "preset" => {
                        tx.execute(
                            "DELETE FROM bot_presets WHERE generation=?1 AND preset_id=?2",
                            params![generation, p[2]],
                        )?;
                    }
                    collection => {
                        let mut v = root(tx, generation)?;
                        patch_array(
                            &mut v,
                            if collection == "persona" {
                                "personas"
                            } else {
                                collection
                            },
                            "id",
                            &p[2],
                            None,
                            None,
                        )?;
                        write_root(tx, generation, &v)?;
                    }
                }
            } else {
                match p[1].as_str() {
                    "character" => {
                        if text_value(
                            tx,
                            "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                            params![generation, p[2]],
                        )?
                        .is_none()
                        {
                            // `chatPage` never syncs, so a character first seen here
                            // selects its first conversation, as a new upstream one does.
                            commit::put_character_detail(
                                tx,
                                generation,
                                &json!({"chaId":p[2],"name":"","type":next.unwrap().get("type").cloned().unwrap_or(json!("character")),"chatPage":0}),
                            )?;
                        }
                    }
                    "conversation" => {
                        let inserted = tx.execute(
                            "INSERT OR IGNORE INTO conversations VALUES(?1,?2,?3,0,0,'',0,?4)",
                            params![
                                generation,
                                p[2],
                                p[3],
                                serde_json::to_string(&json!({"id":p[3],"name":""}))?
                            ],
                        )?;
                        // A replacement writes characters before their conversations, and
                        // no later unit has to recount them.
                        if inserted > 0 {
                            commit::refresh_character_summary(tx, generation, &p[2])?;
                        }
                    }
                    "preset" => {
                        tx.execute(
                            "INSERT OR IGNORE INTO bot_presets VALUES(?1,?2,0,'',NULL,?3)",
                            params![
                                generation,
                                p[2],
                                serde_json::to_string(&json!({"id":p[2],"name":""}))?
                            ],
                        )?;
                    }
                    "persona" => {
                        // Fields can arrive in a later receive than the membership.
                        let mut v = root(tx, generation)?;
                        let present = v
                            .get("personas")
                            .and_then(Value::as_array)
                            .is_some_and(|items| {
                                items.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(&p[2]))
                            });
                        if !present {
                            patch_array(&mut v, "personas", "id", &p[2], None, Some(json!({"id":p[2]})))?;
                            write_root(tx, generation, &v)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        "order" => {
            if p[1] == "characters" {
                let mut v = root(tx, generation)?;
                patch(&mut v, "characterOrder", next)?;
                write_root(tx, generation, &v)?;
            } else if p[1] == "conversations" {
                let mut v = text_value(
                    tx,
                    "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                    params![generation, p[2]],
                )?
                .ok_or_else(|| error("parent-missing"))?;
                let folders = next.and_then(|v| v.get("folders").cloned());
                if v.get("chatFolders").is_some()
                    || folders
                        .as_ref()
                        .is_some_and(|value| value.as_array().is_none_or(|items| !items.is_empty()))
                {
                    patch(&mut v, "chatFolders", folders)?;
                }
                commit::put_character_detail(tx, generation, &v)?;
            }
        }
        "plugin" => {
            let mutation = if let Some(value) = next {
                super::super::PluginStorageMutation::Set {
                    owner: p[1].clone(),
                    key: p[2].clone(),
                    value,
                }
            } else {
                super::super::PluginStorageMutation::Delete {
                    owner: p[1].clone(),
                    key: p[2].clone(),
                }
            };
            commit::apply_plugin_storage_mutation(tx, generation, &mutation)?;
        }
        "asset" | "inlay" => {
            if let Some(v) = next {
                let alias: super::super::AssetAlias = serde_json::from_value(v)?;
                if alias.kind != p[0] || alias.key != p[1] {
                    return Err(error("alias-identity-mismatch"));
                }
                commit::put_asset_alias(tx, generation, &alias)?;
            } else {
                tx.execute(
                    "DELETE FROM asset_aliases WHERE generation=?1 AND kind=?2 AND logical_key=?3",
                    params![generation, p[0], p[1]],
                )?;
            }
        }
        "plugin-local" => {
            if !plugin_participates(tx)? {
                return Ok(());
            }
            if !matches!(p[2].as_str(), "string" | "json") {
                return Err(error("plugin-local-namespace-invalid"));
            }
            let value = next
                .as_ref()
                .map(|v| {
                    v.as_str()
                        .ok_or_else(|| error("plugin-local-value-invalid"))
                })
                .transpose()?;
            if value.is_some_and(|value| plugin_text::encoded_length(value) > plugin_text::MAX_VALUE_BYTES) {
                return Err(error("plugin device value is too large"));
            }
            tx.execute("INSERT INTO plugin_device_storage VALUES(?1,?2,?3,?4,?5,?6,'0','',NULL,NULL,NULL) ON CONFLICT(owner,space,key) DO UPDATE SET value=excluded.value,byte_size=excluded.byte_size,tombstone=excluded.tombstone",params![p[1],p[2],p[3],value,value.map(str::len).unwrap_or(0) as i64,i64::from(value.is_none())])?;
        }
        "hypa" => apply_hypa(tx, &p[1], next)?,
        _ => {}
    }
    Ok(())
}
fn apply_hypa(tx: &Connection, key: &str, next: Option<Value>) -> StoreResult<()> {
    use base64::Engine;
    let v = next.unwrap_or(json!({}));
    let vector = v
        .get("vector")
        .and_then(Value::as_str)
        .map(|s| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .map_err(error)
        })
        .transpose()?;
    tx.execute("INSERT INTO hypa_embeddings VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'0','',NULL,NULL,NULL) ON CONFLICT(cache_key) DO UPDATE SET producer=excluded.producer,model=excluded.model,endpoint=excluded.endpoint,preprocess_version=excluded.preprocess_version,dimensions=excluded.dimensions,vector=excluded.vector,metadata=excluded.metadata,tombstone=excluded.tombstone",params![key,v.get("producer").and_then(Value::as_str).unwrap_or_default(),v.get("model").and_then(Value::as_str).unwrap_or_default(),v.get("endpoint").and_then(Value::as_str),v.get("preprocessVersion").and_then(Value::as_i64).unwrap_or(0),v.get("dimensions").and_then(Value::as_i64).unwrap_or(1),vector,v.get("metadata").and_then(Value::as_str),i64::from(v.as_object().is_none_or(Map::is_empty))])?;
    Ok(())
}

pub(in crate::persistent_store) fn device_replacement_changes(
    tx: &Transaction<'_>,
    replacement: &device_store::sections::FrozenBackupSections,
) -> StoreResult<Vec<(UnitKey, UnitValue)>> {
    use device_store::sections::{FrozenBackupSections, SectionValueRow};
    use base64::Engine;
    fn units(tx: &Connection, rows: &FrozenBackupSections, local: bool) -> StoreResult<BTreeMap<UnitKey, UnitValue>> {
        let mut values = BTreeMap::new();
        for row in &rows.hypa {
            let value = match &row.value {
                SectionValueRow::Hypa { producer, model, endpoint, preprocess_version, dimensions, vector, metadata } => unit_value(
                    tx,
                    &json!({"producer":producer,"model":model,"endpoint":endpoint,"preprocessVersion":preprocess_version,"dimensions":dimensions,
                        "vector":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vector),"metadata":metadata}),
                )?,
                SectionValueRow::Tombstone { .. } => UnitValue::Deleted,
                _ => return Err(error("replacement-hypa-row-invalid")),
            };
            values.insert(unit_key(&["hypa", &row.key1])?, value);
        }
        for row in &rows.local_plugins {
            let value = match &row.value {
                SectionValueRow::Plugin { value, .. } => unit_value(tx, &json!(value))?,
                SectionValueRow::PluginBinary { object, .. } => {
                    if local { plugin_unit_value(tx, &row.key1, &row.key2, &row.key3)? }
                    else { wire(UnitValue::object(RecordDescriptor::content(object.hash.clone())))? }
                }
                SectionValueRow::PluginObject { object, .. } => {
                    if local { plugin_unit_value(tx, &row.key1, &row.key2, &row.key3)? }
                    else { wire(UnitValue::object(RecordDescriptor::content(object.hash.clone())))? }
                }
                SectionValueRow::Tombstone { .. } => UnitValue::Deleted,
                _ => return Err(error("replacement-plugin-row-invalid")),
            };
            values.insert(unit_key(&["plugin-local", &row.key1, &row.key2, &row.key3])?, value);
        }
        Ok(values)
    }
    let before = units(tx, &device_store::sections::capture_replacement_device_rows(tx)?, true)?;
    let after = units(tx, replacement, false)?;
    let mut keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut statement = tx.prepare("SELECT key FROM lww_units WHERE json_extract(key,'$[0]') IN ('hypa','plugin-local')")?;
    let stored = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    for key in stored { keys.insert(wire(key.try_into())?); }
    let mut changes = vec![];
    for key in keys {
        let value = after.get(&key).cloned().unwrap_or(UnitValue::Deleted);
        let prior = read_unit(tx, &key)?.map(|(_, value)| value)
            .or_else(|| before.get(&key).cloned()).unwrap_or(UnitValue::Deleted);
        #[cfg(test)]
        count_work();
        if prior != value { changes.push((key, value)); }
    }
    Ok(changes)
}

pub(in crate::persistent_store) fn capture_device_changes(
    tx: &Transaction<'_>,
    stamp: &Stamp,
) -> StoreResult<()> {
    use base64::Engine;
    let authority = authority(tx)?;
    let revision: i64 = tx.query_row(
        "SELECT revision FROM device_change_context WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    let rows: Vec<(String, String, String, String)> = {
        let mut s =
            tx.prepare("SELECT section,key1,key2,key3 FROM device_changes WHERE revision=?1")?;
        let v = s
            .query_map([revision], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        v
    };
    for (section, k1, k2, k3) in rows {
        let (key, value) = if section == "hypa" {
            let row:Option<(String,String,Option<String>,i64,i64,Option<Vec<u8>>,Option<String>,bool)>=tx.query_row("SELECT producer,model,endpoint,preprocess_version,dimensions,vector,metadata,tombstone FROM hypa_embeddings WHERE cache_key=?1",[&k1],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).optional()?;
            let value = match row {
                Some((
                    producer,
                    model,
                    endpoint,
                    preprocess,
                    dimensions,
                    vector,
                    metadata,
                    false,
                )) => unit_value(
                    tx,
                    &json!({"producer":producer,"model":model,"endpoint":endpoint,"preprocessVersion":preprocess,"dimensions":dimensions,"vector":vector.map(|v|base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v)),"metadata":metadata}),
                )?,
                _ => UnitValue::Deleted,
            };
            (unit_key(&["hypa", &k1])?, value)
        } else {
            let value = plugin_unit_value(tx, &k1, &k2, &k3)?;
            (unit_key(&["plugin-local", &k1, &k2, &k3])?, value)
        };
        #[cfg(test)]
        count_work();
        if read_unit(tx, &key)?.is_some_and(|(_, old)| old == value) {
            continue;
        }
        put_unit(
            tx,
            &key,
            stamp,
            &value,
            &Uuid::new_v4().to_string(),
            Some(authority),
        )?;
    }
    Ok(())
}
fn validate_plugin_unit(db: &Connection, value: &UnitValue) -> StoreResult<()> {
    if let UnitValue::Object { descriptor, .. } = value {
        let (_, length) = plugin_text::object_row(db, &descriptor.object_hash)?;
        if plugin_raw::is_object(db, &descriptor.object_hash)? {
            if *descriptor != RecordDescriptor::content(descriptor.object_hash.clone()) { return Err(error("invalid-large-unit")); }
            plugin_raw::inspect_object(db, &descriptor.object_hash)?;
            return Ok(());
        }
        if length <= plugin_text::PART_BYTES as u64 { return validate_large_unit(db, value).map(|_| ()); }
        if *descriptor != RecordDescriptor::content(descriptor.object_hash.clone()) {
            return Err(error("invalid-large-unit"));
        }
        let info = plugin_text::inspect_object(db, &descriptor.object_hash).map_err(|failure| {
            if failure.to_string().contains("too large") { failure } else { error("invalid-large-unit") }
        })?;
        if info.encoded_length <= MAX_INLINE_UNIT_BYTES as u64 { return Err(error("invalid-large-unit")); }
    } else { json_value(value)?; }
    Ok(())
}

pub(in crate::persistent_store) fn plugin_unit_value(db: &Connection, owner: &str, space: &str, key: &str) -> StoreResult<UnitValue> {
    let row: Option<(i64, i64)> = db.query_row(
        "SELECT rowid,byte_size FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3 AND tombstone=0",
        params![owner, space, key], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    let Some((rowid, size)) = row else { return Ok(UnitValue::Deleted); };
    if size < 0 { return Err(error("plugin device size is invalid")); }
    let binary: bool = db.query_row("SELECT typeof(value)='blob' FROM plugin_device_storage WHERE rowid=?1", [rowid], |row| row.get(0))?;
    if binary {
        if space != "json" { return Err(error("plugin-local-namespace-invalid")); }
        let info = plugin_raw::store(db, || Ok(db.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?))?;
        return wire(UnitValue::object(RecordDescriptor::content(info.hash)));
    }
    if size <= plugin_text::PART_BYTES as i64 {
        let value: String = db.query_row("SELECT value FROM plugin_device_storage WHERE rowid=?1", [rowid], |row| row.get(0))?;
        return unit_value(db, &Value::String(value));
    }
    let info = plugin_text::store_encoded(db, || Ok(db.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?))?;
    wire(UnitValue::object(RecordDescriptor::content(info.hash)))
}
pub(in crate::persistent_store) fn refresh_orders(
    tx: &Transaction<'_>,
    generation: &str,
    changed: &[UnitKey],
) -> StoreResult<()> {
    refresh_orders_charged(tx, generation, changed, None)
}

pub(in crate::persistent_store) fn refresh_orders_charged(
    tx: &Connection, generation: &str, changed: &[UnitKey], mut written: Option<&mut dyn FnMut(i64) -> StoreResult<()>>,
) -> StoreResult<()> {
    let mut scopes = BTreeSet::new();
    for key in changed {
        let p = key.components();
        let scope = match p[0].as_str() {
            "order" => Some(key.clone()),
            "exists" => Some(match p[1].as_str() {
                "character" => unit_key(&["order", "characters"])?,
                "conversation" => unit_key(&["order", "conversations", &p[2]])?,
                "preset" => unit_key(&["order", "presets"])?,
                "persona" => unit_key(&["order", "personas"])?,
                other => unit_key(&["order", other])?,
            }),
            "record" => Some(unit_key(&["order", &p[1]])?),
            "plugin" => Some(unit_key(&["order", "plugin-storage", &p[1]])?),
            "character" if p.get(2).is_some_and(|field| field == "trashTime") => {
                Some(unit_key(&["order", "characters"])?)
            }
            _ => None,
        };
        if let Some(scope) = scope {
            scopes.insert(scope);
        }
    }
    let mut rows = Vec::new();
    for scope in scopes {
        if let Some((_, value)) = read_unit(tx, &scope)? {
            rows.push((scope.as_str().to_owned(), serde_json::to_string(&value)?));
        }
    }
    for (key, value) in rows {
        let key: UnitKey = wire(key.try_into())?;
        let p = key.components();
        let value: UnitValue = serde_json::from_str(&value)?;
        let Some(order) = json_value_resolved(tx, &value)? else {
            continue;
        };
        let Some(order) = (if p[1] == "conversations" {
            order.get("ids").and_then(Value::as_array)
        } else {
            order.as_array()
        }) else {
            continue;
        };
        if p[1] == "plugin-storage" {
            let units: Vec<(String, String)> = {
                let mut q=tx.prepare("SELECT key,stamp FROM lww_units WHERE json_extract(key,'$[0]')='plugin' AND json_extract(key,'$[1]')=?1 AND value<>?2")?;
                let rows = q
                    .query_map(
                        params![p[2], serde_json::to_string(&UnitValue::Deleted)?],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?
                    .collect::<Result<_, _>>()?;
                rows
            };
            let mut live = Vec::new();
            for (key, stamp) in units {
                let key: UnitKey = wire(key.try_into())?;
                live.push(risunest_sync_wire::order::LiveRecord {
                    id: key.components()[2].clone(),
                    creation_stamp: serde_json::from_str(&stamp)?,
                });
            }
            let ids = risunest_sync_wire::order::read_order(
                &order
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                &live,
                &BTreeSet::new(),
            );
            let positions: Vec<i64> = {
                let mut q = tx.prepare(
                    "SELECT ordinal FROM plugin_storage WHERE generation=?1 AND owner=?2 ORDER BY ordinal,storage_key",
                )?;
                let rows = q
                    .query_map(params![generation, p[2]], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                rows
            };
            if positions.len() != ids.len() {
                return Err(error("plugin-order-projection-mismatch"));
            }
            // Owner-local reordering must retain the positions of other owners
            // in the flattened upstream object.
            for (id, position) in ids.iter().zip(positions) {
                if tx.execute("UPDATE plugin_storage SET ordinal=?4 WHERE generation=?1 AND owner=?2 AND storage_key=?3 AND ordinal<>?4",params![generation,p[2],id,position])? > 0 {
                    if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(value AS BLOB)) FROM plugin_storage WHERE generation=?1 AND owner=?2 AND storage_key=?3", params![generation,p[2],id], |row| row.get(0))?)?; }
                }
            }
            continue;
        }
        let exists_kind = match p[1].as_str() {
            "characters" => "character",
            "conversations" => "conversation",
            "presets" => "preset",
            "personas" => "persona",
            other => other,
        };
        let units: Vec<(String, String)> = {
            let scope_filter = if exists_kind == "conversation" {
                "json_extract(u.key,'$[2]')=?3"
            } else {
                "?3 IS NULL"
            };
            let mut s=tx.prepare(&format!("SELECT u.key,u.stamp FROM lww_units u LEFT JOIN lww_retired r ON r.key=u.key WHERE r.key IS NULL AND json_extract(u.key,'$[0]')=?4 AND json_extract(u.key,'$[1]')=?1 AND u.value<>?2 AND {scope_filter}"))?;
            let v = s
                .query_map(
                    params![
                        exists_kind,
                        serde_json::to_string(&UnitValue::Deleted)?,
                        if exists_kind == "conversation" {
                            Some(&p[2])
                        } else {
                            None
                        },
                        if exists_kind == "plugins" {
                            "record"
                        } else {
                            "exists"
                        }
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?
                .collect::<Result<_, _>>()?;
            v
        };
        let mut live = Vec::new();
        for (k, stamp) in units {
            let k: UnitKey = wire(k.try_into())?;
            let parts = k.components();
            if exists_kind == "conversation" && parts[2] != p[2] {
                continue;
            }
            live.push(risunest_sync_wire::order::LiveRecord {
                id: parts.last().unwrap().clone(),
                creation_stamp: serde_json::from_str(&stamp)?,
            });
        }
        let ordered = order
            .iter()
            .flat_map(|v| {
                if let Some(id) = v.as_str() {
                    vec![id.to_owned()]
                } else {
                    v.get("data")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                }
            })
            .collect::<Vec<_>>();
        let ids = risunest_sync_wire::order::read_order(&ordered, &live, &BTreeSet::new());
        if p[1] == "characters" {
            // Trashed characters keep their configured index for the trash
            // view but stay out of the order the sidebar shows.
            let trashed: BTreeSet<String> = {
                let mut s = tx.prepare(
                    "SELECT character_id FROM characters WHERE generation=?1 AND trashed=1",
                )?;
                let v = s
                    .query_map([generation], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                v
            };
            let valid = ids
                .iter()
                .filter(|id| !trashed.contains(*id))
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut seen = BTreeSet::new();
            let mut projected = Vec::new();
            for entry in order {
                if let Some(id) = entry.as_str() {
                    if valid.contains(id) && seen.insert(id.to_owned()) {
                        projected.push(entry.clone());
                    }
                } else if let Some(data) = entry.get("data").and_then(Value::as_array) {
                    let mut folder = entry.clone();
                    let filtered = data
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|id| valid.contains(*id) && seen.insert((*id).to_owned()))
                        .map(|id| json!(id))
                        .collect::<Vec<_>>();
                    patch(&mut folder, "data", Some(json!(filtered)))?;
                    projected.push(folder);
                }
            }
            for id in &ids {
                if valid.contains(id) && seen.insert(id.clone()) {
                    projected.push(json!(id));
                }
            }
            let mut v = root(tx, generation)?;
            patch(&mut v, "characterOrder", Some(json!(projected)))?;
            write_root(tx, generation, &v)?;
            if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(value AS BLOB)) FROM root WHERE generation=?1", [generation], |row| row.get(0))?)?; }
        }
        for (i, id) in ids.iter().enumerate() {
            match p[1].as_str() {
                "characters" => {
                    if tx.execute("UPDATE characters SET configured_index=?3 WHERE generation=?1 AND character_id=?2 AND configured_index<>?3",params![generation,id,i as i64])? > 0 {
                        if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(detail AS BLOB)) FROM characters WHERE generation=?1 AND character_id=?2", params![generation,id], |row| row.get(0))?)?; }
                    }
                }
                "conversations" => {
                    if tx.execute("UPDATE conversations SET configured_index=?4 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 AND configured_index<>?4",params![generation,p[2],id,i as i64])? > 0 {
                        if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(detail AS BLOB)) FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3", params![generation,p[2],id], |row| row.get(0))?)?; }
                    }
                }
                "presets" => {
                    if tx.execute("UPDATE bot_presets SET configured_index=?3 WHERE generation=?1 AND preset_id=?2 AND configured_index<>?3",params![generation,id,i as i64])? > 0 {
                        if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(value AS BLOB)) FROM bot_presets WHERE generation=?1 AND preset_id=?2", params![generation,id], |row| row.get(0))?)?; }
                    }
                }
                _ => {}
            }
        }
        if matches!(
            p[1].as_str(),
            "personas" | "modules" | "loadouts" | "customModels" | "plugins"
        ) {
            let mut v = root(tx, generation)?;
            let collection = v.get_mut(&p[1]).and_then(Value::as_array_mut);
            if let Some(collection) = collection {
                let id_field = if p[1] == "plugins" { "name" } else { "id" };
                collection.sort_by_key(|r| {
                    ids.iter()
                        .position(|id| r.get(id_field).and_then(Value::as_str) == Some(id))
                });
            }
            write_root(tx, generation, &v)?;
            if let Some(written) = written.as_deref_mut() { written(tx.query_row("SELECT length(CAST(value AS BLOB)) FROM root WHERE generation=?1", [generation], |row| row.get(0))?)?; }
        }
    }
    Ok(())
}

/// Folds a conversation's ranges into edits against the stored manifest,
/// clamping each one to the count the earlier ones leave, as the commit applies
/// them. Ranges that change nothing are dropped.
fn conversation_edits(
    db: &Connection,
    generation: &str,
    char_id: &str,
    conv: &str,
    ranges: &[(i64, i64, i64)],
    before: bool,
) -> StoreResult<Option<Vec<super::super::message_pages::MessageEdit>>> {
    if before {
        return Ok(None);
    }
    let body:Option<Vec<u8>>=db.query_row("SELECT body FROM message_page_manifests WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,char_id,conv],|r|r.get(0)).optional()?;
    let Some(body) = body else {
        return Ok(None);
    };
    let old = {
        let result = risunest_external_storage_format::message_pages::MessageManifest::decode(&body);
        #[cfg(test)]
        crate::persistent_store::hash_work::decoded("native_manifest_decode_identity", &body, &result);
        result
    }
        .map_err(error)?
        .message_count
        .0;
    let mut count = i64::try_from(old).map_err(error)?;
    let mut edits = Vec::new();
    for &(start, deleted, inserted) in ranges {
        let start = start.clamp(0, count);
        let deleted = deleted.clamp(0, count - start);
        if deleted == 0 && inserted == 0 {
            continue;
        }
        edits.push(super::super::message_pages::MessageEdit {
            start,
            delete_count: deleted,
            insert_count: inserted,
        });
        count = count - deleted + inserted;
    }
    Ok(Some(edits))
}

pub(in crate::persistent_store) fn preserve_local_root(
    db: &Connection,
    active: &str,
    staged: &str,
) -> StoreResult<()> {
    let old = root(db, active)?;
    let mut next = root(db, staged)?;
    for (key, value) in old.as_object().ok_or_else(|| error("invalid-root"))? {
        if !ROOT_FIELDS.contains(&key.as_str())
            && !matches!(
                key.as_str(),
                "modules"
                    | "plugins"
                    | "loadouts"
                    | "customModels"
                    | "personas"
                    | "characterOrder"
                    | "protectedPresetValues"
                    | "explicitGlobalChatVariables"
            )
        {
            patch(&mut next, key, Some(value.clone()))?;
        }
    }
    write_root(db, staged, &next)
}

fn capture_plugin_order(
    db: &Connection,
    generation: &str,
    owner: &str,
    out: &mut BTreeMap<UnitKey, UnitValue>,
) -> StoreResult<()> {
    let ids: Vec<String> = {
        let mut q=db.prepare("SELECT storage_key FROM plugin_storage WHERE generation=?1 AND owner=?2 ORDER BY ordinal,storage_key")?;
        let rows = q
            .query_map(params![generation, owner], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows
    };
    add(db, out, &["order", "plugin-storage", owner], &json!(ids))
}

type ArchiveObjects = (
    String,
    Vec<u8>,
    risunest_sync_wire::descriptor::RecordDescriptor,
    Vec<(String, Vec<u8>)>,
);
fn archive_objects(archived: &super::super::archive::ArchivedObject) -> StoreResult<ArchiveObjects> {
    use risunest_sync_wire::{
        descriptor::{build_reference_tree, inline_references, RecordDescriptor},
        payload_value,
    };
    let mut archived = archived.clone();
    archived.object_hash = archived.shared_object_hash.clone();
    archived.asset_hashes = archived.shared_asset_hashes.clone();
    let body = payload_value::encode(&serde_json::to_value(&archived)?).map_err(error)?;
    let hash = {
        let hash_input = &body;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_archive", hash_input.len());
        risunest_sync_wire::hash(hash_input)
    };
    let mut descriptor = RecordDescriptor::content(hash.clone());
    let mut dependencies = archived
        .object_roots()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    dependencies.sort();
    dependencies.dedup();
    if inline_references(&dependencies).map_err(error)? {
        descriptor.dependencies = dependencies;
    } else {
        let (root, objects) = {
            let result = build_reference_tree(&dependencies, false);
            #[cfg(test)]
            crate::persistent_store::hash_work::reference_creation(&result);
            result
        }.map_err(error)?;
        descriptor.dependency_root = root;
        return Ok((hash, body, descriptor, objects));
    }
    Ok((hash, body, descriptor, Vec::new()))
}
/// The message objects an archive value of `archived` keeps: its body and the
/// reference tree over its asset roots.
pub(in crate::persistent_store) fn archive_object_hashes(
    archived: &super::super::archive::ArchivedObject,
) -> StoreResult<Vec<String>> {
    let (hash, _, _, objects) = archive_objects(archived)?;
    Ok(std::iter::once(hash).chain(objects.into_iter().map(|(hash, _)| hash)).collect())
}
pub(in crate::persistent_store) fn archive_value(
    db: &Connection,
    archived: &super::super::archive::ArchivedObject,
) -> StoreResult<UnitValue> {
    let (hash, body, descriptor, objects) = archive_objects(archived)?;
    super::super::message_pages::put_object(db, &hash, &body)?;
    for (hash, body) in objects {
        super::super::message_pages::put_object(db, &hash, &body)?;
    }
    {
        let result = UnitValue::object(descriptor);
        #[cfg(test)]
        crate::persistent_store::hash_work::descriptor_creation(&result);
        result
    }.map_err(error)
}
pub(in crate::persistent_store) fn archive_metadata(
    db: &Connection,
    value: &UnitValue,
) -> StoreResult<super::super::archive::ArchivedObject> {
    let UnitValue::Object { descriptor, .. } = value else {
        return Err(error("invalid-archive-value"));
    };
    let body = super::super::message_pages::object_body(db, &descriptor.object_hash)?
        .ok_or_else(|| error("archive-metadata-missing"))?;
    let archived: super::super::archive::ArchivedObject = serde_json::from_slice(&body)?;
    let (_, _, descriptor, _) = archive_objects(&archived)?;
    let expected = {
        let result = UnitValue::object(descriptor);
        #[cfg(test)]
        crate::persistent_store::hash_work::descriptor_creation(&result);
        result
    }.map_err(error)?;
    if expected != *value {
        return Err(error("archive-descriptor-mismatch"));
    }
    Ok(archived)
}
pub(in crate::persistent_store) fn reproject_archived_children(
    tx: &Transaction<'_>,
    generation: &str,
    char_id: &str,
) -> StoreResult<()> {
    let rows: Vec<(String, String)> = {
        let mut q=tx.prepare("SELECT key,value FROM lww_units WHERE (json_extract(key,'$[0]') IN ('character','conversation','messages') AND json_extract(key,'$[1]')=?1) OR (json_extract(key,'$[0]')='order' AND json_extract(key,'$[1]')='conversations' AND json_extract(key,'$[2]')=?1) OR (json_extract(key,'$[0]')='exists' AND json_extract(key,'$[1]')='conversation' AND json_extract(key,'$[2]')=?1)")?;
        let rows = q
            .query_map([char_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows
    };
    let mut rows = rows
        .into_iter()
        .map(|(key, value)| {
            Ok((
                wire(key.try_into())?,
                serde_json::from_str::<UnitValue>(&value)?,
            ))
        })
        .collect::<StoreResult<Vec<(UnitKey, UnitValue)>>>()?;
    rows.sort_by_key(|(key, _)| {
        if key.components()[0] == "exists" {
            0
        } else {
            1
        }
    });
    let keys = rows.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>();
    for (key, value) in rows {
        if parent_status(tx, &key)? == "retired" {
            if key.components()[0] == "exists" {
                apply(tx, generation, &key, &UnitValue::Deleted)?;
            }
            continue;
        }
        if parent_status(tx, &key)? == "ready" {
            apply(tx, generation, &key, &value)?;
        }
    }
    refresh_orders(tx, generation, &keys)?;
    Ok(())
}

pub(in crate::persistent_store) fn shared_archive_character(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| {
            CHARACTER_FIELDS.contains(&key.as_str())
                || matches!(
                    key.as_str(),
                    "chaId"
                        | "type"
                        | "chatFolders"
                        | "statics"
                )
        });
        if let Some(statics) = object.get_mut("statics").and_then(Value::as_object_mut) {
            statics.shift_remove("messages");
        }
    }
    value
}
pub(in crate::persistent_store) fn shared_archive_conversation(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, _| CONVERSATION_FIELDS.contains(&key.as_str()) || key == "id");
    }
    value
}


#[cfg(test)]
mod hash_work_tests {
    use super::*;
    use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work, DomainWork};
    #[test]
    fn effective_message_range_counts_only_executed_manifest_decode() {
        use risunest_external_storage_format::message_pages::{MessageManifest, MANIFEST_SCHEMA};
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE message_page_manifests(generation TEXT,character_id TEXT,conversation_id TEXT,body BLOB)").unwrap();
        let manifest = MessageManifest { schema: MANIFEST_SCHEMA.into(), message_count: 0.into(), pages: vec![] }.encode().unwrap();
        db.execute("INSERT INTO message_page_manifests VALUES('g','c','chat',?1)", [&manifest.bytes]).unwrap();
        reset_hash_work();
        conversation_edits(&db, "g", "c", "chat", &[(0, 0, 1), (1, 0, 1)], true).unwrap();
        assert!(take_hash_work().domains.is_empty());
        conversation_edits(&db, "g", "c", "chat", &[(0, 0, 1), (1, 0, 1)], false).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_manifest_decode_identity"], DomainWork { calls: 1, bytes: manifest.bytes.len() as u64 });
        assert!(work.incomplete.is_empty());
    }
}
