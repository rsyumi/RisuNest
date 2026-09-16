use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const MAX_COLD_DECODED_BYTES: usize = 64 * 1024 * 1024;
const COLD_COPY_BUFFER_BYTES: usize = 64 * 1024;
const COLD_STORAGE_HEADER: &str = "\u{ef01}COLDSTORAGE\u{ef01}";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum F0PayloadKind {
    Asset,
    Inlay,
    Cold,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct F0PayloadDescriptor {
    pub(crate) kind: F0PayloadKind,
    pub(crate) key: String,
    pub(crate) sha256: String,
    pub(crate) byte_length: u64,
    pub(crate) metadata: Value,
    pub(crate) cold_source: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct F0ExpectedMissing {
    pub(crate) target_kind: String,
    pub(crate) target_key: String,
    pub(crate) character_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum F0ReferenceStatus {
    Present,
    ExpectedMissing,
    UnexpectedMissing,
    External,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct F0Reference {
    pub(crate) owner_kind: String,
    pub(crate) owner_id: String,
    pub(crate) source_path: String,
    pub(crate) occurrence: u64,
    pub(crate) target_kind: String,
    pub(crate) target_key: String,
    pub(crate) status: F0ReferenceStatus,
    pub(crate) metadata: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct F0Validation {
    pub(crate) canonical_database_sha256: String,
    pub(crate) reference_graph_sha256: String,
    pub(crate) references: Vec<F0Reference>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum F0ErrorCode {
    CanonicalValue,
    InvalidDatabase,
    InvalidInventory,
    ColdPayload,
    UnexpectedMissing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct F0Error {
    pub(crate) code: F0ErrorCode,
    pub(crate) message: String,
}

impl std::fmt::Display for F0Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for F0Error {}

pub(crate) fn validate_f0_v1(
    database: &Value,
    payloads: &[F0PayloadDescriptor],
    expected_missing: &[F0ExpectedMissing],
) -> Result<F0Validation, F0Error> {
    let validation = rebuild_f0_v1(database, payloads, expected_missing)?;
    if let Some(reference) = validation
        .references
        .iter()
        .find(|reference| reference.status == F0ReferenceStatus::UnexpectedMissing)
    {
        return Err(F0Error {
            code: F0ErrorCode::UnexpectedMissing,
            message: format!(
                "unexpected missing F0 reference at {}: {}:{}",
                reference.source_path, reference.target_kind, reference.target_key
            ),
        });
    }
    Ok(validation)
}

pub(crate) fn rebuild_f0_v1(
    database: &Value,
    payloads: &[F0PayloadDescriptor],
    expected_missing: &[F0ExpectedMissing],
) -> Result<F0Validation, F0Error> {
    let indexes = ReferenceIndexes::new(database, payloads, expected_missing)?;
    let mut collector = GraphCollector::new(&indexes);
    scan_database_root(database, &mut collector)?;
    scan_database_collections(database, &mut collector)?;
    scan_characters(database, &mut collector)?;
    scan_cold_payloads(payloads, &mut collector)?;
    let reference_graph_sha256 = reference_graph_sha256(&collector.references)?;
    Ok(F0Validation {
        canonical_database_sha256: canonical_sha256(database)?,
        reference_graph_sha256,
        references: collector.references,
    })
}

/// The raw portable path applies these same F0 scanners to one stored value at a time. Target
/// membership is checked against its SQLite inventory, avoiding a materialized database graph.
pub(crate) enum PortableFragment<'a> {
    Root {
        value: &'a Value,
        selected_preset: Option<&'a Value>,
    },
    Preset {
        value: &'a Value,
        index: i64,
    },
    Plugin {
        value: &'a Value,
        key: &'a str,
    },
    Character {
        value: &'a Value,
        selected_chat: Option<&'a Value>,
        has_chats: bool,
    },
    Conversation {
        value: &'a Value,
        character_id: &'a str,
    },
    Message {
        value: &'a Value,
        character_id: &'a str,
        conversation_id: &'a str,
        index: i64,
    },
}

pub(crate) fn scan_portable_fragment(
    fragment: PortableFragment<'_>,
) -> Result<Vec<F0Reference>, F0Error> {
    let indexes = ReferenceIndexes {
        present: HashMap::new(),
        expected_missing: HashSet::new(),
        conversations_by_character: HashMap::new(),
        folders_by_character: HashMap::new(),
    };
    let mut collector = GraphCollector::new(&indexes);
    match fragment {
        PortableFragment::Root {
            value,
            selected_preset,
        } => {
            scan_database_root(value, &mut collector)?;
            if let Some(name) = selected_preset
                .and_then(|v| v.get("name"))
                .filter(|v| v.is_string())
            {
                if let Some(reference) = collector
                    .references
                    .iter_mut()
                    .find(|r| r.owner_kind == "root" && r.source_path == "$.botPresetsId")
                {
                    reference.target_key = display_key(Some(name));
                }
            }
            scan_database_collections(value, &mut collector)?;
        }
        PortableFragment::Preset { value, index } => {
            let fallback = format!("#{index}");
            let owner = Owner {
                kind: "preset",
                id: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&fallback),
            };
            collector.optional(
                owner,
                "$.image",
                "asset",
                value.get("image"),
                object(&[("field", json_string("image"))]),
            );
            scan_inlays(value, owner, "$", &mut collector);
        }
        PortableFragment::Plugin { value, key } => scan_inlays(
            value,
            Owner {
                kind: "plugin-storage",
                id: key,
            },
            "$",
            &mut collector,
        ),
        PortableFragment::Character {
            value,
            selected_chat,
            has_chats,
        } => {
            scan_characters(&serde_json::json!({"characters":[value]}), &mut collector)?;
            if has_chats {
                let character_id = value
                    .get("chaId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let index = value.get("chatPage");
                let key = selected_chat
                    .and_then(|v| v.get("id"))
                    .filter(|v| v.is_string())
                    .cloned()
                    .unwrap_or_else(|| Value::String(format!("#{}", display_template(index))));
                collector.emit(
                    Owner {
                        kind: if value.get("type").and_then(Value::as_str) == Some("group") {
                            "group"
                        } else {
                            "character"
                        },
                        id: character_id,
                    },
                    "$.chatPage",
                    "conversation",
                    Some(&key),
                    object(&[
                        ("characterId", json_string(character_id)),
                        ("index", index.cloned().unwrap_or(Value::Null)),
                    ]),
                );
            }
        }
        PortableFragment::Conversation {
            value,
            character_id,
        } => {
            scan_characters(
                &serde_json::json!({"characters":[{"chaId":character_id,"chats":[value]}]}),
                &mut collector,
            )?;
            collector
                .references
                .retain(|r| r.owner_kind == "conversation");
        }
        PortableFragment::Message {
            value,
            character_id,
            conversation_id,
            index,
        } => {
            let id = format!("{character_id}/{conversation_id}");
            let owner = Owner {
                kind: "conversation",
                id: &id,
            };
            if index == 0 {
                if let Some(data) = value
                    .get("data")
                    .and_then(Value::as_str)
                    .and_then(|v| v.strip_prefix(COLD_STORAGE_HEADER))
                {
                    collector.emit(
                        owner,
                        "$.message[0].data",
                        "cold",
                        Some(&json_string(data)),
                        object(&[("encoding", json_string("cold-storage-header"))]),
                    );
                }
            }
            scan_inlays(value, owner, &format!("$.message[{index}]"), &mut collector);
        }
    }
    Ok(collector.references)
}

#[derive(Clone, Copy)]
struct Owner<'a> {
    kind: &'a str,
    id: &'a str,
}

struct ReferenceIndexes {
    present: TargetIndex,
    expected_missing: HashSet<(String, String, Option<String>)>,
    conversations_by_character: HashMap<String, HashSet<String>>,
    folders_by_character: HashMap<String, HashSet<String>>,
}

type TargetIndex = HashMap<String, HashSet<String>>;

impl ReferenceIndexes {
    fn new(
        database: &Value,
        payloads: &[F0PayloadDescriptor],
        expected_missing: &[F0ExpectedMissing],
    ) -> Result<Self, F0Error> {
        let mut present = TargetIndex::new();
        for payload in payloads {
            let kind = payload_kind_name(payload.kind);
            if payload.key.is_empty() || !insert_target(&mut present, kind, &payload.key) {
                return Err(F0Error {
                    code: F0ErrorCode::InvalidInventory,
                    message: format!("duplicate or empty F0 payload key: {kind}:{}", payload.key),
                });
            }
            validate_payload_descriptor(payload)?;
        }
        let mut expected_missing_set = HashSet::new();
        for reference in expected_missing {
            let scoped = matches!(reference.target_kind.as_str(), "conversation" | "folder");
            if !is_target_kind(&reference.target_kind)
                || reference.target_key.is_empty()
                || if scoped {
                    !reference
                        .character_id
                        .as_ref()
                        .is_some_and(|id| !id.is_empty())
                } else {
                    reference.character_id.is_some()
                }
            {
                return Err(F0Error {
                    code: F0ErrorCode::InvalidInventory,
                    message: format!(
                        "invalid F0 expected-missing reference: {}:{}",
                        reference.target_kind, reference.target_key
                    ),
                });
            }
            expected_missing_set.insert((
                reference.target_kind.clone(),
                reference.target_key.clone(),
                reference.character_id.clone(),
            ));
        }
        let mut conversations_by_character = HashMap::new();
        let mut folders_by_character = HashMap::new();
        for character in required_array(database, "characters")? {
            let Some(character) = character.as_object() else {
                continue;
            };
            let character_id = string_field(character, "chaId").unwrap_or_default();
            if !character_id.is_empty() {
                insert_target(&mut present, "character", character_id);
            }
            let conversations: HashSet<String> = array_field(character, "chats")
                .iter()
                .filter_map(|chat| chat.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
            conversations_by_character.insert(character_id.to_owned(), conversations);
            let folders: HashSet<String> = array_field(character, "chatFolders")
                .iter()
                .filter_map(|folder| folder.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect();
            folders_by_character.insert(character_id.to_owned(), folders);
        }
        index_named(database, "botPresets", "name", "preset", &mut present);
        index_named(database, "modules", "id", "module", &mut present);
        index_named(database, "personas", "id", "persona", &mut present);
        for persona in optional_array(database, "personas") {
            if let Some(id) = persona
                .get("embeddedModule")
                .and_then(|module| module.get("id"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                insert_target(&mut present, "module", id);
            }
        }
        index_named(database, "loadouts", "name", "loadout", &mut present);
        for item in optional_array(database, "characterOrder") {
            if let Some(id) = item
                .as_object()
                .and_then(|item| string_field(item, "id"))
                .filter(|id| !id.is_empty())
            {
                insert_target(&mut present, "folder", id);
            }
        }
        for (kind, key, character_id) in &expected_missing_set {
            let is_present = if kind == "conversation" {
                conversations_by_character
                    .get(
                        character_id
                            .as_deref()
                            .expect("validated conversation scope"),
                    )
                    .is_some_and(|values| values.contains(key))
            } else if kind == "folder" {
                folders_by_character
                    .get(character_id.as_deref().expect("validated folder scope"))
                    .is_some_and(|values| values.contains(key))
            } else {
                contains_target(&present, kind, key)
            };
            if is_present {
                return Err(F0Error {
                    code: F0ErrorCode::InvalidInventory,
                    message: format!(
                        "F0 expected-missing target is present in the inventory: {kind}:{key}"
                    ),
                });
            }
        }
        Ok(Self {
            present,
            expected_missing: expected_missing_set,
            conversations_by_character,
            folders_by_character,
        })
    }

    fn classify(
        &self,
        owner: Owner<'_>,
        kind: &str,
        raw_key: Option<&Value>,
        key: &str,
        metadata: &Value,
    ) -> F0ReferenceStatus {
        if !raw_key.is_some_and(Value::is_string) || key.is_empty() {
            return F0ReferenceStatus::Invalid;
        }
        if kind == "asset" && is_external_asset_key(key) {
            return F0ReferenceStatus::External;
        }
        let character_id = metadata
            .get("characterId")
            .and_then(Value::as_str)
            .or_else(|| matches!(owner.kind, "character" | "group").then_some(owner.id));
        let present = if kind == "conversation" {
            character_id.is_some_and(|character_id| {
                self.conversations_by_character
                    .get(character_id)
                    .is_some_and(|values| values.contains(key))
            })
        } else if kind == "folder" {
            character_id.is_some_and(|character_id| {
                self.folders_by_character
                    .get(character_id)
                    .is_some_and(|values| values.contains(key))
            })
        } else {
            contains_target(&self.present, kind, key)
        };
        if present {
            F0ReferenceStatus::Present
        } else if self.expected_missing.contains(&(
            kind.to_owned(),
            key.to_owned(),
            matches!(kind, "conversation" | "folder")
                .then_some(character_id)
                .flatten()
                .map(str::to_owned),
        )) {
            F0ReferenceStatus::ExpectedMissing
        } else {
            F0ReferenceStatus::UnexpectedMissing
        }
    }
}

struct GraphCollector<'a> {
    indexes: &'a ReferenceIndexes,
    references: Vec<F0Reference>,
    occurrences: HashMap<(String, String), u64>,
}

impl<'a> GraphCollector<'a> {
    fn new(indexes: &'a ReferenceIndexes) -> Self {
        Self {
            indexes,
            references: Vec::new(),
            occurrences: HashMap::new(),
        }
    }

    fn emit(
        &mut self,
        owner: Owner<'_>,
        path: impl Into<String>,
        kind: &str,
        raw_key: Option<&Value>,
        metadata: Value,
    ) {
        let owner_key = (owner.kind.to_owned(), owner.id.to_owned());
        let occurrence = self.occurrences.entry(owner_key).or_default();
        let target_key = display_key(raw_key);
        let status = self
            .indexes
            .classify(owner, kind, raw_key, &target_key, &metadata);
        self.references.push(F0Reference {
            owner_kind: owner.kind.to_owned(),
            owner_id: owner.id.to_owned(),
            source_path: path.into(),
            occurrence: *occurrence,
            target_kind: kind.to_owned(),
            target_key,
            status,
            metadata,
        });
        *occurrence += 1;
    }

    fn optional(
        &mut self,
        owner: Owner<'_>,
        path: impl Into<String>,
        kind: &str,
        raw_key: Option<&Value>,
        metadata: Value,
    ) {
        if raw_key.is_none_or(|value| value.is_null() || value.as_str() == Some("")) {
            return;
        }
        self.emit(owner, path, kind, raw_key, metadata);
    }
}

fn scan_database_root(database: &Value, collector: &mut GraphCollector<'_>) -> Result<(), F0Error> {
    let owner = Owner {
        kind: "root",
        id: "database",
    };
    collector.optional(
        owner,
        "$.userIcon",
        "asset",
        database.get("userIcon"),
        object(&[("field", json_string("userIcon"))]),
    );
    collector.optional(
        owner,
        "$.customBackground",
        "asset",
        database.get("customBackground"),
        object(&[("field", json_string("customBackground"))]),
    );
    for (index, module_id) in optional_array(database, "enabledModules")
        .iter()
        .enumerate()
    {
        collector.emit(
            owner,
            format!("$.enabledModules[{index}]"),
            "module",
            Some(module_id),
            empty_object(),
        );
    }
    let preset_index = database.get("botPresetsId");
    let selected_preset = selected_array_value(database, "botPresets", preset_index);
    let preset_key = selected_preset
        .and_then(|value| value.get("name"))
        .filter(|value| value.is_string())
        .cloned()
        .unwrap_or_else(|| Value::String(format!("#{}", display_template(preset_index))));
    collector.emit(
        owner,
        "$.botPresetsId",
        "preset",
        Some(&preset_key),
        object(&[("index", preset_index.cloned().unwrap_or(Value::Null))]),
    );
    let persona_index = database.get("selectedPersona");
    let selected_persona = selected_array_value(database, "personas", persona_index);
    let persona_key = selected_persona
        .and_then(|value| value.get("id"))
        .filter(|value| value.is_string())
        .cloned()
        .unwrap_or_else(|| Value::String(format!("#{}", display_template(persona_index))));
    collector.emit(
        owner,
        "$.selectedPersona",
        "persona",
        Some(&persona_key),
        object(&[("index", persona_index.cloned().unwrap_or(Value::Null))]),
    );
    collector.optional(
        owner,
        "$.lastLoadedLoadoutName",
        "loadout",
        database.get("lastLoadedLoadoutName"),
        empty_object(),
    );
    for (index, item) in optional_array(database, "characterOrder")
        .iter()
        .enumerate()
    {
        if item.is_string() {
            collector.emit(
                owner,
                format!("$.characterOrder[{index}]"),
                "character",
                Some(item),
                empty_object(),
            );
            continue;
        }
        let folder = item.as_object().ok_or_else(|| {
            invalid_database("F0 database characterOrder entries must be strings or objects")
        })?;
        let folder_id = string_field(folder, "id").unwrap_or_default();
        let folder_owner = Owner {
            kind: "folder",
            id: folder_id,
        };
        collector.optional(
            folder_owner,
            "$.img",
            "asset",
            folder.get("img"),
            object(&[("field", json_string("img"))]),
        );
        collector.optional(
            folder_owner,
            "$.imgFile",
            "asset",
            folder.get("imgFile"),
            object(&[("field", json_string("imgFile"))]),
        );
        for (character_index, character_id) in array_field(folder, "data").iter().enumerate() {
            collector.emit(
                folder_owner,
                format!("$.data[{character_index}]"),
                "character",
                Some(character_id),
                empty_object(),
            );
        }
        scan_inlays(item, folder_owner, "$", collector);
    }

    let collection_keys = [
        "characters",
        "botPresets",
        "pluginCustomStorage",
        "modules",
        "personas",
        "loadouts",
        "characterOrder",
    ];
    let database = database
        .as_object()
        .ok_or_else(|| invalid_database("F0 database root must be an object"))?;
    for (key, value) in database {
        if collection_keys.contains(&key.as_str()) {
            continue;
        }
        scan_inlays(value, owner, &property_path("$", key), collector);
    }
    Ok(())
}

fn scan_database_collections(
    database: &Value,
    collector: &mut GraphCollector<'_>,
) -> Result<(), F0Error> {
    for module in optional_array(database, "modules") {
        let module = module
            .as_object()
            .ok_or_else(|| invalid_database("F0 database modules must be objects"))?;
        let module_id = string_field(module, "id").unwrap_or_default();
        scan_module(
            module,
            Owner {
                kind: "module",
                id: module_id,
            },
            "$",
            true,
            collector,
        );
    }
    for (index, persona) in optional_array(database, "personas").iter().enumerate() {
        let persona = persona
            .as_object()
            .ok_or_else(|| invalid_database("F0 database personas must be objects"))?;
        let owner_id = string_field(persona, "id")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("#{index}"));
        let owner = Owner {
            kind: "persona",
            id: &owner_id,
        };
        collector.optional(
            owner,
            "$.icon",
            "asset",
            persona.get("icon"),
            object(&[("field", json_string("icon"))]),
        );
        if let Some(embedded_module) = persona.get("embeddedModule").and_then(Value::as_object) {
            collector.emit(
                owner,
                "$.embeddedModule.id",
                "module",
                embedded_module.get("id"),
                empty_object(),
            );
            scan_module(embedded_module, owner, "$.embeddedModule", false, collector);
        }
        scan_inlays(&Value::Object(persona.clone()), owner, "$", collector);
    }
    for (index, preset) in optional_array(database, "botPresets").iter().enumerate() {
        let owner_id = preset
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("#{index}"));
        let owner = Owner {
            kind: "preset",
            id: &owner_id,
        };
        collector.optional(
            owner,
            "$.image",
            "asset",
            preset.get("image"),
            object(&[("field", json_string("image"))]),
        );
        scan_inlays(preset, owner, "$", collector);
    }
    if let Some(storage) = database
        .get("pluginCustomStorage")
        .and_then(Value::as_object)
    {
        for (key, value) in storage {
            scan_inlays(
                value,
                Owner {
                    kind: "plugin-storage",
                    id: key,
                },
                "$",
                collector,
            );
        }
    }
    for loadout in optional_array(database, "loadouts") {
        let loadout = loadout
            .as_object()
            .ok_or_else(|| invalid_database("F0 database loadouts must be objects"))?;
        let owner = Owner {
            kind: "loadout",
            id: string_field(loadout, "name").unwrap_or_default(),
        };
        for (index, character_id) in array_field(loadout, "characterIds").iter().enumerate() {
            collector.emit(
                owner,
                format!("$.characterIds[{index}]"),
                "character",
                Some(character_id),
                empty_object(),
            );
        }
        for (index, module_id) in array_field(loadout, "modules").iter().enumerate() {
            collector.emit(
                owner,
                format!("$.modules[{index}]"),
                "module",
                Some(module_id),
                empty_object(),
            );
        }
        collector.emit(
            owner,
            "$.presetName",
            "preset",
            loadout.get("presetName"),
            empty_object(),
        );
        collector.emit(
            owner,
            "$.personaId",
            "persona",
            loadout.get("personaId"),
            empty_object(),
        );
        for (index, icon) in array_field(loadout, "icons").iter().enumerate() {
            collector.emit(
                owner,
                format!("$.icons[{index}]"),
                "asset",
                Some(icon),
                object(&[("field", json_string("icons"))]),
            );
        }
        scan_inlays(&Value::Object(loadout.clone()), owner, "$", collector);
    }
    Ok(())
}

fn scan_module(
    module: &serde_json::Map<String, Value>,
    owner: Owner<'_>,
    base_path: &str,
    include_inlays: bool,
    collector: &mut GraphCollector<'_>,
) {
    scan_asset_tuples(
        owner,
        &format!("{base_path}.assets"),
        array_field(module, "assets"),
        collector,
    );
    collector.optional(
        owner,
        format!("{base_path}.icon"),
        "asset",
        module.get("icon"),
        object(&[("field", json_string("icon"))]),
    );
    if include_inlays {
        scan_inlays(&Value::Object(module.clone()), owner, base_path, collector);
    }
}

fn scan_characters(database: &Value, collector: &mut GraphCollector<'_>) -> Result<(), F0Error> {
    for character in required_array(database, "characters")? {
        let Some(character) = character.as_object() else {
            return Err(invalid_database("F0 database characters must be objects"));
        };
        let character_id = string_field(character, "chaId").unwrap_or_default();
        let owner = Owner {
            kind: if string_field(character, "type") == Some("group") {
                "group"
            } else {
                "character"
            },
            id: character_id,
        };
        scan_character_assets(character, owner, "$", collector);
        if owner.kind == "group" {
            for (index, character_id) in array_field(character, "characters").iter().enumerate() {
                collector.emit(
                    owner,
                    format!("$.characters[{index}]"),
                    "character",
                    Some(character_id),
                    empty_object(),
                );
            }
        }
        for (index, module_id) in array_field(character, "modules").iter().enumerate() {
            collector.emit(
                owner,
                format!("$.modules[{index}]"),
                "module",
                Some(module_id),
                empty_object(),
            );
        }
        let chats = array_field(character, "chats");
        if !chats.is_empty() {
            let chat_index = character.get("chatPage");
            let selected_chat = selected_slice_value(chats, chat_index);
            let chat_key = selected_chat
                .and_then(|chat| chat.get("id"))
                .filter(|value| value.is_string())
                .cloned()
                .unwrap_or_else(|| Value::String(format!("#{}", display_template(chat_index))));
            collector.emit(
                owner,
                "$.chatPage",
                "conversation",
                Some(&chat_key),
                object(&[
                    ("characterId", json_string(character_id)),
                    ("index", chat_index.cloned().unwrap_or(Value::Null)),
                ]),
            );
        }
        collector.optional(
            owner,
            "$.coldstorage",
            "cold",
            character.get("coldstorage"),
            empty_object(),
        );
        for (index, cold_key) in array_field(character, "coldStoragedChats")
            .iter()
            .enumerate()
        {
            collector.emit(
                owner,
                format!("$.coldStoragedChats[{index}]"),
                "cold",
                Some(cold_key),
                empty_object(),
            );
        }
        for (key, value) in character {
            if key != "chats" {
                scan_inlays(value, owner, &property_path("$", key), collector);
            }
        }
        for (chat_index, chat) in chats.iter().enumerate() {
            let chat_id = chat
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("#{chat_index}"));
            let chat_owner_id = format!("{character_id}/{chat_id}");
            let chat_owner = Owner {
                kind: "conversation",
                id: &chat_owner_id,
            };
            for (index, module_id) in chat
                .get("modules")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .enumerate()
            {
                collector.emit(
                    chat_owner,
                    format!("$.modules[{index}]"),
                    "module",
                    Some(module_id),
                    empty_object(),
                );
            }
            collector.optional(
                chat_owner,
                "$.bindedPersona",
                "persona",
                chat.get("bindedPersona"),
                empty_object(),
            );
            collector.optional(
                chat_owner,
                "$.folderId",
                "folder",
                chat.get("folderId"),
                object(&[("characterId", json_string(character_id))]),
            );
            if let Some(data) = chat
                .get("message")
                .and_then(Value::as_array)
                .and_then(|messages| messages.first())
                .and_then(|message| message.get("data"))
                .and_then(Value::as_str)
                .filter(|data| data.starts_with(COLD_STORAGE_HEADER))
            {
                let cold_key = Value::String(data[COLD_STORAGE_HEADER.len()..].to_owned());
                collector.emit(
                    chat_owner,
                    "$.message[0].data",
                    "cold",
                    Some(&cold_key),
                    object(&[("encoding", json_string("cold-storage-header"))]),
                );
            }
            scan_inlays(chat, chat_owner, "$", collector);
        }
    }
    Ok(())
}

fn scan_character_assets(
    character: &serde_json::Map<String, Value>,
    owner: Owner<'_>,
    base_path: &str,
    collector: &mut GraphCollector<'_>,
) {
    collector.optional(
        owner,
        format!("{base_path}.image"),
        "asset",
        character.get("image"),
        object(&[("field", json_string("image"))]),
    );
    for (index, emotion) in array_field(character, "emotionImages").iter().enumerate() {
        let tuple = emotion.as_array();
        collector.emit(
            owner,
            format!("{base_path}.emotionImages[{index}][1]"),
            "asset",
            tuple.and_then(|tuple| tuple.get(1)),
            object(&[
                (
                    "name",
                    tuple
                        .and_then(|tuple| tuple.first())
                        .and_then(Value::as_str)
                        .map_or_else(|| json_string(""), json_string),
                ),
                ("field", json_string("emotionImages")),
            ]),
        );
    }
    scan_asset_tuples(
        owner,
        &format!("{base_path}.additionalAssets"),
        array_field(character, "additionalAssets"),
        collector,
    );
    if string_field(character, "type") != Some("group") {
        if let Some(files) = character
            .get("vits")
            .and_then(|vits| vits.get("files"))
            .and_then(Value::as_object)
        {
            for (name, key) in files {
                collector.emit(
                    owner,
                    property_path(&format!("{base_path}.vits.files"), name),
                    "asset",
                    Some(key),
                    object(&[
                        ("name", json_string(name)),
                        ("field", json_string("vits.files")),
                    ]),
                );
            }
        }
        for (index, asset) in array_field(character, "ccAssets").iter().enumerate() {
            collector.emit(
                owner,
                format!("{base_path}.ccAssets[{index}].uri"),
                "asset",
                asset.get("uri"),
                object(&[
                    (
                        "name",
                        asset
                            .get("name")
                            .and_then(Value::as_str)
                            .map_or_else(|| json_string(""), json_string),
                    ),
                    (
                        "ext",
                        asset
                            .get("ext")
                            .and_then(Value::as_str)
                            .map_or_else(|| json_string(""), json_string),
                    ),
                    (
                        "mediaType",
                        asset
                            .get("type")
                            .and_then(Value::as_str)
                            .map_or_else(|| json_string(""), json_string),
                    ),
                ]),
            );
        }
    }
}

fn scan_asset_tuples(
    owner: Owner<'_>,
    base_path: &str,
    tuples: &[Value],
    collector: &mut GraphCollector<'_>,
) {
    for (index, tuple) in tuples.iter().enumerate() {
        let tuple = tuple.as_array();
        collector.emit(
            owner,
            format!("{base_path}[{index}][1]"),
            "asset",
            tuple.and_then(|tuple| tuple.get(1)),
            object(&[
                (
                    "name",
                    tuple
                        .and_then(|tuple| tuple.first())
                        .and_then(Value::as_str)
                        .map_or_else(|| json_string(""), json_string),
                ),
                (
                    "ext",
                    tuple
                        .and_then(|tuple| tuple.get(2))
                        .and_then(Value::as_str)
                        .map_or_else(|| json_string(""), json_string),
                ),
            ]),
        );
    }
}

fn scan_cold_payloads(
    payloads: &[F0PayloadDescriptor],
    collector: &mut GraphCollector<'_>,
) -> Result<(), F0Error> {
    for payload in payloads {
        if payload.kind != F0PayloadKind::Cold {
            continue;
        }
        let source = payload.cold_source.as_deref().ok_or_else(|| F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!("F0 cold payload is missing its source: {}", payload.key),
        })?;
        let value = decode_cold_payload(source, payload)?;
        let owner = Owner {
            kind: "cold",
            id: &payload.key,
        };
        if let Some(character) = value.get("character").and_then(Value::as_object) {
            scan_character_assets(character, owner, "$.value.character", collector);
        }
        scan_inlays(&value, owner, "$.value", collector);
    }
    Ok(())
}

fn decode_cold_payload(source: &Path, payload: &F0PayloadDescriptor) -> Result<Value, F0Error> {
    decode_cold_payload_with_limit(source, payload, MAX_COLD_DECODED_BYTES)
}

fn decode_cold_payload_with_limit(
    source: &Path,
    payload: &F0PayloadDescriptor,
    decoded_limit: usize,
) -> Result<Value, F0Error> {
    let mut source_file = File::open(source).map_err(|error| cold_error(payload, error))?;
    let actual_length = source_file
        .metadata()
        .map_err(|error| cold_error(payload, error))?
        .len();
    if actual_length != payload.byte_length {
        return Err(F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!(
                "F0 cold payload length mismatch for {}: expected {}, actual {}",
                payload.key, payload.byte_length, actual_length
            ),
        });
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COLD_COPY_BUFFER_BYTES];
    loop {
        let read = source_file
            .read(&mut buffer)
            .map_err(|error| cold_error(payload, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual_hash = hex::encode(hasher.finalize());
    if !actual_hash.eq_ignore_ascii_case(&payload.sha256) {
        return Err(F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!("F0 cold payload hash mismatch for {}", payload.key),
        });
    }
    source_file
        .seek(SeekFrom::Start(0))
        .map_err(|error| cold_error(payload, error))?;
    crate::cold_payload_codec::decode_cold_json(source_file, decoded_limit)
        .map_err(|error| cold_error(payload, error))
}

fn validate_payload_descriptor(payload: &F0PayloadDescriptor) -> Result<(), F0Error> {
    if payload.sha256.len() != 64 || !payload.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!("invalid F0 payload SHA-256 for {}", payload.key),
        });
    }
    if payload.kind != F0PayloadKind::Cold && payload.cold_source.is_some() {
        return Err(F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!("non-cold F0 payload has a cold source: {}", payload.key),
        });
    }
    let metadata = payload.metadata.as_object().ok_or_else(|| F0Error {
        code: F0ErrorCode::InvalidInventory,
        message: format!("F0 payload metadata must be an object: {}", payload.key),
    })?;
    if payload.kind != F0PayloadKind::Cold {
        for field in ["name", "ext", "mime"] {
            if !metadata.get(field).is_some_and(Value::is_string) {
                return Err(F0Error {
                    code: F0ErrorCode::InvalidInventory,
                    message: format!(
                        "F0 payload metadata field {field} must be a string: {}",
                        payload.key
                    ),
                });
            }
        }
    }
    let inlay_type = metadata.get("inlayType").and_then(Value::as_str);
    if payload.kind == F0PayloadKind::Inlay {
        if !matches!(inlay_type, Some("image" | "audio" | "video" | "signature")) {
            return Err(F0Error {
                code: F0ErrorCode::InvalidInventory,
                message: format!(
                    "F0 Inlay metadata requires a supported inlayType: {}",
                    payload.key
                ),
            });
        }
    } else if payload.kind == F0PayloadKind::Asset && inlay_type.is_some() {
        return Err(F0Error {
            code: F0ErrorCode::InvalidInventory,
            message: format!(
                "F0 non-Inlay metadata cannot declare inlayType: {}",
                payload.key
            ),
        });
    }
    Ok(())
}

