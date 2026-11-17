use super::*;
use crate::external_storage::{
    connection_commands::ConnectedRepository,
    connection_store::StoredConnection,
    contract::{ConnectionConfig, RemoteLocator},
    fake,
    lww_tests::{small_asset, CycleFixture},
};
use std::{collections::BTreeMap, sync::{Arc, Mutex}, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CrashPoint {
    AfterRegistration,
    AfterPublish,
    AfterPublishedRegistration,
}

static CRASHES: Mutex<BTreeMap<PathBuf, CrashPoint>> = Mutex::new(BTreeMap::new());

/// Stops a hydration of `root` at `point` as a process exit would, leaving
/// whatever was already on disk.
pub(super) fn crash_point(root: &Path, point: CrashPoint) -> crate::server_sync::Result<()> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    if CRASHES.lock().unwrap().get(&root) == Some(&point) {
        return Err(crate::server_sync::SyncError::new("synthetic-crash", 500));
    }
    Ok(())
}

struct Crash(PathBuf);
impl Drop for Crash {
    fn drop(&mut self) {
        CRASHES.lock().unwrap().remove(&self.0);
    }
}
fn crash_at(root: &Path, point: CrashPoint) -> Crash {
    let root = std::fs::canonicalize(root).unwrap();
    CRASHES.lock().unwrap().insert(root.clone(), point);
    Crash(root)
}

/// What a hydration of one repository opened, by kind: the remote body
/// registry, the asset catalog, a source connection, a connection store, and
/// the scratch directories it staged under.
static OBSERVED: Mutex<Vec<(&'static str, PathBuf, PathBuf)>> = Mutex::new(Vec::new());
pub(super) fn observe(kind: &'static str, root: &Path, path: &Path) {
    OBSERVED.lock().unwrap().push((kind, root.to_owned(), path.to_owned()));
}
fn forget_observations(root: &Path) {
    OBSERVED.lock().unwrap().retain(|(_, r, _)| r != root);
}
fn observed(kind: &str, root: &Path) -> Vec<PathBuf> {
    OBSERVED.lock().unwrap().iter().filter(|(k, r, _)| *k == kind && r == root).map(|(_, _, path)| path.clone()).collect()
}
/// Whether `observed` names `root`, however either is spelled.
fn same_root(observed: &Path, root: &Path, canonical: &Path) -> bool {
    observed == root || std::fs::canonicalize(observed).is_ok_and(|observed| observed == canonical)
}
/// How many times the registry under `root` was opened, by any spelling of `root`.
pub(crate) fn registry_opens(root: &Path) -> usize {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    OBSERVED.lock().unwrap().iter().filter(|(kind, r, _)| *kind == "remote-bodies" && same_root(r, root, &canonical)).count()
}
pub(crate) fn forget_registry_opens(root: &Path) {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    OBSERVED.lock().unwrap().retain(|(kind, r, _)| *kind != "remote-bodies" || !same_root(r, root, &canonical));
}
/// Registers a standalone source of `byte_length` bytes for `hash` that no
/// provider holds, for a test that only asks the registry about it.
pub(crate) fn register_synthetic_source(root: &Path, hash: &str, byte_length: u64) {
    register(root, &Source {
        hash: hash.into(),
        library_id: "synthetic-library".into(),
        connection_id: "synthetic-connection".into(),
        connection_root: root.to_owned(),
        protected_segment: "synthetic-segment".into(),
        body: LargeBody {
            object_id: "synthetic-body".into(),
            sha256: hash.into(),
            byte_length: byte_length.into(),
            plaintext_byte_length: byte_length.into(),
            locator: Some(RemoteLocator { connection_identity: "synthetic-account".into(), collection: None, object: "synthetic-body".into() }),
        },
    })
    .unwrap();
}

static HOLDS: Mutex<Vec<(PathBuf, usize, Duration)>> = Mutex::new(Vec::new());
/// One hold of the repository mutation lock while `bodies` were published.
pub(super) fn record_hold(root: &Path, bodies: usize, held: Duration) {
    HOLDS.lock().unwrap().push((root.to_owned(), bodies, held));
}
fn holds(root: &Path) -> Vec<(usize, Duration)> {
    HOLDS.lock().unwrap().iter().filter(|(r, ..)| r == root).map(|(_, bodies, held)| (*bodies, *held)).collect()
}

static REGISTRATIONS: Mutex<Vec<(PathBuf, usize, Duration)>> = Mutex::new(Vec::new());
/// One hold of the repository mutation lock while a page of `hashes` was registered.
pub(super) fn record_registration_hold(root: &Path, hashes: usize, held: Duration) {
    REGISTRATIONS.lock().unwrap().push((root.to_owned(), hashes, held));
}
fn registration_holds(root: &Path) -> Vec<(usize, Duration)> {
    REGISTRATIONS.lock().unwrap().iter().filter(|(r, ..)| r == root).map(|(_, hashes, held)| (*hashes, *held)).collect()
}

type Hook = Box<dyn FnOnce() + Send>;
static AFTER_LOOKUP: Mutex<BTreeMap<PathBuf, Hook>> = Mutex::new(BTreeMap::new());
/// Runs what a test set for `root`, once, after a page was looked up in the
/// registry and before its rows are registered.
pub(super) fn after_lookup(root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let hook = AFTER_LOOKUP.lock().unwrap().remove(&root);
    if let Some(hook) = hook {
        hook();
    }
}
fn on_after_lookup(root: &Path, hook: impl FnOnce() + Send + 'static) {
    AFTER_LOOKUP.lock().unwrap().insert(std::fs::canonicalize(root).unwrap(), Box::new(hook));
}

static BEFORE_PUBLISH: Mutex<BTreeMap<PathBuf, Hook>> = Mutex::new(BTreeMap::new());
/// Runs what a test set for `root`, once, after bodies were read and before
/// the first batch of them is published.
pub(super) fn before_publish(root: &Path) {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_owned());
    let hook = BEFORE_PUBLISH.lock().unwrap().remove(&root);
    if let Some(hook) = hook {
        hook();
    }
}
fn on_before_publish(root: &Path, hook: impl FnOnce() + Send + 'static) {
    BEFORE_PUBLISH.lock().unwrap().insert(std::fs::canonicalize(root).unwrap(), Box::new(hook));
}

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// A receiver whose packed bodies are registered and not local, with the
/// connection they are fetched through. Each publication puts its bodies in a
/// pack of its own.
struct Received {
    f: CycleFixture,
    publications: Vec<Vec<(String, u64)>>,
    _connection: TestSourceConnection,
}

