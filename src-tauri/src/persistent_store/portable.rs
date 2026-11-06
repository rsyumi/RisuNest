//! Raw generation projection for portable archives. Never execute source DDL.
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

pub(crate) const PRESENCE_BATCH: usize = 1024;

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
            let sanitized_root = if table.name == "root" {
                match row.get_ref(0)? {
                    ValueRef::Text(bytes) => serde_json::from_slice::<serde_json::Value>(bytes).ok().and_then(|mut value| {
                        value.as_object_mut()?.remove("account")?;
                        serde_json::to_string(&value).ok()
                    }),
                    _ => None,
                }
            } else { None };
            for index in 0..table.columns.len() {
                let value = match sanitized_root.as_ref() {
                    Some(root) if index == 0 => ValueRef::Text(root.as_bytes()),
                    _ => row.get_ref(index)?,
                };
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
    pub(crate) fn portable_staged_counts(&self, staging: &str) -> StoreResult<(u64, u64)> {
        Ok((
            self.connection.query_row("SELECT count(*) FROM characters WHERE generation=?1", [staging], |row| row.get::<_, i64>(0))? as u64,
            self.connection.query_row("SELECT count(*) FROM bot_presets WHERE generation=?1", [staging], |row| row.get::<_, i64>(0))? as u64,
        ))
    }
    pub(crate) fn portable_source_generation(&self, lease: &str) -> StoreResult<String> {
        let (_, target) = self.read_view(Some(lease))?;
        Ok(target.generation.clone())
    }
    pub(crate) fn portable_export_lower_bound(&self) -> StoreResult<u64> {
        let objects: i64 = self.connection.query_row("SELECT coalesce(sum(byte_size),0) FROM asset_objects", [], |row| row.get(0))?;
        let database = super::snapshot::active_database_bytes(&self.connection)?;
        u64::try_from(objects).ok().and_then(|objects| database.checked_add(objects))
            .ok_or_else(|| invalid("portable export size overflow"))
    }

    pub(crate) fn capture_portable_units(&self, lease: &str, destination: &Connection, probe: &dyn CancellationProbe) -> StoreResult<super::external_capture::BackupDependencyInventory> {
        use super::external_capture::BackupBodyRole;
        let units = self.lww_backup_unit_values(lease)?;
        destination.execute_batch("CREATE TEMP TABLE backup_payload_spool(hash TEXT PRIMARY KEY,body BLOB NOT NULL)")?;
        let inventory = self.lww_backup_dependency_inventory(lease, &units, probe, true, &mut |hash,body,role| {
            cancelled(probe)?;
            match role {
                BackupBodyRole::Control=>{destination.execute("INSERT INTO backup_controls VALUES(?1,?2)",rusqlite::params![hash,body])?;},
                BackupBodyRole::Payload=>{destination.execute("INSERT INTO backup_payload_spool VALUES(?1,?2)",rusqlite::params![hash,body])?;},
            }
            Ok(())
        })?;
        for (key, value) in units {
            cancelled(probe)?;
            destination.execute("INSERT INTO backup_units VALUES(?1,?2)",rusqlite::params![String::from(key),serde_json::to_string(&value)?])?;
        }
        for (hash, size) in &inventory.payloads {
            let size=size.ok_or_else(||invalid("original payload size is unavailable"))?;
            destination.execute("INSERT INTO backup_payloads VALUES(?1,?2,?3)",rusqlite::params![hash,i64::try_from(size).map_err(|_|invalid("original payload size exceeds SQLite limit"))?,inventory.record_payloads.contains(hash)])?;
        }
        Ok(inventory)
    }

    /// Stages the original units of a portable backup as the source units of
    /// `staging_id` and verifies the controls and payloads they name.
    pub(crate) fn stage_portable_units(&mut self, staging_id: &str, source: &Connection, probe: &dyn CancellationProbe) -> StoreResult<()> {
        let mut large = std::collections::BTreeSet::new();
        let mut manifests = Vec::new();
        {
            let writer = self.replacement_source_writer(staging_id)?;
            let mut statement = source.prepare("SELECT key,value FROM backup_units ORDER BY key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                cancelled(probe)?;
                let key: risunest_sync_wire::unit::UnitKey = row.get::<_,String>(0)?.try_into().map_err(|_|invalid("invalid original unit key"))?;
                let value: risunest_sync_wire::unit::UnitValue = serde_json::from_str(&row.get::<_,String>(1)?)?;
                value.validate().map_err(|e| invalid(&e.to_string()))?;
                if super::lww::lww_known_unit_key(&key) {
                    if let risunest_sync_wire::unit::UnitValue::Object { descriptor, .. } = &value {
                        large.insert(descriptor.object_hash.clone());
                    }
                }
                manifests.extend(super::external_capture::message_manifest_hash(&key,&value));
                writer.put(&key,&value)?;
            }
            writer.finish()?;
        }
        let temporary=tempfile::tempdir_in(&self.snapshots_dir)?;
        let certified=crate::sqlite_open::open(temporary.path().join("certified.sqlite"))?;
        // A scratch table that is dropped with its directory, so no commit needs to be durable.
        certified.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-16384; PRAGMA temp_store=FILE;
            CREATE TABLE controls(hash TEXT PRIMARY KEY,body BLOB NOT NULL); BEGIN")?;
        let read_manifest = |hash: &str| -> StoreResult<Option<Vec<u8>>> {
            Ok(source.query_row("SELECT body FROM backup_controls WHERE hash=?1",[hash],|row|row.get(0)).optional()?)
        };
        let pages = super::external_capture::OriginalMessagePages::from_manifests(manifests,&read_manifest);
        // A control above the bound is read only as a known unit body or as a
        // page whose manifest declares that length.
        let read = |hash: &str| -> StoreResult<Option<Vec<u8>>> {
            let row: Option<(i64,Option<Vec<u8>>)> = source.query_row("SELECT length(body),CASE WHEN length(body)<=?2 THEN body END FROM backup_controls WHERE hash=?1",
                rusqlite::params![hash,risunest_sync_wire::MAX_METADATA_BYTES as i64],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            let Some((length,body)) = row else {return Ok(None)};
            if body.is_some() {return Ok(body)}
            let length = u64::try_from(length).map_err(|_|invalid("invalid original control length"))?;
            if !large.contains(hash)
                && pages.page(hash)?.is_none_or(|page|page.byte_length.0 != length) {
                return Ok(None);
            }
            Ok(source.query_row("SELECT body FROM backup_controls WHERE hash=?1",[hash],|row|row.get(0)).optional()?)
        };
        let inventory = super::external_capture::streamed_unit_dependency_inventory(self.replacement_source_units(staging_id),
            &read,
            &|hash| {
                let size:Option<i64>=source.query_row("SELECT byte_length FROM backup_payloads WHERE hash=?1",[hash],|row|row.get(0)).optional()?;
                size.map(|size|u64::try_from(size).map_err(|_|invalid("invalid original payload size"))).transpose()
            },probe,true,&mut |hash,body,role| {
                if role!=super::external_capture::BackupBodyRole::Control {return Err(invalid("payload body is misclassified as an original control"));}
                certified.execute("INSERT INTO controls VALUES(?1,?2)",rusqlite::params![hash,body])?;
                Ok(())
            })?;
        certified.execute_batch("COMMIT")?;
        let controls:i64=source.query_row("SELECT COUNT(*) FROM backup_controls",[],|row|row.get(0))?;
        if usize::try_from(controls).ok()!=Some(inventory.controls.len()) {return Err(invalid("unreferenced original control"));}
        for hash in inventory.controls.keys() {
            let present:bool=source.query_row("SELECT EXISTS(SELECT 1 FROM backup_controls WHERE hash=?1)",[hash],|row|row.get(0))?;
            if !present {return Err(invalid("missing original control"));}
        }
        let payloads:i64=source.query_row("SELECT COUNT(*) FROM backup_payloads",[],|row|row.get(0))?;
        if usize::try_from(payloads).ok()!=Some(inventory.payloads.len()) {return Err(invalid("unreferenced original payload"));}
        for (hash,size) in &inventory.payloads {
            let row:Option<(i64,bool)>=source.query_row("SELECT byte_length,record FROM backup_payloads WHERE hash=?1",[hash],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            let expected_size=size.ok_or_else(||invalid("original payload size is unavailable"))?;
            if row!=Some((i64::try_from(expected_size).map_err(|_|invalid("original payload size exceeds SQLite limit"))?,inventory.record_payloads.contains(hash))) {return Err(invalid("original payload metadata differs"));}
        }
        let mut statement=certified.prepare("SELECT hash,body FROM controls ORDER BY hash")?;
        let mut rows=statement.query([])?;
        while let Some(row)=rows.next()? {
            cancelled(probe)?;
            super::message_pages::put_object(&self.connection,&row.get::<_,String>(0)?,&row.get::<_,Vec<u8>>(1)?)?;
        }
        for unit in self.replacement_source_units(staging_id) {
            let (key,value) = unit?;
            super::lww::validate_received(&self.connection,&key,&value)?;
        }
        Ok(())
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

    pub(crate) fn portable_object_present(&self, hash: &str, expected_size:u64) -> StoreResult<bool> {
        risunest_sync_wire::validate_hash(hash).map_err(|e| super::StoreError::Validation { message: e.to_string() })?;
        match crate::asset_repository::PayloadCas::new(&self.repository_root)?.stat_object(hash)? {
            None=>Ok(false),
            Some(size) if size==expected_size=>Ok(true),
            Some(_)=>Err(invalid("existing portable payload size mismatch")),
        }
    }

    /// Local body sizes for many objects, validating the repository once per batch rather than
    /// once per object.
    pub(crate) fn portable_object_sizes(&self, hashes: &[&str]) -> StoreResult<Vec<Option<u64>>> {
        let scan = crate::asset_repository::PayloadCas::new(&self.repository_root)?.into_read_scan();
        let mut sizes = Vec::with_capacity(hashes.len());
        for batch in hashes.chunks(PRESENCE_BATCH) {
            match scan.stat_objects(batch.iter().copied()) {
                Ok(batch) => sizes.extend(batch),
                // A write that creates a shard during the batch changes the scanned hierarchy;
                // the per-object check gives the same answer without that requirement.
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                    let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
                    for hash in batch {
                        sizes.push(cas.stat_object(hash)?);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(sizes)
    }

    pub(crate) fn portable_objects_present(&self, objects: &[(&str, u64)]) -> StoreResult<Vec<bool>> {
        let hashes = objects.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        self.portable_object_sizes(&hashes)?
            .into_iter()
            .zip(objects)
            .map(|(size, (_, expected))| match size {
                None => Ok(false),
                Some(size) if size == *expected => Ok(true),
                Some(_) => Err(invalid("existing portable payload size mismatch")),
            })
            .collect()
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
                let mut message_owner: Option<(String, String)> = None;
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
                    if table.name == "messages" {
                        let owner = (row.get::<_, String>(0)?, row.get::<_, String>(1)?);
                        if message_owner.as_ref().is_some_and(|previous| previous != &owner) {
                            let (character, conversation) = message_owner.take().unwrap();
                            super::message_pages::stage_conversation_pages(
                                &transaction, &stage.staging_id, &character, &conversation, &|| cancelled(probe),
                            )?;
                        }
                        let index: i64 = row.get(2)?;
                        if index % 128 == 127 {
                            super::message_pages::stage_conversation_pages(
                                &transaction, &stage.staging_id, &owner.0, &owner.1, &|| cancelled(probe),
                            )?;
                        }
                        message_owner = Some(owner);
                    }
                }
                if let Some((character, conversation)) = message_owner {
                    super::message_pages::stage_conversation_pages(
                        &transaction, &stage.staging_id, &character, &conversation, &|| cancelled(probe),
                    )?;
                }
            }
            if digest_tables(&transaction, Some(&stage.staging_id), probe)? != expected {
                return Err(invalid("portable staging changed raw SQL values"));
            }
            // A library saved before activation gave every embedded module an id may hold one
            // without it, and every later root save names that module's owner by its id.
            {
                let mut roots = transaction.prepare(
                    "SELECT rowid,value FROM root WHERE generation=?1 AND typeof(value)='text' AND json_valid(value)",
                )?;
                let roots = roots
                    .query_map([&stage.staging_id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                for (rowid, raw) in roots {
                    let mut value: serde_json::Value = serde_json::from_str(&raw)?;
                    if value.as_object_mut().is_some_and(super::record_projection::assign_embedded_module_ids) {
                        transaction.execute(
                            "UPDATE root SET value=?1 WHERE rowid=?2",
                            rusqlite::params![serde_json::to_string(&value)?, rowid],
                        )?;
                    }
                }
            }
            {
                let mut conversations = transaction
                    .prepare("SELECT character_id,conversation_id FROM conversations WHERE generation=?1 AND message_count=0")?;
                let conversations = conversations.query_map([&stage.staging_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
                for conversation in conversations {
                    let (character, conversation) = conversation?;
                    super::message_pages::stage_conversation_pages(
                        &transaction, &stage.staging_id, &character, &conversation, &|| cancelled(probe),
                    )?;
                }
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
                    | "snapshot_original_meta"
                    | "snapshot_leases"
                    | "content_change_context"
                    | "content_changes"
                    | "content_change_consumers"
                    | "content_change_floor"
                    | "message_page_indexes"
                    | "message_page_manifests"
                    | "replacement_source_units"
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
        let mut expected = std::iter::once(("generation", "TEXT"))
            .chain(table.columns.iter().copied())
            .map(|(name, kind)| (name.to_owned(), kind.to_owned()))
            .collect::<Vec<_>>();
        if table.name=="messages" {
            expected.extend([("canonical_hash".to_owned(),"TEXT".to_owned()),("canonical_size".to_owned(),"INTEGER".to_owned())]);
        }
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
    fn portable_presence_checks_physical_metadata_without_hashing_present_bodies() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let bytes=b"synthetic physically present payload";
        let hash=risunest_sync_wire::hash(bytes);
        store.asset_object_catalog().register(&[super::super::asset_object_catalog::AssetObjectRegistration {object_hash:hash.clone(),byte_size:bytes.len() as u64}],0).unwrap();
        assert!(!store.portable_object_present(&hash,bytes.len() as u64).unwrap());
        let cas=crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        cas.prepare_bytes(bytes).unwrap();
        crate::asset_repository::body_io::reset_body_io();
        assert!(store.portable_object_present(&hash,bytes.len() as u64).unwrap());
        let observed=crate::asset_repository::body_io::take_body_io();
        assert!(observed.complete());
        assert_eq!(observed.asset_work().opens,0);
        assert_eq!(observed.asset_work().read_bytes,0);
        assert!(observed.asset_work().body_sha.values().all(|work|work.bytes==0));
        assert!(store.portable_object_present(&hash,bytes.len() as u64+1).is_err());
    }

    #[test]
    fn portable_capture_excludes_root_account_but_keeps_other_values() {
        let source = rusqlite::Connection::open_in_memory().unwrap();
        source.execute_batch("CREATE TABLE root(generation TEXT,value TEXT)").unwrap();
        source.execute("INSERT INTO root VALUES('chosen',?1)", [r#"{"account":{"token":"synthetic-secret"},"username":"Synthetic"}"#]).unwrap();
        for table in TABLES.iter().filter(|table| table.name != "root") {
            let columns = table.columns.iter().map(|(name, kind)| format!("{name} {kind}")).collect::<Vec<_>>().join(",");
            source.execute_batch(&format!("CREATE TABLE {} (generation TEXT,{columns})", table.name)).unwrap();
        }
        source.execute_batch("ALTER TABLE messages ADD COLUMN canonical_hash TEXT; ALTER TABLE messages ADD COLUMN canonical_size INTEGER").unwrap();
        let mut output = rusqlite::Connection::open_in_memory().unwrap();
        create_raw_tables(&output).unwrap();
        copy_generation(&source, "chosen", &mut output, &Never).unwrap();
        let encoded: String = output.query_row("SELECT value FROM root", [], |row| row.get(0)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value, serde_json::json!({"username":"Synthetic"}));
        let original: String = source.query_row("SELECT value FROM root", [], |row| row.get(0)).unwrap();
        assert!(original.contains("synthetic-secret"));
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
                "INSERT INTO messages VALUES('chosen','orphan','chat',9,NULL,?1,'',0)",
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
    fn portable_large_controls_still_require_integrity_and_exact_references() {
        use super::super::external_capture::{large_metadata_fixture, original_unit_dependency_closure};
        let (units, bodies, large) = large_metadata_fixture();
        let closure = original_unit_dependency_closure(&units, &|hash| Ok(bodies.get(hash).cloned()), &|_| Ok(Some(1)), &Never).unwrap();
        for tampered in [true, false] {
            let source = Connection::open_in_memory().unwrap();
            source.execute_batch("CREATE TABLE backup_units(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE backup_controls(hash TEXT PRIMARY KEY,body BLOB); CREATE TABLE backup_payloads(hash TEXT PRIMARY KEY,byte_length INTEGER,record INTEGER)").unwrap();
            for (key, value) in &units {
                source.execute("INSERT INTO backup_units VALUES(?1,?2)", rusqlite::params![String::from(key.clone()), serde_json::to_string(value).unwrap()]).unwrap();
            }
            for (hash, body) in &closure.controls {
                let mut body = body.clone();
                if tampered && hash == &large[0] { body.push(b' '); }
                source.execute("INSERT INTO backup_controls VALUES(?1,?2)", rusqlite::params![hash, body]).unwrap();
            }
            for (hash, size) in &closure.managed_payloads {
                source.execute("INSERT INTO backup_payloads VALUES(?1,?2,?3)", rusqlite::params![hash, *size as i64, closure.record_payloads.contains(hash)]).unwrap();
            }
            if !tampered {
                let body = b"unreferenced synthetic control";
                source.execute("INSERT INTO backup_controls VALUES(?1,?2)", rusqlite::params![risunest_sync_wire::hash(body), body.as_slice()]).unwrap();
            }
            let directory = tempfile::tempdir().unwrap();
            let mut target = PersistentStore::open(directory.path()).unwrap();
            let stage = target.replace_begin().unwrap();
            let failure = target.stage_portable_units(&stage.staging_id, &source, &Never).unwrap_err().to_string();
            let expected = if tampered { "Backup source body integrity failed" } else { "unreferenced original control" };
            assert!(failure.contains(expected), "{failure}");
            assert_eq!(target.revision().unwrap(), 0);
        }
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
