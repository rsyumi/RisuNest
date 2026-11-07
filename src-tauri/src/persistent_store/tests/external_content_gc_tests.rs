use super::*;
use crate::external_storage::{
    capture::{CaptureCatalog, DurableCaptureReference},
    content_store::{file_location, ContentStore},
};
use crate::persistent_store::{
    content_capture::ContentCaptureSink,
    external_content_gc::{CollectionOutcome, ContentCollection},
    sync_selection, RootMutation,
};
use risunest_external_storage_format::{
    content_identity::hash,
};

struct Never;
impl crate::local_backup::CancellationProbe for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
}

fn empty_store() -> (tempfile::TempDir, PersistentStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(directory.path()).unwrap();
    (directory, store)
}

fn identity(revision: i64) -> sync_selection::CaptureIdentity {
    sync_selection::CaptureIdentity {
        store_id: "store".into(),
        library_epoch: "library".into(),
        generation: "generation".into(),
        selection_epoch: "selection".into(),
        revision,
    }
}

/// A finished capture of `bodies` under `captures/<id>`, not registered.
fn written(
    store: &PersistentStore,
    id: &str,
    bodies: &[&[u8]],
) -> (DurableCaptureReference, std::path::PathBuf) {
    let external = store.repository_root.join("external-storage");
    let mut catalog =
        CaptureCatalog::create(&external.join("captures").join(id), &external, None).unwrap();
    catalog.begin(&identity(1), None).unwrap();
    for (index, body) in bodies.iter().enumerate() {
        catalog.record(&format!("record/{index}"), body).unwrap();
    }
    catalog.finish().unwrap();
    let reference = catalog.durable_reference(id, &store.repository_root).unwrap();
    let path = catalog.manifest().unwrap().1.to_path_buf();
    (reference, path)
}

fn register(store: &PersistentStore, reference: &DurableCaptureReference, path: &Path) {
    store
        .connection
        .execute(
            "INSERT INTO external_storage_captures VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                reference.capture_id,
                serde_json::to_string(&reference.identity).unwrap(),
                "scope",
                "logical-v1",
                "consumer",
                reference.catalog_hash,
            ],
        )
        .unwrap();
    store
        .connection
        .execute(
            "INSERT INTO external_storage_capture_files VALUES(?1,?2,?3)",
            params![
                reference.capture_id,
                path.to_string_lossy().into_owned(),
                reference.catalog_hash,
            ],
        )
        .unwrap();
}


fn held(store: &PersistentStore, body: &[u8]) -> bool {
    ContentStore::open_existing(&store.repository_root.join("external-storage"))
        .unwrap()
        .unwrap()
        .stat(&hex::encode(hash(body)))
        .unwrap()
        .is_some()
}

fn collected(store: &mut PersistentStore) -> ContentCollection {
    match store.collect_external_content().unwrap() {
        CollectionOutcome::Collected(collection) => collection,
        CollectionOutcome::Deferred => panic!("the collection was deferred"),
    }
}

/// Larger than a database body, so it is kept as a file.
fn large(fill: u8) -> Vec<u8> {
    vec![fill; 70 * 1024]
}

#[test]
fn bodies_no_registered_capture_names_are_reclaimed_from_both_stores() {
    let (_directory, mut store) = empty_store();
    let (kept_big, dropped_big) = (large(1), large(2));
    let (kept, kept_path) = written(&store, "kept", &[b"kept", &kept_big, b"shared"]);
    written(&store, "dropped", &[b"dropped", &dropped_big, b"shared"]);
    register(&store, &kept, &kept_path);

    assert_eq!(
        collected(&mut store),
        ContentCollection { database_bodies: 1, file_bodies: 1, capture_directories: 0 }
    );
    for body in [&b"kept"[..], &kept_big, b"shared"] {
        assert!(held(&store, body));
    }
    for body in [&b"dropped"[..], &dropped_big] {
        assert!(!held(&store, body));
    }
    // What is left is exactly what the owner names, so another pass removes
    // nothing.
    assert_eq!(collected(&mut store), ContentCollection::default());
}

#[test]
fn a_capture_between_writing_and_registering_defers_the_pass() {
    let (_directory, mut store) = empty_store();
    let deferral = store.active_readers.defer_asset_inventory();
    let (reference, path) = written(&store, "writing", &[b"unregistered"]);
    assert_eq!(store.collect_external_content().unwrap(), CollectionOutcome::Deferred);
    assert!(held(&store, b"unregistered"));
    register(&store, &reference, &path);
    drop(deferral);
    assert_eq!(collected(&mut store), ContentCollection::default());
    assert!(held(&store, b"unregistered"));
}

