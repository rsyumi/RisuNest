use super::{server_sync_outbox::ServerDirtyKey, StoreError, StoreResult};
use crate::{
    asset_repository::{owner_manifest_codec::decode_owner_manifest, PayloadCas},
    logical_records::{
        decode_logical_record, encode_logical_record_key, LogicalRecordEnvelope,
        LogicalRecordLocator,
    },
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Server projection has one semantic record per conversation. Message IDs may
/// be null or duplicated; sequence and exact values are preserved independently.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ServerPayload {
    pub record: LogicalRecordEnvelope,
    pub messages: Option<Vec<Value>>,
    #[serde(skip)]
    pub derived_objects: std::collections::BTreeMap<String, Vec<u8>>,
}

pub(crate) fn locator(key: &ServerDirtyKey) -> StoreResult<LogicalRecordLocator> {
    Ok(match key.kind.as_str() {
        "root" => LogicalRecordLocator::Root,
        "preset" => LogicalRecordLocator::Preset {
            preset_id: key.key1.clone(),
        },
        "plugin" => LogicalRecordLocator::Plugin {
            storage_key: key.key1.clone(),
        },
        "character" => LogicalRecordLocator::Character {
            character_id: key.key1.clone(),
        },
        "conversation" => LogicalRecordLocator::Conversation {
            character_id: key.key1.clone(),
            conversation_id: key.key2.clone(),
        },
        "asset" => LogicalRecordLocator::Asset {
            logical_key: key.key1.clone(),
        },
        "inlay" => LogicalRecordLocator::Inlay {
            logical_key: key.key1.clone(),
        },
        "cold" => LogicalRecordLocator::Cold {
            logical_key: key.key1.clone(),
        },
        "owner" if key.key1 == "character-additional-assets" => LogicalRecordLocator::Character {
            character_id: key.key2.clone(),
        },
        "owner" => LogicalRecordLocator::Root,
        _ => return invalid("Unknown server record family"),
    })
}
pub(crate) fn wire_key(key: &ServerDirtyKey) -> StoreResult<String> {
    encode_logical_record_key(&locator(key)?).map_err(|_| StoreError::Validation {
        message: "Invalid server record key".into(),
    })
}

pub(crate) fn project(
    db: &Connection,
    cas: &PayloadCas,
    generation: &str,
    key: &ServerDirtyKey,
) -> StoreResult<Option<ServerPayload>> {
    // Revision zero contains only the store's uncommitted root placeholder.
    // Advertising it as user content creates a false conflict on first receive.
    if key.kind == "root" && key.revision == 0 {
        return Ok(None);
    }
    let locator = locator(key)?;
    let (table, column1, column2, value1, value2) = match &locator {
        LogicalRecordLocator::Root => ("root", "generation", None, generation, ""),
        LogicalRecordLocator::Preset { preset_id } => {
            ("bot_presets", "preset_id", None, preset_id.as_str(), "")
        }
        LogicalRecordLocator::Plugin { storage_key } => (
            "plugin_storage",
            "storage_key",
            None,
            storage_key.as_str(),
            "",
        ),
        LogicalRecordLocator::Character { character_id } => (
            "characters",
            "character_id",
            None,
            character_id.as_str(),
            "",
        ),
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => (
            "conversations",
            "character_id",
            Some("conversation_id"),
            character_id.as_str(),
            conversation_id.as_str(),
        ),
        LogicalRecordLocator::Asset { logical_key } => (
            "asset_aliases",
            "logical_key",
            Some("kind"),
            logical_key.as_str(),
            "asset",
        ),
        LogicalRecordLocator::Inlay { logical_key } => (
            "asset_aliases",
            "logical_key",
            Some("kind"),
            logical_key.as_str(),
            "inlay",
        ),
        LogicalRecordLocator::Cold { logical_key } => {
            ("cold_aliases", "key", None, logical_key.as_str(), "")
        }
    };
    let suffix = column2
        .map(|c| format!(" AND {c}=?3"))
        .unwrap_or_else(|| " AND ?3=''".into());
    let exists: bool = db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE generation=?1 AND {column1}=?2{suffix})"
        ),
        params![generation, value1, value2],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let record_key = encode_logical_record_key(&locator).map_err(|_| StoreError::Validation {
        message: "Invalid server record key".into(),
    })?;
    let mut derived_objects = std::collections::BTreeMap::new();
    let residency = std::cell::OnceCell::new();
    let bytes = super::record_projection::reconstruct_record_with_owner_objects(
        db,
        cas,
        generation,
        &record_key,
        Vec::new(),
        |bytes| {
            derived_objects.insert(risunest_sync_wire::hash(bytes), bytes.to_vec());
            Ok(())
        },
        &|hash| {
            if let Some(size) = cas.stat_object(hash)? {
                return Ok(size);
            }
            let residency = residency
                .get_or_init(|| {
                    crate::server_sync::residency::Residency::open(cas.repository_root())
                })
                .as_ref()
                .map_err(|_| StoreError::Validation {
                    message: "Remote owner custody unavailable".into(),
                })?;
            residency
                .object(hash, None)
                .map_err(|_| StoreError::Validation {
                    message: "Remote owner custody unavailable".into(),
                })?
                .map(|object| object.size)
                .ok_or_else(|| StoreError::Validation {
                    message: "Owner payload is unavailable locally and remotely".into(),
                })
        },
    )?;
    let mut record = decode_logical_record(&bytes).map_err(|_| StoreError::Validation {
        message: "Invalid server source record".into(),
    })?;
    // These fields describe local view/activity, not shared character settings.
    match &mut record {
        LogicalRecordEnvelope::Root { value, .. } => {
            if let Some(statics) = value.get_mut("statics").and_then(Value::as_object_mut) {
                statics.shift_remove("messages");
                if statics.is_empty() {
                    value.as_object_mut().unwrap().shift_remove("statics");
                }
            }
        }
        LogicalRecordEnvelope::Character {
            detail,
            owner_heads,
            ..
        } => {
            if let Some(detail) = detail.as_object_mut() {
                for field in ["chatPage", "lastInteraction"] {
                    if let Some(index) = detail.keys().position(|key| key == field) {
                        for head in owner_heads.iter_mut() {
                            if let Some(position) = head.property_index.as_mut() {
                                if (index as u64) < *position {
                                    *position -= 1;
                                }
                            }
                        }
                        detail.shift_remove(field);
                    }
                }
            }
        }
        _ => (),
    }
    let messages = if let LogicalRecordLocator::Conversation {
        character_id,
        conversation_id,
    } = &locator
    {
        let mut statement=db.prepare("SELECT value FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 ORDER BY message_index")?;
        let mut rows = statement.query(params![generation, character_id, conversation_id])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push(serde_json::from_str(&row.get::<_, String>(0)?)?);
        }
        Some(values)
    } else {
        None
    };
    Ok(Some(ServerPayload {
        record,
        messages,
        derived_objects,
    }))
}

