//! PDS-owned capture cache selection. Hydration is planned from the change
//! index before capture admission, and no network runs through a PDS snapshot.
use super::{
    content_change_index, external_storage_state, sync_selection, PersistentStore, StoreError,
    StoreResult,
};
use crate::{
    asset_repository::PayloadCas,
    external_storage::capture::{BackupDependencySpool, CaptureCatalog, DurableCaptureReference},
    local_backup::CancellationProbe,
    server_sync::residency::HydrationSession,
};
use risunest_external_storage_format::format::library_fingerprint_domain;
use rusqlite::{params, OptionalExtension};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub(crate) struct CapturedSnapshot {
    pub id: String,
    pub identity: sync_selection::CaptureIdentity,
    pub catalog: CaptureCatalog,
    pub projected_records: usize,
    pub shared: bool,
}

impl CapturedSnapshot {
    pub(crate) fn durable_reference(
        &self,
        repository_root: &Path,
    ) -> StoreResult<DurableCaptureReference> {
        let reference = self
            .catalog
            .durable_reference(&self.id, repository_root)?;
        if reference.identity != self.identity {
            return Err(invalid("Capture catalog identity differs"));
        }
        Ok(reference)
    }
}

pub(crate) struct CaptureHydration {
    consumer: String,
    identity: sync_selection::CaptureIdentity,
    scope_id: [u8; 32],
    mode: HydrationMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HydrationMode {
    Reuse,
    Incremental,
    Rebuild,
}

pub(crate) struct BackupDependencyClosure {
    pub controls: BTreeMap<String, Vec<u8>>,
    pub payload_bodies: BTreeMap<String, Vec<u8>>,
    pub managed_payloads: BTreeMap<String, u64>,
    pub record_payloads: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackupBodyRole { Control, Payload }

#[derive(Default)]
pub(crate) struct BackupDependencyInventory {
    pub controls: BTreeMap<String, u64>,
    pub payloads: BTreeMap<String, Option<u64>>,
    pub spooled_payloads: BTreeSet<String>,
    pub record_payloads: BTreeSet<String>,
}

impl PersistentStore {
    #[cfg(test)]
    pub(crate) fn lww_backup_dependency_closure(
        &self,
        lease: &str,
        units: &BTreeMap<risunest_sync_wire::unit::UnitKey, risunest_sync_wire::unit::UnitValue>,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<BackupDependencyClosure> {
        let mut controls = BTreeMap::new();
        let mut payload_bodies = BTreeMap::new();
        let inventory = self.lww_backup_dependency_inventory(lease,units,probe,true,&mut |hash,bytes,role| {
            match role {
                BackupBodyRole::Control => {controls.insert(hash.to_owned(),bytes.to_vec());}
                BackupBodyRole::Payload => {payload_bodies.insert(hash.to_owned(),bytes.to_vec());}
            }
            Ok(())
        })?;
        let managed_payloads = inventory.payloads.into_iter().filter(|(hash,_)| !inventory.spooled_payloads.contains(hash))
            .map(|(hash,size)| size.map(|size| (hash,size)).ok_or_else(|| invalid("Backup payload size is unavailable")))
            .collect::<StoreResult<_>>()?;
        Ok(BackupDependencyClosure {controls,payload_bodies,managed_payloads,record_payloads:inventory.record_payloads})
    }

    pub(crate) fn lww_backup_dependency_inventory(
        &self, lease: &str,
        units: &BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
        probe: &dyn CancellationProbe, copy_bodies: bool,
        emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
    ) -> StoreResult<BackupDependencyInventory> {
        check(probe)?;
        if self.lww_backup_unit_values(lease)? != *units {
            return Err(invalid("Backup dependency source differs from the complete pinned units"));
        }
        let source = crate::external_storage::capture::OriginalBackupUnits::capture(&self.repository_root, |emit| {
            for (key,value) in units {emit(key.clone(),value.clone())?;}
            Ok(())
        })?;
        self.lww_backup_dependency_inventory_streamed(lease,&source,probe,copy_bodies,emit)
    }
    fn lww_backup_dependency_inventory_streamed(
        &self,
        lease: &str,
        units: &crate::external_storage::capture::OriginalBackupUnits,
        probe: &dyn CancellationProbe,
        copy_bodies: bool,
        emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
    ) -> StoreResult<BackupDependencyInventory> {
        check(probe)?;
        let reader = self.revision_leases.get(lease)
            .ok_or_else(|| invalid("Backup dependency lease is unavailable"))?;
        let mut large = BTreeSet::new();
        let mut manifests = Vec::new();
        for unit in units.units()? {
            check(probe)?;
            let (key,value) = unit?;
            if super::lww::lww_known_unit_key(&key) {
                if let risunest_sync_wire::unit::UnitValue::Object {descriptor,..} = &value { large.insert(descriptor.object_hash.clone()); }
            }
            if let Some(hash) = message_manifest_hash(&key,&value) { manifests.push(hash); }
        }
        let read_manifest = |manifest: &str| -> StoreResult<Option<Vec<u8>>> {
            let size:Option<i64>=reader.connection.query_row("SELECT length(body) FROM message_page_objects WHERE hash=?1",[manifest],|row|row.get(0)).optional()?;
            if size.is_some_and(|size|size<0) {return Err(invalid("Backup manifest length is invalid"));}
            super::message_pages::object_body(&reader.connection,manifest)
        };
        let pages = OriginalMessagePages::from_manifests(manifests,&read_manifest);
        let load = |hash: &str| -> StoreResult<Option<Vec<u8>>> {
            check(probe)?;
            let length: Option<i64> = reader.connection.query_row(
                "SELECT length(body) FROM message_page_objects WHERE hash=?1", [hash], |row| row.get(0),
            ).optional()?;
            if length.is_some_and(|size| size < 0) {return Err(invalid("Backup control length is invalid"));}
            if let Some(size)=length.filter(|size|*size as u64>risunest_sync_wire::MAX_METADATA_BYTES as u64 && !large.contains(hash)) {
                let page=pages.page(hash)?.ok_or_else(||invalid("Backup control exceeds its bounded metadata limit"))?;
                if page.byte_length.0!=size as u64 {return Err(invalid("Backup message page length differs"));}
            }
            super::message_pages::object_body(&reader.connection, hash)
        };
        let payload_size = |hash: &str| -> StoreResult<Option<u64>> {
            let size: Option<i64> = reader.connection.query_row(
                "SELECT byte_size FROM asset_objects WHERE object_hash=?1", [hash], |row| row.get(0),
            ).optional()?;
            size.map(u64::try_from).transpose().map_err(|_| invalid("Pinned backup payload size is invalid"))
        };
        streamed_unit_dependency_inventory(units.units()?,&load,&payload_size,probe,copy_bodies,emit)
    }
}

#[cfg(test)]
pub(crate) fn original_unit_dependency_closure(
    units: &BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    probe: &dyn CancellationProbe,
) -> StoreResult<BackupDependencyClosure> {
    let mut controls = BTreeMap::new();
    let mut payload_bodies = BTreeMap::new();
    let inventory = original_unit_dependency_inventory(units,read_body,payload_size,probe,true,&mut |hash,bytes,role| {
        match role {
            BackupBodyRole::Control => { controls.insert(hash.to_owned(),bytes.to_vec()); }
            BackupBodyRole::Payload => { payload_bodies.insert(hash.to_owned(),bytes.to_vec()); }
        }
        Ok(())
    })?;
    let managed_payloads = inventory.payloads.into_iter()
        .filter(|(hash,_)| !inventory.spooled_payloads.contains(hash))
        .map(|(hash,size)| size.map(|size| (hash,size)).ok_or_else(|| invalid("Backup payload size is unavailable")))
        .collect::<StoreResult<_>>()?;
    Ok(BackupDependencyClosure {controls,payload_bodies,managed_payloads,record_payloads:inventory.record_payloads})
}

pub(crate) fn hash_backup_body(bytes:&[u8], domain:&'static str) -> String {
    #[cfg(test)]
    crate::persistent_store::hash_work::observe(domain,bytes.len());
    #[cfg(not(test))]
    let _ = domain;
    risunest_sync_wire::hash(bytes)
}

pub(crate) fn published_json_asset_roots(bytes:&[u8])->StoreResult<crate::asset_repository::migration_gc::AssetRootSet> {
    let mut roots=crate::asset_repository::migration_gc::AssetRootSet::default();
    if bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES {
        if bytes.starts_with(risunest_external_storage_format::message_pages::PAGE_PREFIX) {
            if let Ok(messages)=verified_large_message_page(bytes) {
                for value in messages {super::snapshot::observe_json_value(&value,None,&mut roots);}
                return Ok(roots);
            }
        }
        verified_large_unit_body(bytes)?;
    }
    let value:serde_json::Value=serde_json::from_slice(bytes)?;
    super::snapshot::observe_json_value(&value,None,&mut roots);
    Ok(roots)
}

/// A large unit body is the canonical JSON of the value its unit names.
pub(crate) fn verified_large_unit_body(bytes:&[u8])->StoreResult<()> {
    if !risunest_sync_wire::payload_value::is_canonical(bytes) {
        return Err(invalid("Large unit body is not canonical JSON"));
    }
    Ok(())
}

/// Checks a large string control without allocating its encoded or decoded value.
/// Other control shapes retain their existing decoder.
pub(crate) fn verified_string_control(input: &mut (impl std::io::Read + std::io::Seek), hash: &str, length: u64, domain: &'static str) -> StoreResult<bool> {
    use std::io::SeekFrom;
    if length <= super::device_store::plugin_values::text_value::PART_BYTES as u64 { return Ok(false); }
    let mut first = [0]; input.read_exact(&mut first)?; input.seek(SeekFrom::Start(0))?;
    if first[0] != b'"' { return Ok(false); }
    let info = super::device_store::plugin_values::text_value::decode(&mut *input, std::io::sink(), u64::MAX)?;
    #[cfg(test)] super::hash_work::observe(domain, info.encoded_length as usize);
    #[cfg(not(test))] let _ = domain;
    if info.hash != hash || info.encoded_length != length { return Err(invalid("Control body identity differs")); }
    input.seek(SeekFrom::Start(0))?;
    Ok(true)
}

pub(crate) fn published_string_asset_roots(input: &mut (impl std::io::Read + std::io::Seek), hash: &str, length: u64) -> StoreResult<Option<crate::asset_repository::migration_gc::AssetRootSet>> {
    if !verified_string_control(input, hash, length, "native_published_string_identity")? { return Ok(None); }
    let mut roots = crate::asset_repository::migration_gc::AssetRootSet::default();
    let info = super::snapshot::observe_encoded_text(&mut *input, &mut roots)?;
    if info.hash != hash || info.encoded_length != length { return Err(invalid("Control body identity differs")); }
    input.seek(std::io::SeekFrom::Start(0))?;
    Ok(Some(roots))
}

/// Content controls keep their format validation independently of metadata request bounds.
/// A body up to one part may be raw bytes, as the parts of structured plugin values are.
pub(crate) fn verified_oversized_control(bytes:&[u8])->StoreResult<()> {
    if bytes.starts_with(risunest_external_storage_format::message_pages::PAGE_PREFIX) && verified_large_message_page(bytes).is_ok() {return Ok(());}
    if bytes.len() as u64 <= super::device_store::plugin_values::structure::PART_BYTES {return Ok(());}
    verified_large_unit_body(bytes)
}

/// Known kinds other than messages and archive carry Object values only as
/// large units. Unknown kinds stay opaque.
pub(crate) fn is_large_unit(key:&risunest_sync_wire::unit::UnitKey)->bool {
    super::lww::lww_known_unit_key(key) && !matches!(key.components()[0].as_str(),"messages"|"archive")
}

/// The body hashes of the large units in `units`, the only controls besides
/// indivisible message pages that may exceed the metadata bound.
pub(crate) fn large_unit_body_hashes(units:&BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>)->BTreeSet<String> {
    units.iter().filter_map(|(key,value)| large_unit_body_hash(key,value)).collect()
}

pub(crate) fn large_unit_body_hash(key:&risunest_sync_wire::unit::UnitKey,value:&risunest_sync_wire::unit::UnitValue)->Option<String> {
    match value {
        risunest_sync_wire::unit::UnitValue::Object{descriptor,..} if is_large_unit(key) => Some(descriptor.object_hash.clone()),
        _ => None,
    }
}

/// The parts a structured plugin value names: raw bytes that travel as
/// control objects, never as assets.
pub(crate) fn plugin_value_parts<'a>(key:&risunest_sync_wire::unit::UnitKey,value:&'a risunest_sync_wire::unit::UnitValue)->&'a [String] {
    match value {
        risunest_sync_wire::unit::UnitValue::Object{descriptor,..} if key.components()[0]=="plugin-local" => &descriptor.dependencies,
        _ => &[],
    }
}

/// The hash of the message manifest a unit names, when it names one.
pub(crate) fn message_manifest_hash(key:&risunest_sync_wire::unit::UnitKey,value:&risunest_sync_wire::unit::UnitValue)->Option<String> {
    if !super::lww::lww_known_unit_key(key) || key.components()[0]!="messages" {return None;}
    match value {
        risunest_sync_wire::unit::UnitValue::Object{descriptor,..} => Some(descriptor.object_hash.clone()),
        _ => None,
    }
}

pub(crate) fn verified_large_message_page(bytes:&[u8])->StoreResult<Vec<serde_json::Value>> {
    let decoded=risunest_external_storage_format::logical_records::decode_message_page(bytes);
    #[cfg(test)]
    crate::persistent_store::hash_work::decoded("native_large_message_page_decode_identity",bytes,&decoded);
    let messages=decoded.map_err(|_|invalid("Oversized control is not a canonical message page"))?;
    if messages.len()!=1 {return Err(invalid("Oversized message page must contain one indivisible message"));}
    Ok(messages)
}

/// The pages the original message manifests list, read once on first use.
pub(crate) struct OriginalMessagePages<'a> {
    manifests:Vec<String>,
    read_manifest:&'a dyn Fn(&str)->StoreResult<Option<Vec<u8>>>,
    pages:std::cell::OnceCell<BTreeMap<String,Option<risunest_external_storage_format::message_pages::ManifestPage>>>,
}

