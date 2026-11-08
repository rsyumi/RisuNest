//! What a repair may change, and how a diagnosis turns into the choices a reader is offered.
//! Nothing here touches the store: the planner reads findings and reports candidates, and the
//! store applies the chosen ones to a staged copy that the existing gate re-checks.

use super::{codes, Finding, Locator, Owner, ScanResult};

/// One change the reader can choose. `id` is stable for a diagnosis, so a selection made in the
/// preview still names the same change when it is applied.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairCandidate {
    pub(crate) id: String,
    pub(crate) action: RepairAction,
    /// Position of the finding this answers in the diagnosis it came from.
    pub(crate) finding: usize,
    /// Whether the screen offers it as the choice already made.
    pub(crate) preferred: bool,
    /// The change drops a value nothing restores. The preview says so separately.
    pub(crate) discards: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub(crate) enum RepairAction {
    SelectFirstConversation { character_id: String },
    /// Selects the first preset in place of one the root no longer holds.
    SelectFirstPreset,
    /// Selects the first persona in place of one the root no longer holds.
    SelectFirstPersona,
    /// Gives a persona's embedded module the id activation would give it, so the root can name it.
    AssignEmbeddedModuleId { persona_id: String },
    /// Removes one reference from the record that holds it: the field or entry at `source_path`,
    /// or only the inlay token named by `token`. The stored bytes stay where they are.
    DropReference {
        owner: Owner,
        source_path: String,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<u64>,
    },
    /// Removes an alias whose stored payload is absent or no longer matches it.
    DropAlias { kind: String, key: String },
    /// Rebinds an alias to the digest its stored bytes actually have. The bytes may not be what
    /// the owner meant, which is why removal stays the offered choice.
    AdoptStoredPayload { kind: String, key: String },
    /// Recomputes the derived columns, counts and orders of one table from its own records.
    /// Every value it writes is derivable, so nothing is lost.
    NormalizeRecords { table: String },
    /// Keeps one record of a table that must hold exactly one, and drops the rest.
    KeepSingleRecord { table: String },
    /// Gives orphans an owner again by restoring the missing container as a trashed record.
    RecoverOrphans { table: String },
}

impl RepairAction {
    /// The table a staged edit touches, for the preview's account of what changes.
    pub(crate) fn subject(&self) -> &str {
        match self {
            Self::SelectFirstConversation { .. } => "character",
            Self::SelectFirstPreset | Self::SelectFirstPersona => "root",
            Self::AssignEmbeddedModuleId { .. } => "persona",
            Self::DropReference { owner, .. } => &owner.kind,
            Self::DropAlias { kind, .. } | Self::AdoptStoredPayload { kind, .. } => kind,
            Self::NormalizeRecords { table }
            | Self::KeepSingleRecord { table }
            | Self::RecoverOrphans { table } => table,
        }
    }
}

fn candidate(
    finding: usize,
    action: RepairAction,
    preferred: bool,
    discards: bool,
) -> RepairCandidate {
    RepairCandidate {
        id: format!("{finding}:{}", action_id(&action)),
        action,
        finding,
        preferred,
        discards,
    }
}

fn action_id(action: &RepairAction) -> String {
    match action {
        RepairAction::SelectFirstConversation { .. } => "select-first-conversation".to_owned(),
        RepairAction::SelectFirstPreset => "select-first-preset".to_owned(),
        RepairAction::SelectFirstPersona => "select-first-persona".to_owned(),
        RepairAction::AssignEmbeddedModuleId { .. } => "assign-embedded-module-id".to_owned(),
        RepairAction::DropReference { .. } => "drop-reference".to_owned(),
        RepairAction::DropAlias { .. } => "drop-alias".to_owned(),
        RepairAction::AdoptStoredPayload { .. } => "adopt-stored-payload".to_owned(),
        RepairAction::NormalizeRecords { .. } => "normalize-records".to_owned(),
        RepairAction::KeepSingleRecord { .. } => "keep-single-record".to_owned(),
        RepairAction::RecoverOrphans { .. } => "recover-orphans".to_owned(),
    }
}

/// Tables whose derived values the diagnosis reports through a table-level finding. A record
/// rule names its own table through the finding's owner kind.
const NORMALIZABLE: &[&str] = &[
    "characters",
    "conversations",
    "messages",
    "bot_presets",
    "plugin_storage",
];
const SINGLETON: &[&str] = &["root"];
const ORPHANABLE: &[&str] = &["conversations", "messages"];

fn record_candidates(index: usize, finding: &Finding) -> Vec<RepairCandidate> {
    let table = finding.owner.kind.as_str();
    if table == "root"
        && finding
            .detail
            .starts_with("portable root contains separated field: ")
    {
        return vec![candidate(
            index,
            RepairAction::NormalizeRecords {
                table: table.to_owned(),
            },
            true,
            false,
        )];
    }
    if NORMALIZABLE.contains(&table) {
        return vec![candidate(
            index,
            RepairAction::NormalizeRecords {
                table: table.to_owned(),
            },
            true,
            false,
        )];
    }
    if SINGLETON.contains(&table) {
        return vec![candidate(
            index,
            RepairAction::KeepSingleRecord {
                table: table.to_owned(),
            },
            false,
            true,
        )];
    }
    // A damaged storage class, invalid UTF-8 or unreadable JSON leaves nothing to derive from.
    Vec::new()
}