fn cold_error(payload: &F0PayloadDescriptor, error: std::io::Error) -> F0Error {
    F0Error {
        code: F0ErrorCode::ColdPayload,
        message: format!("failed to read F0 cold payload {}: {error}", payload.key),
    }
}

fn scan_inlays(
    value: &Value,
    owner: Owner<'_>,
    base_path: &str,
    collector: &mut GraphCollector<'_>,
) {
    match value {
        Value::String(value) => {
            for (token_index, (key, offset)) in inlay_tokens(value).into_iter().enumerate() {
                let key = Value::String(key);
                collector.emit(
                    owner,
                    base_path,
                    "inlay",
                    Some(&key),
                    object(&[
                        ("tokenIndex", Value::from(token_index as u64)),
                        ("offset", Value::from(offset as u64)),
                    ]),
                );
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                scan_inlays(item, owner, &format!("{base_path}[{index}]"), collector);
            }
        }
        Value::Object(entries) => {
            for (key, item) in entries {
                scan_inlays(item, owner, &property_path(base_path, key), collector);
            }
        }
        _ => {}
    }
}

fn inlay_tokens(value: &str) -> Vec<(String, usize)> {
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("{{") {
        let start = cursor + relative_start;
        let after_open = start + 2;
        let prefix = ["inlay::", "inlayed::", "inlayeddata::"]
            .into_iter()
            .find(|prefix| value[after_open..].starts_with(prefix));
        let Some(prefix) = prefix else {
            cursor = after_open;
            continue;
        };
        let key_start = after_open + prefix.len();
        let Some(relative_end) = value[key_start..].find("}}") else {
            cursor = after_open;
            continue;
        };
        let key_end = key_start + relative_end;
        let key = &value[key_start..key_end];
        if !key.is_empty() && !key.chars().any(is_js_dot_line_terminator) {
            tokens.push((key.to_owned(), value[..start].encode_utf16().count()));
            cursor = key_end + 2;
        } else {
            cursor = after_open;
        }
    }
    tokens
}

