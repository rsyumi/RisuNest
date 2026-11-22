use super::*;
use serde_json::{json, Value};

fn assign_ids(records: &mut [Value]) -> StoreResult<()> {
    let mut seen = std::collections::HashSet::new();
    for record in records {
        let object = record
            .as_object_mut()
            .ok_or_else(|| StoreError::Validation {
                message: "Imported record must be an object".into(),
            })?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let id = if let Some(id) = id.filter(|id| seen.insert(id.clone())) {
            id
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            seen.insert(id.clone());
            id
        };
        object.insert("id".into(), json!(id));
    }
    Ok(())
}
fn selected_index(root: &Value, field: &str, records: &[Value]) -> usize {
    match root.get(field) {
        Some(Value::String(id)) => records
            .iter()
            .position(|record| record.get("id").and_then(Value::as_str) == Some(id))
            .unwrap_or(0),
        Some(value) => value.as_u64().unwrap_or(0) as usize,
        None => 0,
    }
    .min(records.len().saturating_sub(1))
}
fn normalize_root(mut root: Value) -> StoreResult<Value> {
    let selected = root.get("selectedPersona").cloned();
    for field in ["modules", "loadouts", "customModels", "personas"] {
        if let Some(records) = root.get_mut(field).and_then(Value::as_array_mut) {
            assign_ids(records)?;
        }
    }
    let index = {
        let records = root
            .get("personas")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut selection = json!({});
        selection["selectedPersona"] = selected.unwrap_or(json!(0));
        selected_index(&selection, "selectedPersona", &records)
    };
    let mirrors = [
        ("username", "name"),
        ("userIcon", "icon"),
        ("personaPrompt", "personaPrompt"),
        ("userNote", "note"),
    ]
    .into_iter()
    .filter_map(|(key, field)| root.get(key).cloned().map(|value| (field, value)))
    .collect::<Vec<_>>();
    if let Some(personas) = root.get_mut("personas").and_then(Value::as_array_mut) {
        for persona in personas.iter_mut() {
            if let Some(module) = persona.get_mut("embeddedModule") {
                assign_ids(std::slice::from_mut(module))?;
            }
        }
        if let Some(persona) = personas.get_mut(index) {
            for (field, value) in mirrors {
                persona[field] = value;
            }
            let id = persona["id"].clone();
            root["selectedPersona"] = id;
        }
    }
    // Upstream RisuAI edits only `globalChatVariables`, so a copy an earlier export left
    // in the file is replaced.
    if let Some(value) = root.get("globalChatVariables").cloned() {
        root["explicitGlobalChatVariables"] = value;
    }
    Ok(root)
}
fn preserve_preset_mirrors(root: &mut Value, presets: &mut [Value]) {
    let index = selected_index(root, "botPresetsId", presets);
    if let Some(preset) = presets.get_mut(index) {
        for (key, field) in super::export::PRESET_MIRRORS {
            let flag = super::export::protected_preset_flag(key);
            if let Some(value) = root.get(*key).cloned() {
                if flag.is_some_and(|f| root.get(f).and_then(Value::as_bool).unwrap_or(false)) {
                    if !root
                        .get("protectedPresetValues")
                        .is_some_and(Value::is_object)
                    {
                        root["protectedPresetValues"] = json!({});
                    }
                    root["protectedPresetValues"][*key] = value;
                } else {
                    preset[*field] = value;
                }
            }
        }
        root["botPresetsId"] = preset["id"].clone();
    }
}
impl PersistentStore {
    pub(crate) fn replace_put_upstream_root(
        &mut self,
        staging_id: &str,
        root: &Value,
    ) -> StoreResult<()> {
        commit::require_staging(&self.connection, staging_id)?;
        let mut root = normalize_root(root.clone())?;
        let mut presets: Vec<Value> = {
            let mut q=self.connection.prepare("SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index,preset_id")?;
            let rows = q
                .query_map([staging_id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|r| serde_json::from_str(&r).map_err(Into::into))
                .collect::<StoreResult<_>>()?
        };
        if !presets.is_empty() {
            preserve_preset_mirrors(&mut root, &mut presets);
            self.replace_put_presets(staging_id, &presets)?;
        }
        self.replace_put_root(staging_id, &root)
    }
    pub(crate) fn replace_put_upstream_presets(
        &mut self,
        staging_id: &str,
        presets: &[Value],
    ) -> StoreResult<()> {
        commit::require_staging(&self.connection, staging_id)?;
        let mut presets = presets.to_vec();
        assign_ids(&mut presets)?;
        let raw: String = self.connection.query_row(
            "SELECT value FROM root WHERE generation=?1",
            [staging_id],
            |r| r.get(0),
        )?;
        let mut root: Value = serde_json::from_str(&raw)?;
        preserve_preset_mirrors(&mut root, &mut presets);
        self.replace_put_presets(staging_id, &presets)?;
        self.replace_put_root(staging_id, &root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_identity_assignment_is_confined_to_stage_and_preserves_mirror_edits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(dir.path()).unwrap();
        let before = store.read_root(None).unwrap().value;
        let stage = store.replace_begin().unwrap().staging_id;
        store.replace_put_upstream_root(&stage,&json!({"botPresetsId":1,"selectedPersona":0,"temperature":0.8,"NAIsettings":{"synthetic":true},"username":"edited","doNotChangeFallbackModels":true,"fallbackModels":["protected"],"personas":[{"id":"persona-kept","name":"stale","embeddedModule":{"assets":[]}},{"id":"persona-kept","name":"duplicate"},{"name":"missing"}],"modules":[{"id":"kept"},{"id":"kept"},{}],"loadouts":[{}],"globalChatVariables":{"toggle_synthetic":"1"},"explicitGlobalChatVariables":{"stale":"0"}})).unwrap();
        assert!(store.replace_put_presets(&stage, &[json!({"name":"missing"})]).is_err());
        store
            .replace_put_upstream_presets(
                &stage,
                &[
                    json!({"id":"existing","name":"one"}),
                    json!({"temperature":0.1,"fallbackModels":["preset"]}),
                    json!({"id":"existing","name":"duplicate"}),
                ],
            )
            .unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT value FROM root WHERE generation=?1",
                [&stage],
                |r| r.get(0),
            )
            .unwrap();
        let root: Value = serde_json::from_str(&raw).unwrap();
        let selected = root["botPresetsId"].as_str().unwrap();
        let raw: String = store
            .connection
            .query_row(
                "SELECT value FROM bot_presets WHERE generation=?1 AND preset_id=?2",
                params![stage, selected],
                |r| r.get(0),
            )
            .unwrap();
        let preset: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(preset["temperature"], 0.8);
        assert_eq!(preset["NAISettings"]["synthetic"], true);
        assert_eq!(preset["fallbackModels"], json!(["preset"]));
        assert_eq!(
            root["protectedPresetValues"]["fallbackModels"],
            json!(["protected"])
        );
        assert_eq!(root["personas"][0]["name"], "edited");
        assert_eq!(root["personas"][0]["id"], "persona-kept");
        assert_eq!(root["selectedPersona"], "persona-kept");
        assert!(root["personas"][0]["embeddedModule"]["id"]
            .as_str()
            .is_some());
        assert_eq!(root["modules"][0]["id"], "kept");
        assert_ne!(root["modules"][1]["id"], "kept");
        assert_eq!(root["explicitGlobalChatVariables"], json!({"toggle_synthetic":"1"}));
        let staged = store.materialize_staging(&stage).unwrap();
        for field in ["botPresets", "personas"] {
            let records = staged[field].as_array().unwrap();
            let ids = records.iter().map(|record| record["id"].as_str().unwrap()).collect::<std::collections::HashSet<_>>();
            assert_eq!(ids.len(), records.len());
            for record in &records[1..] {
                assert!(uuid::Uuid::parse_str(record["id"].as_str().unwrap()).is_ok());
            }
        }
        assert_eq!(staged["botPresets"][0]["id"], "existing");
        store.replace_put_upstream_root(&stage, &root).unwrap();
        store.replace_put_upstream_presets(&stage, staged["botPresets"].as_array().unwrap()).unwrap();
        assert_eq!(store.materialize_staging(&stage).unwrap(), staged);
        assert_eq!(store.read_root(None).unwrap().value, before);
        assert_eq!(store.revision().unwrap(), 0);
        store.replace_commit(&stage, None).unwrap();
        let generation = active_generation(&store.connection).unwrap();
        let mut exported = root.as_object().unwrap().clone();
        exported.insert("temperature".into(), json!(999));
        exported.insert("username".into(), json!("stale"));
        super::super::export::derive_identity_root(&store.connection, &generation, &mut exported)
            .unwrap();
        assert_eq!(exported["temperature"], 0.8);
        assert_eq!(exported["username"], "edited");
        assert_eq!(exported["botPresetsId"], 1);
        assert_eq!(exported["selectedPersona"], 0);
        assert_eq!(exported["fallbackModels"], json!(["protected"]));
        for no_preset in [json!("missing-preset"), json!(""), json!(-1)] {
            let mut exported = root.as_object().unwrap().clone();
            exported.insert("botPresetsId".into(), no_preset);
            exported.insert("temperature".into(), json!(999));
            super::super::export::derive_identity_root(&store.connection, &generation, &mut exported)
                .unwrap();
            assert_eq!(exported["botPresetsId"], -1);
            assert_eq!(exported["temperature"], 999);
        }
    }
}