impl<'a> OriginalMessagePages<'a> {
    pub(crate) fn new(
        units:&'a BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
        read_manifest:&'a dyn Fn(&str)->StoreResult<Option<Vec<u8>>>,
    ) -> Self {
        Self::from_manifests(units.iter().filter_map(|(key,value)|message_manifest_hash(key,value)).collect(),read_manifest)
    }

    /// The pages of the manifests `manifests` names, as `message_manifest_hash` gives them.
    pub(crate) fn from_manifests(manifests:Vec<String>,read_manifest:&'a dyn Fn(&str)->StoreResult<Option<Vec<u8>>>) -> Self {
        Self {manifests,read_manifest,pages:std::cell::OnceCell::new()}
    }

    /// The page `hash` names. Two manifests that describe it differently fail.
    pub(crate) fn page(&self,hash:&str) -> StoreResult<Option<&risunest_external_storage_format::message_pages::ManifestPage>> {
        let pages=match self.pages.get() {
            Some(pages)=>pages,
            None=>{let pages=self.read()?; self.pages.get_or_init(||pages)}
        };
        match pages.get(hash) {
            None=>Ok(None),
            Some(Some(page))=>Ok(Some(page)),
            Some(None)=>Err(invalid("Original message page references differ")),
        }
    }

    fn read(&self) -> StoreResult<BTreeMap<String,Option<risunest_external_storage_format::message_pages::ManifestPage>>> {
        let mut pages=BTreeMap::new();
        for manifest in &self.manifests {
            let bytes=(self.read_manifest)(manifest)?.ok_or_else(||invalid("Original message manifest is unavailable"))?;
            if hash_backup_body(&bytes,"native_backup_manifest_source")!=*manifest {return Err(invalid("Original message manifest identity differs"));}
            let decoded=risunest_external_storage_format::message_pages::MessageManifest::decode(&bytes);
            #[cfg(test)] crate::persistent_store::hash_work::decoded("native_backup_manifest_decode_identity",&bytes,&decoded);
            for page in decoded.map_err(|_|invalid("Original message manifest integrity failed"))?.pages {
                match pages.entry(page.hash.clone()) {
                    std::collections::btree_map::Entry::Vacant(entry)=>{entry.insert(Some(page));}
                    std::collections::btree_map::Entry::Occupied(mut entry)=>{
                        if entry.get().as_ref().is_some_and(|previous|previous!=&page) {entry.insert(None);}
                    }
                }
            }
        }
        Ok(pages)
    }
}

pub(crate) fn original_unit_dependency_inventory(
    units: &BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    probe: &dyn CancellationProbe,
    copy_bodies: bool,
    emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
) -> StoreResult<BackupDependencyInventory> {
    dependency_inventory(units.iter().map(Ok),read_body,payload_size,None,None,probe,copy_bodies,emit)
}

pub(crate) fn original_unit_dependency_inventory_with_ranges(
    units: &BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    ranged: &mut dyn FnMut(&str) -> StoreResult<Option<u64>>,
    probe: &dyn CancellationProbe,
    emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
) -> StoreResult<BackupDependencyInventory> {
    dependency_inventory(units.iter().map(Ok),read_body,payload_size,None,Some(ranged),probe,false,emit)
}

/// The inventory of units read one at a time instead of held in a map.
pub(crate) fn streamed_unit_dependency_inventory(
    units: impl IntoIterator<Item = StoreResult<(risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue)>>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    probe: &dyn CancellationProbe,
    copy_bodies: bool,
    emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
) -> StoreResult<BackupDependencyInventory> {
    dependency_inventory(units,read_body,payload_size,None,None,probe,copy_bodies,emit)
}

/// The inventory without reading any control above the metadata bound.
/// Message pages and large unit bodies are recorded at the lengths their
/// manifests and `large_length` declare, unverified.
pub(crate) fn original_unit_control_lengths(
    units: impl IntoIterator<Item = StoreResult<(risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue)>>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    large_length: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    probe: &dyn CancellationProbe,
) -> StoreResult<BackupDependencyInventory> {
    dependency_inventory(units,read_body,payload_size,Some(large_length),None,probe,false,&mut |_,_,_| Ok(()))
}

