//! What an archive holds, and which of it an import brings in. A partial import keeps the whole
//! settings record and chooses among the records that stand on their own, closing over what the
//! chosen ones refer to so a selection does not quietly break itself.

use super::*;
use crate::lossless_f0::{scan_portable_fragment, F0ReferenceStatus, PortableFragment};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One record an import can take or leave.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveEntry {
    pub(crate) id: String,
    /// Conversations under a character; zero for anything else.
    pub(crate) conversations: u64,
    /// Findings the diagnosis raised against this record.
    pub(crate) damaged: u64,
}

/// The records an archive holds, grouped the way the import screen offers them. The settings
/// record and the storage authorities are not here: an import always brings those.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveInventory {
    pub(crate) characters: Vec<ArchiveEntry>,
    pub(crate) presets: Vec<ArchiveEntry>,
    pub(crate) plugins: Vec<ArchiveEntry>,
}

/// What the reader chose. An empty selection of a kind means none of that kind, so a partial
/// import must name what it wants rather than relying on a default.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ArchiveSelection {
    pub(crate) characters: Vec<String>,
    pub(crate) presets: Vec<String>,
    pub(crate) plugins: Vec<String>,
    /// Records the reader left out on purpose even though a chosen record refers to them. Their
    /// references come in broken, which the preview says before anything is staged.
    pub(crate) excluded: Vec<String>,
}

/// A selection after closure, with what closing it added and what it still leaves broken.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClosedSelection {
    pub(crate) characters: Vec<String>,
    pub(crate) presets: Vec<String>,
    pub(crate) plugins: Vec<String>,
    /// Records closure pulled in because something chosen refers to them.
    pub(crate) added: Vec<String>,
    /// References that come in with nothing to point at, because the reader excluded the target.
    pub(crate) dangling: Vec<String>,
}

fn counted(db: &Connection, sql: &str) -> Result<Vec<ArchiveEntry>> {
    let mut statement = db.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(ArchiveEntry {
            id: row.get(0)?,
            conversations: sql_u64(row.get(1)?)?,
            damaged: 0,
        });
    }
    Ok(entries)
}

/// Reads what an archive holds. `damaged` counts the findings a diagnosis raised against each
/// record, so the screen can mark the ones that are already broken.
pub(crate) fn inventory(
    db: &Connection,
    findings: &[crate::data_health::Finding],
) -> Result<ArchiveInventory> {
    let mut inventory = ArchiveInventory {
        characters: counted(
            db,
            "SELECT character_id,conversation_count FROM characters ORDER BY configured_index",
        )?,
        presets: counted(db, "SELECT preset_id,0 FROM bot_presets ORDER BY configured_index")?,
        plugins: counted(db, "SELECT storage_key,0 FROM plugin_storage ORDER BY ordinal")?,
    };
    let mut damage: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for finding in findings {
        // A conversation or message names its character first, which is the record the reader
        // chooses, so the damage is counted there.
        let owner = finding.owner.id.split('/').next().unwrap_or_default();
        let kind = match finding.owner.kind.as_str() {
            "conversation" | "message" => "character",
            kind => kind,
        };
        *damage.entry((kind, owner)).or_default() += 1;
    }
    for (kind, entries) in [
        ("character", &mut inventory.characters),
        ("preset", &mut inventory.presets),
        ("plugin", &mut inventory.plugins),
    ] {
        for entry in entries.iter_mut() {
            entry.damaged = damage
                .get(&(kind, entry.id.as_str()))
                .copied()
                .unwrap_or_default();
        }
    }
    Ok(inventory)
}

fn character_detail(db: &Connection, id: &str) -> Result<Option<Value>> {
    let serialized: Option<String> = db
        .query_row(
            "SELECT detail FROM characters WHERE character_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(serialized.and_then(|serialized| serde_json::from_str::<Value>(&serialized).ok()))
}

/// Adds the records a chosen record contains, unless the reader excluded them. A group names its
/// members, and a member the reader left out is reported rather than quietly followed.
pub(crate) fn close(db: &Connection, selection: &ArchiveSelection) -> Result<ClosedSelection> {
    let excluded: BTreeSet<&str> = selection.excluded.iter().map(String::as_str).collect();
    let mut characters: BTreeSet<String> = selection
        .characters
        .iter()
        .filter(|id| !excluded.contains(id.as_str()))
        .cloned()
        .collect();
    let mut added = BTreeSet::new();
    let mut dangling = BTreeSet::new();
    let mut pending: Vec<String> = characters.iter().cloned().collect();
    while let Some(id) = pending.pop() {
        let Some(value) = character_detail(db, &id)? else {
            continue;
        };
        let Ok(references) = scan_portable_fragment(PortableFragment::Character {
            value: &value,
            selected_chat: None,
            has_chats: false,
        }) else {
            continue;
        };
        for reference in references {
            if matches!(reference.status, F0ReferenceStatus::Invalid)
                || reference.target_kind != "character"
            {
                continue;
            }
            let target = reference.target_key;
            if excluded.contains(target.as_str()) {
                dangling.insert(format!("character:{target}"));
                continue;
            }
            if characters.insert(target.clone()) {
                added.insert(target.clone());
                pending.push(target);
            }
        }
    }
    Ok(ClosedSelection {
        characters: characters.into_iter().collect(),
        presets: selection
            .presets
            .iter()
            .filter(|id| !excluded.contains(id.as_str()))
            .cloned()
            .collect(),
        plugins: selection
            .plugins
            .iter()
            .filter(|key| !excluded.contains(key.as_str()))
            .cloned()
            .collect(),
        added: added.into_iter().collect(),
        dangling: dangling.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests;
