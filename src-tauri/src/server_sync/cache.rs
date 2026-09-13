use super::{Result, SyncError};
use crate::{
    asset_repository::PayloadCas, persistent_store::server_sync_projection::ServerPayload,
};
use risunest_sync_wire::{
    canonical,
    descriptor::{build_reference_tree, visit_reference_tree, RecordDescriptor},
    payload, RecordVersion, WireError, MAX_METADATA_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read},
    path::Path,
};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordObject {
    schema: String,
    payload: payload::Payload,
}
pub(crate) struct Cache {
    pub cas: PayloadCas,
}
pub(crate) struct ProjectedRecord {
    pub version: RecordVersion,
    pub local_hash: String,
    pub objects: Vec<String>,
}
impl Cache {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self {
            cas: PayloadCas::new(root)?,
        })
    }
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        Ok(self.cas.prepare_bytes(bytes)?.content_hash)
    }
    pub fn read(&self, hash: &str, limit: usize) -> Result<Vec<u8>> {
        let file = self
            .cas
            .open_object(hash)?
            .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
        if file.metadata()?.len() > limit as u64 {
            return Err(SyncError::new("cached-object-too-large", 413));
        }
        let mut bytes = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(SyncError::new("cached-object-too-large", 413));
        }
        if risunest_sync_wire::hash(&bytes) != hash {
            return Err(SyncError::new("cached-object-corrupt", 409));
        }
        Ok(bytes)
    }
    pub fn project(
        &self,
        payload: &ServerPayload,
        dependencies: &[String],
        relations: &[String],
        scopes: Vec<String>,
    ) -> Result<ProjectedRecord> {
        let bytes =
            serde_json::to_vec(payload).map_err(|_| SyncError::new("projection-encoding", 409))?;
        let local_hash = risunest_sync_wire::hash(&bytes);
        let mut objects = BTreeSet::new();
        let segmented = payload::build(&mut Cursor::new(&bytes), |bytes| {
            let hash = self
                .put(bytes)
                .map_err(|_| WireError("cache-write-failed"))?;
            objects.insert(hash);
            Ok(())
        })?;
        let object_hash = self.put(&canonical::encode(&RecordObject {
            schema: "risunest-server-record-v1".into(),
            payload: segmented,
        })?)?;
        objects.extend(dependencies.iter().cloned());
        let (dependency_root, pages) =
            build_reference_tree(&objects.iter().cloned().collect::<Vec<_>>(), false)?;
        for (hash, bytes) in pages {
            self.put(&bytes)?;
            objects.insert(hash);
        }
        let (relation_root, pages) = build_reference_tree(relations, true)?;
        for (hash, bytes) in pages {
            self.put(&bytes)?;
            objects.insert(hash);
        }
        let descriptor = RecordDescriptor {
            object_hash: object_hash.clone(),
            dependency_root,
            relation_root,
            scopes,
        };
        let descriptor_hash = self.put(&descriptor.bytes()?)?;
        objects.insert(object_hash.clone());
        objects.insert(descriptor_hash.clone());
        Ok(ProjectedRecord {
            version: RecordVersion::Live {
                object_hash,
                descriptor_hash: Some(descriptor_hash),
            },
            local_hash,
            objects: objects.into_iter().collect(),
        })
    }
    pub fn restore(&self, version: &RecordVersion) -> Result<(ServerPayload, String)> {
        self.restore_with(version, |hash, limit| self.read(hash, limit))
    }
    pub fn restore_with(
        &self,
        version: &RecordVersion,
        mut read: impl FnMut(&str, usize) -> Result<Vec<u8>>,
    ) -> Result<(ServerPayload, String)> {
        let RecordVersion::Live { object_hash, .. } = version else {
            return Err(SyncError::new("record-is-not-live", 409));
        };
        let object: RecordObject = canonical::decode(
            &self.read(object_hash, MAX_METADATA_BYTES)?,
            MAX_METADATA_BYTES,
        )?;
        if object.schema != "risunest-server-record-v1" {
            return Err(SyncError::new("unsupported-server-record", 409));
        }
        let mut bytes = Vec::new();
        payload::restore(
            &object.payload,
            |hash| read(hash, payload::MAX_CHUNK).map_err(|_| WireError("cached-payload-invalid")),
            &mut bytes,
        )?;
        let payload: ServerPayload = serde_json::from_slice(&bytes)
            .map_err(|_| SyncError::new("invalid-server-payload", 409))?;
        if serde_json::to_vec(&payload)
            .map_err(|_| SyncError::new("invalid-server-payload", 409))?
            != bytes
        {
            return Err(SyncError::new("noncanonical-server-payload", 409));
        }
        Ok((payload, object.payload.content_hash))
    }
    pub fn closure(&self, version: &RecordVersion) -> Result<Vec<String>> {
        let RecordVersion::Live {
            object_hash,
            descriptor_hash: Some(descriptor_hash),
        } = version
        else {
            return Ok(version
                .object_hashes()
                .into_iter()
                .map(str::to_owned)
                .collect());
        };
        let descriptor: RecordDescriptor = canonical::decode(
            &self.read(descriptor_hash, MAX_METADATA_BYTES)?,
            MAX_METADATA_BYTES,
        )?;
        descriptor.validate()?;
        if descriptor.object_hash != *object_hash {
            return Err(SyncError::new("descriptor-record-mismatch", 409));
        }
        let mut hashes = BTreeSet::from([object_hash.clone(), descriptor_hash.clone()]);
        for (root, relations) in [
            (descriptor.dependency_root, false),
            (descriptor.relation_root, true),
        ] {
            if let Some(root) = root {
                visit_reference_tree(
                    &root,
                    relations,
                    |hash| {
                        self.read(hash, MAX_METADATA_BYTES)
                            .map_err(|_| WireError("cached-descriptor-invalid"))
                    },
                    |hash, page| {
                        if page || !relations {
                            hashes.insert(hash.into());
                        }
                        Ok(())
                    },
                )?;
            }
        }
        Ok(hashes.into_iter().collect())
    }
    /// Candidate metadata/chunks belong to this record. The thousands of asset
    /// bodies reachable from an owner manifest are not a candidate inventory to
    /// stat and transmit on every small owner edit.
    pub fn base_candidates(&self, key: &str, version: &RecordVersion) -> Result<Vec<String>> {
        use crate::logical_records::{
            decode_logical_record_key, LogicalRecordEnvelope as Envelope,
            LogicalRecordLocator as Locator,
        };
        let RecordVersion::Live {
            object_hash,
            descriptor_hash: Some(descriptor_hash),
        } = version
        else {
            return Ok(Vec::new());
        };
        let descriptor: RecordDescriptor = canonical::decode(
            &self.read(descriptor_hash, MAX_METADATA_BYTES)?,
            MAX_METADATA_BYTES,
        )?;
        descriptor.validate()?;
        if descriptor.object_hash != *object_hash {
            return Err(SyncError::new("descriptor-record-mismatch", 409));
        }
        let mut hashes = BTreeSet::from([object_hash.clone(), descriptor_hash.clone()]);
        for (root, relations) in [
            (descriptor.dependency_root, false),
            (descriptor.relation_root, true),
        ] {
            if let Some(root) = root {
                visit_reference_tree(
                    &root,
                    relations,
                    |hash| {
                        self.read(hash, MAX_METADATA_BYTES)
                            .map_err(|_| WireError("cached-descriptor-invalid"))
                    },
                    |hash, page| {
                        if page {
                            hashes.insert(hash.into());
                        }
                        Ok(())
                    },
                )?;
            }
        }
        let object: RecordObject = canonical::decode(
            &self.read(object_hash, MAX_METADATA_BYTES)?,
            MAX_METADATA_BYTES,
        )?;
        if object.schema != "risunest-server-record-v1" {
            return Err(SyncError::new("unsupported-server-record", 409));
        }
        let mut pending = vec![(object.payload.root, 0)];
        let mut seen = BTreeSet::new();
        while let Some((hash, depth)) = pending.pop() {
            if depth >= 8 || !seen.insert(hash.clone()) || seen.len() > payload::MAX_PARTS {
                return Err(SyncError::new("cached-payload-invalid", 409));
            }
            hashes.insert(hash.clone());
            let index: payload::Index =
                canonical::decode(&self.read(&hash, MAX_METADATA_BYTES)?, MAX_METADATA_BYTES)?;
            match index {
                payload::Index::Chunks { parts } => {
                    hashes.extend(parts.into_iter().map(|p| p.hash));
                }
                payload::Index::Branches { parts } => {
                    pending.extend(parts.into_iter().map(|p| (p.hash, depth + 1)));
                }
            }
        }
        let locator = decode_logical_record_key(key)
            .map_err(|_| SyncError::new("invalid-server-key", 409))?;
        if matches!(
            locator,
            Locator::Root
                | Locator::Character { .. }
                | Locator::Asset { .. }
                | Locator::Inlay { .. }
                | Locator::Cold { .. }
        ) {
            match self.restore(version)?.0.record {
                Envelope::Root { owner_heads, .. } | Envelope::Character { owner_heads, .. } => {
                    hashes.extend(owner_heads.into_iter().filter_map(|h| h.manifest_hash))
                }
                Envelope::Asset { object_hash, .. }
                | Envelope::Inlay { object_hash, .. }
                | Envelope::Cold { object_hash, .. } => hashes.extend(object_hash),
                _ => (),
            }
        }
        Ok(hashes.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical_records::LogicalRecordEnvelope;
    #[test]
    fn candidate_inventory_excludes_a_hundred_thousand_opaque_dependencies() {
        use crate::logical_records::{encode_logical_record_key, LogicalRecordLocator};
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let payload = ServerPayload {
            record: LogicalRecordEnvelope::Plugin {
                ordinal: 0,
                value: serde_json::json!("synthetic"),
            },
            messages: None,
            derived_objects: Default::default(),
        };
        let mut dependencies = (0u32..100_000)
            .map(|n| risunest_sync_wire::hash(&n.to_be_bytes()))
            .collect::<Vec<_>>();
        dependencies.sort();
        let projected = cache
            .project(&payload, &dependencies, &[], vec!["plugin-storage".into()])
            .unwrap();
        let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: "synthetic".into(),
        })
        .unwrap();
        let candidates = cache.base_candidates(&key, &projected.version).unwrap();
        assert!(candidates.len() < 2000);
        assert!(candidates
            .iter()
            .all(|hash| dependencies.binary_search(hash).is_err()));
        assert!(cache.closure(&projected.version).unwrap().len() >= 100_000);
        // Only control pages and the record's segmented payload were materialized
        // by this unit fixture; selecting candidates must not stat the raw bodies.
        assert!(dependencies.iter().take(3).all(|hash| cache
            .cas
            .stat_object(hash)
            .unwrap()
            .is_none()));
    }
    #[test]
    fn cached_record_preserves_exact_large_plugin_bytes_and_rejects_missing_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let payload = ServerPayload {
            derived_objects: Default::default(),
            record: LogicalRecordEnvelope::Plugin {
                ordinal: 0,
                value: serde_json::json!({"synthetic":"가🦀x".repeat(1_000_000),"empty":"","ordered":[null,false,1]}),
            },
            messages: None,
        };
        let projected = cache
            .project(&payload, &[], &[], vec!["plugin-storage".into()])
            .unwrap();
        let (restored, hash) = cache.restore(&projected.version).unwrap();
        assert_eq!(hash, projected.local_hash);
        assert_eq!(
            serde_json::to_vec(&restored).unwrap(),
            serde_json::to_vec(&payload).unwrap()
        );
        assert_eq!(
            cache.closure(&projected.version).unwrap(),
            projected.objects
        );
        let missing_dir = tempfile::tempdir().unwrap();
        let missing = Cache::open(missing_dir.path()).unwrap();
        assert!(missing.restore(&projected.version).is_err());
    }
}
