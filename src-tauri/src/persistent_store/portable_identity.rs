use super::lww::{SourceLayer, REMAPPED_SOURCE, STAGED_SOURCE};
use super::stored_json::{self, Depth, Document, Object, Raw};
use super::{PersistentStore, StoreResult};
use risunest_sync_wire::unit::{UnitKey, UnitValue};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IdentityRemap {
    pub(crate) records: BTreeMap<String, BTreeMap<String, String>>,
    pub(crate) conversations: BTreeMap<String, BTreeMap<String, String>>,
}

impl IdentityRemap {
    fn id(&self, kind: &str, id: &str) -> String {
        self.records
            .get(kind)
            .and_then(|m| m.get(id))
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }
    pub(crate) fn conversation(&self, owner: &str, id: &str) -> String {
        self.conversations
            .get(owner)
            .and_then(|m| m.get(id))
            .cloned()
            .unwrap_or_else(|| id.to_owned())
    }
    fn field(&self, value: &mut Value, field: &str, kind: &str) {
        if let Some(id) = value.get(field).and_then(Value::as_str).map(str::to_owned) {
            value[field] = Value::String(self.id(kind, &id));
        }
    }
    fn list(&self, value: &mut Value, field: &str, kind: &str) {
        if let Some(values) = value.get_mut(field).and_then(Value::as_array_mut) {
            for value in values {
                if let Some(id) = value.as_str() {
                    *value = Value::String(self.id(kind, id));
                }
            }
        }
    }
    fn order(&self, value: &mut Value, kind: &str, owner: Option<&str>) {
        if let Some(values) = value.as_array_mut() {
            for value in values {
                if let Some(id) = value.as_str() {
                    *value =
                        Value::String(owner.map_or_else(
                            || self.id(kind, id),
                            |owner| self.conversation(owner, id),
                        ));
                } else if let Some(data) = value.get_mut("data") {
                    self.order(data, kind, owner);
                }
            }
        } else if let Some(ids) = value.get_mut("ids") {
            self.order(ids, kind, owner);
        }
    }
    pub(crate) fn character(&self, value: &mut Value) {
        self.field(value, "chaId", "character");
        self.list(value, "modules", "modules");
    }
    pub(crate) fn chat(&self, owner: &str, value: &mut Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            value["id"] = Value::String(self.conversation(owner, id));
        }
        self.field(value, "bindedPersona", "persona");
        self.list(value, "modules", "modules");
    }
    fn root(&self, value: &mut Value) {
        self.field(value, "botPresetsId", "preset");
        self.field(value, "selectedPersona", "persona");
        self.list(value, "enabledModules", "modules");
        self.models(value);
        if let Some(order) = value.get_mut("characterOrder") {
            self.order(order, "character", None);
        }
        for (field, kind) in ROOT_RECORDS {
            if let Some(records) = value.get_mut(field).and_then(Value::as_array_mut) {
                for record in records {
                    self.record(kind, record);
                }
            }
        }
    }
    fn record(&self, kind: &str, record: &mut Value) {
        self.field(record, "id", kind);
        if kind == "loadouts" {
            self.list(record, "characterIds", "character");
            self.list(record, "modules", "modules");
            self.field(record, "personaId", "persona");
        }
        if kind == "persona" {
            if let Some(module) = record.get_mut("embeddedModule") {
                self.field(module, "id", "modules");
            }
        }
    }
    /// The stored root text with `root` applied, as `serde_json::to_string`
    /// writes the edited value. Records are written from their text.
    fn root_text(&self, text: &str) -> StoreResult<String> {
        let document = Document::parse(text, Depth::Limited)?;
        let mut out = Vec::new();
        let Some(root) = document.value().object()? else {
            document.value().write_json(&mut out)?;
            return stored_json::text(out);
        };
        stored_json::write_json_object(root.members(), &mut out, |name, member, out| {
            let Some((_, kind)) = ROOT_RECORDS.iter().find(|(field, _)| *field == name) else {
                return edit_member(name, member, out, ROOT_REMAPPED, |value| self.root(value));
            };
            let Some(records) = member.elements()? else {
                return member.write_json(out);
            };
            out.push(b'[');
            for (index, record) in records.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let Some(record) = record.object()? else {
                    record.write_json(out)?;
                    continue;
                };
                write_edited(&record, out, &["id", "characterIds", "modules", "personaId"], |name, member, out| {
                    if *kind == "persona" && name == "embeddedModule" {
                        if let Some(module) = member.object()? {
                            return write_edited(&module, out, &["id"], |_, member, out| member.write_json(out), |value| {
                                self.field(value, "id", "modules")
                            });
                        }
                    }
                    member.write_json(out)
                }, |value| self.record(kind, value))?;
            }
            out.push(b']');
            Ok(())
        })?;
        stored_json::text(out)
    }
    /// A stored record text with `edit` applied to the members it changes,
    /// `names`, as `serde_json::to_string` writes the edited value.
    fn detail_text(text: &str, names: &[&str], edit: impl Fn(&mut Value)) -> StoreResult<String> {
        let document = Document::parse(text, Depth::Limited)?;
        let mut out = Vec::new();
        match document.value().object()? {
            Some(record) => write_edited(&record, &mut out, names, |_, member, out| member.write_json(out), edit)?,
            None => document.value().write_json(&mut out)?,
        }
        stored_json::text(out)
    }
    fn models(&self, value: &mut Value) {
        for field in ["aiModel", "subModel"] {
            self.field(value, field, "customModels");
        }
        for field in [
            "seperateModels",
            "seperateModelsForAxModels",
            "fallbackModels",
        ] {
            if let Some(models) = value.get_mut(field) {
                if let Some(values) = models.as_array_mut() {
                    for v in values {
                        if let Some(id) = v.as_str() {
                            *v = Value::String(self.id("customModels", id));
                        }
                    }
                } else if let Some(values) = models.as_object_mut() {
                    for v in values.values_mut() {
                        if let Some(id) = v.as_str() {
                            *v = Value::String(self.id("customModels", id));
                        }
                    }
                }
            }
        }
    }
    fn key(&self, key: &UnitKey) -> StoreResult<UnitKey> {
        let mut p = key.components();
        match p[0].as_str() {
            "exists" if p[1] == "conversation" => {
                let owner = p[2].clone();
                p[2] = self.id("character", &owner);
                p[3] = self.conversation(&owner, &p[3]);
            }
            "exists" => p[2] = self.id(&p[1], &p[2]),
            "character" | "archive" => p[1] = self.id("character", &p[1]),
            "conversation" | "messages" => {
                let owner = p[1].clone();
                p[1] = self.id("character", &owner);
                p[2] = self.conversation(&owner, &p[2]);
            }
            "preset" | "persona" => p[1] = self.id(&p[0], &p[1]),
            "record" => p[2] = self.id(&p[1], &p[2]),
            "order" if p[1] == "conversations" => p[2] = self.id("character", &p[2]),
            _ => (),
        }
        UnitKey::new(&p.iter().map(String::as_str).collect::<Vec<_>>()).map_err(|e| {
            super::StoreError::Validation {
                message: e.to_string(),
            }
        })
    }
    fn unit(&self, key: &UnitKey, value: &mut Value) {
        let p = key.components();
        match p[0].as_str() {
            "root" => {
                let mut root = serde_json::json!({p[1].clone():value.clone()});
                self.root(&mut root);
                *value = root[&p[1]].take();
            }
            "character" | "conversation" => {
                let field = p.last().unwrap();
                let mut detail = serde_json::json!({field.clone():value.clone()});
                if p[0] == "character" {
                    self.character(&mut detail);
                } else {
                    self.chat(&p[1], &mut detail);
                }
                *value = detail[field].take();
            }
            "preset-protected" => self.models(value),
            "preset" => {
                let field = p.last().unwrap();
                let mut detail = serde_json::json!({field.clone():value.clone()});
                self.models(&mut detail);
                *value = detail[field].take();
            }
            "persona" if p[2] == "embeddedModule" => self.field(value, "id", "modules"),
            "record" => {
                self.field(value, "id", &p[1]);
                if p[1] == "loadouts" {
                    self.list(value, "characterIds", "character");
                    self.list(value, "modules", "modules");
                    self.field(value, "personaId", "persona");
                }
            }
            "order" => self.order(
                value,
                match p[1].as_str() {
                    "characters" => "character",
                    "presets" => "preset",
                    "personas" => "persona",
                    kind => kind,
                },
                if p[1] == "conversations" {
                    Some(&p[2])
                } else {
                    None
                },
            ),
            _ => (),
        }
    }
}