#[test]
fn an_abandoned_capture_loses_its_bodies_now_and_its_directory_once_old() {
    let (_directory, mut store) = empty_store();
    let (_, path) = written(&store, "abandoned", &[b"abandoned"]);
    assert_eq!(collected(&mut store).database_bodies, 1);
    assert!(!held(&store, b"abandoned"));
    assert!(path.exists());

    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60);
    fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
    assert_eq!(collected(&mut store).capture_directories, 1);
    assert!(!path.parent().unwrap().exists());
}

#[test]
fn an_owner_that_cannot_be_read_defers_the_pass() {
    for damage in ["catalog", "body"] {
        let (_directory, mut store) = empty_store();
        let big = large(3);
        let (reference, path) = written(&store, "registered", &[b"registered", &big]);
        register(&store, &reference, &path);
        written(&store, "abandoned", &[b"garbage"]);
        match damage {
            "catalog" => fs::write(&path, b"synthetic-corrupt-catalog").unwrap(),
            _ => fs::remove_file(file_location(
                &store.repository_root.join("external-storage"),
                &hex::encode(hash(&big)),
            ))
            .unwrap(),
        }
        assert_eq!(
            store.collect_external_content().unwrap(),
            CollectionOutcome::Deferred,
            "{damage}"
        );
        assert!(held(&store, b"garbage"), "{damage}");
    }
}

/// Bodies an earlier version left in the flat directory are kept and removed
/// by the same marks as the bodies in subdirectories.
#[test]
fn file_bodies_in_the_flat_directory_and_in_shards_are_collected_alike() {
    let (_directory, mut store) = empty_store();
    let external = store.repository_root.join("external-storage");
    let bodies = [large(4), large(5), large(6), large(7)];
    let (kept, kept_path) = written(&store, "kept", &[&bodies[0], &bodies[1]]);
    written(&store, "dropped", &[&bodies[2], &bodies[3]]);
    register(&store, &kept, &kept_path);
    let flat = external.join("objects");
    fs::create_dir(&flat).unwrap();
    for body in [&bodies[1], &bodies[3]] {
        let digest = hex::encode(hash(body));
        fs::rename(file_location(&external, &digest), flat.join(&digest)).unwrap();
    }
    assert_eq!(collected(&mut store).file_bodies, 2);
    assert!(held(&store, &bodies[0]) && held(&store, &bodies[1]));
    assert!(!held(&store, &bodies[2]) && !held(&store, &bodies[3]));
    assert!(flat.join(hex::encode(hash(&bodies[1]))).is_file());
}

#[test]
fn a_pass_stops_at_its_removal_bound_and_the_next_one_finishes() {
    let (_directory, mut store) = empty_store();
    let bodies: Vec<Vec<u8>> = (0..5_000u32).map(|index| index.to_le_bytes().to_vec()).collect();
    let references: Vec<&[u8]> = bodies.iter().map(Vec::as_slice).collect();
    written(&store, "abandoned", &references);
    assert_eq!(collected(&mut store).database_bodies, 4_096);
    assert_eq!(collected(&mut store).database_bodies, 5_000 - 4_096);
    assert_eq!(collected(&mut store), ContentCollection::default());
}