/// Owners whose records a repair edits. A persona, module or loadout lives inside the root under
/// its own identity and is edited only when one entry there carries it. A folder's entries are
/// rebuilt on save, so its references are left to the reader.
const EDITABLE_OWNERS: &[&str] = &["root", "preset", "plugin-storage", "character", "conversation"];

/// Fields that removing cannot answer. The reference graph reports a loadout's preset and
/// persona even when they are absent.
const KEPT_FIELDS: &[(&str, &str)] = &[
    ("loadout", "$.presetName"),
    ("loadout", "$.personaId"),
];

/// The root collection that keeps owners of this kind, and the field that names its entries.
pub(crate) fn root_collection(owner_kind: &str) -> Option<(&'static str, &'static str)> {
    match owner_kind {
        "persona" => Some(("personas", "id")),
        "module" => Some(("modules", "id")),
        "loadout" => Some(("loadouts", "name")),
        _ => None,
    }
}

/// The collection and position of the one root entry that carries an owner's identity. None when
/// no entry or several carry it, or when the reference graph named the owner by its position, as
/// it names a persona without an id, since another entry can take that position.
pub(crate) fn root_entry(root: &serde_json::Value, owner: &Owner) -> Option<(&'static str, usize)> {
    let (collection, field) = root_collection(&owner.kind)?;
    if owner.id.is_empty() {
        return None;
    }
    let mut found = None;
    for (index, entry) in root.get(collection)?.as_array()?.iter().enumerate() {
        let named = entry.get(field).and_then(serde_json::Value::as_str);
        let carries = match named {
            Some(identity) => identity == owner.id,
            None => owner.kind == "persona" && owner.id == format!("#{index}"),
        };
        if carries {
            if found.is_some() {
                return None;
            }
            found = Some((index, named.is_some()));
        }
    }
    match found {
        Some((index, true)) => Some((collection, index)),
        _ => None,
    }
}

fn editable(owner_kind: &str, locator: &Locator) -> bool {
    !KEPT_FIELDS.contains(&(owner_kind, locator.source_path.as_str()))
        && (EDITABLE_OWNERS.contains(&owner_kind)
            || (locator.sole_entry && root_collection(owner_kind).is_some()))
}

/// A conversation's `$.message[N]…` as the path within message row N, the record a repair edits.
fn message_location(owner: &Owner, path: &str) -> Option<(Owner, String)> {
    if owner.kind != "conversation" {
        return None;
    }
    let (index, rest) = path.strip_prefix("$.message[")?.split_once(']')?;
    index.parse::<u64>().ok()?;
    Some((
        Owner {
            kind: "message".to_owned(),
            id: format!("{}/{index}", owner.id),
        },
        format!("${rest}"),
    ))
}

/// What removing one reference takes out of which record. An inlay is cut from its text, and an
/// asset held by a tuple or an entry takes that whole tuple or entry, so no half of it is left.
/// None when a repair cannot remove exactly that reference.
fn reference_removal(finding: &Finding, locator: &Locator) -> Option<(Owner, String, Option<u64>)> {
    if !editable(&finding.owner.kind, locator) {
        return None;
    }
    let (owner, path) = message_location(&finding.owner, &locator.source_path)
        .unwrap_or_else(|| (finding.owner.clone(), locator.source_path.clone()));
    match finding.target.as_ref().map(|target| target.kind.as_str()) {
        // A diagnosis written before tokens were numbered cannot say which part of the text to cut.
        Some("inlay") => Some((owner, path, Some(locator.token?))),
        Some("asset") => {
            let entry = path
                .strip_suffix("[1]")
                .or_else(|| path.strip_suffix(".uri"))
                .filter(|entry| entry.ends_with(']'))
                .map(str::to_owned)
                .unwrap_or(path);
            Some((owner, entry, None))
        }
        _ => Some((owner, path, None)),
    }
}

