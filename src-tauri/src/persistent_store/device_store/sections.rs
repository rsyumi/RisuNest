//! Section rows as a remote adapter sees them. Publication reads rows here and
//! reception merges them back, so one rule decides every value no matter which
//! remote carried it.
use super::{invalid, observe_remote_clock, sequence, DeviceStore, Section};
use crate::persistent_store::StoreResult;
use risunest_sync_wire::Sequence;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
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

/// The sections a device chooses to take part in, in the order the settings
/// screen lists them.
pub(crate) const CHOOSABLE_SECTIONS: [Section; 2] = [Section::Hypa, Section::LocalPlugins];

/// Resolves the identifier a renderer sends. An unknown one is refused rather
/// than silently treated as one of the known sections.
pub(crate) fn section_from_id(id: &str) -> Option<Section> {
    CHOOSABLE_SECTIONS
        .into_iter()
        .find(|section| section.as_str() == id)
}

/// The commit a removal first reached a remote in, and when that happened. A
/// removal this device has not published yet carries none, and the marker only
/// means anything inside the remote lineage that issued the commit number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TombstonePublication {
    pub generation: Sequence,
    pub at_ms: u64,
}

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
    Tombstone {
        first_published: Option<TombstonePublication>,
    },
}

impl SectionValueRow {
    pub(crate) fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone { .. })
    }
    /// Whether two rows hold the same thing. A removal's first publication
    /// marker is bookkeeping about the removal, not part of what the key holds,
    /// so two removals of the same key are the same content either way.
    pub(crate) fn same_content(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Tombstone { .. }, Self::Tombstone { .. }) => true,
            _ => self == other,
        }
    }
    fn first_published(&self) -> Option<&TombstonePublication> {
        match self {
            Self::Tombstone { first_published } => first_published.as_ref(),
            _ => None,
        }
    }
}

/// The triple a section row is named by, in the order the change index holds.
pub(crate) type SectionKey = (String, String, String);

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

/// Keys whose current value no remote has been told about. A restore uses them
/// to tell its own interrupted attempt apart from a value some remote already
/// carries, and publication uses them to decide whether it owes one at all. A
/// row reissued above the version it was published at counts as unpublished.
fn unpublished_keys(
    db: &Connection,
    section: Section,
) -> StoreResult<BTreeSet<(String, String, String)>> {
    let mut keys = BTreeSet::new();
    match section {
        Section::Hypa => {
            let mut statement = db.prepare(
                "SELECT cache_key FROM hypa_embeddings
                    WHERE published_clock IS NULL OR published_clock<>write_clock",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, String::new(), String::new()));
            }
        }
        Section::LocalPlugins => {
            let mut statement = db.prepare(
                "SELECT owner,space,key FROM plugin_device_storage
                    WHERE published_clock IS NULL OR published_clock<>write_clock",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                keys.insert((row.get(0)?, row.get(1)?, row.get(2)?));
            }
        }
    }
    Ok(keys)
}

/// The stored marker pair. The schema keeps the two columns set or unset
/// together, so a half-written pair is a broken device file rather than a
/// removal this device may publish.
fn first_published(
    generation: Option<String>,
    at_ms: Option<i64>,
) -> StoreResult<Option<TombstonePublication>> {
    match (generation, at_ms) {
        (Some(generation), Some(at_ms)) => Ok(Some(TombstonePublication {
            generation: sequence(&generation)?,
            at_ms: u64::try_from(at_ms)
                .map_err(|_| invalid("device removal marker time is out of range"))?,
        })),
        (None, None) => Ok(None),
        _ => Err(invalid("device removal marker is incomplete")),
    }
}

/// Whether `candidate` names an earlier first publication than `current`. A
/// removal this device has not published yet takes whichever marker a remote
/// carries for the same version.
fn earlier_marker(candidate: &SectionValueRow, current: &SectionValueRow) -> bool {
    match (candidate.first_published(), current.first_published()) {
        (Some(candidate), Some(current)) => {
            (&candidate.generation, candidate.at_ms) < (&current.generation, current.at_ms)
        }
        (Some(_), None) => true,
        _ => false,
    }
}

