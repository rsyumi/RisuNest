//! Device sections on the server replica. Their rows live in the device file
//! and settle on their own write clock, so the library's three way base plays
//! no part in deciding a winner here.
use super::device_store::{observe_remote_clock, DeviceStore, Section};
use super::{StoreError, StoreResult};
use risunest_external_storage_format::section::{
    decode_local_plugin_entry_key, hypa_entry_key, local_plugin_entry_key, HypaValue,
    InlineOrObject, LocalPluginValue, ObjectReference, PluginSpace, SectionEntry,
    SectionEntryVersion, SectionKind, SectionValue, MAX_INLINE_VALUE_BYTES,
};
use risunest_sync_wire::{Domain, Sequence};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::Value;
use std::collections::BTreeMap;

/// One page of section keys, bounded like every other replica scan.
pub(crate) const SECTION_PAGE: usize = 512;

fn invalid<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.to_owned(),
    })
}

pub(crate) fn section_of(domain: Domain) -> Option<Section> {
    match domain {
        Domain::Hypa => Some(Section::Hypa),
        Domain::LocalPlugins => Some(Section::LocalPlugins),
        Domain::Library => None,
    }
}

pub(crate) fn kind_of(domain: Domain) -> Option<SectionKind> {
    match domain {
        Domain::Hypa => Some(SectionKind::Hypa),
        Domain::LocalPlugins => Some(SectionKind::LocalPlugins),
        Domain::Library => None,
    }
}

/// The sections this device publishes and applies, with the generation the
/// choice was made in, in wire order.
pub(crate) fn participation(device: &DeviceStore) -> StoreResult<Vec<(Domain, String)>> {
    let mut chosen = Vec::new();
    for domain in Domain::ALL {
        let Some(section) = section_of(domain) else {
            continue;
        };
        let row: Option<(bool, String)> = device
            .connection()
            .query_row(
                "SELECT participating,participation_generation FROM device_sections WHERE section=?1",
                [section.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((participating, generation)) = row else {
            return invalid("Device section row is missing");
        };
        if participating {
            chosen.push((domain, generation));
        }
    }
    Ok(chosen)
}

/// The identity of a row as the merge rule sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalEntry {
    pub version: SectionEntryVersion,
    pub entry: SectionEntry,
    /// The vector body when it is too large to ride inside the entry.
    pub object: Option<Vec<u8>>,
    pub published: bool,
}

fn sequence(value: &str) -> StoreResult<Sequence> {
    Sequence::try_from(value.to_owned())
        .map_err(|_| StoreError::Validation {
            message: "Device write clock is invalid".into(),
        })
}

fn format_error(_: risunest_external_storage_format::FormatError) -> StoreError {
    StoreError::Validation {
        message: "Section entry is invalid".into(),
    }
}

/// Orders two versions by the clock and breaks a tie on the writer identity.
/// Wall clock time never takes part.
pub(crate) fn newer(candidate: &SectionEntryVersion, current: &SectionEntryVersion) -> bool {
    (&candidate.write_clock, &candidate.writer_id) > (&current.write_clock, &current.writer_id)
}

/// What the replica does with one key once both sides are known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Apply,
    Publish,
    Settled,
}

/// The merge rule both remote adapters share. The larger clock wins, a tie is
/// broken on the writer identity, and one version can only ever stand for one
/// value.
pub(crate) fn resolve(local: Option<&LocalEntry>, received: &SectionEntry) -> StoreResult<Outcome> {
    let Some(version) = received.version.as_ref() else {
        return invalid("Section entry carries no version");
    };
    let Some(current) = local else {
        return Ok(Outcome::Apply);
    };
    if newer(version, &current.version) {
        return Ok(Outcome::Apply);
    }
    if newer(&current.version, version) {
        return Ok(Outcome::Publish);
    }
    if &current.entry != received {
        return invalid("Section version carries two different values");
    }
    Ok(Outcome::Settled)
}

fn vector_value(bytes: &[u8]) -> StoreResult<(InlineOrObject, Option<Vec<u8>>)> {
    if bytes.len() <= MAX_INLINE_VALUE_BYTES {
        return Ok((InlineOrObject::inline(bytes).map_err(format_error)?, None));
    }
    let digest = risunest_sync_wire::hash(bytes);
    let mut content_sha256 = [0u8; 32];
    hex::decode_to_slice(&digest, &mut content_sha256).map_err(|_| StoreError::Validation {
        message: "Section object hash is invalid".into(),
    })?;
    Ok((
        InlineOrObject::Object(ObjectReference {
            content_sha256,
            byte_length: bytes.len() as u64,
        }),
        Some(bytes.to_vec()),
    ))
}

