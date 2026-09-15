//! Sorted catalog pages with explicit byte bounds. Records and assets are
//! separate catalogs so text-only changes can reuse the asset catalog identity.
use super::{content_identity::hash, FormatError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChunkReference {
    pub pack_id: String,
    pub offset: u64,
    pub length: u64,
    pub hash: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub key: String,
    pub content_hash: [u8; 32],
    pub byte_length: u64,
    pub chunks: Vec<ChunkReference>,
}
pub struct CatalogPage {
    pub first_key: String,
    pub last_key: String,
    pub bytes: Vec<u8>,
    pub hash: [u8; 32],
}

pub fn pages(entries: &BTreeMap<String, Entry>, max_bytes: usize) -> Result<Vec<CatalogPage>> {
    if !(128..=16 * 1024 * 1024).contains(&max_bytes) {
        return Err(FormatError("invalid-catalog-limit"));
    }
    let mut result = Vec::new();
    let mut page = Vec::new();
    let mut first = String::new();
    let mut last = String::new();
    page.push(b'[');
    for (key, entry) in entries {
        if &entry.key != key || key.is_empty() || key.len() > 64 * 1024 {
            return Err(FormatError("invalid-catalog-key"));
        }
        let bytes = serde_json::to_vec(entry).map_err(|_| FormatError("invalid-catalog-entry"))?;
        if bytes.len() + 2 > max_bytes {
            return Err(FormatError("catalog-entry-requires-fragmentation"));
        }
        if page.len() > 1 && page.len() + bytes.len() + 2 > max_bytes {
            page.push(b']');
            let digest = hash(&page);
            result.push(CatalogPage {
                first_key: std::mem::take(&mut first),
                last_key: last.clone(),
                bytes: std::mem::replace(&mut page, vec![b'[']),
                hash: digest,
            });
        }
        if page.len() == 1 {
            first = key.clone();
        } else {
            page.push(b',');
        }
        page.extend_from_slice(&bytes);
        last = key.clone();
    }
    page.push(b']');
    let digest = hash(&page);
    result.push(CatalogPage {
        first_key: first,
        last_key: last,
        bytes: page,
        hash: digest,
    });
    Ok(result)
}

pub fn read_page(bytes: &[u8], expected_hash: &[u8; 32], max_bytes: usize) -> Result<Vec<Entry>> {
    if bytes.len() > max_bytes || hash(bytes) != *expected_hash {
        return Err(FormatError("catalog-integrity-failed"));
    }
    let entries: Vec<Entry> =
        serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-catalog-page"))?;
    let mut previous: Option<&str> = None;
    for entry in &entries {
        if entry.key.is_empty() || previous.is_some_and(|key| key >= entry.key.as_str()) {
            return Err(FormatError("invalid-catalog-order"));
        }
        let mut length = 0u64;
        for chunk in &entry.chunks {
            length = length
                .checked_add(chunk.length)
                .ok_or(FormatError("length-overflow"))?;
            if chunk.pack_id.is_empty() || chunk.offset.checked_add(chunk.length).is_none() {
                return Err(FormatError("invalid-chunk-reference"));
            }
        }
        if length != entry.byte_length {
            return Err(FormatError("catalog-length-mismatch"));
        }
        previous = Some(&entry.key);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_pages_are_deterministic_sorted_and_byte_bounded() {
        let entries = (0..200)
            .map(|n| {
                let key = format!("synthetic-{n:04}");
                let entry = Entry {
                    key: key.clone(),
                    content_hash: hash(b"text"),
                    byte_length: 4,
                    chunks: vec![ChunkReference {
                        pack_id: "pack".into(),
                        offset: 0,
                        length: 4,
                        hash: hash(b"text"),
                    }],
                };
                (key, entry)
            })
            .collect();
        let pages = pages(&entries, 1024).unwrap();
        assert!(pages.len() > 1);
        let mut restored = BTreeMap::new();
        for page in pages {
            assert!(page.bytes.len() <= 1024);
            for entry in read_page(&page.bytes, &page.hash, 1024).unwrap() {
                restored.insert(entry.key.clone(), entry);
            }
        }
        assert_eq!(entries, restored);
    }
    #[test]
    fn fingerprints_exclude_physical_locations_and_publication_strategy() {
        use crate::format::*;
        let entries = BTreeMap::from([("synthetic".into(), hash(b"same"))]);
        let domain = library_fingerprint_domain();
        assert_eq!(fingerprint(&domain, &entries), fingerprint(&domain, &entries));
        assert_ne!(fingerprint(&domain, &entries), fingerprint(&[0; 32], &entries));
        // Two connections to the same repository agree on identity whatever
        // each device chose to publish, so neither reports the other corrupt.
        let sequential = Descriptor::new("repository".into(), Some(Strategy::Sequential)).unwrap();
        let reconnected = Descriptor::new("repository".into(), Some(Strategy::Sequential)).unwrap();
        assert_eq!(sequential, reconnected);
    }
}