/// Reads a diagnosis and reports what each finding can be answered with. A finding no action
/// fits keeps an empty list, which the screen shows as needing manual action.
pub(crate) fn plan(result: &ScanResult) -> Vec<RepairCandidate> {
    let mut candidates = Vec::new();
    for (index, finding) in result.items.iter().enumerate() {
        match finding.code.as_str() {
            codes::REFERENCE_MISSING | codes::REFERENCE_INVALID => {
                let Some(locator) = &finding.locator else {
                    continue;
                };
                if finding.owner.kind == "character" && locator.source_path == "$.chatPage" {
                    candidates.push(candidate(index, RepairAction::SelectFirstConversation {
                        character_id: finding.owner.id.clone(),
                    }, true, false));
                    continue;
                }
                // The reference graph reports these even when they are absent, so removing them
                // answers nothing and only a value does.
                let created = match (finding.owner.kind.as_str(), locator.source_path.as_str()) {
                    ("root", "$.botPresetsId") => Some(Some(RepairAction::SelectFirstPreset)),
                    ("root", "$.selectedPersona") => Some(Some(RepairAction::SelectFirstPersona)),
                    ("persona", "$.embeddedModule.id") => Some(locator.sole_entry.then(|| {
                        RepairAction::AssignEmbeddedModuleId { persona_id: finding.owner.id.clone() }
                    })),
                    _ => None,
                };
                if let Some(action) = created {
                    candidates.extend(action.map(|action| candidate(index, action, true, false)));
                    continue;
                }
                let Some((owner, source_path, token)) = reference_removal(finding, locator) else {
                    continue;
                };
                candidates.push(candidate(
                    index,
                    RepairAction::DropReference {
                        owner,
                        source_path,
                        occurrence: locator.occurrence,
                        token,
                    },
                    true,
                    true,
                ));
            }
            codes::ALIAS_OBJECT_ABSENT => candidates.push(candidate(
                index,
                RepairAction::DropAlias {
                    kind: finding.owner.kind.clone(),
                    key: finding.owner.id.clone(),
                },
                true,
                true,
            )),
            codes::ALIAS_OBJECT_MISMATCH => {
                candidates.push(candidate(
                    index,
                    RepairAction::DropAlias {
                        kind: finding.owner.kind.clone(),
                        key: finding.owner.id.clone(),
                    },
                    true,
                    true,
                ));
                candidates.push(candidate(
                    index,
                    RepairAction::AdoptStoredPayload {
                        kind: finding.owner.kind.clone(),
                        key: finding.owner.id.clone(),
                    },
                    false,
                    false,
                ));
            }
            codes::RECORD_INVALID => candidates.extend(record_candidates(index, finding)),
            codes::RECORD_ORPHAN => {
                let table = finding.owner.kind.as_str();
                if ORPHANABLE.contains(&table) {
                    candidates.push(candidate(
                        index,
                        RepairAction::RecoverOrphans {
                            table: table.to_owned(),
                        },
                        true,
                        false,
                    ));
                }
            }
            // Unreferenced files are deleted by the unused image cleanup, never by a repair, and
            // an unclassified failure has no fixed transformation to offer.
            _ => {}
        }
    }
    candidates
}

/// The account the reader sees before anything is applied.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RepairPreview {
    /// Selected changes, in the order they will be applied.
    pub(crate) selected: Vec<RepairCandidate>,
    /// Findings the selection answers.
    pub(crate) answered: usize,
    /// Findings the selection leaves alone.
    pub(crate) remaining: usize,
    /// References the selection removes.
    pub(crate) dropped_references: usize,
    /// Aliases the selection removes, so their stored files stop being referenced.
    pub(crate) dropped_aliases: usize,
    /// Changes that discard a value nothing restores.
    pub(crate) discarding: Vec<String>,
    /// Tables the selection rewrites.
    pub(crate) tables: Vec<String>,
    /// Whether the screen proposes keeping a snapshot before this is applied.
    pub(crate) proposes_snapshot: bool,
}

/// Builds the preview for a selection, ignoring ids the diagnosis does not offer. Only a
/// candidate the planner produced can be applied, so an unknown id never reaches the store.
pub(crate) fn preview(result: &ScanResult, selection: &[String]) -> RepairPreview {
    let offered = plan(result);
    let mut selected: Vec<RepairCandidate> = offered
        .into_iter()
        .filter(|candidate| selection.iter().any(|id| id == &candidate.id))
        .collect();
    selected.sort_by(|left, right| left.id.cmp(&right.id));
    let mut preview = RepairPreview {
        answered: {
            let mut answered: Vec<usize> =
                selected.iter().map(|candidate| candidate.finding).collect();
            answered.sort_unstable();
            answered.dedup();
            answered.len()
        },
        ..RepairPreview::default()
    };
    preview.remaining = result.items.len().saturating_sub(preview.answered);
    for candidate in &selected {
        match &candidate.action {
            RepairAction::DropReference { .. } => preview.dropped_references += 1,
            RepairAction::DropAlias { .. } => preview.dropped_aliases += 1,
            _ => {}
        }
        if candidate.discards {
            preview.discarding.push(candidate.id.clone());
        }
        let table = candidate.action.subject().to_owned();
        if !preview.tables.contains(&table) {
            preview.tables.push(table);
        }
    }
    preview.tables.sort();
    preview.selected = selected;
    preview.proposes_snapshot = proposes_snapshot(result, &preview);
    preview
}

/// Whether a selection is large or blocking enough that the screen proposes a snapshot before
/// it is applied.
fn proposes_snapshot(result: &ScanResult, preview: &RepairPreview) -> bool {
    /// A change past this size is worth a snapshot on its own, whatever it answers.
    const LARGE_CHANGE: usize = 20;
    preview.selected.len() >= LARGE_CHANGE
        || preview.selected.iter().any(|candidate| {
            result
                .items
                .get(candidate.finding)
                .is_some_and(|finding| finding.severity == super::Severity::Blocking)
        })
}

#[cfg(test)]
mod tests;
