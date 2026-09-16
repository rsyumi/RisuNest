use super::*;
use crate::data_health::Finding;
use crate::persistent_store::portable::create_raw_tables;

/// A raw archive projection with a group whose member is another character, two presets and one
/// plugin storage record.
fn fixture() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    create_raw_tables(&db).unwrap();
    db.execute("INSERT INTO root (value) VALUES ('{\"botPresetsId\":1}')", [])
        .unwrap();
    for (index, (id, detail)) in [
        (
            "char-a",
            "{\"chaId\":\"char-a\",\"name\":\"a\",\"type\":\"group\",\"characters\":[\"char-b\"],\"chatPage\":0}",
        ),
        (
            "char-b",
            "{\"chaId\":\"char-b\",\"name\":\"b\",\"type\":\"character\",\"chatPage\":0}",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        db.execute(
            "INSERT INTO characters (character_id,configured_index,recent_at,trashed,name,image,conversation_count,type,creator_notes,trash_time,detail)
             VALUES (?1,?2,0,0,?3,NULL,0,'character',NULL,NULL,?4)",
            rusqlite::params![id, index as i64, &id[5..], detail],
        )
        .unwrap();
    }
    for (index, name) in ["default", "studio"].into_iter().enumerate() {
        db.execute(
            "INSERT INTO bot_presets (preset_id,configured_index,name,image,value) VALUES (?1,?2,?3,NULL,?4)",
            rusqlite::params![
                index.to_string(),
                index as i64,
                name,
                format!("{{\"name\":\"{name}\"}}")
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO plugin_storage (owner,storage_key,byte_size,ordinal,value) VALUES ('synthetic-plugin','p-1',2,0,'{}')",
        [],
    )
    .unwrap();
    db
}

fn selection(characters: &[&str], excluded: &[&str]) -> ArchiveSelection {
    ArchiveSelection {
        characters: characters.iter().map(|id| (*id).to_owned()).collect(),
        presets: Vec::new(),
        plugins: Vec::new(),
        excluded: excluded.iter().map(|id| (*id).to_owned()).collect(),
    }
}

#[test]
fn the_inventory_lists_what_an_archive_holds_and_marks_what_is_damaged() {
    let db = fixture();
    let findings = vec![
        Finding::new("record-invalid", "character", "char-a", "record JSON is invalid"),
        Finding::new(
            "reference-missing",
            "message",
            "char-a/chat-1/0",
            "reference has no target in this library",
        ),
        Finding::new("record-invalid", "preset", "1", "portable preset identity mismatch"),
    ];
    let inventory = inventory(&db, &findings).unwrap();
    assert_eq!(
        inventory
            .characters
            .iter()
            .map(|entry| (entry.id.as_str(), entry.damaged))
            .collect::<Vec<_>>(),
        [("char-a", 2), ("char-b", 0)],
        "a conversation's damage is counted against the character the reader chooses"
    );
    assert_eq!(inventory.presets.len(), 2);
    assert_eq!(inventory.presets[1].damaged, 1);
    assert_eq!(inventory.plugins[0].id, "p-1");
}

#[test]
fn closing_a_selection_brings_in_the_members_a_group_names() {
    let db = fixture();
    let closed = close(&db, &selection(&["char-a"], &[])).unwrap();
    assert_eq!(closed.characters, ["char-a", "char-b"]);
    assert_eq!(closed.added, ["char-b"]);
    assert!(closed.dangling.is_empty());
}

#[test]
fn an_excluded_member_stays_out_and_the_broken_reference_is_reported() {
    let db = fixture();
    let closed = close(&db, &selection(&["char-a"], &["char-b"])).unwrap();
    assert_eq!(closed.characters, ["char-a"]);
    assert_eq!(closed.dangling, ["character:char-b"]);
    assert!(closed.added.is_empty());
}

#[test]
fn a_preset_is_chosen_on_its_own_and_an_excluded_one_never_comes() {
    let db = fixture();
    let closed = close(
        &db,
        &ArchiveSelection {
            characters: Vec::new(),
            presets: vec!["0".to_owned(), "1".to_owned()],
            plugins: vec!["p-1".to_owned()],
            excluded: vec!["1".to_owned()],
        },
    )
    .unwrap();
    assert_eq!(closed.presets, ["0"]);
    assert_eq!(closed.plugins, ["p-1"]);
}

#[test]
fn a_selection_that_names_nothing_closes_to_nothing() {
    let db = fixture();
    let closed = close(&db, &selection(&[], &[])).unwrap();
    assert!(closed.characters.is_empty() && closed.presets.is_empty());
    assert!(closed.added.is_empty() && closed.dangling.is_empty());
}

#[test]
fn a_member_the_archive_does_not_hold_is_still_named_by_the_closure() {
    let db = fixture();
    db.execute(
        "UPDATE characters SET detail='{\"chaId\":\"char-b\",\"name\":\"b\",\"type\":\"group\",\"characters\":[\"char-gone\"],\"chatPage\":0}' WHERE character_id='char-b'",
        [],
    )
    .unwrap();
    let closed = close(&db, &selection(&["char-b"], &[])).unwrap();
    // Closure follows what the record says; the archive simply has no such record to stage, and
    // the reference comes in dangling as it already was.
    assert_eq!(closed.characters, ["char-b", "char-gone"]);
}
