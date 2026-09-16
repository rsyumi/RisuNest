//! Row integrity checks for portable raw records. Payload/owner/reference validation is a
//! separate prerequisite; passing this module alone never authorizes activation.
use super::{
    portable::{PortableTable, TABLES},
    StoreError, StoreResult,
};
use crate::data_health::{codes, Finding, FindingSink, FirstFinding};
use crate::local_backup::CancellationProbe;
use rusqlite::{
    types::{Type, ValueRef},
    Connection, Row,
};
use serde_json::Value;

fn invalid(message: &'static str) -> StoreError {
    StoreError::Validation {
        message: message.into(),
    }
}
fn require(condition: bool, message: &'static str) -> StoreResult<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

pub(crate) fn validate_records(db: &Connection, probe: &dyn CancellationProbe) -> StoreResult<()> {
    let mut first = FirstFinding::default();
    validate_records_into(db, probe, &mut first)?;
    match first.into_inner() {
        Some(finding) => Err(StoreError::Validation {
            message: finding.detail,
        }),
        None => Ok(()),
    }
}

/// The collecting counterpart. Both modes run the same rules over the same rows, so a diagnosis
/// can never disagree with the gate that refuses a backup or an activation.
pub(crate) fn validate_records_into(
    db: &Connection,
    probe: &dyn CancellationProbe,
    sink: &mut dyn FindingSink,
) -> StoreResult<()> {
    for table in TABLES {
        let duplicate: bool = db.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {} GROUP BY {} HAVING count(*)>1)",
                table.name, table.order
            ),
            [],
            |r| r.get(0),
        )?;
        if duplicate
            && !sink.record(Finding::new(
                codes::RECORD_INVALID,
                table.name,
                "",
                "duplicate portable record identity",
            ))
        {
            return Ok(());
        }
        let mut statement = db.prepare(&format!(
            "SELECT {} FROM {} ORDER BY {}",
            table.column_list(),
            table.name,
            table.order
        ))?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            require(
                !probe.is_cancelled(),
                "portable record validation cancelled",
            )?;
            if let Err(error) = validate_row(table, row) {
                let StoreError::Validation { message } = error else {
                    return Err(error);
                };
                if !sink.record(Finding::new(
                    codes::RECORD_INVALID,
                    table.name,
                    row_identity(table, row)?,
                    message,
                )) {
                    return Ok(());
                }
            }
        }
    }
    for (sql, code, owner, message) in RELATIONSHIPS {
        require(
            !probe.is_cancelled(),
            "portable record validation cancelled",
        )?;
        let bad: bool = db.query_row(sql, [], |r| r.get(0))?;
        if bad && !sink.record(Finding::new(code, *owner, "", *message)) {
            return Ok(());
        }
    }
    Ok(())
}