fn dependency_inventory<K: std::borrow::Borrow<risunest_sync_wire::unit::UnitKey>, V: std::borrow::Borrow<risunest_sync_wire::unit::UnitValue>>(
    units: impl IntoIterator<Item = StoreResult<(K,V)>>,
    read_body: &dyn Fn(&str) -> StoreResult<Option<Vec<u8>>>,
    payload_size: &dyn Fn(&str) -> StoreResult<Option<u64>>,
    large_length: Option<&dyn Fn(&str) -> StoreResult<Option<u64>>>,
    mut ranged: Option<&mut dyn FnMut(&str) -> StoreResult<Option<u64>>>,
    probe: &dyn CancellationProbe,
    copy_bodies: bool,
    emit: &mut dyn FnMut(&str,&[u8],BackupBodyRole) -> StoreResult<()>,
) -> StoreResult<BackupDependencyInventory> {
    use risunest_sync_wire::{descriptor::visit_reference_tree,unit::UnitValue};
    let mut inventory = BackupDependencyInventory::default();
    let mut required = BTreeSet::new();
    let load = |hash:&str| -> StoreResult<Option<Vec<u8>>> {
        check(probe)?;
        let bytes = read_body(hash)?;
        if bytes.as_ref().is_some_and(|bytes| hash_backup_body(bytes,"native_backup_dependency_source") != hash) {
            return Err(invalid("Backup source body integrity failed"));
        }
        Ok(bytes)
    };
    fn control(inventory:&mut BackupDependencyInventory,emit:&mut dyn FnMut(&str,&[u8],BackupBodyRole)->StoreResult<()>,hash:&str,bytes:&[u8]) -> StoreResult<()> {
        if inventory.controls.insert(hash.to_owned(),bytes.len() as u64).is_none() {
            emit(hash,bytes,BackupBodyRole::Control)?;
        }
        Ok(())
    }
    for unit in units {
        check(probe)?;
        let (key,value) = unit?;
        let (key,value):(&risunest_sync_wire::unit::UnitKey,&risunest_sync_wire::unit::UnitValue) = (key.borrow(),value.borrow());
        #[cfg(test)]
        crate::persistent_store::hash_work::validation(value);
        value.validate().map_err(|_| invalid("Invalid original backup value"))?;
        let UnitValue::Object {descriptor_hash,descriptor} = value else {continue};
        let bytes = descriptor.bytes().map_err(|_| invalid("Invalid original backup descriptor"))?;
        if hash_backup_body(&bytes,"native_backup_dependency_descriptor") != *descriptor_hash {return Err(invalid("Original backup descriptor identity differs"))}
        control(&mut inventory,emit,descriptor_hash,&bytes)?;
        let kind = key.components();
        if let Some(large_length) = large_length.filter(|_| is_large_unit(key)) {
            check(probe)?;
            let length = large_length(&descriptor.object_hash)?.ok_or_else(|| invalid("Pinned backup large unit body is unavailable"))?;
            inventory.controls.insert(descriptor.object_hash.clone(),length);
        } else if is_large_unit(key) {
            check(probe)?;
            let length = match ranged.as_mut() { Some(read) => read(&descriptor.object_hash)?, None => None };
            if let Some(length) = length { inventory.controls.insert(descriptor.object_hash.clone(), length); }
            else {
                let body = read_body(&descriptor.object_hash)?.ok_or_else(|| invalid("Pinned backup large unit body is unavailable"))?;
                if hash_backup_body(&body,"native_backup_large_unit_source") != descriptor.object_hash {return Err(invalid("Pinned backup large unit body identity differs"))}
                verified_large_unit_body(&body)?;
                control(&mut inventory,emit,&descriptor.object_hash,&body)?;
            }
        } else if super::lww::lww_known_unit_key(key) {
            let body = load(&descriptor.object_hash)?.ok_or_else(|| invalid("Pinned backup control is unavailable"))?;
            if kind[0] == "messages" {
                let result = risunest_external_storage_format::message_pages::MessageManifest::decode(&body);
                #[cfg(test)]
                crate::persistent_store::hash_work::decoded("native_backup_manifest_decode_identity",&body,&result);
                let manifest = result.map_err(|_| invalid("Pinned backup message manifest integrity failed"))?;
                for page in manifest.pages {
                    if copy_bodies {
                        check(probe)?;
                        let bytes = read_body(&page.hash)?.ok_or_else(|| invalid("Pinned backup message page is unavailable"))?;
                        if bytes.len() as u64 != page.byte_length.0 {return Err(invalid("Pinned backup message page length differs"))}
                        if hash_backup_body(&bytes,"native_backup_dependency_source")!=page.hash {return Err(invalid("Pinned backup message page identity differs"));}
                        if bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES {verified_large_message_page(&bytes)?;}
                        control(&mut inventory,emit,&page.hash,&bytes)?;
                    } else {
                        inventory.controls.insert(page.hash,page.byte_length.0);
                    }
                }
            } else {
                let archived: super::archive::ArchivedObject = serde_json::from_slice(&body)?;
                let mut shared = archived.clone();
                shared.object_hash = shared.shared_object_hash.clone();
                shared.asset_hashes = shared.shared_asset_hashes.clone();
                let encoded = risunest_sync_wire::payload_value::encode(&serde_json::to_value(&shared)?)
                    .map_err(|_| invalid("Invalid backup archive metadata"))?;
                if encoded != body {return Err(invalid("Backup archive metadata differs from its shared value"))}
                let mut expected = risunest_sync_wire::descriptor::RecordDescriptor::content(
                    hash_backup_body(&encoded,"native_backup_archive_metadata"));
                let mut roots = shared.object_roots().map(str::to_owned).collect::<Vec<_>>();
                roots.sort(); roots.dedup();
                if risunest_sync_wire::descriptor::inline_references(&roots).map_err(|_| invalid("Invalid backup archive roots"))? {
                    expected.dependencies = roots;
                } else {
                    let tree = risunest_sync_wire::descriptor::build_reference_tree(&roots,false);
                    #[cfg(test)]
                    crate::persistent_store::hash_work::reference_creation(&tree);
                    expected.dependency_root = tree.map_err(|_| invalid("Invalid backup archive roots"))?.0;
                }
                if expected != *descriptor {return Err(invalid("Backup archive descriptor differs from its metadata"))}
            }
            control(&mut inventory,emit,&descriptor.object_hash,&body)?;
        } else {
            required.insert(descriptor.object_hash.clone());
            inventory.record_payloads.insert(descriptor.object_hash.clone());
        }
        required.extend(descriptor.dependencies.iter().cloned());
        for (root,relations) in [(&descriptor.dependency_root,false),(&descriptor.relation_root,true)] {
            let Some(root) = root else {continue};
            let mut source_error = None;
            let visited = visit_reference_tree(root,relations,|hash| {
                let result = (|| {
                    let bytes = load(hash)?.ok_or_else(|| invalid("Backup reference control is unavailable"))?;
                    control(&mut inventory,emit,hash,&bytes)?;
                    Ok(bytes)
                })();
                match result {
                    Ok(bytes) => {
                        #[cfg(test)]
                        crate::persistent_store::hash_work::observe("native_backup_reference_verify",bytes.len());
                        Ok(bytes)
                    },
                    Err(error) => { source_error = Some(error); Err(risunest_sync_wire::WireError("backup-reference-source-failed")) }
                }
            },|value,page| {
                if !page && !relations {required.insert(value.to_owned());}
                Ok(())
            });
            #[cfg(test)]
            if visited.is_err() {crate::persistent_store::hash_work::incomplete("native_backup_reference_verify");}
            check(probe)?;
            if let Some(error) = source_error { return Err(error); }
            visited.map_err(|_| invalid("Backup reference tree integrity failed"))?;
        }
    }
    for hash in required {
        check(probe)?;
        if inventory.controls.contains_key(&hash) {continue}
        if copy_bodies {
            if let Some(bytes) = load(&hash)? {
                emit(&hash,&bytes,BackupBodyRole::Payload)?;
                inventory.payloads.insert(hash.clone(),Some(bytes.len() as u64));
                inventory.spooled_payloads.insert(hash);
                continue;
            }
        }
        let size = payload_size(&hash)?;
        if copy_bodies && size.is_none() {return Err(invalid("Backup payload metadata is unavailable"))}
        inventory.payloads.insert(hash,size);
    }
    Ok(inventory)
}

struct Candidate {
    id: String,
    identity: sync_selection::CaptureIdentity,
    scope_id: String,
    manifest_hash: String,
    path: Option<PathBuf>,
    file_hash: Option<String>,
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
struct Dependency {
    hash: String,
    manifest: bool,
}

const CODEC: &str = "logical-v1";
const PAGE: i64 = 128;
const GC_SCAN_LIMIT: i64 = 128;
/// Registrations one cache cleanup removes at most.
pub(super) const GC_DELETE_LIMIT: usize = 16;
const GC_REFERENCE_LIMIT: i64 = 32;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}

fn hydration_error(error: crate::server_sync::SyncError) -> StoreError {
    if error.code == "cancelled" {
        invalid("External capture cancelled")
    } else {
        invalid(&format!("External capture hydration failed: {}", error.code))
    }
}

fn check(probe: &dyn CancellationProbe) -> StoreResult<()> {
    if probe.is_cancelled() {
        Err(invalid("External capture cancelled"))
    } else {
        Ok(())
    }
}

fn decode_hash(value: &str, message: &str) -> StoreResult<[u8; 32]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| invalid(message))
}

