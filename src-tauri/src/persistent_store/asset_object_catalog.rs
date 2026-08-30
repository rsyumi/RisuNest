use super::{StoreError, StoreResult};
use crate::asset_repository::migration_gc::AssetGcCandidate;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

pub(crate) const ASSET_OBJECT_CATALOG_MAX_PAGE: i64 = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssetObjectRegistration {
    pub(crate) object_hash: String,
    pub(crate) byte_size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AssetObjectCatalogPage {
    pub(crate) items: Vec<AssetGcCandidate>,
    pub(crate) next_cursor: Option<String>,
}

pub(crate) struct AssetObjectCatalog<'a> {
    connection: &'a mut Connection,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CatalogCursor {
    version: u32,
    created_at_ms: i64,
    object_hash: String,
}

impl<'a> AssetObjectCatalog<'a> {
    pub(super) fn new(connection: &'a mut Connection) -> Self {
        Self { connection }
    }

    pub(crate) fn register(
        &mut self,
        objects: &[AssetObjectRegistration],
        created_at_ms: i64,
    ) -> StoreResult<()> {
        if created_at_ms < 0 {
            return validation("asset object creation time must be nonnegative");
        }
        if objects.len() > ASSET_OBJECT_CATALOG_MAX_PAGE as usize {
            return validation("asset object registration batch exceeds the bounded limit");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for object in objects {
            validate_hash(&object.object_hash)?;
            let byte_size =
                i64::try_from(object.byte_size).map_err(|_| StoreError::Validation {
                    message: "asset object size exceeds the SQLite integer limit".to_owned(),
                })?;
            let tombstone: Option<(i64, String)> = transaction
                .query_row(
                    "SELECT byte_size, physical_key FROM asset_object_deletions
                     WHERE object_hash = ?1",
                    [&object.object_hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((deleted_size, physical_key)) = tombstone {
                let expected_key =
                    crate::asset_repository::object_physical_key(&object.object_hash);
                if deleted_size != byte_size || physical_key != expected_key {
                    return validation(
                        "asset object deletion tombstone conflicts with the recreated object",
                    );
                }
                transaction.execute(
                    "INSERT INTO asset_objects (object_hash, byte_size, created_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(object_hash) DO UPDATE SET
                        byte_size = excluded.byte_size,
                        created_at_ms = excluded.created_at_ms",
                    params![object.object_hash, byte_size, created_at_ms],
                )?;
                transaction.execute(
                    "DELETE FROM asset_object_deletions WHERE object_hash = ?1",
                    [&object.object_hash],
                )?;
            } else {
                transaction.execute(
                    "INSERT INTO asset_objects (object_hash, byte_size, created_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(object_hash) DO NOTHING",
                    params![object.object_hash, byte_size, created_at_ms],
                )?;
            }
            let stored_size: i64 = transaction.query_row(
                "SELECT byte_size FROM asset_objects WHERE object_hash = ?1",
                [&object.object_hash],
                |row| row.get(0),
            )?;
            if stored_size != byte_size {
                return validation("asset object catalog size conflicts with the immutable object");
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(super) fn query(
    connection: &Connection,
    limit: i64,
    cursor: Option<&str>,
) -> StoreResult<AssetObjectCatalogPage> {
    if !(1..=ASSET_OBJECT_CATALOG_MAX_PAGE).contains(&limit) {
        return validation("asset object catalog limit is outside the bounded range");
    }
    let cursor = cursor.map(decode_cursor).transpose()?;
    let query_limit = limit.checked_add(1).ok_or_else(|| StoreError::Validation {
        message: "asset object catalog limit overflow".to_owned(),
    })?;
    let mut items = Vec::with_capacity(query_limit as usize);
    match cursor {
        None => {
            let mut statement = connection.prepare(
                "SELECT object_hash, byte_size, created_at_ms
                 FROM asset_objects
                 ORDER BY created_at_ms ASC, object_hash ASC
                 LIMIT ?1",
            )?;
            let rows = statement.query_map([query_limit], read_candidate)?;
            for row in rows {
                items.push(row?);
            }
        }
        Some(cursor) => {
            let mut statement = connection.prepare(
                "SELECT object_hash, byte_size, created_at_ms
                 FROM asset_objects
                 WHERE created_at_ms > ?1
                    OR (created_at_ms = ?1 AND object_hash > ?2)
                 ORDER BY created_at_ms ASC, object_hash ASC
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![cursor.created_at_ms, cursor.object_hash, query_limit],
                read_candidate,
            )?;
            for row in rows {
                items.push(row?);
            }
        }
    }
    let has_more = items.len() > limit as usize;
    if has_more {
        items.pop();
    }
    let next_cursor = has_more
        .then(|| items.last())
        .flatten()
        .map(encode_cursor)
        .transpose()?;
    Ok(AssetObjectCatalogPage { items, next_cursor })
}

fn read_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetGcCandidate> {
    let byte_size: i64 = row.get(1)?;
    Ok(AssetGcCandidate {
        object_hash: row.get(0)?,
        byte_size: u64::try_from(byte_size)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, byte_size))?,
        created_at_ms: row.get(2)?,
    })
}

fn encode_cursor(item: &AssetGcCandidate) -> StoreResult<String> {
    let bytes = serde_json::to_vec(&CatalogCursor {
        version: 1,
        created_at_ms: item.created_at_ms,
        object_hash: item.object_hash.clone(),
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor(value: &str) -> StoreResult<CatalogCursor> {
    if value.is_empty() || value.len() > 512 {
        return validation("asset object catalog cursor is invalid");
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| StoreError::Validation {
            message: "asset object catalog cursor is invalid".to_owned(),
        })?;
    let cursor: CatalogCursor =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::Validation {
            message: "asset object catalog cursor is invalid".to_owned(),
        })?;
    if cursor.version != 1 || cursor.created_at_ms < 0 {
        return validation("asset object catalog cursor is invalid");
    }
    validate_hash(&cursor.object_hash)?;
    Ok(cursor)
}

pub(super) fn validate_cursor(value: &str) -> StoreResult<()> {
    decode_cursor(value).map(|_| ())
}

pub(super) fn cursor_has_successor(connection: &Connection, value: &str) -> StoreResult<bool> {
    let cursor = decode_cursor(value)?;
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM asset_objects
            WHERE created_at_ms > ?1
               OR (created_at_ms = ?1 AND object_hash > ?2)
        )",
        params![cursor.created_at_ms, cursor.object_hash],
        |row| row.get(0),
    )?)
}

fn validate_hash(hash: &str) -> StoreResult<()> {
    if super::is_lowercase_sha256_hex(hash) {
        return Ok(());
    }
    validation("asset object hash must be a lowercase SHA-256 hash")
}

fn validation<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.to_owned(),
    })
}