fn read_rows(tx: &Transaction<'_>, section: Section) -> StoreResult<Vec<SectionRow>> {
    let mut rows = Vec::new();
    match section {
        Section::Hypa => {
            let mut statement = tx.prepare(
                "SELECT cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                        metadata,tombstone,write_clock,writer_id,first_published_generation,
                        first_published_at_ms
                    FROM hypa_embeddings ORDER BY cache_key",
            )?;
            let mut query = statement.query([])?;
            while let Some(row) = query.next()? {
                let tombstone: i64 = row.get(8)?;
                let vector: Option<Vec<u8>> = row.get(6)?;
                let value = if tombstone == 1 || vector.is_none() {
                    SectionValueRow::Tombstone {
                        first_published: first_published(row.get(11)?, row.get(12)?)?,
                    }
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
                "SELECT owner,space,key,value,tombstone,write_clock,writer_id,
                        first_published_generation,first_published_at_ms
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
                    _ => SectionValueRow::Tombstone {
                        first_published: first_published(row.get(7)?, row.get(8)?)?,
                    },
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
            .filter(|row| !row.value.is_tombstone())
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
                Some(_) => {
                    if !unpublished_keys(&self.connection, section)?.is_empty() {
                        return Ok(true);
                    }
                }
                // A lineage this device never exchanged with holds none of its
                // rows, however far the local counter has already travelled.
                None => {
                    if state.max_write_clock > Sequence::from(0u64) {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Records the versions a confirmed publication put on the remote. Each row
    /// is matched at the version it was captured at, so a local write that
    /// landed between the capture and the publication stays unpublished.
    /// Publication bookkeeping is control metadata, so it runs outside a change
    /// context and never reaches the device change index.
    pub(crate) fn note_section_published(
        &mut self,
        section: Section,
        published: &[(SectionKey, Sequence)],
        stamped: &[(SectionKey, Sequence)],
        first_published: &TombstonePublication,
    ) -> StoreResult<()> {
        if published.is_empty() && stamped.is_empty() {
            return Ok(());
        }
        let transaction = self.transaction()?;
        // A removal takes the marker the publication carried, so the device
        // file and every remote that read it name the same commit. A removal
        // rewritten since the capture is not the one that went out.
        {
            let mut statement = match section {
                Section::Hypa => transaction.prepare(
                    "UPDATE hypa_embeddings
                        SET first_published_generation=?3,first_published_at_ms=?4
                        WHERE cache_key=?1 AND write_clock=?2 AND tombstone=1
                          AND first_published_generation IS NULL",
                )?,
                Section::LocalPlugins => transaction.prepare(
                    "UPDATE plugin_device_storage
                        SET first_published_generation=?5,first_published_at_ms=?6
                        WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4
                          AND tombstone=1 AND first_published_generation IS NULL",
                )?,
            };
            let generation = first_published.generation.as_str();
            let at_ms = i64::try_from(first_published.at_ms)
                .map_err(|_| invalid("device removal marker time is out of range"))?;
            for ((key1, key2, key3), clock) in stamped {
                match section {
                    Section::Hypa => {
                        statement.execute(params![key1, clock.as_str(), generation, at_ms])?;
                    }
                    Section::LocalPlugins => {
                        statement.execute(params![
                            key1,
                            key2,
                            key3,
                            clock.as_str(),
                            generation,
                            at_ms
                        ])?;
                    }
                }
            }
        }
        {
            let mut statement = match section {
                Section::Hypa => transaction.prepare(
                    "UPDATE hypa_embeddings SET published_clock=?2
                        WHERE cache_key=?1 AND write_clock=?2",
                )?,
                Section::LocalPlugins => transaction.prepare(
                    "UPDATE plugin_device_storage SET published_clock=?4
                        WHERE owner=?1 AND space=?2 AND key=?3 AND write_clock=?4",
                )?,
            };
            for ((key1, key2, key3), clock) in published {
                match section {
                    Section::Hypa => {
                        statement.execute(params![key1, clock.as_str()])?;
                    }
                    Section::LocalPlugins => {
                        statement.execute(params![key1, key2, key3, clock.as_str()])?;
                    }
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Rewrites this device's section as its own newest writes, above every
    /// version `observed` covers. Values and preserved removals both travel,
    /// so the merge that follows keeps them wherever `remote` holds the same
    /// key and the keys only this device holds still reach the remote. A row
    /// the remote already carries unchanged needs neither, and a row this
    /// device issued above `observed` and has not published is already the
    /// newest, so a retried attempt stamps no second version.
    pub(crate) fn reissue_section_rows(
        &mut self,
        section: Section,
        observed: &Sequence,
        remote: &[SectionRow],
    ) -> StoreResult<usize> {
        let transaction = self.transaction()?;
        let writer_id: String = transaction.query_row(
            "SELECT writer_id FROM device_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        let carried: BTreeMap<(String, String, String), &SectionRow> =
            remote.iter().map(|row| (row.key(), row)).collect();
        let unpublished = unpublished_keys(&transaction, section)?;
        let pending: Vec<SectionRow> = read_rows(&transaction, section)?
            .into_iter()
            .filter(|row| {
                let held = row.writer_id == writer_id
                    && row.write_clock > *observed
                    && unpublished.contains(&row.key());
                let settled = carried
                    .get(&row.key())
                    .is_some_and(|other| {
                        row.same_version(other) && row.value.same_content(&other.value)
                    });
                !held && !settled
            })
            .collect();
        // The issued clock has to clear the remote as well as this device, so
        // a value held here under another writer cannot lose its version.
        observe_remote_clock(&transaction, section, observed)?;
        if !pending.is_empty() {
            super::begin_mutation(&transaction)?;
            let clock = super::issue_write_clock(&transaction, section)?;
            for row in &pending {
                write_row(
                    &transaction,
                    section,
                    &SectionRow {
                        // A reissued removal is a new write for whichever
                        // lineage receives it, and commit numbers mean nothing
                        // across lineages, so it drops the marker it carried
                        // and takes one from the publication that carries it.
                        value: match &row.value {
                            SectionValueRow::Tombstone { .. } => SectionValueRow::Tombstone {
                                first_published: None,
                            },
                            value => value.clone(),
                        },
                        write_clock: clock.clone(),
                        writer_id: writer_id.clone(),
                        ..row.clone()
                    },
                    false,
                )?;
            }
            super::finish_mutation(&transaction)?;
        }
        transaction.commit()?;
        Ok(pending.len())
    }

    /// Drops the removals a remote has reclaimed. A removal goes only when the
    /// received section declares a floor at or above the commit the removal was
    /// first published in and no longer carries that removal itself: a floor can
    /// stand above a removal the remote still holds, and dropping that one would
    /// let another device's older value come back. A removal this device has not
    /// published carries no marker and stays. The caller has already established
    /// that the received section comes from the lineage the markers name.
    ///
    /// This is bookkeeping about a removal rather than a write, so it runs
    /// outside a change context and never reaches the device change index.
    pub(crate) fn reclaim_section_tombstones(
        &mut self,
        section: Section,
        gc_floor: &Sequence,
        received: &[SectionRow],
    ) -> StoreResult<usize> {
        if *gc_floor == Sequence::from(0u64) {
            return Ok(0);
        }
        let carried: BTreeSet<SectionKey> = received
            .iter()
            .filter(|row| row.value.is_tombstone())
            .map(SectionRow::key)
            .collect();
        let transaction = self.transaction()?;
        let reclaimable: Vec<SectionKey> = read_rows(&transaction, section)?
            .into_iter()
            .filter(|row| {
                row.value
                    .first_published()
                    .is_some_and(|marker| marker.generation <= *gc_floor)
                    && !carried.contains(&row.key())
            })
            .map(|row| row.key())
            .collect();
        for (key1, key2, key3) in &reclaimable {
            match section {
                Section::Hypa => {
                    transaction
                        .execute("DELETE FROM hypa_embeddings WHERE cache_key=?1", [key1])?;
                }
                Section::LocalPlugins => {
                    transaction.execute(
                        "DELETE FROM plugin_device_storage
                            WHERE owner=?1 AND space=?2 AND key=?3",
                        params![key1, key2, key3],
                    )?;
                }
            }
        }
        transaction.commit()?;
        Ok(reclaimable.len())
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
        let mut adopted = Vec::new();
        for row in rows {
            if row.writer_id.is_empty() {
                return Err(invalid("received section row has no writer"));
            }
            if row.write_clock > highest {
                highest = row.write_clock.clone();
            }
            match local.get(&row.key()) {
                Some(current) if current.same_version(row) => {
                    if !current.value.same_content(&row.value) {
                        return Err(invalid(
                            "received section row differs at the same version",
                        ));
                    }
                    // Two devices can hold different first publication markers
                    // for one removal when a confirmed publication did not
                    // finish its local bookkeeping. The earliest marker is the
                    // one that happened, and taking it whichever way the rows
                    // arrive keeps the devices from publishing over each other.
                    if earlier_marker(&row.value, &current.value) {
                        adopted.push(row);
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
        for row in &adopted {
            write_row(&transaction, section, row, true)?;
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
            if row.value.is_tombstone() {
                return Err(invalid("restored section row has no value"));
            }
            let settled = held.get(&row.key()).is_some_and(|current| {
                current.value.same_content(&row.value)
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
            if restored.contains(key) || current.value.is_tombstone() {
                continue;
            }
            let clock = super::issue_write_clock(&transaction, section)?;
            write_row(
                &transaction,
                section,
                &SectionRow {
                    value: SectionValueRow::Tombstone {
                        first_published: None,
                    },
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
    let marker = row.value.first_published();
    let generation = marker.map(|marker| marker.generation.as_str().to_owned());
    let at_ms = marker.map(|marker| marker.at_ms as i64);
    match (section, &row.value) {
        (Section::Hypa, SectionValueRow::Tombstone { .. }) => {
            tx.execute(
                "INSERT INTO hypa_embeddings
                    (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                     metadata,tombstone,write_clock,writer_id,published_clock,
                     first_published_generation,first_published_at_ms)
                    VALUES (?1,'','',NULL,0,1,NULL,NULL,1,?2,?3,?4,?5,?6)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        vector=NULL,metadata=NULL,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=excluded.first_published_generation,
                        first_published_at_ms=excluded.first_published_at_ms",
                params![
                    row.key1,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock,
                    generation,
                    at_ms
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
                     metadata,tombstone,write_clock,writer_id,published_clock,
                     first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?11,NULL,NULL)
                    ON CONFLICT(cache_key) DO UPDATE SET
                        producer=excluded.producer,model=excluded.model,
                        endpoint=excluded.endpoint,
                        preprocess_version=excluded.preprocess_version,
                        dimensions=excluded.dimensions,vector=excluded.vector,
                        metadata=excluded.metadata,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=NULL,first_published_at_ms=NULL",
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
        (Section::LocalPlugins, SectionValueRow::Tombstone { .. }) => {
            tx.execute(
                "INSERT INTO plugin_device_storage
                    (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                     published_clock,first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,NULL,0,1,?4,?5,?6,?7,?8)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=NULL,byte_size=0,tombstone=1,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=excluded.first_published_generation,
                        first_published_at_ms=excluded.first_published_at_ms",
                params![
                    row.key1,
                    row.key2,
                    row.key3,
                    row.write_clock.as_str(),
                    row.writer_id,
                    published_clock,
                    generation,
                    at_ms
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
                     published_clock,first_published_generation,first_published_at_ms)
                    VALUES (?1,?2,?3,?4,?5,0,?6,?7,?8,NULL,NULL)
                    ON CONFLICT(owner,space,key) DO UPDATE SET
                        value=excluded.value,byte_size=excluded.byte_size,tombstone=0,
                        write_clock=excluded.write_clock,writer_id=excluded.writer_id,
                        published_clock=excluded.published_clock,
                        first_published_generation=NULL,first_published_at_ms=NULL",
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
