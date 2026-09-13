//! Application eligibility is independent of ZIP/catalog integrity. SQL values remain untouched.
use super::*;
use crate::{
    lossless_f0::{scan_portable_fragment, F0Reference, F0ReferenceStatus, PortableFragment},
    persistent_store::{
        portable_validation, AssetRepositoryAuthorityState, ColdPayloadAuthorityState,
    },
};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct ReferenceCounts {
    pub(crate) total: u64,
    pub(crate) expected_missing: u64,
}

impl VerifiedArchive {
    pub(crate) fn validate_library(
        &self,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        if !self.manifest.library_included
            || self.manifest.repair_required
            || self.manifest.profile != Profile::Portable
        {
            return Err(Error::Invalid(
                "archive library requires repair or is not included",
            ));
        }
        LibraryView {
            db: &self.db,
            objects: self,
        }
        .validate(probe)
    }
}
impl Catalog {
    pub(crate) fn validate_library(
        &self,
        probe: &dyn CancellationProbe,
    ) -> Result<ReferenceCounts> {
        LibraryView {
            db: &self.db,
            objects: self,
        }
        .validate(probe)
    }
}
trait LibraryObjects {
    fn read_object(&self, hash: &str) -> Result<(std::io::Take<std::fs::File>, u64)>;
}
impl LibraryObjects for VerifiedArchive {
    fn read_object(&self, hash: &str) -> Result<(std::io::Take<std::fs::File>, u64)> {
        self.open_object(hash)
    }
}
impl LibraryObjects for Catalog {
    fn read_object(&self, hash: &str) -> Result<(std::io::Take<std::fs::File>, u64)> {
        let (path,size):(String,i64)=self.db.query_row("SELECT s.path,o.byte_length FROM sources s JOIN objects o ON o.sha256=s.sha256 WHERE o.sha256=?1",[hex::decode(hash).map_err(|_|Error::Invalid("invalid object hash"))?],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let size = sql_u64(size)?;
        Ok((std::fs::File::open(path)?.take(size), size))
    }
}
struct LibraryView<'a> {
    db: &'a rusqlite::Connection,
    objects: &'a dyn LibraryObjects,
}
impl LibraryView<'_> {
    fn open_object(&self, hash: &str) -> Result<(std::io::Take<std::fs::File>, u64)> {
        self.objects.read_object(hash)
    }
    fn validate(&self, probe: &dyn CancellationProbe) -> Result<ReferenceCounts> {
        portable_validation::validate_records(&self.db, probe)?;
        let asset: String =
            self.db
                .query_row("SELECT value FROM asset_repository_authority", [], |r| {
                    r.get(0)
                })?;
        let cold: String =
            self.db
                .query_row("SELECT value FROM cold_payload_authority", [], |r| r.get(0))?;
        if !matches!(
            serde_json::from_str::<AssetRepositoryAuthorityState>(&asset)?,
            AssetRepositoryAuthorityState::V2 { .. }
        ) || !matches!(
            serde_json::from_str::<ColdPayloadAuthorityState>(&cold)?,
            ColdPayloadAuthorityState::V2 { .. }
        ) {
            return Err(Error::Invalid(
                "portable activation requires complete v2 storage authority",
            ));
        }
        // Every registered alias needs an exact file binding, independently of reference scans.
        for sql in [
            "SELECT EXISTS(SELECT 1 FROM asset_aliases a WHERE a.object_hash IS NULL OR NOT EXISTS(SELECT 1 FROM files f JOIN objects o ON f.object_hash=o.sha256 WHERE f.kind=a.kind AND f.logical_key=a.logical_key AND f.state='present' AND lower(hex(o.sha256))=a.object_hash AND o.byte_length=a.size))",
            "SELECT EXISTS(SELECT 1 FROM cold_aliases a WHERE a.object_hash IS NULL OR NOT EXISTS(SELECT 1 FROM files f JOIN objects o ON f.object_hash=o.sha256 WHERE f.kind='cold' AND f.logical_key=a.key AND f.state='present' AND lower(hex(o.sha256))=a.object_hash AND o.byte_length=a.size))",
            "SELECT EXISTS(SELECT 1 FROM files WHERE state!='present')",
        ] {if self.db.query_row(sql,[],|r|r.get::<_,bool>(0))? {return Err(Error::Invalid("portable registered payload inventory is incomplete"));}}
        let root: Value = serde_json::from_str(&self.db.query_row::<String, _, _>(
            "SELECT value FROM root",
            [],
            |r| r.get(0),
        )?)?;
        self.validate_owners(&root, probe)?;
        let mut counts = ReferenceCounts::default();
        let selected = selected_row(
            &self.db,
            "SELECT value FROM bot_presets ORDER BY configured_index LIMIT 1 OFFSET ?1",
            root.get("botPresetsId"),
        )?;
        self.validate_fragment(
            PortableFragment::Root {
                value: &root,
                selected_preset: selected.as_ref(),
            },
            &root,
            &mut counts,
            probe,
        )?;
        for (sql,kind) in [("SELECT preset_id,configured_index,value FROM bot_presets ORDER BY configured_index","preset"),("SELECT storage_key,ordinal,value FROM plugin_storage ORDER BY ordinal","plugin")] {
            let mut statement=self.db.prepare(sql)?;let mut rows=statement.query([])?;
            while let Some(row)=rows.next()? {check(probe)?;let key:String=row.get(0)?;let index:i64=row.get(1)?;let value:Value=serde_json::from_str(&row.get::<_,String>(2)?)?;
                let fragment=if kind=="preset" {PortableFragment::Preset{value:&value,index}}else{PortableFragment::Plugin{value:&value,key:&key}};
                self.validate_fragment(fragment,&root,&mut counts,probe)?;
            }
        }
        let mut statement=self.db.prepare("SELECT character_id,detail,conversation_count FROM characters ORDER BY configured_index")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let id: String = row.get(0)?;
            let value: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let selected=match value.get("chatPage").and_then(Value::as_u64).and_then(|n|i64::try_from(n).ok()) {Some(index)=>self.db.query_row("SELECT detail FROM conversations WHERE character_id=?1 ORDER BY configured_index LIMIT 1 OFFSET ?2",params![id,index],|r|r.get::<_,String>(0)).optional()?.map(|v|serde_json::from_str(&v)).transpose()?,None=>None};
            self.validate_fragment(
                PortableFragment::Character {
                    value: &value,
                    selected_chat: selected.as_ref(),
                    has_chats: row.get::<_, i64>(2)? > 0,
                },
                &root,
                &mut counts,
                probe,
            )?;
        }
        let mut statement = self.db.prepare(
            "SELECT character_id,detail FROM conversations ORDER BY character_id,configured_index",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let id: String = row.get(0)?;
            let value: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
            self.validate_fragment(
                PortableFragment::Conversation {
                    value: &value,
                    character_id: &id,
                },
                &root,
                &mut counts,
                probe,
            )?;
        }
        let mut statement=self.db.prepare("SELECT character_id,conversation_id,message_index,value FROM messages ORDER BY character_id,conversation_id,message_index")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let character: String = row.get(0)?;
            let conversation: String = row.get(1)?;
            let value: Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
            self.validate_fragment(
                PortableFragment::Message {
                    value: &value,
                    character_id: &character,
                    conversation_id: &conversation,
                    index: row.get(2)?,
                },
                &root,
                &mut counts,
                probe,
            )?;
        }
        let mut statement = self
            .db
            .prepare("SELECT key,object_hash FROM cold_aliases ORDER BY key")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let key: String = row.get(0)?;
            let hash: String = row.get(1)?;
            let (input, _) = self.open_object(&hash)?;
            let value = crate::cold_payload_codec::decode_cold_json(input, 64 * 1024 * 1024)?;
            self.validate_fragment(
                PortableFragment::Cold {
                    value: &value,
                    key: &key,
                },
                &root,
                &mut counts,
                probe,
            )?;
        }
        Ok(counts)
    }

    fn validate_fragment(
        &self,
        fragment: PortableFragment<'_>,
        root: &Value,
        counts: &mut ReferenceCounts,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let references = scan_portable_fragment(fragment)
            .map_err(|_| Error::Invalid("portable F0 value shape is invalid"))?;
        for reference in references {
            check(probe)?;
            counts.total += 1;
            if matches!(
                reference.status,
                F0ReferenceStatus::Invalid | F0ReferenceStatus::External
            ) {
                continue;
            }
            if !self.reference_present(&reference, root)? {
                counts.expected_missing += 1;
            }
        }
        // Complete v2 aliases and raw rows prove absence. Existing dangling references remain
        // expected missing, as in the previous F0 capture contract; registered missing bytes fail
        // above and cannot be reclassified as a harmless absent reference.
        Ok(())
    }

    fn reference_present(&self, r: &F0Reference, root: &Value) -> Result<bool> {
        let key = &r.target_key;
        Ok(match r.target_kind.as_str() {
            "asset" | "inlay" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM asset_aliases WHERE kind=?1 AND logical_key=?2)",
                params![r.target_kind, key],
                |row| row.get(0),
            )?,
            "cold" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM cold_aliases WHERE key=?1)",
                [key],
                |row| row.get(0),
            )?,
            "character" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM characters WHERE character_id=?1)",
                [key],
                |row| row.get(0),
            )?,
            "preset" => self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM bot_presets WHERE name=?1)",
                [key],
                |row| row.get(0),
            )?,
            "module" => {
                named(root, "modules", "id", key)
                    || root
                        .get("personas")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            items.iter().any(|v| {
                                v.pointer("/embeddedModule/id").and_then(Value::as_str) == Some(key)
                            })
                        })
            }
            "persona" => named(root, "personas", "id", key),
            "loadout" => named(root, "loadouts", "name", key),
            "conversation" | "folder" => {
                let scope = r
                    .metadata
                    .get("characterId")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        matches!(r.owner_kind.as_str(), "character" | "group")
                            .then_some(r.owner_id.as_str())
                    });
                if let Some(scope) = scope {
                    if r.target_kind == "conversation" {
                        self.db.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE character_id=?1 AND conversation_id=?2)",params![scope,key],|row|row.get(0))?
                    } else {
                        let detail: Option<String> = self
                            .db
                            .query_row(
                                "SELECT detail FROM characters WHERE character_id=?1",
                                [scope],
                                |row| row.get(0),
                            )
                            .optional()?;
                        detail
                            .map(|v| serde_json::from_str::<Value>(&v))
                            .transpose()?
                            .is_some_and(|v| named(&v, "chatFolders", "id", key))
                    }
                } else {
                    false
                }
            }
            _ => return Err(Error::Invalid("unknown F0 reference kind")),
        })
    }

    fn validate_owners(&self, root: &Value, probe: &dyn CancellationProbe) -> Result<()> {
        let mut statement=self.db.prepare("SELECT owner_kind,owner_locator,present,manifest_hash,entry_count FROM asset_owner_heads ORDER BY owner_kind,owner_locator")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            check(probe)?;
            let kind: String = row.get(0)?;
            let locator: String = row.get(1)?;
            let present: bool = row.get(2)?;
            let character: Option<Value> = if kind == "character-additional-assets" {
                self.db
                    .query_row(
                        "SELECT detail FROM characters WHERE character_id=?1",
                        [&locator],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|v| serde_json::from_str(&v))
                    .transpose()?
            } else {
                None
            };
            let index = locator.parse::<usize>().ok();
            let (parent, property) = match kind.as_str() {
                "character-additional-assets" => (character.as_ref(), "additionalAssets"),
                "root-module-assets" => (
                    index.and_then(|i| root.get("modules")?.as_array()?.get(i)),
                    "assets",
                ),
                "persona-embedded-module-assets" => (
                    index.and_then(|i| {
                        root.get("personas")?
                            .as_array()?
                            .get(i)?
                            .get("embeddedModule")
                    }),
                    "assets",
                ),
                _ => return Err(Error::Invalid("invalid owner kind")),
            };
            let parent = parent
                .and_then(Value::as_object)
                .ok_or(Error::Invalid("owner parent is missing"))?;
            if parent.contains_key(property) != present {
                return Err(Error::Invalid("owner property presence differs"));
            }
            if !present {
                continue;
            }
            let hash: String = row.get(3)?;
            let (mut input, size) = self.open_object(&hash)?;
            if size > 64 * 1024 * 1024 {
                return Err(Error::Invalid("owner manifest exceeds decoding limit"));
            }
            let mut bytes = Vec::with_capacity(size as usize);
            input.read_to_end(&mut bytes)?;
            let entries =
                crate::asset_repository::owner_manifest_codec::decode_owner_manifest(&bytes)
                    .map_err(|_| Error::Invalid("invalid owner manifest"))?;
            let tuples = parent
                .get(property)
                .and_then(Value::as_array)
                .ok_or(Error::Invalid("owner property is not an array"))?;
            if entries.len()
                != usize::try_from(row.get::<_, i64>(4)?)
                    .map_err(|_| Error::Invalid("owner count overflow"))?
                || entries.len() != tuples.len()
            {
                return Err(Error::Invalid("owner entry count differs"));
            }
            for (entry, tuple) in entries.iter().zip(tuples) {
                check(probe)?;
                let tuple = tuple
                    .as_array()
                    .ok_or(Error::Invalid("invalid owner tuple"))?;
                if tuple.len() < 3
                    || (kind == "character-additional-assets" && tuple.len() != 3)
                    || !tuple[..3]
                        .iter()
                        .zip(&entry.tuple)
                        .all(|(a, b)| a.as_str() == Some(b))
                {
                    return Err(Error::Invalid("owner tuples differ"));
                }
                if let Some(hash) = entry.payload_hash {
                    if !self.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM objects WHERE sha256=?1)",
                        [hash.as_slice()],
                        |r| r.get::<_, bool>(0),
                    )? {
                        return Err(Error::Invalid("owner historical payload is missing"));
                    }
                }
            }
        }
        Ok(())
    }
}

fn named(value: &Value, collection: &str, field: &str, key: &str) -> bool {
    value
        .get(collection)
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|v| v.get(field).and_then(Value::as_str) == Some(key))
        })
}
fn selected_row(
    db: &rusqlite::Connection,
    sql: &str,
    index: Option<&Value>,
) -> Result<Option<Value>> {
    let Some(index) = index
        .and_then(Value::as_u64)
        .and_then(|n| i64::try_from(n).ok())
    else {
        return Ok(None);
    };
    Ok(db
        .query_row(sql, [index], |r| r.get::<_, String>(0))
        .optional()?
        .map(|v| serde_json::from_str(&v))
        .transpose()?)
}
