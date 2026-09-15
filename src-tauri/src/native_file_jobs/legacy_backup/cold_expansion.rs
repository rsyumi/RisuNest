//! Upstream cold payload expansion. A reference becomes the body of the record
//! that carried it, so nothing downstream has to resolve one.
use super::super::restore::pocket_features;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const COLD_STORAGE_HEADER: &str = "\u{ef01}COLDSTORAGE\u{ef01}";

fn chat_cold_key(chat: &Value) -> Option<&str> {
    chat.get("message")?
        .as_array()?
        .first()?
        .get("data")?
        .as_str()?
        .strip_prefix(COLD_STORAGE_HEADER)
}

fn character_cold_key(character: &Value) -> Option<&str> {
    let key = character.get("coldstorage")?.as_str()?;
    (!key.is_empty()).then_some(key)
}

fn references_cold(character: &Value) -> bool {
    if character_cold_key(character).is_some() {
        return true;
    }
    character
        .get("chats")
        .and_then(Value::as_array)
        .is_some_and(|chats| chats.iter().any(|chat| chat_cold_key(chat).is_some()))
}

fn load(path: &Path) -> Result<Value, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("staged cold payload cannot be opened: {error}"))?;
    serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|error| format!("staged cold payload cannot be parsed: {error}"))
}

/// Moves a payload field onto the chat, clearing it when the payload omits it.
fn transfer(chat: &mut Map<String, Value>, source: &mut Map<String, Value>, field: &str) {
    match source.remove(field) {
        Some(value) => {
            chat.insert(field.to_owned(), value);
        }
        None => {
            chat.remove(field);
        }
    }
}

fn apply_chat_payload(chat: &mut Value, payload: Value) -> bool {
    let Some(chat) = chat.as_object_mut() else {
        return false;
    };
    if let Value::Array(messages) = payload {
        chat.insert("message".to_owned(), Value::Array(messages));
        return true;
    }
    let Value::Object(mut source) = payload else {
        return false;
    };
    if !source.get("message").is_some_and(Value::is_array) {
        return false;
    }
    chat.insert("message".to_owned(), source.remove("message").unwrap());
    for field in ["savedToggleValues", "bindedPersona"] {
        if let Some(value) = source.remove(field) {
            chat.insert(field.to_owned(), value);
        }
    }
    for field in ["hypaV2Data", "hypaV3Data", "scriptstate", "localLore"] {
        transfer(chat, &mut source, field);
    }
    true
}

fn expand_character(
    character: &mut Value,
    payloads: &HashMap<String, PathBuf>,
    fallback: &str,
) -> Result<(), String> {
    if let Some(key) = character_cold_key(character) {
        if let Some(path) = payloads.get(key) {
            if let Value::Object(mut payload) = load(path)? {
                if let Some(restored @ Value::Object(_)) = payload.remove("character") {
                    *character = restored;
                }
            }
        }
    }
    if let Some(chats) = character.get_mut("chats").and_then(Value::as_array_mut) {
        for chat in chats.iter_mut() {
            let Some(key) = chat_cold_key(chat) else {
                continue;
            };
            let Some(path) = payloads.get(key) else {
                continue;
            };
            let payload = load(path)?;
            apply_chat_payload(chat, payload);
        }
    }
    if let Some(object) = character.as_object_mut() {
        object.remove("coldstorage");
        object.remove("coldStoragedChats");
    }
    pocket_features::character(character, fallback)
}