fn received(publications: &[usize]) -> Received {
    let mut f = CycleFixture::new();
    let mut published = Vec::new();
    for (index, count) in publications.iter().enumerate() {
        let mut bodies = Vec::new();
        for n in 0..*count {
            let body = format!("synthetic hydration body {index} {n}").into_bytes();
            bodies.push((small_asset(&mut f.a, &format!("hydration-{index}-{n}"), &body), body.len() as u64));
        }
        run(f.publish_a());
        published.push(bodies);
    }
    assert!(run(f.receive_b()) > 0);
    let stored = StoredConnection {
        id: "receiver".into(),
        config: ConnectionConfig {
            provider: "synthetic".into(),
            profile: None,
            endpoint: "https://synthetic.invalid".into(),
            account_id: "fixture".into(),
            location: BTreeMap::new(),
            oauth_profile: None,
        },
        descriptor: f.receiver.descriptor.clone(),
        descriptor_locator: RemoteLocator {
            connection_identity: f.receiver.repository.connection_identity.clone(),
            collection: None,
            object: "descriptor".into(),
        },
        provider_repository_id: f.receiver.repository.repository_id.clone(),
        credential_ref: "credential".into(),
        root_key_ref: "key".into(),
        recovery_key_ref: "recovery".into(),
        retention_policy: None,
        transfer_concurrency: None, labels: Default::default(),
        capabilities: f.receiver.capabilities.clone(),
        created_at_ms: 1,
        verified_at_ms: 1,
        last_sync_at_ms: None,
        last_backup_at_ms: None,
    };
    let connection = Arc::new(ConnectedRepository {
        stored,
        provider: f.provider.clone(),
        handle: fake::repository(),
        dependencies: fake::loopback_dependencies(fake::MemoryVault::default(), 1).dependencies,
        root_key: zeroize::Zeroizing::new([7; 32]),
    });
    let connection = install_test_source_connection(f.directory_b.path(), connection).unwrap();
    let cas = crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap();
    for (hash, _) in published.iter().flatten() {
        assert!(cas.stat_object(hash).unwrap().is_none());
        assert!(packed_source(f.directory_b.path(), hash).unwrap().is_some());
    }
    Received { f, publications: published, _connection: connection }
}