fn reference_graph_sha256(references: &[F0Reference]) -> Result<String, F0Error> {
    let mut hasher = Sha256::new();
    hasher.update(b"L");
    hash_count(&mut hasher, references.len())?;
    for reference in references {
        let edge = reference_graph_value(reference);
        hasher.update(canonical_length(&edge)?.to_be_bytes());
        hash_canonical(&edge, &mut hasher, ObjectKeyOrder::Iteration)?;
    }
    Ok(hex::encode(hasher.finalize()))
}

fn reference_graph_value(reference: &F0Reference) -> Value {
    object(&[
        (
            "owner",
            object(&[
                ("kind", json_string(&reference.owner_kind)),
                ("id", json_string(&reference.owner_id)),
            ]),
        ),
        ("path", json_string(&reference.source_path)),
        ("occurrence", Value::from(reference.occurrence)),
        (
            "target",
            object(&[
                ("kind", json_string(&reference.target_kind)),
                ("key", json_string(&reference.target_key)),
                ("metadata", reference.metadata.clone()),
            ]),
        ),
        ("status", json_string(status_name(reference.status))),
    ])
}

fn payload_kind_name(kind: F0PayloadKind) -> &'static str {
    match kind {
        F0PayloadKind::Asset => "asset",
        F0PayloadKind::Inlay => "inlay",
        F0PayloadKind::Cold => "cold",
    }
}