fn validate_row(table: &PortableTable, row: &Row<'_>) -> StoreResult<()> {
    for (index, (column, declaration)) in table.columns.iter().enumerate() {
        let actual = row.get_ref(index)?.data_type();
        let nullable = matches!(
            *column,
            "image"
                | "creator_notes"
                | "trash_time"
                | "archived_object"
                | "message_id"
                | "object_hash"
                | "manifest_hash"
                | "inlay_type"
                | "width"
                | "height"
                | "claimed_from"
                | "import_batch_id"
                | "assigned_at"
        );
        require(
            actual
                == if *declaration == "INTEGER" {
                    Type::Integer
                } else {
                    Type::Text
                }
                || nullable && actual == Type::Null,
            "portable SQL storage class mismatch",
        )?;
        if actual == Type::Text {
            let _: String = row
                .get(index)
                .map_err(|_| invalid("portable TEXT is not valid UTF-8"))?;
        }
    }
    match table.name {
        "root" => {
            let value = json(row, 0)?;
            require(
                value.is_object()
                    && ["characters", "botPresets", "pluginCustomStorage", "pluginStorageMeta"]
                        .iter()
                        .all(|k| value.get(k).is_none()),
                "portable root contains separated records",
            )?;
        }
        "bot_presets" => {
            let value = json(row, 4)?;
            let index: i64 = row.get(1)?;
            require(
                index >= 0 && row.get::<_, String>(0)? == index.to_string(),
                "portable preset identity mismatch",
            )?;
            text_equal(
                row,
                2,
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )?;
            optional_text_equal(row, 3, value.get("image").and_then(Value::as_str))?;
        }
        "characters" => {
            let value = json(row, 10)?;
            require(
                value.is_object() && value.get("chats").is_none(),
                "portable character contains separated conversations",
            )?;
            required_id(row, 0, &value, "chaId")?;
            text_equal(
                row,
                4,
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("portable character name is invalid"))?,
            )?;
            optional_text_equal(row, 5, value.get("image").and_then(Value::as_str))?;
            require(
                row.get::<_, i64>(1)? >= 0
                    && row.get::<_, i64>(2)?
                        == value
                            .get("lastInteraction")
                            .and_then(Value::as_i64)
                            .unwrap_or_default(),
                "portable character summary differs from detail",
            )?;
            require(
                row.get::<_, i64>(3)? == i64::from(value.get("trashTime").is_some()),
                "portable character trash presence differs",
            )?;
            text_equal(
                row,
                7,
                value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("character"),
            )?;
            optional_text_equal(row, 8, value.get("creatorNotes").and_then(Value::as_str))?;
            require(
                row.get::<_, Option<i64>>(9)?
                    == value.get("trashTime").and_then(Value::as_i64),
                "portable character trash time differs",
            )?;
        }
        "conversations" => {
            let value = json(row, 6)?;
            require(
                value.is_object() && value.get("message").is_none(),
                "portable conversation contains separated messages",
            )?;
            required_id(row, 1, &value, "id")?;
            text_equal(
                row,
                4,
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("portable conversation name is invalid"))?,
            )?;
            require(row.get::<_, i64>(2)? >= 0, "negative conversation order")?;
            // recent_at is independently maintained when lastDate is absent. A range
            // edit can legitimately retain its previous value after the last message changes.
            if let Some(last) = value.get("lastDate").and_then(Value::as_i64) {
                require(
                    row.get::<_, i64>(3)? == last,
                    "portable conversation date differs",
                )?;
            }
        }
        "messages" => {
            let value = json(row, 4)?;
            require(value.is_object(), "portable message is not an object")?;
            optional_text_equal(row, 3, value.get("chatId").and_then(Value::as_str))?;
        }
        "plugin_storage" => {
            let serialized: String = row.get(4)?;
            let _: Value = parse_json(&serialized)?;
            let owner: String = row.get(0)?;
            require(
                super::plugin_owner::validate_owner(&owner),
                "portable plugin owner is invalid",
            )?;
            require(
                row.get::<_, i64>(2)? == serialized.len() as i64
                    && row.get::<_, i64>(3)? >= 0,
                "portable plugin size or ordinal mismatch",
            )?;
        }
        "asset_aliases" => {
            let alias = super::AssetAlias {
                key: row.get(0)?,
                object_hash: row.get(1)?,
                kind: row.get(2)?,
                size: row.get(3)?,
                mime: row.get(4)?,
                name: row.get(5)?,
                ext: row.get(6)?,
                inlay_type: row.get(7)?,
                width: row.get(8)?,
                height: row.get(9)?,
                metadata: json(row, 10)?,
            };
            alias.validate()?;
            require(
                !alias.key.is_empty() && !alias.key.contains('\0'),
                "portable asset key is invalid",
            )?;
        }
        "asset_repository_authority" => {
            let authority: super::AssetRepositoryAuthorityState =
                parse_json(&row.get::<_, String>(0)?)?;
            authority.validate()?;
            require(
                !matches!(
                    authority,
                    super::AssetRepositoryAuthorityState::Preparing { .. }
                ),
                "portable asset authority is preparing",
            )?;
        }
        "asset_owner_heads" => {
            let kind: String = row.get(0)?;
            let locator: String = row.get(1)?;
            let owner = match kind.as_str() {
                "character-additional-assets" => {
                    super::AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: locator,
                    }
                }
                "root-module-assets" => super::AssetOwnerLocator::RootModuleAssets {
                    index: owner_index(&locator)?,
                },
                "persona-embedded-module-assets" => {
                    super::AssetOwnerLocator::PersonaEmbeddedModuleAssets {
                        index: owner_index(&locator)?,
                    }
                }
                _ => return Err(invalid("portable owner kind is invalid")),
            };
            let present: i64 = row.get(2)?;
            require(
                matches!(present, 0 | 1),
                "portable owner presence is invalid",
            )?;
            super::AssetOwnerHead {
                owner,
                present: present == 1,
                manifest_hash: row.get(3)?,
                entry_count: row.get(4)?,
            }
            .validate()?;
        }
        _ => return Err(invalid("unreviewed portable table")),
    }
    Ok(())
}

