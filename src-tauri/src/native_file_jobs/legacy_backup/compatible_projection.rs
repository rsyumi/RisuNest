//! Source-generated structural contracts. Freeform dictionaries are only allowed
//! where the reference type explicitly declares them.
use super::{CompatibilityTarget, NativeJobError};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Default, Clone)]
pub(super) struct Losses(pub BTreeMap<String, u64>);
impl Losses {
    pub fn add(&mut self, code: &str, count: u64) {
        if count > 0 {
            *self.0.entry(code.into()).or_default() += count;
        }
    }
    fn merge(&mut self, other: Self) {
        for (code, count) in other.0 {
            self.add(&code, count);
        }
    }
}

pub(super) struct Projector {
    schema: Value,
    pub target: CompatibilityTarget,
    pub losses: Losses,
    pub affected_conversations: BTreeMap<String, u64>,
    attributed_losses: BTreeMap<String, u64>,
    inventory: Option<HashSet<String>>,
    persona_ids: Option<HashSet<String>>,
}
impl Projector {
    pub fn new(target: CompatibilityTarget) -> Result<Self, NativeJobError> {
        let text = match target {
            CompatibilityTarget::RisuAi => include_str!("compatibility_contracts/risuai.json"),
            CompatibilityTarget::PocketRisu => include_str!("compatibility_contracts/pocket.json"),
        };
        Ok(Self {
            schema: serde_json::from_str(text)
                .map_err(|_| error("invalid compatibility contract"))?,
            target,
            losses: Losses::default(),
            affected_conversations: BTreeMap::new(),
            attributed_losses: BTreeMap::new(),
            inventory: None,
            persona_ids: None,
        })
    }
    pub fn record_conversation(&mut self, before: &BTreeMap<String, u64>) {
        for (code, count) in &self.losses.0 {
            if *count > *before.get(code).unwrap_or(&0) {
                *self.affected_conversations.entry(code.clone()).or_default() += 1;
                *self.attributed_losses.entry(code.clone()).or_default() +=
                    *count - *before.get(code).unwrap_or(&0);
            }
        }
    }
    pub fn known_conversation_scope(&mut self, code: &str, count: u64) {
        self.affected_conversations.insert(code.into(), count);
        self.attributed_losses
            .insert(code.into(), *self.losses.0.get(code).unwrap_or(&0));
    }
    pub fn conversation_scope(&self, code: &str) -> Option<u64> {
        // A category can span root settings and unowned archived cold records as
        // well as active chats. Never present a partial count as an exact scope.
        if self.attributed_losses.get(code) == self.losses.0.get(code) {
            self.affected_conversations.get(code).copied()
        } else {
            None
        }
    }
    pub fn set_inventory(&mut self, names: HashSet<String>) {
        self.inventory = Some(names);
    }
    pub fn set_personas(&mut self, root: &Value) -> Result<(), NativeJobError> {
        let mut ids = HashSet::new();
        if let Some(personas) = root.get("personas").and_then(Value::as_array) {
            for persona in personas {
                let Some(id) = persona
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                else {
                    continue;
                };
                if !ids.insert(id.to_owned()) {
                    return Err(error("persona identity is duplicated"));
                }
            }
        }
        self.persona_ids = Some(ids);
        Ok(())
    }
    pub fn project(&mut self, name: &str, value: &Value) -> Result<Value, NativeJobError> {
        let node = self.schema["types"][name]
            .as_str()
            .ok_or_else(|| error("missing compatibility structural contract"))?;
        let mut external;
        let value = if matches!(name, "Database" | "DataBase" | "botPreset") {
            external = value.clone();
            if let Some(object) = external.as_object_mut() {
                for (local, upstream) in [("messageNameTemplate", "groupTemplate"), ("namedMessageRole", "groupOtherBotRole")] {
                    if let Some(setting) = object.shift_remove(local) { object.insert(upstream.to_owned(), setting); }
                }
                // The option is required, so a model the target rejects would fail the
                // whole export. Context 3 replaces Context 4 only where the contract says so.
                let field = self.schema["nodes"][node]["fields"]["hypaModel"]["node"].as_str();
                let accepts = |model: &str| field.is_some_and(|field| {
                    project_node(&self.schema, field, &Value::String(model.to_owned()), 0).is_ok()
                });
                if matches!(name, "Database" | "DataBase")
                    && object.get("hypaModel").and_then(Value::as_str) == Some("voyageContext4")
                    && !accepts("voyageContext4")
                    && accepts("voyageContext3")
                {
                    object.insert("hypaModel".to_owned(), Value::String("voyageContext3".to_owned()));
                    self.losses.add("unsupported-option-value", 1);
                }
                // Upstream's Separate Parameters section also renders the per-model map, so
                // its inputs store single values there. Requests look entries up by model ID
                // and both targets type each entry as a parameter set.
                if let Some(overrides) = object
                    .get_mut("seperateParameters")
                    .and_then(|parameters| parameters.get_mut("overrides"))
                    .and_then(Value::as_object_mut)
                {
                    let before = overrides.len();
                    overrides.retain(|_, entry| entry.is_object());
                    self.losses.add("unsupported-option-value", (before - overrides.len()) as u64);
                }
            }
            &external
        } else { value };
        let (mut value, losses) =
            project_node(&self.schema, node, value, 0).map_err(|rejection| rejection.into_error(name))?;
        self.losses.merge(losses);
        if name == "Chat" {
            if let (Some(ids), Some(id)) = (
                &self.persona_ids,
                value
                    .get("bindedPersona")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty()),
            ) {
                if !ids.contains(id) {
                    // Both reference checkPersonaBinded implementations fall back
                    // to the global persona for a missing ID. Omission is the
                    // supported representation of that same effective state.
                    value.as_object_mut().unwrap().remove("bindedPersona");
                    self.losses.add("unavailable-persona-binding", 1);
                }
            }
        }
        if let Some(inventory) = &self.inventory {
            if matches!(name, "DataBase" | "character" | "botPreset") {
                validate_asset_fields(&value, inventory)?;
            }
            for key in ["coldstorage", "coldStoragedChats"] {
                if let Some(value) = value.get(key) {
                    validate_cold_ids(value, inventory)?;
                }
            }
            if name == "Message" {
                for key in ["data", "swipes"] {
                    if let Some(value) = value.get(key) {
                        validate_message_references(
                            value,
                            inventory,
                            self.target,
                            &mut self.losses,
                        )?;
                    }
                }
            }
        }
        Ok(value)
    }
}