impl PersistentStore {
    fn capture_candidate(
        &self,
        identity: &sync_selection::CaptureIdentity,
        scope: &str,
    ) -> StoreResult<Option<Candidate>> {
        let candidate: Option<(String, String, Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT c.id,c.manifest_hash,f.catalog_path,f.file_hash FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.identity=?1 AND c.scope_id=?2 AND c.codec_id=?3 AND c.device_capture_id=''",
                params![serde_json::to_string(identity)?, scope, CODEC],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        Ok(
            candidate.map(|(id, manifest_hash, path, file_hash)| Candidate {
                id,
                identity: identity.clone(),
                scope_id: scope.into(),
                manifest_hash,
                path: path.map(PathBuf::from),
                file_hash,
            }),
        )
    }

    fn capture_candidate_by_id(&self, id: &str) -> StoreResult<Option<Candidate>> {
        let candidate: Option<(String, String, String, Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT c.identity,c.scope_id,c.manifest_hash,f.catalog_path,f.file_hash FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.id=?1 AND c.codec_id=?2 AND (c.device_capture_id='' OR c.device_capture_id=f.file_hash)",
                params![id, CODEC],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        candidate
            .map(|(identity, scope_id, manifest_hash, path, file_hash)| {
                Ok(Candidate {
                    id: id.into(),
                    identity: serde_json::from_str(&identity)?,
                    scope_id,
                    manifest_hash,
                    path: path.map(PathBuf::from),
                    file_hash,
                })
            })
            .transpose()
    }

    fn reopen_candidate(&self, candidate: &Candidate) -> StoreResult<CaptureCatalog> {
        let path = candidate
            .path
            .as_ref()
            .ok_or_else(|| invalid("Capture cache file is missing"))?;
        let file_hash = decode_hash(
            candidate
                .file_hash
                .as_deref()
                .ok_or_else(|| invalid("Capture cache hash is missing"))?,
            "Invalid capture cache hash",
        )?;
        let scope = decode_hash(&candidate.scope_id, "Invalid capture scope")?;
        let root = self
            .repository_root
            .join("external-storage")
            .canonicalize()?;
        if !path.canonicalize()?.starts_with(&root) {
            return Err(invalid("Capture cache escaped its native directory"));
        }
        let catalog = CaptureCatalog::reopen(path, &root, &file_hash, &candidate.identity)?;
        if hex::encode(catalog.content_fingerprint(&scope)?) != candidate.manifest_hash {
            return Err(invalid("Capture cache content differs"));
        }
        Ok(catalog)
    }

    /// Reopens the exact immutable capture owned by a durable job. Old capture
    /// revisions remain valid and are not compared with the current PDS head.
    pub(crate) fn reopen_external_capture(
        &self,
        capture_id: &str,
    ) -> StoreResult<CapturedSnapshot> {
        let candidate = self
            .capture_candidate_by_id(capture_id)?
            .ok_or_else(|| invalid("Pinned capture is unavailable"))?;
        if !external_storage_state::capture_has_consumers(&self.connection, capture_id)? {
            return Err(invalid("Capture is not pinned by a durable owner"));
        }
        let catalog = self.reopen_candidate(&candidate)?;
        Ok(CapturedSnapshot {
            id: candidate.id,
            identity: candidate.identity,
            catalog,
            projected_records: 0,
            shared: true,
        })
    }

    fn invalidate_unreferenced_candidate(&mut self, candidate: &Candidate) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        if external_storage_state::capture_has_consumers(&tx, &candidate.id)? {
            return Err(invalid("Referenced capture cache is unavailable"));
        }
        tx.execute(
            "UPDATE content_change_consumers SET rebuild_required=1 WHERE generation=?1 AND revision=?2",
            params![candidate.identity.generation, candidate.identity.revision],
        )?;
        tx.execute(
            "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
            [&candidate.id],
        )?;
        tx.execute(
            "DELETE FROM external_storage_captures WHERE id=?1",
            [&candidate.id],
        )?;
        tx.commit()?;
        if let Some(path) = &candidate.path {
            self.remove_capture_file(path);
        }
        Ok(())
    }

    fn validated_candidate(
        &mut self,
        identity: &sync_selection::CaptureIdentity,
        scope: &str,
    ) -> StoreResult<Option<(Candidate, CaptureCatalog)>> {
        let Some(candidate) = self.capture_candidate(identity, scope)? else {
            return Ok(None);
        };
        match self.reopen_candidate(&candidate) {
            Ok(catalog) => Ok(Some((candidate, catalog))),
            Err(_) => {
                self.invalidate_unreferenced_candidate(&candidate)?;
                Ok(None)
            }
        }
    }

    fn remove_capture_file(&self, path: &Path) {
        let captures = self
            .repository_root
            .join("external-storage")
            .join("captures");
        let Some(parent) = path.parent() else { return };
        if path.file_name().and_then(|name| name.to_str()) != Some("capture.sqlite") {
            return;
        }
        let (Ok(captures), Ok(parent)) = (captures.canonicalize(), parent.canonicalize()) else {
            return;
        };
        if parent == captures || !parent.starts_with(&captures) {
            return;
        }
        if fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata)
        }) && fs::remove_file(path).is_ok()
        {
            let _ = crate::trust_boundary::sync_directory(&parent);
            let _ = fs::remove_dir(&parent);
            let _ = crate::trust_boundary::sync_directory(&captures);
        }
    }

    fn cleanup_terminal_capture_references(&mut self) -> StoreResult<()> {
        let tx = self.connection.transaction()?;
        let terminal = {
            let mut query = tx.prepare(
                "SELECT r.capture_id,r.job_id FROM external_storage_capture_refs r JOIN external_storage_jobs j ON j.id=r.job_id WHERE j.phase IN ('complete','cancelled','stale') ORDER BY r.capture_id,r.job_id LIMIT ?1",
            )?;
            let rows = query.query_map([GC_REFERENCE_LIMIT], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (capture, job) in terminal {
            tx.execute(
                "DELETE FROM external_storage_capture_refs WHERE capture_id=?1 AND job_id=?2",
                params![capture, job],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// How many registrations were removed, which is when bodies may have
    /// become unreferenced. At `GC_DELETE_LIMIT` more may be left.
    pub(super) fn cleanup_capture_cache(&mut self, keep: &str) -> StoreResult<usize> {
        self.cleanup_terminal_capture_references()?;
        let mut removed = 0;
        let mut paths = Vec::new();
        {
            let tx = self.connection.transaction()?;
            let candidates = {
                let mut query = tx.prepare(
                    "SELECT c.id,c.identity,f.catalog_path FROM external_storage_captures c LEFT JOIN external_storage_capture_files f ON f.capture_id=c.id WHERE c.id!=?1 AND NOT EXISTS(SELECT 1 FROM external_storage_capture_refs r WHERE r.capture_id=c.id) ORDER BY c.rowid LIMIT ?2",
                )?;
                let rows = query.query_map(params![keep, GC_SCAN_LIMIT], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (id, encoded, path) in candidates {
                if paths.len() == GC_DELETE_LIMIT {
                    break;
                }
                let needed = match serde_json::from_str::<sync_selection::CaptureIdentity>(&encoded) {
                    Ok(identity) => tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM content_change_consumers WHERE generation=?1 AND revision=?2 AND rebuild_required=0)",
                        params![identity.generation, identity.revision],
                        |row| row.get::<_, bool>(0),
                    )?,
                    Err(_) => false,
                };
                if needed {
                    continue;
                }
                tx.execute(
                    "DELETE FROM external_storage_capture_files WHERE capture_id=?1",
                    [&id],
                )?;
                tx.execute("DELETE FROM external_storage_captures WHERE id=?1", [&id])?;
                removed += 1;
                if let Some(path) = path {
                    paths.push(PathBuf::from(path));
                }
            }
            tx.commit()?;
        }
        for path in paths {
            self.remove_capture_file(&path);
        }
        Ok(removed)
    }

    fn hydrate_hashes(
        &self,
        session: &mut HydrationSession,
        hashes: &[String],
        probe: &dyn CancellationProbe,
    ) -> StoreResult<()> {
        let cancellation = || {
            if probe.is_cancelled() {
                Err(crate::server_sync::SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        };
        for group in hashes.chunks(64) {
            check(probe)?;
            let unavailable = session
                .hydrate_many(group, &cancellation)
                .map_err(hydration_error)?;
            if !unavailable.is_empty() {
                return Err(invalid("Required capture payload is unavailable"));
            }
        }
        Ok(())
    }

    /// Only owner manifests, which projection reads to enumerate payloads. A
    /// payload is captured by its hash and size, whether its body is held
    /// locally or only through custody.
    fn hydrate_dependencies(
        &self,
        session: &mut HydrationSession,
        dependencies: BTreeSet<Dependency>,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<()> {
        let hashes = dependencies
            .into_iter()
            .filter(|item| item.manifest)
            .map(|item| item.hash)
            .collect::<Vec<_>>();
        self.hydrate_hashes(session, &hashes, probe)
    }

    fn full_dependency_page(&self, after: &str) -> StoreResult<BTreeSet<Dependency>> {
        let generation = super::active_generation(&self.connection)?;
        let mut query = self.connection.prepare(
            "WITH dependencies(hash,manifest) AS (
                SELECT object_hash,0 FROM asset_aliases WHERE generation=?1 AND object_hash IS NOT NULL
                UNION SELECT manifest_hash,1 FROM asset_owner_heads WHERE generation=?1 AND manifest_hash IS NOT NULL
                UNION SELECT json_extract(archived_object,'$.objectHash'),0
                      FROM characters WHERE generation=?1 AND archived_object IS NOT NULL
                UNION SELECT assets.value,0 FROM characters, json_each(
                    json_extract(characters.archived_object,'$.assetHashes')
                ) AS assets
                      WHERE generation=?1 AND archived_object IS NOT NULL
             ) SELECT hash,max(manifest) FROM dependencies WHERE hash>?2 GROUP BY hash ORDER BY hash LIMIT ?3",
        )?;
        let rows = query.query_map(params![generation, after, PAGE], |row| {
            Ok(Dependency {
                hash: row.get(0)?,
                manifest: row.get::<_, bool>(1)?,
            })
        })?;
        let dependencies = rows.collect::<Result<_, _>>()?;
        Ok(dependencies)
    }

    fn owner_dependencies(
        &self,
        generation: &str,
        character: Option<&str>,
        dependencies: &mut BTreeSet<Dependency>,
    ) -> StoreResult<()> {
        let mut query = match character {
            Some(_) => self.connection.prepare(
                "SELECT manifest_hash FROM asset_owner_heads WHERE generation=?1 AND owner_kind='character-additional-assets' AND owner_locator=?2 AND manifest_hash IS NOT NULL",
            )?,
            None => self.connection.prepare(
                "SELECT manifest_hash FROM asset_owner_heads WHERE generation=?1 AND owner_kind IN ('root-module-assets','persona-embedded-module-assets') AND manifest_hash IS NOT NULL AND ?2=''",
            )?,
        };
        let hashes = query
            .query_map(params![generation, character.unwrap_or("")], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(query);
        dependencies.extend(hashes.into_iter().map(|hash| Dependency {
            hash,
            manifest: true,
        }));
        Ok(())
    }

    fn record_dependencies(
        &self,
        generation: &str,
        key: &content_change_index::ContentKey,
    ) -> StoreResult<BTreeSet<Dependency>> {
        let mut dependencies = BTreeSet::new();
        let direct = match key.kind.as_str() {
            "asset" | "inlay" => self.connection.query_row(
                "SELECT object_hash FROM asset_aliases WHERE generation=?1 AND kind=?2 AND logical_key=?3",
                params![generation, key.kind, key.key1], |row| row.get::<_, Option<String>>(0),
            ).optional()?.flatten(),
            _ => None,
        };
        if let Some(hash) = direct {
            dependencies.insert(Dependency {
                hash,
                manifest: false,
            });
        }
        match key.kind.as_str() {
            "root" => self.owner_dependencies(generation, None, &mut dependencies)?,
            "character" => {
                self.owner_dependencies(generation, Some(&key.key1), &mut dependencies)?;
                let archived: Option<String> = self
                    .connection
                    .query_row(
                        "SELECT archived_object FROM characters
                         WHERE generation=?1 AND character_id=?2",
                        params![generation, key.key1],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(archived) = archived {
                    let archived: super::archive::ArchivedObject = serde_json::from_str(&archived)?;
                    dependencies.insert(Dependency {
                        hash: archived.object_hash,
                        manifest: false,
                    });
                    dependencies.extend(archived.asset_hashes.into_iter().map(|hash| Dependency {
                        hash,
                        manifest: false,
                    }));
                }
            }
            "owner" if key.key1 == "character-additional-assets" => {
                self.owner_dependencies(generation, Some(&key.key2), &mut dependencies)?
            }
            "owner"
                if matches!(
                    key.key1.as_str(),
                    "root-module-assets" | "persona-embedded-module-assets"
                ) =>
            {
                self.owner_dependencies(generation, None, &mut dependencies)?
            }
            _ => {}
        }
        Ok(dependencies)
    }

    fn incremental_dependency_page(
        &self,
        identity: &sync_selection::CaptureIdentity,
        after_revision: i64,
        after: Option<&content_change_index::ContentKey>,
    ) -> StoreResult<(Vec<content_change_index::ContentKey>, BTreeSet<Dependency>)> {
        let (kind, key1, key2) = after
            .map(|key| (key.kind.as_str(), key.key1.as_str(), key.key2.as_str()))
            .unwrap_or(("", "", ""));
        let keys = {
            let mut query = self.connection.prepare(
                "SELECT kind,key1,key2 FROM content_changes WHERE generation=?1 AND revision>?2 AND revision<=?3 AND (kind,key1,key2)>(?4,?5,?6) ORDER BY kind,key1,key2 LIMIT ?7",
            )?;
            let rows = query.query_map(
                params![
                    identity.generation,
                    after_revision,
                    identity.revision,
                    kind,
                    key1,
                    key2,
                    PAGE
                ],
                |row| {
                    Ok(content_change_index::ContentKey {
                        kind: row.get(0)?,
                        key1: row.get(1)?,
                        key2: row.get(2)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut dependencies = BTreeSet::new();
        for key in &keys {
            dependencies.extend(self.record_dependencies(&identity.generation, key)?);
        }
        Ok((keys, dependencies))
    }

    /// Call before acquiring file(true). Network hydration is restricted to
    /// the owner manifests of changed records unless a full rebuild is
    /// unavoidable.
    pub(crate) fn hydrate_external_capture_dependencies(
        &self,
        consumer: &str,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CaptureHydration> {
        if consumer.is_empty()
        {
            return Err(invalid(
                "Library capture requires the library and referenced asset scope",
            ));
        }
        check(probe)?;
        let identity = sync_selection::identity(&self.connection)?;
        let scope_id = library_fingerprint_domain();
        let scope_hex = hex::encode(scope_id);
        let mut mode = if self
            .capture_candidate(&identity, &scope_hex)?
            .is_some_and(|candidate| self.reopen_candidate(&candidate).is_ok())
        {
            HydrationMode::Reuse
        } else {
            let cursor: Option<(String, i64, bool)> = self.connection.query_row(
                "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
                [consumer], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).optional()?;
            match cursor {
                Some((generation, revision, false)) if generation == identity.generation => {
                    let mut previous = identity.clone();
                    previous.revision = revision;
                    if self
                        .capture_candidate(&previous, &scope_hex)?
                        .is_some_and(|candidate| self.reopen_candidate(&candidate).is_ok())
                    {
                        HydrationMode::Incremental
                    } else {
                        HydrationMode::Rebuild
                    }
                }
                _ => HydrationMode::Rebuild,
            }
        };
        if crate::server_sync::residency::Residency::exists(&self.repository_root) {
            let mut session = HydrationSession::new(&self.repository_root, None)
                .map_err(hydration_error)?;
            match mode {
                HydrationMode::Reuse => {}
                HydrationMode::Incremental => {
                    let after_revision: i64 = self.connection.query_row(
                        "SELECT revision FROM content_change_consumers WHERE id=?1 AND generation=?2 AND rebuild_required=0",
                        params![consumer, identity.generation], |row| row.get(0),
                    )?;
                    let full: bool = self.connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM content_changes WHERE generation=?1 AND kind='full' AND revision>?2 AND revision<=?3)",
                        params![identity.generation, after_revision, identity.revision], |row| row.get(0),
                    )?;
                    if full {
                        self.hydrate_full_dependencies(&mut session, probe)?;
                        mode = HydrationMode::Rebuild;
                    } else {
                        let mut after = None;
                        loop {
                            let (keys, dependencies) = self.incremental_dependency_page(
                                &identity,
                                after_revision,
                                after.as_ref(),
                            )?;
                            if keys.is_empty() {
                                break;
                            }
                            after = keys.last().cloned();
                            self.hydrate_dependencies(&mut session, dependencies, probe)?;
                        }
                    }
                }
                HydrationMode::Rebuild => self.hydrate_full_dependencies(&mut session, probe)?,
            }
        }
        if sync_selection::identity(&self.connection)? != identity {
            return Err(invalid("Library changed during capture hydration"));
        }
        Ok(CaptureHydration {
            consumer: consumer.into(),
            identity,
            scope_id,
            mode,
        })
    }

    fn hydrate_full_dependencies(
        &self,
        session: &mut HydrationSession,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<()> {
        let mut after = String::new();
        loop {
            check(probe)?;
            let dependencies = self.full_dependency_page(&after)?;
            if dependencies.is_empty() {
                return Ok(());
            }
            after = dependencies
                .last()
                .expect("nonempty dependency page")
                .hash
                .clone();
            self.hydrate_dependencies(session, dependencies, probe)?;
        }
    }

    /// Call before acquiring file(true), like the dependency hydration. Makes a
    /// capture serve local recovery: every payload it names is fetched through
    /// custody when it is not held locally, and then held at its recorded
    /// length and content. A local body that differs is not replaced, so the
    /// capture fails rather than serving it.
    pub(crate) fn complete_external_capture(
        &self,
        capture_id: &str,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<()> {
        let capture = self.reopen_external_capture(capture_id)?;
        let cas = PayloadCas::new(&self.repository_root)?;
        let mut query = capture.catalog.db.prepare(
            "SELECT DISTINCT d.hash,d.bytes FROM dependencies d LEFT JOIN generated g ON g.hash=d.hash WHERE g.hash IS NULL ORDER BY d.hash",
        )?;
        let payloads = query
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(query);
        let mut session = HydrationSession::new(&self.repository_root, None)
            .map_err(hydration_error)?;
        for group in payloads.chunks(64) {
            check(probe)?;
            let mut missing = Vec::new();
            for (hash, bytes) in group {
                if cas.stat_object(hash)? != u64::try_from(*bytes).ok() {
                    missing.push(hash.clone());
                }
            }
            self.hydrate_hashes(&mut session, &missing, probe)?;
        }
        let reference = capture.durable_reference(&self.repository_root)?;
        crate::external_storage::capture::validate_recovery_sources(
            [&reference],
            &self.repository_root,
            probe,
        )?;
        Ok(())
    }

    /// Call under file(true), using a hydration token prepared before admission.
    pub(crate) fn capture_external_library(
        &mut self,
        consumer: &str,
        hydration: &CaptureHydration,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CapturedSnapshot> {
        let identity = sync_selection::identity(&self.connection)?;
        let scope_id = library_fingerprint_domain();
        if hydration.consumer != consumer
            || hydration.identity != identity
            || hydration.scope_id != scope_id
        {
            return Err(invalid("Capture hydration is stale"));
        }
        check(probe)?;
        self.cleanup_terminal_capture_references()?;
        let scope_hex = hex::encode(scope_id);
        let root = self.repository_root.join("external-storage");
        if let Some((candidate, catalog)) = self.validated_candidate(&identity, &scope_hex)? {
            let tx = self.connection.transaction()?;
            content_change_index::commit_cursor(
                &tx,
                consumer,
                &identity.generation,
                identity.revision,
            )?;
            content_change_index::prune(&tx)?;
            tx.commit()?;
            if !probe.is_cancelled() && self.cleanup_capture_cache(&candidate.id)? > 0 {
                self.collect_released_external_content();
            }
            return Ok(CapturedSnapshot {
                id: candidate.id,
                identity,
                catalog,
                projected_records: 0,
                shared: true,
            });
        }
        let cursor: Option<(String, i64, bool)> = self.connection.query_row(
            "SELECT generation,revision,rebuild_required FROM content_change_consumers WHERE id=?1",
            [consumer], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).optional()?;
        let previous_identity = match cursor {
            Some((generation, revision, false)) if generation == identity.generation => {
                let mut previous = identity.clone();
                previous.revision = revision;
                Some(previous)
            }
            _ => None,
        };
        let previous = match previous_identity {
            Some(previous) => {
                self.validated_candidate(&previous, &scope_hex)?
                    .map(|(candidate, catalog)| {
                        drop(catalog);
                        candidate
                    })
            }
            None => None,
        };
        if previous.is_none() {
            let tx = self.connection.transaction()?;
            content_change_index::require_rebuild(&tx, consumer)?;
            tx.commit()?;
            if hydration.mode != HydrationMode::Rebuild {
                return Err(invalid("Capture hydration must be retried"));
            }
        }
        let previous_hash = previous
            .as_ref()
            .map(|candidate| {
                decode_hash(
                    candidate
                        .file_hash
                        .as_deref()
                        .ok_or_else(|| invalid("Capture cache hash is missing"))?,
                    "Invalid capture cache hash",
                )
            })
            .transpose()?;
        let id = uuid::Uuid::new_v4().to_string();
        let directory = root.join("captures").join(&id);
        let prior = previous
            .as_ref()
            .zip(previous_hash.as_ref())
            .map(|(candidate, hash)| {
                (
                    candidate
                        .path
                        .as_ref()
                        .expect("validated capture path")
                        .as_path(),
                    hash,
                )
            });
        let mut catalog = CaptureCatalog::create(&directory, &root, prior)?;
        let prepared = self.prepare_content_capture(&id, consumer, identity.revision)?;
        let projected_records = prepared.project(&mut catalog, probe)?;
        check(probe)?;
        let capture_id = prepared.register(self, &catalog, &scope_id, CODEC)?;
        if !probe.is_cancelled() && self.cleanup_capture_cache(&capture_id)? > 0 {
            self.collect_released_external_content();
        }
        Ok(CapturedSnapshot {
            id: capture_id,
            identity,
            catalog,
            projected_records,
            shared: false,
        })
    }

    /// On success or failure after admission, the supplied lease is consumed.
    pub(crate) fn capture_external_library_from_lease(
        &mut self,
        consumer: &str,
        hydration: &CaptureHydration,
        lease: &str,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CapturedSnapshot> {
        self.capture_external_library_from_lease_with_sections(consumer, hydration, lease, Vec::new(), probe)
    }

    pub(crate) fn capture_external_library_from_lease_with_sections(
        &mut self,
        consumer: &str,
        hydration: &CaptureHydration,
        lease: &str,
        sections: Vec<crate::external_storage::sections::CapturedSection>,
        probe: &dyn CancellationProbe,
    ) -> StoreResult<CapturedSnapshot> {
        let reader = self.revision_leases.get(lease)
            .ok_or_else(|| invalid("Capture lease is unavailable"))?;
        let identity = sync_selection::identity(&reader.connection)?;
        let scope = library_fingerprint_domain();
        if hydration.consumer != consumer || hydration.identity != identity || hydration.scope_id != scope {
            return Err(invalid("Capture hydration differs from its pinned lease"));
        }
        check(probe)?;
        let id = uuid::Uuid::new_v4().to_string();
        let root = self.repository_root.join("external-storage");
        let directory = root.join("captures").join(&id);
        let units = crate::external_storage::capture::OriginalBackupUnits::capture(&directory, |emit| self.lww_visit_backup_units(lease,probe,emit))?;
        let mut spool = BackupDependencySpool::new(&directory)?;
        let inventory = self.lww_backup_dependency_inventory_streamed(lease, &units, probe, true,
            &mut |hash, bytes, role| Ok(spool.push(hash, bytes, role)?))?;
        check(probe)?;
        spool.seal()?;
        let prepared = self.prepare_content_capture_from_lease(&id, consumer, lease)?;
        let mut catalog = CaptureCatalog::create(&directory, &root, None)?;
        catalog.install_streamed_backup_inputs(units, inventory, spool, sections)?;
        let projected_records = prepared.project(&mut catalog, probe)?;
        check(probe)?;
        let (manifest,_,_) = catalog.manifest()?;
        let id = prepared.register_with_device_capture_id(self, &catalog, &scope, CODEC, &hex::encode(manifest))?;
        Ok(CapturedSnapshot { id, identity, catalog, projected_records, shared: false })
    }

    pub(crate) fn retain_external_capture(
        &mut self,
        capture: &str,
        owner: &str,
    ) -> StoreResult<()> {
        if owner.is_empty() {
            return Err(invalid("Missing capture owner"));
        }
        let tx = self.connection.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_storage_captures WHERE id=?1)",
            [capture],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(invalid("Capture does not exist"));
        }
        tx.execute(
            "INSERT OR IGNORE INTO external_storage_capture_refs VALUES(?1,?2)",
            params![capture, owner],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn release_external_capture(
        &mut self,
        capture: &str,
        owner: &str,
    ) -> StoreResult<bool> {
        let tx = self.connection.transaction()?;
        let active: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_storage_jobs WHERE id=?1 AND phase NOT IN ('complete','cancelled','stale'))",
            [owner], |row| row.get(0),
        )?;
        if active {
            return Err(invalid("Active job still owns its capture"));
        }
        tx.execute(
            "DELETE FROM external_storage_capture_refs WHERE capture_id=?1 AND job_id=?2",
            params![capture, owner],
        )?;
        let remaining = external_storage_state::capture_has_consumers(&tx, capture)?;
        tx.commit()?;
        Ok(!remaining)
    }


}

#[cfg(test)]
pub(crate) fn large_metadata_fixture() -> (BTreeMap<risunest_sync_wire::unit::UnitKey,risunest_sync_wire::unit::UnitValue>,BTreeMap<String,Vec<u8>>,Vec<String>) {
    use risunest_external_storage_format::message_pages::{MessageManifest,ManifestPage,MANIFEST_SCHEMA};
    use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
    let page=risunest_external_storage_format::logical_records::encode_message_page(&[serde_json::json!({"role":"user","data":"synthetic"})]).unwrap();
    let manifest=MessageManifest {schema:MANIFEST_SCHEMA.into(),message_count:9500u64.into(),pages:vec![ManifestPage {hash:page.hash.clone(),message_count:1,byte_length:page.size.into()};9500]}.encode().unwrap();
    let assets=(0..8000).map(|index|risunest_sync_wire::hash(format!("synthetic-asset-{index}").as_bytes())).collect::<Vec<_>>();
    let archived=super::archive::ArchivedObject {object_hash:"f".repeat(64),shared_object_hash:"f".repeat(64),archived_at:1,conversation_count:1,message_count:1,
        asset_hashes:assets.clone(),shared_asset_hashes:assets,identity_remap:Vec::new()};
    let archive=risunest_sync_wire::payload_value::encode(&serde_json::to_value(&archived).unwrap()).unwrap();
    let archive_hash=risunest_sync_wire::hash(&archive);
    assert!(manifest.bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES);
    assert!(archive.len()>risunest_sync_wire::MAX_METADATA_BYTES);
    let mut roots=archived.object_roots().map(str::to_owned).collect::<Vec<_>>();roots.sort();roots.dedup();
    let (tree,nodes)=risunest_sync_wire::descriptor::build_reference_tree(&roots,false).unwrap();
    let mut descriptor=RecordDescriptor::content(archive_hash.clone());descriptor.dependency_root=tree;
    let units=BTreeMap::from([
        (UnitKey::new(&["archive","synthetic-character"]).unwrap(),UnitValue::object(descriptor).unwrap()),
        (UnitKey::new(&["messages","synthetic-character","synthetic-chat"]).unwrap(),UnitValue::object(RecordDescriptor::content(manifest.hash.clone())).unwrap()),
    ]);
    let mut bodies=BTreeMap::from([(manifest.hash.clone(),manifest.bytes),(archive_hash.clone(),archive),(page.hash,page.bytes)]);
    for (hash,body) in nodes {bodies.insert(hash,body);}
    (units,bodies,vec![archive_hash,manifest.hash])
}

#[cfg(test)]
mod hydration_tests {
    use super::*;

    #[test]
    fn archive_asset_metadata_and_long_message_manifests_survive_capture_spooling() {
        let (units,bodies,large)=large_metadata_fixture();
        let closure=original_unit_dependency_closure(&units,&|hash|Ok(bodies.get(hash).cloned()),&|_|Ok(Some(1)),&crate::local_backup::NeverCancelled).unwrap();
        let directory=tempfile::tempdir().unwrap();
        let mut spool=BackupDependencySpool::new(directory.path()).unwrap();
        for hash in &large {
            assert_eq!(closure.controls[hash],bodies[hash]);
            spool.push(hash,&closure.controls[hash],BackupBodyRole::Control).unwrap();
        }
        spool.seal().unwrap();
        for hash in &large {assert_eq!(spool.control(hash).unwrap().unwrap(),bodies[hash]);}
        let mut corrupted=bodies.clone();corrupted.get_mut(&large[0]).unwrap().push(b' ');
        assert!(original_unit_dependency_closure(&units,&|hash|Ok(corrupted.get(hash).cloned()),&|_|Ok(Some(1)),&crate::local_backup::NeverCancelled).is_err());
    }

    #[test]
    fn only_a_verified_indivisible_message_page_can_exceed_metadata_limit() {
        let message=serde_json::json!({"role":"user","content":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)});
        let encoded=risunest_external_storage_format::logical_records::encode_message_page(&[message.clone()]).unwrap();
        assert!(encoded.bytes.len()>risunest_sync_wire::MAX_METADATA_BYTES);
        assert_eq!(verified_large_message_page(&encoded.bytes).unwrap(),vec![message.clone()]);
        assert!(published_json_asset_roots(&encoded.bytes).is_ok());
        let mut damaged=encoded.bytes.clone();damaged[0]=b'[';
        assert!(verified_large_message_page(&damaged).is_err());
        let multiple=risunest_external_storage_format::logical_records::encode_message_page(&[message,serde_json::json!({})]).unwrap();
        assert!(verified_large_message_page(&multiple.bytes).is_err());
        assert!(verified_large_message_page(&serde_json::to_vec(&serde_json::json!({"content":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)})).unwrap()).is_err());
    }

    #[test]
    fn oversized_control_decodes_only_a_body_with_the_message_page_prefix() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work};
        let message=serde_json::json!({"role":"user","content":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)});
        let page=risunest_external_storage_format::logical_records::encode_message_page(&[message.clone()]).unwrap().bytes;
        let large=risunest_sync_wire::payload_value::encode(&message).unwrap();
        assert!(large.len()>risunest_sync_wire::MAX_METADATA_BYTES);
        reset_hash_work();
        verified_oversized_control(&page).unwrap();
        let work=take_hash_work();
        assert_eq!(work.domains["native_large_message_page_decode_identity"].calls,1);
        assert!(work.incomplete.is_empty());
        reset_hash_work();
        verified_oversized_control(&large).unwrap();
        assert_eq!(take_hash_work(),Default::default());
        // A body up to one part may be raw bytes; a larger one is a page or canonical JSON.
        let part=crate::persistent_store::device_store::plugin_values::structure::PART_BYTES as usize;
        reset_hash_work();
        verified_oversized_control(&vec![0xff;part]).unwrap();
        assert!(verified_oversized_control(&vec![0xff;part+1]).is_err());
        assert_eq!(take_hash_work(),Default::default());
    }

    #[test]
    fn published_asset_roots_decode_only_a_body_with_the_message_page_prefix() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work};
        let asset="a".repeat(64);
        let message=serde_json::json!({"role":"user","content":"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES),"asset":asset});
        let page=risunest_external_storage_format::logical_records::encode_message_page(&[message.clone()]).unwrap().bytes;
        let large=risunest_sync_wire::payload_value::encode(&serde_json::json!([asset,"x".repeat(risunest_sync_wire::MAX_METADATA_BYTES)])).unwrap();
        reset_hash_work();
        published_json_asset_roots(&page).unwrap();
        let work=take_hash_work();
        assert_eq!(work.domains["native_large_message_page_decode_identity"].calls,1);
        assert!(work.incomplete.is_empty());
        reset_hash_work();
        assert!(published_json_asset_roots(&large).unwrap().object_hashes.contains(&asset));
        assert_eq!(take_hash_work(),Default::default());
    }

    #[test]
    fn oversized_message_pages_read_each_original_manifest_once() {
        use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let data = "x".repeat(risunest_sync_wire::MAX_METADATA_BYTES);
        store.commit(&super::super::WorkingSetCommit {
            expected_revision: store.revision().unwrap(),
            add_character: Some(serde_json::json!({
                "type": "character", "chaId": "synthetic-large", "name": "Large",
                "chats": [{"id": "synthetic-large-chat", "name": "Large", "message": [
                    {"role": "user", "data": format!("first {data}")},
                    {"role": "char", "data": format!("second {data}")},
                ]}],
            })),
            ..Default::default()
        }).unwrap();
        let (lease, _sections) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
        let units = store.lww_backup_unit_values(&lease.lease).unwrap();
        reset_hash_work();
        let closure = store.lww_backup_dependency_closure(&lease.lease, &units, &crate::local_backup::NeverCancelled).unwrap();
        let work = take_hash_work();
        assert_eq!(closure.controls.values().filter(|bytes| bytes.len() > risunest_sync_wire::MAX_METADATA_BYTES).count(), 2);
        assert_eq!(work.domains["native_backup_manifest_source"].calls, 1);
        store.release_revision(&lease.lease).unwrap();
    }

    fn receive_original_units(store: &mut PersistentStore, request: &str, changes: Vec<super::super::lww::Change>) {
        use super::super::lww::{ApplyReceive, Header, Progress, StageReceive};
        let header = Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: request.into() };
        store.lww_stage_receive(&StageReceive {
            header: header.clone(), changes,
            progress: Progress { kind: "server".into(), cursor: 1.into(), writer_id: None },
            admitted_time_upper_ms: u64::MAX.into(),
        }).unwrap();
        store.lww_apply_receive(&ApplyReceive { header: header.clone(), generating: Vec::new() }).unwrap();
        store.lww_finish_receive(&header).unwrap();
    }

    #[test]
    fn original_unit_closure_keeps_opaque_and_held_controls_and_separates_relation_keys() {
        use risunest_sync_wire::{descriptor::{build_reference_tree, RecordDescriptor}, unit::{UnitKey, UnitValue}, stamp::Stamp};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let asset = PayloadCas::new(directory.path()).unwrap().prepare_bytes(b"synthetic managed dependency").unwrap();
        store.asset_object_catalog().register(&[super::super::asset_object_catalog::AssetObjectRegistration {
            object_hash: asset.content_hash.clone(), byte_size: asset.byte_size,
        }], 1).unwrap();
        let body = b"synthetic original control";
        let body_hash = risunest_sync_wire::hash(body);
        store.lww_put_object(&body_hash, body).unwrap();
        let dependencies = build_reference_tree(&[asset.content_hash.clone()], false).unwrap();
        let relation = UnitKey::new(&["root", "language"]).unwrap().as_str().to_owned();
        let relations = build_reference_tree(&[relation.clone()], true).unwrap();
        for (hash, bytes) in dependencies.1.iter().chain(&relations.1) {
            store.lww_put_object(hash, bytes).unwrap();
        }
        let mut descriptor = RecordDescriptor::content(body_hash.clone());
        descriptor.dependency_root = dependencies.0;
        descriptor.relation_root = relations.0;
        let value = UnitValue::object(descriptor).unwrap();
        let stamp = Stamp { physical_ms: 1.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000091".into() };
        let opaque = UnitKey::new(&["future-unit", "synthetic"]).unwrap();
        let held = UnitKey::new(&["field", "characters", "missing-parent", "name"]).unwrap();
        receive_original_units(&mut store, "original-controls", vec![
            super::super::lww::Change { key: opaque.clone(), stamp: stamp.clone(), value: value.clone() },
            super::super::lww::Change { key: held.clone(), stamp: stamp.clone(), value: value.clone() },
        ]);
        let revision = store.revision().unwrap();
        let (lease, _sections) = store.lww_acquire_backup_capture(revision).unwrap();
        let units = store.lww_backup_unit_values(&lease.lease).unwrap();
        assert_eq!(units[&opaque], value);
        assert_eq!(units[&held], value);
        let closure = store.lww_backup_dependency_closure(&lease.lease, &units, &crate::local_backup::NeverCancelled).unwrap();
        assert_eq!(closure.payload_bodies[&body_hash], body);
        assert!(!closure.controls.contains_key(&body_hash));
        assert_eq!(closure.managed_payloads[&asset.content_hash], asset.byte_size);
        assert!(!closure.managed_payloads.contains_key(&relation));
        assert!(closure.record_payloads.contains(&body_hash));
        assert!(store.lww_backup_dependency_closure(&lease.lease, &BTreeMap::new(), &crate::local_backup::NeverCancelled).is_err());
        store.connection.execute_batch("DROP TRIGGER message_page_objects_immutable_update;").unwrap();
        store.connection.execute("UPDATE message_page_objects SET body=?1 WHERE hash=?2", params![b"synthetic corruption".as_slice(),body_hash]).unwrap();
        let pinned = store.lww_backup_dependency_closure(&lease.lease, &units, &crate::local_backup::NeverCancelled).unwrap();
        assert_eq!(pinned.payload_bodies[&body_hash], body);
        store.release_revision(&lease.lease).unwrap();
        let (new_lease, _) = store.lww_acquire_backup_capture(revision).unwrap();
        let new_units = store.lww_backup_unit_values(&new_lease.lease).unwrap();
        assert!(store.lww_backup_dependency_closure(&new_lease.lease, &new_units, &crate::local_backup::NeverCancelled).is_err());
        store.release_revision(&new_lease.lease).unwrap();
    }

    #[test]
    fn original_unit_closure_verifies_known_manifests_without_certifying_unreferenced_bodies() {
        use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
        let manifest = risunest_external_storage_format::message_pages::MessageManifest {
            schema:risunest_external_storage_format::message_pages::MANIFEST_SCHEMA.into(),
            message_count:0u64.into(),pages:Vec::new(),
        }.encode().unwrap();
        let units = BTreeMap::from([(
            UnitKey::new(&["messages","synthetic-character","synthetic-conversation"]).unwrap(),
            UnitValue::object(RecordDescriptor::content(manifest.hash.clone())).unwrap(),
        )]);
        let reads = std::cell::RefCell::new(Vec::new());
        let read = |hash:&str| -> StoreResult<Option<Vec<u8>>> {
            reads.borrow_mut().push(hash.to_owned());
            Ok(if hash == manifest.hash {Some(manifest.bytes.clone())} else {Some(b"unreferenced synthetic bytes".to_vec())})
        };
        let closure = original_unit_dependency_closure(&units,&read,&|_| Ok(None),&crate::local_backup::NeverCancelled).unwrap();
        assert_eq!(*reads.borrow(),[manifest.hash.clone()]);
        assert_eq!(closure.controls[&manifest.hash],manifest.bytes);
        assert!(closure.payload_bodies.is_empty() && closure.managed_payloads.is_empty());
        assert!(original_unit_dependency_closure(&units,&|_| Ok(None),&|_| Ok(None),&crate::local_backup::NeverCancelled).is_err());
        assert!(original_unit_dependency_closure(&units,&|_| Ok(Some(b"wrong body".to_vec())),&|_| Ok(None),&crate::local_backup::NeverCancelled).is_err());
    }

    #[test]
    fn original_unit_inventory_keeps_real_archive_metadata_and_shared_payload_roots() {
        use risunest_sync_wire::unit::UnitKey;
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let compressed = risunest_sync_wire::hash(b"synthetic compressed archive");
        let asset = risunest_sync_wire::hash(b"synthetic archived asset");
        let archived = super::super::archive::ArchivedObject {
            object_hash:compressed.clone(),shared_object_hash:compressed.clone(),archived_at:1,
            conversation_count:0,message_count:0,asset_hashes:vec![asset.clone()],shared_asset_hashes:vec![asset.clone()],identity_remap:Vec::new(),
        };
        let value = super::super::lww::archive_value(&store.connection,&archived).unwrap();
        let units = BTreeMap::from([(UnitKey::new(&["archive","synthetic"]).unwrap(),value.clone())]);
        let metadata_hash = match &value {risunest_sync_wire::unit::UnitValue::Object{descriptor,..} => descriptor.object_hash.clone(),_ => unreachable!()};
        let mut emitted = Vec::new();
        crate::persistent_store::hash_work::reset_hash_work();
        let inventory = original_unit_dependency_inventory(&units,
            &|hash| {assert_eq!(hash,metadata_hash,"Archive payload opened during metadata validation"); super::super::message_pages::object_body(&store.connection,hash)},
            &|_| Ok(Some(32)),&crate::local_backup::NeverCancelled,false,
            &mut |hash,_,role| {emitted.push((hash.to_owned(),role)); Ok(())}).unwrap();
        let hashes = crate::persistent_store::hash_work::take_hash_work();
        assert!(hashes.incomplete.is_empty());
        for domain in ["native_backup_dependency_source","native_backup_dependency_descriptor","native_backup_archive_metadata","native_wire_descriptor_validation"] {
            assert!(hashes.domains[domain].bytes > 0,"Missing actual SHA observation: {domain}");
        }
        assert!(inventory.controls.contains_key(&metadata_hash));
        assert_eq!(inventory.payloads,BTreeMap::from([(compressed,Some(32)),(asset,Some(32))]));
        assert!(!inventory.record_payloads.contains(&metadata_hash));
        assert!(emitted.iter().all(|(_,role)| *role == BackupBodyRole::Control));
        let mut invalid_units = units.clone();
        let mut descriptor = match value {risunest_sync_wire::unit::UnitValue::Object{descriptor,..} => descriptor,_ => unreachable!()};
        descriptor.dependencies.clear();
        invalid_units.insert(UnitKey::new(&["archive","synthetic"]).unwrap(),risunest_sync_wire::unit::UnitValue::object(descriptor).unwrap());
        assert!(original_unit_dependency_inventory(&invalid_units,&|hash| super::super::message_pages::object_body(&store.connection,hash),&|_| Ok(Some(32)),&crate::local_backup::NeverCancelled,false,&mut |_,_,_| Ok(())).is_err());
    }

    #[test]
    fn original_unit_inventory_failed_reference_decode_invalidates_hash_receipt() {
        use risunest_sync_wire::{descriptor::RecordDescriptor,unit::{UnitKey,UnitValue}};
        let malformed = b"synthetic invalid reference JSON";
        let root = risunest_sync_wire::hash(malformed);
        let mut descriptor = RecordDescriptor::content(risunest_sync_wire::hash(b"synthetic opaque payload"));
        descriptor.dependency_root = Some(root.clone());
        let units = BTreeMap::from([(UnitKey::new(&["future-unit","malformed-tree"]).unwrap(),UnitValue::object(descriptor).unwrap())]);
        crate::persistent_store::hash_work::reset_hash_work();
        assert!(original_unit_dependency_inventory(&units,&|hash| {assert_eq!(hash,root); Ok(Some(malformed.to_vec()))},&|_| Ok(None),&crate::local_backup::NeverCancelled,false,&mut |_,_,_| Ok(())).is_err());
        let observed = crate::persistent_store::hash_work::take_hash_work();
        assert_eq!(observed.incomplete["native_backup_reference_verify"],1);
        assert_eq!(observed.domains["native_backup_dependency_source"].bytes,malformed.len() as u64);
    }

    #[test]
    fn original_unit_inventory_gc_never_reads_payloads_and_streams_typed_controls() {
        use risunest_sync_wire::{descriptor::{build_reference_tree,RecordDescriptor},unit::{UnitKey,UnitValue}};
        let payload = risunest_sync_wire::hash(b"synthetic opaque payload");
        let dependency = risunest_sync_wire::hash(b"synthetic dependency");
        let tree = build_reference_tree(&[dependency.clone()],false).unwrap();
        let mut descriptor = RecordDescriptor::content(payload.clone());
        descriptor.dependency_root = tree.0;
        let value = UnitValue::object(descriptor).unwrap();
        let units = BTreeMap::from([(UnitKey::new(&["future-unit","stream"]).unwrap(),value)]);
        let reads = std::cell::RefCell::new(Vec::new());
        let read = |hash:&str| -> StoreResult<Option<Vec<u8>>> {
            reads.borrow_mut().push(hash.to_owned());
            assert!(hash != payload && hash != dependency,"GC opened a payload body");
            Ok(tree.1.iter().find(|(digest,_)| digest == hash).map(|(_,bytes)| bytes.clone()))
        };
        let mut emitted = Vec::new();
        let inventory = original_unit_dependency_inventory(&units,&read,&|_| Ok(None),&crate::local_backup::NeverCancelled,false,
            &mut |hash,_,role| {emitted.push((hash.to_owned(),role)); Ok(())}).unwrap();
        assert_eq!(inventory.payloads,BTreeMap::from([(payload.clone(),None),(dependency.clone(),None)]));
        assert!(inventory.record_payloads.contains(&payload));
        assert!(emitted.iter().all(|(_,role)| *role == BackupBodyRole::Control));
        assert_eq!(reads.borrow().len(),tree.1.len());
        assert_eq!(inventory.controls.len(),tree.1.len()+1);
        let failure = original_unit_dependency_inventory(&units,&read,&|_| Ok(None),&crate::local_backup::NeverCancelled,false,
            &mut |hash,_,_| if tree.1.iter().any(|(digest,_)| digest == hash) {Err(invalid("synthetic sink failure"))} else {Ok(())});
        assert!(matches!(failure,Err(StoreError::Validation{message}) if message == "synthetic sink failure"));
    }

    #[test]
    fn original_unit_closure_rejects_missing_source_and_cancellation_without_consuming_the_lease() {
        use risunest_sync_wire::{descriptor::RecordDescriptor, unit::{UnitKey, UnitValue}, stamp::Stamp};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        receive_original_units(&mut store, "missing-original-control", vec![super::super::lww::Change {
            key: UnitKey::new(&["future-unit", "missing-body"]).unwrap(),
            stamp: Stamp { physical_ms: 1.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000092".into() },
            value: UnitValue::object(RecordDescriptor::content("ab".repeat(32))).unwrap(),
        }]);
        let revision = store.revision().unwrap();
        let (lease, _) = store.lww_acquire_backup_capture(revision).unwrap();
        let units = store.lww_backup_unit_values(&lease.lease).unwrap();
        assert!(store.lww_backup_dependency_closure(&lease.lease, &units, &crate::local_backup::NeverCancelled).is_err());
        struct Cancelled;
        impl CancellationProbe for Cancelled { fn is_cancelled(&self) -> bool { true } }
        let error = store.lww_backup_dependency_closure(&lease.lease, &units, &Cancelled).err().unwrap();
        assert!(matches!(error, StoreError::Validation { message } if message == "External capture cancelled"));
        assert!(store.revision_leases.contains_key(&lease.lease));
        store.release_revision(&lease.lease).unwrap();
    }

    #[test]
    fn supplied_capture_lease_keeps_the_original_view_and_is_consumed() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let probe = crate::local_backup::NeverCancelled;
        let commit = serde_json::from_value(serde_json::json!({
            "expectedRevision":0,"root":{"username":"synthetic original"}
        })).unwrap();
        store.commit(&commit).unwrap();
        let hydration = store.hydrate_external_capture_dependencies("backup", &probe).unwrap();
        let first = store.capture_external_library("backup", &hydration, &probe).unwrap();
        let scope = library_fingerprint_domain();
        let fingerprint = first.catalog.content_fingerprint(&scope).unwrap();
        let hydration = store.hydrate_external_capture_dependencies("backup", &probe).unwrap();
        let (lease, prepared_sections) = store.lww_acquire_backup_capture(1).unwrap();
        let original_units = store.lww_backup_unit_values(&lease.lease).unwrap();
        let sections = crate::external_storage::sections::capture_prepared_backup_sections(
            &prepared_sections,&directory.path().join("fixed-sections"),&crate::external_storage::contract::Cancellation::default(),
        ).unwrap();
        let fixed_section_identity = sections.iter().map(|section| (section.kind,section.content_fingerprint)).collect::<Vec<_>>();
        let commit = serde_json::from_value(serde_json::json!({
            "expectedRevision":1,"root":{"username":"synthetic later"}
        })).unwrap();
        store.commit(&commit).unwrap();
        assert!(store.capture_external_library_from_lease("wrong", &hydration, &lease.lease, &probe).is_err());
        assert!(store.revision_leases.contains_key(&lease.lease));
        let captured = store.capture_external_library_from_lease_with_sections("backup", &hydration, &lease.lease, sections, &probe).unwrap();
        assert_ne!(captured.id,first.id);
        assert!(first.catalog.original_backup_units().is_err());
        assert_eq!(captured.identity.revision, 1);
        assert_eq!(captured.catalog.content_fingerprint(&scope).unwrap(), fingerprint);
        assert_eq!(store.revision().unwrap(), 2);
        assert_eq!(captured.catalog.original_backup_units().unwrap().units().unwrap().collect::<StoreResult<BTreeMap<_,_>>>().unwrap(), original_units);
        assert_eq!(captured.catalog.backup_sections().unwrap().iter().map(|section| (section.kind,section.content_fingerprint)).collect::<Vec<_>>(),fixed_section_identity);
        store.retain_external_capture(&captured.id,"synthetic-backup-job").unwrap();
        let reopened = store.reopen_external_capture(&captured.id).unwrap();
        assert_eq!(reopened.catalog.original_backup_units().unwrap().units().unwrap().collect::<StoreResult<BTreeMap<_,_>>>().unwrap(),original_units);
        assert_eq!(reopened.catalog.backup_sections().unwrap().iter().map(|section| (section.kind,section.content_fingerprint)).collect::<Vec<_>>(),fixed_section_identity);
        assert!(!store.revision_leases.contains_key(&lease.lease));
        assert!(store.capture_external_library_from_lease("backup", &hydration, &lease.lease, &probe).is_err());
        let later = store.acquire_revision(2).unwrap();
        assert!(store.capture_external_library_from_lease("backup", &hydration, &later.lease, &probe).is_err());
        assert!(store.revision_leases.contains_key(&later.lease));
        store.release_revision(&later.lease).unwrap();
    }


    #[test]
    fn grouped_manifest_hydration_preserves_scope_unavailability_and_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut dependencies = BTreeSet::new();
        for index in 0..130 {
            let object = cas.prepare_bytes(format!("synthetic manifest {index}").as_bytes()).unwrap();
            dependencies.insert(Dependency { hash: object.content_hash, manifest: true });
        }
        let absent = "a1".repeat(32);
        dependencies.insert(Dependency { hash: absent.clone(), manifest: false });
        let mut session = HydrationSession::new(directory.path(), None).unwrap();
        store.hydrate_dependencies(
            &mut session, dependencies.clone(), &crate::local_backup::NeverCancelled,
        ).unwrap();
        assert_eq!(cas.stat_object(&absent).unwrap(), None);

        dependencies.insert(Dependency { hash: absent, manifest: true });
        let error = store.hydrate_dependencies(
            &mut session, dependencies, &crate::local_backup::NeverCancelled,
        ).unwrap_err();
        assert!(matches!(error, StoreError::Validation { message }
            if message == "Required capture payload is unavailable"));

        struct Cancelled;
        impl CancellationProbe for Cancelled {
            fn is_cancelled(&self) -> bool { true }
        }
        let error = store.hydrate_hashes(
            &mut session, &["b2".repeat(32)], &Cancelled,
        ).unwrap_err();
        assert!(matches!(error, StoreError::Validation { message }
            if message == "External capture cancelled"));
    }


}