/// Singleton tables order by their only column, which is the record body itself.
fn row_identity(table: &PortableTable, row: &Row<'_>) -> StoreResult<String> {
    if table.order == "value" {
        return Ok(String::new());
    }
    let mut parts = Vec::new();
    for column in table.order.split(',').map(str::trim) {
        parts.push(match row.get_ref(column)? {
            ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            ValueRef::Integer(value) => value.to_string(),
            _ => String::new(),
        });
    }
    Ok(parts.join("/"))
}

/// Cross-record rules, each reported on its own so one damaged relationship cannot hide the
/// others. Orphans are reported as orphans: nothing points at them and nothing breaks.
const RELATIONSHIPS: &[(&str, &str, &str, &str)] = &[
    (
        "SELECT (SELECT count(*) FROM root)!=1",
        codes::RECORD_INVALID,
        "root",
        "portable root record count is not one",
    ),
    (
        "SELECT (SELECT count(*) FROM asset_repository_authority)>1",
        codes::RECORD_INVALID,
        "authority",
        "portable storage authority has more than one record",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM conversations c WHERE NOT EXISTS(SELECT 1 FROM characters p WHERE p.character_id=c.character_id))",
        codes::RECORD_ORPHAN,
        "conversations",
        "portable conversation has no character",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM messages m WHERE NOT EXISTS(SELECT 1 FROM conversations c WHERE c.character_id=m.character_id AND c.conversation_id=m.conversation_id))",
        codes::RECORD_ORPHAN,
        "messages",
        "portable message has no conversation",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM characters c WHERE c.conversation_count!=(SELECT count(*) FROM conversations x WHERE x.character_id=c.character_id))",
        codes::RECORD_INVALID,
        "characters",
        "portable character conversation count differs",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM conversations c WHERE c.message_count!=(SELECT count(*) FROM messages m WHERE m.character_id=c.character_id AND m.conversation_id=c.conversation_id))",
        codes::RECORD_INVALID,
        "conversations",
        "portable conversation message count differs",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM messages GROUP BY character_id,conversation_id HAVING min(message_index)!=0 OR max(message_index)!=count(*)-1)",
        codes::RECORD_INVALID,
        "messages",
        "portable message index range is not contiguous",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM characters GROUP BY configured_index HAVING count(*)>1)",
        codes::RECORD_INVALID,
        "characters",
        "portable character order is duplicated",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM conversations GROUP BY character_id,configured_index HAVING count(*)>1)",
        codes::RECORD_INVALID,
        "conversations",
        "portable conversation order is duplicated",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM plugin_storage GROUP BY ordinal HAVING count(*)>1)",
        codes::RECORD_INVALID,
        "plugin_storage",
        "portable plugin ordinal is duplicated",
    ),
    (
        "SELECT EXISTS(SELECT 1 FROM bot_presets GROUP BY configured_index HAVING count(*)>1)",
        codes::RECORD_INVALID,
        "bot_presets",
        "portable preset order is duplicated",
    ),
];