/// Returns the rewritten batch, or `None` when no record referenced a payload.
pub(super) fn expand_cold_payloads(
    characters: &[Value],
    payloads: &HashMap<String, PathBuf>,
) -> Result<Option<Vec<Value>>, String> {
    if !characters.iter().any(references_cold) {
        return Ok(None);
    }
    let mut expanded = characters.to_vec();
    for (index, character) in expanded.iter_mut().enumerate() {
        expand_character(character, payloads, &format!("cold:character:{index}"))?;
    }
    Ok(Some(expanded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn staged(directory: &Path, key: &str, body: Value) -> (String, PathBuf) {
        let path = directory.join(format!("cold-{key}.json"));
        std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        (key.to_owned(), path)
    }

    fn placeholder_chat(key: &str) -> Value {
        json!({
            "id": format!("chat-{key}"),
            "message": [{ "role": "char", "data": format!("{COLD_STORAGE_HEADER}{key}") }],
        })
    }

    fn temp_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!("cold-expansion-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn restores_an_archived_character_and_its_nested_chat() {
        let directory = temp_directory();
        let payloads = HashMap::from([
            staged(
                &directory,
                "character-key",
                json!({
                    "character": {
                        "chaId": "cha-1",
                        "name": "Restored",
                        "chats": [placeholder_chat("chat-key")],
                    }
                }),
            ),
            staged(
                &directory,
                "chat-key",
                json!({
                    "message": [{ "role": "user", "data": "restored" }],
                    "scriptstate": { "a": 1 },
                }),
            ),
        ]);
        let characters = vec![json!({
            "chaId": "cha-1",
            "name": "Stub",
            "chats": [placeholder_chat("chat-key")],
            "coldstorage": "character-key",
            "coldStoragedChats": ["chat-key"],
        })];

        let expanded = expand_cold_payloads(&characters, &payloads)
            .unwrap()
            .unwrap();

        assert_eq!(expanded[0]["name"], json!("Restored"));
        assert_eq!(
            expanded[0]["chats"][0]["message"],
            json!([{ "role": "user", "data": "restored" }])
        );
        assert_eq!(expanded[0]["chats"][0]["scriptstate"], json!({ "a": 1 }));
        assert!(expanded[0].get("coldstorage").is_none());
        assert!(expanded[0].get("coldStoragedChats").is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn applies_a_bare_message_array_payload() {
        let directory = temp_directory();
        let payloads = HashMap::from([staged(
            &directory,
            "chat-key",
            json!([{ "role": "user", "data": "bare" }]),
        )]);
        let characters = vec![json!({ "chaId": "cha-1", "chats": [placeholder_chat("chat-key")] })];

        let expanded = expand_cold_payloads(&characters, &payloads)
            .unwrap()
            .unwrap();

        assert_eq!(
            expanded[0]["chats"][0]["message"],
            json!([{ "role": "user", "data": "bare" }])
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn keeps_the_record_when_a_payload_is_missing() {
        let characters = vec![json!({
            "chaId": "cha-1",
            "name": "Stub",
            "chats": [placeholder_chat("absent")],
            "coldstorage": "absent-character",
        })];

        let expanded = expand_cold_payloads(&characters, &HashMap::new())
            .unwrap()
            .unwrap();

        assert_eq!(expanded[0]["name"], json!("Stub"));
        assert_eq!(
            expanded[0]["chats"][0]["message"][0]["data"],
            json!(format!("{COLD_STORAGE_HEADER}absent"))
        );
        assert!(expanded[0].get("coldstorage").is_none());
    }

    #[test]
    fn leaves_a_batch_without_references_untouched() {
        let characters = vec![json!({ "chaId": "cha-1", "chats": [] })];
        assert!(expand_cold_payloads(&characters, &HashMap::new())
            .unwrap()
            .is_none());
    }

    #[test]
    fn normalizes_pocket_risu_swipes_while_expanding() {
        let directory = temp_directory();
        let payloads = HashMap::from([staged(
            &directory,
            "chat-key",
            json!([{ "role": "char", "data": "b", "swipes": ["a", "b"], "swipeId": 1 }]),
        )]);
        let characters = vec![json!({ "chaId": "cha-1", "chats": [placeholder_chat("chat-key")] })];

        let expanded = expand_cold_payloads(&characters, &payloads)
            .unwrap()
            .unwrap();

        let variants = &expanded[0]["chats"][0]["message"][0]["responseVariants"];
        assert_eq!(variants["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(variants["selectedId"], variants["candidates"][1]["id"]);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
