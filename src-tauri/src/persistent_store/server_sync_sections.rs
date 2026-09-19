//! Server transport for device sections. DeviceStore owns row conversion,
//! merge policy and publication bookkeeping.
use super::device_store::{DeviceStore, Section};
use super::device_store::sections::{
    kind_of_section, resolve_section_row, SectionMergeDecision, SectionWriteInput,
};
use super::{StoreError, StoreResult};
use risunest_external_storage_format::section::{SectionEntry, SectionEntryVersion, SectionKind};
use risunest_sync_wire::{Domain, Sequence};

pub(crate) use super::device_store::sections::LocalSectionEntry as LocalEntry;

pub(crate) const SECTION_PAGE: usize = 256;

fn require_section(domain: Domain) -> StoreResult<Section> {
    section_of(domain).ok_or_else(|| StoreError::Validation {
        message: "The library is not a device section".into(),
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
    section_of(domain).map(kind_of_section)
}

pub(crate) fn participation(device: &DeviceStore) -> StoreResult<Vec<(Domain, String)>> {
    let mut chosen = Vec::new();
    for domain in Domain::ALL {
        let Some(section) = section_of(domain) else { continue; };
        let state = device.section_state(section)?;
        if state.participating {
            chosen.push((domain, state.participation_generation.as_str().to_owned()));
        }
    }
    Ok(chosen)
}

pub(crate) fn has_pending(device: &DeviceStore) -> StoreResult<bool> {
    for (domain, _) in participation(device)? {
        if device.has_pending_section_entries(require_section(domain)?)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn stamp_unpublished_removal(
    device: &DeviceStore,
    domain: Domain,
    key: &str,
    generation: &Sequence,
    now_ms: u64,
) -> StoreResult<()> {
    device.stamp_section_removal(require_section(domain)?, key, generation, now_ms)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Apply,
    Publish,
    Settled,
}

pub(crate) fn resolve(local: Option<&LocalEntry>, received: &SectionEntry) -> StoreResult<Outcome> {
    Ok(match resolve_section_row(local.map(|local| &local.entry), received)? {
        SectionMergeDecision::ApplyIncoming => Outcome::Apply,
        SectionMergeDecision::PublishLocal => Outcome::Publish,
        SectionMergeDecision::Settled => Outcome::Settled,
        SectionMergeDecision::MergeRemovalMarker { write_local: true, .. } => Outcome::Apply,
        SectionMergeDecision::MergeRemovalMarker { publish: true, .. } => Outcome::Publish,
        SectionMergeDecision::MergeRemovalMarker { .. } => Outcome::Settled,
    })
}

pub(crate) fn read_local(device: &DeviceStore, domain: Domain, key: &str) -> StoreResult<Option<LocalEntry>> {
    device.read_section_entry(require_section(domain)?, key)
}

pub(crate) fn pending_page(device: &DeviceStore, domain: Domain, after: &str, limit: usize) -> StoreResult<Vec<String>> {
    device.pending_section_entry_keys(require_section(domain)?, after, limit)
}

pub(crate) fn forget_publications(device: &mut DeviceStore) -> StoreResult<()> {
    device.forget_section_publications()
}

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
    MarkVersion {
        domain: Domain,
        key: String,
        version: SectionEntryVersion,
    },
}

fn write_section(tx: &rusqlite::Transaction<'_>, write: &SectionWrite) -> StoreResult<()> {
    let input = match write {
        SectionWrite::Apply { domain, entry, object } => SectionWriteInput::Apply {
            section: require_section(*domain)?, entry, object: object.as_deref(),
        },
        SectionWrite::Mark { domain, key } => SectionWriteInput::Mark { section: require_section(*domain)?, key },
        SectionWrite::MarkVersion { domain, key, version } => SectionWriteInput::MarkVersion {
            section: require_section(*domain)?, key, version,
        },
    };
    DeviceStore::write_section_entry(tx, &input)
}

#[cfg(test)]
pub(crate) fn write_sections(device: &mut DeviceStore, writes: &[SectionWrite]) -> StoreResult<()> {
    let tx = device.transaction()?;
    for write in writes { write_section(&tx, write)?; }
    tx.commit()?;
    Ok(())
}

impl super::PersistentStore {
    pub(crate) fn write_prepared_server_sections(
        &mut self,
        mut load: impl FnMut(&str, &str, &str, &str, &str) -> crate::server_sync::Result<Option<SectionWrite>>,
    ) -> crate::server_sync::Result<()> {
        let device = self.device_store.as_mut().map_err(|message| StoreError::Store {
            message: message.clone(),
        })?;
        let tx = device.transaction()?;
        let mut statement = self.connection.prepare(
            "SELECT domain,key,action,remote,version FROM server_section_records ORDER BY domain,key",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let (domain, key, action, remote, version): (String, String, String, String, String) =
                (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
            if let Some(write) = load(&domain, &key, &action, &remote, &version)? {
                write_section(&tx, &write)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
