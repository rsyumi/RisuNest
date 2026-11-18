use serde_json::{json, Map, Value};

pub(crate) fn toggle_entry(key: &str, value: &Value) -> bool {
    key.starts_with("toggle_") && value.is_string()
}

/// Keeps the `toggle_` string entries of a toggle record; false when the value is not a record.
fn toggles(value: &mut Value) -> bool {
    let Some(values) = value.as_object_mut() else {
        return false;
    };
    values.retain(|key, value| toggle_entry(key, value));
    true
}

/// Keeps a named toggle preset; values that are not a record become empty.
pub(crate) fn toggle_preset(value: &mut Value) -> bool {
    let Some(preset) = value.as_object_mut() else {
        return false;
    };
    if !preset.get("name").is_some_and(Value::is_string) {
        return false;
    }
    if !preset.get_mut("values").is_some_and(toggles) {
        preset.insert("values".into(), Value::Object(Map::new()));
    }
    true
}

/// Drops root values RisuNest cannot read, such as the null PocketRisu stores for a cleared toggle default.
pub(crate) fn root(root: &mut Map<String, Value>) {
    if root
        .get("disableToggleBinding")
        .is_some_and(|value| !value.is_boolean())
    {
        root.shift_remove("disableToggleBinding");
    }
    if root
        .get_mut("defaultToggleValues")
        .is_some_and(|value| !toggles(value))
    {
        root.shift_remove("defaultToggleValues");
    }
    match root.get_mut("togglePresets") {
        Some(Value::Array(presets)) => presets.retain_mut(toggle_preset),
        Some(_) => {
            root.shift_remove("togglePresets");
        }
        None => {}
    }
}

/// Where a restored chat comes from. A RisuSave RisuNest wrote keeps a chat's preset binding
/// fields as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    External,
    RisuNest,
}

pub(crate) fn character(value: &mut Value, fallback: &str, origin: Origin) {
    if let Some(chats) = value.get_mut("chats").and_then(Value::as_array_mut) {
        for (index, value) in chats.iter_mut().enumerate() {
            chat(value, &format!("{fallback}:chat:{index}"), origin);
        }
    }
}

pub(crate) fn chat(value: &mut Value, fallback: &str, origin: Origin) {
    let Some(chat) = value.as_object_mut() else {
        return;
    };
    if chat
        .get_mut("savedToggleValues")
        .is_some_and(|value| !toggles(value))
    {
        chat.shift_remove("savedToggleValues");
    }
    if chat
        .get("bindedPersona")
        .is_some_and(|value| !value.is_string())
    {
        chat.shift_remove("bindedPersona");
    }
    if origin == Origin::External {
        if chat
            .get("promptPresetId")
            .is_some_and(|value| !value.is_string())
        {
            chat.shift_remove("promptPresetId");
        }
        // PocketRisu and RisuBard store a chat's preset binding as bindedBotPreset.
        if let Some(bound) = chat.shift_remove("bindedBotPreset") {
            if let Some(id) = bound.as_str().filter(|id| !id.is_empty()) {
                if !chat.contains_key("promptPresetId") {
                    chat.insert("promptPresetId".into(), Value::String(id.to_owned()));
                }
            }
        }
    }
    let id = chat
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned();
    if let Some(messages) = chat.get_mut("message").and_then(Value::as_array_mut) {
        for (index, value) in messages.iter_mut().enumerate() {
            message(value, &format!("{id}:response:{index}"));
        }
    }
}

pub(crate) fn message(value: &mut Value, fallback: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let Some(Value::Array(items)) = object.get("swipes") else {
        return;
    };
    if object.contains_key("responseVariants") {
        return;
    }
    // Array holes are not candidates; a usable swipeId keeps pointing at its own text.
    let raw_index = object
        .get("swipeId")
        .and_then(Value::as_f64)
        .filter(|index| index.is_finite() && index.fract() == 0.0);
    let mut swipes = Vec::new();
    let mut selected = None;
    for (index, swipe) in items.iter().enumerate() {
        if let Some(swipe) = swipe.as_str() {
            if raw_index == Some(index as f64) {
                selected = Some(swipes.len());
            }
            swipes.push(swipe.to_owned());
        }
    }
    let body = match object.get("data").and_then(Value::as_str) {
        Some(body) => body.to_owned(),
        None => {
            let body = swipes
                .get(selected.unwrap_or(0))
                .cloned()
                .unwrap_or_default();
            object.insert("data".into(), Value::String(body.clone()));
            body
        }
    };
    let mut warnings = Vec::new();
    let index = match selected {
        Some(index) if swipes[index] == body => index,
        Some(_) => {
            warnings.push("swipe-body-mismatch");
            swipes.push(body.clone());
            swipes.len() - 1
        }
        None => {
            warnings.push("invalid-swipe-index");
            let matching: Vec<_> = swipes
                .iter()
                .enumerate()
                .filter(|(_, data)| **data == body)
                .map(|(index, _)| index)
                .collect();
            if matching.len() == 1 {
                matching[0]
            } else {
                swipes.push(body.clone());
                swipes.len() - 1
            }
        }
    };
    let group_id = object
        .get("chatId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(fallback)
        .to_owned();
    let mut snapshot = object.clone();
    snapshot.shift_remove("swipes");
    snapshot.shift_remove("swipeId");
    let candidates: Vec<_> = swipes
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let mut snapshot = snapshot.clone();
            snapshot.insert("data".into(), Value::String(data));
            if i != index {
                for key in ["generationInfo", "promptInfo", "time"] {
                    snapshot.shift_remove(key);
                }
            }
            json!({"id": format!("{group_id}:{i}"), "messages": [snapshot]})
        })
        .collect();
    object.insert("responseVariants".into(), json!({"groupId": group_id, "selectedId": format!("{group_id}:{index}"), "candidates": candidates}));
    object.insert("chatId".into(), Value::String(group_id));
    if !warnings.is_empty() {
        object.insert("pocketRisuImportWarnings".into(), json!(warnings));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_feature_fixtures() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../src/ts/drive/fixtures/pocket-features.json"
        ))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let mut actual = fixture["input"].clone();
            message(&mut actual, "fallback");
            assert_eq!(actual, fixture["expected"]);
        }
    }
    #[test]
    fn shared_chat_fixtures() {
        let fixtures: Value = serde_json::from_str(include_str!(
            "../../../src/ts/drive/fixtures/pocket-chat-features.json"
        ))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let mut actual = fixture["input"].clone();
            let origin = if fixture["risuNestSave"] == true { Origin::RisuNest } else { Origin::External };
            chat(&mut actual, "fallback", origin);
            assert_eq!(actual, fixture["expected"]);
        }
    }
}
