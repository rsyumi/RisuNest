//! Authenticated external-storage object envelope and snapshot metadata.
//! The public envelope header supports provider listings whose opaque locator
//! does not preserve the immutable object ID. Its fields become trusted only
//! after the enclosed secretstream authenticates the exact header bytes.
use super::{crypto, FormatError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const ENVELOPE_MAGIC: &[u8; 4] = b"RNX1";
const HEADER_SCHEMA: &str = "risunest.external-object/v1";
const SNAPSHOT_SCHEMA: &str = "risunest.external-snapshot/v1";
const CATALOG_SCHEMA: &str = "risunest.external-catalog/v1";
pub const MAX_PUBLIC_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;

/// Extend the library's descriptor-scope fingerprint with the independently
/// captured device state. Library-only snapshots preserve the PDS fingerprint.
pub fn combine_content_fingerprint(
    scope_id: &[u8; 32],
    library_fingerprint: &[u8; 32],
    device_identity: Option<&[u8; 32]>,
) -> [u8; 32] {
    let Some(device_identity) = device_identity else {
        return *library_fingerprint;
    };
    let mut digest = Sha256::new();
    digest.update(b"risunest.external-snapshot-fingerprint/v1\0");
    digest.update(scope_id);
    digest.update(library_fingerprint);
    digest.update(device_identity);
    digest.finalize().into()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ObjectRole {
    Descriptor,
    Pack,
    Catalog,
    Snapshot,
    BackupPoint,
    Head,
}

/// Produce the provider-visible identity for content-addressed objects without
/// publishing the plaintext digest. Callers pass a repository-derived key.
pub fn keyed_object_id(
    key: &[u8; 32],
    namespace: &str,
    role: ObjectRole,
    plaintext_sha256: &[u8; 32],
) -> Result<String> {
    if namespace.is_empty() || namespace.len() > 1024 || namespace.contains('\0') {
        return Err(FormatError("invalid-object-namespace"));
    }
    let (prefix, role_tag) = match role {
        ObjectRole::Pack => ("pack", b"pack".as_slice()),
        ObjectRole::Catalog => ("catalog", b"catalog".as_slice()),
        _ => return Err(FormatError("invalid-keyed-object-role")),
    };
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(b"risunest.external-object-id/v1\0");
    inner.update((namespace.len() as u64).to_le_bytes());
    inner.update(namespace.as_bytes());
    inner.update(role_tag);
    inner.update([0]);
    inner.update(plaintext_sha256);
    let inner: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    Ok(format!("{prefix}-{}", hex::encode(outer.finalize())))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicObjectHeader {
    pub schema: String,
    pub repository_id: String,
    pub object_id: String,
    pub role: ObjectRole,
    pub plaintext_length: u64,
}

impl PublicObjectHeader {
    pub fn new(
        repository_id: String,
        object_id: String,
        role: ObjectRole,
        plaintext_length: u64,
    ) -> Result<Self> {
        let value = Self {
            schema: HEADER_SCHEMA.into(),
            repository_id,
            object_id,
            role,
            plaintext_length,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != HEADER_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.object_id.is_empty()
            || self.object_id.len() > 8192
            || self.object_id.contains('\0')
        {
            return Err(FormatError("invalid-object-header"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let encoded = serde_json::to_vec(self).map_err(|_| FormatError("invalid-object-header"))?;
        if encoded.len() > MAX_PUBLIC_HEADER_BYTES {
            return Err(FormatError("object-header-limit-exceeded"));
        }
        Ok(encoded)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WireLocator {
    pub connection_identity: String,
    pub collection: Option<String>,
    pub object: String,
}

impl WireLocator {
    pub fn validate(&self) -> Result<()> {
        if self.connection_identity.is_empty()
            || self.connection_identity.len() > 8192
            || self.object.is_empty()
            || self.object.len() > 8192
            || self.connection_identity.contains('\0')
            || self.object.contains('\0')
            || self
                .collection
                .as_ref()
                .is_some_and(|value| value.len() > 8192 || value.contains('\0'))
        {
            return Err(FormatError("invalid-object-locator"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredObject {
    pub header: PublicObjectHeader,
    pub locator: WireLocator,
    pub ciphertext_length: u64,
    pub ciphertext_sha256: [u8; 32],
    pub plaintext_length: u64,
    pub plaintext_sha256: [u8; 32],
}

impl StoredObject {
    pub fn validate(&self) -> Result<()> {
        self.header.validate()?;
        self.locator.validate()?;
        if self.ciphertext_length != envelope_length(&self.header)?
            || self.plaintext_length != self.header.plaintext_length
        {
            return Err(FormatError("object-length-mismatch"));
        }
        Ok(())
    }
}

pub fn envelope_length(header: &PublicObjectHeader) -> Result<u64> {
    let encoded = header.encode()?;
    (8u64)
        .checked_add(encoded.len() as u64)
        .and_then(|n| {
            crypto::ciphertext_length(header.plaintext_length)
                .ok()
                .and_then(|m| n.checked_add(m))
        })
        .ok_or(FormatError("length-overflow"))
}

pub fn seal_envelope(
    input: &mut impl Read,
    output: &mut impl Write,
    key: &[u8; 32],
    header: &PublicObjectHeader,
) -> Result<()> {
    let encoded = header.encode()?;
    output.write_all(ENVELOPE_MAGIC)?;
    output.write_all(&(encoded.len() as u32).to_le_bytes())?;
    output.write_all(&encoded)?;
    crypto::encrypt(input, output, key, &encoded, header.plaintext_length)?;
    Ok(())
}

/// Returns an unauthenticated header and leaves the reader at the encrypted
/// secretstream. Callers must not trust it until `open_envelope` succeeds.
pub fn read_public_header(input: &mut impl Read) -> Result<(PublicObjectHeader, Vec<u8>)> {
    let mut magic = [0; 4];
    input.read_exact(&mut magic)?;
    if &magic != ENVELOPE_MAGIC {
        return Err(FormatError("invalid-object-envelope"));
    }
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_PUBLIC_HEADER_BYTES {
        return Err(FormatError("object-header-limit-exceeded"));
    }
    let mut encoded = vec![0; length];
    input.read_exact(&mut encoded)?;
    let header: PublicObjectHeader =
        serde_json::from_slice(&encoded).map_err(|_| FormatError("invalid-object-header"))?;
    header.validate()?;
    if header.encode()? != encoded {
        return Err(FormatError("non-canonical-object-header"));
    }
    Ok((header, encoded))
}

pub fn open_envelope(
    input: &mut impl Read,
    output: &mut impl Write,
    key: &[u8; 32],
    max_plaintext: u64,
) -> Result<PublicObjectHeader> {
    let (header, encoded) = read_public_header(input)?;
    if header.plaintext_length > max_plaintext {
        return Err(FormatError("decoded-limit-exceeded"));
    }
    let length = crypto::decrypt(input, output, key, &encoded, max_plaintext)?;
    if length != header.plaintext_length {
        return Err(FormatError("plaintext-integrity-failed"));
    }
    Ok(header)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CatalogKind {
    Records,
    Assets,
    Device,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CatalogEntryKind {
    Record,
    Object,
    DeviceCatalog,
    DeviceObject,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StoredChunk {
    pub pack_id: String,
    pub offset: u64,
    pub stored_length: u64,
    pub plaintext_length: u64,
    pub plaintext_sha256: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogEntryFragment {
    pub kind: CatalogEntryKind,
    pub key: String,
    pub content_sha256: [u8; 32],
    pub byte_length: u64,
    pub fragment_index: u32,
    pub fragment_count: u32,
    pub chunks: Vec<StoredChunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogChild {
    pub first_key: String,
    pub last_key: String,
    pub object: StoredObject,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogDocument {
    pub schema: String,
    pub kind: CatalogKind,
    pub level: u16,
    pub first_key: String,
    pub last_key: String,
    pub entries: Vec<CatalogEntryFragment>,
    pub children: Vec<CatalogChild>,
    pub packs: Vec<StoredObject>,
}

impl CatalogDocument {
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes =
            serde_json::to_vec(self).map_err(|_| FormatError("invalid-catalog-document"))?;
        if bytes.len() > max_bytes.min(MAX_METADATA_BYTES) {
            return Err(FormatError("catalog-limit-exceeded"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        if bytes.len() > max_bytes.min(MAX_METADATA_BYTES) {
            return Err(FormatError("catalog-limit-exceeded"));
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-catalog-document"))?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-catalog-document"));
        }
        Ok(value)
    }
    pub fn leaf(
        kind: CatalogKind,
        entries: Vec<CatalogEntryFragment>,
        packs: Vec<StoredObject>,
    ) -> Result<Self> {
        let first_key = entries
            .first()
            .map(|entry| entry.key.clone())
            .unwrap_or_default();
        let last_key = entries
            .last()
            .map(|entry| entry.key.clone())
            .unwrap_or_default();
        let value = Self {
            schema: CATALOG_SCHEMA.into(),
            kind,
            level: 0,
            first_key,
            last_key,
            entries,
            children: Vec::new(),
            packs,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn branch(kind: CatalogKind, level: u16, children: Vec<CatalogChild>) -> Result<Self> {
        let first_key = children
            .first()
            .map(|child| child.first_key.clone())
            .unwrap_or_default();
        let last_key = children
            .last()
            .map(|child| child.last_key.clone())
            .unwrap_or_default();
        let value = Self {
            schema: CATALOG_SCHEMA.into(),
            kind,
            level,
            first_key,
            last_key,
            entries: Vec::new(),
            children,
            packs: Vec::new(),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != CATALOG_SCHEMA || self.level > 32 {
            return Err(FormatError("invalid-catalog-document"));
        }
        if self.level == 0 {
            if !self.children.is_empty()
                || self.entries.is_empty() != self.first_key.is_empty()
                || self.entries.is_empty() != self.last_key.is_empty()
            {
                return Err(FormatError("invalid-catalog-document"));
            }
            let mut previous: Option<(&str, u32)> = None;
            for entry in &self.entries {
                if entry.key.is_empty()
                    || entry.fragment_count == 0
                    || entry.fragment_index >= entry.fragment_count
                    || entry.chunks.is_empty()
                {
                    return Err(FormatError("invalid-catalog-entry"));
                }
                if let Some((key, fragment)) = previous {
                    if key > entry.key.as_str()
                        || (key == entry.key && fragment + 1 != entry.fragment_index)
                    {
                        return Err(FormatError("invalid-catalog-order"));
                    }
                }
                let sum = entry.chunks.iter().try_fold(0u64, |total, chunk| {
                    if chunk.pack_id.is_empty()
                        || chunk.stored_length == 0
                        || chunk.offset.checked_add(chunk.stored_length).is_none()
                    {
                        return Err(FormatError("invalid-chunk-reference"));
                    }
                    total
                        .checked_add(chunk.plaintext_length)
                        .ok_or(FormatError("length-overflow"))
                })?;
                if sum > entry.byte_length {
                    return Err(FormatError("catalog-length-mismatch"));
                }
                previous = Some((&entry.key, entry.fragment_index));
            }
            if self
                .entries
                .first()
                .is_some_and(|entry| entry.key != self.first_key)
                || self
                    .entries
                    .last()
                    .is_some_and(|entry| entry.key != self.last_key)
            {
                return Err(FormatError("invalid-catalog-range"));
            }
            let mut previous_object = None;
            for object in &self.packs {
                object.validate()?;
                if object.header.role != ObjectRole::Pack
                    || previous_object
                        .is_some_and(|id: &str| id >= object.header.object_id.as_str())
                {
                    return Err(FormatError("invalid-pack-reference"));
                }
                previous_object = Some(object.header.object_id.as_str());
            }
        } else {
            if !self.entries.is_empty() || !self.packs.is_empty() || self.children.is_empty() {
                return Err(FormatError("invalid-catalog-document"));
            }
            let mut previous: Option<&str> = None;
            for child in &self.children {
                child.object.validate()?;
                if child.object.header.role != ObjectRole::Catalog
                    || child.first_key.is_empty()
                    || child.first_key > child.last_key
                    || previous.is_some_and(|key| key > child.first_key.as_str())
                {
                    return Err(FormatError("invalid-catalog-child"));
                }
                previous = Some(&child.last_key);
            }
            if self
                .children
                .first()
                .is_some_and(|child| child.first_key != self.first_key)
                || self
                    .children
                    .last()
                    .is_some_and(|child| child.last_key != self.last_key)
            {
                return Err(FormatError("invalid-catalog-range"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotDocument {
    pub schema: String,
    pub snapshot_id: String,
    pub repository_id: String,
    pub library_id: String,
    pub author_device_id: String,
    pub created_at_ms: u64,
    pub logical_revision: u64,
    pub scope_id: [u8; 32],
    pub library_scope_id: [u8; 32],
    pub parent_snapshot_id: Option<String>,
    pub fingerprint: [u8; 32],
    pub library_fingerprint: [u8; 32],
    pub record_catalog: StoredObject,
    pub asset_catalog: StoredObject,
    pub device_catalog: Option<StoredObject>,
    pub device_identity: Option<[u8; 32]>,
    pub device_sections: Vec<String>,
}

impl SnapshotDocument {
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| FormatError("invalid-snapshot"))?;
        if bytes.len() > max_bytes.min(MAX_METADATA_BYTES) {
            return Err(FormatError("snapshot-limit-exceeded"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        if bytes.len() > max_bytes.min(MAX_METADATA_BYTES) {
            return Err(FormatError("snapshot-limit-exceeded"));
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-snapshot"))?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-snapshot"));
        }
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != SNAPSHOT_SCHEMA
            || self.snapshot_id.is_empty()
            || self.snapshot_id.len() > 1024
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.library_id.is_empty()
            || self.library_id.len() > 1024
            || self.author_device_id.is_empty()
            || self.author_device_id.len() > 1024
            || self.logical_revision > i64::MAX as u64
            || self
                .parent_snapshot_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 1024)
        {
            return Err(FormatError("invalid-snapshot"));
        }
        for object in [
            (CatalogKind::Records, &self.record_catalog),
            (CatalogKind::Assets, &self.asset_catalog),
        ]
        .map(|(_, object)| object)
        {
            object.validate()?;
            if object.header.repository_id != self.repository_id
                || object.header.role != ObjectRole::Catalog
            {
                return Err(FormatError("invalid-snapshot-catalog"));
            }
        }
        if self.device_catalog.is_some() != self.device_identity.is_some()
            || self.device_catalog.is_some() != !self.device_sections.is_empty()
            || self.fingerprint
                != combine_content_fingerprint(
                    &self.scope_id,
                    &self.library_fingerprint,
                    self.device_identity.as_ref(),
                )
        {
            return Err(FormatError("invalid-snapshot-device"));
        }
        if let Some(object) = &self.device_catalog {
            object.validate()?;
            if object.header.repository_id != self.repository_id
                || object.header.role != ObjectRole::Catalog
            {
                return Err(FormatError("invalid-snapshot-device"));
            }
            let mut previous: Option<&str> = None;
            for section in &self.device_sections {
                if section.is_empty()
                    || section.len() > 64 * 1024
                    || previous.is_some_and(|value| value >= section.as_str())
                {
                    return Err(FormatError("invalid-device-sections"));
                }
                previous = Some(section);
            }
        }
        Ok(())
    }
    pub fn new(
        snapshot_id: String,
        repository_id: String,
        library_id: String,
        author_device_id: String,
        created_at_ms: u64,
        logical_revision: u64,
        scope_id: [u8; 32],
        library_scope_id: [u8; 32],
        parent_snapshot_id: Option<String>,
        fingerprint: [u8; 32],
        library_fingerprint: [u8; 32],
        record_catalog: StoredObject,
        asset_catalog: StoredObject,
        device_catalog: Option<StoredObject>,
        device_identity: Option<[u8; 32]>,
        device_sections: Vec<String>,
    ) -> Result<Self> {
        let value = Self {
            schema: SNAPSHOT_SCHEMA.into(),
            snapshot_id,
            repository_id,
            library_id,
            author_device_id,
            created_at_ms,
            logical_revision,
            scope_id,
            library_scope_id,
            parent_snapshot_id,
            fingerprint,
            library_fingerprint,
            record_catalog,
            asset_catalog,
            device_catalog,
            device_identity,
            device_sections,
        };
        value.validate()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_exposes_identity_but_trusts_it_only_after_authentication() {
        let bytes = b"synthetic payload";
        let header = PublicObjectHeader::new(
            "repository".into(),
            "pack-object".into(),
            ObjectRole::Pack,
            bytes.len() as u64,
        )
        .unwrap();
        let mut sealed = Vec::new();
        seal_envelope(
            &mut std::io::Cursor::new(bytes),
            &mut sealed,
            &[7; 32],
            &header,
        )
        .unwrap();
        assert_eq!(sealed.len() as u64, envelope_length(&header).unwrap());
        let (visible, _) = read_public_header(&mut std::io::Cursor::new(&sealed)).unwrap();
        assert_eq!(visible, header);
        let mut opened = Vec::new();
        assert_eq!(
            open_envelope(
                &mut std::io::Cursor::new(&sealed),
                &mut opened,
                &[7; 32],
                1024
            )
            .unwrap(),
            header
        );
        assert_eq!(opened, bytes);
        let marker = sealed.iter().position(|byte| *byte == b'p').unwrap();
        sealed[marker] ^= 1;
        assert!(open_envelope(
            &mut std::io::Cursor::new(&sealed),
            &mut Vec::new(),
            &[7; 32],
            1024
        )
        .is_err());
    }

    #[test]
    fn keyed_object_names_hide_plaintext_hashes_and_are_repository_specific() {
        let plaintext_hash = [0xabu8; 32];
        let visible_hash = hex::encode(plaintext_hash);
        let first_key = crypto::derive_key(&[1; 32], "repository-a", "data").unwrap();
        let second_key = crypto::derive_key(&[1; 32], "repository-b", "data").unwrap();
        let first =
            keyed_object_id(&first_key, "job-a", ObjectRole::Pack, &plaintext_hash).unwrap();
        let second =
            keyed_object_id(&second_key, "job-a", ObjectRole::Pack, &plaintext_hash).unwrap();
        let other_job =
            keyed_object_id(&first_key, "job-b", ObjectRole::Pack, &plaintext_hash).unwrap();
        let catalog =
            keyed_object_id(&first_key, "job-a", ObjectRole::Catalog, &plaintext_hash).unwrap();
        assert!(first.starts_with("pack-"));
        assert!(!first.contains(&visible_hash));
        assert_ne!(first, second);
        assert_ne!(first, other_job);
        assert_ne!(first, catalog);
    }

    #[test]
    fn snapshot_and_catalog_encoding_is_canonical_and_bounded() {
        let pack_header =
            PublicObjectHeader::new("repository".into(), "pack".into(), ObjectRole::Pack, 10)
                .unwrap();
        let pack = StoredObject {
            ciphertext_length: envelope_length(&pack_header).unwrap(),
            ciphertext_sha256: [2; 32],
            plaintext_length: 10,
            plaintext_sha256: [1; 32],
            locator: WireLocator {
                connection_identity: "account/root".into(),
                collection: None,
                object: "opaque".into(),
            },
            header: pack_header,
        };
        let entry = CatalogEntryFragment {
            kind: CatalogEntryKind::Record,
            key: "record/a".into(),
            content_sha256: [3; 32],
            byte_length: 4,
            fragment_index: 0,
            fragment_count: 1,
            chunks: vec![StoredChunk {
                pack_id: "pack".into(),
                offset: 0,
                stored_length: 10,
                plaintext_length: 4,
                plaintext_sha256: [3; 32],
            }],
        };
        let catalog = CatalogDocument::leaf(CatalogKind::Records, vec![entry], vec![pack]).unwrap();
        let encoded = catalog.encode(4096).unwrap();
        assert_eq!(CatalogDocument::decode(&encoded, 4096).unwrap(), catalog);
        assert!(catalog.encode(128).is_err());
    }
}
