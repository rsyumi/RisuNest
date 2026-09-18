//! Raw generation projection for portable archives. Never execute source DDL or reserialize JSON.
use super::{PersistentStore, StoreError, StoreResult};
use crate::local_backup::CancellationProbe;
use rusqlite::{types::ValueRef, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub(crate) struct PortableTable {
    pub(crate) name: &'static str,
    /// SQL declaration types describe the live schema. Archive columns intentionally have no
    /// affinity: a damaged source's actual storage class must survive capture unchanged.
    pub(crate) columns: &'static [(&'static str, &'static str)],
    pub(crate) order: &'static str,
}

pub(crate) const TABLES: &[PortableTable] = &[
    PortableTable {
        name: "root",
        columns: &[("value", "TEXT")],
        order: "value",
    },
    PortableTable {
        name: "bot_presets",
        columns: &[
            ("preset_id", "TEXT"),
            ("configured_index", "INTEGER"),
            ("name", "TEXT"),
            ("image", "TEXT"),
            ("value", "TEXT"),
        ],
        order: "preset_id",
    },
    PortableTable {
        name: "characters",
        columns: &[
            ("character_id", "TEXT"),
            ("configured_index", "INTEGER"),
            ("recent_at", "INTEGER"),
            ("trashed", "INTEGER"),
            ("name", "TEXT"),
            ("image", "TEXT"),
            ("conversation_count", "INTEGER"),
            ("type", "TEXT"),
            ("creator_notes", "TEXT"),
            ("trash_time", "INTEGER"),
            ("detail", "TEXT"),
            ("archived_object", "TEXT"),
        ],
        order: "character_id",
    },
    PortableTable {
        name: "conversations",
        columns: &[
            ("character_id", "TEXT"),
            ("conversation_id", "TEXT"),
            ("configured_index", "INTEGER"),
            ("recent_at", "INTEGER"),
            ("name", "TEXT"),
            ("message_count", "INTEGER"),
            ("detail", "TEXT"),
        ],
        order: "character_id, conversation_id",
    },
    PortableTable {
        name: "messages",
        columns: &[
            ("character_id", "TEXT"),
            ("conversation_id", "TEXT"),
            ("message_index", "INTEGER"),
            ("message_id", "TEXT"),
            ("value", "TEXT"),
        ],
        order: "character_id, conversation_id, message_index",
    },
    PortableTable {
        name: "plugin_storage",
        columns: &[
            ("owner", "TEXT"),
            ("storage_key", "TEXT"),
            ("byte_size", "INTEGER"),
            ("ordinal", "INTEGER"),
            ("value", "TEXT"),
            ("claimed_from", "TEXT"),
            ("import_batch_id", "TEXT"),
            ("assigned_at", "INTEGER"),
        ],
        order: "owner, storage_key",
    },
    PortableTable {
        name: "asset_aliases",
        columns: &[
            ("logical_key", "TEXT"),
            ("object_hash", "TEXT"),
            ("kind", "TEXT"),
            ("size", "INTEGER"),
            ("mime", "TEXT"),
            ("name", "TEXT"),
            ("ext", "TEXT"),
            ("inlay_type", "TEXT"),
            ("width", "INTEGER"),
            ("height", "INTEGER"),
            ("metadata", "TEXT"),
        ],
        order: "kind, logical_key",
    },
    PortableTable {
        name: "asset_owner_heads",
        columns: &[
            ("owner_kind", "TEXT"),
            ("owner_locator", "TEXT"),
            ("present", "INTEGER"),
            ("manifest_hash", "TEXT"),
            ("entry_count", "INTEGER"),
        ],
        order: "owner_kind, owner_locator",
    },
    PortableTable {
        name: "asset_repository_authority",
        columns: &[("value", "TEXT")],
        order: "value",
    },
];

impl PortableTable {
    pub(crate) fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|(name, _)| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub(crate) fn create_sql(&self) -> String {
        format!("CREATE TABLE {} ({})", self.name, self.column_list())
    }

    pub(crate) fn index_sql(&self) -> Option<(String, String)> {
        if self.columns.len() == 1 {
            return None;
        }
        let name = format!("portable_{}_order", self.name);
        Some((
            name.clone(),
            format!("CREATE INDEX {name} ON {} ({})", self.name, self.order),
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawTableDigest {
    pub(crate) table: &'static str,
    pub(crate) rows: u64,
    pub(crate) sha256: [u8; 32],
}

pub(crate) fn create_raw_tables(destination: &Connection) -> StoreResult<()> {
    for table in TABLES {
        destination.execute_batch(&table.create_sql())?;
        if let Some((_, sql)) = table.index_sql() {
            destination.execute_batch(&sql)?;
        }
    }
    Ok(())
}

/// Presents one generation under the raw table names, without copying a row. SQLite resolves an
/// unqualified name in the temp schema first, so this needs a connection of its own.
pub(crate) fn install_generation_views(
    destination: &Connection,
    generation: &str,
) -> StoreResult<()> {
    let quoted = generation.replace('\'', "''");
    for table in TABLES {
        destination.execute_batch(&format!(
            "CREATE TEMP VIEW {name} AS SELECT {columns} FROM main.{name} WHERE generation='{quoted}'",
            name = table.name,
            columns = table.column_list(),
        ))?;
    }
    Ok(())
}

fn cancelled(probe: &dyn CancellationProbe) -> StoreResult<()> {
    if probe.is_cancelled() {
        return Err(StoreError::Validation {
            message: "portable capture cancelled".into(),
        });
    }
    Ok(())
}

/// Copy one stable read transaction into an already-created, owned archive catalog. Values are
/// bound straight from SQLite, so invalid JSON, SQL NULL and even invalid TEXT bytes survive.
/// The transaction rolls back all tables on cancellation/failure. Memory is one SQLite row.
fn copy_generation(
    source: &Connection,
    generation: &str,
    destination: &mut Connection,
    probe: &dyn CancellationProbe,
) -> StoreResult<Vec<RawTableDigest>> {
    validate_live_columns(source)?;
    let transaction = destination.transaction()?;
    let mut result = Vec::with_capacity(TABLES.len());
    for table in TABLES {
        cancelled(probe)?;
        let columns = table.column_list();
        let mut select = source.prepare(&format!(
            "SELECT {columns} FROM {} WHERE generation=?1 ORDER BY {}",
            table.name, table.order
        ))?;
        let placeholders = (0..table.columns.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let mut insert = transaction.prepare(&format!(
            "INSERT INTO {} ({columns}) VALUES ({placeholders})",
            table.name
        ))?;
        let mut rows = select.query([generation])?;
        let mut digest = table_hasher(table);
        let mut count = 0_u64;
        while let Some(row) = rows.next()? {
            cancelled(probe)?;
            digest.update([0xf0]);
            for index in 0..table.columns.len() {
                let value = row.get_ref(index)?;
                hash_value(&mut digest, value);
                insert
                    .raw_bind_parameter(index + 1, rusqlite::types::ToSqlOutput::Borrowed(value))?;
            }
            insert.raw_execute()?;
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("portable row count overflow"))?;
        }
        digest.update(count.to_le_bytes());
        result.push(RawTableDigest {
            table: table.name,
            rows: count,
            sha256: digest.finalize().into(),
        });
    }
    cancelled(probe)?;
    transaction.commit()?;
    Ok(result)
}

impl PersistentStore {
    pub(crate) fn portable_source_generation(&self, lease: &str) -> StoreResult<String> {
        let (_, target) = self.read_view(Some(lease))?;
        Ok(target.generation.clone())
    }
    pub(crate) fn capture_portable_records(
        &self,
        lease: &str,
        destination: &mut Connection,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<Vec<RawTableDigest>> {
        let (source, target) = self.read_view(Some(lease))?;
        copy_generation(source, &target.generation, destination, probe)
    }

    /// Prepare raw rows without activating them. The file restore coordinator must additionally
    /// validate payloads, owner manifests and the F0 reference contract, publish recovery, and
    /// obtain the existing replacement fence before using the normal commit API.
    pub(crate) fn stage_portable_records(
        &mut self,
        source: &Connection,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<super::StagingResult> {
        super::portable_validation::validate_records(source, probe)?;
        let expected = digest_raw_tables(source, probe)?;
        let stage = self.replace_begin()?;
        let result = (|| -> StoreResult<()> {
            let transaction = self.connection.transaction()?;
            for table in TABLES {
                cancelled(probe)?;
                transaction.execute(
                    &format!("DELETE FROM {} WHERE generation=?1", table.name),
                    [&stage.staging_id],
                )?;
                let columns = table.column_list();
                let mut select = source.prepare(&format!(
                    "SELECT {columns} FROM {} ORDER BY {}",
                    table.name, table.order
                ))?;
                let placeholders = (0..=table.columns.len())
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                let mut insert = transaction.prepare(&format!(
                    "INSERT INTO {} (generation,{columns}) VALUES ({placeholders})",
                    table.name
                ))?;
                let mut rows = select.query([])?;
                while let Some(row) = rows.next()? {
                    cancelled(probe)?;
                    insert.raw_bind_parameter(1, &stage.staging_id)?;
                    for index in 0..table.columns.len() {
                        insert.raw_bind_parameter(
                            index + 2,
                            rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(index)?),
                        )?;
                    }
                    insert.raw_execute()?;
                }
            }
            if digest_tables(&transaction, Some(&stage.staging_id), probe)? != expected {
                return Err(invalid("portable staging changed raw SQL values"));
            }
            cancelled(probe)?;
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.replace_abort(&stage.staging_id)?;
            return Err(error);
        }
        Ok(stage)
    }
}

/// Stages only the records a partial import chose, keeping the settings record and the storage
/// authorities whole. Every row it writes is copied from the archive unchanged except for the
/// ordering columns, which are renumbered because the records around them are gone.
///
/// The stored files all come in: an import that left some out would have to decide which of them
/// the kept records still need, and the unused image cleanup answers that question afterwards
/// with the whole library in view.
pub(crate) fn stage_portable_records_selected(
    store: &mut PersistentStore,
    source: &Connection,
    selection: &crate::portable_backup::ClosedSelection,
    probe: &dyn CancellationProbe,
) -> StoreResult<super::StagingResult> {
    super::portable_validation::validate_records(source, probe)?;
    let stage = store.replace_begin()?;
    let result = copy_selected(store, source, &stage.staging_id, selection, probe);
    if let Err(error) = result {
        store.replace_abort(&stage.staging_id)?;
        return Err(error);
    }
    Ok(stage)
}

fn quoted_list(values: &[String]) -> String {
    values
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

fn copy_selected(
    store: &mut PersistentStore,
    source: &Connection,
    staging: &str,
    selection: &crate::portable_backup::ClosedSelection,
    probe: &dyn CancellationProbe,
) -> StoreResult<()> {
    let characters = quoted_list(&selection.characters);
    let filters: Vec<(&str, String)> = vec![
        ("bot_presets", format!("preset_id IN ({})", quoted_list(&selection.presets))),
        ("plugin_storage", format!("storage_key IN ({})", quoted_list(&selection.plugins))),
        ("characters", format!("character_id IN ({characters})")),
        ("conversations", format!("character_id IN ({characters})")),
        ("messages", format!("character_id IN ({characters})")),
    ];
    let transaction = store.connection.transaction()?;
    for table in TABLES {
        cancelled(probe)?;
        let filter = filters
            .iter()
            .find(|(name, _)| *name == table.name)
            .map(|(_, filter)| filter.as_str())
            .unwrap_or_default();
        let columns = table.column_list();
        transaction.execute(
            &format!("DELETE FROM {} WHERE generation=?1", table.name),
            [staging],
        )?;
        let mut select = source.prepare(&format!(
            "SELECT {columns} FROM {}{} ORDER BY {}",
            table.name,
            match filter.is_empty() {
                true => String::new(),
                false => format!(" WHERE {filter}"),
            },
            table.order
        ))?;
        let placeholders = (0..=table.columns.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let mut insert = transaction.prepare(&format!(
            "INSERT INTO {} (generation,{columns}) VALUES ({placeholders})",
            table.name
        ))?;
        let mut rows = select.query([])?;
        while let Some(row) = rows.next()? {
            cancelled(probe)?;
            insert.raw_bind_parameter(1, staging)?;
            for index in 0..table.columns.len() {
                insert.raw_bind_parameter(
                    index + 2,
                    rusqlite::types::ToSqlOutput::Borrowed(row.get_ref(index)?),
                )?;
            }
            insert.raw_execute()?;
        }
        drop(rows);
        drop(insert);
        drop(select);
    }
    // An owner head belongs to a record; the ones whose character stayed behind have no owner.
    transaction.execute(
        "DELETE FROM asset_owner_heads WHERE generation=?1 AND owner_kind IN ('character','group')
         AND owner_locator NOT IN (SELECT character_id FROM characters WHERE generation=?1)",
        [staging],
    )?;
    renumber_selected(&transaction, staging)?;
    transaction.commit()?;
    Ok(())
}

/// Closes the gaps the dropped records left in the ordering columns, and points the chosen preset
/// at wherever it landed. Nothing else about the settings record changes.
fn renumber_selected(transaction: &Connection, staging: &str) -> StoreResult<()> {
    for (table, identity_columns, order) in [
        ("characters", &["character_id"][..], "configured_index"),
        ("plugin_storage", &["owner", "storage_key"][..], "ordinal"),
    ] {
        let identity = identity_columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let prefixed = identity_columns
            .iter()
            .map(|column| format!("{table}.\"{column}\""))
            .collect::<Vec<_>>()
            .join(", ");
        transaction.execute(
            &format!(
                "UPDATE {table} SET \"{order}\"=(
                     SELECT position-1 FROM (
                         SELECT {identity}, row_number() OVER (ORDER BY \"{order}\") AS position
                         FROM {table} WHERE generation=?1
                     ) ranked WHERE ({identity})=({prefixed})
                 ) WHERE generation=?1",
            ),
            [staging],
        )?;
    }
    // A preset is named by its own position, so renumbering has to carry the identity with it.
    let kept: Vec<String> = {
        let mut statement = transaction.prepare(
            "SELECT preset_id FROM bot_presets WHERE generation=?1 ORDER BY configured_index",
        )?;
        let mut rows = statement.query([staging])?;
        let mut kept = Vec::new();
        while let Some(row) = rows.next()? {
            kept.push(row.get(0)?);
        }
        kept
    };
    let selected: Option<i64> = transaction
        .query_row(
            "SELECT CAST(json_extract(value,'$.botPresetsId') AS INTEGER) FROM root WHERE generation=?1",
            [staging],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    for (position, preset_id) in kept.iter().enumerate() {
        transaction.execute(
            "UPDATE bot_presets SET preset_id=?3, configured_index=?4 WHERE generation=?1 AND preset_id=?2",
            rusqlite::params![staging, preset_id, position.to_string(), position as i64],
        )?;
    }
    if let Some(selected) = selected {
        // The chosen preset keeps being the chosen one where it landed, or the first that stayed.
        let landed = kept
            .iter()
            .position(|preset_id| preset_id.parse::<i64>() == Ok(selected))
            .map(|position| position as i64)
            .unwrap_or(0)
            .min((kept.len() as i64 - 1).max(0));
        if landed != selected {
            transaction.execute(
                "UPDATE root SET value=json_set(value,'$.botPresetsId',?2) WHERE generation=?1",
                rusqlite::params![staging, landed],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn digest_raw_tables(
    source: &Connection,
    probe: &dyn CancellationProbe,
) -> StoreResult<Vec<RawTableDigest>> {
    digest_tables(source, None, probe)
}

fn digest_tables(
    source: &Connection,
    generation: Option<&str>,
    probe: &dyn CancellationProbe,
) -> StoreResult<Vec<RawTableDigest>> {
    let mut result = Vec::with_capacity(TABLES.len());
    for table in TABLES {
        let mut select = source.prepare(&format!(
            "SELECT {} FROM {} {} ORDER BY {}",
            table.column_list(),
            table.name,
            if generation.is_some() {
                "WHERE generation=?1"
            } else {
                ""
            },
            table.order
        ))?;
        let mut rows = select.query(rusqlite::params_from_iter(generation))?;
        let mut digest = table_hasher(table);
        let mut count = 0_u64;
        while let Some(row) = rows.next()? {
            cancelled(probe)?;
            digest.update([0xf0]);
            for index in 0..table.columns.len() {
                hash_value(&mut digest, row.get_ref(index)?);
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| invalid("portable row count overflow"))?;
        }
        digest.update(count.to_le_bytes());
        result.push(RawTableDigest {
            table: table.name,
            rows: count,
            sha256: digest.finalize().into(),
        });
    }
    cancelled(probe)?;
    Ok(result)
}

fn table_hasher(table: &PortableTable) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(b"risunest-portable-sql-v1\0");
    hash.update((table.name.len() as u64).to_le_bytes());
    hash.update(table.name.as_bytes());
    hash.update((table.columns.len() as u64).to_le_bytes());
    hash
}

fn hash_value(hash: &mut Sha256, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hash.update([2]);
            hash.update(value.to_bits().to_le_bytes());
        }
        ValueRef::Text(value) => {
            hash.update([3]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        ValueRef::Blob(value) => {
            hash.update([4]);
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
    }
}

fn validate_live_columns(source: &Connection) -> StoreResult<()> {
    // Operational generation rows are explicitly excluded, never inherited from the live-store
    // cleanup list. A newly introduced family must be classified before claiming full capture.
    let mut tables = source.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let mut names = tables.query([])?;
    while let Some(row) = names.next()? {
        let name: String = row.get(0)?;
        let generation: bool = source.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name='generation')",
            [&name],
            |r| r.get(0),
        )?;
        if generation
            && !TABLES.iter().any(|t| t.name == name)
            && !matches!(
                name.as_str(),
                "asset_alias_replacement_candidates"
                    | "snapshot_leases"
                    | "server_sync_context"
                    | "content_change_context"
                    | "content_changes"
                    | "content_change_consumers"
                    | "content_change_floor"
            )
        {
            return Err(invalid(&format!(
                "unclassified generation-scoped source table: {name}"
            )));
        }
    }
    for table in TABLES {
        let mut statement = source.prepare(&format!("PRAGMA table_info({})", table.name))?;
        let actual = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let expected = std::iter::once(("generation", "TEXT"))
            .chain(table.columns.iter().copied())
            .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
            .collect::<Vec<_>>();
        if actual != expected {
            return Err(invalid(
                "portable source columns differ from the reviewed schema",
            ));
        }
    }
    Ok(())
}

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    struct Never;
    impl CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }

    #[test]
    fn portable_capture_reads_the_leased_revision_after_a_live_commit() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let stage = store.replace_begin().unwrap();
        store
            .replace_put_root(&stage.staging_id, &serde_json::json!({"marker":"before"}))
            .unwrap();
        store.replace_commit(&stage.staging_id, Some(0)).unwrap();
        let lease = store.acquire_revision(1).unwrap().lease;
        let next = store.replace_begin().unwrap();
        store
            .replace_put_root(&next.staging_id, &serde_json::json!({"marker":"after"}))
            .unwrap();
        store.replace_commit(&next.staging_id, Some(1)).unwrap();
        let mut output = Connection::open_in_memory().unwrap();
        create_raw_tables(&output).unwrap();
        let captured = store
            .capture_portable_records(&lease, &mut output, &Never)
            .unwrap();
        assert_eq!(captured, digest_raw_tables(&output, &Never).unwrap());
        assert_eq!(
            output
                .query_row("SELECT value FROM root", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "{\"marker\":\"before\"}"
        );
        store.release_revision(&lease).unwrap();
    }

    #[test]
    fn raw_capture_retains_orphans_bad_json_types_and_exact_text() {
        let mut source = Connection::open_in_memory().unwrap();
        super::super::schema::initialize(&mut source).unwrap();
        source
            .execute(
                "INSERT INTO root VALUES('chosen',?1)",
                ["{ \"z\":null, \"a\":{}, \"empty\":\"\" }"],
            )
            .unwrap();
        source
            .execute("INSERT INTO root VALUES('other','{}')", [])
            .unwrap();
        source
            .execute(
                "INSERT INTO conversations VALUES('chosen','orphan','chat',7,0,'',1,?1)",
                ["not JSON"],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO messages VALUES('chosen','orphan','chat',9,NULL,?1)",
                ["{\"data\":\"合成\"}"],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO plugin_storage VALUES('chosen','synthetic-plugin','blob',3,8,?1,NULL,NULL,NULL)",
                params![vec![0_u8, 0xff, 0x20]],
            )
            .unwrap();
        source
            .execute(
                "INSERT INTO plugin_storage VALUES('chosen','synthetic-plugin','bad-text',1,9,CAST(x'ff' AS TEXT),NULL,NULL,NULL)",
                [],
            )
            .unwrap();
        let mut output = Connection::open_in_memory().unwrap();
        create_raw_tables(&output).unwrap();
        let copied = copy_generation(&source, "chosen", &mut output, &Never).unwrap();
        assert_eq!(copied, digest_raw_tables(&output, &Never).unwrap());
        assert_eq!(
            output
                .query_row("SELECT count(*) FROM root", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            output
                .query_row("SELECT detail FROM conversations", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "not JSON"
        );
        assert_eq!(
            output
                .query_row("SELECT value FROM root", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "{ \"z\":null, \"a\":{}, \"empty\":\"\" }"
        );
        assert_eq!(output.query_row("SELECT typeof(value), hex(value) FROM plugin_storage WHERE storage_key='bad-text'", [], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?))).unwrap(), ("text".into(), "FF".into()));
        assert_eq!(
            output
                .query_row(
                    "SELECT typeof(value) FROM plugin_storage WHERE storage_key='blob'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "blob"
        );
        assert_eq!(
            output
                .query_row("SELECT message_id FROM messages", [], |r| r
                    .get::<_, Option<String>>(0))
                .unwrap(),
            None
        );
        assert_eq!(
            source
                .query_row("SELECT count(*) FROM root", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn unknown_source_column_is_not_silently_omitted() {
        let mut source = Connection::open_in_memory().unwrap();
        super::super::schema::initialize(&mut source).unwrap();
        source
            .execute_batch("ALTER TABLE root ADD COLUMN new_data TEXT")
            .unwrap();
        assert!(validate_live_columns(&source).is_err());
    }

    #[test]
    fn digest_distinguishes_sql_classes_null_empty_and_boundaries() {
        fn digest(value: ValueRef<'_>) -> Vec<u8> {
            let mut h = Sha256::new();
            hash_value(&mut h, value);
            h.finalize().to_vec()
        }
        assert_ne!(
            digest(ValueRef::Text(b"same")),
            digest(ValueRef::Blob(b"same"))
        );
        assert_ne!(digest(ValueRef::Null), digest(ValueRef::Text(b"")));
        assert_ne!(digest(ValueRef::Integer(1)), digest(ValueRef::Real(1.0)));
    }

    #[test]
    fn cancellation_rolls_back_partial_tables() {
        use std::cell::Cell;
        struct CancelAfter(Cell<u32>);
        impl CancellationProbe for CancelAfter {
            fn is_cancelled(&self) -> bool {
                self.0.set(self.0.get() + 1);
                self.0.get() > 2
            }
        }
        let mut source = Connection::open_in_memory().unwrap();
        super::super::schema::initialize(&mut source).unwrap();
        source
            .execute("INSERT INTO root VALUES('chosen','{}')", [])
            .unwrap();
        let mut output = Connection::open_in_memory().unwrap();
        create_raw_tables(&output).unwrap();
        assert!(
            copy_generation(&source, "chosen", &mut output, &CancelAfter(Cell::new(0))).is_err()
        );
        assert_eq!(
            output
                .query_row("SELECT count(*) FROM root", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
