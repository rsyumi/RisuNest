//! Section rows as a remote adapter sees them. Publication reads rows here and
//! reception merges them back, so one rule decides every value no matter which
//! remote carried it.
use super::{invalid, observe_remote_clock, sequence, DeviceStore, Section};
use crate::persistent_store::StoreResult;
use risunest_sync_wire::Sequence;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};

/// Device settings a restored installation wants back, as opposed to the
/// coordination state it must never inherit from another run.
pub(crate) const LOCAL_SETTING_KEYS: [&str; 9] = [
    "accountst",
    "dosync",
    "hub",
    "ignoreRisuAuth",
    "nightlyWarned",
    "risuNestDeviceSettings",
    "risuNestUpdateSettings",
    "risu_service_tos_v1",
    "risunest_tos_v1",
];

/// Every section value a device can publish, plus the removal of one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SectionValueRow {
    Hypa {
        producer: String,
        model: String,
        endpoint: Option<String>,
        preprocess_version: i64,
        dimensions: i64,
        vector: Vec<u8>,
        metadata: Option<String>,
    },
    Plugin {
        space: String,
        value: String,
    },
    Setting {
        value: String,
    },
    PluginPermission {
        granted: bool,
    },
    Tombstone,
}

/// The key triple matches the change index, so a row and its change entry name
/// the same thing without a second encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionRow {
    pub key1: String,
    pub key2: String,
    pub key3: String,
    pub value: SectionValueRow,
    pub write_clock: Sequence,
    pub writer_id: String,
}

impl SectionRow {
    pub(crate) fn key(&self) -> (String, String, String) {
        (self.key1.clone(), self.key2.clone(), self.key3.clone())
    }
    fn version_after(&self, other: &Self) -> bool {
        (&self.write_clock, &self.writer_id) > (&other.write_clock, &other.writer_id)
    }
    fn same_version(&self, other: &Self) -> bool {
        self.write_clock == other.write_clock && self.writer_id == other.writer_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionState {
    pub participating: bool,
    pub max_write_clock: Sequence,
    pub gc_floor: Sequence,
    pub participation_generation: Sequence,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SectionApplyOutcome {
    pub applied: usize,
    pub kept: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SectionCursor {
    pub applied_generation: Sequence,
    pub applied_gc_floor: Sequence,
    pub observed_max_write_clock: Sequence,
}

fn setting_is_local(key: &str) -> bool {
    LOCAL_SETTING_KEYS.contains(&key)
}

/// Keys no remote has been told about yet. A restore uses them to tell its own
/// interrupted attempt apart from a value some remote already carries.
fn unpublished_keys(
    tx: &Transaction<'_>,
    section: Section,
) -> StoreResult<BTreeSet<(String, String, String)>> {
    let mut keys = BTreeSet::new();
    match section {
        Section::Hypa => {
            let mut statement = tx
                .prepare("SELECT cache_key FROM hypa_embeddings WHERE published_clock IS NULL")?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, String::new(), String::new()));
            }
        }
        Section::LocalPlugins => {
            let mut statement = tx.prepare(
                "SELECT owner,space,key FROM plugin_device_storage WHERE published_clock IS NULL",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, row.get(1)?, row.get(2)?));
            }
        }
    }
    Ok(keys)
}