fn is_target_kind(kind: &str) -> bool {
    matches!(
        kind,
        "asset"
            | "inlay"
            | "cold"
            | "character"
            | "preset"
            | "persona"
            | "module"
            | "loadout"
            | "folder"
            | "card"
            | "conversation"
    )
}

fn status_name(status: F0ReferenceStatus) -> &'static str {
    match status {
        F0ReferenceStatus::Present => "present",
        F0ReferenceStatus::ExpectedMissing => "expected-missing",
        F0ReferenceStatus::UnexpectedMissing => "unexpected-missing",
        F0ReferenceStatus::External => "external",
        F0ReferenceStatus::Invalid => "invalid",
    }
}

fn required_array<'a>(database: &'a Value, field: &str) -> Result<&'a [Value], F0Error> {
    database
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid_database(format!("F0 database field {field} must be an array")))
}

fn optional_array<'a>(database: &'a Value, field: &str) -> &'a [Value] {
    database
        .get(field)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn array_field<'a>(object: &'a serde_json::Map<String, Value>, field: &str) -> &'a [Value] {
    object
        .get(field)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
    object.get(field).and_then(Value::as_str)
}

fn index_named(
    database: &Value,
    collection: &str,
    field: &str,
    kind: &str,
    present: &mut TargetIndex,
) {
    for item in optional_array(database, collection) {
        if let Some(value) = item
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            insert_target(present, kind, value);
        }
    }
}