fn catalog(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap()
}
fn row(root: &Path, hash: &str) -> Option<u64> {
    use rusqlite::OptionalExtension;
    catalog(root)
        .query_row("SELECT byte_size FROM asset_objects WHERE object_hash=?1", [hash], |row| row.get::<_, i64>(0))
        .optional()
        .unwrap()
        .map(|size| size as u64)
}
/// Hashes a historical snapshot root names enter hydration with no catalog row.
fn forget_rows(root: &Path, hashes: &[(String, u64)]) {
    let db = catalog(root);
    for (hash, _) in hashes {
        db.execute("DELETE FROM asset_objects WHERE object_hash=?1", [hash]).unwrap();
    }
}
fn hydrate(root: &Path, hashes: &[(String, u64)]) -> crate::server_sync::Result<Vec<String>> {
    let digests = hashes.iter().map(|(hash, _)| hash.clone()).collect::<Vec<_>>();
    let before = registration_holds(root).len();
    let result = hydrate_registered_many(root, &digests, &Default::default(), None, &|| Ok(()), &mut |_, _| {});
    for (hashes, held) in &registration_holds(root)[before..] {
        eprintln!("registration hold: {hashes} hashes in {held:?}");
    }
    result
}
fn assert_resident(root: &Path, bodies: &[(String, u64)]) {
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    for (hash, size) in bodies {
        assert_eq!(cas.stat_object(hash).unwrap(), Some(*size));
        assert_eq!(row(root, hash), Some(*size), "every published body ends with its catalog row");
    }
}