fn read_rows(tx: &Transaction<'_>, section: Section) -> StoreResult<Vec<SectionRow>> {
    let mut rows = Vec::new();
    match section {
        Section::Hypa => {
            let mut statement = tx.prepare(
                "SELECT cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                        metadata,tombstone,write_clock,writer_id
                    FROM hypa_embeddings ORDER BY cache_key",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let tombstone: i64 = row.get(8)?;
                let vector: Option<Vec<u8>> = row.get(6)?;
                let value = if tombstone == 1 || vector.is_none() {
                    SectionValueRow::Tombstone
                } else {
                    SectionValueRow::Hypa {
                        producer: row.get(1)?,
                        model: row.get(2)?,
                        endpoint: row.get(3)?,
                        preprocess_version: row.get(4)?,
                        dimensions: row.get(5)?,
                        vector: vector.expect("checked vector"),
                        metadata: row.get(7)?,
                    }
                };
                rows.push(SectionRow {
                    key1: row.get(0)?,
                    key2: String::new(),
                    key3: String::new(),
                    value,
                    write_clock: sequence(&row.get::<_, String>(9)?)?,
                    writer_id: row.get(10)?,
                });
            }
        }
        Section::LocalPlugins => {
            let mut statement = tx.prepare(
                "SELECT owner,space,key,value,tombstone,write_clock,writer_id
                    FROM plugin_device_storage ORDER BY owner,space,key",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let tombstone: i64 = row.get(4)?;
                let stored: Option<String> = row.get(3)?;
                let value = match (tombstone, stored) {
                    (0, Some(value)) => SectionValueRow::Plugin {
                        space: row.get(1)?,
                        value,
                    },
                    _ => SectionValueRow::Tombstone,
                };
                rows.push(SectionRow {
                    key1: row.get(0)?,
                    key2: row.get(1)?,
                    key3: row.get(2)?,
                    value,
                    write_clock: sequence(&row.get::<_, String>(5)?)?,
                    writer_id: row.get(6)?,
                });
            }
        }
    }
    Ok(rows)
}

impl DeviceStore {
    pub(crate) fn section_state(&self, section: Section) -> StoreResult<SectionState> {
        let (participating, max_write_clock, gc_floor, participation_generation): (
            i64,
            String,
            String,
            String,
        ) = self.connection.query_row(
            "SELECT participating,max_write_clock,gc_floor,participation_generation
                FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(SectionState {
            participating: participating == 1,
            max_write_clock: sequence(&max_write_clock)?,
            gc_floor: sequence(&gc_floor)?,
            participation_generation: sequence(&participation_generation)?,
        })
    }

    /// Turning a section on or off changes what this device exchanges from the
    /// next publication onward. The stored values are left alone either way.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn set_section_participating(
        &mut self,
        section: Section,
        participating: bool,
    ) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let current: i64 = transaction.query_row(
            "SELECT participating FROM device_sections WHERE section=?1",
            [section.as_str()],
            |row| row.get(0),
        )?;
        if (current == 1) != participating {
            let generation = sequence(&transaction.query_row(
                "SELECT participation_generation FROM device_sections WHERE section=?1",
                [section.as_str()],
                |row| row.get::<_, String>(0),
            )?)?
            .next()
            .map_err(|_| invalid("device participation generation is exhausted"))?;
            transaction.execute(
                "UPDATE device_sections SET participating=?1,participation_generation=?2
                    WHERE section=?3",
                params![
                    i64::from(participating),
                    generation.as_str(),
                    section.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Every row of a synchronized section, tombstones included. A removal only
    /// travels while its tombstone does.
    pub(crate) fn read_section_rows(&mut self, section: Section) -> StoreResult<Vec<SectionRow>> {
        let transaction = self.transaction()?;
        let rows = read_rows(&transaction, section)?;
        transaction.commit()?;
        Ok(rows)
    }

    /// The values a backup keeps for the same device. Control rows stay behind,
    /// and a removed value is simply absent rather than carried as a removal.
    pub(crate) fn read_backup_section_rows(
        &mut self,
        section: Section,
    ) -> StoreResult<Vec<SectionRow>> {
        Ok(self
            .read_section_rows(section)?
            .into_iter()
            .filter(|row| row.value != SectionValueRow::Tombstone)
            .collect())
    }

    /// Device-fixed values. They are backup material only and never reach a
    /// synchronized state.
    pub(crate) fn read_local_setting_rows(&mut self) -> StoreResult<Vec<SectionRow>> {
        let transaction = self.transaction()?;
        let mut rows = Vec::new();
        {
            let mut statement = transaction
                .prepare("SELECT key,value FROM device_settings ORDER BY key")?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let key: String = row.get(0)?;
                if !setting_is_local(&key) {
                    continue;
                }
                rows.push(SectionRow {
                    key1: "setting".into(),
                    key2: key,
                    key3: String::new(),
                    value: SectionValueRow::Setting { value: row.get(1)? },
                    write_clock: Sequence::from(0u64),
                    writer_id: String::new(),
                });
            }
            let mut statement = transaction.prepare(
                "SELECT code_hash,permission,granted FROM plugin_permissions
                    ORDER BY code_hash,permission",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                rows.push(SectionRow {
                    key1: "pluginPermission".into(),
                    key2: row.get(0)?,
                    key3: row.get(1)?,
                    value: SectionValueRow::PluginPermission {
                        granted: row.get::<_, i64>(2)? == 1,
                    },
                    write_clock: Sequence::from(0u64),
                    writer_id: String::new(),
                });
            }
        }
        transaction.commit()?;
        Ok(rows)
    }