fn read_hypa(db: &Connection, key: &str) -> StoreResult<Option<LocalEntry>> {
    let row: Option<(
        String,
        String,
        Option<String>,
        i64,
        i64,
        Option<Vec<u8>>,
        Option<String>,
        bool,
        String,
        String,
        Option<String>,
    )> = db
        .query_row(
            "SELECT producer,model,endpoint,preprocess_version,dimensions,vector,metadata,
                    tombstone,write_clock,writer_id,published_clock
                FROM hypa_embeddings WHERE cache_key=?1",
            [key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            },
        )
        .optional()?;
    let Some((
        producer,
        model,
        endpoint,
        preprocess_version,
        dimensions,
        vector,
        metadata,
        tombstone,
        write_clock,
        writer_id,
        published_clock,
    )) = row
    else {
        return Ok(None);
    };
    let version = SectionEntryVersion {
        write_clock: sequence(&write_clock)?,
        writer_id,
    };
    let (value, object) = if tombstone {
        (SectionValue::Tombstone, None)
    } else {
        let Some(vector) = vector else {
            return invalid("Embedding row carries no vector");
        };
        let (reference, object) = vector_value(&vector)?;
        (
            SectionValue::Hypa(HypaValue {
                producer,
                model,
                endpoint,
                preprocess_version: u32::try_from(preprocess_version)
                    .map_err(|_| StoreError::Validation {
                        message: "Embedding preprocess version is out of range".into(),
                    })?,
                dimensions: u32::try_from(dimensions).map_err(|_| StoreError::Validation {
                    message: "Embedding dimensions are out of range".into(),
                })?,
                vector: reference,
                metadata: metadata
                    .map(|text| serde_json::from_str::<Value>(&text))
                    .transpose()?,
            }),
            object,
        )
    };
    let entry = SectionEntry::new(
        SectionKind::Hypa,
        hypa_entry_key(key).map_err(format_error)?,
        value,
        Some(version.clone()),
    )
    .map_err(format_error)?;
    Ok(Some(LocalEntry {
        published: published_clock.as_deref() == Some(version.write_clock.as_str()),
        version,
        entry,
        object,
    }))
}

