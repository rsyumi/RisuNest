use super::{
    query, AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState, ReadTarget,
    StoreError, StoreResult,
};
use crate::asset_repository::{owner_manifest_codec, PayloadCas};
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub(super) struct OwnerManifestProjector {
    cas: Option<PayloadCas>,
    heads: HashMap<AssetOwnerLocator, AssetOwnerHead>,
}

impl OwnerManifestProjector {
    pub(super) fn from_snapshots_dir(
        connection: &Connection,
        target: &ReadTarget,
        snapshots_dir: &Path,
    ) -> StoreResult<Self> {
        let authority = query::read_asset_repository_authority(connection, target)?.value;
        match authority {
            AssetRepositoryAuthorityState::Legacy => Ok(Self {
                cas: None,
                heads: HashMap::new(),
            }),
            AssetRepositoryAuthorityState::Preparing { .. } => Err(validation(
                "owner manifest projection cannot read a preparing asset repository",
            )),
            AssetRepositoryAuthorityState::V2 { .. } => {
                let persistent_dir = snapshots_dir.parent().ok_or_else(|| {
                    validation("owner manifest projection cannot locate the persistent directory")
                })?;
                let repository_root = persistent_dir.parent().ok_or_else(|| {
                    validation("owner manifest projection cannot locate the repository root")
                })?;
                let heads = query::list_asset_owner_heads(connection, target)?
                    .value
                    .into_iter()
                    .map(|head| (head.owner.clone(), head))
                    .collect();
                Ok(Self {
                    cas: Some(PayloadCas::new(repository_root).map_err(|error| {
                        validation(format!("owner manifest repository is unavailable: {error}"))
                    })?),
                    heads,
                })
            }
        }
    }

    pub(super) fn project_database(&self, database: &mut Value) -> StoreResult<()> {
        if self.cas.is_none() {
            return Ok(());
        }
        let root = database
            .as_object_mut()
            .ok_or_else(|| validation("owner manifest projection requires an object database"))?;
        self.project_root(root)?;

        let characters = root
            .get_mut("characters")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| validation("owner manifest projection requires the character array"))?;
        let mut character_ids = HashSet::with_capacity(characters.len());
        for character in characters {
            let character = character.as_object_mut().ok_or_else(|| {
                validation("owner manifest projection requires object character details")
            })?;
            let character_id = character
                .get("chaId")
                .and_then(Value::as_str)
                .ok_or_else(|| validation("owner manifest projection requires a character ID"))?
                .to_owned();
            character_ids.insert(character_id.clone());
            self.project_character(&character_id, character)?;
        }
        self.validate_character_owners(&character_ids)
    }

    pub(super) fn project_root(&self, root: &mut Map<String, Value>) -> StoreResult<()> {
        if self.cas.is_none() {
            return Ok(());
        }
        for (owner, head) in &self.heads {
            let (parent, property) = match owner {
                AssetOwnerLocator::RootModuleAssets { index } => {
                    let index = usize::try_from(*index).map_err(|_| {
                        validation("owner manifest root module index does not fit this platform")
                    })?;
                    let parent = root
                        .get_mut("modules")
                        .and_then(Value::as_array_mut)
                        .and_then(|modules| modules.get_mut(index))
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| {
                            validation("owner manifest root module occurrence does not exist")
                        })?;
                    (parent, "assets")
                }
                AssetOwnerLocator::PersonaEmbeddedModuleAssets { index } => {
                    let index = usize::try_from(*index).map_err(|_| {
                        validation("owner manifest persona module index does not fit this platform")
                    })?;
                    let parent = root
                        .get_mut("personas")
                        .and_then(Value::as_array_mut)
                        .and_then(|personas| personas.get_mut(index))
                        .and_then(Value::as_object_mut)
                        .and_then(|persona| persona.get_mut("embeddedModule"))
                        .and_then(Value::as_object_mut)
                        .ok_or_else(|| {
                            validation("owner manifest persona module occurrence does not exist")
                        })?;
                    (parent, "assets")
                }
                AssetOwnerLocator::CharacterAdditionalAssets { .. } => continue,
            };
            self.apply_head(head, parent, property)?;
        }
        Ok(())
    }

    pub(super) fn project_character(
        &self,
        character_id: &str,
        character: &mut Map<String, Value>,
    ) -> StoreResult<()> {
        let owner = AssetOwnerLocator::CharacterAdditionalAssets {
            character_id: character_id.to_owned(),
        };
        if let Some(head) = self.heads.get(&owner) {
            self.apply_head(head, character, "additionalAssets")?;
        }
        Ok(())
    }

    pub(super) fn validate_character_owners(
        &self,
        character_ids: &HashSet<String>,
    ) -> StoreResult<()> {
        for owner in self.heads.keys() {
            if let AssetOwnerLocator::CharacterAdditionalAssets { character_id } = owner {
                if !character_ids.contains(character_id) {
                    return Err(validation(
                        "owner manifest character occurrence does not exist",
                    ));
                }
            }
        }
        Ok(())
    }

    fn apply_head(
        &self,
        head: &AssetOwnerHead,
        parent: &mut Map<String, Value>,
        property: &str,
    ) -> StoreResult<()> {
        if parent.contains_key(property) != head.present {
            return Err(validation(
                "owner manifest property presence does not match pinned legacy data",
            ));
        }
        if !head.present {
            return Ok(());
        }

        let manifest_hash = head
            .manifest_hash
            .as_deref()
            .ok_or_else(|| validation("owner manifest head is missing its manifest hash"))?;
        let bytes = self
            .cas
            .as_ref()
            .expect("v2 projection has a CAS")
            .read_object(manifest_hash)
            .map_err(|error| {
                validation(format!(
                    "owner manifest {manifest_hash} cannot be read: {error}"
                ))
            })?
            .ok_or_else(|| validation(format!("owner manifest {manifest_hash} is missing")))?;
        if owner_manifest_codec::owner_manifest_identity(&bytes) != manifest_hash {
            return Err(validation(format!(
                "owner manifest {manifest_hash} content hash mismatch"
            )));
        }
        let entries = owner_manifest_codec::decode_owner_manifest(&bytes).map_err(|error| {
            validation(format!(
                "owner manifest {manifest_hash} is invalid: {error}"
            ))
        })?;
        if entries.len() as i64 != head.entry_count {
            return Err(validation(format!(
                "owner manifest {manifest_hash} entry count mismatch"
            )));
        }
        let tuples = Value::Array(
            entries
                .into_iter()
                .map(|entry| Value::Array(entry.tuple.into_iter().map(Value::String).collect()))
                .collect(),
        );
        if parent.get(property) != Some(&tuples) {
            return Err(validation(
                "owner manifest does not match pinned legacy tuples",
            ));
        }
        parent.insert(property.to_owned(), tuples);
        Ok(())
    }
}

fn validation(message: impl Into<String>) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