    /// Whether a participating section holds a write this remote lineage has
    /// not seen. A device value can change without the library changing, so
    /// this is what makes a section-only edit reach the remote at all.
    pub(crate) fn sections_await_publication(
        &self,
        connection_id: &str,
        library_lineage: &str,
    ) -> StoreResult<bool> {
        for section in [Section::Hypa, Section::LocalPlugins] {
            let state = self.section_state(section)?;
            if !state.participating {
                continue;
            }
            match self.read_section_cursor(connection_id, library_lineage, section)? {
                Some(cursor) => {
                    if state.max_write_clock > cursor.observed_max_write_clock {
                        return Ok(true);
                    }
                }
                None => {
                    if state.max_write_clock > Sequence::from(0u64) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Merges a received section. The higher `(write_clock, writer_id)` wins,
    /// the same version with different content is refused, and a received row
    /// keeps the version it arrived with instead of becoming a local write.
    pub(crate) fn apply_section_rows(
        &mut self,
        section: Section,
        rows: &[SectionRow],
    ) -> StoreResult<SectionApplyOutcome> {
        let transaction = self.transaction()?;
        let mut outcome = SectionApplyOutcome::default();
        let local: BTreeMap<(String, String, String), SectionRow> = read_rows(&transaction, section)?
            .into_iter()
            .map(|row| (row.key(), row))
            .collect();
        let mut highest = Sequence::from(0u64);
        let mut applied = Vec::new();
        for row in rows {
            if row.writer_id.is_empty() {
                return Err(invalid("received section row has no writer"));
            }
            if row.write_clock > highest {
                highest = row.write_clock.clone();
            }
            match local.get(&row.key()) {
                Some(current) if current.same_version(row) => {
                    if current.value != row.value {
                        return Err(invalid(
                            "received section row differs at the same version",
                        ));
                    }
                    outcome.kept += 1;
                }
                Some(current) if !row.version_after(current) => outcome.kept += 1,
                _ => applied.push(row),
            }
        }
        // Received rows are staged outside a mutation context so the change
        // index does not offer them back as this device's own writes.
        for row in &applied {
            write_row(&transaction, section, row, true)?;
            outcome.applied += 1;
        }
        observe_remote_clock(&transaction, section, &highest)?;
        transaction.commit()?;
        Ok(outcome)
    }

    pub(crate) fn read_section_cursor(
        &self,
        connection_id: &str,
        library_lineage: &str,
        section: Section,
    ) -> StoreResult<Option<SectionCursor>> {
        let stored: Option<(String, String, String)> = self
            .connection
            .query_row(
                "SELECT applied_generation,applied_gc_floor,observed_max_write_clock
                    FROM device_remote_cursors
                    WHERE connection_id=?1 AND library_lineage=?2 AND section=?3",
                params![connection_id, library_lineage, section.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        stored
            .map(|(generation, floor, observed)| {
                Ok(SectionCursor {
                    applied_generation: sequence(&generation)?,
                    applied_gc_floor: sequence(&floor)?,
                    observed_max_write_clock: sequence(&observed)?,
                })
            })
            .transpose()
    }

    /// The applied point of one section on one remote lineage. A cursor never
    /// moves backwards, so a replayed apply cannot lose ground.
    pub(crate) fn write_section_cursor(
        &mut self,
        connection_id: &str,
        library_lineage: &str,
        section: Section,
        cursor: &SectionCursor,
    ) -> StoreResult<()> {
        if connection_id.is_empty() || library_lineage.is_empty() {
            return Err(invalid("section cursor identity is missing"));
        }
        let existing = self.read_section_cursor(connection_id, library_lineage, section)?;
        let merged = match existing {
            Some(current) => SectionCursor {
                applied_generation: cursor
                    .applied_generation
                    .clone()
                    .max(current.applied_generation),
                applied_gc_floor: cursor.applied_gc_floor.clone().max(current.applied_gc_floor),
                observed_max_write_clock: cursor
                    .observed_max_write_clock
                    .clone()
                    .max(current.observed_max_write_clock),
            },
            None => cursor.clone(),
        };
        self.connection.execute(
            "INSERT INTO device_remote_cursors
                (connection_id,library_lineage,section,applied_generation,applied_gc_floor,
                 observed_max_write_clock)
                VALUES (?1,?2,?3,?4,?5,?6)
                ON CONFLICT(connection_id,library_lineage,section) DO UPDATE SET
                    applied_generation=excluded.applied_generation,
                    applied_gc_floor=excluded.applied_gc_floor,
                    observed_max_write_clock=excluded.observed_max_write_clock",
            params![
                connection_id,
                library_lineage,
                section.as_str(),
                merged.applied_generation.as_str(),
                merged.applied_gc_floor.as_str(),
                merged.observed_max_write_clock.as_str()
            ],
        )?;
        Ok(())
    }

    /// Installs backup material for the same device. A restored value is this
    /// device's own write, so it takes a freshly issued clock and this writer
    /// rather than whatever produced the bundle. The section is replaced: a key
    /// the material leaves out is removed, so restoring an empty section empties
    /// it. A value this device already holds unpublished under its own writer is
    /// left alone, which keeps a retried restore from issuing a second clock for
    /// something it already wrote.
    pub(crate) fn restore_section_rows(
        &mut self,
        section: Section,
        rows: &[SectionRow],
    ) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let writer_id: String = transaction.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let restored: BTreeSet<(String, String, String)> =
            rows.iter().map(SectionRow::key).collect();
        if restored.len() != rows.len() {
            return Err(invalid("restored section rows repeat a key"));
        }
        let unpublished = unpublished_keys(&transaction, section)?;
        let held: BTreeMap<(String, String, String), SectionRow> = read_rows(&transaction, section)?
            .into_iter()
            .map(|row| (row.key(), row))
            .collect();
        super::begin_mutation(&transaction)?;
        for row in rows {
            if row.value == SectionValueRow::Tombstone {
                return Err(invalid("restored section row has no value"));
            }
            let settled = held.get(&row.key()).is_some_and(|current| {
                current.value == row.value
                    && current.writer_id == writer_id
                    && unpublished.contains(&row.key())
            });
            if settled {
                continue;
            }
            let clock = super::issue_write_clock(&transaction, section)?;
            write_row(
                &transaction,
                section,
                &SectionRow {
                    write_clock: clock,
                    writer_id: writer_id.clone(),
                    ..row.clone()
                },
                false,
            )?;
        }
        for (key, current) in &held {
            if restored.contains(key) || current.value == SectionValueRow::Tombstone {
                continue;
            }
            let clock = super::issue_write_clock(&transaction, section)?;
            write_row(
                &transaction,
                section,
                &SectionRow {
                    value: SectionValueRow::Tombstone,
                    write_clock: clock,
                    writer_id: writer_id.clone(),
                    ..current.clone()
                },
                false,
            )?;
        }
        super::finish_mutation(&transaction)?;
        transaction.commit()?;
        Ok(())
    }

    /// Installs backup material for the same device. Versions are reissued
    /// locally because a bundle carries user values without them. The area is
    /// replaced within its own bounds: a local setting or permission the
    /// material leaves out is removed, while every device setting outside the
    /// backed-up list keeps whatever this device holds.
    pub(crate) fn restore_local_setting_rows(&mut self, rows: &[SectionRow]) -> StoreResult<()> {
        let transaction = self.transaction()?;
        let mut settings = BTreeSet::new();
        let mut permissions = BTreeSet::new();
        for row in rows {
            match &row.value {
                SectionValueRow::Setting { value } => {
                    if row.key1 != "setting" || !setting_is_local(&row.key2) {
                        return Err(invalid("restored device setting is not a device setting"));
                    }
                    if !settings.insert(row.key2.clone()) {
                        return Err(invalid("restored device settings repeat a key"));
                    }
                    transaction.execute(
                        "INSERT INTO device_settings (key,value) VALUES (?1,?2)
                            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        params![row.key2, value],
                    )?;
                }
                SectionValueRow::PluginPermission { granted } => {
                    if row.key1 != "pluginPermission" || row.key2.is_empty() || row.key3.is_empty()
                    {
                        return Err(invalid("restored plugin permission is incomplete"));
                    }
                    if !permissions.insert((row.key2.clone(), row.key3.clone())) {
                        return Err(invalid("restored plugin permissions repeat a key"));
                    }
                    transaction.execute(
                        "INSERT INTO plugin_permissions (code_hash,permission,granted)
                            VALUES (?1,?2,?3)
                            ON CONFLICT(code_hash,permission) DO UPDATE SET granted=excluded.granted",
                        params![row.key2, row.key3, i64::from(*granted)],
                    )?;
                }
                _ => return Err(invalid("restored device setting has the wrong shape")),
            }
        }
        for key in LOCAL_SETTING_KEYS {
            if !settings.contains(key) {
                transaction.execute("DELETE FROM device_settings WHERE key=?1", [key])?;
            }
        }
        let held = {
            let mut statement =
                transaction.prepare("SELECT code_hash,permission FROM plugin_permissions")?;
            let mut query = statement.query([])?;
            let mut held = Vec::new();
            while let Some(row) = query.next()? {
                held.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
            }
            held
        };
        for key in held {
            if !permissions.contains(&key) {
                transaction.execute(
                    "DELETE FROM plugin_permissions WHERE code_hash=?1 AND permission=?2",
                    params![key.0, key.1],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

/// `published` is false for a restored value: it is a new local write that no
/// remote has seen yet.
fn write_row(
    tx: &Transaction<'_>,
    section: Section,
    row: &SectionRow,
    published: bool,
) -> StoreResult<()> {
    let published_clock = published.then(|| row.write_clock.as_str().to_owned());
    match (section, &row.value) {
        (Section::Hypa, SectionValueRow::Tombstone) => {
            tx.execute(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock)
                    VALUES (?1,'','',NULL,0,1,NULL,NULL,1,?2,?3,?4)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        vector=NULL,metadata=NULL,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock",
                params![
                    row.key1,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        (
            Section::Hypa,
            SectionValueRow::Hypa {
                producer,
                model,
                endpoint,
                preprocess_version,
                dimensions,
                vector,
                metadata,
            },
        ) => {
            tx.execute(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        producer=excluded.producer,model=excluded.model,
                        endpoint=excluded.endpoint,
                        preprocess_version=excluded.preprocess_version,
                        dimensions=excluded.dimensions,vector=excluded.vector,
                        metadata=excluded.metadata,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock",
                params![
                    row.key1,
                    producer,
                    model,
                    endpoint,
                    preprocess_version,
                    dimensions,
                    vector,
                    metadata,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        (Section::LocalPlugins, SectionValueRow::Tombstone) => {
            tx.execute(
                "INSERT INTO plugin_device_storage
                    (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                     published_clock)
                    VALUES (?1,?2,?3,NULL,0,1,?4,?5,?6)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=NULL,byte_size=0,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock",
                params![
                    row.key1,
                    row.key2,
                    row.key3,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        (Section::LocalPlugins, SectionValueRow::Plugin { space, value }) => {
            if *space != row.key2 {
                return Err(invalid("received plugin value names another space"));
            }
            let byte_size = i64::try_from(value.len())
                .map_err(|_| invalid("received plugin value is too large"))?;
            tx.execute(
                "INSERT INTO plugin_device_storage
                    (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                     published_clock)
                    VALUES (?1,?2,?3,?4,?5,0,?6,?7,?8)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=excluded.value,byte_size=excluded.byte_size,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock",
                params![
                    row.key1,
                    row.key2,
                    row.key3,
                    value,
                    byte_size,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock
                ],
            )?;
        }
        _ => return Err(invalid("received section row belongs to another section")),
    }
    Ok(())
}