impl PersistentStore {
    /// Remaps the stage's identities that retired records still hold. Returns
    /// the layer of the source units a commit reads, when units are staged.
    pub(crate) fn remap_retired_staging(
        &mut self,
        staging: &str,
        staged_source: bool,
    ) -> StoreResult<Option<i64>> {
        let tx = self.connection.transaction()?;
        super::commit::validate_replace_commit(&tx, staging, None)?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT value FROM app_kv WHERE key=?1",
                [remap_key(staging)],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            let map: IdentityRemap = serde_json::from_str(&prior)?;
            let layer = remap_source(&tx, &map, staging, staged_source)?;
            tx.commit()?;
            return Ok(layer);
        }
        let mut candidates = std::collections::BTreeSet::new();
        let chars = identity_rows(
            &tx,
            "SELECT character_id FROM characters WHERE generation=?1",
            staging,
        )?;
        for id in &chars {
            candidates.insert(("character".to_owned(), id.clone()));
        }
        let presets = identity_rows(
            &tx,
            "SELECT preset_id FROM bot_presets WHERE generation=?1",
            staging,
        )?;
        for id in &presets {
            candidates.insert(("preset".to_owned(), id.clone()));
        }
        for root in identity_rows(&tx, "SELECT value FROM root WHERE generation=?1", staging)? {
            let root = Document::parse(&root, Depth::Limited)?;
            let root = root.value().object()?;
            for (field, kind) in ROOT_RECORDS {
                let records = root.as_ref().and_then(|root| root.get(field)).map(Raw::elements).transpose()?;
                for record in records.flatten().unwrap_or_default() {
                    let Some(record) = record.object()? else { continue };
                    if let Some(id) = text_member(&record, "id")? {
                        candidates.insert((kind.to_owned(), id));
                    }
                    if let Some(module) = record.get("embeddedModule").map(Raw::object).transpose()?.flatten() {
                        if let Some(id) = text_member(&module, "id")? {
                            candidates.insert(("modules".to_owned(), id));
                        }
                    }
                }
            }
        }
        if staged_source {
            for unit in super::lww::replacement_source_units(&tx, SourceLayer::staged(staging)) {
                let (key, value) = unit?;
                let p = key.components();
                if p[0] == "exists"
                    && p[1] != "conversation"
                    && !matches!(value, UnitValue::Deleted)
                {
                    candidates.insert((p[1].clone(), p[2].clone()));
                }
            }
        }
        let raw = {
            let mut q = tx.prepare("SELECT key FROM lww_retired")?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut map = IdentityRemap::default();
        for raw in raw {
            let key: UnitKey = raw.try_into().map_err(|e: risunest_sync_wire::WireError| {
                super::StoreError::Validation {
                    message: e.to_string(),
                }
            })?;
            let p = key.components();
            let fresh = uuid::Uuid::new_v4().to_string();
            if p[1] == "conversation" {
                if candidates.contains(&("character".to_owned(), p[2].clone())) {
                    map.conversations
                        .entry(p[2].clone())
                        .or_default()
                        .insert(p[3].clone(), fresh);
                }
            } else if candidates.contains(&(p[1].clone(), p[2].clone())) {
                map.records
                    .entry(p[1].clone())
                    .or_default()
                    .insert(p[2].clone(), fresh);
            }
        }
        if map.records.is_empty() && map.conversations.is_empty() {
            tx.commit()?;
            return Ok(staged_source.then_some(STAGED_SOURCE));
        }
        let rows = identity_rows(&tx, "SELECT value FROM root WHERE generation=?1", staging)?;
        for text in rows {
            tx.execute(
                "UPDATE root SET value=?2 WHERE generation=?1",
                params![staging, map.root_text(&text)?],
            )?;
        }
        for id in presets {
            let raw: String = tx.query_row(
                "SELECT value FROM bot_presets WHERE generation=?1 AND preset_id=?2",
                params![staging, id],
                |r| r.get(0),
            )?;
            let value = IdentityRemap::detail_text(&raw, PRESET_REMAPPED, |value| {
                map.field(value, "id", "preset");
                map.models(value);
            })?;
            tx.execute(
                "UPDATE bot_presets SET preset_id=?3,value=?4 WHERE generation=?1 AND preset_id=?2",
                params![
                    staging,
                    id,
                    map.id("preset", &id),
                    value
                ],
            )?;
        }
        for id in chars {
            let raw: String = tx.query_row(
                "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                params![staging, id],
                |r| r.get(0),
            )?;
            let value = IdentityRemap::detail_text(&raw, &["chaId", "modules"], |value| map.character(value))?;
            let archive: Option<String> = tx.query_row(
                "SELECT archived_object FROM characters WHERE generation=?1 AND character_id=?2",
                params![staging, id],
                |r| r.get(0),
            )?;
            let archive = archive
                .map(|raw| -> StoreResult<String> {
                    let mut archived: super::archive::ArchivedObject = serde_json::from_str(&raw)?;
                    archived.identity_remap.push(map.clone());
                    Ok(serde_json::to_string(&archived)?)
                })
                .transpose()?;
            tx.execute("UPDATE characters SET character_id=?3,detail=?4,archived_object=?5 WHERE generation=?1 AND character_id=?2",params![staging,id,map.id("character",&id),value,archive])?;
        }
        let chats = {
            let mut q = tx.prepare(
                "SELECT character_id,conversation_id FROM conversations WHERE generation=?1",
            )?;
            let rows = q
                .query_map([staging], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (owner, id) in chats {
            let raw: String = tx.query_row("SELECT detail FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![staging,owner,id],|r|r.get(0))?;
            let value = IdentityRemap::detail_text(&raw, &["id", "bindedPersona", "modules"], |value| map.chat(&owner, value))?;
            tx.execute("UPDATE conversations SET character_id=?4,conversation_id=?5,detail=?6 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![staging,owner,id,map.id("character",&owner),map.conversation(&owner,&id),value])?;
        }
        let message_owners = {
            let mut q = tx.prepare(
                "SELECT DISTINCT character_id,conversation_id FROM messages WHERE generation=?1",
            )?;
            let rows = q
                .query_map([staging], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (owner, id) in message_owners {
            tx.execute("UPDATE messages SET character_id=?4,conversation_id=?5 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![staging,owner,id,map.id("character",&owner),map.conversation(&owner,&id)])?;
        }
        // Pages hold message bodies only, so a renamed conversation keeps them.
        let page_owners = {
            let mut q = tx.prepare(
                "SELECT character_id,conversation_id FROM message_page_manifests WHERE generation=?1
                 UNION SELECT character_id,conversation_id FROM message_page_indexes WHERE generation=?1",
            )?;
            let rows = q
                .query_map([staging], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (owner, id) in page_owners {
            for table in ["message_page_indexes", "message_page_manifests"] {
                tx.execute(&format!("UPDATE {table} SET character_id=?4,conversation_id=?5 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3"),params![staging,owner,id,map.id("character",&owner),map.conversation(&owner,&id)])?;
            }
        }
        let heads = {
            let mut q = tx.prepare(
                "SELECT owner_kind,owner_locator FROM asset_owner_heads WHERE generation=?1",
            )?;
            let rows = q
                .query_map([staging], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (kind, raw) in heads {
            use super::AssetOwnerLocator as Owner;
            let owner = match Owner::from_storage(&kind, &raw)? {
                Owner::CharacterAdditionalAssets { character_id } => {
                    Owner::CharacterAdditionalAssets {
                        character_id: map.id("character", &character_id),
                    }
                }
                Owner::RootModuleAssets { module_id } => Owner::RootModuleAssets {
                    module_id: map.id("modules", &module_id),
                },
                Owner::PersonaEmbeddedModuleAssets { persona_id, module_id } => {
                    Owner::PersonaEmbeddedModuleAssets {
                        persona_id: map.id("persona", &persona_id),
                        module_id: map.id("modules", &module_id),
                    }
                }
            };
            let (_, locator) = owner.storage_identity();
            if locator != raw {
                tx.execute("UPDATE asset_owner_heads SET owner_locator=?4 WHERE generation=?1 AND owner_kind=?2 AND owner_locator=?3",params![staging,kind,raw,locator])?;
            }
        }
        tx.execute(
            "INSERT INTO app_kv(key,value) VALUES(?1,?2)",
            params![remap_key(staging), serde_json::to_string(&map)?],
        )?;
        let layer = remap_source(&tx, &map, staging, staged_source)?;
        tx.commit()?;
        Ok(layer)
    }
}

fn identity_rows(db: &Connection, sql: &str, generation: &str) -> StoreResult<Vec<String>> {
    let mut query = db.prepare(sql)?;
    let rows = query
        .query_map([generation], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The root collections whose records carry identities, with their kinds.
const ROOT_RECORDS: [(&str, &str); 4] = [
    ("personas", "persona"),
    ("modules", "modules"),
    ("loadouts", "loadouts"),
    ("customModels", "customModels"),
];
/// The root fields besides `ROOT_RECORDS` that `IdentityRemap::root` changes.
const ROOT_REMAPPED: &[&str] = &[
    "botPresetsId",
    "selectedPersona",
    "enabledModules",
    "aiModel",
    "subModel",
    "seperateModels",
    "seperateModelsForAxModels",
    "fallbackModels",
    "characterOrder",
];
/// The preset fields a remap changes.
const PRESET_REMAPPED: &[&str] = &[
    "id",
    "aiModel",
    "subModel",
    "seperateModels",
    "seperateModelsForAxModels",
    "fallbackModels",
];

fn text_member(object: &Object<'_>, name: &str) -> StoreResult<Option<String>> {
    Ok(object.get(name).map(Raw::as_str).transpose()?.flatten().map(|text| text.into_owned()))
}

/// Writes a member, applying `edit` to it as the only member of an object when
/// it is one of `names`. Every edit changes members independently of each
/// other, so this writes what editing the whole object would.
fn edit_member(
    name: &str,
    member: Raw<'_>,
    out: &mut Vec<u8>,
    names: &[&str],
    edit: impl FnOnce(&mut Value),
) -> StoreResult<()> {
    if !names.contains(&name) {
        return member.write_json(out);
    }
    let mut value = Value::Object(serde_json::Map::from_iter([(name.to_owned(), member.parse_value()?)]));
    edit(&mut value);
    serde_json::to_writer(&mut *out, &value[name])?;
    Ok(())
}

/// Writes an object with `edit` applied to its members named in `names`, and
/// every other member written by `write_member`.
fn write_edited<'r>(
    object: &Object<'r>,
    out: &mut Vec<u8>,
    names: &[&str],
    mut write_member: impl FnMut(&str, Raw<'r>, &mut Vec<u8>) -> StoreResult<()>,
    edit: impl Fn(&mut Value),
) -> StoreResult<()> {
    stored_json::write_json_object(object.members(), out, |name, member, out| {
        if names.contains(&name) {
            edit_member(name, member, out, names, &edit)
        } else {
            write_member(name, member, out)
        }
    })
}

fn remap_key(staging: &str) -> String {
    format!("lww-import-remap:{staging}")
}

/// Forgets the identity remap of a stage that can no longer be committed,
/// because it was activated, discarded or retired.
pub(super) fn forget_identity_remap(db: &Connection, generation: &str) -> StoreResult<()> {
    db.execute("DELETE FROM app_kv WHERE key=?1", [remap_key(generation)])?;
    Ok(())
}

/// Rebuilds the remapped layer of the staged source units from the staged layer.
fn remap_source(db: &Connection, map: &IdentityRemap, staging: &str, staged_source: bool) -> StoreResult<Option<i64>> {
    if !staged_source {
        return Ok(None);
    }
    super::lww::clear_replacement_source(db, staging, Some(REMAPPED_SOURCE))?;
    for unit in super::lww::replacement_source_units(db, SourceLayer::staged(staging)) {
        let (key, value) = unit?;
        let (key, value) = remap_unit(db, map, &key, value)?;
        super::lww::insert_replacement_source(db, staging, REMAPPED_SOURCE, &key, &value)?;
    }
    Ok(Some(REMAPPED_SOURCE))
}

fn remap_unit(db: &Connection, map: &IdentityRemap, key: &UnitKey, mut value: UnitValue) -> StoreResult<(UnitKey, UnitValue)> {
    if key.components()[0] == "archive" && !matches!(value, UnitValue::Deleted) {
        let mut archive = super::lww::archive_metadata(db, &value)?;
        archive.identity_remap.push(map.clone());
        value = super::lww::archive_value(db, &archive)?;
    } else if matches!(value, UnitValue::Inline { .. })
        || (matches!(value, UnitValue::Object { .. })
            && super::external_capture::is_large_unit(key))
    {
        let original = super::lww::json_value_resolved(db, &value)?
            .ok_or_else(|| super::StoreError::Validation {
                message: "remapped unit value is missing".into(),
            })?;
        let mut payload = original.clone();
        map.unit(key, &mut payload);
        // Remapped identities change the value's length, so its
        // inline or large form follows the new bytes.
        if payload != original {
            value = super::lww::unit_value(db, &payload)?;
        }
    }
    Ok((map.key(key)?, value))
}

#[cfg(test)]
mod tests {
    use super::super::{
        lww::{Header, UnitMutation},
        WorkingSetCommit,
    };
    use super::*;
    use serde_json::json;
    fn key(p: &[&str]) -> UnitKey {
        UnitKey::new(p).unwrap()
    }
    fn stage(store: &mut PersistentStore) -> String {
        let id = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&id,&json!({"botPresetsId":"preset","selectedPersona":"persona","personas":[{"id":"persona"}],"modules":[{"id":"module"}],"enabledModules":["module"],"loadouts":[{"id":"loadout","characterIds":["char"],"modules":["module"],"personaId":"persona","presetName":"kept upstream name"}],"customModels":[{"id":"model"}],"characterOrder":[{"name":"folder","data":["char"]}]})).unwrap();
        store
            .replace_put_presets(
                &id,
                &[json!({"id":"preset","name":"kept upstream name","aiModel":"model"})],
            )
            .unwrap();
        store.replace_add_characters(&id,&[json!({"chaId":"char","type":"character","name":"synthetic","modules":["module"],"chats":[{"id":"chat","name":"synthetic chat","bindedPersona":"persona","modules":["module"],"message":[{"role":"user","data":"synthetic","chatId":"message"}],"bookmarks":["message"],"bookmarkNames":{"message":"kept"},"hypaV3Data":{"memos":["message"]}}]})]).unwrap();
        id
    }
    fn retire(store: &mut PersistentStore, p: &[&str]) {
        store
            .commit(&WorkingSetCommit {
                expected_revision: store.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Delete { key: key(p) }]),
                ..Default::default()
            })
            .unwrap();
    }
    #[test]
    fn native_trash_summary_uses_the_unit_stamp_and_never_mutable_time_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let initial = stage(&mut store);
        store.replace_commit(&initial, None).unwrap();
        let trash_key = key(&["character", "char", "trashTime"]);
        store
            .commit(&WorkingSetCommit {
                expected_revision: store.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: trash_key.clone(),
                    value: json!(1),
                }]),
                ..Default::default()
            })
            .unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT stamp FROM lww_units WHERE key=?1",
                [trash_key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        let stamp: risunest_sync_wire::stamp::Stamp = serde_json::from_str(&raw).unwrap();
        let summary = store.read_character_summary("char", None).unwrap().unwrap();
        assert_eq!(summary.trash_time, Some(1));
        assert_eq!(
            summary.trash_stamp_ms,
            Some(stamp.physical_ms.0.to_string())
        );
        assert_ne!(stamp.physical_ms.0, 1);
        store
            .connection
            .execute("DELETE FROM lww_units WHERE key=?1", [trash_key.as_str()])
            .unwrap();
        let missing = store.read_character_summary("char", None).unwrap().unwrap();
        assert_eq!(missing.trash_time, Some(1));
        assert_eq!(missing.trash_stamp_ms, None);
    }

    #[test]
    fn upstream_retired_entities_remap_references_without_touching_message_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let original = stage(&mut store);
        store.replace_commit(&original, None).unwrap();
        for p in [
            vec!["exists", "conversation", "char", "chat"],
            vec!["exists", "character", "char"],
            vec!["exists", "preset", "preset"],
            vec!["exists", "persona", "persona"],
            vec!["exists", "modules", "module"],
            vec!["exists", "loadouts", "loadout"],
            vec!["exists", "customModels", "model"],
        ] {
            retire(&mut store, &p);
        }
        let incoming = stage(&mut store);
        store.replace_commit(&incoming, None).unwrap();
        let db = store.materialize(None).unwrap();
        let char = &db["characters"][0];
        let id = char["chaId"].as_str().unwrap();
        assert_ne!(id, "char");
        assert_eq!(db["loadouts"][0]["characterIds"][0], id);
        assert_eq!(db["characterOrder"][0]["data"][0], id);
        assert_eq!(db["loadouts"][0]["presetName"], "kept upstream name");
        assert_eq!(db["enabledModules"][0], db["modules"][0]["id"]);
        assert_eq!(db["botPresetsId"], db["botPresets"][0]["id"]);
        assert_eq!(db["botPresets"][0]["aiModel"], db["customModels"][0]["id"]);
        let chat = &char["chats"][0];
        assert_ne!(chat["id"], "chat");
        assert_eq!(chat["bindedPersona"], db["personas"][0]["id"]);
        assert_eq!(chat["message"][0]["chatId"], "message");
        assert_eq!(chat["bookmarks"], json!(["message"]));
        assert_eq!(chat["bookmarkNames"], json!({"message":"kept"}));
        assert_eq!(chat["hypaV3Data"], json!({"memos":["message"]}));
    }
    #[test]
    fn remapping_stage_and_source_units_is_idempotent_before_intent_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let initial = stage(&mut store);
        store.replace_commit(&initial, None).unwrap();
        retire(&mut store, &["exists", "character", "char"]);
        let incoming = stage(&mut store);
        let source = BTreeMap::from([
            (
                key(&["exists", "character", "char"]),
                UnitValue::inline(br#"{"type":"character"}"#).unwrap(),
            ),
            (
                key(&["character", "char", "name"]),
                UnitValue::inline(
                    br#""Synthetic name""#,
                )
                .unwrap(),
            ),
        ]);
        let remapped = |store: &PersistentStore| {
            super::super::lww::replacement_source_units(&store.connection, SourceLayer { generation: &incoming, layer: REMAPPED_SOURCE })
                .collect::<StoreResult<Vec<_>>>()
                .unwrap()
        };
        store.stage_replacement_source(&incoming, &source).unwrap();
        assert_eq!(store.remap_retired_staging(&incoming, true).unwrap(), Some(REMAPPED_SOURCE));
        let first = remapped(&store);
        let projection = store.materialize_staging(&incoming).unwrap();
        assert_eq!(store.remap_retired_staging(&incoming, true).unwrap(), Some(REMAPPED_SOURCE));
        assert_eq!(first.len(), source.len());
        assert_ne!(first.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(), source.keys().cloned().collect::<Vec<_>>());
        assert_eq!(first, remapped(&store));
        assert_eq!(projection, store.materialize_staging(&incoming).unwrap());
        let header = Header {
            binding_authority: store.lww_binding_authority().unwrap(),
            request_id: "source-remap".into(),
        };
        let result = store
            .lww_commit_replacement_units(&header, &incoming, Some(&source))
            .unwrap();
        assert_eq!(
            store
                .lww_commit_replacement_units(&header, &incoming, Some(&source))
                .unwrap()
                .revision,
            result.revision
        );
        let mut altered = source.clone();
        altered.insert(
            key(&["root", "language"]),
            UnitValue::inline(br#""en""#).unwrap(),
        );
        assert!(store
            .lww_commit_replacement_units(&header, &incoming, Some(&altered))
            .is_err());
    }
    fn remap_records(store: &PersistentStore) -> i64 {
        store
            .connection
            .query_row(
                "SELECT count(*) FROM app_kv WHERE key LIKE 'lww-import-remap:%'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }
    #[test]
    fn a_remap_record_ends_when_its_stage_is_activated_discarded_or_retired() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let initial = stage(&mut store);
        store.replace_commit(&initial, None).unwrap();
        retire(&mut store, &["exists", "character", "char"]);

        let activated = stage(&mut store);
        store.remap_retired_staging(&activated, false).unwrap();
        assert_eq!(remap_records(&store), 1);
        store.replace_commit(&activated, None).unwrap();
        assert_eq!(remap_records(&store), 0);

        let discarded = stage(&mut store);
        store.remap_retired_staging(&discarded, false).unwrap();
        assert_eq!(remap_records(&store), 1);
        store.replace_abort(&discarded).unwrap();
        assert_eq!(remap_records(&store), 0);

        // A stage left behind when the app stopped is retired by the next open.
        let abandoned = stage(&mut store);
        store.remap_retired_staging(&abandoned, false).unwrap();
        assert_eq!(remap_records(&store), 1);
        drop(store);
        let store = PersistentStore::open(dir.path()).unwrap();
        assert_eq!(
            super::super::commit::generation_state(&store.connection, &abandoned)
                .unwrap()
                .as_deref(),
            Some("retired")
        );
        assert_eq!(remap_records(&store), 0);
    }
    #[test]
    fn archive_remap_survives_a_second_backup_without_opening_compressed_bodies() {
        use crate::logical_records::{
            decode_logical_record, encode_logical_record_key, LogicalRecordLocator,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let initial = stage(&mut store);
        store.replace_commit(&initial, None).unwrap();
        let revision = store.revision().unwrap();
        store.archive_character("char", revision, 10).unwrap();
        let generation = super::super::active_generation(&store.connection).unwrap();
        let archived =
            super::super::archive::read_archived_object(&store.connection, &generation, "char")
                .unwrap()
                .unwrap();
        let incoming = store.replace_begin().unwrap().staging_id;
        let cas = crate::asset_repository::PayloadCas::new(dir.path()).unwrap();
        let locator = LogicalRecordLocator::Character {
            character_id: "char".into(),
        };
        let key = encode_logical_record_key(&locator).unwrap();
        let encoded = super::super::record_projection::reconstruct_record_with_owner_objects(
            &store.connection,
            &cas,
            &generation,
            &key,
            vec![],
            |_| Ok(()),
            &|hash| Ok(cas.stat_object(hash)?.unwrap()),
        )
        .unwrap();
        let envelope = decode_logical_record(&encoded).unwrap();
        store
            .replace_put_root(&incoming, &json!({"characterOrder":["char"]}))
            .unwrap();
        store.replace_put_presets(&incoming, &[]).unwrap();
        let tx = store.connection.transaction().unwrap();
        super::super::record_apply::apply_record_rows(&tx, &incoming, &key, &locator, &envelope, 0)
            .unwrap();
        tx.commit().unwrap();
        retire(&mut store, &["exists", "character", "char"]);
        let local = cas.read_object(&archived.object_hash).unwrap().unwrap();
        let shared = cas
            .read_object(&archived.shared_object_hash)
            .unwrap()
            .unwrap();
        // The body catalog retains both roots, while neither immutable body is readable during activation.
        let local_path = dir
            .path()
            .join("assets/objects")
            .join(&archived.object_hash[..2])
            .join(&archived.object_hash[2..]);
        let shared_path = dir
            .path()
            .join("assets/objects")
            .join(&archived.shared_object_hash[..2])
            .join(&archived.shared_object_hash[2..]);
        std::fs::remove_file(&local_path).unwrap();
        if shared_path != local_path {
            std::fs::remove_file(&shared_path).unwrap();
        }
        store.replace_commit(&incoming, None).unwrap();
        let page = store
            .query_characters(
                &super::super::CharacterQuery {
                    search: None,
                    order: super::super::QueryOrder::Configured,
                    trash: false,
                    limit: 20,
                    cursor: None,
                },
                None,
            )
            .unwrap();
        let fresh = page.items[0].id.clone();
        assert_ne!(fresh, "char");
        let generation = super::super::active_generation(&store.connection).unwrap();
        let archived2 =
            super::super::archive::read_archived_object(&store.connection, &generation, &fresh)
                .unwrap()
                .unwrap();
        assert_eq!(archived2.object_hash, archived.object_hash);
        assert_eq!(archived2.shared_object_hash, archived.shared_object_hash);
        let locator = LogicalRecordLocator::Character {
            character_id: fresh.clone(),
        };
        let key = encode_logical_record_key(&locator).unwrap();
        let encoded = super::super::record_projection::reconstruct_record_with_owner_objects(
            &store.connection,
            &cas,
            &generation,
            &key,
            vec![],
            |_| Ok(()),
            &|_| Ok(local.len() as u64),
        )
        .unwrap();
        let envelope = decode_logical_record(&encoded).unwrap();
        let second = store.replace_begin().unwrap().staging_id;
        store
            .replace_put_root(&second, &json!({"characterOrder":[fresh]}))
            .unwrap();
        store.replace_put_presets(&second, &[]).unwrap();
        let tx = store.connection.transaction().unwrap();
        super::super::record_apply::apply_record_rows(&tx, &second, &key, &locator, &envelope, 0)
            .unwrap();
        tx.commit().unwrap();
        store.replace_commit(&second, None).unwrap();
        cas.prepare_bytes(&local).unwrap();
        cas.prepare_bytes(&shared).unwrap();
        let revision = store.revision().unwrap();
        store.restore_character(&fresh, revision).unwrap();
        let db = store.materialize(None).unwrap();
        assert_eq!(db["characters"][0]["chaId"], fresh);
        assert_eq!(
            db["characters"][0]["chats"][0]["message"][0]["chatId"],
            "message"
        );
        assert_eq!(
            db["characters"][0]["chats"][0]["bookmarks"],
            json!(["message"])
        );
    }
    // A character order whose canonical JSON is exactly `size` bytes.
    fn sized_order(id: &str, size: usize) -> Value {
        let overhead = risunest_sync_wire::payload_value::encode(&json!([{"data": [id], "name": ""}]))
            .unwrap()
            .len();
        json!([{"data": [id], "name": "x".repeat(size - overhead)}])
    }
    // Remaps one character order unit and checks that the result holds the
    // remapped value in the form its canonical length requires.
    fn assert_order_remap(db: &Connection, before: Value) {
        use super::super::lww::{unit_value, validate_large_unit};
        use risunest_sync_wire::unit::MAX_INLINE_UNIT_BYTES;
        let map = IdentityRemap {
            records: BTreeMap::from([(
                "character".to_owned(),
                BTreeMap::from([
                    ("char".to_owned(), "r".repeat(40)),
                    ("l".repeat(64), "s".to_owned()),
                ]),
            )]),
            conversations: BTreeMap::new(),
        };
        let mut expected = before.clone();
        expected[0]["data"][0] = json!(map.id("character", before[0]["data"][0].as_str().unwrap()));
        let order_key = key(&["order", "characters"]);
        let original = unit_value(db, &before).unwrap();
        let (remapped_key, value) = remap_unit(db, &map, &order_key, original.clone()).unwrap();
        assert_eq!(remapped_key, order_key);
        let value = &value;
        let large = risunest_sync_wire::payload_value::encode(&expected).unwrap().len() > MAX_INLINE_UNIT_BYTES;
        assert_eq!(matches!(value, UnitValue::Object { .. }), large);
        assert!(validate_large_unit(db, value).unwrap() == Some(expected.clone()));
        if before == expected {
            assert!(*value == original);
        }
    }
    #[test]
    fn remapped_inline_units_become_large_when_they_outgrow_the_inline_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();
        let bound = risunest_sync_wire::unit::MAX_INLINE_UNIT_BYTES;
        assert_order_remap(&store.connection, sized_order("char", bound));
    }
    #[test]
    fn remapped_large_units_rewrite_their_bodies_and_keep_the_inline_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(dir.path()).unwrap();
        let bound = risunest_sync_wire::unit::MAX_INLINE_UNIT_BYTES;
        assert_order_remap(&store.connection, sized_order("char", bound + 4096));
        assert_order_remap(&store.connection, sized_order(&"l".repeat(64), bound + 16));
        assert_order_remap(&store.connection, sized_order("kept", bound + 4096));
        // An opaque unit is passed through without reading a body.
        let opaque = (
            key(&["future-unit", "order"]),
            UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content("a".repeat(64))).unwrap(),
        );
        assert_eq!(
            remap_unit(&store.connection, &IdentityRemap::default(), &opaque.0, opaque.1.clone()).unwrap(),
            opaque
        );
    }

    #[test]
    fn remapped_texts_are_what_the_remapped_values_write() {
        let mut map = IdentityRemap::default();
        for (kind, id) in [
            ("preset", "preset"),
            ("persona", "persona"),
            ("modules", "module"),
            ("modules", "embedded"),
            ("loadouts", "loadout"),
            ("customModels", "model"),
            ("character", "char"),
        ] {
            map.records.entry(kind.into()).or_default().insert(id.into(), format!("fresh-{id}"));
        }
        map.conversations.entry("char".into()).or_default().insert("chat".into(), "fresh-chat".into());
        let binary = (0..40).map(|index| format!("\"{index}\":{}", index % 256)).collect::<Vec<_>>().join(",");
        let roots = [
            format!(
                r#"{{"username":"aé","botPresetsId":"preset","selectedPersona":"persona","aiModel":"model","subModel":"other",
                "enabledModules":["module","other",3],"seperateModels":{{"memory":"model","x":1}},"fallbackModels":["model"],
                "seperateModelsForAxModels":"model","characterOrder":["char",{{"name":"f","data":["char","x"]}},{{"ids":["char"]}}],
                "modules":[{{"id":"module","binary":{{{binary}}},"modules":["module"]}},{{"name":"no id"}},5],
                "personas":[{{"id":"persona","embeddedModule":{{"binary":{{{binary}}},"id":"embedded","id":"module"}}}},{{"id":"x","embeddedModule":"embedded"}}],
                "loadouts":[{{"id":"loadout","characterIds":["char"],"modules":["module"],"personaId":"persona","n":-0}}],
                "customModels":[{{"id":"model","binary":{{{binary}}}}}],"botPresetsId":"persona","n":1E5}}"#
            ),
            r#"{"modules":{"id":"module"},"personas":[],"characterOrder":"char"}"#.into(),
            r#"{"$serde_json::private::RawValue":"{\"modules\":[{\"id\":\"module\"}]}"}"#.into(),
            "[]".into(),
            "{}".into(),
        ];
        for text in &roots {
            let mut value: Value = serde_json::from_str(text).unwrap();
            map.root(&mut value);
            assert_eq!(map.root_text(text).unwrap(), serde_json::to_string(&value).unwrap(), "{text}");
        }
        let details = [
            format!(r#"{{"chaId":"char","modules":["module"],"binary":{{{binary}}},"id":"chat","bindedPersona":"persona","aiModel":"model","n":-0.0}}"#),
            r#"{"chaId":3,"id":["chat"],"modules":"module","chaId":"char"}"#.into(),
            r#"{"$serde_json::private::RawValue":"{\"chaId\":\"char\"}"}"#.into(),
            r#""char""#.into(),
        ];
        let edits: [(&[&str], &dyn Fn(&mut Value)); 3] = [
            (&["chaId", "modules"], &|value| map.character(value)),
            (&["id", "bindedPersona", "modules"], &|value| map.chat("char", value)),
            (PRESET_REMAPPED, &|value| {
                map.field(value, "id", "preset");
                map.models(value);
            }),
        ];
        for text in &details {
            for (names, edit) in &edits {
                let mut value: Value = serde_json::from_str(text).unwrap();
                edit(&mut value);
                assert_eq!(IdentityRemap::detail_text(text, names, edit).unwrap(), serde_json::to_string(&value).unwrap(), "{text}");
            }
        }
    }
}