fn edit_root(store: &mut PersistentStore, value: i64) {
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![RootMutation::Set {
                key: "synthetic".into(),
                value: json!(value),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
}

fn database_bodies(store: &PersistentStore) -> usize {
    ContentStore::open_existing(&store.repository_root.join("external-storage"))
        .unwrap()
        .unwrap()
        .page("", 1_000_000)
        .unwrap()
        .len()
}

/// A capture that supersedes another releases the other's bodies, and the
/// capture itself runs the pass, so a long edit history does not grow the
/// store. A capture cancelled after writing leaves bodies the next pass takes.
#[test]
fn repeated_captures_keep_only_what_the_live_captures_name() {
    let (_directory, mut store, _) = open_fixture();
    let mut sizes = Vec::new();
    for revision in 0..30 {
        edit_root(&mut store, revision);
        let hydration = store.hydrate_external_capture_dependencies("backup", &Never).unwrap();
        let capture = store.capture_external_library("backup", &hydration, &Never).unwrap();
        let named: i64 = capture
            .catalog
            .db
            .query_row(
                "SELECT count(*) FROM (SELECT hash FROM records UNION SELECT hash FROM generated)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(capture);
        sizes.push((database_bodies(&store), named as usize));
    }
    // The superseded capture's root body goes; the live one's stay.
    for (index, (held, named)) in sizes.iter().enumerate().skip(1) {
        assert_eq!(held, named, "capture {index}: {sizes:?}");
    }

    // Cancelled after it wrote its bodies: dropped without registering.
    let before = database_bodies(&store);
    edit_root(&mut store, 99);
    let prepared = store.prepare_content_capture("cancelled", "cancelled-consumer", store.revision().unwrap()).unwrap();
    let external = store.repository_root.join("external-storage");
    let mut catalog =
        CaptureCatalog::create(&external.join("captures").join("cancelled"), &external, None)
            .unwrap();
    prepared.project(&mut catalog, &Never).unwrap();
    drop(prepared);
    drop(catalog);
    assert!(database_bodies(&store) > before);
    collected(&mut store);
    assert_eq!(database_bodies(&store), before);
}

/// How long a pass takes, and how long it holds the repository mutation lock,
/// on a library-sized live capture with a superseded one beside it.
#[test]
#[ignore = "measurement"]
fn measures_a_pass_over_a_library_sized_capture() {
    const RECORDS: u32 = 40_000;
    const MOVED: u32 = 1_000;
    let (_directory, mut store) = empty_store();
    let body = |index: u32, version: u32| format!("{{\"record\":{index},\"v\":{version}}}").into_bytes();
    let live: Vec<Vec<u8>> = (0..RECORDS).map(|index| body(index, u32::from(index < MOVED))).collect();
    let old: Vec<Vec<u8>> = (0..RECORDS).map(|index| body(index, 0)).collect();
    let live_refs: Vec<&[u8]> = live.iter().map(Vec::as_slice).collect();
    let old_refs: Vec<&[u8]> = old.iter().map(Vec::as_slice).collect();
    written(&store, "superseded", &old_refs);
    let (reference, path) = written(&store, "live", &live_refs);
    register(&store, &reference, &path);

    let started = std::time::Instant::now();
    crate::external_storage::capture::registered_capture_roots([&reference], &store.repository_root)
        .unwrap();
    let mark = started.elapsed();
    let started = std::time::Instant::now();
    let collection = collected(&mut store);
    let pass = started.elapsed();
    assert_eq!(collection.database_bodies, MOVED as usize);
    println!(
        "records={RECORDS} removed={} mark_ms={} pass_ms={} locked_ms_at_most={}",
        collection.database_bodies,
        mark.as_millis(),
        pass.as_millis(),
        pass.saturating_sub(mark).as_millis(),
    );
}

/// A root large enough to be kept as a file body, different for each value.
fn set_root(store: &mut PersistentStore, value: i64) {
    store
        .commit(&WorkingSetCommit {
            root_mutations: Some(vec![RootMutation::Set {
                key: "synthetic".into(),
                value: json!(format!("{value}{}", "x".repeat(70 * 1024))),
            }]),
            ..empty_working_set_commit(store.revision().unwrap())
        })
        .unwrap();
}

/// What a backup job does before its upload: capture the library through a
/// lease, as the product does, and hold the capture for the job. Returns the
/// capture and the bodies it names.
fn back_up(
    store: &mut PersistentStore,
    connection: &str,
    job: &str,
) -> (String, std::collections::BTreeSet<String>) {
    let probe = crate::local_backup::NeverCancelled;
    let hydration = store.hydrate_external_capture_dependencies(connection, &probe).unwrap();
    let (lease, prepared) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    let sections = crate::external_storage::sections::capture_prepared_backup_sections(
        &prepared,
        &store.repository_root.join(format!("sections-{job}")),
        &crate::external_storage::contract::Cancellation::default(),
    )
    .unwrap();
    let capture = store
        .capture_external_library_from_lease_with_sections(
            connection,
            &hydration,
            &lease.lease,
            sections,
            &probe,
        )
        .unwrap();
    store
        .external_prepare_backup(job, connection, "format-repository", &capture.id, &format!("point-{job}"))
        .unwrap();
    let mut statement = capture
        .catalog
        .db
        .prepare("SELECT hash FROM records UNION SELECT hash FROM generated")
        .unwrap();
    let bodies = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(statement);
    (capture.id.clone(), bodies)
}

fn finish(store: &mut PersistentStore, job: &str) {
    store
        .external_finish_backup(job, &format!("point-{job}"), "snapshot", "observation")
        .unwrap();
}

/// The cleanup a job runs once its worker has ended.
fn release(
    store: &mut PersistentStore,
    job: &str,
) -> crate::external_storage::contract::Result<crate::external_storage::journal::SpoolCleanup> {
    let directory = store.repository_root.join("jobs").join(job);
    crate::external_storage::journal::TransferJournal::cleanup_terminal_spools_at(
        &directory,
        job,
        store,
        "format-repository",
    )
}

/// The transfer journal an uploading job keeps, which its cleanup reopens.
fn keep_journal(store: &PersistentStore, job: &str) {
    let owner = store.external_job(job).unwrap().unwrap();
    crate::external_storage::journal::TransferJournal::open(
        &store.repository_root.join("jobs").join(job),
        crate::external_storage::journal::JobIdentity {
            job_id: owner.id,
            connection_id: owner.connection_id,
            repository_id: "repository".into(),
            capture_id: owner.capture_id,
            capture: owner.identity,
        },
    )
    .unwrap();
}

fn released(store: &mut PersistentStore, job: &str) {
    assert_eq!(
        release(store, job).unwrap(),
        crate::external_storage::journal::SpoolCleanup::Removed { objects: 0, bytes: 0 },
    );
}

fn capture_directory(store: &PersistentStore, capture: &str) -> std::path::PathBuf {
    store.repository_root.join("external-storage").join("captures").join(capture)
}

fn holds(store: &PersistentStore, digest: &str) -> bool {
    ContentStore::open_existing(&store.repository_root.join("external-storage"))
        .unwrap()
        .unwrap()
        .stat(digest)
        .unwrap()
        .is_some()
}

/// A finished backup collects what only the backup before it named. Its own
/// capture stays, since the connection's next backup follows on from it.
#[test]
fn a_finished_backup_collects_what_only_the_backup_before_it_named() {
    let (_directory, mut store) = empty_store();
    let external = store.repository_root.join("external-storage");
    set_root(&mut store, 1);
    let (first, first_bodies) = back_up(&mut store, "connection", "first");
    finish(&mut store, "first");
    released(&mut store, "first");
    assert!(capture_directory(&store, &first).join("capture.sqlite").is_file());
    assert!(first_bodies.iter().all(|digest| holds(&store, digest)));

    set_root(&mut store, 2);
    let (second, second_bodies) = back_up(&mut store, "connection", "second");
    keep_journal(&store, "second");
    finish(&mut store, "second");
    released(&mut store, "second");
    assert!(!capture_directory(&store, &first).exists());
    assert!(capture_directory(&store, &second).join("capture.sqlite").is_file());
    assert!(second_bodies.iter().all(|digest| holds(&store, digest)));
    let only_first: Vec<_> = first_bodies.difference(&second_bodies).collect();
    assert!(only_first.iter().any(|digest| {
        let path = file_location(&external, digest);
        path.parent().is_some_and(Path::is_dir) && !path.exists()
    }));
    assert!(only_first.iter().all(|digest| !holds(&store, digest)));
}

/// A backup still uploading keeps its capture even after a library
/// replacement made its connection start over, and the newest finished
/// capture of each connection stays.
#[test]
fn a_release_keeps_what_a_running_backup_or_the_newest_capture_names() {
    let (_directory, mut store) = empty_store();
    set_root(&mut store, 1);
    let (first, _) = back_up(&mut store, "connection-a", "first");
    finish(&mut store, "first");
    released(&mut store, "first");
    set_root(&mut store, 2);
    let (running, running_bodies) = back_up(&mut store, "connection-b", "running");
    store
        .connection
        .execute("UPDATE content_change_consumers SET rebuild_required=1", [])
        .unwrap();
    set_root(&mut store, 3);
    let (newest, newest_bodies) = back_up(&mut store, "connection-a", "newest");
    finish(&mut store, "newest");
    released(&mut store, "newest");

    assert!(!capture_directory(&store, &first).exists());
    for (capture, bodies) in [(&running, &running_bodies), (&newest, &newest_bodies)] {
        assert!(capture_directory(&store, capture).join("capture.sqlite").is_file());
        assert!(bodies.iter().all(|digest| holds(&store, digest)));
    }
}

/// While a capture writes, a release leaves every capture and body and still
/// succeeds; the next release collects them. A collection that fails does not
/// fail the release either.
#[test]
fn a_release_whose_collection_cannot_run_still_succeeds() {
    let (_directory, mut store) = empty_store();
    let external = store.repository_root.join("external-storage");
    set_root(&mut store, 1);
    let (first, first_bodies) = back_up(&mut store, "connection", "first");
    finish(&mut store, "first");
    released(&mut store, "first");
    set_root(&mut store, 2);
    let (_, second_bodies) = back_up(&mut store, "connection", "second");
    finish(&mut store, "second");

    let writing = store.active_readers.defer_asset_inventory();
    released(&mut store, "second");
    assert!(capture_directory(&store, &first).join("capture.sqlite").is_file());
    assert!(first_bodies.iter().all(|digest| holds(&store, digest)));
    drop(writing);
    released(&mut store, "second");
    assert!(!capture_directory(&store, &first).exists());

    // Once the connection is gone nothing names the second capture, and a
    // store that cannot be opened fails the pass.
    store.external_prepare_connection_removal("connection").unwrap();
    let files: Vec<_> = second_bodies
        .iter()
        .map(|digest| file_location(&external, digest))
        .filter(|path| path.is_file())
        .collect();
    assert!(!files.is_empty());
    fs::write(external.join("content.sqlite"), b"synthetic-unreadable-store").unwrap();
    released(&mut store, "second");
    assert!(files.iter().all(|path| path.is_file()));
}

/// An opened store collects what releases before it left, including more
/// captures than one step removes.
#[test]
fn an_opened_store_collects_what_earlier_releases_left() {
    use tauri::Manager;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut captures = Vec::new();
    {
        let mut store = PersistentStore::open(root).unwrap();
        for index in 0..18 {
            set_root(&mut store, index);
            let job = format!("job-{index}");
            captures.push(back_up(&mut store, "connection", &job).0);
            finish(&mut store, &job);
        }
    }
    let newest = captures.pop().unwrap();
    let external = root.join("external-storage").join("captures");
    assert!(captures.iter().all(|capture| external.join(capture).is_dir()));

    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    app.manage(crate::app_paths::AppPaths {
        data: root.to_owned(),
        webview: None,
        logs: root.join("logs"),
        cache: root.join("cache"),
        cleanup_control: root.join("cleanup"),
        tauri_derived: Vec::new(),
        install: None,
        integration: None,
    });
    app.manage(crate::persistent_store::commands::PersistentStoreState::default());
    app.manage(crate::external_storage::job_store::JobCommandState::default());
    app.manage(crate::native_file_jobs::NativeFileJobState::initialize(root.join("native-file-jobs")));
    crate::persistent_store::commands::open_for_renderer(app.handle(), &app.state()).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while captures.iter().any(|capture| external.join(capture).exists()) {
        assert!(std::time::Instant::now() < deadline, "released captures stayed after the store opened");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(external.join(&newest).join("capture.sqlite").is_file());
}

/// A backup capture takes its hold on collection only while no pass sweeps,
/// so a sweep that began before it cannot remove the bodies it writes.
#[test]
fn a_backup_capture_waits_for_a_running_sweep_before_it_writes() {
    let (_directory, mut store) = empty_store();
    set_root(&mut store, 1);
    let (lease, _) = store.lww_acquire_backup_capture(store.revision().unwrap()).unwrap();
    let readers = Arc::clone(&store.active_readers);
    let sweep = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
    let (prepared_sender, prepared) = std::sync::mpsc::channel();
    let (finish_sender, finished) = std::sync::mpsc::channel::<()>();
    let (store, lease) = (&mut store, &lease.lease);
    // The test's side owns the sender, so a failed assertion ends the capture
    // thread instead of leaving it waiting, and the lock is released before
    // any assertion so a failure does not poison it.
    thread::scope(move |scope| {
        scope.spawn(move || {
            let capture = store.prepare_content_capture_from_lease("capture", "connection", lease);
            prepared_sender.send(()).unwrap();
            let _ = finished.recv();
            drop(capture);
        });
        // Nothing shows that the capture is waiting, so it is given time to
        // finish if it does not wait.
        let waited = prepared.recv_timeout(Duration::from_millis(500)).is_err();
        let deferred_during_sweep = readers.asset_inventory_deferred();
        drop(sweep);
        assert!(waited && !deferred_during_sweep);
        prepared.recv().unwrap();
        assert!(readers.asset_inventory_deferred());
        finish_sender.send(()).unwrap();
    });
}
