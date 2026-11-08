use serde_json::{Map, Value};
use std::collections::HashSet;

pub(super) fn excluded_character(kind: Option<&str>, id: Option<&str>) -> bool {
    kind == Some("group") || id == Some("§temp")
}

pub(super) fn exclude_record(value: &Value, excluded: &mut HashSet<String>) -> bool {
    let id = value.get("chaId").and_then(Value::as_str);
    if !excluded_character(value.get("type").and_then(Value::as_str), id) {
        return false;
    }
    if let Some(id) = id { excluded.insert(id.to_owned()); }
    true
}

/// Upstream message name settings and the names they are stored under here.
pub(super) const MESSAGE_NAME_SETTINGS: [(&str, &str); 2] = [
    ("groupTemplate", "messageNameTemplate"),
    ("groupOtherBotRole", "namedMessageRole"),
];

pub(super) fn message_name_settings(value: &mut Map<String, Value>) {
    for (upstream, local) in MESSAGE_NAME_SETTINGS {
        if let Some(setting) = value.shift_remove(upstream) {
            value.insert(local.to_owned(), setting);
        }
    }
}

pub(super) fn prepare_root(root: &mut Map<String, Value>, excluded: &HashSet<String>) {
    let keep = |value: &Value| value.as_str().is_none_or(|id| id != "§temp" && !excluded.contains(id));
    if let Some(Value::Array(order)) = root.get_mut("characterOrder") {
        order.retain_mut(|entry| {
            if let Some(Value::Array(ids)) = entry.get_mut("data") {
                ids.retain(&keep);
                !ids.is_empty()
            } else { keep(entry) }
        });
    }
    if let Some(Value::Array(loadouts)) = root.get_mut("loadouts") {
        for loadout in loadouts {
            if let Some(Value::Array(ids)) = loadout.get_mut("characterIds") { ids.retain(&keep); }
        }
    }
    message_name_settings(root);
    if let Some(Value::Object(protected)) = root.get_mut("protectedPresetValues") {
        message_name_settings(protected);
    }
}

pub(super) fn prepare_presets(presets: &mut [Value]) {
    for preset in presets {
        if let Some(preset) = preset.as_object_mut() { message_name_settings(preset); }
    }
}