pub(crate) fn dependencies(payload: &ServerPayload, cas: &PayloadCas) -> StoreResult<Vec<String>> {
    let mut hashes = BTreeSet::new();
    match &payload.record {
        LogicalRecordEnvelope::Root { owner_heads, .. }
        | LogicalRecordEnvelope::Character { owner_heads, .. } => {
            for head in owner_heads {
                if let Some(hash) = &head.manifest_hash {
                    hashes.insert(hash.clone());
                    let bytes = if let Some(bytes) = payload.derived_objects.get(hash) {
                        bytes.clone()
                    } else {
                        cas.read_object(hash)?
                            .ok_or_else(|| StoreError::Validation {
                                message: "Server source owner object is missing".into(),
                            })?
                    };
                    for entry in
                        decode_owner_manifest(&bytes).map_err(|_| StoreError::Validation {
                            message: "Server source owner object is invalid".into(),
                        })?
                    {
                        if let Some(hash) = entry.payload_hash {
                            hashes.insert(hex::encode(hash));
                        }
                    }
                }
            }
        }
        LogicalRecordEnvelope::Asset { object_hash, .. }
        | LogicalRecordEnvelope::Inlay { object_hash, .. }
        | LogicalRecordEnvelope::Cold { object_hash, .. } => {
            if let Some(hash) = object_hash {
                hashes.insert(hash.clone());
            }
        }
        _ => (),
    }
    Ok(hashes.into_iter().collect())
}
pub(crate) fn preserve_local_view(
    incoming: &mut ServerPayload,
    raw_character: Option<&Value>,
    raw_root: Option<&Value>,
) {
    match &mut incoming.record {
        LogicalRecordEnvelope::Root { value, .. } => {
            if let Some(messages) = raw_root
                .and_then(|v| v.get("statics"))
                .and_then(|v| v.get("messages"))
            {
                if let Some(root) = value.as_object_mut() {
                    let statics = root
                        .entry("statics")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(statics) = statics.as_object_mut() {
                        statics.insert("messages".into(), messages.clone());
                    }
                }
            }
        }
        LogicalRecordEnvelope::Character { detail, .. } => {
            if let Some(detail) = detail.as_object_mut() {
                for key in ["chatPage", "lastInteraction"] {
                    if let Some(value) = raw_character.and_then(|v| v.get(key)) {
                        detail.insert(key.into(), value.clone());
                    }
                }
            }
        }
        _ => (),
    }
}
fn invalid<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.into(),
    })
}

pub(crate) fn all_keys_page(
    db: &Connection,
    generation: &str,
    after: Option<(&str, &str, &str)>,
    limit: usize,
    revision: i64,
) -> StoreResult<Vec<ServerDirtyKey>> {
    let (kind, key1, key2) = after.unwrap_or(("", "", ""));
    let mut keys = Vec::new();
    for (family, table, first, second, predicate) in [
        (
            "asset",
            "asset_aliases",
            "logical_key",
            "''",
            "AND kind='asset'",
        ),
        ("character", "characters", "character_id", "''", ""),
        ("cold", "cold_aliases", "key", "''", ""),
        (
            "conversation",
            "conversations",
            "character_id",
            "conversation_id",
            "",
        ),
        (
            "inlay",
            "asset_aliases",
            "logical_key",
            "''",
            "AND kind='inlay'",
        ),
        ("plugin", "plugin_storage", "storage_key", "''", ""),
        ("preset", "bot_presets", "preset_id", "''", ""),
        ("root", "root", "''", "''", ""),
    ] {
        if family < kind || keys.len() >= limit.min(1024) {
            continue;
        }
        let (after1, after2) = if family == kind {
            (key1, key2)
        } else {
            ("", "")
        };
        let condition = if family == kind {
            format!("AND ({first},{second})>(?2,?3)")
        } else {
            "AND ?2='' AND ?3=''".into()
        };
        let mut statement=db.prepare(&format!("SELECT {first},{second} FROM {table} WHERE generation=?1 {predicate} {condition} ORDER BY {first},{second} LIMIT ?4"))?;
        let rows = statement.query_map(
            params![
                generation,
                after1,
                after2,
                (limit.min(1024) - keys.len()) as i64
            ],
            |r| {
                Ok(ServerDirtyKey {
                    kind: family.into(),
                    key1: r.get(0)?,
                    key2: r.get(1)?,
                    revision,
                })
            },
        )?;
        keys.extend(rows.collect::<Result<Vec<_>, _>>()?);
    }
    Ok(keys)
}
