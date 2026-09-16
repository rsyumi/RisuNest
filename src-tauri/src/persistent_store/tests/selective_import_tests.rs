use super::*;
use crate::local_backup::NeverCancelled;
use crate::persistent_store::portable::{create_raw_tables, stage_portable_records_selected};
use crate::portable_backup::{close_selection, ArchiveSelection};

/// A raw archive projection with two characters, two presets and one plugin storage record. The
/// settings record names the second preset, so a partial import has to keep that choice honest.
fn archive() -> rusqlite::Connection {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    create_raw_tables(&db).unwrap();
    db.execute(
        "INSERT INTO root (value) VALUES ('{\"botPresetsId\":1,\"characterOrder\":[\"char-a\",\"char-b\"]}')",
        [],
    )
    .unwrap();
    for (index, id) in ["char-a", "char-b"].into_iter().enumerate() {
        db.execute(
            "INSERT INTO characters (character_id,configured_index,recent_at,trashed,name,image,conversation_count,type,creator_notes,trash_time,detail)
             VALUES (?1,?2,0,0,?3,NULL,1,'character',NULL,NULL,?4)",
            params![
                id,
                index as i64,
                &id[5..],
                format!("{{\"chaId\":\"{id}\",\"name\":\"{}\",\"type\":\"character\",\"chatPage\":0}}", &id[5..])
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO conversations (character_id,conversation_id,configured_index,recent_at,name,message_count,detail)
             VALUES (?1,?2,0,0,'chat',1,?3)",
            params![
                id,
                format!("chat-{id}"),
                format!("{{\"id\":\"chat-{id}\",\"name\":\"chat\"}}")
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO messages (character_id,conversation_id,message_index,message_id,value)
             VALUES (?1,?2,0,NULL,'{\"role\":\"user\",\"data\":\"hello\"}')",
            params![id, format!("chat-{id}")],
        )
        .unwrap();
    }
    for (index, name) in ["default", "studio"].into_iter().enumerate() {
        db.execute(
            "INSERT INTO bot_presets (preset_id,configured_index,name,image,value) VALUES (?1,?2,?3,NULL,?4)",
            params![
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
    for table in ["asset_repository_authority"] {
        db.execute(
            &format!("INSERT INTO {table} (value) VALUES ('{{\"format\":\"legacy\"}}')"),
            [],
        )
        .unwrap();
    }
    db
}

fn selection(characters: &[&str], presets: &[&str]) -> ArchiveSelection {
    ArchiveSelection {
        characters: characters.iter().map(|id| (*id).to_owned()).collect(),
        presets: presets.iter().map(|id| (*id).to_owned()).collect(),
        plugins: Vec::new(),
        excluded: Vec::new(),
    }
}

fn stage(store: &mut PersistentStore, source: &rusqlite::Connection, chosen: ArchiveSelection) -> String {
    let closed = close_selection(source, &chosen).unwrap();
    stage_portable_records_selected(store, source, &closed, &NeverCancelled)
        .unwrap()
        .staging_id
}

fn ids(store: &PersistentStore, sql: &str, generation: &str) -> Vec<String> {
    let mut statement = store.connection.prepare(sql).unwrap();
    let mut rows = statement.query([generation]).unwrap();
    let mut ids = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        ids.push(row.get(0).unwrap());
    }
    ids
}

#[test]
fn a_partial_import_stages_only_what_was_chosen_and_activates() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    let staging = stage(&mut store, &source, selection(&["char-b"], &["1"]));

    assert_eq!(
        ids(
            &store,
            "SELECT character_id FROM characters WHERE generation=?1 ORDER BY configured_index",
            &staging
        ),
        ["char-b"]
    );
    assert_eq!(
        ids(
            &store,
            "SELECT conversation_id FROM conversations WHERE generation=?1",
            &staging
        ),
        ["chat-char-b"],
        "a character's conversations follow it"
    );
    assert_eq!(
        ids(
            &store,
            "SELECT storage_key FROM plugin_storage WHERE generation=?1",
            &staging
        ),
        Vec::<String>::new(),
        "nothing comes in that was not chosen"
    );

    store.replace_commit(&staging, Some(0)).unwrap();
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        ids(
            &store,
            "SELECT character_id FROM characters WHERE generation=?1",
            &active_generation(&store.connection).unwrap()
        ),
        ["char-b"],
        "the activated library holds exactly what was chosen"
    );
}

#[test]
fn the_records_that_stay_are_renumbered_and_the_chosen_preset_follows_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    let staging = stage(&mut store, &source, selection(&["char-b"], &["1"]));

    let index: i64 = store
        .connection
        .query_row(
            "SELECT configured_index FROM characters WHERE generation=?1",
            [&staging],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index, 0, "the dropped record left a gap that is closed");
    let preset: String = store
        .connection
        .query_row(
            "SELECT preset_id FROM bot_presets WHERE generation=?1",
            [&staging],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(preset, "0", "a preset is named by its own position");
    let selected: i64 = store
        .connection
        .query_row(
            "SELECT json_extract(value,'$.botPresetsId') FROM root WHERE generation=?1",
            [&staging],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(selected, 0, "the chosen preset is still the chosen one");
}

#[test]
fn the_settings_record_always_comes_and_nothing_else_is_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    let staging = stage(&mut store, &source, selection(&[], &[]));

    let order: String = store
        .connection
        .query_row(
            "SELECT json_extract(value,'$.characterOrder') FROM root WHERE generation=?1",
            [&staging],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        order, "[\"char-a\",\"char-b\"]",
        "an import never edits the settings beyond the preset it has to keep valid"
    );
    assert!(ids(
        &store,
        "SELECT character_id FROM characters WHERE generation=?1",
        &staging
    )
    .is_empty());
}

#[test]
fn a_group_brings_its_members_even_when_only_the_group_was_chosen() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    source
        .execute(
            "UPDATE characters SET detail='{\"chaId\":\"char-a\",\"name\":\"a\",\"type\":\"group\",\"characters\":[\"char-b\"],\"chatPage\":0}', type='group' WHERE character_id='char-a'",
            [],
        )
        .unwrap();
    let staging = stage(&mut store, &source, selection(&["char-a"], &[]));
    assert_eq!(
        ids(
            &store,
            "SELECT character_id FROM characters WHERE generation=?1 ORDER BY character_id",
            &staging
        ),
        ["char-a", "char-b"]
    );
}

#[test]
fn a_damaged_archive_is_refused_before_anything_is_staged() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    // A conversation count that disagrees with the conversations is a record rule, and the same
    // gate that guards a whole import guards a partial one.
    source
        .execute("UPDATE characters SET conversation_count=9", [])
        .unwrap();
    let closed = close_selection(&source, &selection(&["char-a"], &[])).unwrap();
    let error =
        stage_portable_records_selected(&mut store, &source, &closed, &NeverCancelled).unwrap_err();
    assert!(matches!(error, StoreError::Validation { .. }), "{error:?}");
    assert_eq!(store.revision().unwrap(), 0);
}

#[test]
fn a_partial_import_never_writes_to_the_archive_it_read() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    let before = crate::persistent_store::portable::digest_raw_tables(&source, &NeverCancelled)
        .unwrap();

    let staging = stage(&mut store, &source, selection(&["char-b"], &["1"]));
    store.replace_commit(&staging, Some(0)).unwrap();

    assert_eq!(
        crate::persistent_store::portable::digest_raw_tables(&source, &NeverCancelled).unwrap(),
        before,
        "the archive an import read is exactly what it was"
    );
}

#[test]
fn a_selection_that_is_refused_leaves_the_library_exactly_as_it_was() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let source = archive();
    source
        .execute("UPDATE conversations SET message_count=9", [])
        .unwrap();
    let before = active_generation(&store.connection).unwrap();

    let closed = close_selection(&source, &selection(&["char-a"], &[])).unwrap();
    assert!(
        stage_portable_records_selected(&mut store, &source, &closed, &NeverCancelled).is_err()
    );
    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(active_generation(&store.connection).unwrap(), before);
}