fn json(row: &Row<'_>, index: usize) -> StoreResult<Value> {
    parse_json(&row.get::<_, String>(index)?)
}
fn parse_json<T: serde::de::DeserializeOwned>(text: &str) -> StoreResult<T> {
    serde_json::from_str(text).map_err(|_| invalid("portable JSON is invalid"))
}
fn text_equal(row: &Row<'_>, index: usize, value: &str) -> StoreResult<()> {
    require(
        row.get::<_, String>(index)? == value,
        "portable derived text differs from JSON",
    )
}
fn optional_text_equal(row: &Row<'_>, index: usize, value: Option<&str>) -> StoreResult<()> {
    require(
        row.get::<_, Option<String>>(index)?.as_deref() == value,
        "portable optional text differs from JSON",
    )
}
fn required_id(row: &Row<'_>, index: usize, value: &Value, key: &str) -> StoreResult<()> {
    let id = value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("portable JSON identity is absent"))?;
    text_equal(row, index, id)
}
fn owner_index(locator: &str) -> StoreResult<i64> {
    let index = locator
        .parse::<i64>()
        .map_err(|_| invalid("portable owner index is invalid"))?;
    require(
        index >= 0 && index.to_string() == locator,
        "portable owner index is noncanonical",
    )?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_health::{Findings, Severity};
    use crate::persistent_store::{
        portable::{create_raw_tables, digest_raw_tables},
        PersistentStore,
    };
    struct Never;
    impl CancellationProbe for Never {
        fn is_cancelled(&self) -> bool {
            false
        }
    }
    struct Always;
    impl CancellationProbe for Always {
        fn is_cancelled(&self) -> bool {
            true
        }
    }

    fn collect(db: &Connection, limit: usize) -> Findings {
        let mut findings = Findings::new(limit);
        validate_records_into(db, &Never, &mut findings).expect("collect record findings");
        findings
    }

    fn fixture() -> (tempfile::TempDir, Connection) {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let database: Value =
            serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json")).unwrap();
        let stage = store.replace_begin().unwrap();
        let mut root = database.clone();
        root.as_object_mut().unwrap().remove("characters");
        root.as_object_mut().unwrap().remove("botPresets");
        root.as_object_mut().unwrap().insert(
            "pluginCustomStorage".into(),
            serde_json::json!({"synthetic":{"value":7}}),
        );
        store.replace_put_root(&stage.staging_id, &root).unwrap();
        store
            .replace_put_presets(
                &stage.staging_id,
                database["botPresets"].as_array().unwrap(),
            )
            .unwrap();
        store
            .replace_add_characters(
                &stage.staging_id,
                database["characters"].as_array().unwrap(),
            )
            .unwrap();
        store.replace_commit(&stage.staging_id, Some(0)).unwrap();
        let lease = store.acquire_revision(1).unwrap().lease;
        let mut output = Connection::open(directory.path().join("raw.sqlite")).unwrap();
        create_raw_tables(&output).unwrap();
        store
            .capture_portable_records(&lease, &mut output, &Never)
            .unwrap();
        store.release_revision(&lease).unwrap();
        (directory, output)
    }

    #[test]
    fn validates_native_fixture_without_changing_any_sql_value() {
        let (_directory, db) = fixture();
        let before = digest_raw_tables(&db, &Never).unwrap();
        validate_records(&db, &Never).unwrap();
        assert_eq!(before, digest_raw_tables(&db, &Never).unwrap());
    }

    #[test]
    fn refuses_damaged_identity_order_parent_and_derived_columns() {
        for sql in [
            "UPDATE characters SET detail='not JSON'",
            "UPDATE characters SET character_id='wrong'",
            "UPDATE characters SET name='wrong'",
            "UPDATE characters SET trashed=1-trashed",
            "UPDATE characters SET conversation_count=conversation_count+1",
            "DELETE FROM characters",
            "DELETE FROM conversations",
            "UPDATE conversations SET message_count=message_count+1",
            "UPDATE messages SET message_index=message_index+1",
            "UPDATE messages SET message_id='wrong'",
            "UPDATE plugin_storage SET byte_size=byte_size+1",
            "INSERT INTO root SELECT * FROM root",
            "UPDATE root SET value=x'00'",
        ] {
            let (_directory, db) = fixture();
            let changed = db.execute(sql, []).unwrap();
            assert!(
                changed > 0,
                "synthetic mutation did not exercise its target: {sql}"
            );
            assert!(
                validate_records(&db, &Never).is_err(),
                "damaged rows accepted: {sql}"
            );
        }
    }

    #[test]
    fn permits_null_duplicate_message_ids_and_gaps_in_plugin_ordinals() {
        let (_directory, db) = fixture();
        db.execute(
            "UPDATE messages SET message_id=NULL,value=json_remove(value,'$.chatId')",
            [],
        )
        .unwrap();
        db.execute("UPDATE plugin_storage SET ordinal=ordinal*3+7", [])
            .unwrap();
        validate_records(&db, &Never).unwrap();
        db.execute(
            "UPDATE messages SET message_id='same',value=json_set(value,'$.chatId','same')",
            [],
        )
        .unwrap();
        validate_records(&db, &Never).unwrap();
    }

    #[test]
    fn collecting_mode_reports_every_damaged_row_where_fail_fast_reports_one() {
        let (_directory, db) = fixture();
        assert!(collect(&db, 64).items.is_empty());
        validate_records(&db, &Never).expect("healthy fixture passes both modes");

        let damaged = db.execute("UPDATE characters SET name='wrong'", []).unwrap();
        assert!(damaged > 1, "fixture needs several characters");
        assert!(validate_records(&db, &Never).is_err());
        let findings = collect(&db, 64);
        assert_eq!(findings.items.len(), damaged);
        assert_eq!(findings.omitted, 0);
        for finding in &findings.items {
            assert_eq!(finding.code, codes::RECORD_INVALID);
            assert_eq!(finding.severity, Severity::Blocking);
            assert_eq!(finding.owner.kind, "characters");
            assert!(!finding.owner.id.is_empty(), "row identity is missing");
        }

        let bounded = collect(&db, 1);
        assert_eq!(bounded.items.len(), 1);
        assert_eq!(bounded.omitted as usize, damaged - 1);
    }

    #[test]
    fn collecting_mode_reports_orphans_apart_from_invalid_records() {
        let (_directory, db) = fixture();
        db.execute("DELETE FROM characters", []).unwrap();
        let findings = collect(&db, 64);
        let orphan = findings
            .items
            .iter()
            .find(|finding| finding.code == codes::RECORD_ORPHAN)
            .expect("conversations without a character are orphans");
        // An orphan still refuses activation, so the diagnosis must not call it harmless.
        assert_eq!(orphan.severity, Severity::Blocking);
        assert_eq!(orphan.owner.kind, "conversations");
        assert!(validate_records(&db, &Never).is_err());
    }

    #[test]
    fn collecting_mode_still_stops_on_cancellation() {
        let (_directory, db) = fixture();
        let mut findings = Findings::new(64);
        assert!(validate_records_into(&db, &Always, &mut findings).is_err());
    }

    #[test]
    fn staging_keeps_the_live_library_and_preserves_raw_sql_after_commit() {
        let (_source_directory, db) = fixture();
        let expected = digest_raw_tables(&db, &Never).unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(destination.path()).unwrap();
        let old = store.replace_begin().unwrap();
        store
            .replace_put_root(
                &old.staging_id,
                &serde_json::json!({"marker":"receiving-device"}),
            )
            .unwrap();
        store.replace_commit(&old.staging_id, Some(0)).unwrap();
        let stage = store.stage_portable_records(&db, &Never).unwrap();
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(
            store.read_root(None).unwrap().value,
            serde_json::json!({"marker":"receiving-device"})
        );
        // This synthetic record-only fixture has no attachment requirements. Production callers
        // must complete payload/reference validation and recovery before the same commit boundary.
        store.replace_commit(&stage.staging_id, Some(1)).unwrap();
        let lease = store.acquire_revision(2).unwrap().lease;
        let mut recaptured = Connection::open_in_memory().unwrap();
        create_raw_tables(&recaptured).unwrap();
        let actual = store
            .capture_portable_records(&lease, &mut recaptured, &Never)
            .unwrap();
        assert_eq!(expected, actual);
        store.release_revision(&lease).unwrap();
    }

    #[test]
    fn invalid_raw_rows_never_create_or_activate_a_staging_generation() {
        let (_source_directory, db) = fixture();
        db.execute("UPDATE characters SET detail='broken'", [])
            .unwrap();
        let destination = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(destination.path()).unwrap();
        assert!(store.stage_portable_records(&db, &Never).is_err());
        assert_eq!(store.revision().unwrap(), 0);
        let staged: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM root WHERE generation LIKE 'staging-%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(staged, 0);
    }
}