fn insert_target(index: &mut TargetIndex, kind: &str, key: &str) -> bool {
    index
        .entry(kind.to_owned())
        .or_default()
        .insert(key.to_owned())
}

fn contains_target(index: &TargetIndex, kind: &str, key: &str) -> bool {
    index.get(kind).is_some_and(|keys| keys.contains(key))
}

fn selected_array_value<'a>(
    database: &'a Value,
    field: &str,
    index: Option<&Value>,
) -> Option<&'a Value> {
    selected_slice_value(optional_array(database, field), index)
}

fn selected_slice_value<'a>(values: &'a [Value], index: Option<&Value>) -> Option<&'a Value> {
    let index = index?
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())?;
    values.get(index)
}

fn display_key(value: Option<&Value>) -> String {
    match value {
        None => "<undefined>".to_owned(),
        Some(Value::String(value)) => value.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn display_template(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
    }
}

fn is_external_asset_key(key: &str) -> bool {
    let lowercase = key.to_ascii_lowercase();
    [
        "http:", "https:", "data:", "blob:", "file:", "content:", "tauri:",
    ]
    .iter()
    .any(|prefix| lowercase.starts_with(prefix))
        || (key.len() >= 3
            && key.as_bytes()[0].is_ascii_alphabetic()
            && key.as_bytes()[1] == b':'
            && matches!(key.as_bytes()[2], b'/' | b'\\'))
}

fn property_path(path: &str, key: &str) -> String {
    if is_property_identifier(key) {
        format!("{path}.{key}")
    } else {
        format!(
            "{path}[{}]",
            serde_json::to_string(key).expect("JSON object keys serialize")
        )
    }
}

fn is_property_identifier(key: &str) -> bool {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || matches!(first, b'_' | b'$'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}

fn is_js_dot_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn object(entries: &[(&str, Value)]) -> Value {
    let mut object = serde_json::Map::new();
    for (key, value) in entries {
        object.insert((*key).to_owned(), value.clone());
    }
    Value::Object(object)
}

fn json_string(value: &str) -> Value {
    Value::String(value.to_owned())
}

fn invalid_database(message: impl Into<String>) -> F0Error {
    F0Error {
        code: F0ErrorCode::InvalidDatabase,
        message: message.into(),
    }
}

fn canonical_sha256(value: &Value) -> Result<String, F0Error> {
    canonical_sha256_with_order(value, ObjectKeyOrder::Sorted)
}

#[cfg(test)]
pub(crate) fn legacy_canonical_database_sha256_v1(value: &Value) -> Result<String, F0Error> {
    canonical_sha256_with_order(value, ObjectKeyOrder::Iteration)
}

fn canonical_sha256_with_order(
    value: &Value,
    object_key_order: ObjectKeyOrder,
) -> Result<String, F0Error> {
    canonical_length(value)?;
    let mut hasher = Sha256::new();
    hash_canonical(value, &mut hasher, object_key_order)?;
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Clone, Copy)]
enum ObjectKeyOrder {
    Iteration,
    Sorted,
}

fn canonical_length(value: &Value) -> Result<u32, F0Error> {
    let length = match value {
        Value::Null | Value::Bool(_) => 1_u64,
        Value::Number(_) => 1 + 4 + 8,
        Value::String(value) => 1 + 4 + value.len() as u64,
        Value::Array(items) => {
            let mut length = 1_u64 + 4;
            for item in items {
                length = length
                    .checked_add(4 + u64::from(canonical_length(item)?))
                    .ok_or_else(canonical_too_large)?;
            }
            length
        }
        Value::Object(entries) => {
            let mut length = 1_u64 + 4;
            for (key, item) in entries {
                let item_length = canonical_length(item)?;
                length = length
                    .checked_add(4 + key.len() as u64)
                    .and_then(|value| value.checked_add(4 + u64::from(item_length)))
                    .ok_or_else(canonical_too_large)?;
            }
            length
        }
    };
    u32::try_from(length).map_err(|_| canonical_too_large())
}

fn hash_canonical(
    value: &Value,
    hasher: &mut Sha256,
    object_key_order: ObjectKeyOrder,
) -> Result<(), F0Error> {
    match value {
        Value::Null => hasher.update(b"N"),
        Value::Bool(true) => hasher.update(b"T"),
        Value::Bool(false) => hasher.update(b"F"),
        Value::Number(number) => {
            let number = number.as_f64().ok_or_else(|| F0Error {
                code: F0ErrorCode::CanonicalValue,
                message: "F0 canonical database contains an unsupported number".to_owned(),
            })?;
            if !number.is_finite() {
                return Err(F0Error {
                    code: F0ErrorCode::CanonicalValue,
                    message: "F0 canonical database contains a non-finite number".to_owned(),
                });
            }
            hasher.update(b"D");
            hasher.update(8_u32.to_be_bytes());
            hasher.update(number.to_be_bytes());
        }
        Value::String(value) => {
            hasher.update(b"S");
            hash_length_delimiter(hasher, value.len())?;
            hasher.update(value.as_bytes());
        }
        Value::Array(items) => {
            hasher.update(b"L");
            hash_count(hasher, items.len())?;
            for item in items {
                hasher.update(canonical_length(item)?.to_be_bytes());
                hash_canonical(item, hasher, object_key_order)?;
            }
        }
        Value::Object(entries) => {
            hasher.update(b"O");
            hash_count(hasher, entries.len())?;
            match object_key_order {
                ObjectKeyOrder::Iteration => {
                    for (key, item) in entries {
                        hash_canonical_object_entry(key, item, hasher, object_key_order)?;
                    }
                }
                ObjectKeyOrder::Sorted => {
                    let mut keys = entries.keys().collect::<Vec<_>>();
                    keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
                    for key in keys {
                        hash_canonical_object_entry(key, &entries[key], hasher, object_key_order)?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn hash_canonical_object_entry(
    key: &str,
    item: &Value,
    hasher: &mut Sha256,
    object_key_order: ObjectKeyOrder,
) -> Result<(), F0Error> {
    hash_length_delimiter(hasher, key.len())?;
    hasher.update(key.as_bytes());
    hasher.update(canonical_length(item)?.to_be_bytes());
    hash_canonical(item, hasher, object_key_order)
}

fn hash_count(hasher: &mut Sha256, count: usize) -> Result<(), F0Error> {
    let count = u32::try_from(count).map_err(|_| canonical_too_large())?;
    hasher.update(count.to_be_bytes());
    Ok(())
}

fn hash_length_delimiter(hasher: &mut Sha256, length: usize) -> Result<(), F0Error> {
    let length = u32::try_from(length).map_err(|_| canonical_too_large())?;
    hasher.update(length.to_be_bytes());
    Ok(())
}

fn canonical_too_large() -> F0Error {
    F0Error {
        code: F0ErrorCode::CanonicalValue,
        message: "F0 canonical value exceeds the length-delimited v1 limit".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{
        write::{DeflateEncoder, GzEncoder, ZlibEncoder},
        Compression,
    };
    use serde_json::json;
    use std::{fs, io::Write};

    #[test]
    fn portable_fragments_preserve_the_complete_f0_reference_multiset() {
        let database = json!({
            "botPresetsId":0,"botPresets":[{"name":"preset","image":"assets/preset","prompt":"{{inlay::preset}}"}],
            "selectedPersona":0,"personas":[{"id":"persona","icon":"assets/persona"}],
            "modules":[{"id":"module","assets":[["label","assets/module","png"]]}],
            "pluginCustomStorage":{"synthetic":"{{inlay::plugin}}"},
            "characters":[
                {"chaId":"character","type":"character","image":"assets/portrait","chatPage":0,"additionalAssets":[["one","assets/one","png"]],"chats":[{"id":"chat","name":"{{inlay::title}}","folderId":"folder","message":[{"role":"user","data":"{{inlay::first}} {{inlay::first}}"},{"role":"char","data":"{{inlay::second}}","swipes":["{{inlay::swipe}}"]}]}]},
                {"chaId":"group","type":"group","characters":["character"],"chatPage":0,"chats":[{"id":"group-chat","message":[{"role":"char","data":"{{inlay::group}}"}]}]}
            ]
        });
        let expected = rebuild_f0_v1(&database, &[], &[]).unwrap().references;
        let mut root = database.clone();
        for key in ["characters", "botPresets", "pluginCustomStorage"] {
            root.as_object_mut().unwrap().remove(key);
        }
        let mut actual = scan_portable_fragment(PortableFragment::Root {
            value: &root,
            selected_preset: database["botPresets"].get(0),
        })
        .unwrap();
        for (index, preset) in database["botPresets"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            actual.extend(
                scan_portable_fragment(PortableFragment::Preset {
                    value: preset,
                    index: index as i64,
                })
                .unwrap(),
            );
        }
        for (key, value) in database["pluginCustomStorage"].as_object().unwrap() {
            actual.extend(scan_portable_fragment(PortableFragment::Plugin { value, key }).unwrap());
        }
        for character in database["characters"].as_array().unwrap() {
            let id = character["chaId"].as_str().unwrap();
            let chats = character["chats"].as_array().unwrap();
            let mut detail = character.clone();
            detail.as_object_mut().unwrap().remove("chats");
            actual.extend(
                scan_portable_fragment(PortableFragment::Character {
                    value: &detail,
                    selected_chat: chats.first(),
                    has_chats: !chats.is_empty(),
                })
                .unwrap(),
            );
            for chat in chats {
                let mut detail = chat.clone();
                detail.as_object_mut().unwrap().remove("message");
                actual.extend(
                    scan_portable_fragment(PortableFragment::Conversation {
                        value: &detail,
                        character_id: id,
                    })
                    .unwrap(),
                );
                for (index, message) in chat["message"].as_array().unwrap().iter().enumerate() {
                    actual.extend(
                        scan_portable_fragment(PortableFragment::Message {
                            value: message,
                            character_id: id,
                            conversation_id: chat["id"].as_str().unwrap(),
                            index: index as i64,
                        })
                        .unwrap(),
                    );
                }
            }
        }
        // Membership is resolved by SQL in portable validation; occurrence restarts per fragment.
        // The multiset retains repeated uses and every semantic field including source paths.
        let normalize = |references: Vec<F0Reference>| {
            let mut values = references
                .into_iter()
                .map(|r| {
                    format!(
                        "{:?}",
                        (
                            r.owner_kind,
                            r.owner_id,
                            r.source_path,
                            r.target_kind,
                            r.target_key,
                            r.metadata
                        )
                    )
                })
                .collect::<Vec<_>>();
            values.sort();
            values
        };
        assert_eq!(normalize(actual), normalize(expected));
    }

    #[test]
    fn decoded_database_semantics_ignore_irrelevant_source_bytes() {
        let compact: serde_json::Value = serde_json::from_str(
            r#"{"characters":[],"botPresets":[{"name":"preset"}],"botPresetsId":0,"personas":[{"id":"persona"}],"selectedPersona":0}"#,
        )
        .unwrap();
        let padded: serde_json::Value = serde_json::from_str(
            "{\n  \"characters\": [],\n  \"botPresets\": [{ \"name\": \"preset\" }],\n  \"botPresetsId\": 0,\n  \"personas\": [{ \"id\": \"persona\" }],\n  \"selectedPersona\": 0\n}\n",
        )
        .unwrap();

        let compact_result = validate_f0_v1(&compact, &[], &[]).unwrap();
        let padded_result = validate_f0_v1(&padded, &[], &[]).unwrap();

        assert_eq!(
            compact_result.canonical_database_sha256,
            padded_result.canonical_database_sha256
        );
        assert_eq!(compact_result.references, padded_result.references);
        assert_eq!(
            compact_result.reference_graph_sha256,
            padded_result.reference_graph_sha256
        );
        assert_eq!(
            canonical_sha256(&json!({ "a": 0 })).unwrap(),
            "6c613a35bce6c2c5b8e70aa0a4203ea9729d72395e6c1f5a9bb77e5eeacd3f70"
        );

        let mut changed = compact;
        changed["botPresets"][0]["name"] = json!("changed");
        let changed_result = validate_f0_v1(&changed, &[], &[]).unwrap();
        assert_ne!(
            compact_result.canonical_database_sha256,
            changed_result.canonical_database_sha256
        );
    }

    #[test]
    fn canonical_database_hash_sorts_object_keys_recursively() {
        let original_value: Value = serde_json::from_str(
            r#"{"characters":[],"botPresets":[{"name":"preset"}],"botPresetsId":0,"personas":[{"id":"persona"}],"selectedPersona":0,"tail":{"zeta":false,"alpha":true},"sequence":[0,1]}"#,
        )
        .unwrap();
        let reordered_value: Value = serde_json::from_str(
            r#"{"sequence":[0,1],"tail":{"alpha":true,"zeta":false},"selectedPersona":0,"personas":[{"id":"persona"}],"botPresetsId":0,"botPresets":[{"name":"preset"}],"characters":[]}"#,
        )
        .unwrap();

        let original = validate_f0_v1(&original_value, &[], &[]).unwrap();
        let reordered = validate_f0_v1(&reordered_value, &[], &[]).unwrap();

        assert_eq!(
            original.canonical_database_sha256,
            reordered.canonical_database_sha256
        );
        assert_ne!(
            legacy_canonical_database_sha256_v1(&original_value).unwrap(),
            legacy_canonical_database_sha256_v1(&reordered_value).unwrap()
        );

        let mut changed_array = reordered_value;
        changed_array["sequence"] = json!([1, 0]);
        assert_ne!(
            original.canonical_database_sha256,
            validate_f0_v1(&changed_array, &[], &[])
                .unwrap()
                .canonical_database_sha256
        );
    }

    #[test]
    fn payload_namespaces_resolve_the_same_key_by_kind() {
        let database = json!({
            "characters": [{
                "type": "character",
                "chaId": "character",
                "image": "shared-key",
                "chats": [{
                    "id": "chat",
                    "message": [{ "data": "{{inlay::shared-key}}" }]
                }],
                "chatPage": 0
            }],
            "botPresets": [{ "name": "preset" }],
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0
        });
        let payloads = [
            payload(F0PayloadKind::Asset, "shared-key"),
            payload(F0PayloadKind::Inlay, "shared-key"),
        ];

        let result = validate_f0_v1(&database, &payloads, &[]).unwrap();
        let shared = result
            .references
            .iter()
            .filter(|reference| reference.target_key == "shared-key")
            .collect::<Vec<_>>();

        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0].target_kind, "asset");
        assert_eq!(shared[0].status, F0ReferenceStatus::Present);
        assert_eq!(shared[1].target_kind, "inlay");
        assert_eq!(shared[1].status, F0ReferenceStatus::Present);

        let mut wrong_kind_database = database;
        wrong_kind_database["characters"][0]["image"] = json!("wrong-kind");
        wrong_kind_database["characters"][0]["chats"][0]["message"][0]["data"] =
            json!("{{inlay::wrong-kind}}");
        let wrong_kind = validate_f0_v1(
            &wrong_kind_database,
            &[payload(F0PayloadKind::Inlay, "wrong-kind")],
            &[],
        )
        .unwrap_err();
        assert_eq!(wrong_kind.code, F0ErrorCode::UnexpectedMissing);
    }

    #[test]
    fn cold_payloads_are_bounded_decoded_and_scanned_in_payload_order() {
        let directory = tempfile::tempdir().unwrap();
        let cold_path = directory.path().join("cold.zlib");
        let cold_value = json!({
            "character": {
                "image": "cold-asset",
                "additionalAssets": [
                    ["first", "cold-asset", "bin"],
                    ["duplicate", "cold-asset", "bin"]
                ],
                "roadmap14Unknown": "{{inlay::cold-inlay}}"
            }
        });
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(serde_json::to_string(&cold_value).unwrap().as_bytes())
            .unwrap();
        let cold_bytes = encoder.finish().unwrap();
        fs::write(&cold_path, &cold_bytes).unwrap();
        let database = json!({
            "characters": [{
                "type": "character",
                "chaId": "character",
                "chats": [],
                "coldstorage": "cold-key"
            }],
            "botPresets": [{ "name": "preset" }],
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0
        });
        let mut cold = payload(F0PayloadKind::Cold, "cold-key");
        cold.sha256 = hex::encode(Sha256::digest(&cold_bytes));
        cold.byte_length = cold_bytes.len() as u64;
        cold.metadata = json!({ "source": "legacy-cold", "ordinal": 4 });
        cold.cold_source = Some(cold_path.clone());
        let payloads = [
            cold,
            payload(F0PayloadKind::Asset, "cold-asset"),
            payload(F0PayloadKind::Inlay, "cold-inlay"),
        ];

        let result = validate_f0_v1(&database, &payloads, &[]).unwrap();
        let cold_references = result
            .references
            .iter()
            .filter(|reference| reference.owner_kind == "cold")
            .map(|reference| {
                (
                    reference.occurrence,
                    reference.target_kind.as_str(),
                    reference.target_key.as_str(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            cold_references,
            vec![
                (0, "asset", "cold-asset"),
                (1, "asset", "cold-asset"),
                (2, "asset", "cold-asset"),
                (3, "inlay", "cold-inlay"),
            ]
        );
        assert!(result.references.iter().any(|reference| {
            reference.owner_kind == "character"
                && reference.target_kind == "cold"
                && reference.target_key == "cold-key"
                && reference.status == F0ReferenceStatus::Present
        }));

        let mut changed_bytes = cold_bytes;
        changed_bytes[0] ^= 1;
        fs::write(&cold_path, changed_bytes).unwrap();
        let changed = rebuild_f0_v1(&database, &payloads, &[]).unwrap_err();
        assert_eq!(changed.code, F0ErrorCode::InvalidInventory);
    }

    #[test]
    fn cold_payload_decoder_matches_fflate_gzip_zlib_and_raw_deflate() {
        let directory = tempfile::tempdir().unwrap();
        let cold_value = json!({
            "character": {
                "image": "asset-key",
                "roadmap14Unknown": "{{inlay::inlay-key}}"
            }
        });
        let fflate_payloads = [
            (
                "gzip",
                "1f8b08004edf8f6a0003ab564ace482c4a4c2e492d52b2aa56cacc4d4c4f55b2524a2c2e4e2dd1cd4ead54d2512aca4f4cc94d2c303409cdcbcecb2fcf034a575767e6e524565a59812990bada5aa5da5a009cf2bb114d000000",
            ),
            (
                "zlib",
                "789cab564ace482c4a4c2e492d52b2aa56cacc4d4c4f55b2524a2c2e4e2dd1cd4ead54d2512aca4f4cc94d2c303409cdcbcecb2fcf034a575767e6e524565a59812990bada5aa5da5a001f211bb2",
            ),
            (
                "raw-deflate",
                "ab564ace482c4a4c2e492d52b2aa56cacc4d4c4f55b2524a2c2e4e2dd1cd4ead54d2512aca4f4cc94d2c303409cdcbcecb2fcf034a575767e6e524565a59812990bada5aa5da5a00",
            ),
        ];
        let mut decoded = Vec::new();

        for (codec, encoded_hex) in fflate_payloads {
            let encoded = hex::decode(encoded_hex).unwrap();
            let path = directory.path().join(format!("cold-{codec}.bin"));
            fs::write(&path, &encoded).unwrap();
            let payload = cold_payload(&path, &encoded);
            decoded.push(decode_cold_payload(&path, &payload).unwrap());
        }

        assert_eq!(decoded, vec![cold_value.clone(); 3]);

        let database = json!({
            "characters": [],
            "botPresets": [{ "name": "preset" }],
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0
        });
        let mut validations = Vec::new();
        for (codec, encoded_hex) in fflate_payloads {
            let encoded = hex::decode(encoded_hex).unwrap();
            let path = directory.path().join(format!("cold-{codec}.bin"));
            validations.push(
                rebuild_f0_v1(
                    &database,
                    &[
                        cold_payload(&path, &encoded),
                        payload(F0PayloadKind::Asset, "asset-key"),
                        payload(F0PayloadKind::Inlay, "inlay-key"),
                    ],
                    &[],
                )
                .unwrap(),
            );
        }
        assert!(validations.windows(2).all(|pair| {
            pair[0].canonical_database_sha256 == pair[1].canonical_database_sha256
                && pair[0].reference_graph_sha256 == pair[1].reference_graph_sha256
                && pair[0].references == pair[1].references
        }));
    }

    #[test]
    fn cold_payload_decoder_rejects_trailing_corrupt_and_unfinished_streams() {
        let directory = tempfile::tempdir().unwrap();
        let source = serde_json::to_vec(&json!({ "message": [] })).unwrap();
        let codecs = [
            ("gzip", encode_gzip(&source)),
            ("zlib", encode_zlib(&source)),
            ("raw-deflate", encode_raw_deflate(&source)),
        ];

        for (codec, encoded) in codecs {
            let mut trailing = encoded.clone();
            trailing.push(0);
            let trailing_path = directory.path().join(format!("{codec}-trailing.bin"));
            fs::write(&trailing_path, &trailing).unwrap();
            let trailing_error =
                decode_cold_payload(&trailing_path, &cold_payload(&trailing_path, &trailing))
                    .unwrap_err();
            assert_eq!(trailing_error.code, F0ErrorCode::ColdPayload);
            assert!(trailing_error.message.contains("trailing compressed bytes"));

            let mut corrupt = encoded;
            if codec == "raw-deflate" {
                corrupt[0] = (corrupt[0] & !0b111) | 0b111;
            } else {
                let last = corrupt.last_mut().unwrap();
                *last ^= 1;
            }
            let corrupt_path = directory.path().join(format!("{codec}-corrupt.bin"));
            fs::write(&corrupt_path, &corrupt).unwrap();
            let corrupt_error =
                decode_cold_payload(&corrupt_path, &cold_payload(&corrupt_path, &corrupt))
                    .expect_err(&format!("{codec} corrupt payload was accepted"));
            assert_eq!(corrupt_error.code, F0ErrorCode::ColdPayload);
        }

        let mut truncated_gzip = encode_gzip(&source);
        truncated_gzip.truncate(truncated_gzip.len() - 1);
        let mut truncated_zlib = encode_zlib(&source);
        truncated_zlib.truncate(truncated_zlib.len() - 4);
        let unfinished_raw = encode_unfinished_raw_deflate(&source);
        for (codec, encoded) in [
            ("gzip", truncated_gzip),
            ("zlib", truncated_zlib),
            ("raw-deflate", unfinished_raw),
        ] {
            let path = directory.path().join(format!("{codec}-unfinished.bin"));
            fs::write(&path, &encoded).unwrap();
            let error = decode_cold_payload(&path, &cold_payload(&path, &encoded))
                .expect_err(&format!("{codec} unfinished payload was accepted"));
            assert_eq!(error.code, F0ErrorCode::ColdPayload);
        }
    }

    #[test]
    fn cold_payload_decoder_enforces_the_decoded_limit_for_every_codec() {
        let directory = tempfile::tempdir().unwrap();
        let source = serde_json::to_vec(&json!({ "message": ["0123456789"] })).unwrap();
        let codecs = [
            ("gzip", encode_gzip(&source)),
            ("zlib", encode_zlib(&source)),
            ("raw-deflate", encode_raw_deflate(&source)),
        ];

        for (codec, encoded) in codecs {
            let path = directory.path().join(format!("{codec}-bomb.bin"));
            fs::write(&path, &encoded).unwrap();
            assert_eq!(
                decode_cold_payload_with_limit(
                    &path,
                    &cold_payload(&path, &encoded),
                    source.len(),
                )
                .unwrap(),
                json!({ "message": ["0123456789"] })
            );
            let error = decode_cold_payload_with_limit(
                &path,
                &cold_payload(&path, &encoded),
                source.len() - 1,
            )
            .unwrap_err();
            assert_eq!(error.code, F0ErrorCode::ColdPayload);
            assert!(error.message.contains("exceeds the decoded limit"));
        }
    }

    fn cold_payload(path: &Path, encoded: &[u8]) -> F0PayloadDescriptor {
        let mut payload = payload(F0PayloadKind::Cold, "cold-key");
        payload.sha256 = hex::encode(Sha256::digest(encoded));
        payload.byte_length = encoded.len() as u64;
        payload.metadata = json!({ "source": "legacy-cold", "ordinal": 0 });
        payload.cold_source = Some(path.to_owned());
        payload
    }

    fn encode_gzip(source: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(source).unwrap();
        encoder.finish().unwrap()
    }

    fn encode_zlib(source: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(source).unwrap();
        encoder.finish().unwrap()
    }

    fn encode_raw_deflate(source: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(source).unwrap();
        encoder.finish().unwrap()
    }

    fn encode_unfinished_raw_deflate(source: &[u8]) -> Vec<u8> {
        let length = u16::try_from(source.len()).unwrap();
        let mut encoded = vec![0, length as u8, (length >> 8) as u8];
        let complement = !length;
        encoded.extend_from_slice(&[complement as u8, (complement >> 8) as u8]);
        encoded.extend_from_slice(source);
        encoded
    }

    #[test]
    fn database_reference_graph_preserves_exact_owner_and_occurrence_order() {
        let database = json!({
            "characters": [{
                "type": "character",
                "chaId": "character-main",
                "image": "character-image",
                "firstMessage": "character {{inlay::character-inlay}}",
                "chats": [{
                    "id": "chat-main",
                    "modules": ["module-main"],
                    "bindedPersona": "persona-main",
                    "folderId": "chat-folder",
                    "message": [
                        { "data": "\u{ef01}COLDSTORAGE\u{ef01}cold-chat" },
                        { "data": "chat {{inlay::chat-inlay}}" }
                    ]
                }],
                "chatFolders": [{ "id": "chat-folder" }],
                "chatPage": 0,
                "modules": ["module-main"],
                "coldstorage": "cold-character",
                "coldStoragedChats": ["cold-chat", "cold-chat"]
            }],
            "botPresets": [{
                "name": "preset-main",
                "image": "preset-image",
                "note": "preset {{inlay::preset-inlay}}"
            }],
            "botPresetsId": 0,
            "modules": [{
                "id": "module-main",
                "assets": [["module", "module-asset", "bin"]],
                "icon": "module-icon",
                "note": "module {{inlay::module-inlay}}"
            }],
            "enabledModules": ["module-main"],
            "personas": [{
                "id": "persona-main",
                "icon": "persona-icon",
                "embeddedModule": {
                    "id": "module-embedded",
                    "assets": [["embedded", "embedded-asset", "bin"]],
                    "icon": "embedded-icon",
                    "note": "embedded {{inlay::embedded-inlay}}"
                },
                "note": "persona {{inlay::persona-inlay}}"
            }],
            "selectedPersona": 0,
            "userIcon": "root-icon",
            "customBackground": "data:image/png;base64,AA==",
            "characterOrder": [{
                "id": "folder-main",
                "data": ["character-main"],
                "img": "data:image/png;base64,AA==",
                "imgFile": "folder-file",
                "note": "folder {{inlay::folder-inlay}}"
            }],
            "loadouts": [{
                "name": "loadout-main",
                "characterIds": ["character-main"],
                "modules": ["module-main"],
                "presetName": "preset-main",
                "personaId": "persona-main",
                "icons": ["loadout-icon"],
                "note": "loadout {{inlay::loadout-inlay}}"
            }],
            "lastLoadedLoadoutName": "loadout-main",
            "pluginCustomStorage": {
                "storage-main": { "note": "plugin {{inlay::plugin-inlay}}" }
            },
            "customField": "root {{inlay::root-inlay}}"
        });
        let payloads = [
            "root-icon",
            "folder-file",
            "module-asset",
            "module-icon",
            "persona-icon",
            "embedded-asset",
            "embedded-icon",
            "preset-image",
            "loadout-icon",
            "character-image",
        ]
        .into_iter()
        .map(|key| payload(F0PayloadKind::Asset, key))
        .chain(
            [
                "folder-inlay",
                "root-inlay",
                "module-inlay",
                "embedded-inlay",
                "persona-inlay",
                "preset-inlay",
                "plugin-inlay",
                "loadout-inlay",
                "character-inlay",
                "chat-inlay",
            ]
            .into_iter()
            .map(|key| payload(F0PayloadKind::Inlay, key)),
        )
        .collect::<Vec<_>>();
        let expected_missing = [
            F0ExpectedMissing {
                target_kind: "cold".to_owned(),
                target_key: "cold-character".to_owned(),
                character_id: None,
            },
            F0ExpectedMissing {
                target_kind: "cold".to_owned(),
                target_key: "cold-chat".to_owned(),
                character_id: None,
            },
        ];

        let result = validate_f0_v1(&database, &payloads, &expected_missing).unwrap();
        let actual = result
            .references
            .iter()
            .map(|reference| {
                (
                    reference.owner_kind.as_str(),
                    reference.occurrence,
                    reference.target_kind.as_str(),
                    reference.target_key.as_str(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            actual,
            vec![
                ("root", 0, "asset", "root-icon"),
                ("root", 1, "asset", "data:image/png;base64,AA=="),
                ("root", 2, "module", "module-main"),
                ("root", 3, "preset", "preset-main"),
                ("root", 4, "persona", "persona-main"),
                ("root", 5, "loadout", "loadout-main"),
                ("folder", 0, "asset", "data:image/png;base64,AA=="),
                ("folder", 1, "asset", "folder-file"),
                ("folder", 2, "character", "character-main"),
                ("folder", 3, "inlay", "folder-inlay"),
                ("root", 6, "inlay", "root-inlay"),
                ("module", 0, "asset", "module-asset"),
                ("module", 1, "asset", "module-icon"),
                ("module", 2, "inlay", "module-inlay"),
                ("persona", 0, "asset", "persona-icon"),
                ("persona", 1, "module", "module-embedded"),
                ("persona", 2, "asset", "embedded-asset"),
                ("persona", 3, "asset", "embedded-icon"),
                ("persona", 4, "inlay", "embedded-inlay"),
                ("persona", 5, "inlay", "persona-inlay"),
                ("preset", 0, "asset", "preset-image"),
                ("preset", 1, "inlay", "preset-inlay"),
                ("plugin-storage", 0, "inlay", "plugin-inlay"),
                ("loadout", 0, "character", "character-main"),
                ("loadout", 1, "module", "module-main"),
                ("loadout", 2, "preset", "preset-main"),
                ("loadout", 3, "persona", "persona-main"),
                ("loadout", 4, "asset", "loadout-icon"),
                ("loadout", 5, "inlay", "loadout-inlay"),
                ("character", 0, "asset", "character-image"),
                ("character", 1, "module", "module-main"),
                ("character", 2, "conversation", "chat-main"),
                ("character", 3, "cold", "cold-character"),
                ("character", 4, "cold", "cold-chat"),
                ("character", 5, "cold", "cold-chat"),
                ("character", 6, "inlay", "character-inlay"),
                ("conversation", 0, "module", "module-main"),
                ("conversation", 1, "persona", "persona-main"),
                ("conversation", 2, "folder", "chat-folder"),
                ("conversation", 3, "cold", "cold-chat"),
                ("conversation", 4, "inlay", "chat-inlay"),
            ]
        );
        assert_eq!(
            result.reference_graph_sha256,
            "661a3aa9eed8faf0355e53097c7292f548a11a4085140e20cefc37a55491d227"
        );
    }

    #[test]
    fn unexpected_missing_is_strict_but_rebuild_retains_the_ordered_diagnostic_graph() {
        let database = json!({
            "characters": [{
                "type": "character",
                "chaId": "character",
                "image": "missing",
                "chats": []
            }],
            "botPresets": [{ "name": "preset" }],
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0
        });
        let rebuilt = rebuild_f0_v1(&database, &[], &[]).unwrap();
        let missing = rebuilt
            .references
            .iter()
            .find(|reference| reference.target_key == "missing")
            .unwrap();
        assert_eq!(missing.status, F0ReferenceStatus::UnexpectedMissing);
        assert_eq!(missing.occurrence, 0);

        let strict = validate_f0_v1(&database, &[], &[]).unwrap_err();
        assert_eq!(strict.code, F0ErrorCode::UnexpectedMissing);

        let policy = [F0ExpectedMissing {
            target_kind: "asset".to_owned(),
            target_key: "missing".to_owned(),
            character_id: None,
        }];
        let accepted = validate_f0_v1(&database, &[], &policy).unwrap();
        assert_eq!(
            accepted
                .references
                .iter()
                .find(|reference| reference.target_key == "missing")
                .unwrap()
                .status,
            F0ReferenceStatus::ExpectedMissing
        );

        let conflict = validate_f0_v1(
            &database,
            &[payload(F0PayloadKind::Asset, "missing")],
            &policy,
        )
        .unwrap_err();
        assert_eq!(conflict.code, F0ErrorCode::InvalidInventory);
    }

    #[test]
    fn expected_missing_folders_use_the_referencing_character_scope() {
        let mut database = json!({
            "botPresets": [{"name":"preset"}], "botPresetsId":0,
            "personas":[{"id":"persona"}], "selectedPersona":0,
            "characters":[
                {"chaId":"a", "chatPage":0, "chats":[{"id":"chat", "folderId":"folder", "message":[]}]},
                {"chaId":"b", "chats":[], "chatFolders":[{"id":"folder"}]}
            ]
        });
        let missing = [F0ExpectedMissing {
            target_kind: "folder".into(),
            target_key: "folder".into(),
            character_id: Some("a".into()),
        }];
        let graph = validate_f0_v1(&database, &[], &missing).unwrap();
        assert!(graph.references.iter().any(|reference| {
            reference.target_kind == "folder"
                && reference.status == F0ReferenceStatus::ExpectedMissing
        }));
        let wrong_scope = [F0ExpectedMissing {
            character_id: Some("b".into()),
            ..missing[0].clone()
        }];
        assert_eq!(
            validate_f0_v1(&database, &[], &wrong_scope)
                .unwrap_err()
                .code,
            F0ErrorCode::InvalidInventory
        );
        database["characters"][0]["chatFolders"] = json!([{"id":"folder"}]);
        assert_eq!(
            validate_f0_v1(&database, &[], &missing).unwrap_err().code,
            F0ErrorCode::InvalidInventory
        );
        assert!(validate_f0_v1(&database, &[], &[]).is_ok());
    }

    #[test]
    fn payload_inventory_requires_kind_specific_metadata() {
        let database = json!({
            "characters": [],
            "botPresets": [{ "name": "preset" }],
            "botPresetsId": 0,
            "personas": [{ "id": "persona" }],
            "selectedPersona": 0
        });
        let mut invalid = payload(F0PayloadKind::Asset, "asset");
        invalid.metadata = json!({ "name": "asset", "ext": "bin" });

        let error = rebuild_f0_v1(&database, &[invalid], &[]).unwrap_err();

        assert_eq!(error.code, F0ErrorCode::InvalidInventory);
        assert!(error.message.contains("mime"));
    }

    fn payload(kind: F0PayloadKind, key: &str) -> F0PayloadDescriptor {
        F0PayloadDescriptor {
            kind,
            key: key.to_owned(),
            sha256: "00".repeat(32),
            byte_length: 0,
            metadata: json!({
                "name": key,
                "ext": "bin",
                "mime": "application/octet-stream",
                "inlayType": (kind == F0PayloadKind::Inlay).then_some("signature")
            }),
            cold_source: None,
        }
    }
}
