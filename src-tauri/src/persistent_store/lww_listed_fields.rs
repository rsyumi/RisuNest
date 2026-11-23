use super::classification::{CHARACTER_FIELDS, CONVERSATION_FIELDS, ROOT_FIELDS};
use super::*;

/// The shared fields whose stored units this library has projected.
const META_KEY: &str = "projectedSharedFields";

/// Fields this build shares that app-v1.0.0 did not. A library without a record was last
/// opened by a build sharing at most the others.
const LISTED_SINCE_FIRST_RELEASE: &[(&str, &str)] = &[
    ("root", "authorNotePresets"),
    ("root", "blockedRealmCreators"),
    ("root", "loadoutApplyOptions"),
    ("root", "risunestScrollNavigation"),
    ("root", "risunestSyncStatusUi"),
    ("character", "onboarding"),
    ("conversation", "authorNotePreset"),
    ("conversation", "onboardingDone"),
    ("conversation", "promptPresetId"),
];

#[derive(Serialize, Deserialize, PartialEq)]
struct Projected {
    root: BTreeSet<String>,
    character: BTreeSet<String>,
    conversation: BTreeSet<String>,
}

impl Projected {
    fn current() -> Self {
        let set = |fields: &[&str]| fields.iter().map(|field| (*field).to_owned()).collect();
        Self {
            root: set(ROOT_FIELDS),
            character: set(CHARACTER_FIELDS),
            conversation: set(CONVERSATION_FIELDS),
        }
    }

    fn first_release() -> Self {
        let mut projected = Self::current();
        for (kind, field) in LISTED_SINCE_FIRST_RELEASE {
            projected.fields_mut(kind).remove(*field);
        }
        projected
    }

    fn recorded(db: &Connection) -> StoreResult<Self> {
        let recorded: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key=?1", [META_KEY], |r| r.get(0))
            .optional()?;
        // Projecting again finds nothing to fill, so a record this build cannot read starts over.
        Ok(recorded
            .and_then(|recorded| serde_json::from_str(&recorded).ok())
            .unwrap_or_else(Self::first_release))
    }

    fn fields_mut(&mut self, kind: &str) -> &mut BTreeSet<String> {
        match kind {
            "root" => &mut self.root,
            "character" => &mut self.character,
            _ => &mut self.conversation,
        }
    }
}

/// Whether the document a field unit patches exists and lacks that field.
fn lacks_field(tx: &Transaction<'_>, generation: &str, key: &UnitKey) -> StoreResult<bool> {
    let p = key.components();
    let path = format!("$.{}", serde_json::to_string(p.last().unwrap())?);
    let field_type: Option<Option<String>> = match p[0].as_str() {
        "root" => tx
            .query_row("SELECT json_type(value,?2) FROM root WHERE generation=?1", params![generation, path], |r| r.get(0))
            .optional()?,
        "character" => tx
            .query_row("SELECT json_type(detail,?3) FROM characters WHERE generation=?1 AND character_id=?2", params![generation, p[1], path], |r| r.get(0))
            .optional()?,
        _ => tx
            .query_row("SELECT json_type(detail,?4) FROM conversations WHERE generation=?1 AND character_id=?2 AND conversation_id=?3", params![generation, p[1], p[2], path], |r| r.get(0))
            .optional()?,
    };
    Ok(matches!(field_type, Some(None)))
}

/// Whether a large unit's body is stored. A build that did not share a field kept no body for
/// its large units, so such a unit waits for a later receive.
pub(in crate::persistent_store) fn body_stored(tx: &Transaction<'_>, value: &UnitValue) -> StoreResult<bool> {
    let UnitValue::Object { descriptor, .. } = value else { return Ok(true) };
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)",
        [&descriptor.object_hash],
        |r| r.get(0),
    )?)
}

