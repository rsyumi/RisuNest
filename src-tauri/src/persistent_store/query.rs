use super::{
    active_generation, compare_plugin_storage_keys, current_revision, AnchorOccurrence,
    ArchivedCharacterSummary, AssetAlias, AssetAliasListQuery, AssetAliasPage, AssetOwnerHead,
    AssetOwnerLocator, AssetRepositoryAuthorityState, CharacterPage, CharacterQuery,
    CharacterSummary, ConversationPage, ConversationQuery, ConversationSummary,
    ConversationWindow, ConversationWindowQuery, PluginStorageCatalog, PluginStorageListItem,
    PluginStorageSummary,
    PresetCatalog, PresetSummary, QueryOrder, ReadTarget, StoreError, StoreResult, Versioned,
    CONVERSATION_RANGE_MAX_LIMIT, JAVASCRIPT_MAX_SAFE_INTEGER,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use std::collections::HashSet;

pub(super) fn read_root(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Versioned<Value>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [&target.generation],
            |row| row.get(0),
        )
        .optional()?;
    Ok(Versioned {
        revision: target.revision,
        value: value.map_or(Ok(Value::Object(Map::new())), |value| {
            serde_json::from_str(&value).map_err(StoreError::from)
        })?,
    })
}

pub(super) fn query_presets(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<PresetCatalog> {
    let mut statement = connection.prepare(
        "SELECT preset_id, name, image, configured_index FROM bot_presets
         WHERE generation = ?1 ORDER BY configured_index ASC",
    )?;
    let items = statement
        .query_map([&target.generation], |row| {
            Ok(PresetSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                image: row.get(2)?,
                configured_index: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PresetCatalog {
        revision: target.revision,
        items,
    })
}

pub(super) fn read_preset(
    connection: &Connection,
    id: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<Value>>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM bot_presets WHERE generation = ?1 AND preset_id = ?2",
            params![target.generation, id],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| {
            Ok(Versioned {
                revision: target.revision,
                value: serde_json::from_str(&value)?,
            })
        })
        .transpose()
}

pub(super) fn query_plugin_storage(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<PluginStorageCatalog> {
    let mut statement = connection.prepare(
        "SELECT owner, storage_key, byte_size, ordinal FROM plugin_storage
         WHERE generation = ?1",
    )?;
    let mut items = statement
        .query_map([&target.generation], |row| {
            Ok((
                PluginStorageSummary {
                    owner: row.get(0)?,
                    key: row.get(1)?,
                    byte_size: row.get(2)?,
                },
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    items.sort_by(|(left, left_ordinal), (right, right_ordinal)| {
        compare_plugin_storage_keys(&left.key, *left_ordinal, &right.key, *right_ordinal)
            .then_with(|| left.owner.cmp(&right.owner))
    });
    Ok(PluginStorageCatalog {
        revision: target.revision,
        items: items.into_iter().map(|(item, _)| item).collect(),
    })
}

/// Ordered by owner then legacy key order. No value body crosses the boundary.
pub(super) fn list_plugin_storage(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Vec<PluginStorageListItem>> {
    let mut statement = connection.prepare(
        "SELECT owner, storage_key, byte_size, ordinal,
                CASE WHEN substr(value, 1, 1) = '\"' THEN 'string' ELSE 'json' END,
                claimed_from, import_batch_id, assigned_at
         FROM plugin_storage WHERE generation = ?1",
    )?;
    let mut items = statement
        .query_map([&target.generation], |row| {
            Ok((
                PluginStorageListItem {
                    owner: row.get(0)?,
                    key: row.get(1)?,
                    space: None,
                    value_type: row.get(4)?,
                    byte_size: row.get(2)?,
                    claimed_from: row.get(5)?,
                    import_batch_id: row.get(6)?,
                    assigned_at: row.get(7)?,
                },
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    items.sort_by(|(left, left_ordinal), (right, right_ordinal)| {
        left.owner.cmp(&right.owner).then_with(|| {
            compare_plugin_storage_keys(&left.key, *left_ordinal, &right.key, *right_ordinal)
        })
    });
    Ok(items.into_iter().map(|(item, _)| item).collect())
}

pub(super) fn read_plugin_storage(
    connection: &Connection,
    owner: &str,
    key: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<Value>>> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM plugin_storage
             WHERE generation = ?1 AND owner = ?2 AND storage_key = ?3",
            params![target.generation, owner, key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| {
            Ok(Versioned {
                revision: target.revision,
                value: serde_json::from_str(&value)?,
            })
        })
        .transpose()
}

pub(super) fn read_asset_alias(
    connection: &Connection,
    kind: &str,
    key: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<AssetAlias>>> {
    validate_asset_kind(kind)?;
    let value = connection
        .query_row(
            "SELECT logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata
             FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
            params![target.generation, kind, key],
            asset_alias_from_row,
        )
        .optional()?;
    value
        .map(|value| {
            value.validate()?;
            Ok(Versioned {
                revision: target.revision,
                value,
            })
        })
        .transpose()
}

pub(super) fn read_asset_aliases_by_keys(
    connection: &Connection,
    kind: &str,
    keys: &[String],
    target: &ReadTarget,
) -> StoreResult<Versioned<Vec<AssetAlias>>> {
    validate_asset_kind(kind)?;
    if !(1..=512).contains(&keys.len()) {
        return Err(StoreError::Validation {
            message: "Asset alias key batch size must be between 1 and 512".to_owned(),
        });
    }
    let mut unique_keys = HashSet::with_capacity(keys.len());
    for key in keys {
        if !unique_keys.insert(key.as_str()) {
            return Err(StoreError::Validation {
                message: "Asset alias key batch must contain unique keys".to_owned(),
            });
        }
    }
    let placeholders = (0..keys.len()).map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata
         FROM asset_aliases
         WHERE generation = ? AND kind = ? AND logical_key IN ({placeholders})"
    );
    let parameters = std::iter::once(target.generation.as_str())
        .chain(std::iter::once(kind))
        .chain(keys.iter().map(String::as_str));
    let mut statement = connection.prepare(&sql)?;
    let values = statement
        .query_map(rusqlite::params_from_iter(parameters), asset_alias_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    for value in &values {
        value.validate()?;
    }
    Ok(Versioned {
        revision: target.revision,
        value: values,
    })
}

pub(super) fn list_asset_alias_page(
    connection: &Connection,
    query: &AssetAliasListQuery,
    target: &ReadTarget,
) -> StoreResult<AssetAliasPage> {
    if !(1..=512).contains(&query.limit) {
        return Err(StoreError::Validation {
            message: "Asset alias page limit must be between 1 and 512".to_owned(),
        });
    }
    if let Some(kind) = &query.kind {
        validate_asset_kind(kind)?;
    }
    let cursor = query
        .cursor
        .as_deref()
        .map(|cursor| {
            serde_json::from_str::<(String, String)>(cursor).map_err(|_| StoreError::Validation {
                message: "Asset alias cursor is invalid".to_owned(),
            })
        })
        .transpose()?;
    if let Some((kind, _)) = &cursor {
        validate_asset_kind(kind)?;
        if query
            .kind
            .as_ref()
            .is_some_and(|query_kind| query_kind != kind)
        {
            return Err(StoreError::Validation {
                message: "Asset alias cursor kind does not match the query".to_owned(),
            });
        }
    }
    let (cursor_kind, cursor_key, has_cursor) =
        cursor.as_ref().map_or(("asset", "", 0_i64), |(kind, key)| {
            (kind.as_str(), key.as_str(), 1)
        });
    let mut statement = connection.prepare(
        "SELECT logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata
         FROM asset_aliases
         WHERE generation = ?1
           AND (?2 IS NULL OR kind = ?2)
           AND (?5 = 0 OR kind > ?3 OR (kind = ?3 AND logical_key > ?4))
         ORDER BY kind ASC, logical_key ASC
         LIMIT ?6",
    )?;
    let mut items = statement
        .query_map(
            params![
                target.generation,
                query.kind,
                cursor_kind,
                cursor_key,
                has_cursor,
                query.limit + 1,
            ],
            asset_alias_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    for item in &items {
        item.validate()?;
    }
    let next_cursor = if items.len() > query.limit as usize {
        items.truncate(query.limit as usize);
        let last = items.last().expect("positive page limit");
        Some(serde_json::to_string(&(
            last.kind.as_str(),
            last.key.as_str(),
        ))?)
    } else {
        None
    };
    Ok(AssetAliasPage {
        revision: target.revision,
        items,
        next_cursor,
    })
}

pub(super) fn read_asset_repository_authority(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Versioned<AssetRepositoryAuthorityState>> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT value FROM asset_repository_authority WHERE generation = ?1",
            [&target.generation],
            |row| row.get(0),
        )
        .optional()?;
    let value = match stored {
        Some(stored) => serde_json::from_str(&stored).map_err(|_| StoreError::Validation {
            message: "Asset repository authority state is invalid".to_owned(),
        })?,
        None => AssetRepositoryAuthorityState::Legacy,
    };
    value.validate()?;
    Ok(Versioned {
        revision: target.revision,
        value,
    })
}

pub(super) fn read_asset_owner_head(
    connection: &Connection,
    owner: &AssetOwnerLocator,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<AssetOwnerHead>>> {
    owner.validate()?;
    let (owner_kind, owner_locator) = owner.storage_identity();
    let value = connection
        .query_row(
            "SELECT present, manifest_hash, entry_count
             FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = ?2 AND owner_locator = ?3",
            params![target.generation, owner_kind, owner_locator],
            |row| {
                Ok(AssetOwnerHead {
                    owner: owner.clone(),
                    present: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    entry_count: row.get(2)?,
                })
            },
        )
        .optional()?;
    value
        .map(|value| {
            value.validate()?;
            Ok(Versioned {
                revision: target.revision,
                value,
            })
        })
        .transpose()
}

pub(super) fn list_asset_aliases(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Versioned<Vec<AssetAlias>>> {
    let mut statement = connection.prepare(
        "SELECT logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata
         FROM asset_aliases WHERE generation = ?1 ORDER BY kind ASC, logical_key ASC",
    )?;
    let values = statement
        .query_map([&target.generation], asset_alias_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    for value in &values {
        value.validate()?;
    }
    Ok(Versioned {
        revision: target.revision,
        value: values,
    })
}

pub(super) fn list_asset_owner_heads(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Versioned<Vec<AssetOwnerHead>>> {
    let rows = {
        let mut statement = connection.prepare(
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads WHERE generation = ?1
             ORDER BY owner_kind ASC, owner_locator ASC",
        )?;
        let rows = statement
            .query_map([&target.generation], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let values = rows
        .into_iter()
        .map(
            |(owner_kind, owner_locator, present, manifest_hash, entry_count)| {
                let owner = match owner_kind.as_str() {
                    "character-additional-assets" => AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: owner_locator,
                    },
                    "root-module-assets" => AssetOwnerLocator::RootModuleAssets {
                        index: stored_asset_owner_index(&owner_locator, "root module")?,
                    },
                    "persona-embedded-module-assets" => {
                        AssetOwnerLocator::PersonaEmbeddedModuleAssets {
                            index: stored_asset_owner_index(&owner_locator, "persona module")?,
                        }
                    }
                    _ => {
                        return Err(StoreError::Validation {
                            message: "Stored asset owner kind is invalid".to_owned(),
                        })
                    }
                };
                let value = AssetOwnerHead {
                    owner,
                    present,
                    manifest_hash,
                    entry_count,
                };
                value.validate()?;
                Ok(value)
            },
        )
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(Versioned {
        revision: target.revision,
        value: values,
    })
}

fn stored_asset_owner_index(value: &str, subject: &str) -> StoreResult<i64> {
    let index = value.parse::<i64>().map_err(|_| StoreError::Validation {
        message: format!("Stored {subject} asset owner locator is invalid"),
    })?;
    if index.to_string() != value {
        return Err(StoreError::Validation {
            message: format!("Stored {subject} asset owner locator is noncanonical"),
        });
    }
    Ok(index)
}

fn asset_alias_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetAlias> {
    Ok(AssetAlias {
        key: row.get(0)?,
        object_hash: row.get(1)?,
        kind: row.get(2)?,
        size: row.get(3)?,
        mime: row.get(4)?,
        name: row.get(5)?,
        ext: row.get(6)?,
        inlay_type: row.get(7)?,
        width: row.get(8)?,
        height: row.get(9)?,
        metadata: serde_json::from_str(&row.get::<_, String>(10)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

pub(super) fn validate_asset_kind(kind: &str) -> StoreResult<()> {
    if matches!(kind, "asset" | "inlay") {
        Ok(())
    } else {
        Err(StoreError::Validation {
            message: "Asset alias kind must be asset or inlay".to_owned(),
        })
    }
}

const CHARACTER_SUMMARY_COLUMNS: &str =
    "character_id, name, image, configured_index, recent_at, trashed, conversation_count,
     type, creator_notes, trash_time, archived_object";

fn character_summary_from_row(row: &rusqlite::Row<'_>) -> StoreResult<CharacterSummary> {
    let archived = row
        .get::<_, Option<String>>(10)?
        .map(|stored| {
            serde_json::from_str::<super::archive::ArchivedObject>(&stored).map(|archived| {
                ArchivedCharacterSummary {
                    archived_at: archived.archived_at,
                    conversation_count: archived.conversation_count,
                    message_count: archived.message_count,
                }
            })
        })
        .transpose()?;
    Ok(CharacterSummary {
        id: row.get(0)?,
        name: row.get(1)?,
        image: row.get(2)?,
        configured_index: row.get(3)?,
        recent_at: row.get(4)?,
        trashed: row.get::<_, i64>(5)? != 0,
        conversation_count: row.get(6)?,
        r#type: row.get(7)?,
        creator_notes: row.get(8)?,
        trash_time: row.get(9)?,
        archived,
    })
}

pub(super) fn query_characters(
    connection: &Connection,
    query: &CharacterQuery,
    target: &ReadTarget,
) -> StoreResult<CharacterPage> {
    let (limit, offset) = page_input(query.limit, query.cursor.as_deref())?;
    let order = order_sql(query.order);
    let search = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let mut statement = connection.prepare(&format!(
        "SELECT {CHARACTER_SUMMARY_COLUMNS}
         FROM characters
         WHERE generation = ?1 AND trashed = ?2
         ORDER BY {order}"
    ))?;
    let mut rows = statement.query(params![target.generation, query.trash as i64])?;
    let mut items = Vec::new();
    let mut matched = 0;
    let mut has_more = false;
    while let Some(row) = rows.next()? {
        let summary = character_summary_from_row(row)?;
        if search
            .as_ref()
            .is_some_and(|search| !summary.name.to_lowercase().contains(search))
        {
            continue;
        }
        if matched < offset {
            matched += 1;
            continue;
        }
        if items.len() as i64 == limit {
            has_more = true;
            break;
        }
        items.push(summary);
        matched += 1;
    }
    Ok(CharacterPage {
        revision: target.revision,
        next_cursor: has_more.then(|| (offset + items.len() as i64).to_string()),
        items,
    })
}

/// One summary by identity. Targeted invalidation reprojects a single changed
/// character without walking the catalog.
pub(super) fn read_character_summary(
    connection: &Connection,
    id: &str,
    target: &ReadTarget,
) -> StoreResult<Option<CharacterSummary>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {CHARACTER_SUMMARY_COLUMNS} FROM characters
         WHERE generation = ?1 AND character_id = ?2"
    ))?;
    let mut rows = statement.query(params![target.generation, id])?;
    match rows.next()? {
        Some(row) => Ok(Some(character_summary_from_row(row)?)),
        None => Ok(None),
    }
}

pub(super) fn read_character(
    connection: &Connection,
    id: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<Value>>> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT detail, archived_object FROM characters
             WHERE generation = ?1 AND character_id = ?2",
            params![target.generation, id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((detail, archived_object)) = row else {
        return Ok(None);
    };
    // Failing is safer than handing back the marker that stands in for the
    // archived detail.
    if archived_object.is_some() {
        return Err(super::archive::archived_error(id));
    }
    Ok(Some(Versioned {
        revision: target.revision,
        value: serde_json::from_str(&detail)?,
    }))
}

pub(super) fn query_conversations(
    connection: &Connection,
    query: &ConversationQuery,
    target: &ReadTarget,
) -> StoreResult<ConversationPage> {
    let (limit, offset) = page_input(query.limit, query.cursor.as_deref())?;
    let order = order_sql(query.order);
    let mut statement = connection.prepare(&format!(
        "SELECT conversation_id, character_id, name, configured_index, recent_at, message_count, detail
         FROM conversations WHERE generation = ?1 AND character_id = ?2
         ORDER BY {order} LIMIT ?3 OFFSET ?4"
    ))?;
    let mut rows = statement.query(params![
        target.generation,
        query.character_id,
        limit + 1,
        offset
    ])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let detail: Value = serde_json::from_str(&row.get::<_, String>(6)?)?;
        items.push(ConversationSummary {
            id: row.get(0)?,
            character_id: row.get(1)?,
            name: row.get(2)?,
            folder_id: detail
                .get("folderId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            binded_persona: detail
                .get("bindedPersona")
                .and_then(Value::as_str)
                .map(str::to_owned),
            configured_index: row.get(3)?,
            recent_at: row.get(4)?,
            message_count: row.get(5)?,
            fm_index: detail.get("fmIndex").and_then(Value::as_i64),
        });
    }
    let has_more = items.len() as i64 > limit;
    if has_more {
        items.pop();
    }
    Ok(ConversationPage {
        revision: target.revision,
        next_cursor: has_more.then(|| (offset + items.len() as i64).to_string()),
        items,
    })
}

pub(super) fn read_conversation(
    connection: &Connection,
    character_id: &str,
    conversation_id: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<Value>>> {
    let detail: Option<String> = connection
        .query_row(
            "SELECT detail FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![target.generation, character_id, conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(detail) = detail else {
        return Ok(None);
    };
    Ok(Some(Versioned {
        revision: target.revision,
        value: conversation_value(
            connection,
            &target.generation,
            character_id,
            conversation_id,
            detail,
        )?,
    }))
}

pub(super) fn read_conversation_metadata(
    connection: &Connection,
    character_id: &str,
    conversation_id: &str,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<super::PersistentConversationMetadata>>> {
    let row: Option<(String, i64)> = connection
        .query_row(
            "SELECT detail, message_count FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![target.generation, character_id, conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((detail, total_messages)) = row else {
        return Ok(None);
    };
    if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(&total_messages) {
        return Err(StoreError::Validation {
            message: "conversation message count must be a nonnegative safe integer".to_owned(),
        });
    }
    let mut conversation = into_object(
        serde_json::from_str(&detail)?,
        "Conversation detail must be an object",
    )?;
    conversation.remove("message");
    Ok(Some(Versioned {
        revision: target.revision,
        value: super::PersistentConversationMetadata {
            character_id: character_id.to_owned(),
            conversation_id: conversation_id.to_owned(),
            conversation: Value::Object(conversation),
            total_messages,
        },
    }))
}

pub(super) fn read_conversation_window(
    connection: &Connection,
    query: &ConversationWindowQuery,
    target: &ReadTarget,
) -> StoreResult<Option<Versioned<ConversationWindow>>> {
    if query.anchor_occurrence.is_some() && query.anchor_message_id.is_none() {
        return Err(StoreError::Validation {
            message: "conversation anchor occurrence requires anchorMessageId".to_owned(),
        });
    }
    let absolute_range = match query.start_index {
        None => None,
        Some(start_index) => {
            if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(&start_index) {
                return Err(StoreError::Validation {
                    message: "conversation range startIndex must be a nonnegative safe integer"
                        .to_owned(),
                });
            }
            let Some(limit) = query.limit else {
                return Err(StoreError::Validation {
                    message: "conversation range limit must be a positive safe integer".to_owned(),
                });
            };
            if !(1..=CONVERSATION_RANGE_MAX_LIMIT).contains(&limit) {
                return Err(StoreError::Validation {
                    message: format!(
                        "conversation range limit must be between 1 and {CONVERSATION_RANGE_MAX_LIMIT}"
                    ),
                });
            }
            if query.anchor_message_id.is_some()
                || query.anchor_occurrence.is_some()
                || query.before.is_some()
                || query.after.is_some()
            {
                return Err(StoreError::Validation {
                    message: "conversation absolute range cannot include anchor options".to_owned(),
                });
            }
            Some((start_index, limit))
        }
    };
    let total: Option<i64> = connection
        .query_row(
            "SELECT message_count FROM conversations WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![target.generation, query.character_id, query.conversation_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(total_messages) = total else {
        return Ok(None);
    };

    let (start_index, end_index) = match absolute_range {
        Some((start_index, limit)) => {
            let start_index = start_index.min(total_messages);
            (start_index, (start_index + limit).min(total_messages))
        }
        None => match query.anchor_message_id.as_deref() {
            Some(anchor_id) => {
                let anchor: Option<i64> = match query
                    .anchor_occurrence
                    .as_ref()
                    .unwrap_or(&AnchorOccurrence::First)
                {
                    AnchorOccurrence::First => connection.query_row(
                        "SELECT message_index FROM messages
                         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_id = ?4
                         ORDER BY message_index ASC LIMIT 1",
                        params![target.generation, query.character_id, query.conversation_id, anchor_id],
                        |row| row.get(0),
                    ).optional()?,
                    AnchorOccurrence::Last => connection.query_row(
                        "SELECT message_index FROM messages
                         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_id = ?4
                         ORDER BY message_index DESC LIMIT 1",
                        params![target.generation, query.character_id, query.conversation_id, anchor_id],
                        |row| row.get(0),
                    ).optional()?,
                };
                let Some(anchor) = anchor else {
                    return Ok(None);
                };
                let before = validated_window_span(query.before, 0, "before")?;
                let after = validated_window_span(query.after, 0, "after")?;
                (
                    (anchor - before).max(0),
                    (anchor + after + 1).min(total_messages),
                )
            }
            None => {
                let limit = validated_window_span(query.limit, 128, "limit")?;
                ((total_messages - limit).max(0), total_messages)
            }
        },
    };
    let messages = read_messages(
        connection,
        &target.generation,
        &query.character_id,
        &query.conversation_id,
        start_index,
        end_index,
    )?;
    Ok(Some(Versioned {
        revision: target.revision,
        value: ConversationWindow {
            character_id: query.character_id.clone(),
            conversation_id: query.conversation_id.clone(),
            messages,
            start_index,
            end_index,
            total_messages,
            has_more_before: start_index > 0,
            has_more_after: end_index < total_messages,
        },
    }))
}

// The anchor and anchorless window paths accept untrusted JSON, so their
// spans get the same safe-integer bound as the absolute-range path to keep
// the index arithmetic overflow-free.
fn validated_window_span(value: Option<i64>, default: i64, name: &str) -> StoreResult<i64> {
    let value = value.unwrap_or(default);
    if !(0..=JAVASCRIPT_MAX_SAFE_INTEGER).contains(&value) {
        return Err(StoreError::Validation {
            message: format!("conversation window {name} must be a nonnegative safe integer"),
        });
    }
    Ok(value)
}

pub(super) fn materialize(connection: &Connection, revision: Option<i64>) -> StoreResult<Value> {
    materialize_with_target(connection, revision).map(|(value, _)| value)
}

pub(super) fn materialize_with_target(
    connection: &Connection,
    revision: Option<i64>,
) -> StoreResult<(Value, ReadTarget)> {
    let transaction = connection.unchecked_transaction()?;
    let actual = current_revision(&transaction)?;
    let expected = revision.unwrap_or(actual);
    if expected != actual {
        return Err(StoreError::RevisionConflict { expected, actual });
    }
    let generation = active_generation(&transaction)?;
    let value = materialize_generation(&transaction, &generation)?
        .ok_or(StoreError::RevisionConflict { expected, actual })?;
    transaction.commit()?;
    Ok((
        value,
        ReadTarget {
            revision: actual,
            generation,
        },
    ))
}

pub(super) fn materialize_target(
    connection: &Connection,
    target: &ReadTarget,
) -> StoreResult<Value> {
    let value = materialize_generation(connection, &target.generation)?.ok_or_else(|| {
        StoreError::Store {
            message: "Persistent lease generation is missing its root".to_owned(),
        }
    })?;
    Ok(value)
}

pub(super) fn materialize_staging(connection: &Connection, staging_id: &str) -> StoreResult<Value> {
    let transaction = connection.unchecked_transaction()?;
    super::commit::require_staging(&transaction, staging_id)?;
    let value = materialize_generation(&transaction, staging_id)?.ok_or_else(|| {
        StoreError::Validation {
            message: "Staging generation does not exist".to_owned(),
        }
    })?;
    transaction.commit()?;
    Ok(value)
}

fn materialize_generation(connection: &Connection, generation: &str) -> StoreResult<Option<Value>> {
    let root: Option<String> = connection
        .query_row(
            "SELECT value FROM root WHERE generation = ?1",
            [generation],
            |row| row.get(0),
        )
        .optional()?;
    let Some(root) = root else {
        return Ok(None);
    };
    let mut database = into_object(
        serde_json::from_str(&root)?,
        "Persistent root must be an object",
    )?;
    let character_records = {
        let mut statement = connection.prepare(
            "SELECT character_id, detail FROM characters
             WHERE generation = ?1 AND archived_object IS NULL
             ORDER BY configured_index ASC",
        )?;
        let rows = statement.query_map([&generation], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut characters = Vec::new();
    for (character_id, detail) in character_records {
        let mut character = into_object(
            serde_json::from_str(&detail)?,
            "Character detail must be an object",
        )?;
        let conversation_records = {
            let mut statement = connection.prepare(
                "SELECT conversation_id, detail FROM conversations
                 WHERE generation = ?1 AND character_id = ?2 ORDER BY configured_index ASC",
            )?;
            let rows = statement.query_map(params![generation, character_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut chats = Vec::new();
        for (conversation_id, detail) in conversation_records {
            chats.push(conversation_value(
                connection,
                generation,
                &character_id,
                &conversation_id,
                detail,
            )?);
        }
        character.insert("chats".to_owned(), Value::Array(chats));
        characters.push(Value::Object(character));
    }
    database.insert("characters".to_owned(), Value::Array(characters));
    let presets = {
        let mut statement = connection.prepare(
            "SELECT value FROM bot_presets WHERE generation = ?1 ORDER BY configured_index ASC",
        )?;
        let presets = statement
            .query_map([&generation], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect::<StoreResult<Vec<_>>>()?;
        presets
    };
    database.insert("botPresets".to_owned(), Value::Array(presets));
    let plugin_storage = super::export::materialized_plugin_storage(connection, &generation)?;
    database.insert(
        "pluginCustomStorage".to_owned(),
        Value::Object(plugin_storage),
    );
    Ok(Some(Value::Object(database)))
}

fn page_input(limit: i64, cursor: Option<&str>) -> StoreResult<(i64, i64)> {
    if limit <= 0 {
        return Err(StoreError::Validation {
            message: "Query limit must be a positive number".to_owned(),
        });
    }
    Ok((limit, cursor.and_then(parse_cursor).unwrap_or(0)))
}

fn parse_cursor(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let value = value.strip_prefix('+').unwrap_or(value);
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn order_sql(order: QueryOrder) -> &'static str {
    match order {
        QueryOrder::Configured => "configured_index ASC",
        QueryOrder::Recent => "recent_at DESC, configured_index ASC",
    }
}

fn read_messages(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    start: i64,
    end: i64,
) -> StoreResult<Vec<Value>> {
    let mut statement = connection.prepare("SELECT value FROM messages WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3 AND message_index >= ?4 AND message_index < ?5 ORDER BY message_index ASC")?;
    let rows = statement.query_map(
        params![generation, character_id, conversation_id, start, end],
        |row| row.get::<_, String>(0),
    )?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn conversation_value(
    connection: &Connection,
    generation: &str,
    character_id: &str,
    conversation_id: &str,
    detail: String,
) -> StoreResult<Value> {
    let mut value = into_object(
        serde_json::from_str(&detail)?,
        "Conversation detail must be an object",
    )?;
    value.insert(
        "message".to_owned(),
        Value::Array(read_messages(
            connection,
            generation,
            character_id,
            conversation_id,
            0,
            i64::MAX,
        )?),
    );
    Ok(Value::Object(value))
}

fn into_object(value: Value, message: &str) -> StoreResult<Map<String, Value>> {
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(StoreError::Store {
            message: message.to_owned(),
        }),
    }
}