#[test]
fn a_source_deletion_and_publish_crash_is_recovered_by_page_start_without_hydration() {
    let received = received(&[3]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    let (repository, target) = (root.to_owned(), bodies[1].0.clone());
    on_before_publish(root, move || {
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
        let db = database(&repository, false).unwrap().unwrap();
        db.execute("DELETE FROM sources WHERE hash=?1", [&target]).unwrap();
        db.execute("DELETE FROM packed_sources WHERE hash=?1", [&target]).unwrap();
        catalog(&repository).execute("DELETE FROM asset_objects WHERE object_hash=?1", [&target]).unwrap();
    });
    {
        let _crash = crash_at(root, CrashPoint::AfterPublish);
        assert_eq!(hydrate(root, bodies).unwrap_err().code, "synthetic-crash");
    }
    assert!(packed_source(root, &bodies[1].0).unwrap().is_none());
    assert_eq!(row(root, &bodies[1].0), None);
    assert_eq!(crate::asset_repository::PayloadCas::new(root).unwrap().stat_object(&bodies[1].0).unwrap(), Some(bodies[1].1));
    let native_jobs = || Ok(Vec::new());
    let open_store = || crate::persistent_store::PersistentStore::open(root).map_err(|error| error.to_string());
    let probe = crate::asset_repository::commands::CasJobOwnerProbe {native_jobs:&native_jobs,
        device_job_owned:&|_|Ok(false), external_job_active:&|_|Ok(false), open_store:&open_store};
    let state = crate::asset_repository::commands::DurableCasJobState::default();
    let journals = crate::asset_repository::job_pins::durable_cas_job_ids(root).unwrap();
    assert!(!journals.is_empty());
    catalog(root).execute_batch("CREATE TRIGGER fail_hydration_recovery BEFORE INSERT ON asset_objects
        BEGIN SELECT RAISE(ABORT, 'synthetic'); END;").unwrap();
    assert!(state.sweep_after_page_start(root, &probe).is_err());
    assert_eq!(crate::asset_repository::job_pins::durable_cas_job_ids(root).unwrap(), journals);
    assert_eq!(row(root, &bodies[1].0), None);
    catalog(root).execute_batch("DROP TRIGGER fail_hydration_recovery").unwrap();
    state.sweep_after_page_start(root, &probe).unwrap();
    assert!(crate::asset_repository::job_pins::durable_cas_job_ids(root).unwrap().is_empty());
    assert_resident(root, bodies);
}

#[test]
fn a_crash_after_published_registration_keeps_its_journal_until_recovery() {
    let received = received(&[1]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    {
        let _crash = crash_at(root, CrashPoint::AfterPublishedRegistration);
        assert_eq!(hydrate(root, bodies).unwrap_err().code, "synthetic-crash");
    }
    assert_resident(root, bodies);
    assert!(!crate::asset_repository::job_pins::durable_cas_job_ids(root).unwrap().is_empty());
    let native_jobs = || Ok(Vec::new());
    let open_store = || crate::persistent_store::PersistentStore::open(root).map_err(|error| error.to_string());
    crate::asset_repository::commands::DurableCasJobState::default().sweep_after_page_start(root,
        &crate::asset_repository::commands::CasJobOwnerProbe {native_jobs:&native_jobs,
            device_job_owned:&|_|Ok(false), external_job_active:&|_|Ok(false), open_store:&open_store}).unwrap();
    assert!(crate::asset_repository::job_pins::durable_cas_job_ids(root).unwrap().is_empty());
    assert_resident(root, bodies);
}

#[test]
fn a_crash_between_publish_and_registration_ends_with_every_row_after_the_next_hydration() {
    let received = received(&[3]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    forget_rows(root, bodies);
    {
        let _crash = crash_at(root, CrashPoint::AfterPublish);
        assert_eq!(hydrate(root, bodies).unwrap_err().code, "synthetic-crash");
    }
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    assert!(
        bodies.iter().any(|(hash, _)| cas.stat_object(hash).unwrap().is_some()),
        "the crash came after a body was published"
    );
    assert!(hydrate(root, bodies).unwrap().is_empty());
    assert_resident(root, bodies);
}

#[test]
fn hydration_opens_the_same_databases_however_many_pack_groups_it_reads() {
    let received = received(&[3, 3, 3]);
    let root = received.f.directory_b.path();
    let packs = received
        .publications
        .iter()
        .map(|bodies| {
            packed_source(root, &bodies[0].0).unwrap().unwrap().packs.iter().map(|pack| pack.header.object_id.clone()).collect::<Vec<_>>()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(packs.len(), 3, "each publication is a pack group of its own");
    let bodies = received.publications.concat();
    forget_observations(root);
    assert!(hydrate(root, &bodies).unwrap().is_empty());
    assert_resident(root, &bodies);
    assert_eq!(observed("remote-bodies", root).len(), 1, "one registry connection for every lookup");
    assert_eq!(observed("source-connection", root).len(), 1, "one source connection for every group");
    assert_eq!(observed("catalog", root).len(), 1, "one catalog connection for every registration");
    let scratch = observed("scratch", root);
    assert!(!scratch.is_empty());
    assert!(scratch.iter().all(|path| path.starts_with(root.join("external-storage").join("scratch"))), "{scratch:?}");
    let staged = super::super::snapshot_restore::CONTENT_STORE_OPENS
        .lock()
        .unwrap()
        .iter()
        .filter(|path| scratch.iter().any(|scratch| path.starts_with(scratch)))
        .count();
    assert_eq!(staged, 0, "staging bodies as files never opens a content store");
}

#[test]
fn publication_holds_the_mutation_lock_once_per_batch_of_bodies() {
    use crate::asset_repository::body_io;
    const BATCH: usize = 64;
    let received = received(&[2 * BATCH + 2]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    body_io::reset_body_io();
    assert!(hydrate(root, bodies).unwrap().is_empty());
    let observed = body_io::take_body_io();
    // Download/decryption workers share this observation with the ten CAS workers.
    assert!(observed.worker_scopes_started >= 10);
    assert_eq!(observed.worker_scopes_started, observed.worker_scopes_settled);
    assert_resident(root, bodies);
    let holds = holds(root);
    for (index, (count, held)) in holds.iter().enumerate() {
        eprintln!("mutation lock hold {index}: {count} bodies in {held:?}");
    }
    assert_eq!(holds.iter().map(|(count, _)| count).sum::<usize>(), bodies.len());
    assert!(holds.iter().all(|(count, _)| *count <= BATCH));
    assert_eq!(holds.len(), bodies.len().div_ceil(BATCH), "every hold but the last publishes a full batch");
    assert_eq!(registration_holds(root).iter().map(|(hashes, _)| *hashes).collect::<Vec<_>>(), vec![bodies.len()], "one page, one hold");
}

#[test]
fn a_crash_after_registration_leaves_rows_that_the_next_hydration_fills() {
    let received = received(&[3]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    forget_rows(root, bodies);
    {
        let _crash = crash_at(root, CrashPoint::AfterRegistration);
        assert_eq!(hydrate(root, bodies).unwrap_err().code, "synthetic-crash");
    }
    let cas = crate::asset_repository::PayloadCas::new(root).unwrap();
    for (hash, size) in bodies {
        assert_eq!(row(root, hash), Some(*size));
        assert!(cas.stat_object(hash).unwrap().is_none());
        assert!(stat(root, hash).unwrap().is_some(), "a row without its file keeps the remote source it stands on");
    }
    assert!(hydrate(root, bodies).unwrap().is_empty());
    assert_resident(root, bodies);
}

#[test]
fn a_source_deleted_between_lookup_and_registration_leaves_no_row_and_its_body_unavailable() {
    let received = received(&[3]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    forget_rows(root, bodies);
    let deleted = bodies[1].0.clone();
    let (registry, target) = (root.to_owned(), deleted.clone());
    on_after_lookup(root, move || {
        // A connection removal deletes the body's sources under the repository mutation lock.
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
        let db = database(&registry, false).unwrap().unwrap();
        db.execute("DELETE FROM sources WHERE hash=?1", [&target]).unwrap();
        db.execute("DELETE FROM packed_sources WHERE hash=?1", [&target]).unwrap();
    });
    assert_eq!(hydrate(root, bodies).unwrap(), vec![deleted.clone()]);
    assert_eq!(row(root, &deleted), None, "a row needs a file or a registered source");
    assert!(crate::asset_repository::PayloadCas::new(root).unwrap().stat_object(&deleted).unwrap().is_none());
    assert!(stat(root, &deleted).unwrap().is_none());
    let kept = bodies.iter().filter(|(hash, _)| *hash != deleted).cloned().collect::<Vec<_>>();
    assert_resident(root, &kept);
}

#[test]
fn a_body_whose_row_and_source_go_while_it_downloads_is_published_with_a_row() {
    let received = received(&[3]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    forget_rows(root, bodies);
    let (repository, target) = (root.to_owned(), bodies[1].0.clone());
    on_before_publish(root, move || {
        // A connection removal deletes the body's sources and, with no file yet, its row.
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
        let db = database(&repository, false).unwrap().unwrap();
        db.execute("DELETE FROM sources WHERE hash=?1", [&target]).unwrap();
        db.execute("DELETE FROM packed_sources WHERE hash=?1", [&target]).unwrap();
        catalog(&repository).execute("DELETE FROM asset_objects WHERE object_hash=?1", [&target]).unwrap();
    });
    assert!(hydrate(root, bodies).unwrap().is_empty());
    assert_resident(root, bodies);
}

#[test]
fn a_local_body_without_a_source_gets_a_row_only_while_its_file_is_there() {
    let received = received(&[2]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    assert!(hydrate(root, bodies).unwrap().is_empty());
    forget_rows(root, bodies);
    let db = database(root, false).unwrap().unwrap();
    for (hash, _) in bodies {
        db.execute("DELETE FROM sources WHERE hash=?1", [hash]).unwrap();
        db.execute("DELETE FROM packed_sources WHERE hash=?1", [hash]).unwrap();
    }
    drop(db);
    let removed = bodies[0].0.clone();
    let (repository, target) = (root.to_owned(), removed.clone());
    on_after_lookup(root, move || {
        // A sweep removes the file under the repository mutation lock after the hydration saw it.
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
        std::fs::remove_file(repository.join("assets").join("objects").join(&target[..2]).join(&target[2..])).unwrap();
    });
    let mut outcomes = Vec::new();
    let digests = bodies.iter().map(|(hash, _)| hash.clone()).collect::<Vec<_>>();
    let unavailable = hydrate_registered_many(root, &digests, &Default::default(), None, &|| Ok(()), &mut |hash, outcome| {
        outcomes.push((hash.to_owned(), outcome));
    })
    .unwrap();
    assert_eq!(unavailable, vec![removed.clone()]);
    assert_eq!(row(root, &removed), None, "a row needs a file or a registered source");
    assert_eq!(outcomes, vec![(bodies[1].0.clone(), crate::server_sync::residency::HydrationOutcome::AlreadyLocal)]);
    assert_resident(root, &bodies[1..]);
}

#[test]
fn a_full_page_of_hashes_is_registered_under_one_hold() {
    use crate::persistent_store::asset_object_catalog::ASSET_OBJECT_CATALOG_MAX_PAGE;
    const PAGE: usize = ASSET_OBJECT_CATALOG_MAX_PAGE as usize;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let _store = crate::persistent_store::PersistentStore::open(root).unwrap();
    let hashes = (0..PAGE + 1).map(|index| format!("{index:064x}")).collect::<Vec<_>>();
    let packed = hashes[..PAGE].iter().map(|hash| synthetic_packed(hash, "library", 1, &["pack"])).collect::<Vec<_>>();
    register_verified_packed_many(root, &packed).unwrap();
    {
        // Stops after the first page, before any body is read.
        let _crash = crash_at(root, CrashPoint::AfterRegistration);
        let error = hydrate_registered_many(root, &hashes, &Default::default(), None, &|| Ok(()), &mut |_, _| {}).unwrap_err();
        assert_eq!(error.code, "synthetic-crash");
    }
    let holds = registration_holds(root);
    for (hashes, held) in &holds {
        eprintln!("registration hold: {hashes} hashes in {held:?}");
    }
    assert_eq!(holds.iter().map(|(hashes, _)| *hashes).collect::<Vec<_>>(), vec![PAGE]);
    let rows: i64 = catalog(root).query_row("SELECT COUNT(*) FROM asset_objects", [], |row| row.get(0)).unwrap();
    assert_eq!(rows as usize, PAGE);
    assert_eq!(row(root, &hashes[PAGE]), None, "the next page was not reached");
}

#[test]
fn hydration_registers_a_local_body_that_has_no_row() {
    let received = received(&[2]);
    let root = received.f.directory_b.path();
    let bodies = &received.publications[0];
    assert!(hydrate(root, bodies).unwrap().is_empty());
    forget_rows(root, bodies);
    let mut outcomes = Vec::new();
    let digests = bodies.iter().map(|(hash, _)| hash.clone()).collect::<Vec<_>>();
    let unavailable = hydrate_registered_many(root, &digests, &Default::default(), None, &|| Ok(()), &mut |hash, outcome| {
        outcomes.push((hash.to_owned(), outcome));
    })
    .unwrap();
    assert!(unavailable.is_empty());
    assert_eq!(
        outcomes,
        digests.iter().map(|hash| (hash.clone(), crate::server_sync::residency::HydrationOutcome::AlreadyLocal)).collect::<Vec<_>>()
    );
    assert_resident(root, bodies);
}

fn synthetic_object(id: &str, role: risunest_external_storage_format::snapshot::ObjectRole) -> StoredObject {
    use risunest_external_storage_format::snapshot::{PublicObjectHeader, WireLocator};
    StoredObject {
        header: PublicObjectHeader::new("synthetic-repository".into(), id.into(), role, 1).unwrap(),
        locator: WireLocator { connection_identity: "synthetic-account".into(), collection: Some("objects".into()), object: id.into() },
        ciphertext_length: 1,
        ciphertext_sha256: [1; 32],
        plaintext_length: 1,
        plaintext_sha256: [2; 32],
    }
}
pub(crate) fn synthetic_packed(hash: &str, library: &str, byte_length: u64, packs: &[&str]) -> PackedSource {
    use risunest_external_storage_format::snapshot::ObjectRole;
    PackedSource {
        hash: hash.into(),
        byte_length,
        library_id: library.into(),
        connection_id: "synthetic-connection".into(),
        connection_root: PathBuf::new(),
        protected_snapshot: "synthetic-snapshot".into(),
        catalog: synthetic_object("synthetic-catalog", ObjectRole::Catalog),
        chunks: Vec::new(),
        packs: packs.iter().map(|id| synthetic_object(id, ObjectRole::Pack)).collect(),
    }
}

#[test]
fn one_registry_connection_answers_every_page_in_the_order_a_single_lookup_would() {
    let root = tempfile::tempdir().unwrap();
    let hashes = (0..LOOKUP_PAGE * 2 + 1).map(|index| format!("{index:064x}")).collect::<Vec<_>>();
    let packed = hashes.iter().enumerate().map(|(index, hash)| synthetic_packed(hash, "library-b", index as u64, &[])).collect::<Vec<_>>();
    register_verified_packed_many(root.path(), &packed).unwrap();
    register_verified_packed_many(root.path(), &[synthetic_packed(&hashes[1], "library-a", 1, &[])]).unwrap();
    register(root.path(), &Source {
        hash: hashes[2].clone(),
        library_id: "library-z".into(),
        connection_id: "synthetic-connection".into(),
        connection_root: PathBuf::new(),
        protected_segment: "synthetic-segment".into(),
        body: LargeBody {
            object_id: "synthetic-body".into(),
            sha256: hashes[2].clone(),
            byte_length: 9u64.into(),
            plaintext_byte_length: 2u64.into(),
            locator: Some(RemoteLocator { connection_identity: "synthetic-account".into(), collection: None, object: "synthetic-body".into() }),
        },
    })
    .unwrap();
    forget_observations(root.path());
    let mut remote = RemoteBodies::connect(root.path()).unwrap();
    let unknown = "f".repeat(64);
    let mut lookup = hashes.iter().map(String::as_str).collect::<Vec<_>>();
    lookup.push(&unknown);
    let registered = remote.registered(&lookup).unwrap();
    assert_eq!(registered.len(), hashes.len(), "every registered hash on every page, and nothing for an unknown one");
    for (index, hash) in hashes.iter().enumerate() {
        let frozen = remote.frozen(hash).unwrap().unwrap();
        match (&registered[hash], frozen) {
            (Registered::Standalone(chosen), FrozenBodySource::Standalone(single)) => {
                assert_eq!(index, 2, "a standalone source wins over a packed one");
                assert_eq!(chosen.library_id, single.library_id);
            }
            (Registered::Packed(connection, outline), FrozenBodySource::Packed(single)) => {
                assert_eq!(connection.2, single.library_id);
                assert_eq!(outline.byte_length, single.byte_length);
                assert_eq!(single.library_id, if index == 1 { "library-a" } else { "library-b" }, "the first library in order wins");
            }
            _ => panic!("the paged lookup and the single lookup chose different kinds for {hash}"),
        }
        assert_eq!(remote.stat(hash).unwrap(), Some(if index == 2 { 2 } else { index as u64 }));
    }
    assert_eq!(observed("remote-bodies", root.path()).len(), 1);
    let full = remote.packed("library-b", hashes.iter().map(String::as_str)).unwrap();
    assert_eq!(full.len(), hashes.len());
}

#[test]
fn interned_sources_share_objects_and_serialize_as_before() {
    let first = synthetic_packed(&"a".repeat(64), "library", 1, &["pack-1", "pack-2"]);
    let second = synthetic_packed(&"b".repeat(64), "library", 1, &["pack-2"]);
    let mut conflicting = synthetic_packed(&"c".repeat(64), "library", 1, &["pack-2"]);
    conflicting.packs[0].plaintext_length = 9;
    let encoded = [&first, &second, &conflicting].map(|source| serde_json::to_string(source).unwrap());
    let mut interner = ObjectInterner::default();
    let shared = [first, second, conflicting].map(|source| source.interned(&mut interner));
    assert!(Arc::ptr_eq(&shared[0].catalog.0, &shared[1].catalog.0));
    assert!(Arc::ptr_eq(&shared[0].packs[1].0, &shared[1].packs[0].0));
    assert!(!Arc::ptr_eq(&shared[1].packs[0].0, &shared[2].packs[0].0), "a different object under a known identity stays its own");
    for (source, encoded) in shared.iter().zip(&encoded) {
        assert_eq!(&serde_json::to_string(source).unwrap(), encoded);
        assert_eq!(&serde_json::to_string(&source.unshared()).unwrap(), encoded);
        let decoded: SharedPackedSource = serde_json::from_str(encoded).unwrap();
        assert_eq!(&serde_json::to_string(&decoded).unwrap(), encoded);
    }
}

#[test]
fn visible_server_and_external_groups_start_in_display_order_without_splitting_shared_packs() {
    use crate::server_sync::residency::{test_remote, HydrationSession};
    for external_first in [true, false] {
        let received = received(&[2, 1]);
        let root = received.f.directory_b.path();
        let first = &received.publications[0];
        let background = &received.publications[1][0].0;
        let server_body = b"synthetic mixed holder icon".to_vec();
        let server_hash = risunest_sync_wire::hash(&server_body);
        test_remote::hold(root, &[(&server_hash, server_body.len() as u64)]);
        test_remote::serve(root, &server_hash, server_body);
        let mut ordered = if external_first { vec![first[0].0.clone(), server_hash.clone()] } else { vec![server_hash.clone(), first[0].0.clone()] };
        let priority = ordered.iter().cloned().collect();
        ordered.extend([background.clone(), first[1].0.clone()]);
        let mut groups = Vec::new();
        let missing = HydrationSession::new(root, None).unwrap().hydrate_many_outcomes_prioritized(&ordered, &priority, &|| Ok(()), |hash, _| {
            let group = if hash == server_hash { "server" } else if hash == background { "background" } else { "shared-pack" };
            if groups.last() != Some(&group) { groups.push(group); }
        }).unwrap();
        assert!(missing.is_empty());
        assert_eq!(groups, if external_first { vec!["shared-pack", "server", "background"] } else { vec!["server", "shared-pack", "background"] });
        assert_resident(root, &received.publications.concat());
        assert!(crate::asset_repository::PayloadCas::new(root).unwrap().stat_object(&server_hash).unwrap().is_some());
    }
}

#[test]
fn a_source_that_needs_its_sign_in_or_key_names_its_connection_and_other_failures_name_none() {
    // A changed WebDAV password answers 401, which the provider reports as unauthorized. A recovery key this device
    // lacks fails as the source connection opens.
    for (kind, opening, code) in [
        (ErrorKind::ReauthRequired, false, "external-reauth-required"),
        (ErrorKind::Unauthorized, false, "external-reauth-required"),
        (ErrorKind::RepositoryKeyUnavailable, true, "external-key-locked"),
        (ErrorKind::RecoveryKeyMismatch, true, "external-key-locked"),
    ] {
        let received = received(&[2]);
        let root = received.f.directory_b.path();
        if opening {
            received.f.provider.fail_open(kind);
        } else {
            for pack in received.f.provider.objects_with_role(crate::external_storage::contract::ObjectRole::Pack) {
                received.f.provider.fail_read(&pack, kind);
            }
        }
        let refused = serde_json::to_value(hydrate(root, &received.publications[0]).unwrap_err()).unwrap();
        assert_eq!((&refused["code"], &refused["connectionId"]), (&serde_json::json!(code), &serde_json::json!("receiver")), "{kind:?}");
    }
    let missing = serde_json::to_value(source_error("receiver")(ProviderError::new(ErrorKind::NotFound))).unwrap();
    assert_eq!(missing["code"], "external-body-unavailable");
    assert!(missing.get("connectionId").is_none());
}
