use super::*;
use crate::persistent_store::{active_generation, snapshot::ASSET_ROOT_SCANS};
use sha2::{Digest, Sha256};
use std::time::Instant;

fn preview_fixture(total: usize) -> (tempfile::TempDir, PersistentStoreState, u64) {
    let directory = tempdir().unwrap();
    let mut store = PersistentStore::open(directory.path()).unwrap();
    let generation = active_generation(&store.connection).unwrap();
    let tx = store.connection.transaction().unwrap();
    let mut candidate_bytes = 0;
    for index in 0..total {
        let bytes = format!("synthetic-gc-object-{index}").into_bytes();
        let hash = hex::encode(Sha256::digest(&bytes));
        let path = directory
            .path()
            .join(crate::asset_repository::object_physical_key(&hash));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &bytes).unwrap();
        tx.execute(
            "INSERT INTO asset_objects(object_hash, byte_size, created_at_ms) VALUES (?1, ?2, 0)",
            rusqlite::params![hash, bytes.len() as i64],
        )
        .unwrap();
        if index % 10 == 0 {
            candidate_bytes += bytes.len() as u64;
        } else {
            tx.execute(
                "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
                 VALUES (?1, ?2, ?3, 'asset', ?4, 'application/octet-stream', ?2, 'bin')",
                rusqlite::params![generation, format!("assets/synthetic-{index}.bin"), hash, bytes.len() as i64],
            ).unwrap();
        }
    }
    tx.commit().unwrap();
    let state = PersistentStoreState {
        store: Mutex::new(Some(store)),
        ..PersistentStoreState::default()
    };
    (directory, state, candidate_bytes)
}

fn run_preview(total: usize) -> usize {
    let (_directory, state, expected_bytes) = preview_fixture(total);
    let guard = state.admit_renderer_operation().unwrap();
    ASSET_ROOT_SCANS.with(|count| count.set(0));
    let start = Instant::now();
    let result = pds_asset_gc_preview_all(&state, &guard).unwrap();
    let elapsed = start.elapsed();
    let scans = ASSET_ROOT_SCANS.with(|count| count.get());
    assert_eq!(result.candidate_count, total.div_ceil(10) as u64);
    assert_eq!(result.candidate_bytes, expected_bytes);
    assert_eq!(result.deleted_count, 0);
    assert_eq!(result.deleted_bytes, 0);
    assert!(result.blockers.is_empty());
    eprintln!(
        "synthetic GC preview: objects={total}, root_scans={scans}, elapsed_ms={:.3}",
        elapsed.as_secs_f64() * 1000.0
    );
    scans
}

#[test]
fn preview_collects_library_roots_once_across_catalog_pages() {
    assert_eq!(run_preview(257), 1);
}

#[test]
fn preview_does_not_cache_roots_across_invocations_or_authorize_deletion() {
    let (directory, state, _) = preview_fixture(1);
    let guard = state.admit_renderer_operation().unwrap();
    assert_eq!(
        pds_asset_gc_preview_all(&state, &guard)
            .unwrap()
            .candidate_count,
        1
    );
    let hash = hex::encode(Sha256::digest(b"synthetic-gc-object-0"));
    with_store_mutex_mut_admitted(&state, &guard, |store| {
        let generation = active_generation(&store.connection)?;
        store.connection.execute(
            "INSERT INTO asset_aliases (generation, logical_key, object_hash, kind, size, mime, name, ext)
             VALUES (?1, 'assets/new-reference.bin', ?2, 'asset', 21, 'application/octet-stream', 'new', 'bin')",
            rusqlite::params![generation, hash],
        )?;
        Ok(())
    }).unwrap();
    let deleted = pds_asset_gc_execute_all(&state, &guard).unwrap();
    assert_eq!(deleted.candidate_count, 0);
    assert_eq!(deleted.deleted_count, 0);
    assert!(directory
        .path()
        .join(crate::asset_repository::object_physical_key(&hash))
        .is_file());
    assert_eq!(
        pds_asset_gc_preview_all(&state, &guard)
            .unwrap()
            .candidate_count,
        0
    );
}

#[test]
#[ignore = "synthetic 10k-object disk benchmark; run explicitly with --ignored --nocapture"]
fn preview_ten_thousand_assets() {
    run_preview(10_000);
}

/// A preview that can be argued with: every object it looked at, what it decided and, when it
/// kept one, what is holding it.
#[test]
fn the_preview_says_what_it_decided_about_every_object_it_looked_at() {
    let (directory, state, _) = preview_fixture(20);
    let guard = state.admit_renderer_operation().unwrap();

    // One object is held by a repair journal alone, which is what an undo still needs.
    let held = hex::encode(Sha256::digest(b"held-by-a-repair"));
    let path = directory
        .path()
        .join(crate::asset_repository::object_physical_key(&held));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"held-by-a-repair").unwrap();
    with_store_mutex(&state, |store| {
        store.connection.execute(
            "INSERT INTO asset_objects(object_hash, byte_size, created_at_ms) VALUES (?1, ?2, 0)",
            rusqlite::params![held, 16_i64],
        )?;
        Ok(())
    })
    .unwrap();
    crate::data_health::journal::write(
        directory.path(),
        &crate::data_health::journal::Journal {
            id: "repair-1".to_owned(),
            created_at: 0,
            from_revision: 0,
            to_revision: 1,
            applied: Vec::new(),
            records: Vec::new(),
            released_objects: std::collections::BTreeSet::from([held.clone()]),
        },
    )
    .unwrap();

    let result = pds_asset_gc_preview_all(&state, &guard).expect("preview every page");
    let rows = &result.candidates;
    assert_eq!(rows.len() as u64, 21, "every object it looked at is listed");
    assert_eq!(result.omitted, 0);

    let kept = rows
        .iter()
        .find(|row| row.object_hash == held)
        .expect("the held object is listed");
    assert_eq!(kept.state, "held");
    assert_eq!(kept.holders, ["repair"], "the reason it stayed is named");

    let used = rows
        .iter()
        .find(|row| row.state == "held" && row.holders.is_empty())
        .expect("an object an alias registers is held by the library itself");
    assert!(used.bytes > 0);
    assert!(rows.iter().any(|row| row.state == "deletable"));
}