fn read_plugin(db: &Connection, key: &str) -> StoreResult<Option<LocalEntry>> {
    let (owner, space, name) = decode_local_plugin_entry_key(key).map_err(format_error)?;
    let row: Option<(Option<String>, bool, String, String, Option<String>)> = db
        .query_row(
            "SELECT value,tombstone,write_clock,writer_id,published_clock
                FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3",
            params![owner, space, name],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((value, tombstone, write_clock, writer_id, published_clock)) = row else {
        return Ok(None);
    };
    let version = SectionEntryVersion {
        write_clock: sequence(&write_clock)?,
        writer_id,
    };
    let value = if tombstone {
        SectionValue::Tombstone
    } else {
        let Some(text) = value else {
            return invalid("Plugin device row carries no value");
        };
        let (space, value) = match space.as_str() {
            "string" => (PluginSpace::String, Value::String(text)),
            "json" => (PluginSpace::Json, serde_json::from_str::<Value>(&text)?),
            _ => return invalid("Plugin device space is invalid"),
        };
        SectionValue::LocalPlugin(LocalPluginValue { space, value })
    };
    let entry = SectionEntry::new(
        SectionKind::LocalPlugins,
        key.to_owned(),
        value,
        Some(version.clone()),
    )
    .map_err(format_error)?;
    Ok(Some(LocalEntry {
        published: published_clock.as_deref() == Some(version.write_clock.as_str()),
        version,
        entry,
        object: None,
    }))
}

pub(crate) fn read_local(
    device: &DeviceStore,
    domain: Domain,
    key: &str,
) -> StoreResult<Option<LocalEntry>> {
    match domain {
        Domain::Hypa => read_hypa(device.connection(), key),
        Domain::LocalPlugins => read_plugin(device.connection(), key),
        Domain::Library => invalid("The library is not a device section"),
    }
}

/// Keys whose current value has not reached the remote this device is bound to.
pub(crate) fn pending_page(
    device: &DeviceStore,
    domain: Domain,
    after: &str,
    limit: usize,
) -> StoreResult<Vec<String>> {
    let db = device.connection();
    match domain {
        Domain::Hypa => {
            let mut statement = db.prepare(
                "SELECT cache_key FROM hypa_embeddings
                    WHERE (published_clock IS NULL OR published_clock<>write_clock)
                      AND cache_key>?1 ORDER BY cache_key LIMIT ?2",
            )?;
            let keys = statement
                .query_map(params![after, limit as i64], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            keys.iter()
                .map(|key| hypa_entry_key(key).map_err(format_error))
                .collect()
        }
        Domain::LocalPlugins => {
            let mut statement = db.prepare(
                "SELECT owner,space,key FROM plugin_device_storage
                    WHERE published_clock IS NULL OR published_clock<>write_clock
                    ORDER BY owner,space,key",
            )?;
            // The wire key is a structured encoding, so its byte order differs
            // from the column order. Page on the encoded key the caller sees.
            let mut keys = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .map(|row| {
                    let row: (String, String, String) = row?;
                    local_plugin_entry_key(&row.0, &row.1, &row.2).map_err(format_error)
                })
                .collect::<StoreResult<Vec<_>>>()?;
            keys.sort();
            Ok(keys
                .into_iter()
                .filter(|key| key.as_str() > after)
                .take(limit)
                .collect())
        }
        Domain::Library => invalid("The library is not a device section"),
    }
}

/// Forgets which values reached a remote. Publication is recorded against the
/// binding that received it, so a new or reset binding starts from nothing.
pub(crate) fn forget_publications(device: &mut DeviceStore) -> StoreResult<()> {
    let tx = device.transaction()?;
    for table in ["hypa_embeddings", "plugin_device_storage"] {
        tx.execute(
            &format!("UPDATE {table} SET published_clock=NULL WHERE published_clock IS NOT NULL"),
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn mark_row(tx: &Transaction<'_>, domain: Domain, key: &str) -> StoreResult<()> {
    // Publication bookkeeping is control metadata, so it runs outside a change
    // context and never reaches the device change index.
    match domain {
        Domain::Hypa => {
            tx.execute(
                "UPDATE hypa_embeddings SET published_clock=write_clock WHERE cache_key=?1",
                [key],
            )?;
        }
        Domain::LocalPlugins => {
            let (owner, space, name) = decode_local_plugin_entry_key(key).map_err(format_error)?;
            tx.execute(
                "UPDATE plugin_device_storage SET published_clock=write_clock
                    WHERE owner=?1 AND space=?2 AND key=?3",
                params![owner, space, name],
            )?;
        }
        Domain::Library => return invalid("The library is not a device section"),
    }
    Ok(())
}

fn apply_row(
    tx: &Transaction<'_>,
    domain: Domain,
    entry: &SectionEntry,
    object: Option<&[u8]>,
) -> StoreResult<()> {
    let version = entry
        .version
        .as_ref()
        .ok_or_else(|| StoreError::Validation {
            message: "Section entry carries no version".into(),
        })?;
    if kind_of(domain) != Some(entry.kind) {
        return invalid("Section entry belongs to another section");
    }
    match domain {
        Domain::Hypa => match &entry.value {
            SectionValue::Hypa(value) => {
                let vector = match &value.vector {
                    InlineOrObject::Inline(_) => {
                        value.vector.decode_inline().map_err(format_error)?
                    }
                    InlineOrObject::Object(reference) => {
                        let bytes = object.ok_or_else(|| StoreError::Validation {
                            message: "Section vector object is missing".into(),
                        })?;
                        if bytes.len() as u64 != reference.byte_length
                            || risunest_sync_wire::hash(bytes) != hex::encode(reference.content_sha256)
                        {
                            return invalid("Section vector object does not match its reference");
                        }
                        bytes.to_vec()
                    }
                };
                if vector.len() != value.dimensions as usize * 4 {
                    return invalid("Section vector length does not match its dimensions");
                }
                tx.execute(
                    "INSERT INTO hypa_embeddings
                        (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                         metadata,tombstone,write_clock,writer_id,published_clock)
                        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,?9,?10,?9)
                        ON CONFLICT(cache_key) DO UPDATE SET
                            producer=excluded.producer,
                            model=excluded.model,
                            endpoint=excluded.endpoint,
                            preprocess_version=excluded.preprocess_version,
                            dimensions=excluded.dimensions,
                            vector=excluded.vector,
                            metadata=excluded.metadata,
                            tombstone=0,
                            write_clock=excluded.write_clock,
                            writer_id=excluded.writer_id,
                            published_clock=excluded.published_clock",
                    params![
                        entry.key,
                        value.producer,
                        value.model,
                        value.endpoint,
                        value.preprocess_version as i64,
                        value.dimensions as i64,
                        vector,
                        value
                            .metadata
                            .as_ref()
                            .map(serde_json::to_string)
                            .transpose()?,
                        version.write_clock.as_str(),
                        version.writer_id,
                    ],
                )?;
            }
            // A deletion is kept as a row even for a key this device never
            // held, so a later delivery of an older value cannot revive it.
            SectionValue::Tombstone => {
                tx.execute(
                    "INSERT INTO hypa_embeddings
                        (cache_key,producer,model,endpoint,preprocess_version,dimensions,vector,
                         metadata,tombstone,write_clock,writer_id,published_clock)
                        VALUES (?1,'','',NULL,0,1,NULL,NULL,1,?2,?3,?2)
                        ON CONFLICT(cache_key) DO UPDATE SET
                            vector=NULL,
                            metadata=NULL,
                            tombstone=1,
                            write_clock=excluded.write_clock,
                            writer_id=excluded.writer_id,
                            published_clock=excluded.published_clock",
                    params![entry.key, version.write_clock.as_str(), version.writer_id],
                )?;
            }
            _ => return invalid("Section entry belongs to another section"),
        },
        Domain::LocalPlugins => {
            let (owner, space, name) =
                decode_local_plugin_entry_key(&entry.key).map_err(format_error)?;
            match &entry.value {
                SectionValue::LocalPlugin(value) => {
                    let declared = match value.space {
                        PluginSpace::String => "string",
                        PluginSpace::Json => "json",
                    };
                    if declared != space {
                        return invalid("Plugin section value declares another space");
                    }
                    let text = match (&value.space, &value.value) {
                        (PluginSpace::String, Value::String(text)) => text.clone(),
                        (PluginSpace::String, _) => {
                            return invalid("Plugin string value is not a string")
                        }
                        (PluginSpace::Json, value) => serde_json::to_string(value)?,
                    };
                    tx.execute(
                        "INSERT INTO plugin_device_storage
                            (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                             published_clock)
                            VALUES (?1,?2,?3,?4,?5,0,?6,?7,?6)
                            ON CONFLICT(owner,space,key) DO UPDATE SET
                                value=excluded.value,
                                byte_size=excluded.byte_size,
                                tombstone=0,
                                write_clock=excluded.write_clock,
                                writer_id=excluded.writer_id,
                                published_clock=excluded.published_clock",
                        params![
                            owner,
                            space,
                            name,
                            text,
                            text.len() as i64,
                            version.write_clock.as_str(),
                            version.writer_id
                        ],
                    )?;
                }
                // A deletion is kept as a row even for a key this device never
                // held, so a later delivery of an older value cannot revive it.
                SectionValue::Tombstone => {
                    tx.execute(
                        "INSERT INTO plugin_device_storage
                            (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,
                             published_clock)
                            VALUES (?1,?2,?3,NULL,0,1,?4,?5,?4)
                            ON CONFLICT(owner,space,key) DO UPDATE SET
                                value=NULL,
                                byte_size=0,
                                tombstone=1,
                                write_clock=excluded.write_clock,
                                writer_id=excluded.writer_id,
                                published_clock=excluded.published_clock",
                        params![
                            owner,
                            space,
                            name,
                            version.write_clock.as_str(),
                            version.writer_id
                        ],
                    )?;
                }
                _ => return invalid("Section entry belongs to another section"),
            }
        }
        Domain::Library => return invalid("The library is not a device section"),
    }
    Ok(())
}

/// What one activation writes into the device file.
pub(crate) enum SectionWrite {
    Apply {
        domain: Domain,
        entry: SectionEntry,
        object: Option<Vec<u8>>,
    },
    Mark {
        domain: Domain,
        key: String,
    },
}

/// Applies received entries and publication bookkeeping in one device
/// transaction. The persistent store commits separately, so a section that
/// landed here stays landed even when the library activation fails afterwards.
/// Received rows are written outside a change context, so the change index does
/// not offer them back as this device's own writes.
pub(crate) fn write_sections(device: &mut DeviceStore, writes: &[SectionWrite]) -> StoreResult<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let tx = device.transaction()?;
    let mut observed: BTreeMap<&'static str, (Section, Sequence)> = BTreeMap::new();
    for write in writes {
        match write {
            SectionWrite::Apply {
                domain,
                entry,
                object,
            } => {
                let Some(section) = section_of(*domain) else {
                    return invalid("The library is not a device section");
                };
                let version = entry
                    .version
                    .as_ref()
                    .ok_or_else(|| StoreError::Validation {
                        message: "Section entry carries no version".into(),
                    })?;
                let highest = observed
                    .entry(section.as_str())
                    .or_insert_with(|| (section, Sequence::from(0u64)));
                if version.write_clock > highest.1 {
                    highest.1 = version.write_clock.clone();
                }
                apply_row(&tx, *domain, entry, object.as_deref())?;
            }
            SectionWrite::Mark { domain, key } => mark_row(&tx, *domain, key)?,
        }
    }
    for (section, highest) in observed.into_values() {
        observe_remote_clock(&tx, section, &highest)?;
    }
    tx.commit()?;
    Ok(())
}
