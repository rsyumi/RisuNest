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

pub(crate) fn write_sections(device: &mut DeviceStore, writes: &[SectionWrite]) -> StoreResult<()> {
    let writes = writes.iter().map(|write| Ok(match write {
        SectionWrite::Apply { domain, entry, object } => SectionWriteInput::Apply {
            section: require_section(*domain)?, entry, object: object.as_deref(),
        },
        SectionWrite::Mark { domain, key } => SectionWriteInput::Mark { section: require_section(*domain)?, key },
        SectionWrite::MarkVersion { domain, key, version } => SectionWriteInput::MarkVersion {
            section: require_section(*domain)?, key, version,
        },
    })).collect::<StoreResult<Vec<_>>>()?;
    device.write_section_entries(&writes)
}