fn validate_cold_ids(value: &Value, inventory: &HashSet<String>) -> Result<(), NativeJobError> {
    match value {
        Value::String(id) if !id.is_empty() => {
            if !inventory.contains(&format!("coldstorage_{id}.json")) {
                return Err(error(
                    "referenced cold payload is missing from target inventory",
                ));
            }
        }
        Value::Array(items) => {
            for item in items {
                validate_cold_ids(item, inventory)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn validate_message_references(
    value: &Value,
    inventory: &HashSet<String>,
    target: CompatibilityTarget,
    losses: &mut Losses,
) -> Result<(), NativeJobError> {
    match value {
        Value::Array(items) => {
            for item in items {
                validate_message_references(item, inventory, target, losses)?;
            }
        }
        Value::String(text) => {
            if let Some(id) = text.strip_prefix("\u{ef01}COLDSTORAGE\u{ef01}") {
                validate_cold_ids(&Value::String(id.into()), inventory)?;
            }
            for id in inlay_references(text) {
                // PocketRisu shows a placeholder for a reference without data.
                if target == CompatibilityTarget::RisuAi
                    || !inventory.contains(&format!("inlay_sidecar/{id}"))
                {
                    losses.add("unsupported-inlay-references", 1)
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn inlay_references(value: &str) -> Vec<&str> {
    let mut references = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("{{") {
        let after_open = cursor + relative_start + 2;
        let prefix = ["inlay::", "inlayed::", "inlayeddata::"]
            .into_iter()
            .find(|prefix| value[after_open..].starts_with(prefix));
        let Some(prefix) = prefix else {
            cursor = after_open;
            continue;
        };
        let key_start = after_open + prefix.len();
        let Some(relative_end) = value[key_start..].find("}}") else {
            cursor = after_open;
            continue;
        };
        let key_end = key_start + relative_end;
        let key = &value[key_start..key_end];
        if !key.is_empty()
            && !key
                .chars()
                .any(|character| matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        {
            references.push(key);
            cursor = key_end + 2;
        } else {
            cursor = after_open;
        }
    }
    references
}
fn validate_asset_fields(value: &Value, inventory: &HashSet<String>) -> Result<(), NativeJobError> {
    fn asset(value: &Value, inventory: &HashSet<String>) -> Result<(), NativeJobError> {
        if let Some(path) = value.as_str().and_then(|s| s.strip_prefix("assets/")) {
            if !inventory.contains(path) {
                return Err(error("referenced asset is missing from target inventory"));
            }
        }
        Ok(())
    }
    match value {
        Value::Array(items) => {
            for item in items {
                validate_asset_fields(item, inventory)?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "image" | "icon" | "customBackground" | "userIcon" | "imgFile" | "img" => {
                        asset(value, inventory)?
                    }
                    "additionalAssets" | "emotionImages" | "assets" => {
                        if let Some(items) = value.as_array() {
                            for tuple in items {
                                if let Some(value) = tuple.get(1) {
                                    asset(value, inventory)?;
                                }
                            }
                        }
                    }
                    "ccAssets" => {
                        if let Some(items) = value.as_array() {
                            for item in items {
                                if let Some(value) = item.get("uri") {
                                    asset(value, inventory)?;
                                }
                            }
                        }
                    }
                    "vits" => {
                        if let Some(files) = value.get("files").and_then(Value::as_object) {
                            for value in files.values() {
                                asset(value, inventory)?;
                            }
                        }
                    }
                    "modules" | "personas" | "embeddedModule" | "characterOrder" => {
                        validate_asset_fields(value, inventory)?
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn error(message: &str) -> NativeJobError {
    NativeJobError::new("unsupported-projection", message)
}

/// A projection failure with the contract path to the rejected value. Segments
/// are pushed while unwinding, so they are stored innermost first.
struct Rejection {
    message: &'static str,
    path: Vec<String>,
}
impl Rejection {
    fn new(message: &'static str) -> Self {
        Self { message, path: Vec::new() }
    }
    fn at(mut self, segment: String) -> Self {
        self.path.push(segment);
        self
    }
    fn into_error(self, root: &str) -> NativeJobError {
        let path: String = self.path.into_iter().rev().collect();
        error(&format!("{} at {root}{path}", self.message))
    }
}

/// Values the target does not define, replaced by the closest value it accepts
/// instead of failing a required field or dropping an optional one.
fn fallback(
    schema: &Value,
    key: &str,
    id: &str,
    value: &Value,
    depth: usize,
) -> Option<(Value, Losses)> {
    let (replacement, replaced) = match (key, value) {
        // Requests already send High when a model has no XHigh effort.
        ("adaptiveThinkingEffort" | "adaptive_thinking_effort", Value::String(effort))
            if effort == "xhigh" =>
        {
            (Value::from("high"), 1)
        }
        // LLMTokenizer.DeepSeekV4 (14) counts as DeepSeek (13); any other
        // tokenizer the target lacks becomes Unknown (0).
        ("tokenizer", Value::Number(tokenizer)) => {
            (Value::from(if tokenizer.as_u64() == Some(14) { 13 } else { 0 }), 1)
        }
        // A model flag the target lacks has no behavior there, so only that flag is removed.
        ("customFlags" | "flags", Value::Array(flags)) => {
            // An optional list is a union around the array.
            let node = &schema["nodes"][id];
            let item = match node["kind"].as_str()? {
                "array" => node["items"].as_str(),
                "union" => node["variants"].as_array()?.iter().find_map(|variant| {
                    let variant = &schema["nodes"][variant.as_str()?];
                    (variant["kind"] == "array").then(|| variant["items"].as_str()).flatten()
                }),
                _ => None,
            }?;
            let kept: Vec<Value> = flags
                .iter()
                .filter(|flag| project_node(schema, item, flag, depth + 1).is_ok())
                .cloned()
                .collect();
            let removed = (flags.len() - kept.len()) as u64;
            (Value::Array(kept), removed)
        }
        _ => return None,
    };
    let (value, mut losses) = project_node(schema, id, &replacement, depth).ok()?;
    losses.add("unsupported-option-value", replaced);
    Some((value, losses))
}

fn project_node(
    schema: &Value,
    id: &str,
    value: &Value,
    depth: usize,
) -> Result<(Value, Losses), Rejection> {
    if depth > 256 {
        return Err(Rejection::new("compatibility structure exceeds nesting limit"));
    }
    let node = &schema["nodes"][id];
    let mut losses = Losses::default();
    let output = match node["kind"].as_str().unwrap_or("") {
        "any" => value.clone(),
        "literal" if node["value"] == *value => value.clone(),
        "scalar"
            if match node["type"].as_str().unwrap_or("") {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "boolean" => value.is_boolean(),
                "null" => value.is_null(),
                _ => false,
            } =>
        {
            value.clone()
        }
        "union" => {
            let mut best: Option<(Value, Losses)> = None;
            for branch in node["variants"]
                .as_array()
                .ok_or_else(|| Rejection::new("invalid union contract"))?
            {
                if let Ok(candidate) =
                    project_node(schema, branch.as_str().unwrap_or(""), value, depth + 1)
                {
                    let cost: u64 = candidate.1 .0.values().sum();
                    if best
                        .as_ref()
                        .is_none_or(|b| cost < b.1 .0.values().sum::<u64>())
                    {
                        best = Some(candidate);
                    }
                }
            }
            return best.ok_or_else(|| Rejection::new("value is not supported by target contract"));
        }
        "object" if value.is_object() => {
            let mut out = Map::new();
            let fields = node["fields"]
                .as_object()
                .ok_or_else(|| Rejection::new("invalid object contract"))?;
            for (key, value) in value.as_object().unwrap() {
                let field = fields.get(key);
                let child = field
                    .and_then(|f| f["node"].as_str())
                    .or_else(|| node["additional"].as_str());
                if let Some(child) = child {
                    let projected = project_node(schema, child, value, depth + 1).or_else(|rejection| {
                        field
                            .and_then(|_| fallback(schema, key, child, value, depth + 1))
                            .ok_or(rejection)
                    });
                    match projected {
                        Ok((value, child_losses)) => {
                            out.insert(key.clone(), value);
                            losses.merge(child_losses);
                        }
                        Err(_) if field.is_some_and(|f| f["optional"].as_bool() == Some(true)) => {
                            losses.add("unsupported-option-value", 1)
                        }
                        // Dictionary keys are user data, so the path names only declared fields.
                        Err(rejection) => {
                            return Err(rejection.at(if field.is_some() {
                                format!(".{key}")
                            } else {
                                ".*".to_owned()
                            }))
                        }
                    }
                } else if key.starts_with("__") && schema["types"]["Message"].as_str() == Some(id) {
                    // Plugin-owned message fields: both targets keep unknown
                    // message fields in their own data, and their plugins write them.
                    out.insert(key.clone(), value.clone());
                } else {
                    losses.add("unsupported-structural-field", 1);
                }
            }
            Value::Object(out)
        }
        "array" if value.is_array() => {
            let child = node["items"]
                .as_str()
                .ok_or_else(|| Rejection::new("invalid array contract"))?;
            let mut out = Vec::new();
            for (index, item) in value.as_array().unwrap().iter().enumerate() {
                let (item, l) = project_node(schema, child, item, depth + 1)
                    .map_err(|rejection| rejection.at(format!("[{index}]")))?;
                losses.merge(l);
                out.push(item);
            }
            Value::Array(out)
        }
        "tuple" if value.is_array() => {
            let items = node["items"]
                .as_array()
                .ok_or_else(|| Rejection::new("invalid tuple contract"))?;
            let values = value.as_array().unwrap();
            let optional = node["optional"].as_array();
            if (values.len()..items.len())
                .any(|i| optional.and_then(|v| v.get(i)).and_then(Value::as_bool) != Some(true))
            {
                return Err(Rejection::new("tuple length is unsupported by target"));
            }
            losses.add(
                "unsupported-tuple-elements",
                values.len().saturating_sub(items.len()) as u64,
            );
            let mut out = Vec::new();
            for (index, (item, child)) in values.iter().zip(items).enumerate() {
                let (item, l) =
                    project_node(schema, child.as_str().unwrap_or(""), item, depth + 1)
                        .map_err(|rejection| rejection.at(format!("[{index}]")))?;
                losses.merge(l);
                out.push(item);
            }
            Value::Array(out)
        }
        _ => return Err(Rejection::new("value is not supported by target contract")),
    };
    Ok((output, losses))
}

// Only exact values in declared asset fields (or explicit plugin data) are
/// rewritten. Prompts, scripts and regular expressions are never searched.
pub(super) fn rewrite_assets(value: &mut Value, replacements: &HashMap<String, String>) {
    fn mapped(value: &mut Value, replacements: &HashMap<String, String>) {
        if let Value::String(s) = value {
            if let Some(new) = replacements.get(s) {
                *s = new.clone();
            }
        }
    }
    fn tuples(value: &mut Value, replacements: &HashMap<String, String>) {
        if let Some(items) = value.as_array_mut() {
            for tuple in items {
                if let Some(item) = tuple.get_mut(1) {
                    mapped(item, replacements);
                }
            }
        }
    }
    match value {
        Value::Object(object) => {
            for (key, item) in object {
                match key.as_str() {
                    "image" | "icon" | "customBackground" | "userIcon" | "imgFile" | "img" => {
                        mapped(item, replacements)
                    }
                    "additionalAssets" | "emotionImages" | "assets" => tuples(item, replacements),
                    "ccAssets" => {
                        if let Some(items) = item.as_array_mut() {
                            for asset in items {
                                if let Some(uri) = asset.get_mut("uri") {
                                    mapped(uri, replacements)
                                }
                            }
                        }
                    }
                    "vits" => {
                        if let Some(files) = item.get_mut("files").and_then(Value::as_object_mut) {
                            for file in files.values_mut() {
                                mapped(file, replacements)
                            }
                        }
                    }
                    "modules" | "personas" | "embeddedModule" | "characterOrder" => {
                        rewrite_assets(item, replacements)
                    }
                    _ => {}
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_assets(item, replacements);
            }
        }
        _ => {}
    }
}
pub(super) fn rewrite_plugin(
    value: &mut Value,
    replacements: &HashMap<String, String>,
    losses: &mut Losses,
) {
    match value {
        Value::String(s) => {
            if let Some(new) = replacements.get(s) {
                *s = new.clone();
            } else if replacements.keys().any(|key| s.contains(key)) {
                losses.add("opaque-plugin-reference", 1)
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_plugin(item, replacements, losses)
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                rewrite_plugin(item, replacements, losses)
            }
        }
        _ => {}
    }
}

pub(super) fn pocket_swipes(message: &mut Value, losses: &mut Losses) {
    let Some(variants) = message.get("responseVariants").cloned() else {
        if message.get("swipes").is_some() || message.get("swipeId").is_some() {
            let index = message
                .get("swipeId")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            let valid = message
                .get("swipes")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.len() > 1
                        && items.iter().all(Value::is_string)
                        && index.is_some_and(|n| n < items.len())
                });
            if valid && message.get("data").is_some_and(Value::is_string) {
                let data = message["data"].clone();
                message["swipes"][index.unwrap()] = data;
            } else if let Some(object) = message.as_object_mut() {
                object.remove("swipes");
                object.remove("swipeId");
                losses.add("unrepresentable-reroll-candidates", 1);
            }
        }
        return;
    };
    if let Some(object) = message.as_object_mut() {
        object.remove("swipes");
        object.remove("swipeId");
    }
    let Some(candidates) = variants["candidates"].as_array() else {
        losses.add("invalid-reroll-candidates", 1);
        return;
    };
    let selected = variants["selectedId"].as_str();
    if selected.is_none()
        || candidates
            .iter()
            .filter(|c| c["id"].as_str() == selected)
            .count()
            != 1
    {
        losses.add("invalid-reroll-candidates", 1);
        return;
    }
    let Some(current) = candidates.iter().find(|c| c["id"].as_str() == selected) else {
        losses.add("invalid-reroll-candidates", 1);
        return;
    };
    if current["messages"].as_array().map(Vec::len) != Some(1) {
        losses.add("unrepresentable-reroll-candidates", candidates.len() as u64);
        return;
    }
    let mut swipes = Vec::new();
    let mut index = None;
    for candidate in candidates {
        let is_selected = candidate["id"].as_str() == selected;
        let snapshots = candidate["messages"].as_array();
        let representable = snapshots.is_some_and(|items| {
            items.len() == 1
                && [
                    "role",
                    "saying",
                    "disabled",
                    "isComment",
                    "name",
                    "otherUser",
                ]
                .iter()
                .all(|key| items[0].get(key) == message.get(key))
        });
        if !is_selected && !representable {
            losses.add("unrepresentable-reroll-candidates", 1);
            continue;
        }
        let text = if is_selected {
            index = Some(swipes.len());
            message.get("data")
        } else {
            snapshots.unwrap()[0].get("data")
        };
        if let Some(Value::String(text)) = text {
            swipes.push(Value::String(text.clone()));
        } else {
            losses.add("unrepresentable-reroll-candidates", 1);
            continue;
        }
        if snapshots.unwrap()[0].as_object().is_some_and(|o| {
            ["generationInfo", "promptInfo", "time"]
                .iter()
                .any(|k| o.get(*k) != message.get(*k))
        }) {
            losses.add("reroll-candidate-metadata", 1);
        }
    }
    if swipes.len() > 1 {
        if let Some(index) = index {
            message["swipes"] = Value::Array(swipes);
            message["swipeId"] = Value::from(index);
            losses.add("converted-swipes", 1);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn compatible_report_does_not_present_partial_conversation_scope_as_exact() {
        let mut projector = super::Projector::new(super::CompatibilityTarget::RisuAi).unwrap();
        projector.losses.add("shared-category", 1);
        let before = projector.losses.0.clone();
        projector.losses.add("shared-category", 1);
        projector.losses.add("chat-only-category", 2);
        projector.record_conversation(&before);
        assert_eq!(projector.conversation_scope("shared-category"), None);
        assert_eq!(projector.conversation_scope("chat-only-category"), Some(1));
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn compatible_projection_uses_structural_boundaries_and_preserves_freeform_storage() {
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let mut projector = Projector::new(target).unwrap();
            let value = json!({"plugins":[{"name":"Synthetic","script":"unknownRoot=1","arg":{"unknownRoot":"value"},"unknownPluginField":true}],"pluginCustomStorage":{"unknownRoot":{"unknownNested":[null,true,42,"kept"]}},"unknownRoot":true});
            let result = projector.project("DataBase", &value).unwrap();
            assert!(result.get("unknownRoot").is_none());
            assert_eq!(result["pluginCustomStorage"], value["pluginCustomStorage"]);
            assert!(result["plugins"][0].get("unknownPluginField").is_none());
        }
    }

    #[test]
    fn compatible_projection_falls_back_to_context_3_only_where_the_contract_rejects_context_4() {
        // Rewrites the required hypaModel union so it accepts or rejects Context 4.
        fn accept_context_4(projector: &mut Projector, name: &str, accepted: bool) {
            let schema = &mut projector.schema;
            let database = schema["types"][name].as_str().unwrap().to_owned();
            let field = schema["nodes"][&database]["fields"]["hypaModel"]["node"].as_str().unwrap().to_owned();
            let mut variants = schema["nodes"][&field]["variants"].as_array().unwrap().iter()
                .filter(|variant| schema["nodes"][variant.as_str().unwrap()]["value"] != "voyageContext4")
                .cloned().collect::<Vec<_>>();
            if accepted {
                schema["nodes"]["synthetic-context-4"] = json!({"kind": "literal", "value": "voyageContext4"});
                variants.push(json!("synthetic-context-4"));
            }
            schema["nodes"][&field]["variants"] = Value::Array(variants);
        }
        for name in ["Database", "DataBase"] {
            for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
                // None keeps the shipped contract, which rejects Context 4 only for RisuAI.
                for accepted in [None, Some(true), Some(false)] {
                    for model in ["voyageContext3", "voyageContext4"] {
                        let mut projector = Projector::new(target).unwrap();
                        if let Some(accepted) = accepted {
                            accept_context_4(&mut projector, name, accepted);
                        }
                        let rejected = accepted.map_or(target == CompatibilityTarget::RisuAi, |accepted| !accepted);
                        let fallback = rejected && model == "voyageContext4";
                        let source = json!({"hypaModel": model, "voyageApiKey": "synthetic-key"});
                        let projected = projector.project(name, &source).unwrap();
                        assert_eq!(projected, json!({
                            "hypaModel": if fallback { "voyageContext3" } else { model },
                            "voyageApiKey": "synthetic-key",
                        }));
                        let expected_losses = if fallback {
                            BTreeMap::from([("unsupported-option-value".to_owned(), 1)])
                        } else {
                            BTreeMap::new()
                        };
                        assert_eq!(projector.losses.0, expected_losses);
                        assert_eq!(source["hypaModel"], model);
                    }
                }
                let mut projector = Projector::new(target).unwrap();
                let error = projector.project(name, &json!({"hypaModel": "unknown-model"})).unwrap_err();
                assert_eq!(error.code, "unsupported-projection");
                assert_eq!(error.message, format!("value is not supported by target contract at {name}.hypaModel"));
            }
        }
    }

    #[test]
    fn compatible_projection_replaces_values_pocket_lacks_with_the_closest_supported_value() {
        let database = json!({
            "adaptiveThinkingEffort": "xhigh",
            // Both references declare this as false but write and read true.
            "botSettingAtStart": true,
            "customFlags": [21, 23, 26],
            "customModels": [
                {"name": "Synthetic V4", "tokenizer": 14, "flags": [0, 24]},
                {"name": "Synthetic GLM", "tokenizer": 16, "flags": [25]},
            ],
            "seperateParameters": {"memory": {"adaptive_thinking_effort": "xhigh"}},
        });
        let preset = json!({"adaptiveThinkingEffort": "xhigh", "customFlags": [1, 23]});
        // RisuAI defines every one of these values, so nothing changes there.
        let mut projector = Projector::new(CompatibilityTarget::RisuAi).unwrap();
        assert_eq!(projector.project("DataBase", &database).unwrap(), database);
        assert_eq!(projector.project("botPreset", &preset).unwrap(), preset);
        assert!(projector.losses.0.is_empty());

        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        assert_eq!(projector.project("DataBase", &database).unwrap(), json!({
            "adaptiveThinkingEffort": "high",
            "botSettingAtStart": true,
            "customFlags": [21],
            "customModels": [
                {"name": "Synthetic V4", "tokenizer": 13, "flags": [0]},
                {"name": "Synthetic GLM", "tokenizer": 0, "flags": []},
            ],
            "seperateParameters": {"memory": {"adaptive_thinking_effort": "high"}},
        }));
        // Optional preset fields are replaced too instead of being dropped.
        assert_eq!(
            projector.project("botPreset", &preset).unwrap(),
            json!({"adaptiveThinkingEffort": "high", "customFlags": [1]})
        );
        // One per replaced value and one per removed flag.
        assert_eq!(projector.losses.0, BTreeMap::from([("unsupported-option-value".to_owned(), 10)]));

        let error = projector
            .project("DataBase", &json!({"customModels": [{"name": "Synthetic", "format": -1}]}))
            .unwrap_err();
        assert_eq!(
            error.message,
            "value is not supported by target contract at DataBase.customModels[0].format"
        );
    }

    #[test]
    fn compatible_projection_keeps_null_where_the_references_store_it() {
        // Both references store these nulls; thinkingTokens,
        // postHistoryInstructions, the cleared number settings and a cleared
        // lorebook insertion order are required fields.
        let lore = json!([{
            "key": "synthetic", "secondkey": "", "comment": "", "content": "synthetic", "mode": "normal",
            "alwaysActive": false, "selective": false, "insertorder": null,
        }]);
        let records = [
            ("DataBase", json!({"thinkingTokens": null, "textScreenColor": null, "textScreenBorder": null, "promptTemplate": null})),
            ("DataBase", json!({
                "loreBookDepth": null, "loreBookToken": null, "autoContinueMinTokens": null, "requestRetrys": null,
                "genTime": null, "assetMaxDifference": null, "localNetworkTimeoutSec": null, "maxContext": null,
                "maxResponse": null, "generationSeed": null,
            })),
            ("botPreset", json!({"maxContext": null, "maxResponse": null})),
            ("character", json!({"postHistoryInstructions": null, "globalLore": lore})),
            ("Chat", json!({"folderId": null, "localLore": lore})),
            ("DataBase", json!({
                "loreBook": [{"name": "synthetic", "data": lore}],
                "modules": [{"name": "synthetic", "description": "", "id": "synthetic-module", "lorebook": lore}],
            })),
            ("Message", json!({"role": "user", "data": "synthetic", "name": null})),
        ];
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let mut projector = Projector::new(target).unwrap();
            for (name, value) in &records {
                assert_eq!(projector.project(name, value).map_err(|error| error.message), Ok(value.clone()));
            }
            assert!(projector.losses.0.is_empty());
        }
    }

    #[test]
    fn compatible_projection_drops_parameter_overrides_that_are_not_parameter_sets() {
        // Upstream's Separate Parameters section also renders the per-model map,
        // so its inputs write single parameter values straight into it.
        let parameters = json!({
            "memory": {"thinking_type": "off"},
            "overrides": {
                "thinking_type": "off",
                "temperature": -1000,
                "top_k": null,
                "Synthetic Model": {"temperature": 70, "thinking_type": "budget"},
            },
        });
        for name in ["DataBase", "botPreset"] {
            for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
                let mut projector = Projector::new(target).unwrap();
                let source = json!({"seperateParameters": parameters});
                let projected = projector.project(name, &source).unwrap();
                assert_eq!(projected["seperateParameters"], json!({
                    "memory": {"thinking_type": "off"},
                    "overrides": {"Synthetic Model": {"temperature": 70, "thinking_type": "budget"}},
                }));
                assert_eq!(projector.losses.0, BTreeMap::from([("unsupported-option-value".to_owned(), 3)]));
                assert_eq!(source["seperateParameters"], parameters);
            }
        }
    }

    #[test]
    fn compatible_projection_keeps_plugin_owned_message_fields_only() {
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let mut projector = Projector::new(target).unwrap();
            let value = json!({"message":[{"role":"char","data":"synthetic","chatId":"m1","__plugin":{"record":[1,"two",null]},"__flag":true,"surprise":1}],"__chatField":1});
            let result = projector.project("Chat", &value).unwrap();
            let message = &result["message"][0];
            assert_eq!(message["__plugin"], value["message"][0]["__plugin"]);
            assert_eq!(message["__flag"], true);
            assert!(message.get("surprise").is_none());
            assert!(result.get("__chatField").is_none());
            assert_eq!(projector.losses.0.get("unsupported-structural-field"), Some(&2));
        }
    }

    #[test]
    fn compatible_swipes_keep_current_text_and_skip_structural_or_multi_message_candidates() {
        let mut message = json!({"role":"char","data":"edited","saying":"speaker","responseVariants":{"selectedId":"current","candidates":[{"id":"different","messages":[{"role":"user","data":"wrong","saying":"speaker"}]},{"id":"valid","messages":[{"role":"char","data":"alternative","saying":"speaker","time":2}]},{"id":"many","messages":[{"role":"char","data":"a"},{"role":"char","data":"b"}]},{"id":"current","messages":[{"role":"char","data":"stale","saying":"speaker"}]}]}});
        let mut losses = Losses::default();
        pocket_swipes(&mut message, &mut losses);
        assert_eq!(message["swipes"], json!(["alternative", "edited"]));
        assert_eq!(message["swipeId"], 1);
        assert_eq!(losses.0["unrepresentable-reroll-candidates"], 2);
        assert_eq!(losses.0["reroll-candidate-metadata"], 1);
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        let result = projector.project("Message", &message).unwrap();
        assert!(result.get("responseVariants").is_none());
        assert_eq!(
            result["data"],
            result["swipes"][result["swipeId"].as_u64().unwrap() as usize]
        );
    }

    #[test]
    fn compatible_asset_rewrites_are_exact_and_do_not_change_prompts_or_plugin_code() {
        let replacements = HashMap::from([("assets/old.png".into(), "assets/new.png".into())]);
        let mut value = json!({"image":"assets/old.png","description":"assets/old.png","customscript":[{"in":"assets/old.png"}],"additionalAssets":[["label","assets/old.png","png"]]});
        rewrite_assets(&mut value, &replacements);
        assert_eq!(value["image"], "assets/new.png");
        assert_eq!(value["description"], "assets/old.png");
        assert_eq!(value["customscript"][0]["in"], "assets/old.png");
        assert_eq!(value["additionalAssets"][0][1], "assets/new.png");
        let mut storage = json!({"exact":"assets/old.png","opaque":"prefix assets/old.png suffix"});
        let mut losses = Losses::default();
        rewrite_plugin(&mut storage, &replacements, &mut losses);
        assert_eq!(storage["exact"], "assets/new.png");
        assert_eq!(storage["opaque"], "prefix assets/old.png suffix");
        assert_eq!(losses.0["opaque-plugin-reference"], 1);
    }

    #[test]
    fn compatible_inventory_rejects_missing_references_and_reports_risuai_inlay_loss() {
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        projector.set_inventory(HashSet::new());
        assert!(projector
            .project("character", &json!({"image":"assets/missing.png"}))
            .is_err());
        assert!(projector
            .project(
                "Message",
                &json!({"role":"char","data":"\u{ef01}COLDSTORAGE\u{ef01}missing"})
            )
            .is_err());
        let mut projector = Projector::new(CompatibilityTarget::RisuAi).unwrap();
        projector.set_inventory(HashSet::new());
        let source = json!({"role":"char","data":"user text {{inlay::missing}} untouched"});
        assert_eq!(projector.project("Message", &source).unwrap(), source);
        assert_eq!(projector.losses.0["unsupported-inlay-references"], 1);
    }

    #[test]
    fn pocket_keeps_inlay_references_without_data_and_reports_them() {
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        projector.set_inventory(HashSet::from(["inlay_sidecar/present".to_owned()]));
        let source = json!({"role":"char","data":"user text {{inlay::missing}} {{inlay::present}} {{inlayed::gone}}"});
        assert_eq!(projector.project("Message", &source).unwrap(), source);
        assert_eq!(projector.losses.0["unsupported-inlay-references"], 2);
    }

    #[test]
    fn compatible_inlay_scanner_matches_runtime_token_boundaries() {
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        projector.set_inventory(HashSet::from([
            "inlay_sidecar/inner".to_owned(),
            "inlay_sidecar/second".to_owned(),
        ]));
        let source = json!({
            "role": "char",
            "data": "{{inlay::}} {{inlay::line\nbreak}} {{random::{{inlay::inner}}::{{inlayeddata::second}}}}"
        });
        assert_eq!(projector.project("Message", &source).unwrap(), source);

        let mut projector = Projector::new(CompatibilityTarget::RisuAi).unwrap();
        projector.set_inventory(HashSet::new());
        projector.project("Message", &source).unwrap();
        assert_eq!(projector.losses.0["unsupported-inlay-references"], 2);
    }

    #[test]
    fn compatible_personas_allow_reference_default_ids_and_report_unavailable_bindings() {
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        projector
            .set_personas(&json!({"personas":[{"name":"Unbound"},{"id":"known","name":"Bound"}]}))
            .unwrap();
        assert!(projector
            .project("Chat", &json!({"bindedPersona":"known"}))
            .is_ok());
        assert!(projector
            .project("Chat", &json!({"bindedPersona":"missing"}))
            .unwrap()
            .get("bindedPersona")
            .is_none());
        assert_eq!(projector.losses.0["unavailable-persona-binding"], 1);
    }

    #[test]
    fn compatible_reroll_drops_stale_swipes_when_selected_response_is_multi_message() {
        let mut message = json!({"role":"char","data":"current","swipes":["old a","old b"],"swipeId":0,"responseVariants":{"selectedId":"a","candidates":[{"id":"a","messages":[{"role":"char","data":"first"},{"role":"char","data":"second"}]}]}});
        pocket_swipes(&mut message, &mut Losses::default());
        assert!(message.get("swipes").is_none());
        assert!(message.get("swipeId").is_none());
        assert_eq!(message["data"], "current");
    }

    #[test]
    fn compatible_owner_tuples_keep_supported_asset_values_and_report_trailing_extensions() {
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let mut projector = Projector::new(target).unwrap();
            let result=projector.project("DataBase",&json!({"modules":[{"assets":[["asset","assets/a.png","png",{"unsupportedTail":true}]]}]})).unwrap();
            assert_eq!(
                result["modules"][0]["assets"],
                json!([["asset", "assets/a.png", "png"]])
            );
            assert_eq!(projector.losses.0["unsupported-tuple-elements"], 1);
        }
    }

    #[test]
    fn message_name_settings_use_the_upstream_preset_format() {
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let mut projector = Projector::new(target).unwrap();
            let source = json!({"messageNameTemplate":"[{{char}}] {{slot}}","namedMessageRole":"system","promptSettings":{"sendName":true}});
            let result = projector.project("botPreset", &source).unwrap();
            assert_eq!(result["groupTemplate"], "[{{char}}] {{slot}}");
            assert_eq!(result["groupOtherBotRole"], "system");
            assert_eq!(result["promptSettings"]["sendName"], true);
        }
    }

}
