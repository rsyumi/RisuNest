use serde_json::{json, Map, Value};
use std::collections::HashSet;

fn toggles(value: &Value) -> Result<(), String> {
    let values = value
        .as_object()
        .ok_or("PocketRisu toggle values must be a record")?;
    if values
        .iter()
        .any(|(key, value)| !key.starts_with("toggle_") || !value.is_string())
    {
        return Err("PocketRisu toggle values must be toggle_ strings".into());
    }
    Ok(())
}

pub(crate) fn root(root: &Map<String, Value>) -> Result<(), String> {
    if root
        .get("disableToggleBinding")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("PocketRisu disableToggleBinding must be boolean".into());
    }
    if let Some(value) = root.get("defaultToggleValues") {
        toggles(value)?;
    }
    if let Some(value) = root.get("togglePresets") {
        for preset in value
            .as_array()
            .ok_or("PocketRisu toggle presets must be an array")?
        {
            if !preset.get("name").is_some_and(Value::is_string) {
                return Err("PocketRisu toggle preset must have a name".into());
            }
            toggles(
                preset
                    .get("values")
                    .ok_or("PocketRisu toggle preset values missing")?,
            )?;
        }
    }
    if let Some(personas) = root.get("personas").and_then(Value::as_array) {
        let mut ids = HashSet::new();
        for persona in personas {
            if let Some(id) = persona.get("id") {
                let id = id
                    .as_str()
                    .ok_or("PocketRisu persona IDs must be strings")?;
                if !id.is_empty() && !ids.insert(id) {
                    return Err("PocketRisu persona IDs must be unique".into());
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn character(value: &mut Value, fallback: &str) -> Result<(), String> {
    if let Some(chats) = value.get_mut("chats").and_then(Value::as_array_mut) {
        for (index, value) in chats.iter_mut().enumerate() {
            chat(value, &format!("{fallback}:chat:{index}"))?;
        }
    }
    Ok(())
}

pub(crate) fn chat(value: &mut Value, fallback: &str) -> Result<(), String> {
    if let Some(value) = value.get("savedToggleValues") {
        toggles(value)?;
    }
    if value
        .get("bindedPersona")
        .is_some_and(|value| !value.is_string())
    {
        return Err("PocketRisu persona binding must be a string".into());
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_owned();
    if let Some(messages) = value.get_mut("message").and_then(Value::as_array_mut) {
        for (index, value) in messages.iter_mut().enumerate() {
            message(value, &format!("{id}:response:{index}"))?;
        }
    }
    Ok(())
}

pub(crate) fn message(value: &mut Value, fallback: &str) -> Result<(), String> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let Some(swipes) = object.get("swipes") else {
        return Ok(());
    };
    let mut swipes: Vec<String> = swipes
        .as_array()
        .ok_or("PocketRisu swipes must be strings")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or("PocketRisu swipes must be strings".to_owned())
        })
        .collect::<Result<_, _>>()?;
    let raw_index = object.get("swipeId");
    if raw_index.is_some_and(|value| {
        value
            .as_f64()
            .is_none_or(|number| !number.is_finite() || number.fract() != 0.0)
    }) {
        return Err("PocketRisu swipeId must be an integer".into());
    }
    let body = object
        .get("data")
        .and_then(Value::as_str)
        .ok_or("PocketRisu response body must be a string")?
        .to_owned();
    if object.contains_key("responseVariants") {
        return Ok(());
    }
    let mut warnings = Vec::new();
    let index = match raw_index
        .and_then(Value::as_f64)
        .filter(|index| *index >= 0.0 && *index < swipes.len() as f64)
        .map(|index| index as usize)
    {
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
    Ok(())
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
            let result = message(&mut actual, "fallback");
            if fixture["invalid"] == true {
                assert!(result.is_err());
            } else {
                result.unwrap();
                assert_eq!(actual, fixture["expected"]);
            }
        }
    }
}