/// Stored units of the fields `projected` does not list, for documents of `generation` that
/// lack the field.
fn lacking_units(tx: &Transaction<'_>, generation: &str, projected: &Projected) -> StoreResult<Vec<(UnitKey, UnitValue)>> {
    let current = Projected::current();
    let mut candidates = BTreeSet::new();
    for (kind, column, now, then) in [
        ("root", 1, &current.root, &projected.root),
        ("character", 2, &current.character, &projected.character),
        ("conversation", 3, &current.conversation, &projected.conversation),
    ] {
        let added: Vec<&String> = now.difference(then).collect();
        if added.is_empty() {
            continue;
        }
        let mut statement = tx.prepare(&format!(
            "SELECT key FROM lww_units WHERE json_extract(key,'$[0]')=?1 AND json_extract(key,'$[{column}]') IN (SELECT value FROM json_each(?2))"
        ))?;
        let keys = statement.query_map(params![kind, serde_json::to_string(&added)?], |r| r.get::<_, String>(0))?;
        for key in keys {
            candidates.insert(key?);
        }
    }
    let mut lacking = Vec::new();
    for key in candidates {
        let key: UnitKey = wire(key.try_into())?;
        if !projection::known(&key) {
            continue;
        }
        let Some((_, value)) = read_unit(tx, &key)? else { continue };
        if matches!(value, UnitValue::Deleted) || !body_stored(tx, &value)? {
            continue;
        }
        // Restoring an archived character projects every unit it holds.
        if parent_status_in_generation(tx, &key, Some(generation))? == "ready" && lacks_field(tx, generation, &key)? {
            lacking.push((key, value));
        }
    }
    Ok(lacking)
}

/// A backup an earlier build made holds units of fields that build did not share, while its
/// documents lack them.
pub(in crate::persistent_store) fn project_replacement_listed_fields(tx: &Transaction<'_>, generation: &str) -> StoreResult<()> {
    for (key, value) in lacking_units(tx, generation, &Projected::first_release())? {
        projection::apply(tx, generation, &key, &value)?;
    }
    Ok(())
}

impl PersistentStore {
    /// A build that did not share a field kept its received units opaque, so the documents
    /// lack them. Projects those units once a build shares the field, without publishing them.
    pub(in crate::persistent_store) fn lww_project_listed_fields(&mut self) -> StoreResult<()> {
        let current = Projected::current();
        // Most opens find the record current, so they take no write lock.
        if Projected::recorded(&self.connection)? == current {
            return Ok(());
        }
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recorded = Projected::recorded(&tx)?;
        if recorded == current {
            return Ok(());
        }
        let generation = active_generation(&tx)?;
        let pending = lacking_units(&tx, &generation, &recorded)?;
        if !pending.is_empty() {
            let revision = current_revision(&tx)? + 1;
            super::super::content_change_index::begin_mutation(&tx, &generation, revision, "server")?;
            for (key, value) in &pending {
                projection::apply(&tx, &generation, key, value)?;
            }
            super::super::content_change_index::finish_mutation(&tx)?;
            commit::set_active(&tx, revision, &generation)?;
        }
        tx.execute(
            "INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![META_KEY, serde_json::to_string(&current)?],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // app-v1.0.0 shared 295 root, 78 character and 22 conversation fields; the digest covers
    // their names as `kind:field` lines in order. A field listed after it and missing from
    // `LISTED_SINCE_FIRST_RELEASE` would stay out of the documents after an upgrade from a
    // build without a record, and after restoring an earlier backup.
    #[test]
    fn the_first_release_baseline_lists_only_what_app_v1_0_0_shared() {
        let baseline = Projected::first_release();
        assert_eq!((baseline.root.len(), baseline.character.len(), baseline.conversation.len()), (295, 78, 22));
        let names: String = [("root", &baseline.root), ("character", &baseline.character), ("conversation", &baseline.conversation)]
            .into_iter()
            .flat_map(|(kind, fields)| fields.iter().map(move |field| format!("{kind}:{field}
")))
            .collect();
        assert_eq!(risunest_sync_wire::hash(names.as_bytes()), "fc8aa6ccca3d53c942ff8e2c911f949e4f567ec6395a36dc4ad4147740153e4a");
    }
}
